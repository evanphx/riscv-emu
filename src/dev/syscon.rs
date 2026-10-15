//! System controller for poweroff and reboot.

use crate::cpu::HartSignals;
use crate::isa::Exception;
use crate::logger::{Level, Log};
use crate::mmu::Mmio;
use std::sync::Arc;

pub const POWEROFF_MAGIC: u32 = 0x706f6666; // poff
pub const REBOOT_MAGIC: u32 = 0x72656274; // rebt
pub const SUSPEND_MAGIC: u32 = 0x736c6570; // slep

pub struct Syscon {
    pub log: Log,
    pub hart: Arc<HartSignals>,
    pub start: u64,
    pub end: u64,
}

impl Syscon {
    pub fn new(hart: Arc<HartSignals>, start: u64, size: u64) -> Syscon {
        Syscon {
            log: Log::new("Syscon", Level::Error),
            hart,
            start,
            end: start + size,
        }
    }
}

impl Mmio for Syscon {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        paddr >= self.start && paddr + size <= self.end
    }

    fn load(&mut self, _now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        crate::log_debug!(self.log, "Syscon.Load{}({:x})", size * 8, paddr);
        Ok(0)
    }

    fn store(&mut self, _now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        crate::log_debug!(self.log, "Syscon.Store{}({:x},{:x})", size * 8, paddr, v);
        if size != 4 {
            self.hart.shutdown();
            return Ok(());
        }
        let v = v as u32;
        match paddr - self.start {
            0 => {
                if v != POWEROFF_MAGIC {
                    self.log.error(format_args!(
                        "invalid poweroff magic {:x}, expected {:x}",
                        v, POWEROFF_MAGIC
                    ));
                } else {
                    self.hart.shutdown();
                }
            }
            4 => {
                if v != REBOOT_MAGIC {
                    self.log.error(format_args!(
                        "invalid reboot magic {:x}, expected {:x}",
                        v, REBOOT_MAGIC
                    ));
                } else {
                    self.hart.shutdown();
                }
            }
            ofs => {
                self.log.error(format_args!("invalid offset {:x}", ofs));
            }
        }
        Ok(())
    }
}
