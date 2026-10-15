//! Trap and interrupt handling.

use super::Cpu;
use crate::isa::{self, Mode, Trap, CAUSE_INTERRUPT};
use crate::memory::INVALID_PAGENUM;

impl Cpu {
    /// Raises the exception at the current PC. If a trap handler is
    /// installed, it gets the first chance to handle the trap. Returns
    /// an error if the trap could not be handled.
    pub fn raise(&mut self, cause: u64, tval: u64) -> Result<(), String> {
        self.reservation_valid = false;
        self.code_vpn = INVALID_PAGENUM;

        if let Some(mut handler) = self.trap_handler.take() {
            let trap = Trap::new(self.pc, cause, tval);
            let r = handler(self, &trap);
            self.trap_handler = Some(handler);
            // The handler may have changed the address space.
            self.code_vpn = INVALID_PAGENUM;
            return match r {
                Ok(true) => Ok(()),
                Ok(false) => Err(format!("unhandled trap: {}", trap)),
                Err(e) => Err(e),
            };
        }

        self.take_trap(self.pc, cause, tval);
        Ok(())
    }

    /// Takes the exception: updates the trap CSRs and moves PC to the
    /// trap vector. The target mode is determined by medeleg.
    pub fn take_trap(&mut self, epc: u64, cause: u64, tval: u64) {
        let medeleg = self.csr[isa::CSR_MEDELEG as usize];
        let target = if self.mode == Mode::M || cause >= 64 || medeleg & (1 << cause) == 0 {
            Mode::M
        } else {
            Mode::S
        };
        self.enter_trap(target, epc, cause, tval);
    }

    /// Takes the interrupt with the cause number to the target mode.
    pub fn interrupt(&mut self, target: Mode, cause: u64) {
        let epc = self.pc;
        self.reservation_valid = false;
        self.enter_trap(target, epc, cause | CAUSE_INTERRUPT, 0);
    }

    fn enter_trap(&mut self, target: Mode, epc: u64, cause: u64, tval: u64) {
        let mut ms = self.mstatus;
        let tvec = match target {
            Mode::M => {
                ms.set_mpp(self.mode);
                ms.set_mpie(ms.mie());
                ms.set_mie(false);
                self.csr[isa::CSR_MEPC as usize] = epc;
                self.csr[isa::CSR_MCAUSE as usize] = cause;
                self.csr[isa::CSR_MTVAL as usize] = tval;
                self.csr[isa::CSR_MTVEC as usize]
            }
            Mode::S => {
                ms.set_spp(self.mode);
                ms.set_spie(ms.sie());
                ms.set_sie(false);
                self.csr[isa::CSR_SEPC as usize] = epc;
                self.csr[isa::CSR_SCAUSE as usize] = cause;
                self.csr[isa::CSR_STVAL as usize] = tval;
                self.csr[isa::CSR_STVEC as usize]
            }
            _ => panic!("invalid trap target {}", target),
        };
        self.mstatus = ms;
        self.set_mode(target);
        self.sync_mmu_state();
        self.code_vpn = INVALID_PAGENUM;

        let base = tvec & !0x3;
        if tvec & 0x3 == 1 && cause & CAUSE_INTERRUPT != 0 {
            self.pc = base + (cause & !CAUSE_INTERRUPT) * 4;
        } else {
            self.pc = base;
        }
    }
}
