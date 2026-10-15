//! Host-Target Interface (HTIF) used by the riscv-tests.

use crate::cpu::HartSignals;
use crate::isa::Exception;
use crate::memory::Memory;
use crate::mmu::Overlay;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// HTIF device intercepting stores to the tohost register.
pub struct Htif {
    pub signals: Arc<HartSignals>,
    pub start: u64,
    pub end: u64,
    pub to_host: u64,
    pub from_host: u64,
    /// Exit status written by the guest. Value 1 means success;
    /// otherwise the failing test number is status >> 1.
    pub exit_status: Arc<AtomicU64>,
}

impl Htif {
    pub fn new(signals: Arc<HartSignals>, start: u64, size: u64, to: u64, from: u64) -> Htif {
        Htif {
            signals,
            start,
            end: start + size,
            to_host: to,
            from_host: from,
            exit_status: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Overlay for Htif {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        paddr >= self.start && paddr + size <= self.end
    }

    fn store(&mut self, mem: &Memory, paddr: u64) -> Result<(), Exception> {
        if paddr != self.to_host {
            return Ok(());
        }
        let ofs = mem.offset(paddr);
        let reg = mem.read_u64(ofs);
        // Mark command completed.
        mem.write_u64(ofs, 0);

        let devid = (reg >> 56) & 0xff;
        let command = (reg >> 48) & 0xff;
        let payload = reg & 0x00ff_ffff_ffff_ffff;

        match (devid, command) {
            (0, 0) => {
                self.exit_status.store(payload, Ordering::SeqCst);
                self.signals.shutdown();
                Ok(())
            }
            (0, c) => Err(Exception::fatal(format!("unknown dev command {:02x}", c))),
            (d, _) => Err(Exception::fatal(format!("unknown devid {:02x}", d))),
        }
    }
}
