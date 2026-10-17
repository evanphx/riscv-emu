//! Linux/x86-64 RAM-alias experiment. Guest translations populate sparse
//! host page tables over coherent memfd RAM. Faults return to an ordinary
//! JIT slow path; the signal handler never walks guest tables or maps RAM.

use super::*;
use crate::cpu::JTLB_BANKS;
use std::cell::Cell;
use std::sync::OnceLock;

const WINDOW: usize = 1usize << 39;
const HALF: usize = WINDOW / 2;
const LENGTH: usize = WINDOW * JTLB_BANKS;

pub(in crate::cpu) struct HostMap {
    base: usize,
    fd: std::os::fd::RawFd,
    pages: HashMap<(usize, u64), (u64, bool)>,
    pub fills: u64,
    pub flushes: u64,
}

impl HostMap {
    pub fn new(fd: std::os::fd::RawFd) -> Result<Self, String> {
        install_handler()?;
        // SAFETY: reserve address space only, no committed RAM. Each
        // MAP_FIXED below is confined to this owned reservation.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LENGTH,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(format!(
                "reserve host guest-address windows: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            base: base as usize,
            fd,
            pages: HashMap::default(),
            fills: 0,
            flushes: 0,
        })
    }

    pub fn bias(&self, bank: usize) -> u64 {
        (self.base + bank * WINDOW + HALF) as u64
    }

    pub fn fill(&mut self, bank: usize, vaddr: u64, offset: u64, readable: bool, writable: bool) {
        // Bare-mode addresses outside Sv39 still use the ordinary helper.
        let signed = vaddr as i64;
        if !(-(HALF as i64)..HALF as i64).contains(&signed) {
            return;
        }
        let page = vaddr & !0xfff;
        if !readable {
            self.remove(bank, page);
            return;
        }
        if self.pages.get(&(bank, page)) == Some(&(offset, writable)) {
            return;
        }
        let addr = self.bias(bank).wrapping_add(page) as *mut libc::c_void;
        // SAFETY: page is canonical, so addr is inside this bank's
        // reservation. offset is a checked full RAM page in the memfd.
        let p = unsafe {
            libc::mmap(
                addr,
                4096,
                libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 },
                libc::MAP_SHARED | libc::MAP_FIXED,
                self.fd,
                offset as libc::off_t,
            )
        };
        assert_ne!(
            p,
            libc::MAP_FAILED,
            "map guest RAM alias: {}",
            std::io::Error::last_os_error()
        );
        self.pages.insert((bank, page), (offset, writable));
        self.fills += 1;
    }

    /// Compilation must revoke every writable virtual alias of this
    /// physical page, including aliases evicted from the software TLB.
    pub fn protect_code(&mut self, offset: u64) {
        let base = self.base;
        for (&(bank, page), (physical, writable)) in self.pages.iter_mut() {
            if *physical != offset || !*writable {
                continue;
            }
            let addr = (base + bank * WINDOW + HALF).wrapping_add(page as usize);
            // SAFETY: this page is a live alias in our reservation.
            let result = unsafe { libc::mprotect(addr as *mut _, 4096, libc::PROT_READ) };
            assert_eq!(
                result,
                0,
                "protect guest code alias: {}",
                std::io::Error::last_os_error()
            );
            *writable = false;
        }
    }

    fn remove(&mut self, bank: usize, page: u64) {
        if self.pages.remove(&(bank, page)).is_some() {
            self.reserve(self.bias(bank).wrapping_add(page) as usize, 4096);
        }
    }

    pub fn flush_page(&mut self, addr: u64) {
        for bank in 0..JTLB_BANKS {
            self.remove(bank, addr & !0xfff);
        }
        self.flushes += 1;
    }

    pub fn clear(&mut self) {
        if !self.pages.is_empty() {
            self.reserve(self.base, LENGTH);
            self.pages.clear();
            self.flushes += 1;
        }
    }

    fn reserve(&self, addr: usize, len: usize) {
        // SAFETY: replaces only mappings within our owned reservation.
        let p = unsafe {
            libc::mmap(
                addr as *mut _,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        assert_ne!(
            p,
            libc::MAP_FAILED,
            "revoke guest RAM aliases: {}",
            std::io::Error::last_os_error()
        );
    }
}

impl Drop for HostMap {
    fn drop(&mut self) {
        // SAFETY: releases the reservation and every alias within it.
        unsafe {
            libc::munmap(self.base as *mut _, LENGTH);
        }
    }
}

struct FaultContext {
    jit: *const Jit,
    lo: usize,
    hi: usize,
    faults: Cell<u64>,
}

thread_local! {
    // Initialized before native execution; handler access allocates nothing.
    static ACTIVE: Cell<*const FaultContext> = const { Cell::new(std::ptr::null()) };
}

static PREVIOUS: OnceLock<libc::sigaction> = OnceLock::new();
static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();

fn install_handler() -> Result<(), String> {
    INSTALLED
        .get_or_init(|| {
            // SAFETY: valid sigaction buffers; publish the old disposition
            // before installing our process-wide forwarding handler.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(libc::SIGSEGV, std::ptr::null(), &mut old) != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let _ = PREVIOUS.set(old);
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                action.sa_sigaction = fault_handler as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(libc::SIGSEGV, &action, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
            Ok(())
        })
        .clone()
}

/// Only alias accesses in registered native code can recover. RCX holds a
/// RIP-relative slow-path label, never an embedded process-specific pointer.
unsafe extern "C" fn fault_handler(
    sig: i32,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let recovered = ACTIVE
        .try_with(|active| {
            let Some(c) = (unsafe { active.get().as_ref() }) else {
                return false;
            };
            // SAFETY: Linux supplies siginfo/ucontext; c and its JIT mappings
            // remain alive and unchanged for this thread's native invocation.
            unsafe {
                let address = (*info).si_addr() as usize;
                if address < c.lo || address >= c.hi {
                    return false;
                }
                let uc = &mut *(context as *mut libc::ucontext_t);
                let rip = uc.uc_mcontext.gregs[libc::REG_RIP as usize] as usize;
                let slow = uc.uc_mcontext.gregs[libc::REG_RCX as usize] as usize;
                let jit = &*c.jit;
                let code = |ip: usize| {
                    (ip >= jit.arena.ptr as usize && ip < jit.arena.ptr as usize + jit.arena.pos)
                        || jit
                            .mapped
                            .iter()
                            .any(|m| ip >= m.ptr as usize && ip < m.ptr as usize + m.len)
                };
                if !code(rip) || !code(slow) {
                    return false;
                }
                uc.uc_mcontext.gregs[libc::REG_RIP as usize] = slow as libc::greg_t;
                c.faults.set(c.faults.get() + 1);
                true
            }
        })
        .unwrap_or(false);
    if recovered {
        return;
    }
    // Preserve Rust's stack-overflow handler and any preexisting handler.
    // Unexpected host faults must never be mistaken for guest page faults.
    unsafe {
        if let Some(old) = PREVIOUS.get() {
            if old.sa_sigaction != libc::SIG_DFL && old.sa_sigaction != libc::SIG_IGN {
                if old.sa_flags & libc::SA_SIGINFO != 0 {
                    let handler: unsafe extern "C" fn(
                        i32,
                        *mut libc::siginfo_t,
                        *mut libc::c_void,
                    ) = std::mem::transmute(old.sa_sigaction);
                    handler(sig, info, context);
                } else {
                    let handler: unsafe extern "C" fn(i32) = std::mem::transmute(old.sa_sigaction);
                    handler(sig);
                }
                return;
            }
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Set thread-local recovery bounds only around this CPU's native code.
pub(super) unsafe fn enter(cpu: *mut Cpu, func: BlockFn, enter: EnterFn) -> u64 {
    // SAFETY: the caller owns Cpu and all its mappings throughout the call.
    let map = unsafe { (*cpu).jit_host_map.as_ref().unwrap() };
    let c = FaultContext {
        jit: unsafe { (*cpu).jit.as_deref().unwrap() },
        lo: map.base,
        hi: map.base + LENGTH,
        faults: Cell::new(0),
    };
    let previous = ACTIVE.with(|active| active.replace(&c));
    let result = unsafe { enter(cpu, func) };
    ACTIVE.with(|active| active.set(previous));
    unsafe {
        (*cpu).jit.as_mut().unwrap().stats.mmap_faults += c.faults.get();
    }
    result
}
