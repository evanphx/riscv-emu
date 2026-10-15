//! Control and Status Register (CSR) numbers and names.

pub const CSR_FFLAGS: u16 = 0x001;
pub const CSR_FRM: u16 = 0x002;
pub const CSR_FCSR: u16 = 0x003;
pub const CSR_VSTART: u16 = 0x008;
pub const CSR_VXSAT: u16 = 0x009;
pub const CSR_VXRM: u16 = 0x00a;
pub const CSR_VCSR: u16 = 0x00f;
pub const CSR_SEED: u16 = 0x015;
pub const CSR_SSTATUS: u16 = 0x100;
pub const CSR_SIE: u16 = 0x104;
pub const CSR_STVEC: u16 = 0x105;
pub const CSR_SCOUNTEREN: u16 = 0x106;
pub const CSR_SENVCFG: u16 = 0x10a;
pub const CSR_SSCRATCH: u16 = 0x140;
pub const CSR_SEPC: u16 = 0x141;
pub const CSR_SCAUSE: u16 = 0x142;
pub const CSR_STVAL: u16 = 0x143;
pub const CSR_SIP: u16 = 0x144;
pub const CSR_STIMECMP: u16 = 0x14d;
pub const CSR_SATP: u16 = 0x180;
pub const CSR_MSTATUS: u16 = 0x300;
pub const CSR_MISA: u16 = 0x301;
pub const CSR_MEDELEG: u16 = 0x302;
pub const CSR_MIDELEG: u16 = 0x303;
pub const CSR_MIE: u16 = 0x304;
pub const CSR_MTVEC: u16 = 0x305;
pub const CSR_MCOUNTEREN: u16 = 0x306;
pub const CSR_MENVCFG: u16 = 0x30a;
pub const CSR_MSTATEEN0: u16 = 0x30c;
pub const CSR_MCOUNTINHIBIT: u16 = 0x320;
pub const CSR_MHPMEVENT3: u16 = 0x323;
pub const CSR_MSCRATCH: u16 = 0x340;
pub const CSR_MEPC: u16 = 0x341;
pub const CSR_MCAUSE: u16 = 0x342;
pub const CSR_MTVAL: u16 = 0x343;
pub const CSR_MIP: u16 = 0x344;
pub const CSR_PMPCFG0: u16 = 0x3a0;
pub const CSR_PMPADDR0: u16 = 0x3b0;
pub const CSR_TSELECT: u16 = 0x7a0;
pub const CSR_TDATA1: u16 = 0x7a1;
pub const CSR_TINFO: u16 = 0x7a4;
pub const CSR_MCYCLE: u16 = 0xb00;
pub const CSR_MINSTRET: u16 = 0xb02;
pub const CSR_CYCLE: u16 = 0xc00;
pub const CSR_TIME: u16 = 0xc01;
pub const CSR_INSTRET: u16 = 0xc02;
pub const CSR_VL: u16 = 0xc20;
pub const CSR_VTYPE: u16 = 0xc21;
pub const CSR_VLENB: u16 = 0xc22;
pub const CSR_MVENDORID: u16 = 0xf11;
pub const CSR_MARCHID: u16 = 0xf12;
pub const CSR_MIMPID: u16 = 0xf13;
pub const CSR_MHARTID: u16 = 0xf14;
pub const CSR_MCONFIGPTR: u16 = 0xf15;
pub const CSR_SCOUNTINHIBIT: u16 = 0xfb0;

/// Emulator debug CSR: bit 0 enables the CPU debug trace.
pub const CSR_GOEMU_DEBUG: u16 = 0x800;
/// Emulator CSR returning host wall clock time in nanoseconds.
pub const CSR_GOEMU_TIME: u16 = 0x801;
/// Emulator CSR controlling host profiling.
pub const CSR_GOEMU_CPU_PROFILE: u16 = 0x802;

/// Returns true if the CSR is read-only.
pub fn csr_read_only(csr: u16) -> bool {
    (csr >> 10) & 0b11 == 0b11
}

/// Returns the minimum privilege level required to access the CSR.
pub fn csr_privilege(csr: u16) -> super::Mode {
    super::Mode::from_bits(((csr >> 8) & 0b11) as u64)
}

static CSR_NAMES: &[(u16, &str)] = &[
    (0x001, "fflags"),
    (0x002, "frm"),
    (0x003, "fcsr"),
    (0x008, "vstart"),
    (0x009, "vxsat"),
    (0x00a, "vxrm"),
    (0x00f, "vcsr"),
    (0xc20, "vl"),
    (0xc21, "vtype"),
    (0xc22, "vlenb"),
    (0x011, "ssp"),
    (0x015, "seed"),
    (0x017, "jvt"),
    (0xc00, "cycle"),
    (0xc01, "time"),
    (0xc02, "instret"),
    (0x100, "sstatus"),
    (0x104, "sie"),
    (0x105, "stvec"),
    (0x106, "scounteren"),
    (0x10a, "senvcfg"),
    (0x120, "scountinhibit"),
    (0x140, "sscratch"),
    (0x141, "sepc"),
    (0x142, "scause"),
    (0x143, "stval"),
    (0x144, "sip"),
    (0xda0, "scountovf"),
    (0x150, "siselect"),
    (0x151, "sireg"),
    (0x180, "satp"),
    (0x14d, "stimecmp"),
    (0x5a8, "scontext"),
    (0x181, "srmcfg"),
    (0x10c, "sstateen0"),
    (0x10d, "sstateen1"),
    (0x10e, "sstateen2"),
    (0x10f, "sstateen3"),
    (0x600, "hstatus"),
    (0x602, "hedeleg"),
    (0x603, "hideleg"),
    (0x604, "hie"),
    (0x606, "hcounteren"),
    (0x607, "hgeie"),
    (0x643, "htval"),
    (0x644, "hip"),
    (0x645, "hvip"),
    (0x64a, "htinst"),
    (0xe12, "hgeip"),
    (0x60a, "henvcfg"),
    (0x680, "hgatp"),
    (0x605, "htimedelta"),
    (0x200, "vsstatus"),
    (0x204, "vsie"),
    (0x205, "vstvec"),
    (0x240, "vsscratch"),
    (0x241, "vsepc"),
    (0x242, "vscause"),
    (0x243, "vstval"),
    (0x244, "vsip"),
    (0x280, "vsatp"),
    (0x24d, "vstimecmp"),
    (0xf11, "mvendorid"),
    (0xf12, "marchid"),
    (0xf13, "mimpid"),
    (0xf14, "mhartid"),
    (0xf15, "mconfigptr"),
    (0x300, "mstatus"),
    (0x301, "misa"),
    (0x302, "medeleg"),
    (0x303, "mideleg"),
    (0x304, "mie"),
    (0x305, "mtvec"),
    (0x306, "mcounteren"),
    (0x340, "mscratch"),
    (0x341, "mepc"),
    (0x342, "mcause"),
    (0x343, "mtval"),
    (0x344, "mip"),
    (0x34a, "mtinst"),
    (0x34b, "mtval2"),
    (0x30a, "menvcfg"),
    (0x747, "mseccfg"),
    (0x30c, "mstateen0"),
    (0x30d, "mstateen1"),
    (0x30e, "mstateen2"),
    (0x30f, "mstateen3"),
    (0x740, "mnscratch"),
    (0x741, "mnepc"),
    (0x742, "mncause"),
    (0x744, "mnstatus"),
    (0xb00, "mcycle"),
    (0xb02, "minstret"),
    (0x320, "mcountinhibit"),
    (0x321, "mcyclecfg"),
    (0x322, "minstretcfg"),
    (0x7a0, "tselect"),
    (0x7a1, "tdata1"),
    (0x7a2, "tdata2"),
    (0x7a3, "tdata3"),
    (0x7a8, "mcontext"),
    (0x7b0, "dcsr"),
    (0x7b1, "dpc"),
    (0x7b2, "dscratch0"),
    (0x7b3, "dscratch1"),
];

/// Returns the symbolic name of the CSR.
pub fn csr_name(csr: u16) -> String {
    if let Some((_, name)) = CSR_NAMES.iter().find(|(n, _)| *n == csr) {
        return name.to_string();
    }
    match csr {
        0xc03..=0xc1f => format!("hpmcounter{}", csr - 0xc00),
        0xb03..=0xb1f => format!("mhpmcounter{}", csr - 0xb00),
        0x323..=0x33f => format!("mhpmevent{}", csr - 0x320),
        0x3a0..=0x3af if csr.is_multiple_of(2) => format!("pmpcfg{}", csr - 0x3a0),
        0x3b0..=0x3ef => format!("pmpaddr{}", csr - 0x3b0),
        _ => format!("{:03x}", csr),
    }
}
