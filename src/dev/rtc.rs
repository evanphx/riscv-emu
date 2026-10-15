//! Google Goldfish real-time clock.

use crate::isa::Exception;
use crate::mmu::Mmio;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct GoldfishRtc {
    pub start: u64,
    pub end: u64,
    /// TIME_HIGH latched when TIME_LOW is read.
    high: u32,
}

impl GoldfishRtc {
    pub fn new(start: u64, size: u64) -> GoldfishRtc {
        GoldfishRtc {
            start,
            end: start + size,
            high: 0,
        }
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

impl Mmio for GoldfishRtc {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        paddr >= self.start && paddr + size <= self.end
    }

    fn load(&mut self, _now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        if size != 4 {
            return Ok(0);
        }
        match paddr - self.start {
            0x00 => {
                // TIME_LOW
                let now = now_nanos();
                self.high = (now >> 32) as u32;
                Ok(now & 0xffff_ffff)
            }
            0x04 => Ok(self.high as u64), // TIME_HIGH
            _ => Ok(0),
        }
    }

    fn store(&mut self, _now: u64, _paddr: u64, _size: u64, _v: u64) -> Result<(), Exception> {
        Ok(())
    }
}
