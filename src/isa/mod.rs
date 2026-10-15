//! RISC-V Instruction Set Architecture definitions: opcodes, decoded
//! instructions, privilege modes, status registers, CSR numbers, and
//! trap causes.

mod csr;
mod decode;
pub mod elf;
mod instr;
mod trap;

pub use csr::*;
pub use decode::*;
pub use instr::*;
pub use trap::*;

/// Processor extension flags for the `misa` register.
pub mod misa {
    pub const A: u64 = 1 << 0;
    pub const B: u64 = 1 << 1;
    pub const C: u64 = 1 << 2;
    pub const D: u64 = 1 << 3;
    pub const E: u64 = 1 << 4;
    pub const F: u64 = 1 << 5;
    pub const G: u64 = 1 << 6;
    pub const H: u64 = 1 << 7;
    pub const I: u64 = 1 << 8;
    pub const M: u64 = 1 << 12;
    pub const S: u64 = 1 << 18;
    pub const U: u64 = 1 << 20;
    pub const V: u64 = 1 << 21;
    pub const MXL: u64 = 2 << 62;
}

/// CPU privilege modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Mode {
    U = 0,
    S = 1,
    H = 2,
    M = 3,
}

impl Mode {
    pub fn from_bits(v: u64) -> Mode {
        match v & 3 {
            0 => Mode::U,
            1 => Mode::S,
            2 => Mode::H,
            _ => Mode::M,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Mode::U => "U",
            Mode::S => "S",
            Mode::H => "H",
            Mode::M => "M",
        };
        f.write_str(s)
    }
}

/// Bit positions in the machine status register.
pub mod ms {
    pub const SIE: u32 = 1;
    pub const MIE: u32 = 3;
    pub const SPIE: u32 = 5;
    pub const UBE: u32 = 6;
    pub const MPIE: u32 = 7;
    pub const SPP: u32 = 8;
    pub const VS: u32 = 9;
    pub const MPP: u32 = 11;
    pub const FS: u32 = 13;
    pub const XS: u32 = 15;
    pub const MPRV: u32 = 17;
    pub const SUM: u32 = 18;
    pub const MXR: u32 = 19;
    pub const TVM: u32 = 20;
    pub const TW: u32 = 21;
    pub const TSR: u32 = 22;
    pub const SPELP: u32 = 23;
    pub const SDT: u32 = 24;
    pub const UXL: u32 = 32;
    pub const SXL: u32 = 34;
    pub const SBE: u32 = 36;
    pub const MBE: u32 = 37;
    pub const GVA: u32 = 38;
    pub const MPV: u32 = 39;
    pub const MPELP: u32 = 41;
    pub const MDT: u32 = 42;
    pub const SD: u32 = 63;
}

/// Register status used by the floating point and vector extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RegStatus {
    Off = 0,
    Initial = 1,
    Clean = 2,
    Dirty = 3,
}

impl RegStatus {
    fn from_bits(v: u64) -> RegStatus {
        match v & 3 {
            0 => RegStatus::Off,
            1 => RegStatus::Initial,
            2 => RegStatus::Clean,
            _ => RegStatus::Dirty,
        }
    }
}

/// The machine status register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mstatus(pub u64);

/// The sstatus bits that are stored in mstatus.
pub const SSTATUS_MASK: u64 = (1 << ms::SIE)
    | (1 << ms::SPIE)
    | (1 << ms::UBE)
    | (1 << ms::SPP)
    | (3 << ms::VS)
    | (3 << ms::FS)
    | (3 << ms::XS)
    | (1 << ms::SUM)
    | (1 << ms::MXR)
    | (1 << ms::SPELP)
    | (1 << ms::SDT)
    | (3 << ms::UXL)
    | (1 << ms::SD);

/// The mstatus bits that are constant.
pub const MCONST_MASK: u64 = 3 << ms::UXL;

macro_rules! flag {
    ($get:ident, $set:ident, $bit:expr) => {
        #[inline(always)]
        pub fn $get(self) -> bool {
            self.0 & (1 << $bit) != 0
        }
        #[inline(always)]
        pub fn $set(&mut self, v: bool) {
            if v {
                self.0 |= 1 << $bit;
            } else {
                self.0 &= !(1 << $bit);
            }
        }
    };
}

impl Mstatus {
    flag!(sie, set_sie, ms::SIE);
    flag!(mie, set_mie, ms::MIE);
    flag!(spie, set_spie, ms::SPIE);
    flag!(mpie, set_mpie, ms::MPIE);
    flag!(mprv, set_mprv, ms::MPRV);
    flag!(sum, set_sum, ms::SUM);
    flag!(mxr, set_mxr, ms::MXR);
    flag!(tvm, set_tvm, ms::TVM);
    flag!(tw, set_tw, ms::TW);
    flag!(tsr, set_tsr, ms::TSR);
    flag!(sd, set_sd, ms::SD);

    pub fn spp(self) -> Mode {
        Mode::from_bits((self.0 >> ms::SPP) & 1)
    }

    pub fn set_spp(&mut self, mode: Mode) {
        assert!(mode <= Mode::S, "set_spp: invalid mode");
        self.0 &= !(1 << ms::SPP);
        self.0 |= ((mode as u64) & 1) << ms::SPP;
    }

    pub fn mpp(self) -> Mode {
        Mode::from_bits((self.0 >> ms::MPP) & 3)
    }

    pub fn set_mpp(&mut self, mode: Mode) {
        self.0 &= !(3 << ms::MPP);
        self.0 |= ((mode as u64) & 3) << ms::MPP;
    }

    pub fn vs(self) -> RegStatus {
        RegStatus::from_bits(self.0 >> ms::VS)
    }

    pub fn set_vs(&mut self, s: RegStatus) {
        self.0 &= !(3 << ms::VS);
        self.0 |= (s as u64) << ms::VS;
    }

    pub fn fs(self) -> RegStatus {
        RegStatus::from_bits(self.0 >> ms::FS)
    }

    pub fn set_fs(&mut self, s: RegStatus) {
        self.0 &= !(3 << ms::FS);
        self.0 |= (s as u64) << ms::FS;
    }
}

/// Selected element width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sew(pub u32);

impl Sew {
    pub const E8: Sew = Sew(8);
    pub const E16: Sew = Sew(16);
    pub const E32: Sew = Sew(32);
    pub const E64: Sew = Sew(64);

    /// Element length in bytes.
    pub fn bytes(self) -> u64 {
        self.0 as u64 / 8
    }
}

/// The vector type register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VType(pub u64);

impl VType {
    /// The group multiplier (LMUL).
    pub fn vlmul(self) -> f32 {
        match self.0 & 0b111 {
            0b000 => 1.0,
            0b001 => 2.0,
            0b010 => 4.0,
            0b011 => 8.0,
            0b111 => 0.5,
            0b110 => 0.25,
            0b101 => 0.125,
            _ => 1.0,
        }
    }

    /// The selected element width (SEW).
    pub fn vsew(self) -> Sew {
        Sew(8 << ((self.0 >> 3) & 0b111))
    }

    pub fn vta(self) -> bool {
        self.0 & (1 << 6) != 0
    }

    pub fn vma(self) -> bool {
        self.0 & (1 << 7) != 0
    }

    pub fn altfmt(self) -> bool {
        self.0 & (1 << 8) != 0
    }
}

impl std::fmt::Display for VType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lmul = match self.0 & 0b111 {
            0b000 => "m1",
            0b001 => "m2",
            0b010 => "m4",
            0b011 => "m8",
            0b111 => "mf2",
            0b110 => "mf4",
            0b101 => "mf8",
            _ => "reserved",
        };
        write!(f, "e{},{}", self.vsew().0, lmul)?;
        if self.vta() {
            f.write_str(",ta")?;
        }
        if self.vma() {
            f.write_str(",ma")?;
        }
        Ok(())
    }
}
