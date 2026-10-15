//! Cranelift JIT compiler for guest code.
//!
//! Guest code is translated one basic block at a time. A block starts
//! at a guest PC and extends until a control transfer instruction, an
//! instruction the JIT does not support, the end of the guest page, or
//! a length limit. Unsupported instructions (CSR, system, floating
//! point, atomic, and vector instructions) end the block and run in
//! the interpreter.
//!
//! Blocks are keyed by both the virtual PC and the physical location
//! of the code. Guest PC values are computed relative to the PC at the
//! block entry, so the machine code depends only on the code bytes and
//! their page offset, and is reused when the same code appears at
//! another address. Generated code keeps the guest registers in SSA
//! values and writes them through to the CPU state. Loads and stores
//! inline the TLB lookup and access guest RAM directly on a hit;
//! misses, MMIO, and faults call back into the CPU. A block returns the
//! number of instructions it executed so that the CPU time and
//! instruction counters advance exactly as in the interpreter.
//!
//! RISC-V requires fence.i before executing modified code, so fence.i
//! starts a new epoch. A block compiled in an older epoch is checked
//! against the current guest code bytes before it runs again.
//!
//! Blocks chain to each other: an exit looks up the next block in the
//! jump cache and tail-calls it, up to an instruction budget, so most
//! block transitions do not return to the dispatcher.
//!
//! The generated code is position independent: it reaches host
//! addresses through the CPU state, so it can be saved to a code cache
//! file and a later run can map the file and run the code in place.
//! Loaded blocks are validated against the guest code like blocks from
//! an older epoch.

use super::Cpu;
use crate::isa::{self, Exception, Instr, Op};
use crate::memory::PAGE_SIZE;
use crate::mmu::JitTlbEntry;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, InstBuilder, MemFlagsData, SigRef, Signature, Type, Value,
};
use cranelift_codegen::isa::{CallConv, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::mem::offset_of;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Compiled block entry point. Blocks use Cranelift's tail calling
/// convention with the arguments (cpu: *mut Cpu, count: u64), where
/// count is the number of instructions executed by the chained blocks
/// before this one. A block returns the total number of executed
/// instructions in the low 32 bits and the exit status in the high
/// bits. The dispatcher enters blocks through the enter trampoline.
type BlockFn = *const u8;

/// Trampoline that calls the block with the system calling convention.
type EnterFn = unsafe extern "C" fn(*mut Cpu, BlockFn) -> u64;

/// Number of entries in the jump cache.
const JCACHE_SIZE: usize = 16384;

/// Jump cache entry. Blocks jump directly to the next block through
/// the jump cache, which maps a guest virtual PC to its block. An entry
/// is valid if its key matches Cpu::jit_jkey, which combines the
/// privilege mode and fetch translation state with a generation that
/// changes on TLB flushes and code changes.
#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct JEntry {
    vpc: u64,
    key: u64,
    func: BlockFn,
}

const EMPTY_JENTRY: JEntry = JEntry {
    vpc: u64::MAX,
    key: 0,
    func: std::ptr::null(),
};

/// Exit status: the block completed and PC holds the next PC.
const EXIT_OK: u64 = 0;
/// Exit status: an instruction raised an exception, which is stored
/// in `Cpu::jit_exception`; PC holds the faulting instruction.
const EXIT_FAULT: u64 = 1;

const CACHE_BITS: u32 = 16;
const CACHE_SIZE: usize = 1 << CACHE_BITS;
const MAX_BLOCK_INSTRS: usize = 200;

/// Maximum number of instructions in an optimized region.
const MAX_REGION_INSTRS: usize = 128;

/// Default number of times the interpreter must reach a block start
/// before the block is compiled.
pub const DEFAULT_HOT_THRESHOLD: u16 = 1000;

/// JIT statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitStats {
    pub compiled: u64,
    pub compiled_instrs: u64,
    pub compile_time: Duration,
    pub invalidated: u64,
    /// Calls from the dispatcher into compiled code.
    pub dispatches: u64,
    /// Instructions executed by compiled code.
    pub jit_instrs: u64,
    /// Blocks reused for the same code at another RAM location.
    pub reused: u64,
    /// Blocks loaded from the code cache.
    pub loaded: u64,
    /// Loaded blocks that matched the guest code and ran.
    pub cache_hits: u64,
}

impl std::fmt::Display for JitStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "JIT: {} blocks ({} instructions) compiled in {:?}, {} dispatches, \
             {} instructions executed, {} invalidated",
            self.compiled,
            self.compiled_instrs,
            self.compile_time,
            self.dispatches,
            self.jit_instrs,
            self.invalidated
        )?;
        if self.reused > 0 {
            write!(f, ", {} reused at other addresses", self.reused)?;
        }
        if self.loaded > 0 {
            write!(
                f,
                ", {} loaded from cache ({} used)",
                self.loaded, self.cache_hits
            )?;
        }
        Ok(())
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
    /// RAM offset of the guest code bytes.
    phys: u64,
    /// Guest code bytes the block was compiled from.
    code: Vec<u8>,
    /// Block key: virtual PC and RAM offset of the block start.
    vpc: u64,
    start: u64,
    instrs: u32,
    /// Machine code length.
    size: u32,
    /// Number of runs since the block was last used, for blocks
    /// loaded from the code cache.
    age: u8,
    /// The block ran in this run.
    used: bool,
}

/// Epoch of blocks loaded from the code cache. They are checked
/// against the guest code before they first run.
const UNVALIDATED: u64 = u64::MAX;

/// Cache slot marking a removed block. Unlike an empty slot, lookups
/// fall back to the block index.
const TOMBSTONE: CacheEntry = CacheEntry {
    vpc: u64::MAX,
    phys: u64::MAX,
    block: u32::MAX,
};

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
    /// Mapped code cache files, which loaded blocks run from.
    mapped: Vec<Mapping>,
    /// Linux perf map of the blocks, enabled with RSEMU_PERF_MAP.
    perf_map: Option<std::io::BufWriter<std::fs::File>>,
    /// Trampoline arena and entry.
    _enter_arena: CodeArena,
    enter: EnterFn,
    /// Helper stubs: load, store, exec, probe.
    stubs: [u64; 4],
    jcache: Box<[JEntry]>,
    /// Hashed bitmap of the pages with jump cache entries.
    jcache_pages: [u64; 64],
    cache: Box<[CacheEntry]>,
    blocks: Vec<Block>,
    index: HashMap<(u64, u64), u32, BuildHasherDefault<FxHasher>>,
    /// Blocks by code key, for reusing compiled code when the same
    /// code appears at another address.
    by_code: HashMap<u64, Vec<u32>, BuildHasherDefault<FxHasher>>,
    /// Bitmap filter of the by_code keys.
    code_filter: Box<[u64]>,
    hot: Box<[u16]>,
    hot_threshold: u16,
    /// Compile with Cranelift's optimizations.
    optimize: bool,
    epoch: u64,
    pub stats: JitStats,
    /// Persistent code cache file.
    cache_path: Option<PathBuf>,
    /// Code cache status messages for the summary.
    cache_notes: Vec<String>,
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

/// The RAM pages that contain compiled code. Stores to them go
/// through the CPU, which invalidates the code. Compiled code stores
/// directly through the store tags of the JIT TLB, so the entries of
/// code pages lose their store tags.
struct CodePages<'a> {
    mem: &'a crate::memory::Memory,
    jtlb: &'a mut [JitTlbEntry],
    /// Host address of RAM offset 0.
    ram: u64,
}

impl CodePages<'_> {
    /// Marks the RAM page at the offset as containing compiled code.
    fn mark(&mut self, ofs: u64) {
        if !self.mem.mark_code(ofs) {
            return;
        }
        let host = self.ram + (ofs & !0xfff);
        for e in self.jtlb.iter_mut() {
            if e.tag_w != u64::MAX && (e.tag_w & !0xfff).wrapping_add(e.delta) == host {
                e.tag_w = u64::MAX;
            }
        }
    }
}

/// Returns the code pages of the CPU. A macro so that the borrows of
/// the fields stay disjoint from the JIT.
macro_rules! code_pages {
    ($cpu:expr) => {
        &mut CodePages {
            mem: &$cpu.mem,
            jtlb: &mut $cpu.jtlb[..],
            ram: $cpu.mem.ptr_at(0) as u64,
        }
    };
}

const CODE_FILTER_WORDS: usize = 1024;

/// Returns the code key of a block: the page offset of the block start
/// and its first two code bytes.
#[inline]
fn code_key(vpc: u64, first: u16) -> u64 {
    (vpc & (PAGE_SIZE - 1)) << 16 | first as u64
}

#[inline]
fn code_filter_bit(key: u64) -> (usize, u64) {
    let h = key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 48;
    ((h >> 6) as usize % CODE_FILTER_WORDS, 1 << (h & 63))
}

impl Block {
    fn code_key(&self) -> u64 {
        let i = ((self.vpc - self.phys) & (PAGE_SIZE - 1)) as usize;
        code_key(
            self.vpc,
            u16::from_le_bytes([self.code[i], self.code[i + 1]]),
        )
    }
}

#[inline]
fn slot(phys: u64) -> usize {
    (((phys >> 1) ^ (phys >> (CACHE_BITS + 1))) as usize) & (CACHE_SIZE - 1)
}

/// How compiled code executes a CSR instruction.
#[derive(Clone, Copy, PartialEq)]
enum CsrKind {
    /// A read of a counter, computed from the instruction count.
    Counter,
    /// An access to a CSR that does not change the privilege or
    /// translation state, executed by the interpreter.
    Helper,
}

/// Returns how compiled code executes the CSR instruction, or None if
/// it must run in the dispatcher.
#[inline(always)]
fn csr_kind(i: &Instr) -> Option<CsrKind> {
    use Op::*;
    if !matches!(i.op, Csrrw | Csrrs | Csrrc | Csrrwi | Csrrsi | Csrrci) {
        return None;
    }
    let write = matches!(i.op, Csrrw | Csrrwi) || i.rs1 != 0;
    match (i.imm as u32 & 0xfff) as u16 {
        isa::CSR_CYCLE | isa::CSR_TIME | isa::CSR_INSTRET if !write => Some(CsrKind::Counter),
        isa::CSR_FFLAGS
        | isa::CSR_FRM
        | isa::CSR_FCSR
        | isa::CSR_SSTATUS
        | isa::CSR_SIE
        | isa::CSR_STVEC
        | isa::CSR_SCOUNTEREN
        | isa::CSR_SSCRATCH
        | isa::CSR_SEPC
        | isa::CSR_SCAUSE
        | isa::CSR_STVAL
        | isa::CSR_SIP => Some(CsrKind::Helper),
        _ => None,
    }
}

/// Tests if the JIT can compile the instruction.
#[inline(always)]
pub(super) fn jitable(i: &Instr) -> bool {
    jitable_op(i.op) || csr_kind(i).is_some()
}

/// Tests if the JIT compiles the instruction, apart from CSR
/// instructions, which depend on the CSR number.
#[inline(always)]
fn jitable_op(op: Op) -> bool {
    use Op::*;
    native_fp(op)
        || helper_op(op)
        || matches!(
            op,
            Add | Sub
                | Sll
                | Slt
                | Sltu
                | Xor
                | Srl
                | Sra
                | Or
                | And
                | Addi
                | Slti
                | Sltiu
                | Xori
                | Ori
                | Andi
                | Slli
                | Srli
                | Srai
                | Addw
                | Subw
                | Sllw
                | Srlw
                | Sraw
                | Addiw
                | Slliw
                | SlliUw
                | Srliw
                | Sraiw
                | Mul
                | Mulh
                | Mulhsu
                | Mulhu
                | Div
                | Divu
                | Rem
                | Remu
                | Mulw
                | Divw
                | Divuw
                | Remw
                | Remuw
                | Lb
                | Lh
                | Lw
                | Ld
                | Lbu
                | Lhu
                | Lwu
                | Sb
                | Sh
                | Sw
                | Sd
                | Beq
                | Bne
                | Blt
                | Bge
                | Bltu
                | Bgeu
                | Jal
                | Jalr
                | Lui
                | Auipc
                | Fence
                | CzeroEqz
                | CzeroNez
                | Bclr
                | Bclri
                | Bext
                | Bexti
                | Binv
                | Binvi
                | Bset
                | Bseti
                | AddUw
                | Sh1add
                | Sh2add
                | Sh3add
                | Sh1addUw
                | Sh2addUw
                | Sh3addUw
                | Andn
                | Orn
                | Xnor
                | Clz
                | Clzw
                | Ctz
                | Ctzw
                | Cpop
                | Cpopw
                | Max
                | Maxu
                | Min
                | Minu
                | SextB
                | SextH
                | ZextH
                | Rol
                | Rolw
                | Ror
                | Rorw
                | Rori
                | Roriw
                | Rev8
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
        LrW | ScW
            | AmoaddW
            | AmoswapW
            | AmoxorW
            | AmoandW
            | AmoorW
            | AmominW
            | AmomaxW
            | AmominuW
            | AmomaxuW
            | LrD
            | ScD
            | AmoaddD
            | AmoswapD
            | AmoxorD
            | AmoandD
            | AmoorD
            | AmominD
            | AmomaxD
            | AmominuD
            | AmomaxuD
            | FaddS
            | FaddD
            | FsubS
            | FsubD
            | FmulS
            | FmulD
            | FdivS
            | FdivD
            | FsqrtS
            | FsqrtD
            | FmaddS
            | FmaddD
            | FmsubS
            | FmsubD
            | FnmsubS
            | FnmsubD
            | FnmaddS
            | FnmaddD
            | FsgnjS
            | FsgnjnS
            | FsgnjxS
            | FsgnjD
            | FsgnjnD
            | FsgnjxD
            | FminS
            | FmaxS
            | FminD
            | FmaxD
            | FeqS
            | FltS
            | FleS
            | FeqD
            | FltD
            | FleD
            | FcvtWS
            | FcvtWuS
            | FcvtLS
            | FcvtLuS
            | FcvtWD
            | FcvtWuD
            | FcvtLD
            | FcvtLuD
            | FcvtSW
            | FcvtSWu
            | FcvtSL
            | FcvtSLu
            | FcvtDW
            | FcvtDWu
            | FcvtDL
            | FcvtDLu
            | FcvtSD
            | FcvtDS
            | FclassS
            | FclassD
            | Vsetvli
            | Vsetivli
            | Vsetvl
            | VandVX
            | VxorVV
            | VmsneVV
            | VidV
            | VmvVX
            | VmvVV
            | VmvVI
            | Vle8V
            | Vle16V
            | Vle32V
            | Vle64V
            | Vse8V
            | Vse16V
            | Vse32V
            | Vse64V
            | VaddVI
            | VmseqVI
            | VsllVI
            | VsraVI
            | Clmul
            | Clmulh
            | Clmulr
            | Pack
            | Packh
            | Packw
            | Brev8
            | Xperm4
            | Xperm8
            | OrcB
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
    let csr = matches!(
        instr.op,
        Op::Csrrw | Op::Csrrs | Op::Csrrc | Op::Csrrwi | Op::Csrrsi | Op::Csrrci
    );
    if let Err(e) = cpu.execute(instr, raw as u32, size) {
        cpu.jit_exception = Some(e);
        cpu.jit_fault = 1;
        return;
    }
    // A CSR write that leaves an interrupt deliverable stops the block
    // after the instruction, so that the dispatcher takes it.
    if csr && cpu.irq_deliverable() {
        cpu.jit_fault = FAULT_STOP;
        cpu.next_irq_check = 0;
    }
}

/// Cpu::jit_fault value requesting that the block stops after the
/// helper instruction.
const FAULT_STOP: u8 = 2;

/// Returns the size and whether it is a store of an integer or
/// floating point memory access compiled to native code.
fn mem_access(op: Op) -> Option<(i64, bool)> {
    use Op::*;
    Some(match op {
        Lb | Lbu => (1, false),
        Lh | Lhu => (2, false),
        Lw | Lwu | Flw => (4, false),
        Ld | Fld => (8, false),
        Sb => (1, true),
        Sh => (2, true),
        Sw | Fsw => (4, true),
        Sd | Fsd => (8, true),
        _ => return None,
    })
}

/// Guest register of the stack pointer.
const SP: u8 = 2;

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

/// Checks for the generated code that the data accesses to the range
/// [lo, hi] are on one page that the JIT TLB allows for loads, or for
/// stores if write is set, filling the TLB entry if needed. Returns 1
/// if they are, otherwise 0; the accesses then run in the interpreter,
/// which raises any exception precisely.
unsafe extern "C" fn jit_probe(cpu: *mut Cpu, lo: u64, hi: u64, write: u64) -> u64 {
    let cpu = &mut *cpu;
    cpu.jit_probe_range(lo, hi, write != 0) as u64
}

/// Opens the perf map file /tmp/perf-PID.map that names the compiled
/// code for Linux perf, if the environment variable RSEMU_PERF_MAP is
/// set. Blocks are named by their guest PC.
fn open_perf_map(glue: &CodeArena) -> Option<std::io::BufWriter<std::fs::File>> {
    use std::io::Write;
    std::env::var_os("RSEMU_PERF_MAP")?;
    let path = format!("/tmp/perf-{}.map", std::process::id());
    let f = std::fs::File::create(path).ok()?;
    let mut f = std::io::BufWriter::with_capacity(1 << 20, f);
    let _ = writeln!(f, "{:x} {:x} jit-glue", glue.ptr as usize, glue.pos);
    Some(f)
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
        // The code arena relies on coherent instruction caches.
        if !cfg!(target_arch = "x86_64") {
            return Err("the JIT supports x86-64 hosts only".to_string());
        }
        let (compiler, worker) = if background {
            (None, Some(Worker::start()?))
        } else {
            (Some(Compiler::new()?), None)
        };
        let mut enter_arena = CodeArena::new(PAGE_SIZE as usize)?;
        let enter = compile_enter(&mut enter_arena)?;
        let stubs = compile_stubs(&mut enter_arena)?;
        let perf_map = open_perf_map(&enter_arena);
        Ok(Jit {
            compiler,
            worker,
            _enter_arena: enter_arena,
            enter,
            stubs,
            jcache: vec![EMPTY_JENTRY; JCACHE_SIZE].into_boxed_slice(),
            jcache_pages: [0; 64],
            pending: HashSet::default(),
            poll: 0,
            arena: CodeArena::new(CODE_ARENA_SIZE)?,
            mapped: Vec::new(),
            perf_map,
            cache: vec![CacheEntry::default(); CACHE_SIZE].into_boxed_slice(),
            blocks: Vec::new(),
            index: HashMap::default(),
            by_code: HashMap::default(),
            code_filter: vec![0u64; CODE_FILTER_WORDS].into_boxed_slice(),
            hot: vec![0u16; CACHE_SIZE].into_boxed_slice(),
            hot_threshold: hot_threshold.max(1),
            optimize: false,
            epoch: 0,
            stats: JitStats::default(),
            cache_path: None,
            cache_notes: Vec::new(),
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
        code_pages: &mut CodePages,
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
            // Mark before comparing, so that a concurrent write is
            // noticed.
            code_pages.mark(b.phys);
            if !cpu_mem.bytes_equal(b.phys, &b.code) {
                self.stats.invalidated += 1;
                self.index.remove(&(vpc, phys));
                self.cache[s] = TOMBSTONE;
                return None;
            }
            if !b.used {
                b.used = true;
                self.stats.cache_hits += 1;
            }
            b.epoch = self.epoch;
        }
        Some(b.func)
    }

    /// Counts a visit to the block start. Returns the number of
    /// visits.
    fn bump(&mut self, phys: u64) -> u16 {
        let h = &mut self.hot[slot(phys)];
        *h = h.saturating_add(1);
        *h
    }

    /// Looks for a compiled block for the same code at another
    /// address. The generated code depends only on the code bytes and
    /// their page offset, so a block whose code matches the code at
    /// vpc / phys can run there too. Returns the block function for
    /// the new location.
    fn reuse(
        &mut self,
        mem: &crate::memory::Memory,
        code_pages: &mut CodePages,
        vpc: u64,
        phys: u64,
    ) -> Option<BlockFn> {
        let key = code_key(vpc, mem.read_u16(phys));
        let (w, bit) = code_filter_bit(key);
        if self.code_filter[w] & bit == 0 {
            return None;
        }
        let page_base = phys & !(PAGE_SIZE - 1);
        let candidates = self.by_code.get(&key)?;
        // Mark before comparing, so that a concurrent write is noticed.
        code_pages.mark(phys);
        let found = candidates.iter().copied().find(|&idx| {
            let b = &self.blocks[idx as usize];
            mem.bytes_equal(page_base + (b.phys & (PAGE_SIZE - 1)), &b.code)
        })?;
        let b = &mut self.blocks[found as usize];
        if !b.used {
            b.used = true;
            self.stats.cache_hits += 1;
        }
        let block = Block {
            func: b.func,
            epoch: self.epoch,
            phys: page_base + (b.phys & (PAGE_SIZE - 1)),
            code: b.code.clone(),
            vpc,
            start: phys,
            instrs: b.instrs,
            size: b.size,
            age: 0,
            used: true,
        };
        self.stats.reused += 1;
        let idx = self.blocks.len() as u32;
        let func = block.func;
        self.blocks.push(block);
        self.index.insert((vpc, phys), idx);
        self.cache[slot(phys)] = CacheEntry {
            vpc,
            phys,
            block: idx + 1,
        };
        Some(func)
    }

    /// Decodes the trace starting at vpc, with its code at the RAM
    /// offset phys. The trace follows not-taken conditional branches
    /// and direct jumps within the page.
    fn decode_block(page: &[u8], vpc: u64) -> Vec<BlockInstr> {
        let rd16 = |o: usize| u16::from_le_bytes([page[o], page[o + 1]]) as u32;
        let page_end = PAGE_SIZE as usize;
        let mut out: Vec<BlockInstr> = Vec::new();
        let (mut pc, mut ofs) = (vpc, (vpc & (PAGE_SIZE - 1)) as usize);
        loop {
            if out.len() >= MAX_BLOCK_INSTRS || ofs + 2 > page_end {
                break;
            }
            let lo = rd16(ofs);
            let (instr, size, raw) = if lo & 0b11 == 0b11 {
                if ofs + 4 > page_end {
                    break;
                }
                let raw = lo | rd16(ofs + 2) << 16;
                (isa::decode(raw).unwrap_or_default(), 4, raw)
            } else {
                (isa::decode_c_fast(lo as u16), 2, lo)
            };
            if !jitable(&instr) {
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
                    ofs = (target & (PAGE_SIZE - 1)) as usize;
                }
                _ => {
                    pc = pc.wrapping_add(size);
                    ofs += size as usize;
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

    /// Decodes the region of the code at vpc for optimized
    /// compilation: the instructions reachable from vpc within the page
    /// through fall-through, branches, and direct jumps, in address
    /// order. Branches and jumps are marked as last instructions; the
    /// translator links them to the region's instructions.
    fn decode_region(page: &[u8], vpc: u64) -> Vec<BlockInstr> {
        let rd16 = |o: usize| u16::from_le_bytes([page[o], page[o + 1]]) as u32;
        let base = vpc & !(PAGE_SIZE - 1);
        let mut region: BTreeMap<u64, BlockInstr> = BTreeMap::new();
        let mut work = vec![vpc];
        while let Some(pc) = work.pop() {
            if region.len() >= MAX_REGION_INSTRS {
                break;
            }
            if pc & !(PAGE_SIZE - 1) != base || region.contains_key(&pc) {
                continue;
            }
            // Instructions may not overlap.
            if let Some((p, b)) = region.range(..pc).next_back() {
                if p + b.size > pc {
                    continue;
                }
            }
            let ofs = (pc - base) as usize;
            if ofs + 2 > page.len() {
                continue;
            }
            let lo = rd16(ofs);
            let (instr, size, raw) = if lo & 0b11 == 0b11 {
                if ofs + 4 > page.len() {
                    continue;
                }
                let raw = lo | rd16(ofs + 2) << 16;
                (isa::decode(raw).unwrap_or_default(), 4, raw)
            } else {
                (isa::decode_c_fast(lo as u16), 2, lo)
            };
            if !jitable(&instr) {
                continue;
            }
            if let Some((&p, _)) = region.range(pc + 1..).next() {
                if p < pc + size {
                    continue;
                }
            }
            let next = pc.wrapping_add(size);
            let target = pc.wrapping_add(instr.imm as i64 as u64);
            match instr.op {
                Op::Jalr => {}
                Op::Jal => work.push(target),
                op if is_terminator(op) => {
                    work.push(target);
                    work.push(next);
                }
                _ => work.push(next),
            }
            region.insert(
                pc,
                BlockInstr {
                    pc,
                    instr,
                    raw,
                    size,
                    last: is_terminator(instr.op),
                },
            );
        }
        region.into_values().collect()
    }

    /// Creates the compile request for the block at vpc / phys. The
    /// page must be marked as a code page before, so that writes after
    /// the snapshot of the code are noticed.
    fn request(cpu: &Cpu, vpc: u64, phys: u64, epoch: u64) -> Option<Request> {
        // Decode from a snapshot of the page, so that the compiled
        // code matches the saved code bytes even if the page changes
        // concurrently.
        let page_base = phys & !(PAGE_SIZE - 1);
        let mut page = vec![0u8; PAGE_SIZE as usize];
        cpu.mem.read_bytes(page_base, &mut page);
        let instrs = if cpu.jit_env().optimize {
            Self::decode_region(&page, vpc)
        } else {
            Self::decode_block(&page, vpc)
        };
        if instrs.is_empty() {
            return None;
        }
        let lo = instrs.iter().map(|b| b.pc & (PAGE_SIZE - 1)).min().unwrap();
        let hi = instrs
            .iter()
            .map(|b| (b.pc & (PAGE_SIZE - 1)) + b.size)
            .max()
            .unwrap();
        let code = page[lo as usize..hi as usize].to_vec();
        Some(Request {
            vpc,
            phys,
            epoch,
            code_ofs: page_base + lo,
            code,
            instrs,
            env: cpu.jit_env(),
        })
    }

    /// Copies the machine code into the arena and adds the block to
    /// the translation cache. Returns the block back if the arena is
    /// full.
    fn add_block(&mut self, block: Block, machine_code: &[u8]) -> Result<BlockFn, Block> {
        let Some(entry) = self.arena.add(machine_code) else {
            return Err(block);
        };
        Ok(self.add_code(block, entry, machine_code.len()))
    }

    /// Adds the block with the machine code at entry to the
    /// translation cache.
    fn add_code(&mut self, mut block: Block, entry: BlockFn, size: usize) -> BlockFn {
        let func: BlockFn = entry;
        block.func = func;
        block.size = size as u32;
        let (vpc, start) = (block.vpc, block.start);
        if let Some(f) = self.perf_map.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{:x} {:x} rv:{vpc:x}", entry as usize, block.size);
        }
        let idx = self.blocks.len() as u32;
        self.blocks.push(block);
        self.index.insert((vpc, start), idx);
        let key = self.blocks[idx as usize].code_key();
        self.by_code.entry(key).or_default().push(idx);
        let (w, bit) = code_filter_bit(key);
        self.code_filter[w] |= bit;
        self.cache[slot(start)] = CacheEntry {
            vpc,
            phys: start,
            block: idx + 1,
        };
        func
    }

    fn flush_perf_map(&mut self) {
        if let Some(f) = self.perf_map.as_mut() {
            let _ = std::io::Write::flush(f);
        }
    }

    /// Drops all compiled code.
    fn flush(&mut self) {
        self.cache.fill(CacheEntry::default());
        self.blocks.clear();
        self.index.clear();
        self.by_code.clear();
        self.code_filter.fill(0);
        self.jcache.fill(EMPTY_JENTRY);
        self.arena.reset();
        self.mapped.clear();
    }

    /// Installs the compiled code. Returns the block function.
    fn install(&mut self, r: Compiled) -> Result<BlockFn, String> {
        let req = r.request;
        let block = Block {
            func: std::ptr::null(),
            epoch: req.epoch,
            phys: req.code_ofs,
            code: req.code,
            vpc: req.vpc,
            start: req.phys,
            instrs: req.instrs.len() as u32,
            size: 0,
            age: 0,
            used: true,
        };
        let func = match self.add_block(block, &r.machine_code) {
            Ok(f) => f,
            Err(block) => {
                // The arena is full: drop all compiled code.
                self.flush();
                self.add_block(block, &r.machine_code)
                    .map_err(|_| "JIT block too large")?
            }
        };
        self.flush_perf_map();
        self.stats.compiled += 1;
        self.stats.compiled_instrs += req.instrs.len() as u64;
        self.stats.compile_time += r.time;
        Ok(func)
    }

    /// Installs the blocks compiled in the background.
    fn install_pending(&mut self, code_pages: &mut CodePages) -> Result<(), String> {
        loop {
            let Some(r) = self.worker.as_ref().and_then(|w| w.rx.try_recv().ok()) else {
                return Ok(());
            };
            let r = r?;
            self.pending.remove(&(r.request.vpc, r.request.phys));
            code_pages.mark(r.request.code_ofs);
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
    /// The optimizing ISA, created on first use.
    isa_opt: Option<OwnedTargetIsa>,
    ctx: Context,
    fctx: FunctionBuilderContext,
}

/// Returns the signature of the block functions.
fn block_signature(ptr: Type) -> Signature {
    let mut sig = Signature::new(CallConv::Tail);
    sig.params.push(AbiParam::new(ptr));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Compiles a glue function with the signature into the arena. The
/// body is built from the function parameters.
fn compile_glue(
    arena: &mut CodeArena,
    sig: Signature,
    body: impl FnOnce(&mut FunctionBuilder, &[Value]),
) -> Result<*const u8, String> {
    let isa = host_isa()?;
    let mut ctx = Context::new();
    let mut fctx = FunctionBuilderContext::new();
    ctx.func.signature = sig;
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        body(&mut b, &params);
        b.seal_all_blocks();
        b.finalize(isa.frontend_config());
    }
    let compiled = ctx
        .compile(&*isa, &mut ControlPlane::default())
        .map_err(|e| format!("JIT glue: {:?}", e.inner))?;
    arena
        .add(compiled.code_buffer())
        .ok_or_else(|| "JIT glue: no space".to_string())
}

/// Compiles the trampoline that calls a block from Rust.
fn compile_enter(arena: &mut CodeArena) -> Result<EnterFn, String> {
    let ptr = types::I64;
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(ptr));
    sig.params.push(AbiParam::new(ptr));
    sig.returns.push(AbiParam::new(types::I64));
    let code = compile_glue(arena, sig, |b, p| {
        let sigref = b.import_signature(block_signature(ptr));
        let zero = b.ins().iconst(types::I64, 0);
        let call = b.ins().call_indirect(sigref, p[1], &[p[0], zero]);
        let r = b.inst_results(call)[0];
        b.ins().return_(&[r]);
    })?;
    // SAFETY: the code was compiled with the EnterFn signature.
    Ok(unsafe { std::mem::transmute::<*const u8, EnterFn>(code) })
}

/// Number of parameters of the helper functions: load, store, exec.
const HELPER_PARAMS: [usize; 4] = [3, 4, 3, 4];

/// Returns the signature of the helper stubs called by the blocks.
/// The stubs preserve all registers, so that the rarely taken helper
/// calls do not constrain register allocation in the blocks.
fn stub_signature(helper: usize) -> Signature {
    let mut sig = Signature::new(CallConv::PreserveAll);
    for _ in 0..HELPER_PARAMS[helper] {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig
}

/// Compiles the stubs that call the helper functions jit_load,
/// jit_store, and jit_exec with the system calling convention. The
/// load stub stores the loaded value in Cpu::jit_value.
fn compile_stubs(arena: &mut CodeArena) -> Result<[u64; 4], String> {
    let helpers = [
        jit_load as *const () as i64,
        jit_store as *const () as i64,
        jit_exec as *const () as i64,
        jit_probe as *const () as i64,
    ];
    let mut out = [0; 4];
    for (h, addr) in helpers.into_iter().enumerate() {
        let code = compile_glue(arena, stub_signature(h), |b, p| {
            let mut sig = Signature::new(CallConv::SystemV);
            for _ in 0..HELPER_PARAMS[h] {
                sig.params.push(AbiParam::new(types::I64));
            }
            if h == 0 || h == 3 {
                sig.returns.push(AbiParam::new(types::I64));
            }
            let sigref = b.import_signature(sig);
            let callee = b.ins().iconst(types::I64, addr);
            let call = b.ins().call_indirect(sigref, callee, p);
            if h == 0 || h == 3 {
                let v = b.inst_results(call)[0];
                b.ins().store(MemFlagsData::trusted(), v, p[0], OFS_VALUE);
            }
            b.ins().return_(&[]);
        })?;
        out[h] = code as u64;
    }
    Ok(out)
}

/// Returns the code generator for the host.
fn host_isa() -> Result<OwnedTargetIsa, String> {
    host_isa_opt(false)
}

/// Returns the host ISA, with Cranelift's optimizations if optimize is
/// set. Without them, compiling is several times faster.
fn host_isa_opt(optimize: bool) -> Result<OwnedTargetIsa, String> {
    let mut flags = settings::builder();
    let level = if optimize { "speed" } else { "none" };
    flags.set("opt_level", level).map_err(|e| e.to_string())?;
    flags
        .set("enable_verifier", "false")
        .map_err(|e| e.to_string())?;
    // Required by tail calls.
    flags
        .set("preserve_frame_pointers", "true")
        .map_err(|e| e.to_string())?;
    cranelift_native::builder()
        .map_err(|e| e.to_string())?
        .finish(settings::Flags::new(flags))
        .map_err(|e| e.to_string())
}

impl Compiler {
    fn new() -> Result<Compiler, String> {
        Ok(Compiler {
            isa: host_isa()?,
            isa_opt: None,
            ctx: Context::new(),
            fctx: FunctionBuilderContext::new(),
        })
    }

    /// Compiles the block into machine code.
    fn compile(&mut self, req: Request) -> Result<Compiled, String> {
        let start = Instant::now();
        if req.env.optimize && self.isa_opt.is_none() {
            self.isa_opt = Some(host_isa_opt(true)?);
        }
        let isa = match &self.isa_opt {
            Some(isa) if req.env.optimize => isa,
            _ => &self.isa,
        };
        let ptr = isa.pointer_type();
        self.ctx.clear();
        self.ctx.func.signature = block_signature(ptr);

        let frontend_config = isa.frontend_config();
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let mut t = Translator::new(&mut b, ptr, req.env, req.vpc);
            t.translate(&req.instrs);
            b.seal_all_blocks();
            b.finalize(frontend_config);
        }

        let compiled = self
            .ctx
            .compile(&**isa, &mut ControlPlane::default())
            .map_err(|e| format!("JIT compile failed at {:x}: {:?}", req.vpc, e.inner))?;
        // The code must be position independent.
        if let Some(r) = compiled.buffer.relocs().first() {
            return Err(format!("JIT: unexpected relocation {:?}", r.kind));
        }
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

/// Code generation options.
#[derive(Clone, Copy)]
struct CodegenEnv {
    inline_stores: bool,
    /// Compile with Cranelift's optimizations. Optimized and
    /// unoptimized code are interchangeable, so this is not part of the
    /// code cache fingerprint.
    optimize: bool,
}

/// Host addresses used by the generated code. The code refers to them
/// through relocations, so that it can be saved and loaded into
/// another process.
#[derive(Clone, Copy)]
enum Sym {
    Tlb,
    Load,
    Store,
    Exec,
    JCache,
    Probe,
}

pub(super) const NUM_SYMS: usize = 6;

/// Values of the symbols, indexed by Sym.
const OFS_X: i32 = offset_of!(Cpu, x) as i32;
const OFS_ADDRS: i32 = offset_of!(Cpu, jit_addrs) as i32;
const OFS_F: i32 = offset_of!(Cpu, f) as i32;
const OFS_MSTATUS: i32 = offset_of!(Cpu, mstatus) as i32;
const OFS_TIME: i32 = offset_of!(Cpu, time) as i32;
const OFS_INSTRET: i32 = offset_of!(Cpu, instret) as i32;
const OFS_PC: i32 = offset_of!(Cpu, pc) as i32;
const OFS_FAULT: i32 = offset_of!(Cpu, jit_fault) as i32;
const OFS_RESV: i32 = offset_of!(Cpu, reservation_valid) as i32;
const OFS_JKEY: i32 = offset_of!(Cpu, jit_jkey) as i32;
const OFS_VALUE: i32 = offset_of!(Cpu, jit_value) as i32;

const JE_VPC: i32 = offset_of!(JEntry, vpc) as i32;
const JE_KEY: i32 = offset_of!(JEntry, key) as i32;
const JE_FUNC: i32 = offset_of!(JEntry, func) as i32;
const JENTRY_SHIFT: i64 = std::mem::size_of::<JEntry>().trailing_zeros() as i64;
const _: () = assert!(std::mem::size_of::<JEntry>().is_power_of_two());

const OFS_JTLB: i32 = offset_of!(Cpu, jtlb) as i32;
const OFS_JIT_DATA_KEY: i32 = offset_of!(Cpu, jit_data_key) as i32;
const JTLB_TAG_R: i32 = offset_of!(JitTlbEntry, tag_r) as i32;
const JTLB_TAG_W: i32 = offset_of!(JitTlbEntry, tag_w) as i32;
const JTLB_DELTA: i32 = offset_of!(JitTlbEntry, delta) as i32;
const JTLB_SHIFT: i64 = std::mem::size_of::<JitTlbEntry>().trailing_zeros() as i64;

const _: () = assert!(std::mem::size_of::<JitTlbEntry>().is_power_of_two());

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
    /// Blocks of the guest PCs that jumps can target inside the
    /// function: the start, and in regions the branch and jump targets.
    leaders: HashMap<u64, cranelift_codegen::ir::Block>,
    /// Leaders whose code has been started; jumps to them are
    /// backward jumps.
    visited: HashSet<u64>,
    /// Instructions executed by the completed loop iterations.
    count: Variable,
    start_vpc: u64,
    /// Guest PC at the block entry, which is start_vpc. Guest PC
    /// values are computed relative to it so that the code does not
    /// depend on the virtual address of the guest code.
    entry_pc: Value,
    vars: [Option<Variable>; 32],
    /// Shared exit blocks.
    exit_blk: Option<cranelift_codegen::ir::Block>,
    chain_blk: Option<cranelift_codegen::ir::Block>,
    /// Signatures of the helper functions.
    load_sig: Option<SigRef>,
    store_sig: Option<SigRef>,
    exec_sig: Option<SigRef>,
    probe_sig: Option<SigRef>,
    /// Groups of stack accesses that share one TLB check: the group and
    /// the offset from the group base of the accesses by guest PC.
    group_of: HashMap<u64, (usize, i64)>,
    groups: Vec<AccessGroup>,
}

/// Accesses through the same base register value within a basic block,
/// whose page is checked once at the first access. The range is
/// [base + lo, base + hi].
struct AccessGroup {
    first_pc: u64,
    lo: i64,
    hi: i64,
    write: bool,
    /// Host address of the base value after the check.
    host_base: Option<Value>,
    /// A store of the group has cleared the LR reservation.
    resv_cleared: bool,
}

impl<'a, 'b> Translator<'a, 'b> {
    fn new(b: &'a mut FunctionBuilder<'b>, ptr: Type, env: CodegenEnv, start_vpc: u64) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let cpu = b.block_params(entry)[0];
        let count_in = b.block_params(entry)[1];
        let entry_pc = b.ins().load(types::I64, Self::flags(), cpu, OFS_PC);
        let header = b.create_block();
        let count = b.declare_var(types::I64);
        b.def_var(count, count_in);
        Translator {
            b,
            ptr,
            env,
            cpu,
            header,
            leaders: HashMap::from([(start_vpc, header)]),
            visited: HashSet::new(),
            count,
            start_vpc,
            entry_pc,
            vars: [None; 32],
            exit_blk: None,
            chain_blk: None,
            load_sig: None,
            store_sig: None,
            exec_sig: None,
            probe_sig: None,
            group_of: HashMap::new(),
            groups: Vec::new(),
        }
    }

    /// Returns the host address of the symbol, which the CPU state
    /// holds so that the code is position independent.
    fn sym(&mut self, s: Sym) -> Value {
        let f = MemFlagsData::trusted().with_readonly().with_can_move();
        self.b
            .ins()
            .load(self.ptr, f, self.cpu, OFS_ADDRS + 8 * s as i32)
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

    /// Returns the guest PC value pc.
    fn pc_value(&mut self, pc: u64) -> Value {
        let delta = pc.wrapping_sub(self.start_vpc) as i64;
        self.b.ins().iadd_imm_s(self.entry_pc, delta)
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
        let ret = if status == 0 {
            count
        } else {
            self.b.ins().bor_imm_s(count, (status << 32) as i64)
        };
        let exit = self.exit_block();
        self.b.ins().jump(exit, &[pc.into(), ret.into()]);
    }

    /// Returns the shared exit block, which takes the guest PC and the
    /// return value.
    fn exit_block(&mut self) -> cranelift_codegen::ir::Block {
        if let Some(blk) = self.exit_blk {
            return blk;
        }
        let blk = self.b.create_block();
        self.b.append_block_param(blk, types::I64);
        self.b.append_block_param(blk, types::I64);
        self.b.set_cold_block(blk);
        self.exit_blk = Some(blk);
        blk
    }

    /// Exits the block to pc after n instructions of the current
    /// iteration.
    fn jump_exit(&mut self, pc: Value, n: u64, status: u64) {
        let c = self.total(n);
        self.exit_with(pc, c, status);
    }

    /// Continues at the guest PC pc after n instructions of the current
    /// iteration: jumps directly to the next block if the jump cache
    /// has one and the instruction budget allows, otherwise returns to
    /// the dispatcher.
    fn chain_exit(&mut self, pc: Value, n: u64) {
        let total = self.total(n);
        let chain = match self.chain_blk {
            Some(blk) => blk,
            None => {
                let blk = self.b.create_block();
                self.b.append_block_param(blk, types::I64);
                self.b.append_block_param(blk, types::I64);
                self.chain_blk = Some(blk);
                blk
            }
        };
        self.b.ins().jump(chain, &[pc.into(), total.into()]);
    }

    /// Emits the shared exit blocks.
    fn finish(&mut self) {
        let f = Self::flags();
        if let Some(chain) = self.chain_blk {
            self.b.switch_to_block(chain);
            let (pc, total) = (self.b.block_params(chain)[0], self.b.block_params(chain)[1]);
            self.b.ins().store(f, pc, self.cpu, OFS_PC);
            let ret = self.b.create_block();
            let lookup = self.b.create_block();
            let check_key = self.b.create_block();
            let jump = self.b.create_block();
            self.b.set_cold_block(ret);

            let more = self
                .b
                .ins()
                .icmp_imm_s(IntCC::SignedLessThan, total, LOOP_LIMIT);
            self.b.ins().brif(more, lookup, &[], ret, &[]);

            self.b.switch_to_block(lookup);
            let idx = self.b.ins().ushr_imm_s(pc, 1);
            let idx = self.b.ins().band_imm_s(idx, JCACHE_SIZE as i64 - 1);
            let ofs = self.b.ins().ishl_imm_s(idx, JENTRY_SHIFT);
            let jcache = self.sym(Sym::JCache);
            let ent = self.b.ins().iadd(jcache, ofs);
            let e_vpc = self.b.ins().load(types::I64, f, ent, JE_VPC);
            let hit = self.b.ins().icmp(IntCC::Equal, e_vpc, pc);
            self.b.ins().brif(hit, check_key, &[], ret, &[]);

            self.b.switch_to_block(check_key);
            let e_key = self.b.ins().load(types::I64, f, ent, JE_KEY);
            let key = self.b.ins().load(types::I64, f, self.cpu, OFS_JKEY);
            let ok = self.b.ins().icmp(IntCC::Equal, e_key, key);
            self.b.ins().brif(ok, jump, &[], ret, &[]);

            self.b.switch_to_block(jump);
            let func = self.b.ins().load(self.ptr, f, ent, JE_FUNC);
            let sigref = self.b.import_signature(block_signature(self.ptr));
            self.b
                .ins()
                .return_call_indirect(sigref, func, &[self.cpu, total]);

            self.b.switch_to_block(ret);
            self.b.ins().return_(&[total]);
        }
        if let Some(exit) = self.exit_blk {
            self.b.switch_to_block(exit);
            let (pc, ret) = (self.b.block_params(exit)[0], self.b.block_params(exit)[1]);
            self.b.ins().store(f, pc, self.cpu, OFS_PC);
            self.b.ins().return_(&[ret]);
        }
    }

    /// Jumps to the guest PC target after n instructions of the
    /// current basic block. Jumps to the block start, and in regions
    /// to the region's branch targets, stay in native code; backward
    /// jumps loop until the loop limit is reached.
    fn jump_to(&mut self, target: u64, n: u64) {
        let pcv = self.pc_value(target);
        let Some(&blk) = self.leaders.get(&target) else {
            self.chain_exit(pcv, n);
            return;
        };
        let total = self.total(n);
        self.b.def_var(self.count, total);
        if !self.visited.contains(&target) {
            self.b.ins().jump(blk, &[]);
            return;
        }
        let more = self
            .b
            .ins()
            .icmp_imm_s(IntCC::SignedLessThan, total, LOOP_LIMIT);
        let out = self.b.create_block();
        self.b.ins().brif(more, blk, &[], out, &[]);
        self.b.switch_to_block(out);
        self.exit_with(pcv, total, EXIT_OK);
    }

    /// Plans the groups of stack accesses that share a TLB check. A
    /// group is a run of accesses through SP within a basic block while
    /// SP does not change. Groups need two accesses and must fit in a
    /// page.
    fn plan_groups(&mut self, instrs: &[BlockInstr]) {
        // Open group: first PC, members (pc, offset), lo, hi, write.
        let mut cur: Option<(u64, Vec<(u64, i64)>, i64, i64, bool)> = None;
        let close = |t: &mut Self, cur: &mut Option<(u64, Vec<(u64, i64)>, i64, i64, bool)>| {
            if let Some((first_pc, members, lo, hi, write)) = cur.take() {
                if members.len() >= 2 && hi - lo < PAGE_SIZE as i64 {
                    let id = t.groups.len();
                    t.groups.push(AccessGroup {
                        first_pc,
                        lo,
                        hi,
                        write,
                        host_base: None,
                        resv_cleared: false,
                    });
                    for (pc, off) in members {
                        t.group_of.insert(pc, (id, off));
                    }
                }
            }
        };
        let mut prev_next = None;
        for bi in instrs {
            let i = &bi.instr;
            // A new basic block starts at leaders and after gaps.
            if self.leaders.contains_key(&bi.pc) || prev_next != Some(bi.pc) {
                close(self, &mut cur);
            }
            prev_next = Some(bi.pc.wrapping_add(bi.size));
            if let Some((size, store)) = mem_access(i.op) {
                if i.rs1 == SP && (!store || self.env.inline_stores) {
                    let off = i.imm as i64;
                    match cur.as_mut() {
                        Some((_, members, lo, hi, write)) => {
                            members.push((bi.pc, off));
                            *lo = (*lo).min(off);
                            *hi = (*hi).max(off + size - 1);
                            *write |= store;
                        }
                        None => cur = Some((bi.pc, vec![(bi.pc, off)], off, off + size - 1, store)),
                    }
                }
            }
            // Any write to the integer or floating point register with
            // the number of SP ends the group, conservatively.
            if i.rd == SP && mem_access(i.op).is_none_or(|(_, st)| !st) {
                close(self, &mut cur);
            }
            if is_terminator(i.op) || helper_op(i.op) || csr_kind(&i).is_some() {
                close(self, &mut cur);
            }
        }
        close(self, &mut cur);
    }

    /// Returns the host address of the access of the instruction at pc
    /// with the guest address base + off if the access is in a checked
    /// group. At the first access of a group, emits the check of the
    /// group's range, which exits to the interpreter at the access if
    /// the range is not accessible directly.
    fn group_host(&mut self, k: u64, pc: u64, base: Value) -> Option<Value> {
        let &(id, off) = self.group_of.get(&pc)?;
        if self.groups[id].first_pc == pc {
            let (lo, hi, write) = (self.groups[id].lo, self.groups[id].hi, self.groups[id].write);
            let lo_v = self.b.ins().iadd_imm_s(base, lo);
            let hi_v = self.b.ins().iadd_imm_s(base, hi);
            let slow = self.b.create_block();
            self.b.set_cold_block(slow);
            let cont = self.b.create_block();
            self.b.append_block_param(cont, types::I64);
            let host_lo = self.tlb_fast_range(lo_v, hi_v, write, slow);
            let hb = self.b.ins().iadd_imm_s(host_lo, -lo);
            self.b.ins().jump(cont, &[hb.into()]);

            // Fill the TLB entry and check again; run the accesses in
            // the interpreter if that does not help.
            self.b.switch_to_block(slow);
            let sigref = self.helper_sig(3);
            let callee = self.sym(Sym::Probe);
            let w = self.iconst(write as i64);
            self.b
                .ins()
                .call_indirect(sigref, callee, &[self.cpu, lo_v, hi_v, w]);
            let ok = self
                .b
                .ins()
                .load(types::I64, Self::flags(), self.cpu, OFS_VALUE);
            let refill = self.b.create_block();
            let out = self.b.create_block();
            self.b.ins().brif(ok, refill, &[], out, &[]);
            self.b.switch_to_block(out);
            let pcv = self.pc_value(pc);
            self.jump_exit(pcv, k, EXIT_OK);
            self.b.switch_to_block(refill);
            let idx = self.b.ins().ushr_imm_s(lo_v, 12 - JTLB_SHIFT);
            let idx = self.b.ins().band_imm_s(idx, 0xfff << JTLB_SHIFT);
            let ent = self.b.ins().iadd(self.cpu, idx);
            let delta = self
                .b
                .ins()
                .load(types::I64, Self::flags(), ent, OFS_JTLB + JTLB_DELTA);
            let hb = self.b.ins().iadd(base, delta);
            self.b.ins().jump(cont, &[hb.into()]);

            self.b.switch_to_block(cont);
            self.groups[id].host_base = Some(self.b.block_params(cont)[0]);
        }
        let hb = self.groups[id].host_base?;
        Some(self.b.ins().iadd_imm_s(hb, off))
    }

    /// Translates a region from decode_region. The instructions are
    /// in address order and each basic block starts at a leader.
    fn translate_region(&mut self, instrs: &[BlockInstr]) {
        for bi in instrs {
            if !bi.last {
                continue;
            }
            let target = bi.pc.wrapping_add(bi.instr.imm as i64 as u64);
            let next = bi.pc.wrapping_add(bi.size);
            let succ: &[u64] = match bi.instr.op {
                Op::Jalr => &[],
                Op::Jal => &[target],
                _ => &[target, next],
            };
            for &t in succ {
                if instrs.binary_search_by_key(&t, |b| b.pc).is_ok()
                    && !self.leaders.contains_key(&t)
                {
                    let blk = self.b.create_block();
                    self.leaders.insert(t, blk);
                }
            }
        }
        self.plan_groups(instrs);
        self.b.ins().jump(self.header, &[]);
        // Instructions of the current basic block, or None if the
        // previous instruction ended it.
        let mut k: Option<u64> = None;
        for (i, bi) in instrs.iter().enumerate() {
            if let Some(&blk) = self.leaders.get(&bi.pc) {
                if let Some(n) = k {
                    self.jump_to(bi.pc, n);
                }
                self.b.switch_to_block(blk);
                self.visited.insert(bi.pc);
                k = Some(0);
            }
            let Some(n) = k else {
                // Not reachable by fall-through or a jump.
                continue;
            };
            if self.instr(n, bi) {
                k = None;
                continue;
            }
            let next = bi.pc.wrapping_add(bi.size);
            if instrs.get(i + 1).is_some_and(|b| b.pc == next) {
                k = Some(n + 1);
            } else {
                self.jump_to(next, n + 1);
                k = None;
            }
        }
        if let Some(n) = k {
            let last = instrs.last().unwrap();
            self.jump_to(last.pc.wrapping_add(last.size), n);
        }
        self.finish();
    }

    fn translate(&mut self, instrs: &[BlockInstr]) {
        self.load_regs(instrs);
        if !self.env.optimize {
            self.plan_groups(instrs);
        }
        if self.env.optimize {
            self.translate_region(instrs);
            return;
        }
        self.visited.insert(self.start_vpc);

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
        self.finish();
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

    /// Emits the inline TLB lookup of a size byte access, a load if
    /// write is false, otherwise a store. Continues at slow on a miss,
    /// otherwise returns the host address in a new current block.
    fn tlb_fast(
        &mut self,
        addr: Value,
        size: i64,
        write: bool,
        slow: cranelift_codegen::ir::Block,
    ) -> Value {
        let last = if size > 1 {
            self.b.ins().iadd_imm_s(addr, size - 1)
        } else {
            addr
        };
        self.tlb_fast_range(addr, last, write, slow)
    }

    /// Emits the inline TLB lookup of the accesses to [addr, last].
    fn tlb_fast_range(
        &mut self,
        addr: Value,
        last: Value,
        write: bool,
        slow: cranelift_codegen::ir::Block,
    ) -> Value {
        let f = Self::flags();
        // The JIT TLB is in the CPU state: the entry of the page is at
        // cpu + OFS_JTLB + ((addr >> 12) & 0xfff) << JTLB_SHIFT.
        let idx = self.b.ins().ushr_imm_s(addr, 12 - JTLB_SHIFT);
        let idx = self.b.ins().band_imm_s(idx, 0xfff << JTLB_SHIFT);
        let ent = self.b.ins().iadd(self.cpu, idx);

        // The tag is computed from the last byte of the access and
        // compared with the entry of the first byte, so an access that
        // crosses a page boundary never matches. The data key is loaded
        // at each access rather than kept in a register: it folds into
        // the or as a memory operand.
        let tag = self.b.ins().band_imm_s(last, !0xfff);
        let key = self
            .b
            .ins()
            .load(types::I64, f, self.cpu, OFS_JIT_DATA_KEY);
        let tag = self.b.ins().bor(tag, key);
        let tag_ofs = OFS_JTLB + if write { JTLB_TAG_W } else { JTLB_TAG_R };
        let e_tag = self.b.ins().load(types::I64, f, ent, tag_ofs);
        let ok = self.b.ins().icmp(IntCC::Equal, e_tag, tag);
        let fast = self.b.create_block();
        self.b.ins().brif(ok, fast, &[], slow, &[]);

        self.b.switch_to_block(fast);
        let delta = self
            .b
            .ins()
            .load(types::I64, f, ent, OFS_JTLB + JTLB_DELTA);
        self.b.ins().iadd(addr, delta)
    }

    /// Returns the signature reference, importing the signature with
    /// the parameter count and return on first use.
    fn helper_sig(&mut self, which: u8) -> SigRef {
        let slot = match which {
            0 => &self.load_sig,
            1 => &self.store_sig,
            2 => &self.exec_sig,
            _ => &self.probe_sig,
        };
        if let Some(s) = *slot {
            return s;
        }
        let s = self.b.import_signature(stub_signature(which as usize));
        match which {
            0 => self.load_sig = Some(s),
            1 => self.store_sig = Some(s),
            2 => self.exec_sig = Some(s),
            _ => self.probe_sig = Some(s),
        }
        s
    }

    /// Emits a guest load. Faults exit the block at the instruction.
    fn load(&mut self, k: u64, pc: u64, addr: Value, size: i64, signed: bool) -> Value {
        if let Some(host) = self.group_access(k, pc) {
            let f = MemFlagsData::new();
            return match (size, signed) {
                (1, false) => self.b.ins().uload8(types::I64, f, host, 0),
                (1, true) => self.b.ins().sload8(types::I64, f, host, 0),
                (2, false) => self.b.ins().uload16(types::I64, f, host, 0),
                (2, true) => self.b.ins().sload16(types::I64, f, host, 0),
                (4, false) => self.b.ins().uload32(f, host, 0),
                (4, true) => self.b.ins().sload32(f, host, 0),
                _ => self.b.ins().load(types::I64, f, host, 0),
            };
        }
        let slow = self.b.create_block();
        let done = self.b.create_block();
        self.b.append_block_param(done, types::I64);
        self.b.set_cold_block(slow);

        // Fast path: direct RAM access.
        let host = self.tlb_fast(addr, size, false, slow);
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
        let sigref = self.helper_sig(0);
        let callee = self.sym(Sym::Load);
        let sz = self.iconst(size);
        self.b
            .ins()
            .call_indirect(sigref, callee, &[self.cpu, addr, sz]);
        let raw = self
            .b
            .ins()
            .load(types::I64, Self::flags(), self.cpu, OFS_VALUE);
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
        self.check_fault(k, pc, done, &[v]);

        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// Emits a guest store.
    fn store(&mut self, k: u64, pc: u64, addr: Value, size: i64, v: Value) {
        if let Some(host) = self.group_access(k, pc) {
            let f = MemFlagsData::new();
            match size {
                1 => self.b.ins().istore8(f, v, host, 0),
                2 => self.b.ins().istore16(f, v, host, 0),
                4 => self.b.ins().istore32(f, v, host, 0),
                _ => self.b.ins().store(f, v, host, 0),
            };
            // The stores of a group run together, so the first one
            // clears the reservation for all.
            let id = self.group_of[&pc].0;
            if !self.groups[id].resv_cleared {
                self.groups[id].resv_cleared = true;
                let zero = self.b.ins().iconst(types::I8, 0);
                self.b.ins().store(Self::flags(), zero, self.cpu, OFS_RESV);
            }
            return;
        }
        let done = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        if self.env.inline_stores {
            // Pages with compiled code have no direct write permission,
            // so stores to them take the slow path, which invalidates
            // the code.
            let host = self.tlb_fast(addr, size, true, slow);
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
        let sigref = self.helper_sig(1);
        let callee = self.sym(Sym::Store);
        let sz = self.iconst(size);
        self.b
            .ins()
            .call_indirect(sigref, callee, &[self.cpu, addr, sz, v]);
        self.check_fault(k, pc, done, &[]);

        self.b.switch_to_block(done);
    }

    /// Returns the host address of the access at pc if it is in a
    /// checked group of stack accesses.
    fn group_access(&mut self, k: u64, pc: u64) -> Option<Value> {
        if !self.group_of.contains_key(&pc) {
            return None;
        }
        let base = self.get(SP);
        self.group_host(k, pc, base)
    }

    /// Exits the block with a fault if the last helper call faulted;
    /// otherwise continues at cont with the arguments.
    fn check_fault(&mut self, k: u64, pc: u64, cont: cranelift_codegen::ir::Block, args: &[Value]) {
        let fault = self
            .b
            .ins()
            .uload8(types::I32, Self::flags(), self.cpu, OFS_FAULT);
        let pcv = self.pc_value(pc);
        let c = self.total(k + 1);
        let ret = self.b.ins().bor_imm_s(c, (EXIT_FAULT << 32) as i64);
        let exit = self.exit_block();
        let args: Vec<_> = args.iter().map(|&v| v.into()).collect();
        self.b
            .ins()
            .brif(fault, exit, &[pcv.into(), ret.into()], cont, &args);
    }

    /// Continues at cont after the helper instruction bi unless it
    /// faulted or requested a stop. A stop exits after the instruction.
    fn check_stop(&mut self, k: u64, bi: &BlockInstr, cont: cranelift_codegen::ir::Block) {
        let fault = self
            .b
            .ins()
            .uload8(types::I32, Self::flags(), self.cpu, OFS_FAULT);
        let slow = self.b.create_block();
        let stop = self.b.create_block();
        let fail = self.b.create_block();
        self.b.set_cold_block(slow);
        self.b.set_cold_block(stop);
        self.b.set_cold_block(fail);
        self.b.ins().brif(fault, slow, &[], cont, &[]);
        self.b.switch_to_block(slow);
        let is_stop = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, fault, FAULT_STOP as i64);
        self.b.ins().brif(is_stop, stop, &[], fail, &[]);
        self.b.switch_to_block(stop);
        let zero = self.b.ins().iconst(types::I8, 0);
        self.b.ins().store(Self::flags(), zero, self.cpu, OFS_FAULT);
        let next = self.pc_value(bi.pc.wrapping_add(bi.size));
        self.jump_exit(next, k + 1, EXIT_OK);
        self.b.switch_to_block(fail);
        let pcv = self.pc_value(bi.pc);
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
        let pcv = self.pc_value(pc);
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
        self.b.ins().store(Self::flags(), ms, self.cpu, OFS_MSTATUS);
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
    fn helper(&mut self, k: u64, bi: &BlockInstr, stop: bool) {
        let sigref = self.helper_sig(2);
        let callee = self.sym(Sym::Exec);
        let packed = self.iconst(pack_instr(&bi.instr) as i64);
        let raw = self.iconst(bi.raw as i64);
        self.b
            .ins()
            .call_indirect(sigref, callee, &[self.cpu, packed, raw]);
        let cont = self.b.create_block();
        if stop {
            self.check_stop(k, bi, cont);
        } else {
            self.check_fault(k, bi.pc, cont, &[]);
        }
        self.b.switch_to_block(cont);
        let rd = bi.instr.rd;
        if rd != 0 {
            if let Some(var) = self.vars[rd as usize] {
                let v =
                    self.b
                        .ins()
                        .load(types::I64, Self::flags(), self.cpu, OFS_X + 8 * rd as i32);
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
            self.helper(k, bi, false);
            return false;
        }
        match csr_kind(&i) {
            Some(CsrKind::Helper) => {
                self.helper(k, bi, true);
                return false;
            }
            Some(CsrKind::Counter) => {
                // The dispatcher adds the instruction count to time
                // and instret when the block returns. The interpreter
                // increments both before executing an instruction, and
                // instret reads exclude the current instruction.
                let csr = (i.imm as u32 & 0xfff) as u16;
                let (ofs, n) = if csr == isa::CSR_INSTRET {
                    (OFS_INSTRET, k)
                } else {
                    (OFS_TIME, k + 1)
                };
                let base = ins!().load(types::I64, Self::flags(), self.cpu, ofs);
                let c = self.total(n);
                let v = ins!().iadd(base, c);
                self.set(rd, v);
                return false;
            }
            None => {}
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
                let link = self.pc_value(next);
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
                let link = self.pc_value(next);
                self.set(rd, link);
                self.chain_exit(target, k + 1);
                return true;
            }
            Lui => self.iconst(imm),
            Auipc => self.pc_value(pc.wrapping_add(imm as u64)),
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
        self.jit_addrs = self.jit_addresses();
        Ok(())
    }

    /// Returns the host addresses the generated code refers to.
    fn jit_addresses(&self) -> [u64; NUM_SYMS] {
        let mut syms = [0; NUM_SYMS];
        syms[Sym::Tlb as usize] = self.tlb.as_ptr() as u64;
        let stubs = self.jit.as_ref().map_or([0; 4], |j| j.stubs);
        syms[Sym::Probe as usize] = stubs[3];
        syms[Sym::Load as usize] = stubs[0];
        syms[Sym::Store as usize] = stubs[1];
        syms[Sym::Exec as usize] = stubs[2];
        syms[Sym::JCache as usize] = self.jit.as_ref().map_or(0, |j| j.jcache.as_ptr() as u64);
        syms
    }

    /// Returns the JIT statistics if the JIT is enabled.
    pub fn jit_stats(&self) -> Option<JitStats> {
        self.jit.as_ref().map(|j| j.stats)
    }

    /// Handles a write to compiled code: the code is revalidated
    /// before it runs again. CPU stores take effect immediately; other
    /// writes, such as device DMA, at the latest at the next fence.i.
    #[inline(never)]
    pub(super) fn jit_code_written(&mut self) {
        if self.mem.take_code_dirty() {
            if let Some(jit) = self.jit.as_mut() {
                jit.new_epoch();
            }
            self.new_jit_gen();
        }
    }

    /// Removes the jump cache entries of the virtual page of vaddr
    /// after its translation was flushed.
    pub(super) fn jit_flush_page(&mut self, vaddr: u64) {
        let Some(jit) = self.jit.as_mut() else {
            return;
        };
        let page = (vaddr >> 12) & 0xfff;
        if jit.jcache_pages[(page >> 6) as usize] & (1 << (page & 63)) == 0 {
            return;
        }
        // The entries of a page are at consecutive indices.
        let vpage = vaddr >> 12;
        let first = ((vaddr & !0xfff) >> 1) as usize & (JCACHE_SIZE - 1);
        for i in 0..(PAGE_SIZE as usize / 2).min(JCACHE_SIZE) {
            let e = &mut jit.jcache[(first + i) & (JCACHE_SIZE - 1)];
            if e.vpc >> 12 == vpage {
                e.vpc = u64::MAX;
            }
        }
    }

    /// Forgets the pages of the jump cache entries, which are all
    /// invalid after a generation change.
    pub(super) fn jit_clear_jcache_pages(&mut self) {
        if let Some(jit) = self.jit.as_mut() {
            jit.jcache_pages = [0; 64];
        }
    }

    /// Handles fence.i: starts a new JIT code epoch if compiled code
    /// was modified.
    pub(super) fn jit_fence_i(&mut self) {
        self.jit_code_written();
    }

    /// Runs the compiled block at PC if there is one. Returns Ok(true)
    /// if a block was executed.
    pub(super) fn try_jit(&mut self) -> Result<bool, Exception> {
        let pc = self.pc;
        // Fast path: the jump cache has the blocks that ran with the
        // current translations and code.
        if let Some(jit) = self.jit.as_ref() {
            let e = &jit.jcache[((pc >> 1) as usize) & (JCACHE_SIZE - 1)];
            if e.vpc == pc && e.key == self.jit_jkey {
                let func = e.func;
                return self.jit_enter(func);
            }
        }
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
                let jit = self.jit.as_mut().unwrap();
                jit.install_pending(code_pages!(self))
                    .map_err(Exception::fatal)?;
            }
        }
        let jit = self.jit.as_mut().unwrap();
        let func = match jit.lookup(&self.mem, code_pages!(self), pc, phys) {
            Some(f) => f,
            None => {
                if jit.pending.contains(&(pc, phys)) {
                    return Ok(false);
                }
                let visits = jit.bump(phys);
                let reused = if visits == 1 || visits >= jit.hot_threshold {
                    jit.reuse(&self.mem, code_pages!(self), pc, phys)
                } else {
                    None
                };
                if let Some(f) = reused {
                    f
                } else {
                    if visits < jit.hot_threshold {
                        return Ok(false);
                    }
                    jit.hot[slot(phys)] = 0;
                    match self.jit_compile(pc, phys)? {
                        Some(f) => f,
                        None => return Ok(false),
                    }
                }
            }
        };
        self.jit_run(func)
    }

    /// Compiles the block at pc / phys, or queues it for the
    /// background compiler. Returns None if the block is not ready.
    fn jit_compile(&mut self, pc: u64, phys: u64) -> Result<Option<BlockFn>, Exception> {
        let epoch = self.jit.as_ref().unwrap().epoch;
        code_pages!(self).mark(phys);
        let Some(req) = Jit::request(self, pc, phys, epoch) else {
            return Ok(None);
        };
        let jit = self.jit.as_mut().unwrap();
        if let Some(worker) = jit.worker.as_ref() {
            worker
                .tx
                .send(req)
                .map_err(|_| Exception::fatal("JIT worker exited"))?;
            jit.pending.insert((pc, phys));
            return Ok(None);
        }
        let compiled = jit
            .compiler
            .as_mut()
            .unwrap()
            .compile(req)
            .map_err(Exception::fatal)?;
        code_pages!(self).mark(compiled.request.code_ofs);
        jit.install(compiled).map(Some).map_err(Exception::fatal)
    }

    /// Runs the compiled block. Returns Ok(true) if the block executed
    /// instructions.
    fn jit_run(&mut self, func: BlockFn) -> Result<bool, Exception> {
        let jit = self.jit.as_mut().unwrap();
        // Blocks that end at this PC can jump directly to the block
        // from now on.
        let pc = self.pc;
        jit.jcache[((pc >> 1) as usize) & (JCACHE_SIZE - 1)] = JEntry {
            vpc: pc,
            key: self.jit_jkey,
            func,
        };
        let page = (pc >> 12) & 0xfff;
        jit.jcache_pages[(page >> 6) as usize] |= 1 << (page & 63);
        self.jit_enter(func)
    }

    /// Calls the block.
    #[inline(always)]
    fn jit_enter(&mut self, func: BlockFn) -> Result<bool, Exception> {
        let enter = self.jit.as_ref().unwrap().enter;
        // SAFETY: the block was compiled for this CPU's memory and TLB
        // layout and accesses the CPU only through the pointer.
        let r = unsafe { enter(self as *mut Cpu, func) };
        let count = r & 0xffff_ffff;
        self.time = self.time.wrapping_add(count);
        self.instret = self.instret.wrapping_add(count);
        if let Some(jit) = self.jit.as_mut() {
            jit.stats.dispatches += 1;
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

/// Code cache file format version.
const CACHE_VERSION: u32 = 7;
const CACHE_MAGIC: &[u8; 8] = b"RSEMUJIT";
/// Blocks loaded from the cache but not used in this many runs are
/// dropped from the cache.
const CACHE_MAX_AGE: u8 = 4;

/// Returns the fingerprint of the code generator: the generated code
/// depends on the emulator binary (structure layouts, helper
/// functions), the host CPU features, and the code generation
/// options.
fn cache_fingerprint(env: CodegenEnv) -> Result<u64, String> {
    static FINGERPRINT: std::sync::OnceLock<Result<u64, String>> = std::sync::OnceLock::new();
    let fp = FINGERPRINT.get_or_init(host_fingerprint).clone()?;
    let mut h = FxHasher(fp);
    h.write_u64(env.inline_stores as u64);
    Ok(h.finish())
}

/// Returns the fingerprint of the emulator binary and the host CPU.
/// The binary is identified by its file identity and modification
/// time, which change when it is rebuilt, so that starting does not
/// need to read it.
fn host_fingerprint() -> Result<u64, String> {
    use std::os::unix::fs::MetadataExt;
    let exe = std::fs::metadata("/proc/self/exe")
        .or_else(|_| std::env::current_exe().and_then(std::fs::metadata))
        .map_err(|e| format!("failed to read the emulator binary: {}", e))?;
    let isa = host_isa()?;
    let mut h = FxHasher::default();
    h.write_u64(CACHE_VERSION as u64);
    for v in [exe.dev(), exe.ino(), exe.size()] {
        h.write_u64(v);
    }
    h.write_i64(exe.mtime());
    h.write_i64(exe.mtime_nsec());
    h.write(isa.flags().to_string().as_bytes());
    for v in isa.isa_flags() {
        h.write(v.to_string().as_bytes());
    }
    Ok(h.finish())
}

/// Little-endian reader for the cache file.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.data.len() {
            return Err("truncated file".to_string());
        }
        let (a, b) = self.data.split_at(n);
        self.data = b;
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    fn vec(&mut self) -> Result<&'a [u8], String> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }
}

/// A read-only executable mapping of a code cache file. Blocks loaded
/// from the file run in place.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(file: &std::fs::File) -> Result<Mapping, String> {
        use std::os::fd::AsRawFd;
        let len = file.metadata().map_err(|e| e.to_string())?.len() as usize;
        if len == 0 {
            return Err("truncated file".to_string());
        }
        // SAFETY: maps the file privately; the code cache is replaced
        // by renaming a new file, never written in place.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_EXEC,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(Mapping {
            ptr: ptr as *mut u8,
            len,
        })
    }

    fn data(&self) -> &[u8] {
        // SAFETY: the mapping is readable and len bytes long.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping created in new().
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

/// Alignment of the machine code in the code cache file.
const CACHE_CODE_ALIGN: usize = 16;

// Code cache file format, little endian:
//
//   magic, version u32, fingerprint u64, block count u32,
//   code area offset u64 (page aligned)
//   per block: vpc u64, start u64, phys u64, instrs u32, age u8,
//              guest code length u32 and bytes,
//              machine code offset u64 (in the code area) and length u32
//   code area: the machine code of the blocks

impl Jit {
    /// Loads the blocks of the code cache file. The file is mapped
    /// and the blocks run in place. They are validated against the
    /// guest code before they first run.
    fn load_cache(&mut self, path: &Path, fingerprint: u64, mem_size: u64) -> Result<(), String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let map = Mapping::new(&file)?;
        let data = map.data();
        let mut r = Reader { data };
        if r.bytes(8)? != CACHE_MAGIC || r.u32()? != CACHE_VERSION {
            return Err("not a JIT code cache".to_string());
        }
        if r.u64()? != fingerprint {
            return Err("built by a different emulator binary or host CPU, ignoring".to_string());
        }
        let count = r.u32()?;
        let code_area = r.u64()? as usize;
        if code_area > data.len() || !code_area.is_multiple_of(PAGE_SIZE as usize) {
            return Err("invalid code area".to_string());
        }
        let code_len = data.len() - code_area;
        let mut blocks = Vec::with_capacity(count.min(1 << 20) as usize);
        for _ in 0..count {
            let vpc = r.u64()?;
            let start = r.u64()?;
            let code_ofs = r.u64()?;
            let instrs = r.u32()?;
            let age = r.u8()?;
            let code = r.vec()?.to_vec();
            let mc_ofs = r.u64()? as usize;
            let mc_len = r.u32()? as usize;
            if mc_ofs.checked_add(mc_len).is_none_or(|end| end > code_len) {
                return Err("invalid machine code range".to_string());
            }
            blocks.push((vpc, start, code_ofs, instrs, age, code, mc_ofs, mc_len));
        }
        for (vpc, start, code_ofs, instrs, age, code, mc_ofs, mc_len) in blocks {
            // Skip blocks outside the guest RAM.
            if start >= mem_size || code_ofs.saturating_add(code.len() as u64) > mem_size {
                continue;
            }
            let block = Block {
                func: std::ptr::null(),
                epoch: UNVALIDATED,
                phys: code_ofs,
                code,
                vpc,
                start,
                instrs,
                size: 0,
                age,
                used: false,
            };
            // SAFETY: the range is inside the mapping.
            let entry = unsafe { map.ptr.add(code_area + mc_ofs) };
            self.add_code(block, entry, mc_len);
            self.stats.loaded += 1;
        }
        self.mapped.push(map);
        self.flush_perf_map();
        Ok(())
    }

    /// Writes the valid blocks to the code cache file. Returns the
    /// number of saved blocks and the file size.
    fn save_cache(&mut self, path: &Path, fingerprint: u64) -> Result<(usize, usize), String> {
        // Blocks used in this run first, so that they win over unused
        // blocks for the same code.
        let mut blocks: Vec<(u8, u32)> = self
            .index
            .values()
            .map(|&idx| {
                let b = &self.blocks[idx as usize];
                let age = if b.used { 0 } else { b.age.saturating_add(1) };
                (age, idx)
            })
            .filter(|&(age, _)| age <= CACHE_MAX_AGE)
            .collect();
        blocks.sort_unstable();
        let mut seen = HashSet::new();
        let mut meta = Vec::new();
        let mut code = Vec::new();
        let mut count = 0u32;
        for (age, idx) in blocks {
            let b = &self.blocks[idx as usize];
            // Code at several addresses is saved once.
            if !seen.insert((b.code_key(), b.phys & (PAGE_SIZE - 1), &b.code)) {
                continue;
            }
            // SAFETY: the block's machine code is in the arena or in a
            // mapped cache file.
            let machine_code = unsafe { std::slice::from_raw_parts(b.func, b.size as usize) };
            code.resize(code.len().next_multiple_of(CACHE_CODE_ALIGN), 0);
            meta.extend_from_slice(&b.vpc.to_le_bytes());
            meta.extend_from_slice(&b.start.to_le_bytes());
            meta.extend_from_slice(&b.phys.to_le_bytes());
            meta.extend_from_slice(&b.instrs.to_le_bytes());
            meta.push(age);
            meta.extend_from_slice(&(b.code.len() as u32).to_le_bytes());
            meta.extend_from_slice(&b.code);
            meta.extend_from_slice(&(code.len() as u64).to_le_bytes());
            meta.extend_from_slice(&(machine_code.len() as u32).to_le_bytes());
            code.extend_from_slice(machine_code);
            count += 1;
        }
        let mut out = Vec::with_capacity(meta.len() + code.len() + 2 * PAGE_SIZE as usize);
        out.extend_from_slice(CACHE_MAGIC);
        out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        out.extend_from_slice(&fingerprint.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        let header = out.len() + 8;
        let code_area = (header + meta.len()).next_multiple_of(PAGE_SIZE as usize);
        out.extend_from_slice(&(code_area as u64).to_le_bytes());
        out.extend_from_slice(&meta);
        out.resize(code_area, 0);
        out.extend_from_slice(&code);

        // Write atomically so that a concurrent run never sees a
        // partial file, and the files that runs have mapped stay
        // unchanged.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, &out).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
        Ok((count as usize, out.len()))
    }

    /// Waits for the background compiler to finish the pending
    /// blocks.
    fn drain_pending(&mut self, code_pages: &mut CodePages) -> Result<(), String> {
        // Do not delay the exit much for a long queue.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.pending.is_empty() {
            let Some(worker) = self.worker.as_ref() else {
                break;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(r) = worker.rx.recv_timeout(left) else {
                break;
            };
            let r = r?;
            self.pending.remove(&(r.request.vpc, r.request.phys));
            code_pages.mark(r.request.code_ofs);
            self.install(r)?;
        }
        Ok(())
    }
}

impl Cpu {
    /// Sets the persistent JIT code cache file. The cache is loaded
    /// when the CPU starts running and saved when it stops.
    pub fn set_jit_cache(&mut self, path: impl Into<PathBuf>) {
        if let Some(jit) = self.jit.as_mut() {
            jit.cache_path = Some(path.into());
        }
    }

    fn jit_env(&self) -> CodegenEnv {
        CodegenEnv {
            inline_stores: self.overlay.is_none(),
            optimize: self.jit.as_ref().is_some_and(|j| j.optimize),
        }
    }

    /// Compiles blocks with Cranelift's optimizations, which makes
    /// compiling several times slower and the code faster. Meant for
    /// building a code cache ahead of time.
    pub fn set_jit_optimize(&mut self, on: bool) {
        if let Some(jit) = self.jit.as_mut() {
            jit.optimize = on;
        }
    }

    /// Loads the JIT code cache if one is configured.
    pub(super) fn jit_cache_load(&mut self) {
        let Some(path) = self.jit.as_ref().and_then(|j| j.cache_path.clone()) else {
            return;
        };
        if !path.exists() {
            return;
        }
        // Never overwrite a file that is not a code cache, for example
        // a disk image given by mistake.
        let mut magic = [0u8; 8];
        let is_cache = std::fs::File::open(&path)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
            .map(|_| &magic == CACHE_MAGIC);
        if !matches!(is_cache, Ok(true)) {
            let jit = self.jit.as_mut().unwrap();
            jit.cache_path = None;
            jit.cache_notes.push(format!(
                "JIT cache {}: not a JIT code cache, not using it",
                path.display()
            ));
            return;
        }
        let mem_size = self.mem.size();
        let start = Instant::now();
        let r = cache_fingerprint(self.jit_env())
            .and_then(|fp| self.jit.as_mut().unwrap().load_cache(&path, fp, mem_size));
        if r.is_ok() {
            let jit = self.jit.as_mut().unwrap();
            let note = format!(
                "JIT cache: loaded {} blocks from {} in {:.0?}",
                jit.stats.loaded,
                path.display(),
                start.elapsed()
            );
            jit.cache_notes.push(note);
        }
        if let Err(e) = r {
            let jit = self.jit.as_mut().unwrap();
            jit.flush();
            jit.stats.loaded = 0;
            jit.cache_notes
                .push(format!("JIT cache {}: {}", path.display(), e));
        }
    }

    /// Saves the JIT code cache if one is configured.
    pub(super) fn jit_cache_save(&mut self) {
        let Some(path) = self.jit.as_ref().and_then(|j| j.cache_path.clone()) else {
            return;
        };
        let env = self.jit_env();
        let jit = self.jit.as_mut().unwrap();
        let r = jit
            .drain_pending(code_pages!(self))
            .and_then(|_| cache_fingerprint(env))
            .and_then(|fp| jit.save_cache(&path, fp));
        jit.cache_notes.push(match r {
            Ok((n, size)) => format!(
                "JIT cache: saved {} blocks ({} KiB) to {}",
                n,
                size.div_ceil(1024),
                path.display()
            ),
            Err(e) => format!("JIT cache {}: save failed: {}", path.display(), e),
        });
    }

    /// Returns the JIT code cache status messages.
    pub fn jit_cache_notes(&self) -> Vec<String> {
        self.jit
            .as_ref()
            .map(|j| j.cache_notes.clone())
            .unwrap_or_default()
    }
}
