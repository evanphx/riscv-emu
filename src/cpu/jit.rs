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
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::hash::{BuildHasherDefault, Hasher};
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
pub const DEFAULT_HOT_THRESHOLD: u16 = 3000;

/// JIT statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitStats {
    pub compiled: u64,
    pub compiled_instrs: u64,
    pub compile_time: Duration,
    pub invalidated: u64,
    pub block_runs: u64,
    /// Instructions executed by compiled code.
    pub jit_instrs: u64,
}

impl std::fmt::Display for JitStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "JIT: {} blocks ({} instructions) compiled in {:?}, {} block runs, \
             {} instructions executed, {} invalidated",
            self.compiled,
            self.compiled_instrs,
            self.compile_time,
            self.block_runs,
            self.jit_instrs,
            self.invalidated
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

/// Size of the executable code arena. The translation cache is
/// flushed when the arena is full.
const CODE_ARENA_SIZE: usize = 256 << 20;

/// Executable memory for the generated code. The generated code is
/// position independent and has no relocations, so it is copied into
/// the arena as is.
struct CodeArena {
    ptr: *mut u8,
    size: usize,
    pos: usize,
}

impl CodeArena {
    fn new(size: usize) -> Result<CodeArena, String> {
        // SAFETY: anonymous private mapping.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "failed to map JIT code memory: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(CodeArena {
            ptr: ptr as *mut u8,
            size,
            pos: 0,
        })
    }

    /// Copies the code into the arena. Returns None if the arena is
    /// full.
    fn add(&mut self, code: &[u8]) -> Option<*const u8> {
        let pos = (self.pos + 15) & !15;
        if pos + code.len() > self.size {
            return None;
        }
        // SAFETY: the range is inside the mapping and not used by any
        // other code.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.ptr.add(pos), code.len());
        }
        self.pos = pos + code.len();
        // SAFETY: pos is inside the mapping.
        Some(unsafe { self.ptr.add(pos) })
    }

    fn reset(&mut self) {
        self.pos = 0;
    }
}

impl Drop for CodeArena {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping created in new().
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.size);
        }
    }
}

/// The JIT compiler and its translation cache.
pub struct Jit {
    /// Compiler for synchronous compilation.
    compiler: Option<Compiler>,
    /// Background compiler.
    worker: Option<Worker>,
    /// Blocks being compiled in the background.
    pending: HashSet<(u64, u64), BuildHasherDefault<FxHasher>>,
    /// Counter for polling the background compiler.
    poll: u32,
    arena: CodeArena,
    cache: Box<[CacheEntry]>,
    blocks: Vec<Block>,
    index: HashMap<(u64, u64), u32, BuildHasherDefault<FxHasher>>,
    hot: Box<[u16]>,
    hot_threshold: u16,
    epoch: u64,
    pub stats: JitStats,
}

// SAFETY: the JIT is owned by the CPU and only used from the thread
// running the CPU.
unsafe impl Send for Jit {}

/// A fast hasher for the block index keys.
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

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
    native_fp(op)
        || helper_op(op)
        || matches!(
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

/// Floating point loads, stores, and moves compiled to native code.
fn native_fp(op: Op) -> bool {
    use Op::*;
    matches!(op, Flw | Fld | Fsw | Fsd | FmvXW | FmvXD | FmvWX | FmvDX)
}

/// Instructions that compiled code executes by calling the
/// interpreter: they neither transfer control nor change the
/// privilege or translation state.
fn helper_op(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        LrW | ScW | AmoaddW | AmoswapW | AmoxorW | AmoandW | AmoorW | AmominW | AmomaxW
            | AmominuW | AmomaxuW | LrD | ScD | AmoaddD | AmoswapD | AmoxorD | AmoandD
            | AmoorD | AmominD | AmomaxD | AmominuD | AmomaxuD | FaddS | FaddD | FsubS
            | FsubD | FmulS | FmulD | FdivS | FdivD | FsqrtS | FsqrtD | FmaddS | FmaddD
            | FmsubS | FmsubD | FnmsubS | FnmsubD | FnmaddS | FnmaddD | FsgnjS | FsgnjnS
            | FsgnjxS | FsgnjD | FsgnjnD | FsgnjxD | FminS | FmaxS | FminD | FmaxD | FeqS
            | FltS | FleS | FeqD | FltD | FleD | FcvtWS | FcvtWuS | FcvtLS | FcvtLuS | FcvtWD
            | FcvtWuD | FcvtLD | FcvtLuD | FcvtSW | FcvtSWu | FcvtSL | FcvtSLu | FcvtDW
            | FcvtDWu | FcvtDL | FcvtDLu | FcvtSD | FcvtDS | FclassS | FclassD | Vsetvli
            | Vsetivli | Vsetvl | VandVX | VxorVV | VmsneVV | VidV | VmvVX | VmvVV | VmvVI
            | Vle8V | Vle16V | Vle32V | Vle64V | Vse8V | Vse16V | Vse32V | Vse64V | VaddVI
            | VmseqVI | VsllVI | VsraVI | Clmul | Clmulh | Clmulr | Pack | Packh | Packw
            | Brev8 | Xperm4 | Xperm8 | OrcB
    )
}

/// Packs the instruction for the interpreter helper.
fn pack_instr(i: &Instr) -> u64 {
    i.op as u64
        | (i.rd as u64) << 8
        | (i.rs1 as u64) << 16
        | (i.rs2 as u64) << 24
        | (i.imm as u32 as u64) << 32
}

fn unpack_instr(v: u64) -> Instr {
    Instr {
        op: Op::ALL[(v & 0xff) as usize],
        rd: (v >> 8) as u8,
        rs1: (v >> 16) as u8,
        rs2: (v >> 24) as u8,
        imm: (v >> 32) as u32 as i32,
    }
}

/// Executes the instruction in the interpreter for the generated code.
unsafe extern "C" fn jit_exec(cpu: *mut Cpu, packed: u64, raw: u64) {
    let cpu = &mut *cpu;
    let instr = unpack_instr(packed);
    let size = if raw & 0b11 == 0b11 { 4 } else { 2 };
    if let Err(e) = cpu.execute(instr, raw as u32, size) {
        cpu.jit_exception = Some(e);
        cpu.jit_fault = 1;
    }
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
    raw: u32,
    size: u64,
    /// The instruction ends the block.
    last: bool,
}

impl Jit {
    /// Creates a new JIT. With background compilation, hot blocks
    /// are compiled on a separate thread while the interpreter
    /// continues; otherwise they are compiled immediately.
    pub fn new(hot_threshold: u16, background: bool) -> Result<Jit, String> {
        let (compiler, worker) = if background {
            (None, Some(Worker::start()?))
        } else {
            (Some(Compiler::new()?), None)
        };
        Ok(Jit {
            compiler,
            worker,
            pending: HashSet::default(),
            poll: 0,
            arena: CodeArena::new(CODE_ARENA_SIZE)?,
            cache: vec![CacheEntry::default(); CACHE_SIZE].into_boxed_slice(),
            blocks: Vec::new(),
            index: HashMap::default(),
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

    /// Decodes the trace starting at vpc, with its code at the RAM
    /// offset phys. The trace follows not-taken conditional branches
    /// and direct jumps within the page.
    fn decode_block(cpu: &Cpu, vpc: u64, phys: u64) -> Vec<BlockInstr> {
        let mem = &cpu.mem;
        let page_base = phys & !(PAGE_SIZE - 1);
        let page_end = page_base + PAGE_SIZE;
        let mut out: Vec<BlockInstr> = Vec::new();
        let (mut pc, mut ofs) = (vpc, phys);
        loop {
            if out.len() >= MAX_BLOCK_INSTRS || ofs + 2 > page_end {
                break;
            }
            let lo = mem.read_u16(ofs) as u32;
            let (instr, size, raw) = if lo & 0b11 == 0b11 {
                if ofs + 4 > page_end {
                    break;
                }
                let raw = mem.read_u32(ofs);
                (isa::decode(raw).unwrap_or_default(), 4, raw)
            } else {
                (isa::decode_c_fast(lo as u16), 2, lo)
            };
            if !jitable(instr.op) {
                break;
            }
            out.push(BlockInstr {
                pc,
                instr,
                raw,
                size,
                last: false,
            });
            match instr.op {
                Op::Jalr => break,
                Op::Jal => {
                    let target = pc.wrapping_add(instr.imm as i64 as u64);
                    // Follow jumps within the page that do not loop
                    // back into the trace.
                    if target >> 12 != vpc >> 12 || out.iter().any(|b| b.pc == target) {
                        break;
                    }
                    pc = target;
                    ofs = page_base + (target & (PAGE_SIZE - 1));
                }
                _ => {
                    pc = pc.wrapping_add(size);
                    ofs += size;
                }
            }
            if out.iter().any(|b| b.pc == pc) {
                break;
            }
        }
        if let Some(l) = out.last_mut() {
            l.last = is_terminator(l.instr.op);
        }
        out
    }

    /// Creates the compile request for the block at vpc / phys.
    fn request(cpu: &Cpu, vpc: u64, phys: u64, epoch: u64) -> Option<Request> {
        let instrs = Self::decode_block(cpu, vpc, phys);
        if instrs.is_empty() {
            return None;
        }
        // Snapshot the guest code for revalidation.
        let page_base = phys & !(PAGE_SIZE - 1);
        let lo = instrs.iter().map(|b| b.pc & (PAGE_SIZE - 1)).min().unwrap();
        let hi = instrs
            .iter()
            .map(|b| (b.pc & (PAGE_SIZE - 1)) + b.size)
            .max()
            .unwrap();
        let mut code = vec![0u8; (hi - lo) as usize];
        cpu.mem.read_bytes(page_base + lo, &mut code);
        Some(Request {
            vpc,
            phys,
            epoch,
            code_ofs: page_base + lo,
            code,
            instrs,
            env: CodegenEnv {
                ram_ptr: cpu.mem.ptr_at(0) as i64,
                tlb: cpu.tlb.as_ptr() as i64,
                code_pages: cpu.jit_code_pages.as_ptr() as i64,
                inline_stores: cpu.overlay.is_none(),
            },
        })
    }

    /// Installs the compiled code. Returns the block function.
    fn install(&mut self, r: Compiled) -> Result<BlockFn, String> {
        let entry = match self.arena.add(&r.machine_code) {
            Some(p) => p,
            None => {
                // The arena is full: drop all compiled code.
                self.cache.fill(CacheEntry::default());
                self.blocks.clear();
                self.index.clear();
                self.arena.reset();
                self.arena
                    .add(&r.machine_code)
                    .ok_or("JIT block too large")?
            }
        };
        // SAFETY: the code was compiled with the BlockFn signature.
        let func: BlockFn = unsafe { std::mem::transmute(entry) };
        let req = r.request;
        let idx = self.blocks.len() as u32;
        self.blocks.push(Block {
            func,
            epoch: req.epoch,
            phys: req.code_ofs,
            code: req.code,
        });
        self.index.insert((req.vpc, req.phys), idx);
        self.cache[slot(req.phys)] = CacheEntry {
            vpc: req.vpc,
            phys: req.phys,
            block: idx + 1,
        };
        self.stats.compiled += 1;
        self.stats.compiled_instrs += req.instrs.len() as u64;
        self.stats.compile_time += r.time;
        Ok(func)
    }

    /// Installs the blocks compiled in the background.
    fn install_pending(&mut self, code_pages: &mut [u64]) -> Result<(), String> {
        loop {
            let Some(r) = self.worker.as_ref().and_then(|w| w.rx.try_recv().ok()) else {
                return Ok(());
            };
            let r = r?;
            self.pending.remove(&(r.request.vpc, r.request.phys));
            mark_code_page(code_pages, r.request.code_ofs);
            self.install(r)?;
        }
    }
}

/// A block compile request.
struct Request {
    vpc: u64,
    /// RAM offset of the block start.
    phys: u64,
    epoch: u64,
    /// RAM offset and bytes of the guest code of the block.
    code_ofs: u64,
    code: Vec<u8>,
    instrs: Vec<BlockInstr>,
    env: CodegenEnv,
}

/// A compiled block.
struct Compiled {
    request: Request,
    machine_code: Vec<u8>,
    time: Duration,
}

/// The Cranelift code generator.
struct Compiler {
    isa: OwnedTargetIsa,
    ctx: Context,
    fctx: FunctionBuilderContext,
}

impl Compiler {
    fn new() -> Result<Compiler, String> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").map_err(|e| e.to_string())?;
        flags
            .set("enable_verifier", "false")
            .map_err(|e| e.to_string())?;
        let isa = cranelift_native::builder()
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        Ok(Compiler {
            isa,
            ctx: Context::new(),
            fctx: FunctionBuilderContext::new(),
        })
    }

    /// Compiles the block into machine code.
    fn compile(&mut self, req: Request) -> Result<Compiled, String> {
        let start = Instant::now();
        let ptr = self.isa.pointer_type();
        self.ctx.clear();
        let mut sig = Signature::new(self.isa.default_call_conv());
        sig.params.push(AbiParam::new(ptr));
        sig.returns.push(AbiParam::new(types::I64));
        self.ctx.func.signature = sig;

        let frontend_config = self.isa.frontend_config();
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let mut t = Translator::new(&mut b, ptr, req.env, req.vpc);
            t.translate(&req.instrs);
            b.seal_all_blocks();
            b.finalize(frontend_config);
        }

        let compiled = self
            .ctx
            .compile(&*self.isa, &mut ControlPlane::default())
            .map_err(|e| format!("JIT compile failed at {:x}: {:?}", req.vpc, e.inner))?;
        debug_assert!(compiled.buffer.relocs().is_empty());
        let machine_code = compiled.code_buffer().to_vec();
        Ok(Compiled {
            request: req,
            machine_code,
            time: start.elapsed(),
        })
    }
}

/// Background compiler thread.
struct Worker {
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Result<Compiled, String>>,
}

impl Worker {
    fn start() -> Result<Worker, String> {
        let mut compiler = Compiler::new()?;
        let (tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("jit".to_string())
            .spawn(move || {
                for req in req_rx {
                    if res_tx.send(compiler.compile(req)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Worker { tx, rx })
    }
}

/// Constants embedded in the generated code.
#[derive(Clone, Copy)]
struct CodegenEnv {
    ram_ptr: i64,
    tlb: i64,
    code_pages: i64,
    inline_stores: bool,
}

const OFS_X: i32 = offset_of!(Cpu, x) as i32;
const OFS_F: i32 = offset_of!(Cpu, f) as i32;
const OFS_MSTATUS: i32 = offset_of!(Cpu, mstatus) as i32;
const OFS_PC: i32 = offset_of!(Cpu, pc) as i32;
const OFS_DATA_KEY: i32 = offset_of!(Cpu, data_key) as i32;
const OFS_FAULT: i32 = offset_of!(Cpu, jit_fault) as i32;
const OFS_RESV: i32 = offset_of!(Cpu, reservation_valid) as i32;

const TLB_TAG: i32 = offset_of!(TlbEntry, tag) as i32;
const TLB_HOST_DELTA: i32 = offset_of!(TlbEntry, host_delta) as i32;
const TLB_HOST_FLAGS: i32 = offset_of!(TlbEntry, host_flags) as i32;
const TLB_SHIFT: i64 = std::mem::size_of::<TlbEntry>().trailing_zeros() as i64;

const _: () = assert!(std::mem::size_of::<TlbEntry>().is_power_of_two());

/// Maximum number of instructions a block may execute in a native
/// loop before returning to the dispatcher.
const LOOP_LIMIT: i64 = 4096;

struct Translator<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    ptr: Type,
    env: CodegenEnv,
    cpu: Value,
    /// Loop header following the register loads.
    header: cranelift_codegen::ir::Block,
    /// Instructions executed by the completed loop iterations.
    count: Variable,
    start_vpc: u64,
    vars: [Option<Variable>; 32],
}

impl<'a, 'b> Translator<'a, 'b> {
    fn new(b: &'a mut FunctionBuilder<'b>, ptr: Type, env: CodegenEnv, start_vpc: u64) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let cpu = b.block_params(entry)[0];
        let header = b.create_block();
        let count = b.declare_var(types::I64);
        Translator {
            b,
            ptr,
            env,
            cpu,
            header,
            count,
            start_vpc,
            vars: [None; 32],
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

    /// Sets the register. Register writes go through to the CPU state
    /// so that exits and interpreter calls see the current values.
    fn set(&mut self, r: u8, v: Value) {
        if r == 0 {
            return;
        }
        let var = self.vars[r as usize].expect("register not loaded");
        self.b.def_var(var, v);
        self.b
            .ins()
            .store(Self::flags(), v, self.cpu, OFS_X + 8 * r as i32);
    }

    fn iconst(&mut self, v: i64) -> Value {
        self.b.ins().iconst(types::I64, v)
    }

    /// Returns the total instruction count after n instructions of
    /// the current iteration.
    fn total(&mut self, n: u64) -> Value {
        let c = self.b.use_var(self.count);
        self.b.ins().iadd_imm_s(c, n as i64)
    }

    /// Exits the block: writes PC and returns the instruction count
    /// and status.
    fn exit_with(&mut self, pc: Value, count: Value, status: u64) {
        self.b
            .ins()
            .store(Self::flags(), pc, self.cpu, OFS_PC);
        let ret = if status == 0 {
            count
        } else {
            self.b.ins().bor_imm_s(count, (status << 32) as i64)
        };
        self.b.ins().return_(&[ret]);
    }

    /// Exits the block to pc after n instructions of the current
    /// iteration.
    fn jump_exit(&mut self, pc: Value, n: u64, status: u64) {
        let c = self.total(n);
        self.exit_with(pc, c, status);
    }

    /// Jumps to the guest PC target after n instructions of the
    /// current iteration. A jump to the block start loops in native
    /// code until the loop limit is reached.
    fn jump_to(&mut self, target: u64, n: u64) {
        let pcv = self.iconst(target as i64);
        if target != self.start_vpc {
            self.jump_exit(pcv, n, EXIT_OK);
            return;
        }
        let total = self.total(n);
        self.b.def_var(self.count, total);
        let more = self
            .b
            .ins()
            .icmp_imm_s(IntCC::SignedLessThan, total, LOOP_LIMIT);
        let out = self.b.create_block();
        self.b.ins().brif(more, self.header, &[], out, &[]);
        self.b.switch_to_block(out);
        self.exit_with(pcv, total, EXIT_OK);
    }

    fn translate(&mut self, instrs: &[BlockInstr]) {
        self.load_regs(instrs);

        let zero = self.iconst(0);
        self.b.def_var(self.count, zero);
        self.b.ins().jump(self.header, &[]);
        self.b.switch_to_block(self.header);

        let mut terminated = false;
        for (k, bi) in instrs.iter().enumerate() {
            if self.instr(k as u64, bi) {
                terminated = true;
                break;
            }
        }
        if !terminated {
            let last = instrs.last().unwrap();
            self.jump_to(last.pc.wrapping_add(last.size), instrs.len() as u64);
        }
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
        let f = Self::flags();
        let vpn = self.b.ins().ushr_imm_s(addr, 12);
        let idx = self.b.ins().band_imm_s(vpn, 0xfff);
        let eofs = self.b.ins().ishl_imm_s(idx, TLB_SHIFT);
        let tlb = self.iconst(self.env.tlb);
        let ent = self.b.ins().iadd(tlb, eofs);

        let key = self.b.ins().uload8(types::I64, f, self.cpu, OFS_DATA_KEY);
        let tag = self.b.ins().ishl_imm_s(vpn, 4);
        let tag = self.b.ins().bor(tag, key);
        let e_tag = self.b.ins().load(types::I64, f, ent, TLB_TAG);
        let mut ok = self.b.ins().icmp(IntCC::Equal, e_tag, tag);

        let e_flags = self.b.ins().uload8(types::I32, f, ent, TLB_HOST_FLAGS);
        let masked = self.b.ins().band_imm_s(e_flags, need as i64);
        let c_flags = self.b.ins().icmp_imm_s(IntCC::Equal, masked, need as i64);
        ok = self.b.ins().band(ok, c_flags);

        if size > 1 {
            let pofs = self.b.ins().band_imm_s(addr, 0xfff);
            let c_page =
                self.b
                    .ins()
                    .icmp_imm_s(IntCC::UnsignedLessThanOrEqual, pofs, 0x1000 - size);
            ok = self.b.ins().band(ok, c_page);
        }

        let delta = self.b.ins().load(types::I64, f, ent, TLB_HOST_DELTA);
        let host = self.b.ins().iadd(addr, delta);
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
        let stub = self.b.create_block();
        self.b.ins().brif(fault, stub, &[], cont, &[]);
        self.b.switch_to_block(stub);
        let pcv = self.iconst(pc as i64);
        self.jump_exit(pcv, k + 1, EXIT_FAULT);
    }

    /// Exits before the floating point instruction if the FP unit is
    /// off, so that the interpreter raises the exception.
    fn check_fs(&mut self, k: u64, pc: u64) {
        let ms = self
            .b
            .ins()
            .load(types::I64, Self::flags(), self.cpu, OFS_MSTATUS);
        let fs = self.b.ins().band_imm_s(ms, 3 << isa::ms::FS);
        let off = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(fs, on, &[], off, &[]);
        self.b.switch_to_block(off);
        let pcv = self.iconst(pc as i64);
        self.jump_exit(pcv, k, EXIT_OK);
        self.b.switch_to_block(on);
    }

    /// Marks the floating point state dirty.
    fn set_fs_dirty(&mut self) {
        let ms = self
            .b
            .ins()
            .load(types::I64, Self::flags(), self.cpu, OFS_MSTATUS);
        let ms = self
            .b
            .ins()
            .bor_imm_s(ms, (3 << isa::ms::FS) | (1i64 << isa::ms::SD));
        self.b
            .ins()
            .store(Self::flags(), ms, self.cpu, OFS_MSTATUS);
    }

    fn load_f(&mut self, r: u8) -> Value {
        self.b
            .ins()
            .load(types::I64, Self::flags(), self.cpu, OFS_F + 8 * r as i32)
    }

    fn store_f(&mut self, r: u8, v: Value) {
        self.b
            .ins()
            .store(Self::flags(), v, self.cpu, OFS_F + 8 * r as i32);
    }

    /// Executes the instruction in the interpreter and reloads rd.
    fn helper(&mut self, k: u64, bi: &BlockInstr) {
        let mut sig = Signature::new(self.b.func.signature.call_conv);
        sig.params.push(AbiParam::new(self.ptr));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let sigref = self.b.import_signature(sig);
        let callee = self.iconst(jit_exec as *const () as i64);
        let packed = self.iconst(pack_instr(&bi.instr) as i64);
        let raw = self.iconst(bi.raw as i64);
        self.b
            .ins()
            .call_indirect(sigref, callee, &[self.cpu, packed, raw]);
        let cont = self.b.create_block();
        self.check_fault(k, bi.pc, cont);
        self.b.switch_to_block(cont);
        let rd = bi.instr.rd;
        if rd != 0 {
            if let Some(var) = self.vars[rd as usize] {
                let v = self.b.ins().load(
                    types::I64,
                    Self::flags(),
                    self.cpu,
                    OFS_X + 8 * rd as i32,
                );
                self.b.def_var(var, v);
            }
        }
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

        if helper_op(i.op) {
            self.helper(k, bi);
            return false;
        }

        let v = match i.op {
            Fld | Flw => {
                self.check_fs(k, pc);
                let base = r1!();
                let addr = ins!().iadd_imm_s(base, imm);
                let v = if i.op == Fld {
                    self.load(k, pc, addr, 8, false)
                } else {
                    let v = self.load(k, pc, addr, 4, false);
                    ins!().bor_imm_s(v, 0xffff_ffff_0000_0000u64 as i64)
                };
                self.store_f(rd, v);
                self.set_fs_dirty();
                return false;
            }
            Fsd | Fsw => {
                self.check_fs(k, pc);
                let base = r1!();
                let addr = ins!().iadd_imm_s(base, imm);
                let v = self.load_f(i.rs2);
                let size = if i.op == Fsd { 8 } else { 4 };
                self.store(k, pc, addr, size, v);
                return false;
            }
            FmvXD | FmvXW => {
                self.check_fs(k, pc);
                let v = self.load_f(i.rs1);
                if i.op == FmvXD {
                    v
                } else {
                    self.sext32(v)
                }
            }
            FmvDX | FmvWX => {
                self.check_fs(k, pc);
                let a = r1!();
                let v = if i.op == FmvDX {
                    a
                } else {
                    let lo = self.lo32(a);
                    let lo = ins!().uextend(types::I64, lo);
                    ins!().bor_imm_s(lo, 0xffff_ffff_0000_0000u64 as i64)
                };
                self.store_f(rd, v);
                self.set_fs_dirty();
                return false;
            }
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
                let taken = self.b.create_block();
                let fall = self.b.create_block();
                ins!().brif(c, taken, &[], fall, &[]);
                self.b.switch_to_block(taken);
                self.jump_to(pc.wrapping_add(imm as u64), k + 1);
                self.b.switch_to_block(fall);
                if bi.last {
                    self.jump_to(next, k + 1);
                    return true;
                }
                return false;
            }
            Jal => {
                let link = self.iconst(next as i64);
                self.set(rd, link);
                if bi.last {
                    self.jump_to(pc.wrapping_add(imm as u64), k + 1);
                    return true;
                }
                // The trace continues at the jump target.
                return false;
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
    /// interpreter has reached them hot_threshold times, on a
    /// background thread if background is set.
    pub fn enable_jit(&mut self, hot_threshold: u16, background: bool) -> Result<(), String> {
        self.jit = Some(Box::new(Jit::new(hot_threshold, background)?));
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
        if !jit.pending.is_empty() {
            jit.poll = jit.poll.wrapping_add(1);
            if jit.poll % 64 == 0 {
                jit.install_pending(&mut self.jit_code_pages)
                    .map_err(Exception::fatal)?;
            }
        }
        let func = match jit.lookup(&self.mem, &mut self.jit_code_pages, pc, phys) {
            Some(f) => f,
            None => {
                if jit.pending.contains(&(pc, phys)) || !jit.bump(phys) {
                    return Ok(false);
                }
                let Some(req) = Jit::request(self, pc, phys, self.jit.as_ref().unwrap().epoch)
                else {
                    return Ok(false);
                };
                let jit = self.jit.as_mut().unwrap();
                if let Some(worker) = jit.worker.as_ref() {
                    worker
                        .tx
                        .send(req)
                        .map_err(|_| Exception::fatal("JIT worker exited"))?;
                    jit.pending.insert((pc, phys));
                    return Ok(false);
                }
                let compiled = jit
                    .compiler
                    .as_mut()
                    .unwrap()
                    .compile(req)
                    .map_err(Exception::fatal)?;
                mark_code_page(&mut self.jit_code_pages, compiled.request.code_ofs);
                jit.install(compiled).map_err(Exception::fatal)?
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
            jit.stats.jit_instrs += count;
        }
        if r >> 32 == EXIT_FAULT {
            self.jit_fault = 0;
            return Err(self
                .jit_exception
                .take()
                .unwrap_or_else(|| Exception::fatal("JIT fault without exception")));
        }
        // A block exits without executing anything when its first
        // instruction must run in the interpreter.
        Ok(count > 0)
    }
}
