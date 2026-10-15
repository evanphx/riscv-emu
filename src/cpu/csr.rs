//! Control and Status Register access.

use super::Cpu;
use crate::isa::{self, misa, Exception, Mode, Mstatus, RegStatus, SSTATUS_MASK};
use crate::mmu::Satp;
use std::time::{SystemTime, UNIX_EPOCH};

/// Interrupt bits visible through sie/sip.
const S_INT_MASK: u64 = isa::INT_SSIP | isa::INT_STIP | isa::INT_SEIP;

/// Interrupt bits that can be delegated to S-mode.
const MIDELEG_MASK: u64 = S_INT_MASK;

/// Writable mstatus bits.
const MSTATUS_WRITE_MASK: u64 = (1 << isa::ms::SIE)
    | (1 << isa::ms::MIE)
    | (1 << isa::ms::SPIE)
    | (1 << isa::ms::MPIE)
    | (1 << isa::ms::SPP)
    | (3 << isa::ms::VS)
    | (3 << isa::ms::MPP)
    | (3 << isa::ms::FS)
    | (1 << isa::ms::MPRV)
    | (1 << isa::ms::SUM)
    | (1 << isa::ms::MXR)
    | (1 << isa::ms::TVM)
    | (1 << isa::ms::TW)
    | (1 << isa::ms::TSR);

fn random_u16() -> u16 {
    let mut buf = [0u8; 2];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    u16::from_le_bytes(buf)
}

impl Cpu {
    /// Reads the CSR. The `read` argument tells if this is an actual
    /// read (with possible side effects) or only an access check.
    pub fn csr_load(&mut self, csr: u16, raw: u32, read: bool) -> Result<u64, Exception> {
        if self.mode < isa::csr_privilege(csr) {
            return Err(self.illegal(raw));
        }

        let v = match csr {
            isa::CSR_FFLAGS | isa::CSR_FRM | isa::CSR_FCSR => {
                if self.mstatus.fs() == RegStatus::Off {
                    return Err(self.illegal(raw));
                }
                match csr {
                    isa::CSR_FFLAGS => self.fflags as u64,
                    isa::CSR_FRM => self.frm as u64,
                    _ => (self.frm << 5 | self.fflags) as u64,
                }
            }
            isa::CSR_MSTATUS => self.mstatus.0,
            isa::CSR_MISA => {
                misa::MXL
                    | misa::A
                    | misa::C
                    | misa::D
                    | misa::F
                    | misa::I
                    | misa::M
                    | misa::S
                    | misa::U
                    | misa::V
            }
            // Debug triggers.
            0x7a0..=0x7a4 => 0,
            isa::CSR_CYCLE | isa::CSR_MCYCLE => self.time,
            isa::CSR_TIME => self.time,
            // The instret counter is incremented when the instruction
            // is fetched; report the number of retired instructions
            // before this one.
            isa::CSR_INSTRET | isa::CSR_MINSTRET => self.instret.wrapping_sub(1),
            isa::CSR_MVENDORID => 0,
            isa::CSR_MARCHID => 0x100,
            isa::CSR_MIMPID => 0x1,
            isa::CSR_MHARTID => 0,
            isa::CSR_MCONFIGPTR => 0,
            isa::CSR_SCOUNTINHIBIT => 0,
            isa::CSR_SSTATUS => self.mstatus.0 & SSTATUS_MASK,
            isa::CSR_SIE => self.csr[isa::CSR_MIE as usize] & S_INT_MASK,
            isa::CSR_SIP => self.signals.mip() & S_INT_MASK,
            isa::CSR_MIP => self.signals.mip(),
            isa::CSR_SATP => {
                if self.mode == Mode::S && self.mstatus.tvm() {
                    return Err(self.illegal(raw));
                }
                self.satp.0
            }
            isa::CSR_VSTART => self.vpu.vstart,
            isa::CSR_VXSAT => self.vpu.vxsat as u64,
            isa::CSR_VXRM => self.vpu.vxrm as u64,
            isa::CSR_VCSR => (self.vpu.vxrm as u64) << 1 | self.vpu.vxsat as u64,
            isa::CSR_VL => self.vpu.vl,
            isa::CSR_VTYPE => self.vpu.vtype.0,
            isa::CSR_VLENB => self.vpu.vlen / 8,
            isa::CSR_SEED => {
                if !read {
                    0
                } else {
                    0x8000_0000 | random_u16() as u64
                }
            }
            isa::CSR_GOEMU_TIME => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            // Hardware performance monitoring counters.
            0xb03..=0xb1f | 0xc03..=0xc1f | 0x323..=0x33f => 0,
            _ => self.csr[csr as usize],
        };
        Ok(v)
    }

    /// Writes the CSR.
    pub fn csr_store(&mut self, csr: u16, v: u64, raw: u32) -> Result<(), Exception> {
        if self.mode < isa::csr_privilege(csr) || isa::csr_read_only(csr) {
            return Err(self.illegal(raw));
        }

        match csr {
            isa::CSR_FFLAGS | isa::CSR_FRM | isa::CSR_FCSR => {
                if self.mstatus.fs() == RegStatus::Off {
                    return Err(self.illegal(raw));
                }
                match csr {
                    isa::CSR_FFLAGS => self.fflags = (v & 0x1f) as u32,
                    isa::CSR_FRM => self.frm = (v & 0x7) as u32,
                    _ => {
                        self.fflags = (v & 0x1f) as u32;
                        self.frm = ((v >> 5) & 0x7) as u32;
                    }
                }
                self.set_fs_dirty();
            }
            isa::CSR_MSTATUS => {
                let mut ms = Mstatus((self.mstatus.0 & !MSTATUS_WRITE_MASK) | (v & MSTATUS_WRITE_MASK));
                // MPP cannot hold the reserved H mode.
                if ms.mpp() == Mode::H {
                    ms.set_mpp(Mode::U);
                }
                self.mstatus = ms;
                self.update_mstatus_sd();
                self.sync_mmu_state();
            }
            isa::CSR_MISA => {}
            isa::CSR_MIE => self.csr[csr as usize] = v,
            isa::CSR_MIP => {
                // Only the supervisor interrupt bits are writable.
                let mask = S_INT_MASK;
                let mip = self.signals.mip();
                self.signals.set_mip((mip & !mask) | (v & mask));
            }
            isa::CSR_MIDELEG => self.csr[csr as usize] = v & MIDELEG_MASK,
            isa::CSR_MEDELEG => self.csr[csr as usize] = v & 0xb3ff,
            isa::CSR_SSTATUS => {
                let mask = SSTATUS_MASK & MSTATUS_WRITE_MASK;
                self.mstatus = Mstatus((self.mstatus.0 & !mask) | (v & mask));
                self.update_mstatus_sd();
                self.sync_mmu_state();
            }
            isa::CSR_STIMECMP => {
                self.csr[csr as usize] = v;
                if self.time < v {
                    // Next timer interrupt in the future, clear
                    // interrupt.
                    self.signals.clear_interrupt(isa::INT_STIP);
                } else {
                    self.signals.set_interrupt(isa::INT_STIP);
                }
            }
            isa::CSR_SIE => {
                // sie is a masked view of mie.
                let mie = self.csr[isa::CSR_MIE as usize];
                self.csr[isa::CSR_MIE as usize] = (mie & !S_INT_MASK) | (v & S_INT_MASK);
            }
            isa::CSR_SIP => {
                // Only SSIP is writable through sip.
                let mask = isa::INT_SSIP;
                let mip = self.signals.mip();
                self.signals.set_mip((mip & !mask) | (v & mask));
            }
            isa::CSR_SATP => {
                if self.mode == Mode::S && self.mstatus.tvm() {
                    return Err(self.illegal(raw));
                }
                let satp = Satp(v);
                // Only Bare and Sv39 are supported; other modes leave
                // satp unchanged.
                let mode = satp.mode();
                if mode != crate::mmu::SATP_MODE_BARE && mode != crate::mmu::SATP_MODE_SV39 {
                    return Ok(());
                }
                self.set_satp(satp);
                if self.trace {
                    self.trace_func(self.pc);
                    log::info(&format!("Satp: {}", satp));
                }
            }
            isa::CSR_VSTART => self.vpu.vstart = v,
            isa::CSR_VXSAT => self.vpu.vxsat = v & 1 != 0,
            isa::CSR_VXRM => self.vpu.vxrm = (v & 3) as u8,
            isa::CSR_VCSR => {
                self.vpu.vxsat = v & 1 != 0;
                self.vpu.vxrm = ((v >> 1) & 3) as u8;
            }
            isa::CSR_MCYCLE => self.time = v,
            isa::CSR_MINSTRET => self.instret = v,
            isa::CSR_GOEMU_DEBUG => {
                self.debug_trace = v & 1 != 0;
                self.csr[csr as usize] = v;
            }
            isa::CSR_GOEMU_CPU_PROFILE => {
                // Host profiling is not supported; accept the write.
                self.csr[csr as usize] = v;
            }
            isa::CSR_SEED => {}
            0xb03..=0xb1f | 0x323..=0x33f => {}
            _ => self.csr[csr as usize] = v,
        }
        Ok(())
    }
}

mod log {
    pub fn info(msg: &str) {
        eprintln!("{}", msg);
    }
}
