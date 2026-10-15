//! Opcodes, registers, and decoded instructions.

use std::fmt;

/// Instruction groups (major opcodes).
pub mod group {
    pub const LOAD: u8 = 0x03;
    pub const LOAD_FP: u8 = 0x07;
    pub const CUSTOM0: u8 = 0x0b;
    pub const MISC_MEM: u8 = 0x0f;
    pub const OP_IMM: u8 = 0x13;
    pub const AUIPC: u8 = 0x17;
    pub const OP_IMM32: u8 = 0x1b;
    pub const STORE: u8 = 0x23;
    pub const STORE_FP: u8 = 0x27;
    pub const CUSTOM1: u8 = 0x2b;
    pub const AMO: u8 = 0x2f;
    pub const OP: u8 = 0x33;
    pub const LUI: u8 = 0x37;
    pub const OP32: u8 = 0x3b;
    pub const MADD: u8 = 0x43;
    pub const MSUB: u8 = 0x47;
    pub const NMSUB: u8 = 0x4b;
    pub const NMADD: u8 = 0x4f;
    pub const OP_FP: u8 = 0x53;
    pub const OP_V: u8 = 0x57;
    pub const CUSTOM2: u8 = 0x5b;
    pub const BRANCH: u8 = 0x63;
    pub const JALR: u8 = 0x67;
    pub const JAL: u8 = 0x6f;
    pub const SYSTEM: u8 = 0x73;
    pub const OP_VE: u8 = 0x77;
    pub const CUSTOM3: u8 = 0x7b;

    /// Returns the name of the instruction group.
    pub fn name(g: u8) -> String {
        let s = match g {
            LOAD => "LOAD",
            LOAD_FP => "LOAD-FP",
            CUSTOM0 => "custom-0",
            MISC_MEM => "MISC-MEM",
            OP_IMM => "OP-IMM",
            AUIPC => "AUIPC",
            OP_IMM32 => "OP-IMM-32",
            STORE => "STORE",
            STORE_FP => "STORE-FP",
            CUSTOM1 => "custom-1",
            AMO => "AMO",
            OP => "OP",
            LUI => "LUI",
            OP32 => "OP-32",
            MADD => "MADD",
            MSUB => "MSUB",
            NMSUB => "NMSUB",
            NMADD => "NMADD",
            OP_FP => "OP-FP",
            OP_V => "OP-V",
            CUSTOM2 => "custom-2",
            BRANCH => "BRANCH",
            JALR => "JALR",
            JAL => "JAL",
            SYSTEM => "SYSTEM",
            OP_VE => "OP-VE",
            CUSTOM3 => "custom-3",
            _ => return format!("{{Group {:x}}}", g),
        };
        s.to_string()
    }
}

macro_rules! ops {
    ($($op:ident => $name:expr $(, $desc:expr)?;)*) => {
        /// RISC-V opcodes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(u8)]
        pub enum Op {
            #[default]
            $($op,)*
        }

        impl Op {
            /// Returns the assembler mnemonic of the opcode.
            pub fn name(self) -> &'static str {
                match self {
                    $(Op::$op => $name,)*
                }
            }

            /// Returns a short description of the opcode, if available.
            pub fn desc(self) -> &'static str {
                match self {
                    $(Op::$op => { #[allow(unused_variables)] let d = ""; $(let d = $desc;)? d })*
                }
            }

            /// All opcodes.
            pub const ALL: &'static [Op] = &[$(Op::$op,)*];
        }
    };
}

ops! {
    Invalid => "invalid";

    // Integer register-register.
    Add => "add", "Add";
    Sub => "sub";
    Sll => "sll";
    Slt => "slt";
    Sltu => "sltu";
    Xor => "xor";
    Srl => "srl";
    Sra => "sra";
    Or => "or", "OR";
    And => "and";

    // Integer register-immediate.
    Addi => "addi", "Add Immediate";
    Slti => "slti";
    Sltiu => "sltiu";
    Xori => "xori";
    Ori => "ori";
    Andi => "andi", "AND Immediate";
    Slli => "slli", "Shift Left Logical Immediate";
    Srli => "srli";
    Srai => "srai";

    // 32-bit (RV64).
    Addw => "addw";
    Subw => "subw";
    Sllw => "sllw";
    Srlw => "srlw";
    Sraw => "sraw";
    Addiw => "addiw", "Add Word Immediate";
    Slliw => "slliw";
    SlliUw => "slli.uw";
    Srliw => "srliw";
    Sraiw => "sraiw";

    // M extension.
    Mul => "mul";
    Mulh => "mulh";
    Mulhsu => "mulhsu";
    Mulhu => "mulhu";
    Div => "div";
    Divu => "divu";
    Rem => "rem";
    Remu => "remu";
    Mulw => "mulw";
    Divw => "divw";
    Divuw => "divuw";
    Remw => "remw";
    Remuw => "remuw";

    // Loads.
    Lb => "lb";
    Lh => "lh";
    Lw => "lw", "Load Word";
    Ld => "ld", "Load Doubleword";
    Lbu => "lbu", "Load Byte, Unsigned";
    Lhu => "lhu";
    Lwu => "lwu";

    // Stores.
    Sb => "sb";
    Sh => "sh";
    Sw => "sw";
    Sd => "sd", "Store Doubleword";

    // Control flow.
    Beq => "beq", "Branch if Equal";
    Bne => "bne", "Branch if Not Equal";
    Blt => "blt", "Branch if Less Than";
    Bge => "bge";
    Bltu => "bltu";
    Bgeu => "bgeu", "Branch if Greater Than or Equal";
    Jal => "jal", "Jump and Link";
    Jalr => "jalr", "Jump and Link Register";

    // Upper immediates.
    Lui => "lui";
    Auipc => "auipc", "Add Upper Immediate to PC";

    // System.
    Ecall => "ecall";
    Ebreak => "ebreak";
    Csrrw => "csrrw";
    Csrrs => "csrrs";
    Csrrc => "csrrc";
    Csrrwi => "csrrwi";
    Csrrsi => "csrrsi";
    Csrrci => "csrrci";
    Mret => "mret";
    Sret => "sret";
    Wfi => "wfi";

    // Memory ordering.
    Fence => "fence";
    FenceI => "fence.i";
    SfenceVma => "sfence.vma";
    CboInval => "cbo.inval";
    CboClean => "cbo.clean";
    CboFlush => "cbo.flush";
    CboZero => "cbo.zero";
    PrefetchI => "prefetch.i";
    PrefetchR => "prefetch.r";
    PrefetchW => "prefetch.w";

    // A extension.
    LrW => "lr.w";
    ScW => "sc.w";
    AmoaddW => "amoadd.w";
    AmoswapW => "amoswap.w";
    AmoxorW => "amoxor.w";
    AmoandW => "amoand.w";
    AmoorW => "amoor.w";
    AmominW => "amomin.w";
    AmomaxW => "amomax.w";
    AmominuW => "amominu.w";
    AmomaxuW => "amomaxu.w";
    LrD => "lr.d";
    ScD => "sc.d";
    AmoaddD => "amoadd.d";
    AmoswapD => "amoswap.d";
    AmoxorD => "amoxor.d";
    AmoandD => "amoand.d";
    AmoorD => "amoor.d";
    AmominD => "amomin.d";
    AmomaxD => "amomax.d";
    AmominuD => "amominu.d";
    AmomaxuD => "amomaxu.d";

    // F and D extensions.
    Flw => "flw";
    Fld => "fld";
    Fsw => "fsw";
    Fsd => "fsd";
    FaddS => "fadd.s";
    FaddD => "fadd.d";
    FsubS => "fsub.s";
    FsubD => "fsub.d";
    FmulS => "fmul.s";
    FmulD => "fmul.d";
    FdivS => "fdiv.s";
    FdivD => "fdiv.d";
    FsqrtS => "fsqrt.s";
    FsqrtD => "fsqrt.d";
    FmaddS => "fmadd.s";
    FmaddD => "fmadd.d";
    FmsubS => "fmsub.s";
    FmsubD => "fmsub.d";
    FnmsubS => "fnmsub.s";
    FnmsubD => "fnmsub.d";
    FnmaddS => "fnmadd.s";
    FnmaddD => "fnmadd.d";
    FsgnjS => "fsgnj.s";
    FsgnjnS => "fsgnjn.s";
    FsgnjxS => "fsgnjx.s";
    FsgnjD => "fsgnj.d";
    FsgnjnD => "fsgnjn.d";
    FsgnjxD => "fsgnjx.d";
    FminS => "fmin.s";
    FmaxS => "fmax.s";
    FminD => "fmin.d";
    FmaxD => "fmax.d";
    FeqS => "feq.s";
    FltS => "flt.s";
    FleS => "fle.s";
    FeqD => "feq.d";
    FltD => "flt.d";
    FleD => "fle.d";
    FcvtWS => "fcvt.w.s";
    FcvtWuS => "fcvt.wu.s";
    FcvtLS => "fcvt.l.s";
    FcvtLuS => "fcvt.lu.s";
    FcvtWD => "fcvt.w.d";
    FcvtWuD => "fcvt.wu.d";
    FcvtLD => "fcvt.l.d";
    FcvtLuD => "fcvt.lu.d";
    FcvtSW => "fcvt.s.w";
    FcvtSWu => "fcvt.s.wu";
    FcvtSL => "fcvt.s.l";
    FcvtSLu => "fcvt.s.lu";
    FcvtDW => "fcvt.d.w";
    FcvtDWu => "fcvt.d.wu";
    FcvtDL => "fcvt.d.l";
    FcvtDLu => "fcvt.d.lu";
    FcvtSD => "fcvt.s.d";
    FcvtDS => "fcvt.d.s";
    FmvXW => "fmv.x.w";
    FmvXD => "fmv.x.d";
    FmvWX => "fmv.w.x";
    FmvDX => "fmv.d.x";
    FclassS => "fclass.s";
    FclassD => "fclass.d";

    // V extension.
    Vsetvli => "vsetvli";
    Vsetivli => "vsetivli";
    Vsetvl => "vsetvl";
    VandVX => "vand.vx";
    VxorVV => "vxor.vv";
    VmsneVV => "vmsne.vv";
    VidV => "vid.v";
    VmvVX => "vmv.v.x";
    VmvVV => "vmv.v.v";
    VmvVI => "vmv.v.i";
    Vle8V => "vle8.v";
    Vle16V => "vle16.v";
    Vle32V => "vle32.v";
    Vle64V => "vle64.v";
    Vse8V => "vse8.v";
    Vse16V => "vse16.v";
    Vse32V => "vse32.v";
    Vse64V => "vse64.v";
    VaddVI => "vadd.vi";
    VmseqVI => "vmseq.vi";
    VsllVI => "vsll.vi";
    VsraVI => "vsra.vi";

    // Zicond.
    CzeroEqz => "czero.eqz";
    CzeroNez => "czero.nez";

    // Zbs.
    Bclr => "bclr";
    Bclri => "bclri";
    Bext => "bext";
    Bexti => "bexti";
    Binv => "binv";
    Binvi => "binvi";
    Bset => "bset";
    Bseti => "bseti";

    // Zba.
    AddUw => "add.uw";
    Sh1add => "sh1add";
    Sh2add => "sh2add";
    Sh3add => "sh3add";
    Sh1addUw => "sh1add.uw";
    Sh2addUw => "sh2add.uw";
    Sh3addUw => "sh3add.uw";

    // Zbb.
    Andn => "andn";
    Orn => "orn";
    Xnor => "xnor";
    Clz => "clz";
    Clzw => "clzw";
    Ctz => "ctz";
    Ctzw => "ctzw";
    Cpop => "cpop";
    Cpopw => "cpopw";
    Max => "max";
    Maxu => "maxu";
    Min => "min";
    Minu => "minu";
    SextB => "sext.b";
    SextH => "sext.h";
    ZextH => "zext.h";
    Rol => "rol";
    Rolw => "rolw";
    Ror => "ror";
    Rorw => "rorw";
    Rori => "rori";
    Roriw => "roriw";
    OrcB => "orc.b";
    Rev8 => "rev8";

    // Zbc.
    Clmul => "clmul";
    Clmulh => "clmulh";
    Clmulr => "clmulr";

    // Zbkb and Zbkx.
    Pack => "pack";
    Packh => "packh";
    Packw => "packw";
    Brev8 => "brev8";
    Xperm4 => "xperm4";
    Xperm8 => "xperm8";
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// ABI names of the integer registers.
pub const REGISTER_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// Integer register numbers.
pub mod reg {
    pub const ZERO: u8 = 0;
    pub const RA: u8 = 1;
    pub const SP: u8 = 2;
    pub const GP: u8 = 3;
    pub const TP: u8 = 4;
    pub const T0: u8 = 5;
    pub const T1: u8 = 6;
    pub const T2: u8 = 7;
    pub const S0: u8 = 8;
    pub const S1: u8 = 9;
    pub const A0: u8 = 10;
    pub const A1: u8 = 11;
    pub const A2: u8 = 12;
    pub const A3: u8 = 13;
    pub const A4: u8 = 14;
    pub const A5: u8 = 15;
    pub const A6: u8 = 16;
    pub const A7: u8 = 17;
    pub const S2: u8 = 18;
    pub const S3: u8 = 19;
    pub const S4: u8 = 20;
    pub const S5: u8 = 21;
    pub const S6: u8 = 22;
    pub const S7: u8 = 23;
    pub const S8: u8 = 24;
    pub const S9: u8 = 25;
    pub const S10: u8 = 26;
    pub const S11: u8 = 27;
    pub const T3: u8 = 28;
    pub const T4: u8 = 29;
    pub const T5: u8 = 30;
    pub const T6: u8 = 31;
}

/// Returns the ABI name of the integer register.
pub fn reg_name(r: u8) -> &'static str {
    REGISTER_NAMES.get(r as usize).copied().unwrap_or("x?")
}

/// A decoded RISC-V instruction.
///
/// The `imm` field holds the sign-extended immediate for most
/// instructions. Some instruction classes use it for other operands:
///
///  - CSR instructions: the CSR number; `rs1` holds zimm for the
///    immediate forms.
///  - OP-FP instructions: the rounding mode (funct3).
///  - fused multiply-add: `rs3 | rm << 5`.
///  - vector instructions: the encoding specific fields (vm, mop, nf).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Instr {
    pub op: Op,
    pub rd: u8,
    pub rs1: u8,
    pub rs2: u8,
    pub imm: i32,
}

impl Instr {
    /// The third source register of fused multiply-add instructions.
    #[inline]
    pub fn rs3(&self) -> usize {
        (self.imm & 0x1f) as usize
    }

    /// The rounding mode field of floating point instructions.
    #[inline]
    pub fn rm(&self) -> u32 {
        match self.op {
            Op::FmaddS
            | Op::FmaddD
            | Op::FmsubS
            | Op::FmsubD
            | Op::FnmaddS
            | Op::FnmaddD
            | Op::FnmsubS
            | Op::FnmsubD => ((self.imm >> 5) & 0b111) as u32,
            _ => (self.imm & 0b111) as u32,
        }
    }
}

fn fr(r: u8) -> String {
    format!("f{}", r)
}

fn vr(r: u8) -> String {
    format!("v{}", r)
}

const MAX_OP_NAME_LEN: usize = 10;

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Op::*;
        let name = format!("{:<width$}", self.op.name(), width = MAX_OP_NAME_LEN);
        let rd = reg_name(self.rd);
        let rs1 = reg_name(self.rs1);
        let rs2 = reg_name(self.rs2);
        let imm = self.imm;
        match self.op {
            Invalid => write!(f, "invalid"),

            Add | Sub | Sll | Slt | Sltu | Xor | Srl | Sra | Or | And | Addw | Subw | Sllw
            | Srlw | Sraw | Mul | Mulh | Mulhsu | Mulhu | Div | Divu | Rem | Remu | Mulw | Divw
            | Divuw | Remw | Remuw | CzeroEqz | CzeroNez | Bclr | Bext | Binv | Bset | AddUw
            | Sh1add | Sh2add | Sh3add | Sh1addUw | Sh2addUw | Sh3addUw | Andn | Orn | Xnor
            | Max | Maxu | Min | Minu | Rol | Rolw | Ror | Rorw | Clmul | Clmulh | Clmulr
            | Pack | Packh | Packw | Xperm4 | Xperm8 => {
                write!(f, "{} {},{},{}", name, rd, rs1, rs2)
            }

            Clz | Clzw | Ctz | Ctzw | Cpop | Cpopw | SextB | SextH | ZextH | OrcB | Rev8
            | Brev8 => write!(f, "{} {},{}", name, rd, rs1),

            Addi | Slti | Sltiu | Xori | Ori | Andi | Slli | Srli | Srai | Addiw | Slliw
            | SlliUw | Srliw | Sraiw | Bclri | Bexti | Binvi | Bseti | Rori | Roriw => {
                write!(f, "{} {},{},{}", name, rd, rs1, imm)
            }

            Lui | Auipc => write!(f, "{} {},0x{:x}", name, rd, (imm as u32) >> 12),

            Beq | Bne | Blt | Bge | Bltu | Bgeu => write!(f, "{} {},{},{}", name, rs1, rs2, imm),

            Jal => write!(f, "{} {},{}", name, rd, imm),
            Jalr => write!(f, "{} {},{}({})", name, rd, imm, rs1),

            Lb | Lh | Lw | Ld | Lbu | Lhu | Lwu => write!(f, "{} {},{}({})", name, rd, imm, rs1),
            Sb | Sh | Sw | Sd => write!(f, "{} {},{}({})", name, rs2, imm, rs1),

            Ecall | Ebreak | Mret | Sret | Wfi | Fence | FenceI => write!(f, "{}", name.trim_end()),
            SfenceVma => write!(f, "{} {},{}", name, rs1, rs2),
            CboInval | CboClean | CboFlush | CboZero => write!(f, "{} ({})", name, rs1),
            PrefetchI | PrefetchR | PrefetchW => write!(f, "{} {}({})", name, imm, rs1),

            Csrrw | Csrrs | Csrrc => {
                write!(f, "{} {},{},{}", name, rd, super::csr_name(imm as u16), rs1)
            }
            Csrrwi | Csrrsi | Csrrci => write!(
                f,
                "{} {},{},{}",
                name,
                rd,
                super::csr_name(imm as u16),
                self.rs1
            ),

            LrW | LrD => write!(f, "{} {},({})", name, rd, rs1),
            ScW | ScD | AmoaddW | AmoswapW | AmoxorW | AmoandW | AmoorW | AmominW | AmomaxW
            | AmominuW | AmomaxuW | AmoaddD | AmoswapD | AmoxorD | AmoandD | AmoorD | AmominD
            | AmomaxD | AmominuD | AmomaxuD => {
                write!(f, "{} {},{},({})", name, rd, rs2, rs1)
            }

            Flw | Fld => write!(f, "{} {},{}({})", name, fr(self.rd), imm, rs1),
            Fsw | Fsd => write!(f, "{} {},{}({})", name, fr(self.rs2), imm, rs1),

            FaddS | FaddD | FsubS | FsubD | FmulS | FmulD | FdivS | FdivD | FsgnjS | FsgnjnS
            | FsgnjxS | FsgnjD | FsgnjnD | FsgnjxD | FminS | FmaxS | FminD | FmaxD => write!(
                f,
                "{} {},{},{}",
                name,
                fr(self.rd),
                fr(self.rs1),
                fr(self.rs2)
            ),
            FsqrtS | FsqrtD | FcvtSD | FcvtDS => {
                write!(f, "{} {},{}", name, fr(self.rd), fr(self.rs1))
            }
            FmaddS | FmaddD | FmsubS | FmsubD | FnmsubS | FnmsubD | FnmaddS | FnmaddD => write!(
                f,
                "{} {},{},{},{}",
                name,
                fr(self.rd),
                fr(self.rs1),
                fr(self.rs2),
                fr(self.rs3() as u8)
            ),
            FeqS | FltS | FleS | FeqD | FltD | FleD => {
                write!(f, "{} {},{},{}", name, rd, fr(self.rs1), fr(self.rs2))
            }
            FcvtWS | FcvtWuS | FcvtLS | FcvtLuS | FcvtWD | FcvtWuD | FcvtLD | FcvtLuD | FmvXW
            | FmvXD | FclassS | FclassD => write!(f, "{} {},{}", name, rd, fr(self.rs1)),
            FcvtSW | FcvtSWu | FcvtSL | FcvtSLu | FcvtDW | FcvtDWu | FcvtDL | FcvtDLu | FmvWX
            | FmvDX => write!(f, "{} {},{}", name, fr(self.rd), rs1),

            Vsetvli => write!(
                f,
                "{} {},{},{}",
                name,
                rd,
                rs1,
                super::VType(imm as u32 as u64)
            ),
            Vsetivli => write!(
                f,
                "{} {},{},{}",
                name,
                rd,
                self.rs1,
                super::VType(imm as u32 as u64)
            ),
            Vsetvl => write!(f, "{} {},{},{}", name, rd, rs1, rs2),
            VmvVX => write!(f, "{} {},{}", name, vr(self.rd), rs1),
            VmvVV => write!(f, "{} {},{}", name, vr(self.rd), vr(self.rs1)),
            VmvVI => write!(f, "{} {},{}", name, vr(self.rd), imm),
            VandVX => {
                write!(f, "{} {},{},{}", name, vr(self.rd), vr(self.rs2), rs1)?;
                vmask(f, imm & 1)
            }
            VxorVV | VmsneVV => {
                write!(
                    f,
                    "{} {},{},{}",
                    name,
                    vr(self.rd),
                    vr(self.rs2),
                    vr(self.rs1)
                )?;
                vmask(f, imm & 1)
            }
            VidV => {
                write!(f, "{} {}", name, vr(self.rd))?;
                vmask(f, imm & 1)
            }
            VaddVI | VmseqVI => {
                write!(f, "{} {},{},{}", name, vr(self.rd), vr(self.rs2), imm >> 1)?;
                vmask(f, imm & 1)
            }
            VsllVI | VsraVI => {
                write!(
                    f,
                    "{} {},{},{}",
                    name,
                    vr(self.rd),
                    vr(self.rs2),
                    (imm as u32) & !(1 << 31)
                )?;
                vmask(f, ((imm as u32) >> 31) as i32)
            }
            Vle8V | Vle16V | Vle32V | Vle64V | Vse8V | Vse16V | Vse32V | Vse64V => {
                write!(f, "{} {},({})", name, vr(self.rd), rs1)?;
                vmask(f, imm & 1)
            }
        }
    }
}

fn vmask(f: &mut fmt::Formatter<'_>, vm: i32) -> fmt::Result {
    if vm == 0 {
        f.write_str(",v0.t")
    } else {
        Ok(())
    }
}
