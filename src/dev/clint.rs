//! Core-Local Interruptor (CLINT): machine software interrupts and
//! the machine timer.

use crate::cpu::HartSignals;
use crate::isa::{self, Exception};
use crate::mmu::Mmio;
use crate::snapshot::{StateReader, StateWriter};
use std::sync::Arc;

pub const CLINT_OFS_MSIP: u64 = 0;
pub const CLINT_OFS_MTIMECMP: u64 = 0x4000;
pub const CLINT_OFS_MTIME: u64 = 0xbff8;

pub struct Clint {
    pub hart: Arc<HartSignals>,
    pub start: u64,
    pub end: u64,
    pub msip: u64,
    pub mtimecmp: u64,
    pub mtime: u64,
}

impl Clint {
    pub fn new(hart: Arc<HartSignals>, start: u64, size: u64) -> Clint {
        Clint {
            hart,
            start,
            end: start + size,
            msip: 0,
            mtimecmp: 0,
            mtime: 0,
        }
    }

    /// Updates the machine timer interrupt for the current time.
    pub fn tick(&mut self, now: u64) {
        self.mtime = now;
        if self.mtime >= self.mtimecmp {
            self.hart.set_interrupt(isa::INT_MTIP);
        } else {
            self.hart.clear_interrupt(isa::INT_MTIP);
        }
    }
}

fn update_half(current: u64, offset: u64, val: u64) -> u64 {
    if offset & 4 != 0 {
        (current & 0xffff_ffff) | (val << 32)
    } else {
        (current & 0xffff_ffff_0000_0000) | (val & 0xffff_ffff)
    }
}

impl Mmio for Clint {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        paddr >= self.start && paddr + size <= self.end
    }

    fn load(&mut self, now: u64, paddr: u64, _size: u64) -> Result<u64, Exception> {
        let ofs = paddr - self.start;
        let v = match ofs {
            CLINT_OFS_MSIP => self.msip,
            CLINT_OFS_MTIMECMP => self.mtimecmp,
            CLINT_OFS_MTIME => {
                self.mtime = now;
                self.mtime
            }
            o if (CLINT_OFS_MTIMECMP..CLINT_OFS_MTIMECMP + 8).contains(&o) => {
                if o & 4 != 0 {
                    self.mtimecmp >> 32
                } else {
                    self.mtimecmp
                }
            }
            o if (CLINT_OFS_MTIME..CLINT_OFS_MTIME + 8).contains(&o) => {
                self.mtime = now;
                if o & 4 != 0 {
                    self.mtime >> 32
                } else {
                    self.mtime
                }
            }
            o => {
                eprintln!("CLINT: load: unknown register {:x}", o);
                0
            }
        };
        Ok(v)
    }

    fn store(&mut self, now: u64, paddr: u64, _size: u64, v: u64) -> Result<(), Exception> {
        let ofs = paddr - self.start;
        match ofs {
            CLINT_OFS_MSIP => {
                self.msip = v;
                if v & 1 != 0 {
                    self.hart.set_interrupt(isa::INT_MSIP);
                } else {
                    self.hart.clear_interrupt(isa::INT_MSIP);
                }
            }
            CLINT_OFS_MTIMECMP => {
                self.mtimecmp = v;
                self.tick(now);
            }
            CLINT_OFS_MTIME => {
                self.mtime = v;
                self.tick(v);
            }
            o if (CLINT_OFS_MTIMECMP..CLINT_OFS_MTIMECMP + 8).contains(&o) => {
                self.mtimecmp = update_half(self.mtimecmp, o, v);
                self.tick(now);
            }
            o if (CLINT_OFS_MTIME..CLINT_OFS_MTIME + 8).contains(&o) => {
                self.mtime = update_half(self.mtime, o, v);
                self.tick(self.mtime);
            }
            o => eprintln!("CLINT: store: unknown register {:x} = {:x}", o, v),
        }
        Ok(())
    }

    fn save_state(&mut self, w: &mut StateWriter) -> Result<(), String> {
        w.u64(self.msip);
        w.u64(self.mtimecmp);
        w.u64(self.mtime);
        Ok(())
    }

    fn load_state(&mut self, r: &mut StateReader) -> Result<(), String> {
        self.msip = r.u64()?;
        self.mtimecmp = r.u64()?;
        self.mtime = r.u64()?;
        Ok(())
    }
}
