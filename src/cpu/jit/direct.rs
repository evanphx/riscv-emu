//! Experimental RV64 -> x86-64 emitter. No host-specific optimizer or
//! general register allocator: the six guest registers with the highest
//! loop-weighted use counts get fixed host registers; the rest live in Cpu.
//! Cached registers are flushed on exits and before helpers for precise
//! faults. Code has no host addresses.
//!
//! Private ABI: r12 = Cpu, r13 = retired count, r14 = entry guest PC.
//! Blocks share one stack frame, tail-jump through the existing validated
//! jump cache, and return to the System V trampoline below. Integer-only
//! generated code needs no SIMD saves on memory slow paths.

use super::*;

const AX: u8 = 0;
const CX: u8 = 1;
const DX: u8 = 2;
const TLB: u8 = 5;
const SI: u8 = 6;
const DI: u8 = 7;
const CPU: u8 = 12;
const COUNT: u8 = 13;
const PC: u8 = 14;
const CACHE: [u8; 6] = [3, 15, 8, 9, 10, 11];

#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    ".text",
    ".globl rsemu_direct_enter",
    "rsemu_direct_enter:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov r12, rdi",
    "xor r13d, r13d",
    "call rsi",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

extern "C" {
    fn rsemu_direct_enter(cpu: *mut u8, block: BlockFn) -> u64;
}

pub(super) fn enter() -> EnterFn {
    // SAFETY: the assembly treats Cpu as an opaque pointer, passed in
    // the same System V register as *mut u8.
    unsafe {
        std::mem::transmute::<unsafe extern "C" fn(*mut u8, BlockFn) -> u64, EnterFn>(
            rsemu_direct_enter,
        )
    }
}

/// Only the encodings used by this translator. Branches use patched
/// rel32 offsets within the same block.
#[derive(Default)]
struct Asm {
    code: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixups: Vec<(usize, usize)>,
}

impl Asm {
    fn bytes(&mut self, b: &[u8]) {
        self.code.extend_from_slice(b);
    }
    fn i32(&mut self, v: i32) {
        self.bytes(&v.to_le_bytes());
    }
    fn rex(&mut self, wide: bool, reg: u8, base: u8) {
        let rex = 0x40 | ((wide as u8) << 3) | ((reg >> 3) << 2) | (base >> 3);
        if rex != 0x40 {
            self.bytes(&[rex]);
        }
    }
    fn rr(&mut self, op: &[u8], dst: u8, src: u8, wide: bool) {
        self.rex(wide, src, dst);
        self.bytes(op);
        self.bytes(&[0xc0 | ((src & 7) << 3) | (dst & 7)]);
    }
    fn mov(&mut self, dst: u8, src: u8) {
        if dst != src {
            self.rr(&[0x89], dst, src, true);
        }
    }
    fn imm(&mut self, dst: u8, v: u64) {
        if let Ok(v) = u32::try_from(v) {
            self.rex(false, 0, dst);
            self.bytes(&[0xb8 + (dst & 7)]);
            self.i32(v as i32);
            return;
        }
        if let Ok(v) = i32::try_from(v as i64) {
            self.rex(true, 0, dst);
            self.bytes(&[0xc7, 0xc0 | (dst & 7)]);
            self.i32(v);
            return;
        }
        self.rex(true, 0, dst);
        self.bytes(&[0xb8 + (dst & 7)]);
        self.bytes(&v.to_le_bytes());
    }
    fn mem(&mut self, op: &[u8], reg: u8, base: u8, disp: i32, wide: bool) {
        self.mem_indexed(op, reg, base, None, disp, wide);
    }
    fn mem_indexed(
        &mut self,
        op: &[u8],
        reg: u8,
        base: u8,
        index: Option<u8>,
        disp: i32,
        wide: bool,
    ) {
        let rex = 0x40
            | ((wide as u8) << 3)
            | ((reg >> 3) << 2)
            | (index.map_or(0, |r| r >> 3) << 1)
            | (base >> 3);
        if rex != 0x40 {
            self.bytes(&[rex]);
        }
        self.bytes(op);
        let mode = if disp == 0 && base & 7 != 5 {
            0
        } else if i8::try_from(disp).is_ok() {
            0x40
        } else {
            0x80
        };
        let sib = index.is_some() || base & 7 == 4;
        self.bytes(&[mode | ((reg & 7) << 3) | if sib { 4 } else { base & 7 }]);
        if sib {
            self.bytes(&[(index.map_or(4, |r| r & 7) << 3) | (base & 7)]);
        }
        match mode {
            0x40 => self.bytes(&[disp as u8]),
            0x80 => self.i32(disp),
            _ => {}
        }
    }
    fn load(&mut self, dst: u8, base: u8, disp: i32) {
        self.mem(&[0x8b], dst, base, disp, true);
    }
    fn store(&mut self, src: u8, base: u8, disp: i32) {
        self.mem(&[0x89], src, base, disp, true);
    }
    fn lea(&mut self, dst: u8, base: u8, disp: i32) {
        self.mem(&[0x8d], dst, base, disp, true);
    }
    fn lea_scaled(&mut self, dst: u8, base: Option<u8>, index: u8, shift: u8, disp: i32) {
        let base_bits = base.map_or(5, |b| b & 7);
        self.bytes(&[
            0x48 | ((dst >> 3) << 2) | ((index >> 3) << 1) | base.map_or(0, |b| b >> 3),
            0x8d,
        ]);
        let mode = if base.is_none() || (disp == 0 && base_bits != 5) {
            0
        } else if i8::try_from(disp).is_ok() {
            0x40
        } else {
            0x80
        };
        self.bytes(&[
            mode | ((dst & 7) << 3) | 4,
            (shift << 6) | ((index & 7) << 3) | base_bits,
        ]);
        if base.is_none() || mode == 0x80 {
            self.i32(disp);
        } else if mode == 0x40 {
            self.bytes(&[disp as u8]);
        }
    }
    fn alu(&mut self, kind: u8, dst: u8, imm: i32, wide: bool) {
        self.rex(wide, 0, dst);
        let small = i8::try_from(imm).is_ok();
        self.bytes(&[
            if small { 0x83 } else { 0x81 },
            0xc0 | (kind << 3) | (dst & 7),
        ]);
        if small {
            self.bytes(&[imm as u8]);
        } else {
            self.i32(imm);
        }
    }
    fn shift(&mut self, kind: u8, dst: u8, amount: Option<u8>, wide: bool) {
        self.rex(wide, 0, dst);
        self.bytes(&[
            if amount.is_some() { 0xc1 } else { 0xd3 },
            0xc0 | (kind << 3) | (dst & 7),
        ]);
        if let Some(n) = amount {
            self.bytes(&[n]);
        }
    }
    fn sext32(&mut self) {
        self.bytes(&[0x48, 0x63, 0xc0]);
    }
    fn label(&mut self) -> usize {
        let n = self.labels.len();
        self.labels.push(None);
        n
    }
    fn lea_label(&mut self, dst: u8, label: usize) {
        self.rex(true, dst, 0);
        self.bytes(&[0x8d, 5 | ((dst & 7) << 3)]);
        self.fixups.push((self.code.len(), label));
        self.i32(0);
    }
    fn bind(&mut self, label: usize) {
        self.labels[label] = Some(self.code.len());
    }
    fn branch(&mut self, cc: Option<u8>, label: usize) {
        if let Some(cc) = cc {
            self.bytes(&[0x0f, 0x80 | cc]);
        } else {
            self.bytes(&[0xe9]);
        }
        self.fixups.push((self.code.len(), label));
        self.i32(0);
    }
    fn push(&mut self, reg: u8) {
        self.rex(false, 0, reg);
        self.bytes(&[0x50 + (reg & 7)]);
    }
    fn pop(&mut self, reg: u8) {
        self.rex(false, 0, reg);
        self.bytes(&[0x58 + (reg & 7)]);
    }
    fn finish(mut self) -> Vec<u8> {
        for (pos, label) in self.fixups {
            let delta = self.labels[label].expect("unbound label") as i64 - (pos + 4) as i64;
            self.code[pos..pos + 4].copy_from_slice(&i32::try_from(delta).unwrap().to_le_bytes());
        }
        self.code
    }
}

#[derive(Clone, Copy)]
struct LoopRead {
    start: u64,
    end: u64,
    base: u8,
    lo: i32,
    hi: i32,
    body: usize,
}

/// Native integer operations that cannot fault or clobber RDI. Used by
/// both straight-line read grouping and loop translation hoisting.
fn read_alu(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        Add | Addw
            | Sub
            | Subw
            | And
            | Or
            | Xor
            | Mul
            | Mulw
            | Addi
            | Addiw
            | Andi
            | Ori
            | Xori
            | Lui
            | Auipc
            | Slli
            | Srli
            | Srai
            | Slliw
            | Srliw
            | Sraiw
            | Sll
            | Srl
            | Sra
            | Sllw
            | Srlw
            | Sraw
            | Slt
            | Sltu
            | Slti
            | Sltiu
    )
}

/// Possible low-three-bit values, not observations of a particular run.
/// Restart at every branch entry and helper: a proof must hold on every
/// path into an access, including loop backedges and cold trace entries.
fn aligned_accesses(instrs: &[BlockInstr], leaders: &HashSet<u64>) -> HashSet<u64> {
    use Op::*;
    let mut values = [u8::MAX; 32];
    let mut aligned = HashSet::default();
    for (n, bi) in instrs.iter().enumerate() {
        if leaders.contains(&bi.pc)
            || n == 0
            || instrs[n - 1].pc.wrapping_add(instrs[n - 1].size) != bi.pc
        {
            values.fill(u8::MAX);
        }
        values[0] = 1; // x0 is exactly zero modulo eight.
        let i = bi.instr;
        if let Some((size, write)) = mem_access(i.op) {
            let possible = values[i.rs1 as usize];
            if size > 1
                && (0..8).all(|v| {
                    possible & (1 << v) == 0 || ((v as i64 + i.imm as i64) & (size - 1)) == 0
                })
            {
                aligned.insert(bi.pc);
            }
            if !write && !matches!(i.op, Flw | Fld) {
                values[i.rd as usize] = u8::MAX;
            }
            continue;
        }
        let a = values[i.rs1 as usize];
        let b = values[i.rs2 as usize];
        let mut result = 0;
        match i.op {
            Lui => result = 1 << (i.imm as u64 & 7),
            Auipc => result = 1 << (bi.pc.wrapping_add(i.imm as i64 as u64) & 7),
            Addi | Addiw | Andi | Ori | Xori | Slli | Slliw | SlliUw => {
                for v in 0..8u64 {
                    if a & (1 << v) == 0 {
                        continue;
                    }
                    let imm = i.imm as i64 as u64;
                    let r = match i.op {
                        Addi | Addiw => v.wrapping_add(imm),
                        Andi => v & imm,
                        Ori => v | imm,
                        Xori => v ^ imm,
                        _ => v.wrapping_shl(i.imm as u32),
                    };
                    result |= 1 << (r & 7);
                }
            }
            Add | Addw | AddUw | Sub | Subw | And | Or | Xor | Mul | Mulw | Sh1add | Sh2add
            | Sh3add | Sh1addUw | Sh2addUw | Sh3addUw => {
                for x in 0..8u64 {
                    for y in 0..8u64 {
                        if a & (1 << x) == 0 || b & (1 << y) == 0 {
                            continue;
                        }
                        let r = match i.op {
                            Add | Addw | AddUw => x + y,
                            Sub | Subw => x.wrapping_sub(y),
                            And => x & y,
                            Or => x | y,
                            Xor => x ^ y,
                            Mul | Mulw => x * y,
                            Sh1add | Sh1addUw => (x << 1) + y,
                            Sh2add | Sh2addUw => (x << 2) + y,
                            _ => (x << 3) + y,
                        };
                        result |= 1 << (r & 7);
                    }
                }
            }
            _ if read_alu(i.op) => result = u8::MAX,
            _ => {
                values.fill(u8::MAX);
                continue;
            }
        }
        values[i.rd as usize] = result;
    }
    aligned
}

struct Emitter {
    a: Asm,
    regs: [Option<u8>; 32],
    written: [bool; 32],
    start: u64,
    targets: HashMap<u64, usize>,
    edges: Vec<(usize, u64, u64)>,
    memory_exits: Vec<(usize, usize, u64, u64)>,
    read_exits: Vec<(usize, u64)>,
    tlb_exits: Vec<(usize, usize, usize)>,
    specialized_loads: u64,
    aligned: HashSet<u64>,
    elided_alignment_guards: u64,
    loop_reads: Vec<LoopRead>,
    chain: usize,
    ret: usize,
    env: CodegenEnv,
    pending_count: i32,
}

pub(super) fn compile(req: &Request) -> (Vec<u8>, u64, u64, u64, u64) {
    use Op::*;
    let mut a = Asm::default();
    let chain = a.label();
    let ret = a.label();
    let targets = req.instrs.iter().map(|bi| (bi.pc, a.label())).collect();
    let mut t = Emitter {
        a,
        regs: [None; 32],
        written: [false; 32],
        start: req.vpc,
        targets,
        edges: Vec::new(),
        memory_exits: Vec::new(),
        read_exits: Vec::new(),
        tlb_exits: Vec::new(),
        specialized_loads: 0,
        aligned: HashSet::default(),
        elided_alignment_guards: 0,
        loop_reads: Vec::new(),
        chain,
        ret,
        env: req.env,
        pending_count: 0,
    };
    // An internal backward edge identifies a likely hot loop. Weight
    // operands in its address range above straight-line/exit operands:
    // long constant construction on a cold exit must not evict loop
    // counters. This is a static heuristic, not measured frequency.
    let loops: Vec<_> = req
        .instrs
        .iter()
        .filter(|bi| is_terminator(bi.instr.op) && bi.instr.op != Jalr)
        .filter_map(|bi| {
            let target = bi.pc.wrapping_add(bi.instr.imm as i64 as u64);
            (target <= bi.pc && t.targets.contains_key(&target)).then_some((target, bi.pc))
        })
        .collect();
    let mut uses = [0usize; 32];
    for bi in &req.instrs {
        let i = bi.instr;
        t.written[i.rd as usize] = true;
        // Decoding retains raw register fields even when an opcode
        // uses those bits for an immediate, or addresses FP registers.
        // Only operands actually used by native integer code compete
        // for the six slots. Helpers consume Cpu state, not host regs.
        let regs = match i.op {
            Add | Addw | Sub | Subw | And | Or | Xor | Mul | Mulw | Sll | Srl | Sra | Sllw
            | Srlw | Sraw | Slt | Sltu | Rol | Ror | Rolw | Rorw | Sh1add | Sh2add | Sh3add
            | AddUw | Sh1addUw | Sh2addUw | Sh3addUw | Andn | Orn | Xnor => [i.rd, i.rs1, i.rs2],
            Addi | Addiw | Andi | Ori | Xori | Slti | Sltiu | Slli | Srli | Srai | Slliw
            | Srliw | Sraiw | SlliUw | Rori | Roriw | SextB | SextH | ZextH | Rev8 | Jalr | Lb
            | Lbu | Lh | Lhu | Lw | Lwu | Ld => [i.rd, i.rs1, 0],
            Beq | Bne | Blt | Bge | Bltu | Bgeu | Sb | Sh | Sw | Sd => [0, i.rs1, i.rs2],
            Lui | Auipc | Jal | FmvXD | FmvXW => [i.rd, 0, 0],
            Flw | Fld | Fsw | Fsd | FmvDX | FmvWX => [0, i.rs1, 0],
            _ if matches!(csr_kind(&i), Some(CsrKind::Counter)) => [i.rd, 0, 0],
            _ => [0; 3],
        };
        let weight = if loops.iter().any(|&(lo, hi)| lo <= bi.pc && bi.pc <= hi) {
            32
        } else {
            1
        };
        for r in regs {
            uses[r as usize] += weight;
        }
    }
    let mut order: Vec<usize> = (1..32).filter(|&r| uses[r] > 0).collect();
    order.sort_by_key(|&r| std::cmp::Reverse(uses[r]));
    for (r, host) in order.into_iter().zip(CACHE) {
        t.regs[r] = Some(host);
        t.a.load(host, CPU, OFS_X + 8 * r as i32);
    }
    t.a.load(PC, CPU, OFS_PC);
    // RBP is callee-saved and already saved by the entry trampoline.
    // Helpers preserve it; a CSR changing the bank ends the block.
    t.a.load(
        TLB,
        CPU,
        if req.env.host_mmap {
            OFS_MMAP_BIAS
        } else {
            OFS_JIT_TLB
        },
    );
    t.a.branch(None, t.targets[&req.vpc]);
    let mut leaders = HashSet::from([req.vpc]);
    for bi in &req.instrs {
        if is_terminator(bi.instr.op) {
            leaders.insert(bi.pc.wrapping_add(bi.size));
            if bi.instr.op != Op::Jalr {
                leaders.insert(bi.pc.wrapping_add(bi.instr.imm as i64 as u64));
            }
        }
    }
    if req.env.host_mmap {
        t.aligned = aligned_accesses(&req.instrs, &leaders);
    }
    t.plan_loop_reads(&req.instrs);
    let mut index = 0;
    let mut fused_pairs = 0;
    while let Some(bi) = req.instrs.get(index) {
        if leaders.contains(&bi.pc) {
            t.count();
        }
        t.a.bind(t.targets[&bi.pc]);
        let loop_read = t
            .loop_reads
            .iter()
            .find(|l| l.start <= bi.pc && bi.pc <= l.end)
            .copied();
        if let Some(l) = loop_read.filter(|l| l.start == bi.pc) {
            t.begin_loop_read(l);
        }
        let grouped = if loop_read.is_none() {
            t.read_group(&req.instrs[index..], &leaders)
        } else {
            0
        };
        let terminated = if grouped != 0 {
            index += grouped - 1;
            false
        } else {
            t.pending_count += 1;
            // Never erase a branch entry, even if instructions are adjacent.
            let fused = req.instrs.get(index + 1).is_some_and(|second| {
                second.pc == bi.pc.wrapping_add(bi.size)
                    && !leaders.contains(&second.pc)
                    && t.fuse(bi, second)
            });
            if fused {
                fused_pairs += 1;
                t.pending_count += 1;
                index += 1;
                false
            } else {
                t.instr(bi, req.instrs.get(index + 1).map(|b| b.pc))
            }
        };
        let last = &req.instrs[index];
        let next = last.pc.wrapping_add(last.size);
        if !terminated && !req.instrs.get(index + 1).is_some_and(|b| b.pc == next) {
            t.jump_to(next, last.pc);
        }
        index += 1;
    }
    t.finish();
    (
        t.a.finish(),
        fused_pairs,
        t.specialized_loads,
        t.loop_reads.len() as u64,
        t.elided_alignment_guards,
    )
}

impl Emitter {
    /// These peepholes overwrite the intermediate destination, so no
    /// liveness assumptions are needed. Neither instruction may fault.
    /// In particular, address-generation + load is NOT fused: on a
    /// load fault the intermediate address is architectural state.
    fn fuse(&mut self, first: &BlockInstr, second: &BlockInstr) -> bool {
        use Op::*;
        let (a, b) = (first.instr, second.instr);
        if a.rd == 0 || a.rd != b.rd {
            return false;
        }
        if b.rs1 == a.rd && matches!(b.op, Addi | Addiw) {
            match a.op {
                Lui => {
                    let value = (a.imm as i64).wrapping_add(b.imm as i64);
                    self.a.imm(
                        AX,
                        if b.op == Addiw {
                            value as i32 as i64 as u64
                        } else {
                            value as u64
                        },
                    );
                    self.set(b.rd);
                    return true;
                }
                Auipc => {
                    self.pc(
                        AX,
                        first
                            .pc
                            .wrapping_add(a.imm as i64 as u64)
                            .wrapping_add(b.imm as i64 as u64),
                    );
                    if b.op == Addiw {
                        self.a.sext32();
                    }
                    self.set(b.rd);
                    return true;
                }
                Addi => {
                    self.get(AX, a.rs1);
                    self.a.lea(AX, AX, a.imm + b.imm);
                    if b.op == Addiw {
                        self.a.sext32();
                    }
                    self.set(b.rd);
                    return true;
                }
                _ => {}
            }
        }
        if a.op == Slli && b.op == Add && (1..=3).contains(&a.imm) {
            let base = if b.rs1 == a.rd && b.rs2 != a.rd {
                Some(b.rs2)
            } else if b.rs2 == a.rd && b.rs1 != a.rd {
                Some(b.rs1)
            } else {
                None
            };
            if let Some(base) = base {
                self.scaled(b.rd, Some(base), a.rs1, a.imm as u8, 0);
                return true;
            }
        }
        if a.op == Addi && b.op == Slli && b.rs1 == a.rd && (1..=3).contains(&b.imm) {
            self.scaled(b.rd, None, a.rs1, b.imm as u8, a.imm << b.imm);
            return true;
        }
        if a.op == Slli
            && b.rs1 == a.rd
            && a.imm == b.imm
            && matches!(a.imm, 32 | 48 | 56)
            && matches!(b.op, Srli | Srai)
        {
            self.get(AX, a.rs1);
            match (a.imm, b.op) {
                (32, Srli) => self.a.rr(&[0x89], AX, AX, false),
                (32, _) => self.a.sext32(),
                (48, Srli) => self.a.rr(&[0x0f, 0xb7], AX, AX, true),
                (48, _) => self.a.rr(&[0x0f, 0xbf], AX, AX, true),
                (56, Srli) => self.a.rr(&[0x0f, 0xb6], AX, AX, true),
                _ => self.a.rr(&[0x0f, 0xbe], AX, AX, true),
            }
            self.set(b.rd);
            return true;
        }
        false
    }

    /// Use x86's base + index*scale + displacement addressing directly,
    /// including cached guest registers as operands/destination.
    fn scaled(&mut self, rd: u8, base: Option<u8>, index: u8, shift: u8, disp: i32) {
        if rd == 0 {
            return;
        }
        let base = base.map(|r| self.operand(r, AX));
        let index = self.operand(index, CX);
        let dst = self.regs[rd as usize].unwrap_or(DX);
        self.a.lea_scaled(dst, base, index, shift, disp);
        if self.regs[rd as usize].is_none() {
            self.a.store(dst, CPU, OFS_X + 8 * rd as i32);
        }
    }

    /// Account straight-line arithmetic together, but never move the
    /// accounting across a possible fault, counter read or control edge.
    fn count(&mut self) {
        if self.pending_count != 0 {
            self.a.lea(COUNT, COUNT, self.pending_count);
            self.pending_count = 0;
        }
    }

    fn get(&mut self, dst: u8, r: u8) {
        if r == 0 {
            self.a.rr(&[0x31], dst, dst, false);
        } else if let Some(host) = self.regs[r as usize] {
            self.a.mov(dst, host);
        } else {
            self.a.load(dst, CPU, OFS_X + 8 * r as i32);
        }
    }
    fn operand(&mut self, r: u8, scratch: u8) -> u8 {
        self.regs[r as usize].unwrap_or_else(|| {
            self.get(scratch, r);
            scratch
        })
    }

    /// x86 arithmetic is destructive. Prefer the cached destination
    /// itself, commuting operands when it aliases rs2; SUB must instead
    /// save the old rs2 before overwriting it with rs1.
    fn alu_instr(&mut self, mut i: Instr) -> bool {
        use Op::*;
        if !matches!(
            i.op,
            Add | Addw
                | Sub
                | Subw
                | And
                | Or
                | Xor
                | Mul
                | Mulw
                | Addi
                | Addiw
                | Andi
                | Ori
                | Xori
        ) {
            return false;
        }
        if i.rd == 0 {
            return true;
        }
        let dst = self.regs[i.rd as usize].unwrap_or(AX);
        let word = matches!(i.op, Addw | Subw | Mulw | Addiw);
        if matches!(i.op, Addi | Addiw | Andi | Ori | Xori) {
            self.get(dst, i.rs1);
            let kind = match i.op {
                Addi | Addiw => 0,
                Andi => 4,
                Ori => 1,
                _ => 6,
            };
            self.a.alu(kind, dst, i.imm, !word);
        } else {
            if i.rd == i.rs2 && !matches!(i.op, Sub | Subw) {
                std::mem::swap(&mut i.rs1, &mut i.rs2);
            }
            let mut src = self.operand(i.rs2, CX);
            if src == dst && i.rs1 != i.rs2 {
                self.a.mov(CX, src);
                src = CX;
            }
            self.get(dst, i.rs1);
            match i.op {
                Mul | Mulw => self.a.rr(&[0x0f, 0xaf], src, dst, !word),
                _ => {
                    let op = match i.op {
                        Add | Addw => 0x01,
                        Sub | Subw => 0x29,
                        And => 0x21,
                        Or => 0x09,
                        _ => 0x31,
                    };
                    self.a.rr(&[op], dst, src, !word);
                }
            }
        }
        if word {
            self.a.rr(&[0x63], dst, dst, true);
        }
        if self.regs[i.rd as usize].is_none() {
            self.a.store(dst, CPU, OFS_X + 8 * i.rd as i32);
        }
        true
    }

    fn set(&mut self, r: u8) {
        if r == 0 {
            return;
        }
        if let Some(host) = self.regs[r as usize] {
            self.a.mov(host, AX);
        } else {
            self.a.store(AX, CPU, OFS_X + 8 * r as i32);
        }
    }
    fn flush_regs(&mut self) {
        for r in 1..32 {
            if let Some(host) = self.regs[r].filter(|_| self.written[r]) {
                self.a.store(host, CPU, OFS_X + 8 * r as i32);
            }
        }
    }
    fn pc(&mut self, dst: u8, pc: u64) {
        let delta = pc.wrapping_sub(self.start) as i64;
        if let Ok(delta) = i32::try_from(delta) {
            self.a.lea(dst, PC, delta);
        } else {
            self.a.imm(dst, delta as u64);
            self.a.rr(&[0x01], dst, PC, true);
        }
    }
    fn sym(&mut self, dst: u8, sym: Sym) {
        self.a.load(dst, CPU, OFS_ADDRS + 8 * sym as i32);
    }
    fn jump_to(&mut self, target: u64, from: u64) {
        self.count();
        if let Some(&label) = self.targets.get(&target) {
            if target <= from {
                // Only this loop's own backedge may reuse its translation.
                // All other entries (including after a timeslice) run the guard.
                let label = self
                    .loop_reads
                    .iter()
                    .find(|l| l.start == target && l.end == from)
                    .map_or(label, |l| l.body);
                self.a.alu(7, COUNT, LOOP_LIMIT as i32, true);
                self.a.branch(Some(2), label); // unsigned below
                self.pc(AX, target);
                self.a.branch(None, self.chain);
            } else {
                self.a.branch(None, label);
            }
        } else {
            self.pc(AX, target);
            self.a.branch(None, self.chain);
        }
    }
    fn fault(&mut self, pc: u64) {
        self.pc(CX, pc);
        self.a.store(CX, CPU, OFS_PC);
        self.a.mov(AX, COUNT);
        self.a.imm(CX, EXIT_FAULT << 32);
        self.a.rr(&[0x09], AX, CX, true);
        self.a.branch(None, self.ret);
    }
    fn finish(&mut self) {
        // Branch side exits and guarded backedges live outside the
        // straight-line body, so the hot successor can fall through.
        for (label, target, from) in std::mem::take(&mut self.edges) {
            self.a.bind(label);
            self.jump_to(target, from);
        }
        for (label, fast, slow) in std::mem::take(&mut self.tlb_exits) {
            self.a.bind(label);
            self.tlb_lookup(false, slow);
            self.a.branch(None, fast);
        }
        for (slow, done, pc, kind) in std::mem::take(&mut self.memory_exits) {
            self.a.bind(slow);
            self.memory_slow(pc, kind);
            self.a.branch(None, done);
        }
        for (label, pc) in std::mem::take(&mut self.read_exits) {
            self.a.bind(label);
            self.flush_regs();
            self.pc(AX, pc);
            self.a.store(AX, CPU, OFS_PC);
            self.a.mov(AX, COUNT);
            self.a.branch(None, self.ret);
        }
        self.a.bind(self.chain);
        self.flush_regs();
        self.a.store(AX, CPU, OFS_PC);
        self.a.alu(7, COUNT, LOOP_LIMIT as i32, true);
        let done = self.a.label();
        self.a.branch(Some(3), done);
        self.a.mov(CX, AX);
        self.a.shift(5, CX, Some(1), true);
        self.a.alu(4, CX, JCACHE_SIZE as i32 - 1, true);
        self.a.shift(4, CX, Some(JENTRY_SHIFT as u8), true);
        self.sym(DX, Sym::JCache);
        self.a.rr(&[0x01], CX, DX, true);
        self.a.mem(&[0x39], AX, CX, JE_VPC, true);
        self.a.branch(Some(5), done);
        self.a.load(DX, CPU, OFS_JKEY);
        self.a.mem(&[0x39], DX, CX, JE_KEY, true);
        self.a.branch(Some(5), done);
        self.a.load(AX, CX, JE_FUNC);
        self.a.bytes(&[0xff, 0xe0]); // jmp rax, same stack frame
        self.a.bind(done);
        self.a.mov(AX, COUNT);
        self.a.bind(self.ret);
        self.a.bytes(&[0xc3]);
    }

    /// Find single-entry loops whose only memory accesses read one
    /// unchanged base. Conditional branches and side exits are safe;
    /// stores, helpers, CSRs and fences are not. No inner entry may bypass
    /// the guard, and disjoint plans keep their host bases from conflicting.
    fn plan_loop_reads(&mut self, instrs: &[BlockInstr]) {
        use Op::*;
        for (end, last) in instrs.iter().enumerate() {
            if !matches!(last.instr.op, Beq | Bne | Blt | Bge | Bltu | Bgeu | Jal)
                || (last.instr.op == Jal && last.instr.rd != 0)
            {
                continue;
            }
            let start = last.pc.wrapping_add(last.instr.imm as i64 as u64);
            let Some(first) = instrs[..end].iter().position(|bi| bi.pc == start) else {
                continue;
            };
            if (start < self.start && self.start <= last.pc)
                || self
                    .loop_reads
                    .iter()
                    .any(|l| start <= l.end && l.start <= last.pc)
                || instrs.iter().any(|bi| {
                    if (start <= bi.pc && bi.pc <= last.pc)
                        || !is_terminator(bi.instr.op)
                        || bi.instr.op == Jalr
                    {
                        return false;
                    }
                    let target = bi.pc.wrapping_add(bi.instr.imm as i64 as u64);
                    start < target && target <= last.pc
                })
            {
                continue;
            }
            let body = &instrs[first..end];
            let Some(load) = body
                .iter()
                .find(|bi| matches!(bi.instr.op, Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld))
            else {
                continue;
            };
            let base = load.instr.rs1;
            let (mut lo, mut hi) = (i32::MAX, i32::MIN);
            let valid = body.iter().enumerate().all(|(n, bi)| {
                let i = bi.instr;
                let branch = matches!(i.op, Beq | Bne | Blt | Bge | Bltu | Bgeu);
                if bi.pc.wrapping_add(bi.size) != instrs[first + n + 1].pc
                    || (!branch && base != 0 && i.rd == base)
                {
                    return false;
                }
                if matches!(i.op, Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld) {
                    let size = mem_access(i.op).unwrap().0 as i32;
                    lo = lo.min(i.imm);
                    hi = hi.max(i.imm + size - 1);
                    i.rs1 == base
                } else {
                    read_alu(i.op) || branch
                }
            });
            if valid && hi - lo < 4096 {
                self.loop_reads.push(LoopRead {
                    start,
                    end: last.pc,
                    base,
                    lo,
                    hi,
                    body: self.a.label(),
                });
            }
        }
    }

    fn begin_loop_read(&mut self, l: LoopRead) {
        self.get(DI, l.base);
        self.a.lea(SI, DI, l.lo);
        self.a.lea(DX, DI, l.hi);
        self.a.alu(4, DX, !0xfff, true);
        let slow = self.a.label();
        self.translation(None, false, slow);
        // A failed speculative guard does not raise a fault. Interpret
        // from the header so preceding arithmetic and reads occur in order.
        self.read_exits.push((slow, l.start));
        self.a.rr(&[0x01], DI, AX, true);
        self.a.bind(l.body);
    }

    /// One checked RAM translation for multiple reads of an unchanged
    /// base. Only nonfaulting integer operations may separate the reads;
    /// neither helpers nor stores can clobber the host base in RDI.
    fn read_group(&mut self, instrs: &[BlockInstr], leaders: &HashSet<u64>) -> usize {
        use Op::*;
        let base = instrs[0].instr.rs1;
        if !matches!(instrs[0].instr.op, Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld) {
            return 0;
        }
        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
        let (mut reads, mut end) = (0, 0);
        for (n, bi) in instrs.iter().enumerate() {
            let i = bi.instr;
            if (n != 0
                && (leaders.contains(&bi.pc)
                    || instrs[n - 1].pc.wrapping_add(instrs[n - 1].size) != bi.pc))
                || i.rd == base
            {
                break;
            }
            if matches!(i.op, Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld) {
                if i.rs1 != base {
                    break;
                }
                let size = mem_access(i.op).unwrap().0 as i32;
                let next_lo = lo.min(i.imm);
                let next_hi = hi.max(i.imm + size - 1);
                if next_hi - next_lo >= 4096 {
                    break;
                }
                (lo, hi) = (next_lo, next_hi);
                reads += 1;
                end = n + 1;
            } else if !read_alu(i.op) {
                break;
            }
        }
        if reads < 2 {
            return 0;
        }
        self.count();
        self.get(DI, base);
        self.a.lea(SI, DI, lo);
        self.a.lea(DX, DI, hi);
        self.a.alu(4, DX, !0xfff, true);
        let slow = self.a.label();
        // The index of the first address is part of this range guard:
        // comparing the last page with a predicted slot alone could
        // accept a range whose first address is on another page.
        self.translation(None, false, slow);
        self.read_exits.push((slow, instrs[0].pc));
        self.a.rr(&[0x01], DI, AX, true);
        // On a miss, return *before* the first read, not with a fault
        // for a later read. The interpreter refills and preserves order.
        for bi in &instrs[..end] {
            self.pending_count += 1;
            if matches!(bi.instr.op, Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld) {
                self.load_value(bi.instr.op, DI, None, bi.instr.imm);
                self.set(bi.instr.rd);
            } else {
                self.instr(bi, None);
            }
        }
        end
    }

    fn load_value(&mut self, op: Op, base: u8, index: Option<u8>, offset: i32) {
        let (op, wide): (&[u8], bool) = match op {
            Op::Lb => (&[0x0f, 0xbe], true),
            Op::Lbu => (&[0x0f, 0xb6], false),
            Op::Lh => (&[0x0f, 0xbf], true),
            Op::Lhu => (&[0x0f, 0xb7], false),
            Op::Lw => (&[0x63], true),
            Op::Lwu | Op::Flw => (&[0x8b], false),
            _ => (&[0x8b], true),
        };
        self.a.mem_indexed(op, AX, base, index, offset, wide);
    }

    /// SI is the guest address, DX the full page/alignment tag; AX
    /// receives the current mapping delta. A profile never replaces
    /// validation. Unexpected pages use the ordinary inline lookup.
    fn translation(&mut self, slot: Option<u16>, write: bool, slow: usize) {
        if let Some(slot) = slot.filter(|_| !write) {
            let offset = (slot as i32) << JTLB_SHIFT;
            let generic = self.a.label();
            let fast = self.a.label();
            let tlb = if self.env.host_mmap {
                self.a.load(CX, CPU, OFS_JIT_TLB);
                CX
            } else {
                TLB
            };
            self.a.mem(&[0x39], DX, tlb, offset + JTLB_TAG_R, true);
            self.a.branch(Some(5), generic);
            self.a.load(AX, tlb, offset + JTLB_DELTA);
            self.a.bind(fast);
            self.tlb_exits.push((generic, fast, slow));
            self.specialized_loads += 1;
        } else {
            self.tlb_lookup(write, slow);
        }
    }

    fn tlb_lookup(&mut self, write: bool, slow: usize) {
        let tlb = if self.env.host_mmap {
            self.a.load(CX, CPU, OFS_JIT_TLB);
            CX
        } else {
            TLB
        };
        self.a.mov(AX, SI);
        self.a.shift(5, AX, Some((12 - JTLB_SHIFT) as u8), true);
        self.a.alu(4, AX, 0xfff << JTLB_SHIFT, true);
        self.a.mem_indexed(
            &[0x39],
            DX,
            tlb,
            Some(AX),
            if write { JTLB_TAG_W } else { JTLB_TAG_R },
            true,
        );
        self.a.branch(Some(5), slow);
        self.a
            .mem_indexed(&[0x8b], AX, tlb, Some(AX), JTLB_DELTA, true);
    }

    fn memory(&mut self, bi: &BlockInstr, size: i64, write: bool) {
        if self
            .loop_reads
            .iter()
            .any(|l| l.start <= bi.pc && bi.pc < l.end)
        {
            self.load_value(bi.instr.op, DI, None, bi.instr.imm);
            self.set(bi.instr.rd);
            return;
        }
        self.count();
        let i = bi.instr;
        self.get(SI, i.rs1);
        self.a.lea(SI, SI, i.imm);
        if write {
            if matches!(i.op, Op::Fsd | Op::Fsw) {
                self.a.load(DI, CPU, OFS_F + 8 * i.rs2 as i32);
            } else {
                self.get(DI, i.rs2);
            }
        }
        let slow = self.a.label();
        let done = self.a.label();
        if self.env.host_mmap && (!write || self.env.inline_stores) {
            // Require canonical Sv39 (sar(addr,38) is 0 or -1). Without
            // this check an arbitrary guest pointer could escape the
            // reserved window and access unrelated host process memory.
            self.a.mov(AX, SI);
            self.a.shift(7, AX, Some(38), true);
            self.a.alu(0, AX, 1, true);
            self.a.shift(5, AX, Some(1), true);
            self.a.branch(Some(5), slow);
            if size > 1 && !self.aligned.contains(&bi.pc) {
                // Aligned power-of-two accesses cannot cross a window edge.
                // Split/unaligned accesses retain the precise helper.
                self.a.mov(AX, SI);
                self.a.alu(4, AX, size as i32 - 1, true);
                self.a.branch(Some(5), slow);
            } else if size > 1 {
                self.elided_alignment_guards += 1;
            }
            self.a.lea_label(CX, slow);
            if write {
                if size == 2 {
                    self.a.bytes(&[0x66]);
                }
                if size == 1 {
                    self.a.bytes(&[0x40]); // DIL, not BH
                }
                self.a.mem_indexed(
                    &[if size == 1 { 0x88 } else { 0x89 }],
                    DI,
                    TLB,
                    Some(SI),
                    0,
                    size == 8,
                );
                self.a.mem(&[0xc6], 0, CPU, OFS_RESV, false);
                self.a.bytes(&[0]);
            } else {
                self.load_value(i.op, TLB, Some(SI), 0);
            }
        } else if !write || self.env.inline_stores {
            // The bank base stays in RBP across accesses. Indexed host
            // operands avoid serial additions to form both addresses.
            self.a.mov(DX, SI);
            self.a.alu(4, DX, !0xfff | (size as i32 - 1), true);
            self.translation(bi.load_slot, write, slow);
            if write {
                if size == 2 {
                    self.a.bytes(&[0x66]);
                }
                if size == 1 {
                    self.a.bytes(&[0x40]);
                } // DIL, not BH
                self.a.mem_indexed(
                    &[if size == 1 { 0x88 } else { 0x89 }],
                    DI,
                    AX,
                    Some(SI),
                    0,
                    size == 8,
                );
                self.a.mem(&[0xc6], 0, CPU, OFS_RESV, false);
                self.a.bytes(&[0]);
            } else {
                self.load_value(i.op, AX, Some(SI), 0);
            }
        } else {
            self.a.branch(None, slow);
        }
        let kind = match i.op {
            Op::Lb => MEM_LB,
            Op::Lbu => MEM_LBU,
            Op::Lh => MEM_LH,
            Op::Lhu => MEM_LHU,
            Op::Lw => MEM_LW,
            Op::Lwu | Op::Flw => MEM_LWU,
            Op::Ld | Op::Fld => MEM_LD,
            _ => MEM_SB + size.trailing_zeros() as u64,
        };
        self.memory_exits.push((slow, done, bi.pc, kind));
        // A hit falls through. A successful slow helper jumps back
        // here with the same value in AX; faults return instead.
        self.a.bind(done);
        if !write {
            if matches!(i.op, Op::Flw | Op::Fld) {
                if i.op == Op::Flw {
                    self.a.imm(CX, 0xffff_ffff_0000_0000);
                    self.a.rr(&[0x09], AX, CX, true);
                }
                self.a.store(AX, CPU, OFS_F + 8 * i.rd as i32);
                self.fs_dirty();
            } else {
                self.set(i.rd);
            }
        }
    }

    fn memory_slow(&mut self, pc: u64, kind: u64) {
        self.flush_regs();
        // jit_mem is System V. Save just our volatile cached registers,
        // and align the stack before constructing its six arguments.
        for r in [8, 9, 10, 11] {
            self.a.push(r);
        }
        self.a.alu(5, 4, 8, true); // sub rsp,8
        self.a.mov(8, DI);
        self.a.mov(DI, CPU);
        self.a.mov(DX, COUNT);
        self.pc(CX, pc);
        self.a.imm(9, kind);
        self.sym(AX, Sym::DirectMem);
        self.a.bytes(&[0xff, 0xd0]);
        self.a.alu(0, 4, 8, true);
        for r in [11, 10, 9, 8] {
            self.a.pop(r);
        }
        self.a.rr(&[0x85], AX, AX, true);
        self.a.branch(Some(5), self.ret); // jit_mem already set fault PC/count
        if kind < MEM_SB {
            self.a.load(AX, CPU, OFS_VALUE);
        }
    }

    fn fs_dirty(&mut self) {
        self.a.load(DX, CPU, OFS_MSTATUS);
        self.a.imm(CX, (3 << isa::ms::FS) | (1u64 << isa::ms::SD));
        self.a.rr(&[0x09], DX, CX, true);
        self.a.store(DX, CPU, OFS_MSTATUS);
    }

    fn floating(&mut self, bi: &BlockInstr) {
        use Op::*;
        self.count();
        self.a.load(DX, CPU, OFS_MSTATUS);
        self.a.alu(4, DX, 3 << isa::ms::FS, true);
        let slow = self.a.label();
        let done = self.a.label();
        self.a.branch(Some(4), slow);
        let i = bi.instr;
        if let Some((size, write)) = mem_access(i.op) {
            self.memory(bi, size, write);
        } else if matches!(i.op, FmvXD | FmvXW) {
            self.a.load(AX, CPU, OFS_F + 8 * i.rs1 as i32);
            if i.op == FmvXW {
                self.a.sext32();
            }
            self.set(i.rd);
        } else {
            self.get(AX, i.rs1);
            if i.op == FmvWX {
                self.a.rr(&[0x89], AX, AX, false);
                self.a.imm(CX, 0xffff_ffff_0000_0000);
                self.a.rr(&[0x09], AX, CX, true);
            }
            self.a.store(AX, CPU, OFS_F + 8 * i.rd as i32);
            self.fs_dirty();
        }
        self.a.branch(None, done);
        self.a.bind(slow);
        self.helper(bi);
        self.a.bind(done);
    }

    fn helper(&mut self, bi: &BlockInstr) {
        self.count();
        self.flush_regs();
        self.a.mov(DI, CPU);
        self.a.imm(SI, pack_instr(&bi.instr));
        self.a.imm(DX, bi.raw as u64);
        self.sym(AX, Sym::Exec);
        self.a.alu(5, 4, 8, true);
        self.a.bytes(&[0xff, 0xd0]);
        self.a.alu(0, 4, 8, true);
        let ok = self.a.label();
        self.a.mem(&[0x0f, 0xb6], AX, CPU, OFS_FAULT, false);
        self.a.rr(&[0x85], AX, AX, false);
        self.a.branch(Some(4), ok);
        if matches!(csr_kind(&bi.instr), Some(CsrKind::Helper)) {
            self.a.alu(7, AX, FAULT_STOP as i32, false);
            let fail = self.a.label();
            self.a.branch(Some(5), fail);
            self.a.mem(&[0xc6], 0, CPU, OFS_FAULT, false);
            self.a.bytes(&[0]);
            self.pc(AX, bi.pc.wrapping_add(bi.size));
            // A CSR stop must return to dispatch, not chain.
            self.a.store(AX, CPU, OFS_PC);
            self.a.mov(AX, COUNT);
            self.a.branch(None, self.ret);
            self.a.bind(fail);
        }
        self.fault(bi.pc);
        self.a.bind(ok);
        if let Some(host) = self.regs[bi.instr.rd as usize] {
            self.a.load(host, CPU, OFS_X + 8 * bi.instr.rd as i32);
        }
    }

    fn instr(&mut self, bi: &BlockInstr, following: Option<u64>) -> bool {
        use Op::*;
        let i = bi.instr;
        let next = bi.pc.wrapping_add(bi.size);
        if native_fp(i.op) {
            self.floating(bi);
            return false;
        }
        if let Some((size, write)) = mem_access(i.op) {
            self.memory(bi, size, write);
            return false;
        }
        if matches!(csr_kind(&i), Some(CsrKind::Counter)) {
            self.count();
            let instret = i.imm as u16 & 0xfff == isa::CSR_INSTRET;
            self.a
                .load(AX, CPU, if instret { OFS_INSTRET } else { OFS_TIME });
            self.a.rr(&[0x01], AX, COUNT, true);
            if instret {
                self.a.lea(AX, AX, -1);
            }
            self.set(i.rd);
            return false;
        }
        if matches!(i.op, Sh1add | Sh2add | Sh3add) {
            let shift = match i.op {
                Sh1add => 1,
                Sh2add => 2,
                _ => 3,
            };
            self.scaled(i.rd, Some(i.rs2), i.rs1, shift, 0);
            return false;
        }
        if self.alu_instr(i) {
            return false;
        }
        // AX and CX are operand temporaries, never allocated to guests.
        if !matches!(
            i.op,
            Lui | Auipc | Jal | Fence | Beq | Bne | Blt | Bge | Bltu | Bgeu
        ) {
            self.get(AX, i.rs1);
        }
        if matches!(
            i.op,
            Sll | Srl
                | Sra
                | Sllw
                | Srlw
                | Sraw
                | Rol
                | Ror
                | Rolw
                | Rorw
                | Slt
                | Sltu
                | AddUw
                | Sh1addUw
                | Sh2addUw
                | Sh3addUw
                | Andn
                | Orn
                | Xnor
        ) {
            self.get(CX, i.rs2);
        }
        match i.op {
            Sll | Srl | Sra | Sllw | Srlw | Sraw | Slli | Srli | Srai | Slliw | Srliw | Sraiw
            | Rol | Ror | Rolw | Rorw | Rori | Roriw => {
                let kind = match i.op {
                    Sll | Sllw | Slli | Slliw => 4,
                    Srl | Srlw | Srli | Srliw => 5,
                    Sra | Sraw | Srai | Sraiw => 7,
                    Rol | Rolw => 0,
                    _ => 1,
                };
                let immediate = matches!(
                    i.op,
                    Slli | Srli | Srai | Slliw | Srliw | Sraiw | Rori | Roriw
                );
                let word = matches!(
                    i.op,
                    Sllw | Srlw | Sraw | Slliw | Srliw | Sraiw | Rolw | Rorw | Roriw
                );
                self.a
                    .shift(kind, AX, immediate.then_some(i.imm as u8), !word);
                if word {
                    self.a.sext32();
                }
            }
            Slt | Sltu | Slti | Sltiu => {
                if matches!(i.op, Slti | Sltiu) {
                    self.a.alu(7, AX, i.imm, true);
                } else {
                    self.a.rr(&[0x39], AX, CX, true);
                }
                let cc = if matches!(i.op, Slt | Slti) { 0xc } else { 2 };
                self.a.bytes(&[0x0f, 0x90 | cc, 0xc0, 0x0f, 0xb6, 0xc0]);
            }
            Lui => self.a.imm(AX, i.imm as i64 as u64),
            Auipc => self.pc(AX, bi.pc.wrapping_add(i.imm as i64 as u64)),
            Beq | Bne | Blt | Bge | Bltu | Bgeu => {
                self.count();
                let left = self.operand(i.rs1, AX);
                let right = self.operand(i.rs2, CX);
                self.a.rr(&[0x39], left, right, true);
                let mut cc = match i.op {
                    Beq => 4,
                    Bne => 5,
                    Blt => 0xc,
                    Bge => 0xd,
                    Bltu => 2,
                    _ => 3,
                };
                let mut taken = bi.pc.wrapping_add(i.imm as i64 as u64);
                let mut fallthrough = next;
                // Match physical emission order, not just guest PC
                // order. Never bypass a backward edge's loop limit.
                if following == Some(taken) && taken > bi.pc {
                    std::mem::swap(&mut taken, &mut fallthrough);
                    cc ^= 1;
                }
                let direct = (taken > bi.pc)
                    .then(|| self.targets.get(&taken).copied())
                    .flatten();
                let label = direct.unwrap_or_else(|| {
                    let label = self.a.label();
                    self.edges.push((label, taken, bi.pc));
                    label
                });
                self.a.branch(Some(cc), label);
                if following != Some(fallthrough) || fallthrough <= bi.pc {
                    self.jump_to(fallthrough, bi.pc);
                }
                return true;
            }
            Jal => {
                self.pc(AX, next);
                self.set(i.rd);
                self.jump_to(bi.pc.wrapping_add(i.imm as i64 as u64), bi.pc);
                return true;
            }
            Jalr => {
                self.count();
                self.a.lea(DX, AX, i.imm);
                self.a.alu(4, DX, -2, true);
                self.pc(AX, next);
                self.set(i.rd);
                self.a.mov(AX, DX);
                self.a.branch(None, self.chain);
                return true;
            }
            Fence => return false,
            AddUw | Sh1addUw | Sh2addUw | Sh3addUw | SlliUw => {
                self.a.rr(&[0x89], AX, AX, false);
                let shift = match i.op {
                    Sh1addUw => 1,
                    Sh2addUw => 2,
                    Sh3addUw => 3,
                    SlliUw => i.imm as u8,
                    _ => 0,
                };
                if shift != 0 {
                    self.a.shift(4, AX, Some(shift), true);
                }
                if i.op != SlliUw {
                    self.a.rr(&[0x01], AX, CX, true);
                }
            }
            Andn | Orn | Xnor => {
                self.a.rr(&[0xf7], CX, 2, true); // NOT rcx (/2)
                self.a.rr(
                    &[match i.op {
                        Andn => 0x21,
                        Orn => 0x09,
                        _ => 0x31,
                    }],
                    AX,
                    CX,
                    true,
                );
            }
            SextB | SextH | ZextH => self.a.rr(
                &[
                    0x0f,
                    match i.op {
                        SextB => 0xbe,
                        SextH => 0xbf,
                        _ => 0xb7,
                    },
                ],
                AX,
                AX,
                true,
            ),
            Rev8 => self.a.bytes(&[0x48, 0x0f, 0xc8]),
            _ => {
                self.helper(bi);
                return false;
            }
        }
        self.set(i.rd);
        false
    }
}
