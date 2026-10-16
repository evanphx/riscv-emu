//! NS16550A UART.

use super::Plic;
use crate::isa::Exception;
use crate::logger::{Level, Log};
use crate::mmu::Mmio;
use crate::snapshot::{StateReader, StateWriter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// UART MMIO size.
pub const UART_SIZE: u64 = 256;

// Read registers.
const RREG_RBR: u64 = 0;
const RREG_IER: u64 = 1;
const RREG_IIR: u64 = 2;
const RREG_LCR: u64 = 3;
const RREG_MCR: u64 = 4;
const RREG_LSR: u64 = 5;
const RREG_MSR: u64 = 6;
const RREG_SCR: u64 = 7;

// Write registers.
const WREG_THR: u64 = 0;
const WREG_IER: u64 = 1;
const WREG_FCR: u64 = 2;
const WREG_LCR: u64 = 3;
const WREG_MCR: u64 = 4;
const WREG_SCR: u64 = 7;

pub const LCR_DLAB_BIT: u8 = 0b1000_0000;

pub const MSR_CTS: u8 = 1 << 4;
pub const MSR_DSR: u8 = 1 << 5;
pub const MSR_RI: u8 = 1 << 6;
pub const MSR_DCD: u8 = 1 << 7;

const IER_BITS: [&str; 8] = [
    "Received data available",
    "Transmitter holding register empty",
    "Receiver line status register change",
    "Modem status register change",
    "Sleep mode (16750 only)",
    "Low power mode (16750 only)",
    "reserved",
    "reserved",
];

/// The UART peer provides the data stream behind the UART.
pub trait UartPeer: Send + Sync {
    /// Runs the peer input loop. The peer calls
    /// `uart.input_available()` when it has input data.
    fn run(&self, uart: Uart);
    /// Halts the peer.
    fn halt(&self) -> Result<(), String>;
    /// Receives the next input byte. Returns the byte and a flag
    /// telling if more input is available.
    fn receive(&self) -> (u8, bool);
    /// Sends the byte to the peer.
    fn send(&self, b: u8);
}

#[derive(Default)]
struct Regs {
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    /// Bit 0: receiver data available, bit 1: THR empty.
    isr_pending: u8,
}

/// A 16550A UART. Cloning returns a new handle to the same device.
#[derive(Clone)]
pub struct Uart {
    pub log: Log,
    pub start: u64,
    pub end: u64,
    pub plic: Plic,
    pub irq: u32,
    pub peer: Arc<dyn UartPeer>,
    regs: Arc<Mutex<Regs>>,
    input_avail: Arc<AtomicBool>,
}

impl Uart {
    pub fn new(name: &str, start: u64, plic: Plic, irq: u32, peer: Arc<dyn UartPeer>) -> Uart {
        plic.register_irq(irq, "UART");
        Uart {
            log: Log::new(name, Level::Error),
            start,
            end: start + UART_SIZE,
            plic,
            irq,
            peer,
            regs: Arc::new(Mutex::new(Regs {
                // Divisor 13 matches 115200 baud with a 24MHz clock.
                dll: 13,
                dlm: 0,
                // 8 bits, no parity, 1 stop bit.
                lcr: 0x03,
                ..Default::default()
            })),
            input_avail: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts the peer input loop on a new thread.
    pub fn start_peer(&self) {
        let uart = self.clone();
        let peer = self.peer.clone();
        std::thread::Builder::new()
            .name(format!("uart-{}", self.log.name))
            .spawn(move || peer.run(uart))
            .expect("failed to start UART peer");
    }

    /// Signals that the peer has input available.
    pub fn input_available(&self) {
        let mut r = self.regs.lock().unwrap();
        if r.ier & 0x01 != 0 {
            r.isr_pending |= 0x01;
            self.plic.set_interrupt_request(self.irq, true);
        }
        self.input_avail.store(true, Ordering::SeqCst);
    }

    fn check_interrupts(&self, r: &mut Regs) {
        // Transmitter holding register empty interrupt.
        if r.ier & 0x02 != 0 {
            r.isr_pending |= 0x02;
            self.plic.set_interrupt_request(self.irq, true);
        }
    }

    fn load8(&mut self, reg: u64) -> u8 {
        crate::log_debug!(self.log, "Load8({})", reg);
        match reg {
            RREG_RBR => {
                if self.regs.lock().unwrap().lcr & LCR_DLAB_BIT != 0 {
                    return self.regs.lock().unwrap().dll;
                }
                let (v, more) = self.peer.receive();
                let mut r = self.regs.lock().unwrap();
                if !more {
                    self.input_avail.store(false, Ordering::SeqCst);
                    r.isr_pending &= !0x01;
                    if r.isr_pending == 0 {
                        self.plic.set_interrupt_request(self.irq, false);
                    }
                }
                v
            }
            RREG_IER => {
                let r = self.regs.lock().unwrap();
                if r.lcr & LCR_DLAB_BIT != 0 {
                    r.dlm
                } else {
                    r.ier
                }
            }
            RREG_IIR => {
                let mut r = self.regs.lock().unwrap();
                // FIFO enabled.
                let mut iir = 0xc0u8;
                if r.isr_pending & 0x02 != 0 {
                    iir |= 0x02;
                    // Reading IIR clears the THRE interrupt.
                    r.isr_pending &= !0x02;
                    if r.isr_pending == 0 {
                        self.plic.set_interrupt_request(self.irq, false);
                    }
                    return iir;
                }
                if self.input_avail.load(Ordering::SeqCst) {
                    return iir | 0x04;
                }
                // No interrupt pending.
                iir | 0x01
            }
            RREG_LCR => self.regs.lock().unwrap().lcr,
            RREG_MCR => self.regs.lock().unwrap().mcr,
            RREG_LSR => {
                // THR empty and line idle.
                let mut status = 0x60u8;
                if self.input_avail.load(Ordering::SeqCst) {
                    status |= 0x01;
                }
                status
            }
            RREG_MSR => MSR_CTS | MSR_DSR | MSR_DCD,
            RREG_SCR => self.regs.lock().unwrap().scr,
            _ => 0,
        }
    }

    fn store8(&mut self, reg: u64, v: u8) {
        crate::log_debug!(self.log, "Store8({}, 0x{:02x})", reg, v);
        let mut r = self.regs.lock().unwrap();
        match reg {
            WREG_THR => {
                if r.lcr & LCR_DLAB_BIT != 0 {
                    r.dll = v;
                    return;
                }
                self.peer.send(v);
                self.check_interrupts(&mut r);
            }
            WREG_IER => {
                if r.lcr & LCR_DLAB_BIT != 0 {
                    r.dlm = v;
                    let divisor = ((r.dlm as u32) << 8) + r.dll as u32;
                    if divisor != 0 {
                        crate::log_info!(self.log, "Baud Rate {}", 24_000_000 / (16 * divisor));
                    }
                    return;
                }
                if v != 0 && self.log.enabled(Level::Info) {
                    self.log
                        .info(format_args!("IER: Interrupt Enable Register store:"));
                    for (i, d) in IER_BITS.iter().enumerate() {
                        if v & (1 << i) != 0 {
                            self.log.info(format_args!("  {}: {}", i, d));
                        }
                    }
                }
                r.ier = v;
                self.check_interrupts(&mut r);
            }
            WREG_FCR => r.fcr = v,
            WREG_LCR => r.lcr = v,
            WREG_MCR => r.mcr = v,
            WREG_SCR => r.scr = v,
            _ => {}
        }
    }
}

impl Mmio for Uart {
    fn contains(&self, paddr: u64, size: u64) -> bool {
        paddr >= self.start && paddr + size <= self.end
    }

    fn load(&mut self, _now: u64, paddr: u64, size: u64) -> Result<u64, Exception> {
        if size != 1 {
            return Ok(0);
        }
        Ok(self.load8(paddr - self.start) as u64)
    }

    fn store(&mut self, _now: u64, paddr: u64, size: u64, v: u64) -> Result<(), Exception> {
        if size != 1 {
            eprintln!("UART.store{}: {:x} = {}", size * 8, paddr, v);
            return Ok(());
        }
        self.store8(paddr - self.start, v as u8);
        Ok(())
    }

    fn halt(&mut self) -> Result<(), String> {
        self.peer.halt()
    }

    fn save_state(&mut self, w: &mut StateWriter) -> Result<(), String> {
        let r = self.regs.lock().unwrap();
        for v in [
            r.ier,
            r.fcr,
            r.lcr,
            r.mcr,
            r.scr,
            r.dll,
            r.dlm,
            r.isr_pending,
        ] {
            w.u8(v);
        }
        Ok(())
    }

    fn load_state(&mut self, rd: &mut StateReader) -> Result<(), String> {
        let mut guard = self.regs.lock().unwrap();
        let r = &mut *guard;
        for v in [
            &mut r.ier,
            &mut r.fcr,
            &mut r.lcr,
            &mut r.mcr,
            &mut r.scr,
            &mut r.dll,
            &mut r.dlm,
            &mut r.isr_pending,
        ] {
            *v = rd.u8()?;
        }
        // Host input that was not read before the snapshot is lost, so
        // the receive interrupt is no longer pending.
        r.isr_pending &= !0x01;
        self.input_avail.store(false, Ordering::SeqCst);
        self.plic
            .set_interrupt_request(self.irq, r.isr_pending != 0);
        Ok(())
    }
}
