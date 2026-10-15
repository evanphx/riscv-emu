//! The emulator kernel: address space areas, processes, and file
//! descriptors of the Linux userspace emulation.

mod process;
mod vma;

pub use process::{Fd, Process};
pub use vma::Vma;

use crate::cpu::HartSignals;
use crate::Params;
use std::sync::{Arc, Mutex};

/// The kernel state shared by all processes and their threads.
#[derive(Default)]
pub struct Kernel {
    pub params: Params,

    pub next_pid: u64,

    /// Virtual memory areas, sorted by address.
    pub vma: Vec<Vma>,
    pub heap_start: u64,
    pub heap_end: u64,
    pub mmap_start: u64,
    pub mmap_end: u64,

    /// Exit code, set when the process exits.
    pub exit_code: Option<i32>,
    /// Signals of the running harts, used to stop them on exit.
    pub harts: Vec<Arc<HartSignals>>,
}

impl Kernel {
    /// Creates a new process. If tg is set, the new process is a
    /// thread in the thread group of tg.
    pub fn new_process(kernel: &Arc<Mutex<Kernel>>, tg: Option<&Process>) -> Process {
        let mut kern = kernel.lock().unwrap();
        let pid = kern.next_pid;
        kern.next_pid += 1;

        Process {
            kernel: kernel.clone(),
            pid,
            tgid: tg.map(|tg| tg.tgid).unwrap_or(pid),
            fds: Vec::new(),
            ktrace: kern.params.ktrace,
        }
    }

    /// Records the exit code and stops all harts.
    pub fn exit(&mut self, code: i32) {
        self.exit_code = Some(code);
        for hart in &self.harts {
            hart.shutdown();
        }
    }
}
