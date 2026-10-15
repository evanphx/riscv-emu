//! Exception and interrupt causes.

use std::fmt;

pub const CAUSE_INST_ADDR_MISALIGNED: u64 = 0;
pub const CAUSE_INST_ACCESS_FAULT: u64 = 1;
pub const CAUSE_ILLEGAL_INSTR: u64 = 2;
pub const CAUSE_BREAKPOINT: u64 = 3;
pub const CAUSE_LOAD_ADDR_MISALIGNED: u64 = 4;
pub const CAUSE_LOAD_ACCESS_FAULT: u64 = 5;
pub const CAUSE_STORE_ADDR_MISALIGNED: u64 = 6;
pub const CAUSE_STORE_ACCESS_FAULT: u64 = 7;
pub const CAUSE_ECALL_U: u64 = 8;
pub const CAUSE_ECALL_S: u64 = 9;
pub const CAUSE_ECALL_VS: u64 = 10;
pub const CAUSE_ECALL_M: u64 = 11;
pub const CAUSE_INST_PAGE_FAULT: u64 = 12;
pub const CAUSE_LOAD_PAGE_FAULT: u64 = 13;
pub const CAUSE_STORE_PAGE_FAULT: u64 = 15;
pub const CAUSE_DOUBLE_TRAP: u64 = 16;
pub const CAUSE_SOFTWARE_CHECK: u64 = 18;
pub const CAUSE_HARDWARE_ERROR: u64 = 19;

/// The interrupt bit of the cause register.
pub const CAUSE_INTERRUPT: u64 = 1 << 63;

pub const INT_USIP: u64 = 1 << 0;
pub const INT_SSIP: u64 = 1 << 1;
pub const INT_MSIP: u64 = 1 << 3;
pub const INT_UTIP: u64 = 1 << 4;
pub const INT_STIP: u64 = 1 << 5;
pub const INT_MTIP: u64 = 1 << 7;
pub const INT_UEIP: u64 = 1 << 8;
pub const INT_SEIP: u64 = 1 << 9;
pub const INT_MEIP: u64 = 1 << 11;

/// Returns a description of the pending interrupts in v.
pub fn int_string(v: u64) -> String {
    let names = [
        (INT_MEIP, "MEIP"),
        (INT_SEIP, "SEIP"),
        (INT_UEIP, "UEIP"),
        (INT_MTIP, "MTIP"),
        (INT_STIP, "STIP"),
        (INT_UTIP, "UTIP"),
        (INT_MSIP, "MSIP"),
        (INT_SSIP, "SSIP"),
        (INT_USIP, "USIP"),
    ];
    names
        .iter()
        .filter(|(bit, _)| v & bit != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(",")
}

fn exception_name(cause: u64) -> Option<&'static str> {
    Some(match cause {
        0 => "Instruction address misaligned",
        1 => "Instruction access fault",
        2 => "Illegal instruction",
        3 => "Breakpoint",
        4 => "Load address misaligned",
        5 => "Load access fault",
        6 => "Store/AMO address misaligned",
        7 => "Store/AMO access fault",
        8 => "Environment call from U-mode",
        9 => "Environment call from S-mode",
        10 => "Environment call from VS-mode",
        11 => "Environment call from M-mode",
        12 => "Instruction page fault",
        13 => "Load page fault",
        15 => "Store/AMO page fault",
        16 => "Double trap",
        18 => "Software check",
        19 => "Hardware error",
        20 => "Instruction guest-page fault",
        21 => "Load guest-page fault",
        22 => "Virtual instruction",
        23 => "Store/AMO guest-page fault",
        _ => return None,
    })
}

fn interrupt_name(cause: u64) -> Option<&'static str> {
    Some(match cause {
        1 => "Supervisor software interrupt",
        3 => "Machine software interrupt",
        5 => "Supervisor timer interrupt",
        7 => "Machine timer interrupt",
        9 => "Supervisor external interrupt",
        11 => "Machine external interrupt",
        13 => "Counter-overflow interrupt",
        _ => return None,
    })
}

/// Trap encapsulates runtime exception and interrupt information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trap {
    pub pc: u64,
    pub tval: u64,
    pub cause: u64,
    /// Optional diagnostic message describing the reason.
    pub msg: Option<String>,
}

impl Trap {
    pub fn new(pc: u64, cause: u64, tval: u64) -> Trap {
        Trap {
            pc,
            tval,
            cause,
            msg: None,
        }
    }
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = if self.cause & CAUSE_INTERRUPT != 0 {
            let c = self.cause & !CAUSE_INTERRUPT;
            interrupt_name(c)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Interrupt {}", c))
        } else {
            exception_name(self.cause)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Exception {}", self.cause))
        };
        write!(f, "{}: pc={:x}, tval={:x}", name, self.pc, self.tval)?;
        if let Some(msg) = &self.msg {
            write!(f, " ({})", msg)?;
        }
        Ok(())
    }
}

/// Exception raised by memory accesses and instruction execution. The
/// CPU converts exceptions into architectural traps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exception {
    /// An architectural exception with cause and trap value.
    Trap { cause: u64, tval: u64 },
    /// A fatal emulator error which terminates the CPU.
    Fatal(String),
}

impl Exception {
    #[inline]
    pub fn trap(cause: u64, tval: u64) -> Exception {
        Exception::Trap { cause, tval }
    }

    pub fn fatal<S: Into<String>>(msg: S) -> Exception {
        Exception::Fatal(msg.into())
    }
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exception::Trap { cause, tval } => {
                let t = Trap::new(0, *cause, *tval);
                write!(f, "{}", t)
            }
            Exception::Fatal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Exception {}
