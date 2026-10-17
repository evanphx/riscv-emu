//! Linux userspace emulator. The emulator loads a static or
//! dynamically linked RISC-V Linux ELF executable and runs it in user
//! mode, emulating the Linux syscalls.

pub mod linux;

use crate::cpu::{Cpu, TrapHandler};
use crate::isa::elf::{self, ET_DYN, PF_R, PF_W, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
use crate::isa::{self, reg, Mode, Trap};
use crate::kernel::{Kernel, Process};
use crate::memory::{Memory, PAGE_SIZE, RAM_BASE};
use crate::mmu::{self, Satp, ACCESS_EXEC, ACCESS_READ, ACCESS_WRITE, PTE_R, PTE_U, PTE_W, PTE_X};
use crate::Params;
use linux::auxv::*;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

/// Information about a loaded ELF file.
#[derive(Clone, Debug, Default)]
pub struct FileInfo {
    pub dynamic: bool,
    pub phdr: u64,
    pub phnum: u64,
    /// ELF header e_phoff, used to compute phdr when PT_PHDR is
    /// absent.
    pub phoff: u64,
    pub base: u64,
    pub entry: u64,
    pub interp: String,
}

/// The Linux userspace emulator.
pub struct Emulator {
    pub cpu: Cpu,
    pub mem: Arc<Memory>,

    /// Load address of the next dynamic (ET_DYN) ELF file.
    pub prog_base: u64,
    pub prog_base_end: u64,

    pub prog: Option<FileInfo>,
    pub interp: Option<FileInfo>,

    pub kernel: Arc<Mutex<Kernel>>,
}

impl Emulator {
    /// Creates a new emulator with an 8 MB RAM and a 1 MB stack.
    pub fn new(params: Params) -> Result<Emulator, String> {
        let mem = Arc::new(if params.jit_backend == "direct-mmap" {
            Memory::new_shared(RAM_BASE, 2048 * PAGE_SIZE).map_err(|e| e.to_string())?
        } else {
            Memory::new(RAM_BASE, 2048 * PAGE_SIZE)
        });

        // Skip page 0.
        mem.alloc_page().map_err(|e| e.to_string())?;
        let page = mem.alloc_page().map_err(|e| e.to_string())?;
        let satp = Satp::new(mmu::SATP_MODE_SV39, page);

        // Mmap pool. The Go version starts it at 0x40_0000_0000, which
        // is not a canonical Sv39 address: bit 38 must be zero for
        // user addresses.
        let mmap_start = 0x20_0000_0000;

        let mut kernel = Kernel {
            params: params.clone(),
            mmap_start,
            mmap_end: mmap_start,
            ..Default::default()
        };

        // Stack.
        let stack_top: u64 = 0x7fff_f000;
        let stack_size: u64 = 1024 * 1024;
        let stack_bottom = stack_top - stack_size;

        kernel.add_vma(stack_bottom, stack_top, ACCESS_READ | ACCESS_WRITE, None, 0);

        // Allocate stack pages.
        for i in 0..stack_size / PAGE_SIZE {
            let page = mem.alloc_page().map_err(|e| e.to_string())?;
            mmu::set_map_sv39(
                &mem,
                satp,
                (stack_bottom >> 12) + i,
                page,
                PTE_R | PTE_W | PTE_U,
            )?;
        }

        let mut cpu = Cpu::new(mem.clone());
        cpu.trace = params.cpu_trace;
        crate::cpu::configure_jit(&mut cpu, &params)?;
        cpu.set_mode(Mode::U);
        cpu.set_satp(satp);
        cpu.x[reg::SP as usize] = stack_top;

        kernel.harts.push(cpu.signals.clone());

        Ok(Emulator {
            cpu,
            mem,
            prog_base: 0x400000,
            prog_base_end: 0x400000,
            prog: None,
            interp: None,
            kernel: Arc::new(Mutex::new(kernel)),
        })
    }

    fn debugf(&self, msg: std::fmt::Arguments) {
        if self.kernel.lock().unwrap().params.verbose {
            print!("{}", msg);
        }
    }

    /// Loads the ELF executable and its program interpreter.
    pub fn load_elf(&mut self, file: &str) -> Result<(), String> {
        let info = self.load(file)?;
        let interp = info.interp.clone();
        self.prog = Some(info);

        if !interp.is_empty() {
            // Dynamically linked executable.
            self.debugf(format_args!("PT_INTERP: {}\n", interp));
            let path = self.kernel.lock().unwrap().params.make_path(&interp);
            self.interp = Some(self.load(&path.to_string_lossy())?);
        }
        Ok(())
    }

    fn load(&mut self, file: &str) -> Result<FileInfo, String> {
        let f = elf::File::open(file)?;

        let dynamic = f.typ == ET_DYN;
        let mut info = FileInfo {
            dynamic,
            phnum: f.progs.len() as u64,
            phoff: f.phoff,
            entry: f.entry,
            ..Default::default()
        };
        let base = if dynamic { self.prog_base } else { 0 };
        info.entry += base;

        let satp = self.cpu.satp();
        for prog in &f.progs {
            match prog.typ {
                PT_PHDR => info.phdr = prog.vaddr + base,

                PT_LOAD => {
                    let mut vaddr = prog.vaddr + base;
                    let end = (vaddr + prog.memsz + 4095) & !0xfff;
                    let head_pad = (vaddr & 0xfff) as usize;
                    vaddr &= !0xfff;

                    self.prog_base_end = self.prog_base_end.max(end);
                    if info.base == 0 {
                        info.base = vaddr;
                    }

                    let mut data = vec![0u8; (end - vaddr) as usize];
                    let src = f.prog_data(prog);
                    if src.len() as u64 != prog.filesz {
                        return Err("unexpected EOF".to_string());
                    }
                    data[head_pad..head_pad + src.len()].copy_from_slice(src);

                    let mut prot = 0;
                    let mut flags = PTE_U;
                    for (bit, access, pte) in [
                        (PF_R, ACCESS_READ, PTE_R),
                        (PF_W, ACCESS_WRITE, PTE_W),
                        (PF_X, ACCESS_EXEC, PTE_X),
                    ] {
                        if prog.flags & bit != 0 {
                            prot |= access;
                            flags |= pte;
                        }
                    }

                    let mut kern = self.kernel.lock().unwrap();
                    kern.add_vma(vaddr, end, prot, None, 0);

                    // Load data into memory.
                    for (i, chunk) in data.chunks(PAGE_SIZE as usize).enumerate() {
                        let page = self.mem.alloc_page().map_err(|e| e.to_string())?;
                        let addr = vaddr + i as u64 * PAGE_SIZE;
                        mmu::set_map_sv39(&self.mem, satp, addr >> 12, page, flags)?;
                        let ofs = self.mem.page_offset_of(page).map_err(|e| e.to_string())?;
                        self.mem.write_bytes(ofs, chunk);
                    }

                    if prog.flags & PF_W != 0 && end > kern.heap_end {
                        kern.heap_start = end;
                        kern.heap_end = end;
                    }
                }

                PT_INTERP => {
                    let data = f.prog_data(prog);
                    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
                    info.interp = String::from_utf8_lossy(&data[..len]).to_string();
                }

                _ => {}
            }
        }

        // If no PT_PHDR segment was present, compute the program
        // header address from the first load address plus the ELF
        // header's phoff. glibc requires a valid AT_PHDR to locate the
        // program's PT_DYNAMIC and .fini_array; AT_PHDR=0 causes it to
        // register a null atexit handler and crash on exit.
        if info.phdr == 0 && info.base != 0 {
            info.phdr = info.base + info.phoff;
        }

        // Update base address for the next ELF file.
        self.prog_base = self.prog_base_end;

        Ok(info)
    }

    /// Runs the loaded program with the arguments and environment.
    /// Returns the program's exit code.
    pub fn run(&mut self, argv: &[String], envp: &[String]) -> Result<i32, String> {
        let prog = self.prog.clone().ok_or("no program loaded")?;

        let mut argv_ptrs = Vec::new();
        let mut envp_ptrs = Vec::new();

        self.debugf(format_args!("argv:\n"));
        for (i, v) in argv.iter().enumerate() {
            self.debugf(format_args!(" - argv[{}]:{}\n", i, v));
            self.push_cstring(v)?;
            argv_ptrs.push(self.sp());
        }

        self.debugf(format_args!("envp:\n"));
        for (i, v) in envp.iter().enumerate() {
            self.debugf(format_args!(" - envp[{}]:{}\n", i, v));
            self.push_cstring(v)?;
            envp_ptrs.push(self.sp());
        }

        let mut random = [0u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        self.push_data(&random)?;
        let at_random = self.sp();

        // Calculate the exact number of 8-byte words we are about to
        // push. We ignore the auxiliary vector as it is always a
        // multiple of 16 bytes.
        //
        //  argc (1) + argv ptrs (len) + NULL (1) + envp ptrs (len) + NULL (1)
        let words_to_push = 1 + argv_ptrs.len() + 1 + envp_ptrs.len() + 1;

        // Align sp to 16 bytes.
        self.cpu.x[reg::SP as usize] &= !0b1111;

        // If pushing the words throws us off 16-byte alignment, push
        // a pad word.
        if words_to_push % 2 != 0 {
            self.push(0)?;
        }

        // Auxiliary vector, pushed in reverse order: value first, then
        // type.
        let interp_base = self.interp.as_ref().map_or(0, |i| i.base);
        let phdr = match prog.phdr {
            0 => prog.base + prog.phoff,
            phdr => phdr,
        };
        let auxv = [
            (AT_NULL, 0),
            (AT_RANDOM, at_random),
            (AT_EXECFN, argv_ptrs.first().copied().unwrap_or(0)),
            (AT_PAGESZ, 4096),
            (AT_HWCAP, 0x112d),
            (AT_CLKTCK, 100),
            (AT_PHDR, phdr),
            (AT_PHENT, 56),
            (AT_PHNUM, prog.phnum),
            (AT_BASE, interp_base),
            (AT_ENTRY, prog.entry),
            (AT_UID, 1000),
            (AT_EUID, 1000),
            (AT_GID, 1000),
            (AT_EGID, 1000),
            // Not setuid.
            (AT_SECURE, 0),
        ];
        for (typ, val) in auxv {
            self.push(val)?;
            self.push(typ)?;
        }

        // Environment pointers.
        self.push(0)?;
        for p in envp_ptrs.iter().rev() {
            self.push(*p)?;
        }

        // Argument pointers and count.
        self.push(0)?;
        for p in argv_ptrs.iter().rev() {
            self.push(*p)?;
        }
        self.push(argv_ptrs.len() as u64)?;

        self.cpu.pc = match &self.interp {
            Some(interp) => interp.entry,
            None => prog.entry,
        };

        // Clear argument registers a0-a7.
        for r in reg::A0..=reg::A7 {
            self.cpu.x[r as usize] = 0;
        }

        let mut proc = Kernel::new_process(&self.kernel, None);
        for fd in [
            std::io::stdin().as_fd().try_clone_to_owned(),
            std::io::stdout().as_fd().try_clone_to_owned(),
            std::io::stderr().as_fd().try_clone_to_owned(),
        ] {
            proc.alloc_fd(File::from(fd.map_err(|e| e.to_string())?));
        }
        self.cpu.trap_handler = Some(trap_handler(proc));

        self.cpu.run()?;

        Ok(self.kernel.lock().unwrap().exit_code.unwrap_or(0))
    }

    fn sp(&self) -> u64 {
        self.cpu.x[reg::SP as usize]
    }

    /// Pushes the 64-bit value to the stack.
    pub fn push(&mut self, val: u64) -> Result<(), String> {
        self.push_data(&val.to_le_bytes())
    }

    /// Pushes the NUL-terminated string to the stack.
    pub fn push_cstring(&mut self, val: &str) -> Result<(), String> {
        let mut data = val.as_bytes().to_vec();
        data.push(0);
        self.push_data(&data)
    }

    /// Pushes the data to the stack.
    pub fn push_data(&mut self, data: &[u8]) -> Result<(), String> {
        let sp = self.sp() - data.len() as u64;
        self.cpu.x[reg::SP as usize] = sp;
        self.cpu
            .copy_to_user(sp, data)
            .map_err(|e| format!("stack: {:?}", e))
    }
}

/// Creates the trap handler that handles the traps of the process's
/// hart. Handler errors stop the hart like CPU panics.
pub fn trap_handler(mut proc: Process) -> TrapHandler {
    Box::new(move |cpu, trap| {
        handle_trap(&mut proc, cpu, trap).map_err(|e| format!("CPU: panic: {}", e))
    })
}

/// Handles the trap: maps pages on page faults and executes
/// syscalls. Returns Ok(true) if the trap was handled.
pub fn handle_trap(proc: &mut Process, cpu: &mut Cpu, trap: &Trap) -> Result<bool, String> {
    match trap.cause {
        isa::CAUSE_INST_ACCESS_FAULT
        | isa::CAUSE_LOAD_ACCESS_FAULT
        | isa::CAUSE_STORE_ACCESS_FAULT => {
            println!("trap: {}", trap);
            proc.kernel().print_vma();
            Ok(false)
        }

        isa::CAUSE_INST_PAGE_FAULT | isa::CAUSE_LOAD_PAGE_FAULT | isa::CAUSE_STORE_PAGE_FAULT => {
            page_fault(proc, cpu, trap)
        }

        isa::CAUSE_ILLEGAL_INSTR
        | isa::CAUSE_STORE_ADDR_MISALIGNED
        | isa::CAUSE_LOAD_ADDR_MISALIGNED
        | isa::CAUSE_INST_ADDR_MISALIGNED
        | isa::CAUSE_BREAKPOINT => {
            println!("trap: {}", trap);
            Ok(false)
        }

        isa::CAUSE_ECALL_U | isa::CAUSE_ECALL_S | isa::CAUSE_ECALL_VS | isa::CAUSE_ECALL_M => {
            let x = &cpu.x;
            let a = |r: u8| x[r as usize];
            let id = a(reg::A7);
            let args = [
                a(reg::A0),
                a(reg::A1),
                a(reg::A2),
                a(reg::A3),
                a(reg::A4),
                a(reg::A5),
            ];
            let v = linux::syscall(proc, cpu, id, args)?;
            cpu.x[reg::A0 as usize] = v;
            cpu.pc += 4;
            Ok(true)
        }

        _ => {
            println!("unknown trap: {}", trap);
            Ok(false)
        }
    }
}

/// Maps the faulting page if it belongs to a virtual memory area that
/// allows the access. File-backed pages are read from the file.
fn page_fault(proc: &mut Process, cpu: &mut Cpu, trap: &Trap) -> Result<bool, String> {
    let kern = proc.kernel();
    let Some(vma) = kern.vma.iter().find(|v| v.contains(trap.tval)) else {
        // Unmapped address.
        println!("trap: {}", trap);
        kern.print_vma();
        return Ok(false);
    };
    if !vma.satisfies(trap.cause) {
        println!("trap: permission mismatch: {:x}/{}", trap.tval, vma);
        return Ok(false);
    }

    let mem = &cpu.mem;
    let page = mem.alloc_page().map_err(|e| e.to_string())?;
    let vpage = trap.tval >> 12;

    if let Some(src) = &vma.source {
        let offset = (vpage << 12) - vma.start;
        let mut buf = [0u8; PAGE_SIZE as usize];
        let n = read_full_at(src, &mut buf, vma.offset + offset)?;
        if n == 0 {
            return Err("EOF".to_string());
        }
        let ofs = mem.page_offset_of(page).map_err(|e| e.to_string())?;
        mem.write_bytes(ofs, &buf);
    }

    mmu::set_map_sv39(mem, cpu.satp(), vpage, page, vma.page_table_flags())?;
    Ok(true)
}

/// Reads up to buf.len() bytes from the file offset. Returns the
/// number of bytes read; fewer than requested at the end of file.
fn read_full_at(f: &File, buf: &mut [u8], offset: u64) -> Result<usize, String> {
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], offset + n as u64) {
            Ok(0) => break,
            Ok(m) => n += m,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a static ELF executable with a text segment holding the
    /// code and an empty one page data segment after it.
    fn build_elf(code: &[u32]) -> Vec<u8> {
        const TEXT: u64 = 0x10000;
        const HDRS: u64 = 64 + 2 * 56;

        let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
        elf.resize(16, 0);
        let filesz = HDRS + code.len() as u64 * 4;
        for (v, size) in [
            (elf::ET_EXEC as u64, 2),
            (elf::EM_RISCV as u64, 2),
            (1, 4),           // e_version
            (TEXT + HDRS, 8), // e_entry
            (64, 8),          // e_phoff
            (0, 8),           // e_shoff
            (0, 4),           // e_flags
            (64, 2),          // e_ehsize
            (56, 2),          // e_phentsize
            (2, 2),           // e_phnum
            (64, 2),          // e_shentsize
            (0, 2),           // e_shnum
            (0, 2),           // e_shstrndx
            // Text segment.
            (PT_LOAD as u64, 4),
            ((PF_R | PF_X) as u64, 4),
            (0, 8),
            (TEXT, 8),
            (TEXT, 8),
            (filesz, 8),
            (filesz, 8),
            (0x1000, 8),
            // Data segment.
            (PT_LOAD as u64, 4),
            ((PF_R | PF_W) as u64, 4),
            (0, 8),
            (TEXT + 0x1000, 8),
            (TEXT + 0x1000, 8),
            (0, 8),
            (0x1000, 8),
            (0x1000, 8),
        ] {
            elf.extend_from_slice(&v.to_le_bytes()[..size]);
        }
        for insn in code {
            elf.extend_from_slice(&insn.to_le_bytes());
        }
        elf
    }

    #[test]
    fn brk_page_fault_exit() {
        let code = [
            0x00000513, // li a0,0
            0x0d600893, // li a7,214 (brk)
            0x00000073, // ecall
            0x00050413, // mv s0,a0
            0x000012b7, // lui t0,0x1
            0x00540533, // add a0,s0,t0
            0x0d600893, // li a7,214 (brk)
            0x00000073, // ecall
            0x00543423, // sd t0,8(s0)
            0x00843503, // ld a0,8(s0)
            0x00655513, // srli a0,a0,0x6
            0x05e00893, // li a7,94 (exit_group)
            0x00000073, // ecall
        ];
        let path = std::env::temp_dir().join(format!("rsemu-brk-{}.elf", std::process::id()));
        std::fs::write(&path, build_elf(&code)).unwrap();

        let mut emu = Emulator::new(Params::default()).unwrap();
        let loaded = emu.load_elf(&path.to_string_lossy());
        std::fs::remove_file(&path).unwrap();
        loaded.unwrap();

        let code = emu.run(&["prog".to_string()], &[]).unwrap();
        assert_eq!(code, 0x1000 >> 6);

        // The heap grew by one page after the data segment. Anonymous
        // areas have offset 0 so adjacent areas are not merged.
        let kern = emu.kernel.lock().unwrap();
        assert_eq!((kern.heap_start, kern.heap_end), (0x12000, 0x13000));
        let vma: Vec<String> = kern.vma.iter().map(|v| v.to_string()).collect();
        assert_eq!(
            vma,
            [
                "     10000:     11000 X.R",
                "     11000:     12000 .WR",
                "     12000:     13000 .WR",
                "  7feff000:  7ffff000 .WR",
            ]
        );
    }
}
