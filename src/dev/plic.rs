//! Platform-Level Interrupt Controller (PLIC).

use crate::cpu::HartSignals;
use crate::isa::{self, Exception};
use crate::mmu::Mmio;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Maximum number of interrupt sources supported.
pub const PLIC_MAX_INTERRUPTS: u32 = 63;

/// Maximum number of contexts supported by the PLIC specification.
pub const PLIC_MAX_CONTEXTS: usize = 15872;

/// PLIC memory map size.
pub const PLIC_SIZE: u64 = 0x400000;

#[derive(Clone, Copy, Default)]
struct Gateway {
    priority: u32,
    asserted: bool,
    inflight: bool,
}

struct PlicState {
    harts: Vec<Arc<HartSignals>>,
    num_contexts: usize,
    gateways: [Gateway; PLIC_MAX_INTERRUPTS as usize + 1],
    pending: u64,
    enables: Vec<u64>,
    thresholds: Vec<u32>,
    irqs: HashMap<u32, String>,
}

/// The PLIC. Cloning returns a new handle to the same controller.
#[derive(Clone)]
pub struct Plic {
    state: Arc<Mutex<PlicState>>,
    pub start: u64,
    pub end: u64,
    pub max_interrupts: u32,
}

impl Plic {
    pub fn new(harts: Vec<Arc<HartSignals>>, start: u64) -> Plic {
        let num_contexts = harts.len() * 2;
        assert!(
            num_contexts <= PLIC_MAX_CONTEXTS,
            "maximum number of contexts exceeded"
        );
        Plic {
            state: Arc::new(Mutex::new(PlicState {
                harts,
                num_contexts,
                gateways: [Gateway::default(); PLIC_MAX_INTERRUPTS as usize + 1],
                pending: 0,
                enables: vec![0; num_contexts],
                thresholds: vec![0; num_contexts],
                irqs: HashMap::new(),
            })),
            start,
            end: start + PLIC_SIZE,
            max_interrupts: PLIC_MAX_INTERRUPTS,
        }
    }

    /// Registers a name for the interrupt source.
    pub fn register_irq(&self, irq: u32, name: &str) {
        self.state
            .lock()
            .unwrap()
            .irqs
            .insert(irq, name.to_string());
    }

    /// Returns the registered name of the interrupt source.
    pub fn irq_name(&self, irq: u32) -> String {
        let s = self.state.lock().unwrap();
        format!("{}[{}]", irq, s.irqs.get(&irq).cloned().unwrap_or_default())
    }

    /// Resets the interrupt source.
    pub fn reset(&self, irq: u32) {
        let mut s = self.state.lock().unwrap();
        let g = &mut s.gateways[irq as usize];
        g.asserted = false;
        g.inflight = false;
        s.pending &= !(1 << irq);
    }

    /// Sets the level of the interrupt request line.
    pub fn set_interrupt_request(&self, irq: u32, set: bool) {
        let mut s = self.state.lock().unwrap();
        let old = s.gateways[irq as usize].asserted;
        s.gateways[irq as usize].asserted = set;
        if !old && set && !s.gateways[irq as usize].inflight {
            s.pending |= 1 << irq;
        }
        s.reevaluate();
    }
}

impl PlicState {
    fn claim(&mut self, ctx: usize) -> u32 {
        let mut highest = 0;
        let mut claimed = 0;
        for src in 1..=PLIC_MAX_INTERRUPTS {
            let pending = self.pending & (1 << src) != 0;
            let enabled = self.enables[ctx] & (1 << src) != 0;
            if pending && enabled {
                let prio = self.gateways[src as usize].priority;
                if prio > self.thresholds[ctx] && prio > highest {
                    highest = prio;
                    claimed = src;
                }
            }
        }
        if claimed != 0 {
            self.pending &= !(1 << claimed);
            self.gateways[claimed as usize].inflight = true;
            self.reevaluate();
        }
        claimed
    }

    fn complete(&mut self, src: u32) {
        if src == 0 || src > PLIC_MAX_INTERRUPTS {
            return;
        }
        let g = &mut self.gateways[src as usize];
        if !g.inflight {
            return;
        }
        g.inflight = false;
        if g.asserted {
            self.pending |= 1 << src;
        }
        self.reevaluate();
    }

    fn reevaluate(&mut self) {
        for ctx in 0..self.num_contexts {
            let mut signaled = false;
            for src in 1..=PLIC_MAX_INTERRUPTS {
                let pending = self.pending & (1 << src) != 0;
                let enabled = self.enables[ctx] & (1 << src) != 0;
                if pending && enabled && self.gateways[src as usize].priority > self.thresholds[ctx]
                {
                    signaled = true;
                    break;
                }
            }
            let hart = &self.harts[ctx / 2];
            let int = if ctx % 2 == 0 {
                isa::INT_MEIP
            } else {
                isa::INT_SEIP
            };
            if signaled {
                hart.set_interrupt(int);
            } else {
                hart.clear_interrupt(int);
            }
        }
    }
}

impl Mmio for Plic {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        paddr >= self.start && paddr + size <= self.end
    }

    fn load(&mut self, _now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        if size != 4 {
            return Err(Exception::fatal(format!(
                "PLIC: Load{}(0x{:x})",
                size * 8,
                paddr
            )));
        }
        let mut s = self.state.lock().unwrap();
        let offset = paddr - self.start;
        let v = match offset {
            0x000000..=0x000080 => s.gateways[(offset / 4) as usize].priority,
            0x001000 => s.pending as u32,
            0x001004 => (s.pending >> 32) as u32,
            0x002000..=0x002084 => {
                let ctx = ((offset - 0x2000) / 0x80) as usize;
                if ctx < s.enables.len() {
                    match (offset - 0x2000) % 0x80 {
                        0 => s.enables[ctx] as u32,
                        4 => (s.enables[ctx] >> 32) as u32,
                        _ => 0,
                    }
                } else {
                    0
                }
            }
            o if o >= 0x200000 => {
                let ctx = ((o - 0x200000) / 0x1000) as usize;
                let reg = (o - 0x200000) % 0x1000;
                if ctx < s.thresholds.len() {
                    match reg {
                        0 => s.thresholds[ctx],
                        // Reading the claim register claims the highest
                        // priority pending interrupt.
                        4 => s.claim(ctx),
                        _ => 0,
                    }
                } else {
                    0
                }
            }
            _ => 0,
        };
        Ok(v as u64)
    }

    fn store(&mut self, _now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        if size != 4 {
            return Err(Exception::fatal(format!(
                "PLIC: Store{}(0x{:x}, 0x{:x})",
                size * 8,
                paddr,
                v
            )));
        }
        let val = v as u32;
        let mut s = self.state.lock().unwrap();
        let offset = paddr - self.start;
        match offset {
            0x000000..=0x000080 => {
                let src = offset / 4;
                if src <= PLIC_MAX_INTERRUPTS as u64 {
                    s.gateways[src as usize].priority = val;
                }
            }
            0x002000..=0x002084 => {
                let ctx = ((offset - 0x2000) / 0x80) as usize;
                if ctx < s.enables.len() {
                    let old = s.enables[ctx];
                    match (offset - 0x2000) % 0x80 {
                        0 => s.enables[ctx] = (old & 0xffff_ffff_0000_0000) | val as u64,
                        4 => s.enables[ctx] = (old & 0xffff_ffff) | (val as u64) << 32,
                        _ => {}
                    }
                }
            }
            o if o >= 0x200000 => {
                let ctx = ((o - 0x200000) / 0x1000) as usize;
                let reg = (o - 0x200000) % 0x1000;
                if ctx < s.num_contexts {
                    match reg {
                        0 => {
                            s.thresholds[ctx] = val;
                            s.reevaluate();
                        }
                        // Writing the claim register completes the
                        // interrupt.
                        4 => s.complete(val),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
