//! The virtual RISC-V CPU (hart).

mod csr;
mod debug;
mod fpu;
#[cfg(feature = "jit")]
mod jit;
mod mmu;
pub mod softfloat;
mod symtab;
mod trap;
mod vpu;

pub use debug::fmt_addr;
#[cfg(feature = "jit")]
pub use jit::{JitStats, DEFAULT_HOT_THRESHOLD};
pub use symtab::{SymEntry, SystemMap};
pub use vpu::Vpu;

use crate::isa::{self, reg, Exception, Instr, Mode, Mstatus, Op, RegStatus, Trap};
use crate::memory::{self, Memory, INVALID_PAGENUM};
use crate::mmu::{Mmio, Overlay, Satp, TlbEntry, TLB_SIZE};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Interrupt bits in priority order and their cause numbers.
const INT_NUMS: [u64; 6] = [11, 9, 7, 5, 3, 1];

/// State shared between the hart and the devices running on other
/// threads: the pending interrupts (mip) and the shutdown request.
pub struct HartSignals {
    mip: AtomicU64,
    shutdown: AtomicBool,
    lock: Mutex<()>,
    cond: Condvar,
}

impl Default for HartSignals {
    fn default() -> Self {
        Self::new()
    }
}

impl HartSignals {
    pub fn new() -> HartSignals {
        HartSignals {
            mip: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            lock: Mutex::new(()),
            cond: Condvar::new(),
        }
    }

    /// Returns the pending interrupts.
    #[inline(always)]
    pub fn mip(&self) -> u64 {
        self.mip.load(Ordering::Relaxed)
    }

    /// Sets the pending interrupts register.
    pub fn set_mip(&self, v: u64) {
        self.mip.store(v, Ordering::SeqCst);
        self.wake();
    }

    /// Sets the specified interrupts pending and wakes the hart if
    /// it is waiting for interrupts.
    pub fn set_interrupt(&self, mask: u64) {
        let old = self.mip.fetch_or(mask, Ordering::SeqCst);
        if old & mask != mask {
            self.wake();
        }
    }

    /// Clears the specified pending interrupts.
    pub fn clear_interrupt(&self, mask: u64) {
        self.mip.fetch_and(!mask, Ordering::SeqCst);
    }

    /// Requests hart shutdown.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Tests if shutdown has been requested.
    #[inline(always)]
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    fn wake(&self) {
        let _guard = self.lock.lock().unwrap();
        self.cond.notify_all();
    }
}

/// Enables the JIT compiler for the CPU if the parameters request it.
pub fn configure_jit(cpu: &mut Cpu, params: &crate::Params) -> Result<(), String> {
    if !params.jit {
        return Ok(());
    }
    #[cfg(feature = "jit")]
    {
        let threshold = if params.jit_threshold == 0 {
            DEFAULT_HOT_THRESHOLD
        } else {
            params.jit_threshold
        };
        cpu.enable_jit(threshold, true)
    }
    #[cfg(not(feature = "jit"))]
    {
        let _ = cpu;
        Err("JIT support not compiled in (enable the `jit` feature)".to_string())
    }
}

/// Trap handler callback. The handler returns Ok(true) if it handled
/// the trap, in which case execution continues from the current PC.
pub type TrapHandler = Box<dyn FnMut(&mut Cpu, &Trap) -> Result<bool, String> + Send>;

#[derive(Clone, Copy, Debug, Default)]
struct DecodeEntry {
    raw: u32,
    instr: Instr,
}

const DECODE_CACHE_SIZE: usize = 16384;

/// The RISC-V hart.
pub struct Cpu {
    /// Integer registers.
    pub x: [u64; 32],
    /// Floating point registers (NaN-boxed).
    pub f: [u64; 32],
    /// Program counter.
    pub pc: u64,

    mode: Mode,
    mstatus: Mstatus,

    /// Control and status registers. The mip register is kept in the
    /// shared signals.
    csr: Box<[u64; 4096]>,

    /// Floating point accrued exceptions.
    fflags: u32,
    /// Floating point dynamic rounding mode.
    frm: u32,

    pub vpu: Vpu,

    /// A extension reservation.
    pub reservation: u64,
    pub reservation_valid: bool,

    /// Monotonic CPU time incremented on every instruction.
    pub time: u64,
    /// Instructions retired.
    pub instret: u64,

    pub start_time: Option<Instant>,
    pub runtime: Duration,

    /// Shared interrupt and shutdown state.
    pub signals: Arc<HartSignals>,

    /// Guest RAM.
    pub mem: Arc<Memory>,
    /// Memory-mapped I/O bus.
    pub bus: Option<Box<dyn Mmio>>,
    /// Store overlay (HTIF).
    pub overlay: Option<Box<dyn Overlay>>,

    // MMU state.
    satp: Satp,
    tlb: Box<[TlbEntry; TLB_SIZE]>,
    /// Effective privilege mode for data accesses (MPRV).
    data_mode: Mode,
    data_key: u8,
    fetch_key: u8,

    // Instruction fetch caches.
    code_vpn: u64,
    code_ofs: u64,
    decode_cache: Box<[DecodeEntry; DECODE_CACHE_SIZE]>,

    pub trap_handler: Option<TrapHandler>,

    /// Instruction trace for traps and privilege transitions.
    pub trace: bool,
    /// Instruction level debug trace (CSR 0x800).
    pub debug_trace: bool,
    pub symtab: Option<SystemMap>,
    pub color: bool,
    last_symbol: Option<usize>,
    reg_trace: Option<Box<debug::RegTrace>>,

    /// CPU time of the next interrupt check.
    next_irq_check: u64,
    /// The CPU stops with an error when instret reaches the limit.
    pub instret_limit: u64,

    #[cfg(feature = "jit")]
    jit: Option<Box<jit::Jit>>,
    /// Exception raised by a memory access of JIT compiled code.
    #[cfg(feature = "jit")]
    jit_exception: Option<Exception>,
    /// Set when jit_exception holds an exception.
    #[cfg(feature = "jit")]
    jit_fault: u8,
    /// Bitmap of RAM pages containing compiled code. Empty when the
    /// JIT is disabled.
    #[cfg(feature = "jit")]
    jit_code_pages: Vec<u64>,
}

/// The kind of result of executing one instruction.
enum Flow {
    /// Advance PC by the instruction size.
    Next,
    /// PC was set by the instruction.
    Jump,
}

impl Cpu {
    /// Creates a new CPU with the memory.
    pub fn new(mem: Arc<Memory>) -> Cpu {
        isa::init_decode_tables();
        let mut mstatus = Mstatus(2 << isa::ms::UXL);
        mstatus.set_fs(RegStatus::Initial);
        let mut cpu = Cpu {
            x: [0; 32],
            f: [0; 32],
            pc: 0,
            mode: Mode::M,
            mstatus,
            csr: vec![0u64; 4096].into_boxed_slice().try_into().unwrap(),
            fflags: 0,
            frm: 0,
            vpu: Vpu::new(),
            reservation: 0,
            reservation_valid: false,
            time: 0,
            instret: 0,
            start_time: None,
            runtime: Duration::ZERO,
            signals: Arc::new(HartSignals::new()),
            mem,
            bus: None,
            overlay: None,
            satp: Satp(0),
            tlb: vec![TlbEntry::default(); TLB_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap(),
            data_mode: Mode::M,
            data_key: 0,
            fetch_key: 0,
            code_vpn: INVALID_PAGENUM,
            code_ofs: 0,
            decode_cache: vec![DecodeEntry::default(); DECODE_CACHE_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap(),
            trap_handler: None,
            trace: false,
            debug_trace: false,
            symtab: None,
            color: false,
            last_symbol: None,
            reg_trace: None,
            next_irq_check: 0,
            instret_limit: u64::MAX,
            #[cfg(feature = "jit")]
            jit: None,
            #[cfg(feature = "jit")]
            jit_exception: None,
            #[cfg(feature = "jit")]
            jit_fault: 0,
            #[cfg(feature = "jit")]
            jit_code_pages: Vec::new(),
        };
        cpu.sync_mmu_state();
        cpu
    }

    /// Returns the current privilege mode.
    #[inline(always)]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Sets the current privilege mode.
    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.code_vpn = INVALID_PAGENUM;
            self.sync_mmu_state();
        }
    }

    /// Returns the machine status register.
    #[inline(always)]
    pub fn mstatus(&self) -> Mstatus {
        self.mstatus
    }

    /// Sets the machine status register.
    pub fn set_mstatus(&mut self, v: Mstatus) {
        self.mstatus = v;
        self.sync_mmu_state();
    }

    /// Returns the monotonically increasing CPU time.
    #[inline(always)]
    pub fn now(&self) -> u64 {
        self.time
    }

    /// Requests CPU shutdown.
    pub fn shutdown(&self) {
        self.signals.shutdown();
    }

    /// Sets the specified interrupts pending.
    pub fn set_interrupt(&self, mask: u64) {
        self.signals.set_interrupt(mask);
    }

    /// Clears the specified pending interrupts.
    pub fn clear_interrupt(&self, mask: u64) {
        self.signals.clear_interrupt(mask);
    }

    /// Returns the JIT statistics if the JIT is enabled.
    pub fn jit_summary(&self) -> Option<String> {
        #[cfg(feature = "jit")]
        {
            self.jit_stats().map(|s| s.to_string())
        }
        #[cfg(not(feature = "jit"))]
        {
            None
        }
    }

    /// Raw CSR value without side effects.
    pub fn csr_raw(&self, csr: u16) -> u64 {
        if csr == isa::CSR_MIP {
            self.signals.mip()
        } else {
            self.csr[csr as usize & 0xfff]
        }
    }

    /// Sets the raw CSR value without side effects.
    pub fn set_csr_raw(&mut self, csr: u16, v: u64) {
        if csr == isa::CSR_MIP {
            self.signals.set_mip(v)
        } else {
            self.csr[csr as usize & 0xfff] = v
        }
    }

    /// Invalidates the cached instruction fetch translation.
    pub fn invalidate_code_cache(&mut self) {
        self.code_vpn = INVALID_PAGENUM;
    }

    /// Runs the CPU until shutdown.
    pub fn run(&mut self) -> Result<(), String> {
        let start = Instant::now();
        self.start_time = Some(start);

        let result = loop {
            if self.signals.is_shutdown() {
                break Ok(());
            }
            match self.run_loop() {
                Ok(()) => {}
                Err(Exception::Trap { cause, tval }) => {
                    self.code_vpn = INVALID_PAGENUM;
                    if let Err(e) = self.raise(cause, tval) {
                        break Err(e);
                    }
                }
                Err(Exception::Fatal(msg)) => {
                    break Err(format!("CPU: panic: {}", *msg));
                }
            }
        };
        self.runtime = start.elapsed();

        // Halt all memory-mapped devices.
        if let Some(bus) = self.bus.as_mut() {
            bus.halt()?;
        }
        result
    }

    /// Checks pending interrupts and takes the highest priority
    /// enabled one. Returns true if an interrupt was taken.
    #[inline(never)]
    fn check_interrupts(&mut self) -> bool {
        let mut mip = self.signals.mip();
        if self.time >= self.csr[isa::CSR_STIMECMP as usize] && mip & isa::INT_STIP == 0 {
            mip = self.signals.mip.fetch_or(isa::INT_STIP, Ordering::Relaxed) | isa::INT_STIP;
        }

        let pending = mip & self.csr[isa::CSR_MIE as usize];
        if pending == 0 {
            return false;
        }
        let mideleg = self.csr[isa::CSR_MIDELEG as usize];
        let mode = self.mode;
        for num in INT_NUMS {
            let bit = 1u64 << num;
            if pending & bit == 0 {
                continue;
            }
            if mideleg & bit == 0 {
                if mode < Mode::M || self.mstatus.mie() {
                    self.interrupt(Mode::M, num);
                    return true;
                }
            } else if mode < Mode::S || (mode == Mode::S && self.mstatus.sie()) {
                self.interrupt(Mode::S, num);
                return true;
            }
        }
        false
    }

    /// Fetches the instruction at PC. Returns the raw instruction.
    #[inline(always)]
    fn fetch(&mut self) -> Result<u32, Exception> {
        let pc = self.pc;
        if memory::page(pc) != self.code_vpn {
            self.load_code_page(pc)?;
        }
        let ofs = self.code_ofs + memory::page_offset(pc);
        let p = self.mem.ptr_at(ofs);
        // SAFETY: code_ofs is the RAM offset of the code page, so
        // reads within the page are inside RAM.
        let lo = unsafe { (p as *const u16).read_unaligned() } as u32;
        if lo & 0b11 != 0b11 {
            return Ok(lo);
        }
        if memory::page_offset(pc) <= 0xffc {
            // SAFETY: the instruction is inside the page.
            let hi = unsafe { (p.add(2) as *const u16).read_unaligned() } as u32;
            return Ok(lo | hi << 16);
        }
        // 32-bit instruction crosses page boundary.
        let paddr = self.translate(pc + 2, crate::mmu::ACCESS_EXEC)?;
        let next = self
            .mem
            .page_offset_of(memory::page(paddr))
            .map_err(|_| Exception::trap(isa::CAUSE_INST_ACCESS_FAULT, pc + 2))?;
        let hi = self.mem.read_u16(next + memory::page_offset(paddr)) as u32;
        Ok(lo | hi << 16)
    }

    #[inline(never)]
    fn load_code_page(&mut self, pc: u64) -> Result<(), Exception> {
        let paddr = self.translate(pc, crate::mmu::ACCESS_EXEC)?;
        let ofs = self
            .mem
            .page_offset_of(memory::page(paddr))
            .map_err(|_| Exception::trap(isa::CAUSE_INST_ACCESS_FAULT, paddr))?;
        self.code_ofs = ofs;
        self.code_vpn = memory::page(pc);
        Ok(())
    }

    #[inline(always)]
    fn decode_cached(&mut self, raw: u32) -> Instr {
        let idx = ((self.pc >> 1) as usize) & (DECODE_CACHE_SIZE - 1);
        let entry = &mut self.decode_cache[idx];
        if entry.raw == raw {
            return entry.instr;
        }
        let instr = if raw & 0b11 == 0b11 {
            isa::decode(raw).unwrap_or_default()
        } else {
            isa::decode_c_fast(raw as u16)
        };
        entry.raw = raw;
        entry.instr = instr;
        instr
    }

    /// Clears the decode cache.
    pub fn flush_decode_cache(&mut self) {
        for e in self.decode_cache.iter_mut() {
            *e = DecodeEntry::default();
        }
    }

    /// Executes a single instruction. This is mostly useful for tests
    /// and debuggers; the main loop is `run`.
    pub fn step(&mut self) -> Result<(), String> {
        self.time = self.time.wrapping_add(1);
        if self.time & 0x3f == 0 && self.check_interrupts() {
            return Ok(());
        }
        let r = self.fetch().and_then(|raw| {
            let instr = self.decode_cached(raw);
            self.instret += 1;
            let size = if raw & 0b11 == 0b11 { 4 } else { 2 };
            match self.execute(instr, raw, size)? {
                Flow::Next => self.pc = self.pc.wrapping_add(size),
                Flow::Jump => {}
            }
            Ok(())
        });
        match r {
            Ok(()) => Ok(()),
            Err(Exception::Trap { cause, tval }) => {
                self.code_vpn = INVALID_PAGENUM;
                self.raise(cause, tval)
            }
            Err(Exception::Fatal(msg)) => Err(*msg),
        }
    }

    fn run_loop(&mut self) -> Result<(), Exception> {
        #[cfg(feature = "jit")]
        if self.jit.is_some() && !self.debug_trace {
            return self.run_loop_jit();
        }
        loop {
            if self.debug_trace {
                if self.time >= self.next_irq_check && self.irq_point()? {
                    return Ok(());
                }
                self.interp_one::<true>()?;
                continue;
            }
            // The debug trace is enabled through a CSR; it is checked
            // with the interrupts to keep it out of the instruction
            // loop.
            loop {
                if self.time >= self.next_irq_check {
                    if self.irq_point()? {
                        return Ok(());
                    }
                    if self.debug_trace {
                        break;
                    }
                }
                self.interp_one::<false>()?;
            }
        }
    }

    /// The dispatch loop with the JIT: blocks start at control
    /// transfers and after instructions the JIT does not compile.
    #[cfg(feature = "jit")]
    fn run_loop_jit(&mut self) -> Result<(), Exception> {
        let mut block_start = true;
        loop {
            if self.time >= self.next_irq_check {
                if self.irq_point()? || self.debug_trace {
                    return Ok(());
                }
                block_start = true;
            }
            if block_start && self.try_jit()? {
                continue;
            }
            block_start = match self.interp_one::<false>()? {
                (Flow::Next, op) => !jit::jitable(op),
                (Flow::Jump, _) => true,
            };
        }
    }

    /// Checks interrupts, shutdown, and the instruction limit. Called
    /// every 64 ticks. Returns true if the CPU should stop.
    #[inline(never)]
    fn irq_point(&mut self) -> Result<bool, Exception> {
        self.next_irq_check = (self.time | 0x3f) + 1;
        if self.check_interrupts() {
            return Ok(false);
        }
        if self.signals.is_shutdown() {
            return Ok(true);
        }
        if self.instret >= self.instret_limit {
            return Err(Exception::fatal("instruction limit reached"));
        }
        Ok(false)
    }

    /// Interprets one instruction, with the debug trace if TRACE is
    /// set.
    #[inline(always)]
    fn interp_one<const TRACE: bool>(&mut self) -> Result<(Flow, Op), Exception> {
        // Ensure monotonic clock.
        self.time = self.time.wrapping_add(1);

        let raw = self.fetch()?;
        let instr = self.decode_cached(raw);
        self.instret += 1;

        if TRACE {
            self.trace_instr(raw, &instr);
        }

        let size = if raw & 0b11 == 0b11 { 4 } else { 2 };
        let flow = self.execute(instr, raw, size)?;
        if let Flow::Next = flow {
            self.pc = self.pc.wrapping_add(size);
        }
        Ok((flow, instr.op))
    }

    #[inline(always)]
    fn set_x(&mut self, rd: u8, v: u64) {
        self.x[(rd & 31) as usize] = v;
        self.x[0] = 0;
    }

    #[inline(always)]
    fn rs1(&self, i: &Instr) -> u64 {
        self.x[(i.rs1 & 31) as usize]
    }

    #[inline(always)]
    fn rs2(&self, i: &Instr) -> u64 {
        self.x[(i.rs2 & 31) as usize]
    }

    #[inline(always)]
    fn addr(&self, i: &Instr) -> u64 {
        self.x[(i.rs1 & 31) as usize].wrapping_add(i.imm as i64 as u64)
    }

    fn illegal(&self, raw: u32) -> Exception {
        Exception::trap(isa::CAUSE_ILLEGAL_INSTR, raw as u64)
    }

    #[inline(always)]
    fn branch(&mut self, cond: bool, imm: i32) -> Flow {
        if cond {
            self.pc = self.pc.wrapping_add(imm as i64 as u64);
            Flow::Jump
        } else {
            Flow::Next
        }
    }

    #[inline(always)]
    fn execute(&mut self, i: Instr, raw: u32, size: u64) -> Result<Flow, Exception> {
        use Op::*;
        let rd = i.rd;
        match i.op {
            Invalid => return Err(self.illegal(raw)),

            Add => self.set_x(rd, self.rs1(&i).wrapping_add(self.rs2(&i))),
            Sub => self.set_x(rd, self.rs1(&i).wrapping_sub(self.rs2(&i))),
            Sll => self.set_x(rd, self.rs1(&i) << (self.rs2(&i) & 0x3f)),
            Slt => self.set_x(rd, ((self.rs1(&i) as i64) < (self.rs2(&i) as i64)) as u64),
            Sltu => self.set_x(rd, (self.rs1(&i) < self.rs2(&i)) as u64),
            Xor => self.set_x(rd, self.rs1(&i) ^ self.rs2(&i)),
            Srl => self.set_x(rd, self.rs1(&i) >> (self.rs2(&i) & 0x3f)),
            Sra => self.set_x(rd, ((self.rs1(&i) as i64) >> (self.rs2(&i) & 0x3f)) as u64),
            Or => self.set_x(rd, self.rs1(&i) | self.rs2(&i)),
            And => self.set_x(rd, self.rs1(&i) & self.rs2(&i)),

            Addi => self.set_x(rd, self.rs1(&i).wrapping_add(i.imm as i64 as u64)),
            Slti => self.set_x(rd, ((self.rs1(&i) as i64) < i.imm as i64) as u64),
            Sltiu => self.set_x(rd, (self.rs1(&i) < i.imm as i64 as u64) as u64),
            Xori => self.set_x(rd, self.rs1(&i) ^ i.imm as i64 as u64),
            Ori => self.set_x(rd, self.rs1(&i) | i.imm as i64 as u64),
            Andi => self.set_x(rd, self.rs1(&i) & i.imm as i64 as u64),
            Slli => self.set_x(rd, self.rs1(&i) << (i.imm & 0x3f)),
            Srli => self.set_x(rd, self.rs1(&i) >> (i.imm & 0x3f)),
            Srai => self.set_x(rd, ((self.rs1(&i) as i64) >> (i.imm & 0x3f)) as u64),

            Addw => self.set_x(
                rd,
                (self.rs1(&i) as i32).wrapping_add(self.rs2(&i) as i32) as i64 as u64,
            ),
            Subw => self.set_x(
                rd,
                (self.rs1(&i) as i32).wrapping_sub(self.rs2(&i) as i32) as i64 as u64,
            ),
            Sllw => self.set_x(
                rd,
                ((self.rs1(&i) as u32) << (self.rs2(&i) & 0x1f)) as i32 as i64 as u64,
            ),
            Srlw => self.set_x(
                rd,
                ((self.rs1(&i) as u32) >> (self.rs2(&i) & 0x1f)) as i32 as i64 as u64,
            ),
            Sraw => self.set_x(
                rd,
                ((self.rs1(&i) as i32) >> (self.rs2(&i) & 0x1f)) as i64 as u64,
            ),
            Addiw => self.set_x(rd, (self.rs1(&i) as i32).wrapping_add(i.imm) as i64 as u64),
            Slliw => self.set_x(
                rd,
                ((self.rs1(&i) as u32) << (i.imm & 0x1f)) as i32 as i64 as u64,
            ),
            SlliUw => self.set_x(rd, (self.rs1(&i) & 0xffff_ffff) << (i.imm & 0x3f)),
            Srliw => self.set_x(
                rd,
                ((self.rs1(&i) as u32) >> (i.imm & 0x1f)) as i32 as i64 as u64,
            ),
            Sraiw => self.set_x(rd, ((self.rs1(&i) as i32) >> (i.imm & 0x1f)) as i64 as u64),

            Mul => self.set_x(rd, self.rs1(&i).wrapping_mul(self.rs2(&i))),
            Mulh => {
                let v = ((self.rs1(&i) as i64 as i128) * (self.rs2(&i) as i64 as i128)) >> 64;
                self.set_x(rd, v as u64)
            }
            Mulhsu => {
                let v = ((self.rs1(&i) as i64 as i128).wrapping_mul(self.rs2(&i) as i128)) >> 64;
                self.set_x(rd, v as u64)
            }
            Mulhu => {
                let v = ((self.rs1(&i) as u128) * (self.rs2(&i) as u128)) >> 64;
                self.set_x(rd, v as u64)
            }
            Div => {
                let (a, b) = (self.rs1(&i) as i64, self.rs2(&i) as i64);
                let v = if b == 0 { -1 } else { a.wrapping_div(b) };
                self.set_x(rd, v as u64)
            }
            Divu => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                self.set_x(rd, a.checked_div(b).unwrap_or(u64::MAX))
            }
            Rem => {
                let (a, b) = (self.rs1(&i) as i64, self.rs2(&i) as i64);
                let v = if b == 0 { a } else { a.wrapping_rem(b) };
                self.set_x(rd, v as u64)
            }
            Remu => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                self.set_x(rd, if b == 0 { a } else { a % b })
            }
            Mulw => self.set_x(
                rd,
                (self.rs1(&i) as i32).wrapping_mul(self.rs2(&i) as i32) as i64 as u64,
            ),
            Divw => {
                let (a, b) = (self.rs1(&i) as i32, self.rs2(&i) as i32);
                let v = if b == 0 { -1 } else { a.wrapping_div(b) };
                self.set_x(rd, v as i64 as u64)
            }
            Divuw => {
                let (a, b) = (self.rs1(&i) as u32, self.rs2(&i) as u32);
                let v = a.checked_div(b).unwrap_or(u32::MAX);
                self.set_x(rd, v as i32 as i64 as u64)
            }
            Remw => {
                let (a, b) = (self.rs1(&i) as i32, self.rs2(&i) as i32);
                let v = if b == 0 { a } else { a.wrapping_rem(b) };
                self.set_x(rd, v as i64 as u64)
            }
            Remuw => {
                let (a, b) = (self.rs1(&i) as u32, self.rs2(&i) as u32);
                let v = if b == 0 { a } else { a % b };
                self.set_x(rd, v as i32 as i64 as u64)
            }

            Lb => {
                let v = self.load8(self.addr(&i))?;
                self.set_x(rd, v as i8 as i64 as u64)
            }
            Lh => {
                let v = self.load16(self.addr(&i))?;
                self.set_x(rd, v as i16 as i64 as u64)
            }
            Lw => {
                let v = self.load32(self.addr(&i))?;
                self.set_x(rd, v as i32 as i64 as u64)
            }
            Ld => {
                let v = self.load64(self.addr(&i))?;
                self.set_x(rd, v)
            }
            Lbu => {
                let v = self.load8(self.addr(&i))?;
                self.set_x(rd, v as u64)
            }
            Lhu => {
                let v = self.load16(self.addr(&i))?;
                self.set_x(rd, v as u64)
            }
            Lwu => {
                let v = self.load32(self.addr(&i))?;
                self.set_x(rd, v as u64)
            }

            Sb => {
                self.store8(self.addr(&i), self.rs2(&i) as u8)?;
                self.reservation_valid = false;
            }
            Sh => {
                self.store16(self.addr(&i), self.rs2(&i) as u16)?;
                self.reservation_valid = false;
            }
            Sw => {
                self.store32(self.addr(&i), self.rs2(&i) as u32)?;
                self.reservation_valid = false;
            }
            Sd => {
                self.store64(self.addr(&i), self.rs2(&i))?;
                self.reservation_valid = false;
            }

            Beq => return Ok(self.branch(self.rs1(&i) == self.rs2(&i), i.imm)),
            Bne => return Ok(self.branch(self.rs1(&i) != self.rs2(&i), i.imm)),
            Blt => {
                return Ok(self.branch((self.rs1(&i) as i64) < (self.rs2(&i) as i64), i.imm))
            }
            Bge => {
                return Ok(self.branch((self.rs1(&i) as i64) >= (self.rs2(&i) as i64), i.imm))
            }
            Bltu => return Ok(self.branch(self.rs1(&i) < self.rs2(&i), i.imm)),
            Bgeu => return Ok(self.branch(self.rs1(&i) >= self.rs2(&i), i.imm)),

            Jal => {
                let t = self.pc.wrapping_add(size);
                self.pc = self.pc.wrapping_add(i.imm as i64 as u64);
                self.set_x(rd, t);
                return Ok(Flow::Jump);
            }
            Jalr => {
                let t = self.pc.wrapping_add(size);
                self.pc = self.addr(&i) & !1;
                self.set_x(rd, t);
                return Ok(Flow::Jump);
            }

            Lui => self.set_x(rd, i.imm as i64 as u64),
            Auipc => self.set_x(rd, self.pc.wrapping_add(i.imm as i64 as u64)),

            Ecall => {
                let cause = match self.mode {
                    Mode::U => isa::CAUSE_ECALL_U,
                    Mode::S => isa::CAUSE_ECALL_S,
                    Mode::M => isa::CAUSE_ECALL_M,
                    Mode::H => return Err(self.illegal(raw)),
                };
                if self.trace {
                    self.trace_func(self.pc);
                    self.tracef(
                        raw,
                        &i,
                        &format!(
                            "a7={:x}, a6={:x}, a0={:x}, a1={:x}",
                            self.x[reg::A7 as usize],
                            self.x[reg::A6 as usize],
                            self.x[reg::A0 as usize],
                            self.x[reg::A1 as usize]
                        ),
                    );
                }
                return Err(Exception::trap(cause, 0));
            }
            Ebreak => {
                if self.trace {
                    self.trace_func(self.pc);
                    self.tracef(raw, &i, "");
                }
                return Err(Exception::trap(isa::CAUSE_BREAKPOINT, self.pc));
            }
            Sret => {
                if self.mode < Mode::S || (self.mode == Mode::S && self.mstatus.tsr()) {
                    return Err(self.illegal(raw));
                }
                if self.trace {
                    self.trace_func(self.pc);
                    self.tracef(
                        raw,
                        &i,
                        &format!(
                            "mode={} => {}, sepc={:x}",
                            self.mode,
                            self.mstatus.spp(),
                            self.csr[isa::CSR_SEPC as usize]
                        ),
                    );
                }
                let mut ms = self.mstatus;
                let spp = ms.spp();
                ms.set_sie(ms.spie());
                ms.set_spie(true);
                ms.set_spp(Mode::U);
                if spp != Mode::M {
                    ms.set_mprv(false);
                }
                self.mstatus = ms;
                self.set_mode(spp);
                self.sync_mmu_state();
                self.pc = self.csr[isa::CSR_SEPC as usize];
                self.reservation_valid = false;
                return Ok(Flow::Jump);
            }
            Mret => {
                if self.mode < Mode::M {
                    return Err(self.illegal(raw));
                }
                if self.trace {
                    self.trace_func(self.pc);
                    self.tracef(
                        raw,
                        &i,
                        &format!(
                            "mode={} => {}, mepc={:x}",
                            self.mode,
                            self.mstatus.mpp(),
                            self.csr[isa::CSR_MEPC as usize]
                        ),
                    );
                }
                let mut ms = self.mstatus;
                let mpp = ms.mpp();
                ms.set_mie(ms.mpie());
                ms.set_mpie(true);
                ms.set_mpp(Mode::U);
                if mpp != Mode::M {
                    ms.set_mprv(false);
                }
                self.mstatus = ms;
                self.set_mode(mpp);
                self.sync_mmu_state();
                self.pc = self.csr[isa::CSR_MEPC as usize];
                self.reservation_valid = false;
                return Ok(Flow::Jump);
            }
            Wfi => {
                if self.mode < Mode::M && self.mstatus.tw() {
                    return Err(self.illegal(raw));
                }
                self.wfi();
            }

            Fence | PrefetchI | PrefetchR | PrefetchW | CboInval | CboClean | CboFlush => {}
            FenceI => {
                #[cfg(feature = "jit")]
                self.jit_fence_i();
                self.code_vpn = INVALID_PAGENUM;
                self.flush_decode_cache();
            }
            CboZero => {
                let base = self.rs1(&i) & !63;
                for ofs in (0..64).step_by(8) {
                    self.store64(base + ofs, 0)?;
                }
            }
            SfenceVma => {
                if self.mode < Mode::S || (self.mode == Mode::S && self.mstatus.tvm()) {
                    return Err(self.illegal(raw));
                }
                self.flush_tlb();
                self.code_vpn = INVALID_PAGENUM;
                self.reservation_valid = false;
            }

            Csrrw | Csrrs | Csrrc | Csrrwi | Csrrsi | Csrrci => self.exec_csr(i, raw)?,

            LrW | ScW | AmoaddW | AmoswapW | AmoxorW | AmoandW | AmoorW | AmominW | AmomaxW
            | AmominuW | AmomaxuW | LrD | ScD | AmoaddD | AmoswapD | AmoxorD | AmoandD
            | AmoorD | AmominD | AmomaxD | AmominuD | AmomaxuD => self.exec_amo(i)?,

            Flw | Fld | Fsw | Fsd | FaddS | FaddD | FsubS | FsubD | FmulS | FmulD | FdivS
            | FdivD | FsqrtS | FsqrtD | FmaddS | FmaddD | FmsubS | FmsubD | FnmsubS | FnmsubD
            | FnmaddS | FnmaddD | FsgnjS | FsgnjnS | FsgnjxS | FsgnjD | FsgnjnD | FsgnjxD
            | FminS | FmaxS | FminD | FmaxD | FeqS | FltS | FleS | FeqD | FltD | FleD | FcvtWS
            | FcvtWuS | FcvtLS | FcvtLuS | FcvtWD | FcvtWuD | FcvtLD | FcvtLuD | FcvtSW
            | FcvtSWu | FcvtSL | FcvtSLu | FcvtDW | FcvtDWu | FcvtDL | FcvtDLu | FcvtSD
            | FcvtDS | FmvXW | FmvXD | FmvWX | FmvDX | FclassS | FclassD => self.exec_fp(i, raw)?,

            Vsetvli | Vsetivli | Vsetvl | VandVX | VxorVV | VmsneVV | VidV | VmvVX | VmvVV
            | VmvVI | Vle8V | Vle16V | Vle32V | Vle64V | Vse8V | Vse16V | Vse32V | Vse64V
            | VaddVI | VmseqVI | VsllVI | VsraVI => self.exec_vector(i, raw)?,

            // Zicond.
            CzeroEqz => {
                let v = if self.rs2(&i) == 0 { 0 } else { self.rs1(&i) };
                self.set_x(rd, v)
            }
            CzeroNez => {
                let v = if self.rs2(&i) != 0 { 0 } else { self.rs1(&i) };
                self.set_x(rd, v)
            }

            // Zbs.
            Bclr => self.set_x(rd, self.rs1(&i) & !(1u64 << (self.rs2(&i) & 63))),
            Bclri => self.set_x(rd, self.rs1(&i) & !(1u64 << (i.imm & 63))),
            Bext => self.set_x(rd, (self.rs1(&i) >> (self.rs2(&i) & 63)) & 1),
            Bexti => self.set_x(rd, (self.rs1(&i) >> (i.imm & 63)) & 1),
            Binv => self.set_x(rd, self.rs1(&i) ^ (1u64 << (self.rs2(&i) & 63))),
            Binvi => self.set_x(rd, self.rs1(&i) ^ (1u64 << (i.imm & 63))),
            Bset => self.set_x(rd, self.rs1(&i) | (1u64 << (self.rs2(&i) & 63))),
            Bseti => self.set_x(rd, self.rs1(&i) | (1u64 << (i.imm & 63))),

            // Zba.
            AddUw => self.set_x(rd, (self.rs1(&i) as u32 as u64).wrapping_add(self.rs2(&i))),
            Sh1add => self.set_x(rd, self.rs2(&i).wrapping_add(self.rs1(&i) << 1)),
            Sh2add => self.set_x(rd, self.rs2(&i).wrapping_add(self.rs1(&i) << 2)),
            Sh3add => self.set_x(rd, self.rs2(&i).wrapping_add(self.rs1(&i) << 3)),
            Sh1addUw => self.set_x(
                rd,
                self.rs2(&i).wrapping_add((self.rs1(&i) as u32 as u64) << 1),
            ),
            Sh2addUw => self.set_x(
                rd,
                self.rs2(&i).wrapping_add((self.rs1(&i) as u32 as u64) << 2),
            ),
            Sh3addUw => self.set_x(
                rd,
                self.rs2(&i).wrapping_add((self.rs1(&i) as u32 as u64) << 3),
            ),

            // Zbb.
            Andn => self.set_x(rd, self.rs1(&i) & !self.rs2(&i)),
            Orn => self.set_x(rd, self.rs1(&i) | !self.rs2(&i)),
            Xnor => self.set_x(rd, !(self.rs1(&i) ^ self.rs2(&i))),
            Clz => self.set_x(rd, self.rs1(&i).leading_zeros() as u64),
            Clzw => self.set_x(rd, (self.rs1(&i) as u32).leading_zeros() as u64),
            Ctz => self.set_x(rd, self.rs1(&i).trailing_zeros() as u64),
            Ctzw => self.set_x(rd, (self.rs1(&i) as u32).trailing_zeros() as u64),
            Cpop => self.set_x(rd, self.rs1(&i).count_ones() as u64),
            Cpopw => self.set_x(rd, (self.rs1(&i) as u32).count_ones() as u64),
            Max => self.set_x(
                rd,
                (self.rs1(&i) as i64).max(self.rs2(&i) as i64) as u64,
            ),
            Maxu => self.set_x(rd, self.rs1(&i).max(self.rs2(&i))),
            Min => self.set_x(
                rd,
                (self.rs1(&i) as i64).min(self.rs2(&i) as i64) as u64,
            ),
            Minu => self.set_x(rd, self.rs1(&i).min(self.rs2(&i))),
            SextB => self.set_x(rd, self.rs1(&i) as i8 as i64 as u64),
            SextH => self.set_x(rd, self.rs1(&i) as i16 as i64 as u64),
            ZextH => self.set_x(rd, self.rs1(&i) as u16 as u64),
            Rol => self.set_x(rd, self.rs1(&i).rotate_left((self.rs2(&i) & 63) as u32)),
            Ror => self.set_x(rd, self.rs1(&i).rotate_right((self.rs2(&i) & 63) as u32)),
            Rori => self.set_x(rd, self.rs1(&i).rotate_right((i.imm & 63) as u32)),
            Rolw => self.set_x(
                rd,
                (self.rs1(&i) as u32).rotate_left((self.rs2(&i) & 31) as u32) as i32 as i64 as u64,
            ),
            Rorw => self.set_x(
                rd,
                (self.rs1(&i) as u32).rotate_right((self.rs2(&i) & 31) as u32) as i32 as i64
                    as u64,
            ),
            Roriw => self.set_x(
                rd,
                (self.rs1(&i) as u32).rotate_right((i.imm & 31) as u32) as i32 as i64 as u64,
            ),
            OrcB => {
                let v = self.rs1(&i);
                let mut r = 0u64;
                for b in 0..8 {
                    if (v >> (b * 8)) & 0xff != 0 {
                        r |= 0xff << (b * 8);
                    }
                }
                self.set_x(rd, r)
            }
            Rev8 => self.set_x(rd, self.rs1(&i).swap_bytes()),

            // Zbc.
            Clmul => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                let mut r = 0u64;
                for n in 0..64 {
                    if (b >> n) & 1 != 0 {
                        r ^= a << n;
                    }
                }
                self.set_x(rd, r)
            }
            Clmulh => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                let mut r = 0u64;
                for n in 1..64 {
                    if (b >> n) & 1 != 0 {
                        r ^= a >> (64 - n);
                    }
                }
                self.set_x(rd, r)
            }
            Clmulr => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                let mut r = 0u64;
                for n in 0..64 {
                    if (b >> n) & 1 != 0 {
                        r ^= a >> (63 - n);
                    }
                }
                self.set_x(rd, r)
            }

            // Zbkb and Zbkx.
            Pack => self.set_x(rd, (self.rs1(&i) & 0xffff_ffff) | (self.rs2(&i) << 32)),
            Packh => self.set_x(rd, (self.rs1(&i) & 0xff) | ((self.rs2(&i) & 0xff) << 8)),
            Packw => {
                let v = (self.rs1(&i) & 0xffff) as u32 | ((self.rs2(&i) & 0xffff) as u32) << 16;
                self.set_x(rd, v as i32 as i64 as u64)
            }
            Brev8 => {
                let v = self.rs1(&i);
                let mut r = 0u64;
                for b in 0..8 {
                    let byte = ((v >> (b * 8)) & 0xff) as u8;
                    r |= (byte.reverse_bits() as u64) << (b * 8);
                }
                self.set_x(rd, r)
            }
            Xperm4 => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                let mut r = 0u64;
                for n in (0..64).step_by(4) {
                    let idx = (b >> n) & 0xf;
                    r |= ((a >> (idx * 4)) & 0xf) << n;
                }
                self.set_x(rd, r)
            }
            Xperm8 => {
                let (a, b) = (self.rs1(&i), self.rs2(&i));
                let mut r = 0u64;
                for n in (0..64).step_by(8) {
                    let idx = (b >> n) & 0xff;
                    if idx < 8 {
                        r |= ((a >> (idx * 8)) & 0xff) << n;
                    }
                }
                self.set_x(rd, r)
            }
        }
        Ok(Flow::Next)
    }

    /// Waits for an interrupt or the next supervisor timer deadline.
    fn wfi(&mut self) {
        let stimecmp = self.csr[isa::CSR_STIMECMP as usize];
        let now = self.time;
        if stimecmp == u64::MAX || now >= stimecmp {
            return;
        }
        let mie = self.csr[isa::CSR_MIE as usize];
        let delay = stimecmp - now;
        let deadline = Instant::now() + Duration::from_nanos(delay.saturating_mul(10));

        let signals = self.signals.clone();
        let mut guard = signals.lock.lock().unwrap();
        let mut timeout = false;
        loop {
            if signals.mip() & mie != 0 || signals.is_shutdown() {
                break;
            }
            let t = Instant::now();
            if t >= deadline {
                timeout = true;
                break;
            }
            guard = signals.cond.wait_timeout(guard, deadline - t).unwrap().0;
        }
        drop(guard);
        if timeout {
            self.time += delay;
        }
        if self.time >= self.csr[isa::CSR_STIMECMP as usize] {
            signals.set_interrupt(isa::INT_STIP);
        }
    }

    fn exec_amo(&mut self, i: Instr) -> Result<(), Exception> {
        use Op::*;
        let addr = self.rs1(&i);
        let rs2 = self.rs2(&i);
        let word = matches!(
            i.op,
            LrW | ScW | AmoaddW | AmoswapW | AmoxorW | AmoandW | AmoorW | AmominW | AmomaxW
                | AmominuW | AmomaxuW
        );
        let align = if word { 3 } else { 7 };
        if addr & align != 0 {
            let cause = if matches!(i.op, LrW | LrD) {
                isa::CAUSE_LOAD_ADDR_MISALIGNED
            } else {
                isa::CAUSE_STORE_ADDR_MISALIGNED
            };
            return Err(Exception::trap(cause, addr));
        }

        match i.op {
            LrW => {
                let v = self.load32(addr)?;
                self.set_x(i.rd, v as i32 as i64 as u64);
                self.reservation = addr;
                self.reservation_valid = true;
                return Ok(());
            }
            LrD => {
                let v = self.load64(addr)?;
                self.set_x(i.rd, v);
                self.reservation = addr;
                self.reservation_valid = true;
                return Ok(());
            }
            ScW | ScD => {
                if self.reservation_valid && self.reservation == addr {
                    let r = if i.op == ScW {
                        self.store32(addr, rs2 as u32)
                    } else {
                        self.store64(addr, rs2)
                    };
                    if let Err(e) = r {
                        self.reservation_valid = false;
                        return Err(e);
                    }
                    self.set_x(i.rd, 0);
                } else {
                    self.set_x(i.rd, 1);
                }
                self.reservation_valid = false;
                return Ok(());
            }
            _ => {}
        }

        if word {
            // Check store permission first so that AMOs fault as
            // stores.
            self.translate(addr, crate::mmu::ACCESS_READ | crate::mmu::ACCESS_WRITE)?;
            let v = self.load32(addr)?;
            let b = rs2 as u32;
            let t = match i.op {
                AmoaddW => v.wrapping_add(b),
                AmoswapW => b,
                AmoxorW => v ^ b,
                AmoandW => v & b,
                AmoorW => v | b,
                AmominW => (v as i32).min(b as i32) as u32,
                AmomaxW => (v as i32).max(b as i32) as u32,
                AmominuW => v.min(b),
                AmomaxuW => v.max(b),
                _ => unreachable!(),
            };
            self.store32(addr, t)?;
            self.set_x(i.rd, v as i32 as i64 as u64);
        } else {
            self.translate(addr, crate::mmu::ACCESS_READ | crate::mmu::ACCESS_WRITE)?;
            let v = self.load64(addr)?;
            let t = match i.op {
                AmoaddD => v.wrapping_add(rs2),
                AmoswapD => rs2,
                AmoxorD => v ^ rs2,
                AmoandD => v & rs2,
                AmoorD => v | rs2,
                AmominD => (v as i64).min(rs2 as i64) as u64,
                AmomaxD => (v as i64).max(rs2 as i64) as u64,
                AmominuD => v.min(rs2),
                AmomaxuD => v.max(rs2),
                _ => unreachable!(),
            };
            self.store64(addr, t)?;
            self.set_x(i.rd, v);
        }
        Ok(())
    }

    fn exec_csr(&mut self, i: Instr, raw: u32) -> Result<(), Exception> {
        use Op::*;
        let csr = (i.imm as u32 & 0xfff) as u16;
        let src = match i.op {
            Csrrw | Csrrs | Csrrc => self.rs1(&i),
            _ => i.rs1 as u64,
        };
        // csrrw with rd=x0 does not read the CSR.
        let read = !(matches!(i.op, Csrrw | Csrrwi) && i.rd == 0);
        let write = matches!(i.op, Csrrw | Csrrwi) || i.rs1 != 0;

        let old = if read || write {
            self.csr_load(csr, raw, read)?
        } else {
            0
        };
        if write {
            let v = match i.op {
                Csrrw | Csrrwi => src,
                Csrrs | Csrrsi => old | src,
                _ => old & !src,
            };
            self.csr_store(csr, v, raw)?;
        }
        self.set_x(i.rd, old);
        Ok(())
    }

    /// Marks the floating point state dirty.
    fn set_fs_dirty(&mut self) {
        self.mstatus.set_fs(RegStatus::Dirty);
        self.mstatus.set_sd(true);
    }

    /// Updates the mstatus SD bit from the FS and VS states.
    fn update_mstatus_sd(&mut self) {
        let dirty = self.mstatus.vs() == RegStatus::Dirty || self.mstatus.fs() == RegStatus::Dirty;
        self.mstatus.set_sd(dirty);
    }

    /// Prints the CPU registers.
    pub fn dump(&self, epc: u64) {
        print!("{}", self.dump_string(epc));
    }

    /// Formats the CPU registers.
    pub fn dump_string(&self, epc: u64) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = write!(
            s,
            "CPU: 0 Mode: {} IC: {} Mstatus: {:x}\r\n",
            self.mode, self.instret, self.mstatus.0
        );
        if let Some(sym) = &self.symtab {
            if let Some((e, mapped)) = sym.func_name(epc) {
                let _ = write!(s, "epc : {}+0x{:x}\r\n", e.name, mapped - e.start);
            }
            if let Some((e, mapped)) = sym.func_name(self.x[1]) {
                let _ = write!(s, " ra : {}+0x{:x}\r\n", e.name, mapped - e.start);
            }
        }
        let _ = write!(
            s,
            "epc : {:016x} ra : {:016x} sp : {:016x}\r\n",
            epc, self.x[1], self.x[2]
        );
        let mut line = Vec::new();
        for r in 3..32 {
            line.push(format!("{:>3}: {:016x}", isa::reg_name(r), self.x[r as usize]));
            if line.len() == 3 {
                let _ = write!(s, "{}\r\n", line.join(" "));
                line.clear();
            }
        }
        if !line.is_empty() {
            let _ = write!(s, "{}\r\n", line.join(" "));
        }
        s
    }
}
