//! Cranelift JIT compiler for guest code.
//!
//! Guest code is translated one basic block at a time. A block starts
//! at a guest PC and extends until a control transfer instruction, an
//! instruction the JIT does not support, the end of the guest page, or
//! a length limit. Unsupported instructions (CSR, system, floating
//! point, atomic, and vector instructions) end the block and run in
//! the interpreter.
//!
//! Blocks are keyed by both the virtual PC (PC-relative values are
//! compiled in as constants) and the physical location of the code.
//! Generated code keeps the guest registers in SSA values and writes
//! them back on block exit. Loads and stores inline the TLB lookup and
//! access guest RAM directly on a hit; misses, MMIO, and faults call
//! back into the CPU. A block returns the number of instructions it
//! executed so that the CPU time and instruction counters advance
//! exactly as in the interpreter.
//!
//! RISC-V requires fence.i before executing modified code, so fence.i
//! starts a new epoch. A block compiled in an older epoch is checked
//! against the current guest code bytes before it runs again.

use super::Cpu;
use crate::isa::{self, Exception, Instr, Op};
use crate::memory::PAGE_SIZE;
use crate::mmu::{TlbEntry, PTE_D, PTE_R, PTE_W};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlagsData, Signature, Type, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};
use std::collections::HashMap;
use std::mem::offset_of;
use std::time::{Duration, Instant};

/// Compiled block entry point. Returns the number of executed
/// instructions in the low 32 bits and the exit status in the high
/// bits.
type BlockFn = unsafe extern "C" fn(*mut Cpu) -> u64;

/// Exit status: the block completed and PC holds the next PC.
const EXIT_OK: u64 = 0;
/// Exit status: an instruction raised an exception, which is stored
/// in `Cpu::jit_exception`; PC holds the faulting instruction.
const EXIT_FAULT: u64 = 1;

const CACHE_BITS: u32 = 16;
const CACHE_SIZE: usize = 1 << CACHE_BITS;
const MAX_BLOCK_INSTRS: usize = 200;

/// Default number of times the interpreter must reach a block start
/// before the block is compiled.
pub const DEFAULT_HOT_THRESHOLD: u16 = 50;

/// JIT statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitStats {
    pub compiled: u64,
    pub compiled_instrs: u64,
    pub compile_time: Duration,
    pub invalidated: u64,
    pub block_runs: u64,
}

impl std::fmt::Display for JitStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "JIT: {} blocks ({} instructions) compiled in {:?}, {} block runs, {} invalidated",
            self.compiled, self.compiled_instrs, self.compile_time, self.block_runs, self.invalidated
        )
    }
}

#[derive(Clone, Copy, Default)]
struct CacheEntry {
    vpc: u64,
    /// RAM offset of the block start.
    phys: u64,
    /// Block index + 1; 0 marks an empty entry.
    block: u32,
}

struct Block {
    func: BlockFn,
    epoch: u64,
    phys: u64,
    /// Guest code bytes the block was compiled from.
    code: Vec<u8>,
}

/// The JIT compiler and its translation cache.
pub struct Jit {
    module: JITModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    cache: Box<[CacheEntry]>,
    blocks: Vec<Block>,
    index: HashMap<(u64, u64), u32>,
    hot: Box<[u16]>,
    hot_threshold: u16,
    epoch: u64,
    pub stats: JitStats,
}

// SAFETY: the JIT is owned by the CPU and only used from the thread
// running the CPU.
unsafe impl Send for Jit {}

/// Marks the RAM page at the offset as containing compiled code.
fn mark_code_page(code_pages: &mut [u64], ofs: u64) {
    let page = ofs >> 12;
    code_pages[(page >> 6) as usize] |= 1 << (page & 63);
}

#[inline]
fn slot(phys: u64) -> usize {
    (((phys >> 1) ^ (phys >> (CACHE_BITS + 1))) as usize) & (CACHE_SIZE - 1)
}

/// Tests if the JIT can compile the instruction.
pub(super) fn jitable(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        Add | Sub | Sll | Slt | Sltu | Xor | Srl | Sra | Or | And | Addi | Slti | Sltiu | Xori
            | Ori | Andi | Slli | Srli | Srai | Addw | Subw | Sllw | Srlw | Sraw | Addiw | Slliw
            | SlliUw | Srliw | Sraiw | Mul | Mulh | Mulhsu | Mulhu | Div | Divu | Rem | Remu
            | Mulw | Divw | Divuw | Remw | Remuw | Lb | Lh | Lw | Ld | Lbu | Lhu | Lwu | Sb | Sh
            | Sw | Sd | Beq | Bne | Blt | Bge | Bltu | Bgeu | Jal | Jalr | Lui | Auipc | Fence
            | CzeroEqz | CzeroNez | Bclr | Bclri | Bext | Bexti | Binv | Binvi | Bset | Bseti
            | AddUw | Sh1add | Sh2add | Sh3add | Sh1addUw | Sh2addUw | Sh3addUw | Andn | Orn
            | Xnor | Clz | Clzw | Ctz | Ctzw | Cpop | Cpopw | Max | Maxu | Min | Minu | SextB
            | SextH | ZextH | Rol | Rolw | Ror | Rorw | Rori | Roriw | Rev8
    )
}

fn is_terminator(op: Op) -> bool {
    use Op::*;
    matches!(op, Beq | Bne | Blt | Bge | Bltu | Bgeu | Jal | Jalr)
}

/// Loads a value from guest memory for the generated code.
unsafe extern "C" fn jit_load(cpu: *mut Cpu, addr: u64, size: u64) -> u64 {
    let cpu = &mut *cpu;
    let r = match size {
        1 => cpu.load8(addr).map(|v| v as u64),
        2 => cpu.load16(addr).map(|v| v as u64),
        4 => cpu.load32(addr).map(|v| v as u64),
        _ => cpu.load64(addr),
    };
    match r {
        Ok(v) => v,
        Err(e) => {
            cpu.jit_exception = Some(e);
            cpu.jit_fault = 1;
            0
        }
    }
}

/// Stores a value to guest memory for the generated code.
unsafe extern "C" fn jit_store(cpu: *mut Cpu, addr: u64, size: u64, v: u64) {
    let cpu = &mut *cpu;
    let r = match size {
        1 => cpu.store8(addr, v as u8),
        2 => cpu.store16(addr, v as u16),
        4 => cpu.store32(addr, v as u32),
        _ => cpu.store64(addr, v),
    };
    match r {
        Ok(()) => cpu.reservation_valid = false,
        Err(e) => {
            cpu.jit_exception = Some(e);
            cpu.jit_fault = 1;
        }
    }
}

/// A decoded instruction of the block.
struct BlockInstr {
    pc: u64,
    instr: Instr,
    size: u64,
}

impl Jit {
    pub fn new(hot_threshold: u16) -> Result<Jit, String> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").map_err(|e| e.to_string())?;
        flags
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        flags.set("is_pic", "false").map_err(|e| e.to_string())?;
        let isa = cranelift_native::builder()
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        let module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let ctx = module.make_context();
        Ok(Jit {
            module,
            ctx,
            fctx: FunctionBuilderContext::new(),
            cache: vec![CacheEntry::default(); CACHE_SIZE].into_boxed_slice(),
            blocks: Vec::new(),
            index: HashMap::new(),
            hot: vec![0u16; CACHE_SIZE].into_boxed_slice(),
            hot_threshold: hot_threshold.max(1),
            epoch: 0,
            stats: JitStats::default(),
        })
    }

    /// Starts a new code epoch (fence.i).
    pub fn new_epoch(&mut self) {
        self.epoch += 1;
    }

    /// Returns the compiled block for the code, validating blocks from
    /// earlier epochs.
    fn lookup(
        &mut self,
        cpu_mem: &crate::memory::Memory,
        code_pages: &mut [u64],
        vpc: u64,
        phys: u64,
    ) -> Option<BlockFn> {
        let s = slot(phys);
        let e = self.cache[s];
        let idx = if e.block != 0 && e.vpc == vpc && e.phys == phys {
            e.block - 1
        } else if e.block != 0 {
            let idx = *self.index.get(&(vpc, phys))?;
            self.cache[s] = CacheEntry {
                vpc,
                phys,
                block: idx + 1,
            };
            idx
        } else {
            return None;
        };
        let b = &mut self.blocks[idx as usize];
        if b.epoch != self.epoch {
            let mut cur = vec![0u8; b.code.len()];
            cpu_mem.read_bytes(b.phys, &mut cur);
            if cur != b.code {
                self.stats.invalidated += 1;
                self.index.remove(&(vpc, phys));
                self.cache[s] = CacheEntry::default();
                return None;
            }
            b.epoch = self.epoch;
            mark_code_page(code_pages, b.phys);
        }
        Some(b.func)
    }

    /// Counts a visit to the block start. Returns true if the block
    /// became hot.
    fn bump(&mut self, phys: u64) -> bool {
        let h = &mut self.hot[slot(phys)];
        *h = h.saturating_add(1);
        if *h >= self.hot_threshold {
            *h = 0;
            return true;
        }
        false
    }

    /// Decodes the block starting at the RAM offset phys.
    fn decode_block(cpu: &Cpu, vpc: u64, phys: u64) -> Vec<BlockInstr> {
        let mem = &cpu.mem;
        let page_end = (phys & !(PAGE_SIZE - 1)) + PAGE_SIZE;
        let mut out = Vec::new();
        let (mut pc, mut ofs) = (vpc, phys);
        while out.len() < MAX_BLOCK_INSTRS && ofs + 2 <= page_end {
            let lo = mem.read_u16(ofs) as u32;
            let (instr, size) = if lo & 0b11 == 0b11 {
                if ofs + 4 > page_end {
                    break;
                }
                (isa::decode(mem.read_u32(ofs)).unwrap_or_default(), 4)
            } else {
                (isa::decode_c_fast(lo as u16), 2)
            };
            if !jitable(instr.op) {
                break;
            }
            out.push(BlockInstr { pc, instr, size });
            pc = pc.wrapping_add(size);
            ofs += size;
            if is_terminator(instr.op) {
                break;
            }
        }
        out
    }

    /// Compiles the block starting at vpc / phys. Returns the block
    /// function or None if the block is empty.
    fn compile(&mut self, cpu: &Cpu, vpc: u64, phys: u64) -> Result<Option<BlockFn>, String> {
        let start = Instant::now();
        let instrs = Self::decode_block(cpu, vpc, phys);
        let Some(last) = instrs.last() else {
            return Ok(None);
        };
        let code_len = (last.pc - vpc + last.size) as usize;

        let ptr = self.module.target_config().pointer_type();
        let mut sig = Signature::new(self.module.isa().default_call_conv());
        sig.params.push(AbiParam::new(ptr));
        sig.returns.push(AbiParam::new(types::I64));
        self.ctx.func.signature = sig;

        let env = CodegenEnv {
            ram_ptr: cpu.mem.ptr_at(0) as i64,
            ram_base: cpu.mem.ram_base as i64,
            ram_size: cpu.mem.size() as i64,
            tlb: cpu.tlb.as_ptr() as i64,
            code_pages: cpu.jit_code_pages.as_ptr() as i64,
            inline_stores: cpu.overlay.is_none(),
        };

        let frontend_config = self.module.isa().frontend_config();
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let mut t = Translator::new(&mut b, ptr, env);
            t.translate(&instrs);
            b.seal_all_blocks();
            b.finalize(frontend_config);
        }

        let id = self
            .module
            .declare_anonymous_function(&self.ctx.func.signature)
            .map_err(|e| e.to_string())?;
        self.module
            .define_function(id, &mut self.ctx)
            .map_err(|e| format!("JIT compile failed at {:x}: {:?}", vpc, e))?;
        self.module.clear_context(&mut self.ctx);
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        let code = self.module.get_finalized_function(id);
        // SAFETY: the function was compiled with the BlockFn signature.
        let func: BlockFn = unsafe { std::mem::transmute(code) };

        let mut guest = vec![0u8; code_len];
        cpu.mem.read_bytes(phys, &mut guest);
        let idx = self.blocks.len() as u32;
        self.blocks.push(Block {
            func,
            epoch: self.epoch,
            phys,
            code: guest,
        });
        self.index.insert((vpc, phys), idx);
        self.cache[slot(phys)] = CacheEntry {
            vpc,
            phys,
            block: idx + 1,
        };

        self.stats.compiled += 1;
        self.stats.compiled_instrs += instrs.len() as u64;
        self.stats.compile_time += start.elapsed();
        Ok(Some(func))
    }
}

/// Constants embedded in the generated code.
#[derive(Clone, Copy)]
struct CodegenEnv {
    ram_ptr: i64,
    ram_base: i64,
    ram_size: i64,
    tlb: i64,
    code_pages: i64,
    inline_stores: bool,
}

const OFS_X: i32 = offset_of!(Cpu, x) as i32;
const OFS_PC: i32 = offset_of!(Cpu, pc) as i32;
const OFS_DATA_KEY: i32 = offset_of!(Cpu, data_key) as i32;
const OFS_FAULT: i32 = offset_of!(Cpu, jit_fault) as i32;
const OFS_RESV: i32 = offset_of!(Cpu, reservation_valid) as i32;

const TLB_VPN: i32 = offset_of!(TlbEntry, vpn) as i32;
const TLB_PAGE: i32 = offset_of!(TlbEntry, page) as i32;
const TLB_MASK: i32 = offset_of!(TlbEntry, offset_mask) as i32;
const TLB_FLAGS: i32 = offset_of!(TlbEntry, flags) as i32;
const TLB_KEY: i32 = offset_of!(TlbEntry, key) as i32;
const TLB_SHIFT: i64 = std::mem::size_of::<TlbEntry>().trailing_zeros() as i64;

const _: () = assert!(std::mem::size_of::<TlbEntry>().is_power_of_two());

struct Translator<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    ptr: Type,
    env: CodegenEnv,
    cpu: Value,
    exit: cranelift_codegen::ir::Block,
    vars: [Option<Variable>; 32],
    written: [bool; 32],
}

impl<'a, 'b> Translator<'a, 'b> {
    fn new(b: &'a mut FunctionBuilder<'b>, ptr: Type, env: CodegenEnv) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let cpu = b.block_params(entry)[0];
        let exit = b.create_block();
        b.append_block_param(exit, types::I64); // next pc
        b.append_block_param(exit, types::I64); // instruction count
        b.append_block_param(exit, types::I64); // status
        Translator {
            b,
            ptr,
            env,
            cpu,
            exit,
            vars: [None; 32],
            written: [false; 32],
        }
    }

    fn flags() -> MemFlagsData {
        MemFlagsData::trusted()
    }

    /// Loads all registers used by the block into variables.
    fn load_regs(&mut self, instrs: &[BlockInstr]) {
        let mut used = [false; 32];
        for bi in instrs {
            let i = &bi.instr;
            used[i.rd as usize] = true;
            used[i.rs1 as usize] = true;
            used[i.rs2 as usize] = true;
        }
        for (r, u) in used.iter().enumerate().skip(1) {
            if !*u {
                continue;
            }
            let var = self.b.declare_var(types::I64);
            let v = self
                .b
                .ins()
                .load(types::I64, Self::flags(), self.cpu, OFS_X + 8 * r as i32);
            self.b.def_var(var, v);
            self.vars[r] = Some(var);
        }
    }

    fn get(&mut self, r: u8) -> Value {
        match self.vars[r as usize] {
            Some(var) if r != 0 => self.b.use_var(var),
            _ => self.b.ins().iconst(types::I64, 0),
        }
    }

    fn set(&mut self, r: u8, v: Value) {
        if r == 0 {
            return;
        }
        let var = self.vars[r as usize].expect("register not loaded");
        self.b.def_var(var, v);
        self.written[r as usize] = true;
    }

    fn iconst(&mut self, v: i64) -> Value {
        self.b.ins().iconst(types::I64, v)
    }

    fn jump_exit(&mut self, pc: Value, count: u64, status: u64) {
        let c = self.iconst(count as i64);
        let s = self.iconst(status as i64);
        self.b.ins().jump(self.exit, &[pc.into(), c.into(), s.into()]);
    }

    fn translate(&mut self, instrs: &[BlockInstr]) {
        self.load_regs(instrs);

        let mut terminated = false;
        for (k, bi) in instrs.iter().enumerate() {
            if self.instr(k as u64, bi) {
                terminated = true;
                break;
            }
        }
        if !terminated {
            let last = instrs.last().unwrap();
            let next = self.iconst(last.pc.wrapping_add(last.size) as i64);
            self.jump_exit(next, instrs.len() as u64, EXIT_OK);
        }

        // The exit block writes back the modified registers.
        self.b.switch_to_block(self.exit);
        let params = self.b.block_params(self.exit).to_vec();
        self.b
            .ins()
            .store(Self::flags(), params[0], self.cpu, OFS_PC);
        for r in 1..32 {
            if self.written[r] {
                let v = self.b.use_var(self.vars[r].unwrap());
                self.b
                    .ins()
                    .store(Self::flags(), v, self.cpu, OFS_X + 8 * r as i32);
            }
        }
        let status = self.b.ins().ishl_imm_s(params[2], 32);
        let ret = self.b.ins().bor(params[1], status);
        self.b.ins().return_(&[ret]);
    }

    fn sext32(&mut self, v: Value) -> Value {
        let n = self.b.ins().ireduce(types::I32, v);
        self.b.ins().sextend(types::I64, n)
    }

    fn lo32(&mut self, v: Value) -> Value {
        self.b.ins().ireduce(types::I32, v)
    }

    fn bool64(&mut self, c: Value) -> Value {
        self.b.ins().uextend(types::I64, c)
    }

    /// Computes the inline TLB hit condition and the host address of
    /// a size byte access with the required PTE flags.
    fn tlb_fast(&mut self, addr: Value, size: i64, need: u8) -> (Value, Value) {
        let vpn = self.b.ins().ushr_imm_s(addr, 12);
        let idx = self.b.ins().band_imm_s(vpn, 0xfff);
        let eofs = self.b.ins().ishl_imm_s(idx, TLB_SHIFT);
        let tlb = self.iconst(self.env.tlb);
        let ent = self.b.ins().iadd(tlb, eofs);
        let f = Self::flags();

        let e_vpn = self.b.ins().load(types::I64, f, ent, TLB_VPN);
        let c_vpn = self.b.ins().icmp(IntCC::Equal, e_vpn, vpn);

        let e_key = self.b.ins().uload8(types::I32, f, ent, TLB_KEY);
        let key = self.b.ins().uload8(types::I32, f, self.cpu, OFS_DATA_KEY);
        let c_key = self.b.ins().icmp(IntCC::Equal, e_key, key);

        let e_flags = self.b.ins().uload8(types::I32, f, ent, TLB_FLAGS);
        let masked = self.b.ins().band_imm_s(e_flags, need as i64);
        let c_flags = self.b.ins().icmp_imm_s(IntCC::Equal, masked, need as i64);

        let pofs = self.b.ins().band_imm_s(addr, 0xfff);
        let c_page = self.b.ins().icmp_imm_s(IntCC::UnsignedLessThanOrEqual, pofs, 0x1000 - size);

        let e_page = self.b.ins().load(types::I64, f, ent, TLB_PAGE);
        let e_mask = self.b.ins().load(types::I64, f, ent, TLB_MASK);
        let off = self.b.ins().band(addr, e_mask);
        let paddr = self.b.ins().bor(e_page, off);
        let ram_ofs = self.b.ins().iadd_imm_s(paddr, -self.env.ram_base);
        let c_ram = self.b.ins().icmp_imm_s(
            IntCC::UnsignedLessThanOrEqual,
            ram_ofs,
            self.env.ram_size - size,
        );

        let c1 = self.b.ins().band(c_vpn, c_key);
        let c2 = self.b.ins().band(c_flags, c_page);
        let c3 = self.b.ins().band(c1, c2);
        let ok = self.b.ins().band(c3, c_ram);

        let ram = self.iconst(self.env.ram_ptr);
        let host = self.b.ins().iadd(ram, ram_ofs);
        (ok, host)
    }

    /// Emits a guest load. Faults exit the block at the instruction.
    fn load(&mut self, k: u64, pc: u64, addr: Value, size: i64, signed: bool) -> Value {
        let (ok, host) = self.tlb_fast(addr, size, PTE_R);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let cont = self.b.create_block();
        let done = self.b.create_block();
        self.b.append_block_param(done, types::I64);
        self.b.ins().brif(ok, fast, &[], slow, &[]);

        // Fast path: direct RAM access.
        self.b.switch_to_block(fast);
        let f = MemFlagsData::new();
        let v = match (size, signed) {
            (1, false) => self.b.ins().uload8(types::I64, f, host, 0),
            (1, true) => self.b.ins().sload8(types::I64, f, host, 0),
            (2, false) => self.b.ins().uload16(types::I64, f, host, 0),
            (2, true) => self.b.ins().sload16(types::I64, f, host, 0),
            (4, false) => self.b.ins().uload32(f, host, 0),
            (4, true) => self.b.ins().sload32(f, host, 0),
            _ => self.b.ins().load(types::I64, f, host, 0),
        };
        self.b.ins().jump(done, &[v.into()]);

        // Slow path: call the CPU.
        self.b.switch_to_block(slow);
        let mut sig = Signature::new(self.b.func.signature.call_conv);
        sig.params.push(AbiParam::new(self.ptr));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let sigref = self.b.import_signature(sig);
        let callee = self.iconst(jit_load as *const () as i64);
        let sz = self.iconst(size);
        let call = self
            .b
            .ins()
            .call_indirect(sigref, callee, &[self.cpu, addr, sz]);
        let raw = self.b.inst_results(call)[0];
        self.check_fault(k, pc, cont);

        self.b.switch_to_block(cont);
        let v = match (size, signed) {
            (1, true) => {
                let n = self.b.ins().ireduce(types::I8, raw);
                self.b.ins().sextend(types::I64, n)
            }
            (2, true) => {
                let n = self.b.ins().ireduce(types::I16, raw);
                self.b.ins().sextend(types::I64, n)
            }
            (4, true) => self.sext32(raw),
            _ => raw,
        };
        self.b.ins().jump(done, &[v.into()]);

        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// Emits a guest store.
    fn store(&mut self, k: u64, pc: u64, addr: Value, size: i64, v: Value) {
        let done = self.b.create_block();
        let slow = self.b.create_block();
        if self.env.inline_stores {
            let (ok, host) = self.tlb_fast(addr, size, PTE_W | PTE_D);
            let check = self.b.create_block();
            let fast = self.b.create_block();
            self.b.ins().brif(ok, check, &[], slow, &[]);

            // Stores to pages with compiled code take the slow path,
            // which invalidates the code.
            self.b.switch_to_block(check);
            let ram = self.iconst(self.env.ram_ptr);
            let ofs = self.b.ins().isub(host, ram);
            let page = self.b.ins().ushr_imm_s(ofs, 12);
            let word = self.b.ins().ushr_imm_s(page, 6);
            let word_ofs = self.b.ins().ishl_imm_s(word, 3);
            let pages = self.iconst(self.env.code_pages);
            let word_addr = self.b.ins().iadd(pages, word_ofs);
            let bits = self.b.ins().load(types::I64, Self::flags(), word_addr, 0);
            let bit = self.b.ins().band_imm_s(page, 63);
            let bits = self.b.ins().ushr(bits, bit);
            let code = self.b.ins().band_imm_s(bits, 1);
            self.b.ins().brif(code, slow, &[], fast, &[]);

            self.b.switch_to_block(fast);
            let f = MemFlagsData::new();
            match size {
                1 => {
                    self.b.ins().istore8(f, v, host, 0);
                }
                2 => {
                    self.b.ins().istore16(f, v, host, 0);
                }
                4 => {
                    self.b.ins().istore32(f, v, host, 0);
                }
                _ => {
                    self.b.ins().store(f, v, host, 0);
                }
            }
            let zero = self.b.ins().iconst(types::I8, 0);
            self.b.ins().store(Self::flags(), zero, self.cpu, OFS_RESV);
            self.b.ins().jump(done, &[]);
        } else {
            self.b.ins().jump(slow, &[]);
        }

        self.b.switch_to_block(slow);
        let mut sig = Signature::new(self.b.func.signature.call_conv);
        sig.params.push(AbiParam::new(self.ptr));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let sigref = self.b.import_signature(sig);
        let callee = self.iconst(jit_store as *const () as i64);
        let sz = self.iconst(size);
        self.b
            .ins()
            .call_indirect(sigref, callee, &[self.cpu, addr, sz, v]);
        self.check_fault(k, pc, done);

        self.b.switch_to_block(done);
    }

    /// Exits the block with a fault if the last helper call faulted;
    /// otherwise continues at cont.
    fn check_fault(&mut self, k: u64, pc: u64, cont: cranelift_codegen::ir::Block) {
        let fault = self
            .b
            .ins()
            .uload8(types::I32, Self::flags(), self.cpu, OFS_FAULT);
        let pcv = self.iconst(pc as i64);
        let count = self.iconst(k as i64 + 1);
        let status = self.iconst(EXIT_FAULT as i64);
        self.b.ins().brif(
            fault,
            self.exit,
            &[pcv.into(), count.into(), status.into()],
            cont,
            &[],
        );
    }

    /// Translates the instruction. Returns true if the instruction
    /// ended the block.
    fn instr(&mut self, k: u64, bi: &BlockInstr) -> bool {
        use Op::*;
        let i = bi.instr;
        let pc = bi.pc;
        let next = pc.wrapping_add(bi.size);
        let imm = i.imm as i64;
        let rd = i.rd;

        macro_rules! r1 {
            () => {
                self.get(i.rs1)
            };
        }
        macro_rules! r2 {
            () => {
                self.get(i.rs2)
            };
        }
        macro_rules! ins {
            () => {
                self.b.ins()
            };
        }

        let v = match i.op {
            Add => {
                let (a, b) = (r1!(), r2!());
                ins!().iadd(a, b)
            }
            Sub => {
                let (a, b) = (r1!(), r2!());
                ins!().isub(a, b)
            }
            Sll => {
                let (a, b) = (r1!(), r2!());
                ins!().ishl(a, b)
            }
            Srl => {
                let (a, b) = (r1!(), r2!());
                ins!().ushr(a, b)
            }
            Sra => {
                let (a, b) = (r1!(), r2!());
                ins!().sshr(a, b)
            }
            Slt => {
                let (a, b) = (r1!(), r2!());
                let c = ins!().icmp(IntCC::SignedLessThan, a, b);
                self.bool64(c)
            }
            Sltu => {
                let (a, b) = (r1!(), r2!());
                let c = ins!().icmp(IntCC::UnsignedLessThan, a, b);
                self.bool64(c)
            }
            Xor => {
                let (a, b) = (r1!(), r2!());
                ins!().bxor(a, b)
            }
            Or => {
                let (a, b) = (r1!(), r2!());
                ins!().bor(a, b)
            }
            And => {
                let (a, b) = (r1!(), r2!());
                ins!().band(a, b)
            }
            Addi => {
                let a = r1!();
                ins!().iadd_imm_s(a, imm)
            }
            Slti => {
                let a = r1!();
                let c = ins!().icmp_imm_s(IntCC::SignedLessThan, a, imm);
                self.bool64(c)
            }
            Sltiu => {
                let a = r1!();
                let c = ins!().icmp_imm_s(IntCC::UnsignedLessThan, a, imm);
                self.bool64(c)
            }
            Xori => {
                let a = r1!();
                ins!().bxor_imm_s(a, imm)
            }
            Ori => {
                let a = r1!();
                ins!().bor_imm_s(a, imm)
            }
            Andi => {
                let a = r1!();
                ins!().band_imm_s(a, imm)
            }
            Slli => {
                let a = r1!();
                ins!().ishl_imm_s(a, imm & 63)
            }
            Srli => {
                let a = r1!();
                ins!().ushr_imm_s(a, imm & 63)
            }
            Srai => {
                let a = r1!();
                ins!().sshr_imm_s(a, imm & 63)
            }
            Addw => {
                let (a, b) = (r1!(), r2!());
                let s = ins!().iadd(a, b);
                self.sext32(s)
            }
            Subw => {
                let (a, b) = (r1!(), r2!());
                let s = ins!().isub(a, b);
                self.sext32(s)
            }
            Sllw | Srlw | Sraw => {
                let (a, b) = (r1!(), r2!());
                let a = self.lo32(a);
                let b = self.lo32(b);
                let s = match i.op {
                    Sllw => ins!().ishl(a, b),
                    Srlw => ins!().ushr(a, b),
                    _ => ins!().sshr(a, b),
                };
                ins!().sextend(types::I64, s)
            }
            Addiw => {
                let a = r1!();
                let s = ins!().iadd_imm_s(a, imm);
                self.sext32(s)
            }
            Slliw | Srliw | Sraiw => {
                let a = r1!();
                let a = self.lo32(a);
                let sh = imm & 31;
                let s = match i.op {
                    Slliw => ins!().ishl_imm_s(a, sh),
                    Srliw => ins!().ushr_imm_s(a, sh),
                    _ => ins!().sshr_imm_s(a, sh),
                };
                ins!().sextend(types::I64, s)
            }
            SlliUw => {
                let a = r1!();
                let a = self.lo32(a);
                let a = ins!().uextend(types::I64, a);
                ins!().ishl_imm_s(a, imm & 63)
            }
            Mul => {
                let (a, b) = (r1!(), r2!());
                ins!().imul(a, b)
            }
            Mulh => {
                let (a, b) = (r1!(), r2!());
                ins!().smulhi(a, b)
            }
            Mulhu => {
                let (a, b) = (r1!(), r2!());
                ins!().umulhi(a, b)
            }
            Mulhsu => {
                let (a, b) = (r1!(), r2!());
                let hi = ins!().umulhi(a, b);
                let neg = ins!().icmp_imm_s(IntCC::SignedLessThan, a, 0);
                let zero = self.iconst(0);
                let adj = ins!().select(neg, b, zero);
                ins!().isub(hi, adj)
            }
            Mulw => {
                let (a, b) = (r1!(), r2!());
                let m = ins!().imul(a, b);
                self.sext32(m)
            }
            Div | Rem => {
                let (a, b) = (r1!(), r2!());
                let zero = ins!().icmp_imm_s(IntCC::Equal, b, 0);
                let amin = ins!().icmp_imm_s(IntCC::Equal, a, i64::MIN);
                let bneg = ins!().icmp_imm_s(IntCC::Equal, b, -1);
                let ovf = ins!().band(amin, bneg);
                let bad = ins!().bor(zero, ovf);
                let one = self.iconst(1);
                let bs = ins!().select(bad, one, b);
                if i.op == Div {
                    let q = ins!().sdiv(a, bs);
                    let q = ins!().select(ovf, a, q);
                    let m1 = self.iconst(-1);
                    ins!().select(zero, m1, q)
                } else {
                    let r = ins!().srem(a, bs);
                    let z = self.iconst(0);
                    let r = ins!().select(ovf, z, r);
                    ins!().select(zero, a, r)
                }
            }
            Divu | Remu => {
                let (a, b) = (r1!(), r2!());
                let zero = ins!().icmp_imm_s(IntCC::Equal, b, 0);
                let one = self.iconst(1);
                let bs = ins!().select(zero, one, b);
                if i.op == Divu {
                    let q = ins!().udiv(a, bs);
                    let m1 = self.iconst(-1);
                    ins!().select(zero, m1, q)
                } else {
                    let r = ins!().urem(a, bs);
                    ins!().select(zero, a, r)
                }
            }
            Divw | Remw => {
                let (a, b) = (r1!(), r2!());
                let a = self.lo32(a);
                let b = self.lo32(b);
                let zero = ins!().icmp_imm_s(IntCC::Equal, b, 0);
                let amin = ins!().icmp_imm_s(IntCC::Equal, a, i32::MIN as i64);
                let bneg = ins!().icmp_imm_s(IntCC::Equal, b, -1);
                let ovf = ins!().band(amin, bneg);
                let bad = ins!().bor(zero, ovf);
                let one = ins!().iconst(types::I32, 1);
                let bs = ins!().select(bad, one, b);
                let r = if i.op == Divw {
                    let q = ins!().sdiv(a, bs);
                    let q = ins!().select(ovf, a, q);
                    let m1 = ins!().iconst(types::I32, -1);
                    ins!().select(zero, m1, q)
                } else {
                    let r = ins!().srem(a, bs);
                    let z = ins!().iconst(types::I32, 0);
                    let r = ins!().select(ovf, z, r);
                    ins!().select(zero, a, r)
                };
                ins!().sextend(types::I64, r)
            }
            Divuw | Remuw => {
                let (a, b) = (r1!(), r2!());
                let a = self.lo32(a);
                let b = self.lo32(b);
                let zero = ins!().icmp_imm_s(IntCC::Equal, b, 0);
                let one = ins!().iconst(types::I32, 1);
                let bs = ins!().select(zero, one, b);
                let r = if i.op == Divuw {
                    let q = ins!().udiv(a, bs);
                    let m1 = ins!().iconst(types::I32, -1);
                    ins!().select(zero, m1, q)
                } else {
                    let r = ins!().urem(a, bs);
                    ins!().select(zero, a, r)
                };
                ins!().sextend(types::I64, r)
            }

            Lb | Lh | Lw | Ld | Lbu | Lhu | Lwu => {
                let base = r1!();
                let addr = ins!().iadd_imm_s(base, imm);
                let (size, signed) = match i.op {
                    Lb => (1, true),
                    Lh => (2, true),
                    Lw => (4, true),
                    Ld => (8, false),
                    Lbu => (1, false),
                    Lhu => (2, false),
                    _ => (4, false),
                };
                self.load(k, pc, addr, size, signed)
            }
            Sb | Sh | Sw | Sd => {
                let base = r1!();
                let addr = ins!().iadd_imm_s(base, imm);
                let v = r2!();
                let size = match i.op {
                    Sb => 1,
                    Sh => 2,
                    Sw => 4,
                    _ => 8,
                };
                self.store(k, pc, addr, size, v);
                return false;
            }

            Beq | Bne | Blt | Bge | Bltu | Bgeu => {
                let (a, b) = (r1!(), r2!());
                let cc = match i.op {
                    Beq => IntCC::Equal,
                    Bne => IntCC::NotEqual,
                    Blt => IntCC::SignedLessThan,
                    Bge => IntCC::SignedGreaterThanOrEqual,
                    Bltu => IntCC::UnsignedLessThan,
                    _ => IntCC::UnsignedGreaterThanOrEqual,
                };
                let c = ins!().icmp(cc, a, b);
                let target = self.iconst(pc.wrapping_add(imm as u64) as i64);
                let fall = self.iconst(next as i64);
                let count = self.iconst(k as i64 + 1);
                let status = self.iconst(EXIT_OK as i64);
                ins!().brif(
                    c,
                    self.exit,
                    &[target.into(), count.into(), status.into()],
                    self.exit,
                    &[fall.into(), count.into(), status.into()],
                );
                return true;
            }
            Jal => {
                let link = self.iconst(next as i64);
                self.set(rd, link);
                let target = self.iconst(pc.wrapping_add(imm as u64) as i64);
                self.jump_exit(target, k + 1, EXIT_OK);
                return true;
            }
            Jalr => {
                let base = r1!();
                let t = ins!().iadd_imm_s(base, imm);
                let target = ins!().band_imm_s(t, !1);
                let link = self.iconst(next as i64);
                self.set(rd, link);
                self.jump_exit(target, k + 1, EXIT_OK);
                return true;
            }
            Lui => self.iconst(imm),
            Auipc => self.iconst(pc.wrapping_add(imm as u64) as i64),
            Fence => return false,

            CzeroEqz | CzeroNez => {
                let (a, b) = (r1!(), r2!());
                let cc = if i.op == CzeroEqz {
                    IntCC::Equal
                } else {
                    IntCC::NotEqual
                };
                let c = ins!().icmp_imm_s(cc, b, 0);
                let z = self.iconst(0);
                ins!().select(c, z, a)
            }
            Bclr | Binv | Bset | Bext => {
                let (a, b) = (r1!(), r2!());
                let one = self.iconst(1);
                let bit = ins!().ishl(one, b);
                match i.op {
                    Bclr => ins!().band_not(a, bit),
                    Binv => ins!().bxor(a, bit),
                    Bset => ins!().bor(a, bit),
                    _ => {
                        let s = ins!().ushr(a, b);
                        ins!().band_imm_s(s, 1)
                    }
                }
            }
            Bclri | Binvi | Bseti | Bexti => {
                let a = r1!();
                let bit = 1i64 << (imm & 63);
                match i.op {
                    Bclri => ins!().band_imm_s(a, !bit),
                    Binvi => ins!().bxor_imm_s(a, bit),
                    Bseti => ins!().bor_imm_s(a, bit),
                    _ => {
                        let s = ins!().ushr_imm_s(a, imm & 63);
                        ins!().band_imm_s(s, 1)
                    }
                }
            }
            AddUw | Sh1addUw | Sh2addUw | Sh3addUw => {
                let (a, b) = (r1!(), r2!());
                let a = self.lo32(a);
                let a = ins!().uextend(types::I64, a);
                let sh = match i.op {
                    AddUw => 0,
                    Sh1addUw => 1,
                    Sh2addUw => 2,
                    _ => 3,
                };
                let a = ins!().ishl_imm_s(a, sh);
                ins!().iadd(a, b)
            }
            Sh1add | Sh2add | Sh3add => {
                let (a, b) = (r1!(), r2!());
                let sh = match i.op {
                    Sh1add => 1,
                    Sh2add => 2,
                    _ => 3,
                };
                let a = ins!().ishl_imm_s(a, sh);
                ins!().iadd(a, b)
            }
            Andn => {
                let (a, b) = (r1!(), r2!());
                ins!().band_not(a, b)
            }
            Orn => {
                let (a, b) = (r1!(), r2!());
                ins!().bor_not(a, b)
            }
            Xnor => {
                let (a, b) = (r1!(), r2!());
                ins!().bxor_not(a, b)
            }
            Clz => {
                let a = r1!();
                ins!().clz(a)
            }
            Ctz => {
                let a = r1!();
                ins!().ctz(a)
            }
            Cpop => {
                let a = r1!();
                ins!().popcnt(a)
            }
            Clzw | Ctzw | Cpopw => {
                let a = r1!();
                let a = self.lo32(a);
                let r = match i.op {
                    Clzw => ins!().clz(a),
                    Ctzw => ins!().ctz(a),
                    _ => ins!().popcnt(a),
                };
                ins!().uextend(types::I64, r)
            }
            Max => {
                let (a, b) = (r1!(), r2!());
                ins!().smax(a, b)
            }
            Maxu => {
                let (a, b) = (r1!(), r2!());
                ins!().umax(a, b)
            }
            Min => {
                let (a, b) = (r1!(), r2!());
                ins!().smin(a, b)
            }
            Minu => {
                let (a, b) = (r1!(), r2!());
                ins!().umin(a, b)
            }
            SextB | SextH | ZextH => {
                let a = r1!();
                match i.op {
                    SextB => {
                        let n = ins!().ireduce(types::I8, a);
                        ins!().sextend(types::I64, n)
                    }
                    SextH => {
                        let n = ins!().ireduce(types::I16, a);
                        ins!().sextend(types::I64, n)
                    }
                    _ => {
                        let n = ins!().ireduce(types::I16, a);
                        ins!().uextend(types::I64, n)
                    }
                }
            }
            Rol => {
                let (a, b) = (r1!(), r2!());
                ins!().rotl(a, b)
            }
            Ror => {
                let (a, b) = (r1!(), r2!());
                ins!().rotr(a, b)
            }
            Rori => {
                let a = r1!();
                ins!().rotr_imm_s(a, imm & 63)
            }
            Rolw | Rorw => {
                let (a, b) = (r1!(), r2!());
                let a = self.lo32(a);
                let b = self.lo32(b);
                let r = if i.op == Rolw {
                    ins!().rotl(a, b)
                } else {
                    ins!().rotr(a, b)
                };
                ins!().sextend(types::I64, r)
            }
            Roriw => {
                let a = r1!();
                let a = self.lo32(a);
                let r = ins!().rotr_imm_s(a, imm & 31);
                ins!().sextend(types::I64, r)
            }
            Rev8 => {
                let a = r1!();
                ins!().bswap(a)
            }
            op => unreachable!("not jitable: {}", op),
        };
        self.set(rd, v);
        false
    }
}

impl Cpu {
    /// Enables the JIT compiler. Blocks are compiled after the
    /// interpreter has reached them hot_threshold times.
    pub fn enable_jit(&mut self, hot_threshold: u16) -> Result<(), String> {
        self.jit = Some(Box::new(Jit::new(hot_threshold)?));
        let pages = self.mem.size().div_ceil(PAGE_SIZE);
        self.jit_code_pages = vec![0; pages.div_ceil(64) as usize];
        Ok(())
    }

    /// Returns the JIT statistics if the JIT is enabled.
    pub fn jit_stats(&self) -> Option<JitStats> {
        self.jit.as_ref().map(|j| j.stats)
    }

    /// Handles a CPU store to the RAM offset: if the page contains
    /// compiled code, the code is revalidated before it runs again.
    #[inline(never)]
    pub(super) fn jit_code_write(&mut self, ofs: u64) {
        let page = ofs >> 12;
        let word = &mut self.jit_code_pages[(page >> 6) as usize];
        let bit = 1 << (page & 63);
        if *word & bit != 0 {
            *word &= !bit;
            if let Some(jit) = self.jit.as_mut() {
                jit.new_epoch();
            }
        }
    }

    /// Starts a new JIT code epoch after fence.i.
    pub(super) fn jit_fence_i(&mut self) {
        if let Some(jit) = self.jit.as_mut() {
            jit.new_epoch();
        }
    }

    /// Runs the compiled block at PC if there is one. Returns Ok(true)
    /// if a block was executed.
    pub(super) fn try_jit(&mut self) -> Result<bool, Exception> {
        let pc = self.pc;
        if crate::memory::page(pc) != self.code_vpn {
            self.load_code_page(pc)?;
        }
        let phys = self.code_ofs + crate::memory::page_offset(pc);

        let Some(jit) = self.jit.as_mut() else {
            return Ok(false);
        };
        let func = match jit.lookup(&self.mem, &mut self.jit_code_pages, pc, phys) {
            Some(f) => f,
            None => {
                if !jit.bump(phys) {
                    return Ok(false);
                }
                // Temporarily take the JIT to compile with a shared
                // view of the CPU.
                let mut jit = self.jit.take().unwrap();
                let r = jit.compile(self, pc, phys);
                self.jit = Some(jit);
                mark_code_page(&mut self.jit_code_pages, phys);
                match r.map_err(Exception::fatal)? {
                    Some(f) => f,
                    None => return Ok(false),
                }
            }
        };

        // SAFETY: the block was compiled for this CPU's memory and TLB
        // layout and accesses the CPU only through the pointer.
        let r = unsafe { func(self as *mut Cpu) };
        let count = r & 0xffff_ffff;
        self.time = self.time.wrapping_add(count);
        self.instret = self.instret.wrapping_add(count);
        if let Some(jit) = self.jit.as_mut() {
            jit.stats.block_runs += 1;
        }
        if r >> 32 == EXIT_FAULT {
            self.jit_fault = 0;
            return Err(self
                .jit_exception
                .take()
                .unwrap_or_else(|| Exception::fatal("JIT fault without exception")));
        }
        Ok(true)
    }
}
