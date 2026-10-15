//! Linux syscall emulation.

use super::errno::Errno;
use super::futex;
use super::mman::*;
use super::sched::*;
use super::stat::{marshal_file_info, AT_FDCWD};
use super::syscall_info::syscall_info;
use crate::cpu::Cpu;
use crate::emulator;
use crate::isa::{reg, Exception, Mode, Trap};
use crate::kernel::{Kernel, Process};
use crate::mmu::{ACCESS_EXEC, ACCESS_READ, ACCESS_WRITE};
use std::fs::File;
use std::io::{Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Returns the ktrace line header of the process.
fn ktrace_header(proc: &Process) -> String {
    format!("{:5} {:5} ", proc.pid, proc.tgid)
}

/// Prints a ktrace line if kernel tracing is enabled.
macro_rules! ktracef {
    ($proc:expr, $($arg:tt)*) => {
        if $proc.ktrace {
            print!("{}{}", ktrace_header($proc), format_args!($($arg)*));
        }
    };
}

/// Evaluates the user memory access. If the access faults, the trap
/// is handled as if the CPU had raised it, and the syscall returns
/// EFAULT.
macro_rules! user {
    ($proc:expr, $cpu:expr, $access:expr) => {
        match $access {
            Ok(v) => v,
            Err(e) => return Ok(fault($proc, $cpu, e)),
        }
    };
}

/// Passes the memory access exception to the trap handler and returns
/// EFAULT. The trap handler maps the page if the address belongs to
/// a virtual memory area, but the syscall fails anyway.
fn fault(proc: &mut Process, cpu: &mut Cpu, e: Exception) -> u64 {
    if let Exception::Trap { cause, tval } = e {
        let trap = Trap::new(cpu.pc, cause, tval);
        // The syscall fails with EFAULT whatever the handler does.
        let _ = emulator::handle_trap(proc, cpu, &trap);
    }
    Errno::EFAULT.ret()
}

/// Executes the syscall id with the arguments a0-a5. Returns the
/// syscall return value, or an error if the emulation must stop.
pub fn syscall(proc: &mut Process, cpu: &mut Cpu, id: u64, args: [u64; 6]) -> Result<u64, String> {
    ktrace(proc, id, &args);

    let ret = linux_syscall(proc, cpu, id, args);

    // A successful exit does not return.
    if !(matches!(id, 93 | 94) && ret.is_ok()) {
        ktrace_result(proc, id, &ret);
    }
    ret
}

fn linux_syscall(
    proc: &mut Process,
    cpu: &mut Cpu,
    id: u64,
    args: [u64; 6],
) -> Result<u64, String> {
    let [a0, a1, a2, a3, a4, a5] = args;

    match id {
        17 => {
            // getcwd
            let mut data = b"/".to_vec();
            if data.len() as u64 + 1 > a1 {
                data.truncate(a1.saturating_sub(1) as usize);
            }
            data.push(0);
            user!(proc, cpu, cpu.copy_to_user(a0, &data));
            Ok(0)
        }

        48 => {
            // faccessat
            let pathname = user!(proc, cpu, cpu.user_cstring(a1));
            ktracef!(proc, "     pathname={}\n", pathname);
            let path = proc.kernel().params.make_path(&pathname);
            if std::fs::metadata(path).is_err() {
                return Ok(Errno::ENOENT.ret());
            }
            Ok(0)
        }

        56 => {
            // openat
            let pathname = user!(proc, cpu, cpu.user_cstring(a1));
            ktracef!(proc, "     pathname={}\n", pathname);
            let path = proc.kernel().params.make_path(&pathname);
            match File::open(path) {
                Ok(f) => Ok(proc.alloc_fd(f) as u64),
                Err(_) => Ok(Errno::ENOENT.ret()),
            }
        }

        57 => {
            // close
            if !proc.close_fd(a0 as i64) {
                return Ok(Errno::EBADF.ret());
            }
            Ok(0)
        }

        63 => {
            // read
            let Some(f) = proc.get_fd(a0 as i64) else {
                return Ok(Errno::EBADF.ret());
            };
            let mut buf = vec![0u8; a2 as usize];
            let n = match (&*f).read(&mut buf) {
                // End of file is an error, like in the Go os package.
                Ok(0) if !buf.is_empty() => return Ok(Errno::EIO.ret()),
                Ok(n) => n,
                Err(_) => return Ok(Errno::EIO.ret()),
            };
            user!(proc, cpu, cpu.copy_to_user(a1, &buf[..n]));
            Ok(n as u64)
        }

        64 => {
            // write
            let f = match proc.get_fd(a0 as i64) {
                Some(f) if a0 != 0 => f,
                _ => return Ok(Errno::EBADF.ret()),
            };
            let mut wrote = 0;
            let mut buf = [0u8; 4096];
            while wrote < a2 {
                // Copy up to the end of the page.
                let addr = a1.wrapping_add(wrote);
                let n = (4096 - addr % 4096).min(a2 - wrote) as usize;
                user!(proc, cpu, cpu.copy_from_user(addr, &mut buf[..n]));
                if (&*f).write_all(&buf[..n]).is_err() {
                    return Ok(Errno::EIO.ret());
                }
                wrote += n as u64;
            }
            Ok(wrote)
        }

        66 => {
            // writev
            let Some(f) = proc.get_fd(a0 as i64) else {
                return Ok(Errno::EBADF.ret());
            };
            let mut iov = a1;
            let mut wrote = 0;
            let mut buf = [0u8; 1024];
            for _ in 0..a2 as i64 {
                let mut base = user!(proc, cpu, cpu.load64(iov));
                let mut l = user!(proc, cpu, cpu.load64(iov + 8));
                iov += 16;

                while l > 0 {
                    let n = l.min(buf.len() as u64);
                    user!(proc, cpu, cpu.copy_from_user(base, &mut buf[..n as usize]));
                    if (&*f).write_all(&buf[..n as usize]).is_err() {
                        return Ok(Errno::EIO.ret());
                    }
                    wrote += n;
                    l -= n;
                    base += n;
                }
            }
            Ok(wrote)
        }

        78 => {
            // readlinkat
            if a0 as i64 == AT_FDCWD {
                ktracef!(proc, "     - AT_FDCWD\n");
            }
            Ok(Errno::ENOENT.ret())
        }

        79 => {
            // newfstatat
            if a0 as i64 == AT_FDCWD {
                ktracef!(proc, "     - AT_FDCWD\n");
            }
            let pathname = user!(proc, cpu, cpu.user_cstring(a1));
            let path = proc.kernel().params.make_path(&pathname);
            let Ok(fi) = std::fs::metadata(path) else {
                return Ok(Errno::EIO.ret());
            };
            user!(proc, cpu, cpu.copy_to_user(a2, &marshal_file_info(&fi)));
            Ok(0)
        }

        80 => {
            // fstat
            let Some(f) = proc.get_fd(a0 as i64) else {
                return Ok(Errno::EBADF.ret());
            };
            let Ok(fi) = f.metadata() else {
                return Ok(Errno::EIO.ret());
            };
            user!(proc, cpu, cpu.copy_to_user(a1, &marshal_file_info(&fi)));
            Ok(0)
        }

        93 => {
            // exit
            let mut kern = proc.kernel();
            if kern.params.profile {
                return Err("profile".to_string());
            }
            kern.exit(a0 as i32);
            Ok(0)
        }

        94 => {
            // exit_group
            let mut kern = proc.kernel();
            if kern.params.profile {
                return Err("profile".to_string());
            }
            let runtime = cpu.start_time.map(|t| t.elapsed()).unwrap_or_default();
            ktracef!(
                proc,
                "     val={}, instret={}, runtime={:?}, MIPS={:.2}\n",
                a0,
                cpu.instret,
                runtime,
                cpu.instret as f64 / runtime.as_secs_f64() / 1_000_000.0
            );
            kern.exit(a0 as i32);
            Ok(0)
        }

        // set_tid_address: return the caller's thread ID.
        96 => Ok(proc.pid),

        98 => {
            // futex
            let (addr, op, val) = (a0, a1, a2);
            ktracef!(
                proc,
                "    => futex({:x},{}[{}],{})\n",
                addr,
                op,
                futex::op_name(op),
                val
            );
            match op & 127 {
                futex::FUTEX_WAIT => {
                    let v = user!(proc, cpu, cpu.load32(addr));
                    ktracef!(proc, "    => val={}, wait={}\n", v, val);
                    // Without other threads to wake us, a wait on
                    // the expected value would deadlock. Return EAGAIN
                    // in both cases so the caller retries.
                    Ok(Errno::EAGAIN.ret())
                }
                // No threads are waiting: none woken.
                futex::FUTEX_WAKE => Ok(0),
                _ => {
                    println!(
                        "    => unimplemented futex op {}, returning EINVAL",
                        op & 127
                    );
                    Err(format!("futex op {}", op))
                }
            }
        }

        // set_robust_list
        99 => Ok(0),

        101 => {
            // nanosleep
            let tv_sec = user!(proc, cpu, cpu.load64(a0));
            let tv_nsec = user!(proc, cpu, cpu.load64(a0 + 8));
            ktracef!(proc, "     nanosleep: {},{}\n", tv_sec, tv_nsec);
            let ns = (tv_sec as i64)
                .wrapping_mul(1_000_000_000)
                .wrapping_add(tv_nsec as i64);
            if ns > 0 {
                std::thread::sleep(Duration::from_nanos(ns as u64));
            }
            Ok(0)
        }

        113 => {
            // clock_gettime
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let mut buf = [0u8; 16];
            buf[..8].copy_from_slice(&now.as_secs().to_le_bytes());
            buf[8..].copy_from_slice(&(now.subsec_nanos() as u64).to_le_bytes());
            user!(proc, cpu, cpu.copy_to_user(a1, &buf));
            Ok(0)
        }

        // rt_sigaction: signals are not delivered, accept the
        // handlers so glibc doesn't abort during initialization.
        134 => Ok(0),

        // rt_sigprocmask: no signal mask to manage.
        135 => Ok(0),

        160 => {
            // uname
            let mut buf = [0u8; 390];
            for (ofs, s) in [
                (0, "GoEMU Linux"),
                (65, "goemu.local"),
                (130, "0.0.1"),
                (195, "GoEMU Kernel Version 0.0"),
                (260, "riscv"),
            ] {
                buf[ofs..ofs + s.len()].copy_from_slice(s.as_bytes());
            }
            user!(proc, cpu, cpu.copy_to_user(a0, &buf));
            Ok(0)
        }

        214 => {
            // brk
            let mut kern = proc.kernel();
            if a0 > kern.heap_end {
                let brk = (a0 + 4095) & !0xfff;
                let heap_end = kern.heap_end;
                kern.add_vma(heap_end, brk, ACCESS_READ | ACCESS_WRITE, None, 0);

                ktracef!(proc, "     brk: => {:x} - {:x}\n", heap_end, brk);
                ktracef!(proc, "     VMA:\n");
                for (i, vma) in kern.vma.iter().enumerate() {
                    ktracef!(proc, "     {:3}: {}\n", i, vma);
                }
                kern.heap_end = brk;
            }
            Ok(kern.heap_end)
        }

        // munmap: areas are not unmapped.
        215 => Ok(0),

        220 => clone(proc, cpu, a0, a1),

        222 => mmap(proc, a0, a1, a2, a3, a4 as i64, a5),

        226 => {
            // mprotect
            let (addr, mut size, prot) = (a0, a1, a2);
            if addr & 0xfff != 0 || size == 0 {
                return Ok(Errno::EINVAL.ret());
            }
            // Round size to full pages. Note: this keeps the Go
            // version's rounding which masks the page offset instead
            // of clearing it.
            size = size.wrapping_add(4095) & 0xfff;

            let mut p = Vec::new();
            let mut vma_prot = 0;
            for (bit, name, access) in [
                (PROT_READ, "R", ACCESS_READ),
                (PROT_WRITE, "W", ACCESS_WRITE),
                (PROT_EXEC, "X", ACCESS_EXEC),
            ] {
                if prot & bit != 0 {
                    p.push(name);
                    vma_prot |= access;
                }
            }
            ktracef!(
                proc,
                "     mprotect: {:x}:{:x}: {}\n",
                addr,
                addr.wrapping_add(size),
                p.join(",")
            );

            // XXX if the new area overlaps and protection flags
            // conflict, the page table entries must be updated.
            proc.kernel()
                .add_vma(addr, addr.wrapping_add(size), vma_prot, None, 0);
            Ok(0)
        }

        // prlimit64
        261 => Ok(0),

        278 => {
            // getrandom
            let mut random = vec![0u8; a1 as usize];
            if File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut random))
                .is_err()
            {
                return Ok(Errno::EFAULT.ret());
            }
            user!(proc, cpu, cpu.copy_to_user(a0, &random));
            Ok(a1)
        }

        _ => {
            ktracef!(proc, "RET  skipping syscall {}\n", id);
            Ok(0)
        }
    }
}

/// Implements clone: the child thread starts after the ecall
/// instruction with the stack sp, sharing the address space.
fn clone(proc: &mut Process, cpu: &mut Cpu, flags: u64, sp: u64) -> Result<u64, String> {
    let mut child = Kernel::new_process(&proc.kernel, Some(proc));
    child.fds = proc.fds.clone();

    let mut hart = Cpu::new(cpu.mem.clone());
    hart.set_mode(Mode::U);
    hart.set_satp(cpu.satp());
    hart.x = cpu.x;
    hart.f = cpu.f;
    // The ecall instruction is always 32 bits.
    hart.pc = cpu.pc + 4;
    hart.x[reg::A0 as usize] = 0;
    hart.x[reg::SP as usize] = sp;

    if flags & CLONE_PARENT_SETTID != 0 {
        return Err("clone: PARENT_SETTID".to_string());
    }
    if flags & CLONE_SETTLS != 0 {
        return Err("clone: SETTLS".to_string());
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        return Err("clone: CHILD_SETTID".to_string());
    }
    ktracef!(
        &child,
        "clone: ret={}, PC={:x}\n",
        hart.x[reg::A0 as usize],
        hart.pc
    );

    let (pid, tgid) = (child.pid, child.tgid);
    let kernel = proc.kernel.clone();
    kernel.lock().unwrap().harts.push(hart.signals.clone());
    hart.trap_handler = Some(emulator::trap_handler(child));
    std::thread::Builder::new()
        .name(format!("pid{}", pid))
        .spawn(move || {
            let result = hart.run();
            // The process exit stops all harts silently.
            if kernel.lock().unwrap().exit_code.is_some() {
                return;
            }
            match result {
                Ok(()) => println!("process {} {}: exit", pid, tgid),
                Err(e) => println!("process {} {}: {}", pid, tgid, e),
            }
        })
        .map_err(|e| e.to_string())?;

    ktracef!(proc, "clone: ret={}, PC={:x}\n", pid, cpu.pc);

    Ok(pid)
}

/// Implements mmap. The areas are mapped lazily on page faults.
fn mmap(
    proc: &mut Process,
    a0: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: i64,
    offset: u64,
) -> Result<u64, String> {
    let addr = if a0 == 0 {
        // Choose address from the mmap region.
        proc.kernel().mmap_end
    } else {
        // XXX the address is used as-is.
        ktracef!(proc, "     ?? using provided address {:x}\n", a0);
        a0
    };
    let mut source = None;
    if fd >= 0 {
        source = proc.get_fd(fd);
        if source.is_none() {
            return Ok(Errno::EBADF.ret());
        }
        if offset & 0xfff != 0 {
            // Offset must be multiple of page size.
            return Ok(Errno::EINVAL.ret());
        }
    } else if offset != 0 {
        // No source, offset must be zero.
        return Ok(Errno::EINVAL.ret());
    }

    let mut ps = Vec::new();
    let mut vma_prot = 0;
    for (bit, name, access) in [
        (PROT_READ, "read", ACCESS_READ),
        (PROT_WRITE, "write", ACCESS_WRITE),
        (PROT_EXEC, "exec", ACCESS_EXEC),
    ] {
        if prot & bit != 0 {
            ps.push(name);
            vma_prot |= access;
        }
    }
    let fs: Vec<&str> = MAP_FLAG_NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect();

    ktracef!(proc, "     prot={}, flags={}\n", ps.join(","), fs.join(","));
    ktracef!(proc, "     fd={}, offset={:x}\n", fd, offset);

    // Align size to page size.
    let length = length.wrapping_add(4095) & !4095;

    // Add a reference to the file descriptor.
    if source.is_some() && !proc.ref_fd(fd) {
        return Ok(Errno::EBADF.ret());
    }

    let mut kern = proc.kernel();
    kern.add_vma(addr, addr.wrapping_add(length), vma_prot, source, offset);

    // Update pointer for next call.
    kern.mmap_end += length;
    drop(kern);

    ktracef!(proc, "     => {:x}:{:x}\n", addr, addr.wrapping_add(length));

    Ok(addr)
}

/// Traces the syscall call.
fn ktrace(proc: &Process, id: u64, args: &[u64; 6]) {
    if !proc.ktrace {
        return;
    }
    let mut line = ktrace_header(proc);
    let all = || args.map(|a| a.to_string()).join(",");
    match syscall_info(id) {
        None => line += &format!("CALL {}({})", id, all()),
        Some(info) if info.argc < 0 => line += &format!("CALL {}({})", info.name, all()),
        Some(info) if !info.format.is_empty() => {
            let a: Vec<String> = info
                .format
                .chars()
                .zip(args.iter())
                .map(|(ch, &arg)| match ch {
                    'i' => (arg as i64).to_string(),
                    'p' => format!("{:x}", arg),
                    _ => arg.to_string(),
                })
                .collect();
            line += &format!("CALL {}({})", info.name, a.join(","));
        }
        Some(info) => {
            let a: Vec<String> = args[..info.argc as usize]
                .iter()
                .map(|a| a.to_string())
                .collect();
            line += &format!("CALL {}({})", info.name, a.join(","));
        }
    }
    println!("{}", line);
}

/// Traces the syscall result.
fn ktrace_result(proc: &Process, id: u64, ret: &Result<u64, String>) {
    if !proc.ktrace {
        return;
    }
    let name = match syscall_info(id) {
        Some(info) => info.name.to_string(),
        None => id.to_string(),
    };
    let header = ktrace_header(proc);
    match ret {
        Err(e) => println!("{}ERR  {} {}", header, name, e),
        Ok(ret) if (*ret as i64) < 0 => {
            let errno = Errno((*ret as i64).wrapping_neg());
            println!("{}ERR  {} {}[{}]", header, name, errno, *ret as i64);
        }
        Ok(ret) => println!("{}RET  {} {}", header, name, ret),
    }
}
