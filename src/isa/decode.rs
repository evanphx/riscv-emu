//! Instruction decoding.

use super::group;
use super::{Instr, Op};
use std::sync::OnceLock;

/// Register mapping of the 3-bit register fields in compressed
/// instructions.
const CREG: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

#[inline]
fn creg(v: u16) -> u8 {
    CREG[(v & 0b111) as usize]
}

static RVC_TABLE: OnceLock<Box<[Instr]>> = OnceLock::new();

fn rvc_table() -> &'static [Instr] {
    RVC_TABLE.get_or_init(|| {
        let mut table = vec![Instr::default(); 65536];
        for (i, entry) in table.iter_mut().enumerate() {
            if let Ok(instr) = decode_c(i as u16) {
                *entry = instr;
            }
        }
        table.into_boxed_slice()
    })
}

/// Implements a fast O(1) decoding of the compressed instruction raw
/// using a pre-computed table. Illegal instructions decode to
/// `Op::Invalid`.
#[inline]
pub fn decode_c_fast(raw: u16) -> Instr {
    rvc_table()[raw as usize]
}

/// Initializes the compressed instruction decode table.
pub fn init_decode_tables() {
    rvc_table();
}

#[inline]
fn sext(v: i32, bits: u32) -> i32 {
    let shift = 32 - bits;
    (v << shift) >> shift
}

/// Decodes the compressed (16-bit) instruction raw.
pub fn decode_c(raw: u16) -> Result<Instr, String> {
    let mut instr = Instr::default();

    if raw & 0b11 == 0b11 || raw == 0 {
        return Err(format!("illegal 16-bit instruction: raw={:04x}", raw));
    }

    let r = raw as i32;
    let rds1 = ((raw >> 7) & 0b11111) as u8;
    let rs2 = ((raw >> 2) & 0b11111) as u8;
    let funct3 = (raw >> 13) & 0b111;

    match raw & 0b11 {
        0 => match funct3 {
            0b000 => {
                // c.addi4spn
                instr.imm = (r & 0b1000000) >> 4
                    | (r & 0b100000) >> 2
                    | (r & 0b11000_00000000) >> 7
                    | (r & 0b111_10000000) >> 1;
                if instr.imm == 0 {
                    return Err(format!("c.addi4spn: nzuimm=0, raw={:04x}", raw));
                }
                instr.rd = creg(raw >> 2);
                instr.rs1 = 2;
                instr.op = Op::Addi;
            }
            0b001 => {
                instr.rd = creg(raw >> 2);
                instr.rs1 = creg(raw >> 7);
                instr.imm = (r & 0b11100_00000000) >> 7 | (r & 0b1100000) << 1;
                instr.op = Op::Fld;
            }
            0b010 => {
                instr.rd = creg(raw >> 2);
                instr.rs1 = creg(raw >> 7);
                instr.imm =
                    (r & 0b1000000) >> 4 | (r & 0b11100_00000000) >> 7 | (r & 0b100000) << 1;
                instr.op = Op::Lw;
            }
            0b011 => {
                instr.rd = creg(raw >> 2);
                instr.rs1 = creg(raw >> 7);
                instr.imm = (r & 0b11100_00000000) >> 7 | (r & 0b1100000) << 1;
                instr.op = Op::Ld;
            }
            0b100 => {
                // Zcb load/store byte and halfword.
                let f = (raw >> 10) & 0b111;
                instr.rs1 = creg(raw >> 7);
                match f {
                    0b000 => {
                        instr.rd = creg(raw >> 2);
                        instr.imm = ((r >> 6) & 1) | ((r >> 4) & 0b10);
                        instr.op = Op::Lbu;
                    }
                    0b001 => {
                        instr.rd = creg(raw >> 2);
                        instr.imm = (r >> 4) & 0b10;
                        instr.op = if raw & (1 << 6) == 0 { Op::Lhu } else { Op::Lh };
                    }
                    0b010 => {
                        instr.rs2 = creg(raw >> 2);
                        instr.imm = ((r >> 6) & 1) | ((r >> 4) & 0b10);
                        instr.op = Op::Sb;
                    }
                    0b011 if raw & (1 << 6) == 0 => {
                        instr.rs2 = creg(raw >> 2);
                        instr.imm = (r >> 4) & 0b10;
                        instr.op = Op::Sh;
                    }
                    _ => return Err(format!("zcb: func3={:03b}, raw={:04x}", f, raw)),
                }
            }
            0b101 => {
                instr.rs2 = creg(raw >> 2);
                instr.rs1 = creg(raw >> 7);
                instr.imm = (r & 0b11100_00000000) >> 7 | (r & 0b1100000) << 1;
                instr.op = Op::Fsd;
            }
            0b110 => {
                instr.rs2 = creg(raw >> 2);
                instr.rs1 = creg(raw >> 7);
                instr.imm =
                    (r & 0b1000000) >> 4 | (r & 0b11100_00000000) >> 7 | (r & 0b100000) << 1;
                instr.op = Op::Sw;
            }
            0b111 => {
                instr.rs2 = creg(raw >> 2);
                instr.rs1 = creg(raw >> 7);
                instr.imm = (r & 0b11100_00000000) >> 7 | (r & 0b1100000) << 1;
                instr.op = Op::Sd;
            }
            _ => unreachable!(),
        },

        1 => match funct3 {
            0b000 => {
                // c.addi / c.nop
                instr.rd = rds1;
                instr.rs1 = rds1;
                instr.imm = sext((r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7, 6);
                instr.op = Op::Addi;
            }
            0b001 => {
                // c.addiw
                if rds1 == 0 {
                    return Err(format!("c.addiw: rd=0, raw={:04x}", raw));
                }
                instr.rd = rds1;
                instr.rs1 = rds1;
                instr.imm = sext((r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7, 6);
                instr.op = Op::Addiw;
            }
            0b010 => {
                // c.li
                instr.rd = rds1;
                instr.rs1 = 0;
                instr.imm = sext((r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7, 6);
                instr.op = Op::Addi;
            }
            0b011 => {
                instr.rd = rds1;
                if rds1 == 2 {
                    // c.addi16sp
                    instr.imm = sext(
                        (r & 0b1000000) >> 2
                            | (r & 0b100) << 3
                            | (r & 0b100000) << 1
                            | (r & 0b11000) << 4
                            | (r & 0b10000_00000000) >> 3,
                        10,
                    );
                    if instr.imm == 0 {
                        return Err(format!("c.addi16sp: nzimm=0, raw={:04x}", raw));
                    }
                    instr.rs1 = 2;
                    instr.op = Op::Addi;
                } else {
                    // c.lui
                    instr.imm = sext((r & 0b1111100) << 10 | (r & 0b10000_00000000) << 5, 18);
                    if instr.imm == 0 {
                        return Err(format!("c.lui: nzimm=0, raw={:04x}", raw));
                    }
                    instr.op = Op::Lui;
                }
            }
            0b100 => {
                let funct2 = (raw >> 10) & 0b11;
                instr.rd = creg(raw >> 7);
                instr.rs1 = instr.rd;
                match funct2 {
                    0b00 => {
                        instr.imm = (r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7;
                        instr.op = Op::Srli;
                    }
                    0b01 => {
                        instr.imm = (r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7;
                        instr.op = Op::Srai;
                    }
                    0b10 => {
                        instr.imm = sext((r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7, 6);
                        instr.op = Op::Andi;
                    }
                    _ => {
                        let f3 = ((raw >> 5) & 0b11) | ((raw >> 10) & 0b100);
                        instr.rs2 = creg(raw >> 2);
                        instr.op = match f3 {
                            0b000 => Op::Sub,
                            0b001 => Op::Xor,
                            0b010 => Op::Or,
                            0b011 => Op::And,
                            0b100 => Op::Subw,
                            0b101 => Op::Addw,
                            0b110 => Op::Mul,
                            _ => {
                                // Zcb unary operations.
                                instr.rs2 = 0;
                                match (raw >> 2) & 0b111 {
                                    0b000 => {
                                        instr.imm = 0xff;
                                        Op::Andi
                                    }
                                    0b001 => Op::SextB,
                                    0b010 => Op::ZextH,
                                    0b011 => Op::SextH,
                                    0b100 => Op::AddUw,
                                    0b101 => {
                                        instr.imm = -1;
                                        Op::Xori
                                    }
                                    _ => {
                                        return Err(format!(
                                            "zcb: raw={:04x}, Q1/100/11/{:03b}",
                                            raw, f3
                                        ))
                                    }
                                }
                            }
                        };
                    }
                }
            }
            0b101 => {
                // c.j
                instr.imm = sext(
                    (r & 0b111000) >> 2
                        | (r & 0b1000_00000000) >> 7
                        | (r & 0b100) << 3
                        | (r & 0b10000000) >> 1
                        | (r & 0b1000000) << 1
                        | (r & 0b110_00000000) >> 1
                        | (r & 0b1_00000000) << 2
                        | (r & 0b10000_00000000) >> 1,
                    12,
                );
                instr.op = Op::Jal;
            }
            _ => {
                // c.beqz / c.bnez
                instr.rs1 = creg(raw >> 7);
                instr.imm = sext(
                    (r & 0b11000) >> 2
                        | (r & 0b1100_00000000) >> 7
                        | (r & 0b100) << 3
                        | (r & 0b1100000) << 1
                        | (r & 0b10000_00000000) >> 4,
                    9,
                );
                instr.op = if funct3 == 0b110 { Op::Beq } else { Op::Bne };
            }
        },

        2 => match funct3 {
            0b000 => {
                instr.rd = rds1;
                instr.rs1 = rds1;
                instr.imm = (r & 0b1111100) >> 2 | (r & 0b10000_00000000) >> 7;
                instr.op = Op::Slli;
            }
            0b001 => {
                instr.imm = (r & 0b11100) << 4 | (r & 0b1100000) >> 2 | (r & 0b10000_00000000) >> 7;
                instr.rd = rds1;
                instr.rs1 = 2;
                instr.op = Op::Fld;
            }
            0b010 => {
                if rds1 == 0 {
                    return Err(format!("c.lwsp: rd=0, raw={:04x}", raw));
                }
                instr.imm = (r & 0b1110000) >> 2 | (r & 0b10000_00000000) >> 7 | (r & 0b1100) << 4;
                instr.rd = rds1;
                instr.rs1 = 2;
                instr.op = Op::Lw;
            }
            0b011 => {
                if rds1 == 0 {
                    return Err(format!("c.ldsp: rd=0, raw={:04x}", raw));
                }
                instr.imm = (r & 0b11100) << 4 | (r & 0b1100000) >> 2 | (r & 0b10000_00000000) >> 7;
                instr.rd = rds1;
                instr.rs1 = 2;
                instr.op = Op::Ld;
            }
            0b100 => {
                if raw & 0b10000_00000000 == 0 {
                    if rds1 == 0 {
                        return Err(format!("raw={:04x}", raw));
                    }
                    if rs2 == 0 {
                        // c.jr
                        instr.rs1 = rds1;
                        instr.op = Op::Jalr;
                    } else {
                        // c.mv
                        instr.rd = rds1;
                        instr.rs2 = rs2;
                        instr.op = Op::Add;
                    }
                } else if rds1 == 0 {
                    if rs2 == 0 {
                        instr.op = Op::Ebreak;
                    } else {
                        return Err(format!("raw={:04x}", raw));
                    }
                } else if rs2 == 0 {
                    // c.jalr
                    instr.rd = 1;
                    instr.rs1 = rds1;
                    instr.op = Op::Jalr;
                } else {
                    // c.add
                    instr.rd = rds1;
                    instr.rs1 = rds1;
                    instr.rs2 = rs2;
                    instr.op = Op::Add;
                }
            }
            0b101 => {
                instr.rs1 = 2;
                instr.rs2 = rs2;
                instr.imm = (r & 0b11100_00000000) >> 7 | (r & 0b11_10000000) >> 1;
                instr.op = Op::Fsd;
            }
            0b110 => {
                instr.rs1 = 2;
                instr.rs2 = rs2;
                instr.imm = (r & 0b11110_00000000) >> 7 | (r & 0b01_10000000) >> 1;
                instr.op = Op::Sw;
            }
            _ => {
                instr.rs1 = 2;
                instr.rs2 = rs2;
                instr.imm = (r & 0b11100_00000000) >> 7 | (r & 0b11_10000000) >> 1;
                instr.op = Op::Sd;
            }
        },

        _ => unreachable!(),
    }

    Ok(instr)
}

#[inline]
fn imm_i(raw: u32) -> i32 {
    (raw as i32) >> 20
}

#[inline]
fn imm_s(raw: u32) -> i32 {
    let s = raw as i32;
    ((s >> 20) & !0b11111) | ((s >> 7) & 0b11111)
}

#[inline]
fn imm_b(raw: u32) -> i32 {
    let s = raw as i32;
    ((s >> 19) & !0b01111_11111111)
        | ((s & 0b00000_10000000) << 4)
        | ((s >> 20) & 0b00111_11100000)
        | ((s >> 7) & 0b00000_00011110)
}

#[inline]
fn imm_u(raw: u32) -> i32 {
    (raw & !0xfff) as i32
}

#[inline]
fn imm_j(raw: u32) -> i32 {
    let s = raw as i32;
    ((s >> 11) & !0b1111_11111111_11111111)
        | (s & 0b1111_11110000_00000000)
        | ((s >> 9) & 0b1000_00000000)
        | ((s >> 20) & 0b111_11111110)
}

/// Decodes the 32-bit instruction raw.
pub fn decode(raw: u32) -> Result<Instr, String> {
    let mut instr = Instr::default();

    if raw & 0b11 != 0b11 {
        return Err(format!("illegal 32-bit instruction: {:08x}", raw));
    }

    let grp = (raw & 0b1111111) as u8;

    instr.rd = ((raw >> 7) & 0b11111) as u8;
    instr.rs1 = ((raw >> 15) & 0b11111) as u8;
    instr.rs2 = ((raw >> 20) & 0b11111) as u8;

    let funct3 = (raw >> 12) & 0b111;
    let funct7 = (raw >> 25) & 0b1111111;
    let funct12 = raw >> 20;

    let bad = || {
        Err(format!(
            "{}/{:07b}/{:03b}: raw={:08x}",
            group::name(grp),
            funct7,
            funct3,
            raw
        ))
    };

    match grp {
        group::AUIPC => {
            instr.imm = imm_u(raw);
            instr.op = Op::Auipc;
        }
        group::LUI => {
            instr.imm = imm_u(raw);
            instr.op = Op::Lui;
        }
        group::STORE => {
            instr.imm = imm_s(raw);
            instr.op = match funct3 {
                0 => Op::Sb,
                1 => Op::Sh,
                2 => Op::Sw,
                3 => Op::Sd,
                _ => return bad(),
            };
        }
        group::LOAD => {
            instr.imm = imm_i(raw);
            instr.op = match funct3 {
                0 => Op::Lb,
                1 => Op::Lh,
                2 => Op::Lw,
                3 => Op::Ld,
                4 => Op::Lbu,
                5 => Op::Lhu,
                6 => Op::Lwu,
                _ => return bad(),
            };
        }
        group::OP_IMM => {
            instr.imm = imm_i(raw);
            let shamt = ((raw >> 20) & 0x3f) as i32;
            let funct6 = raw >> 26;
            instr.op = match funct3 {
                0b000 => Op::Addi,
                0b010 => Op::Slti,
                0b011 => Op::Sltiu,
                0b100 => Op::Xori,
                0b110 => Op::Ori,
                0b111 => Op::Andi,
                0b001 => match funct12 {
                    0x600 => Op::Clz,
                    0x601 => Op::Ctz,
                    0x602 => Op::Cpop,
                    0x604 => Op::SextB,
                    0x605 => Op::SextH,
                    _ => {
                        instr.imm = shamt;
                        match funct6 {
                            0b000000 => Op::Slli,
                            0b001010 => Op::Bseti,
                            0b010010 => Op::Bclri,
                            0b011010 => Op::Binvi,
                            _ => return bad(),
                        }
                    }
                },
                _ => match funct12 {
                    // 0b101
                    0x287 => Op::OrcB,
                    0x6b8 => Op::Rev8,
                    0x687 => Op::Brev8,
                    _ => {
                        instr.imm = shamt;
                        match funct6 {
                            0b000000 => Op::Srli,
                            0b010000 => Op::Srai,
                            0b011000 => Op::Rori,
                            0b010010 => Op::Bexti,
                            _ => return bad(),
                        }
                    }
                },
            };
        }
        group::OP_IMM32 => {
            instr.imm = imm_i(raw);
            let shamt5 = ((raw >> 20) & 0x1f) as i32;
            instr.op = match funct3 {
                0b000 => Op::Addiw,
                0b001 => match funct12 {
                    0x600 => Op::Clzw,
                    0x601 => Op::Ctzw,
                    0x602 => Op::Cpopw,
                    _ => {
                        if raw >> 26 == 0b000010 {
                            instr.imm = ((raw >> 20) & 0x3f) as i32;
                            Op::SlliUw
                        } else if funct7 == 0 {
                            instr.imm = shamt5;
                            Op::Slliw
                        } else {
                            return bad();
                        }
                    }
                },
                0b101 => {
                    instr.imm = shamt5;
                    match funct7 {
                        0b0000000 => Op::Srliw,
                        0b0100000 => Op::Sraiw,
                        0b0110000 => Op::Roriw,
                        _ => return bad(),
                    }
                }
                _ => return bad(),
            };
        }
        group::SYSTEM => {
            match funct3 {
                0 => {
                    if funct7 == 0b0001001 {
                        instr.op = Op::SfenceVma;
                    } else {
                        if instr.rd != 0 || instr.rs1 != 0 {
                            return bad();
                        }
                        instr.op = match funct12 {
                            0x000 => Op::Ecall,
                            0x001 => Op::Ebreak,
                            0x102 => Op::Sret,
                            0x105 => Op::Wfi,
                            0x302 => Op::Mret,
                            _ => {
                                return Err(format!("invalid SYSTEM trap/return: raw={:08x}", raw))
                            }
                        };
                    }
                }
                // For CSR instructions, imm holds the CSR number and rs1
                // the zimm value in the immediate forms.
                1 => instr.op = Op::Csrrw,
                2 => instr.op = Op::Csrrs,
                3 => instr.op = Op::Csrrc,
                5 => instr.op = Op::Csrrwi,
                6 => instr.op = Op::Csrrsi,
                7 => instr.op = Op::Csrrci,
                _ => return Err(format!("invalid SYSTEM: raw={:08x}", raw)),
            }
            if funct3 != 0 {
                instr.imm = (funct12 & 0xfff) as i32;
            }
        }
        group::JAL => {
            instr.imm = imm_j(raw);
            instr.op = Op::Jal;
        }
        group::JALR => {
            if funct3 != 0 {
                return bad();
            }
            instr.imm = imm_i(raw);
            instr.op = Op::Jalr;
        }
        group::BRANCH => {
            instr.imm = imm_b(raw);
            instr.op = match funct3 {
                0 => Op::Beq,
                1 => Op::Bne,
                4 => Op::Blt,
                5 => Op::Bge,
                6 => Op::Bltu,
                7 => Op::Bgeu,
                _ => return bad(),
            };
        }
        group::OP => {
            instr.op = match (funct7, funct3) {
                (0, 0) => Op::Add,
                (0, 1) => Op::Sll,
                (0, 2) => Op::Slt,
                (0, 3) => Op::Sltu,
                (0, 4) => Op::Xor,
                (0, 5) => Op::Srl,
                (0, 6) => Op::Or,
                (0, 7) => Op::And,

                (1, 0) => Op::Mul,
                (1, 1) => Op::Mulh,
                (1, 2) => Op::Mulhsu,
                (1, 3) => Op::Mulhu,
                (1, 4) => Op::Div,
                (1, 5) => Op::Divu,
                (1, 6) => Op::Rem,
                (1, 7) => Op::Remu,

                (0b0100000, 0b000) => Op::Sub,
                (0b0100000, 0b101) => Op::Sra,
                (0b0100000, 0b111) => Op::Andn,
                (0b0100000, 0b110) => Op::Orn,
                (0b0100000, 0b100) => Op::Xnor,

                (0b0010000, 0b010) => Op::Sh1add,
                (0b0010000, 0b100) => Op::Sh2add,
                (0b0010000, 0b110) => Op::Sh3add,

                (0b0000101, 0b001) => Op::Clmul,
                (0b0000101, 0b010) => Op::Clmulr,
                (0b0000101, 0b011) => Op::Clmulh,
                (0b0000101, 0b100) => Op::Min,
                (0b0000101, 0b101) => Op::Minu,
                (0b0000101, 0b110) => Op::Max,
                (0b0000101, 0b111) => Op::Maxu,

                (0b0000111, 0b101) => Op::CzeroEqz,
                (0b0000111, 0b111) => Op::CzeroNez,

                (0b0110000, 0b001) => Op::Rol,
                (0b0110000, 0b101) => Op::Ror,

                (0b0100100, 0b001) => Op::Bclr,
                (0b0100100, 0b101) => Op::Bext,
                (0b0110100, 0b001) => Op::Binv,
                (0b0010100, 0b001) => Op::Bset,
                (0b0010100, 0b010) => Op::Xperm4,
                (0b0010100, 0b100) => Op::Xperm8,

                (0b0000100, 0b100) => Op::Pack,
                (0b0000100, 0b111) => Op::Packh,

                _ => return bad(),
            };
        }
        group::OP32 => {
            instr.op = match (funct7, funct3) {
                (0, 0) => Op::Addw,
                (0, 1) => Op::Sllw,
                (0, 5) => Op::Srlw,
                (0b0100000, 0) => Op::Subw,
                (0b0100000, 5) => Op::Sraw,
                (1, 0) => Op::Mulw,
                (1, 4) => Op::Divw,
                (1, 5) => Op::Divuw,
                (1, 6) => Op::Remw,
                (1, 7) => Op::Remuw,
                (0b0000100, 0) => Op::AddUw,
                (0b0000100, 4) => {
                    if instr.rs2 == 0 {
                        Op::ZextH
                    } else {
                        Op::Packw
                    }
                }
                (0b0010000, 2) => Op::Sh1addUw,
                (0b0010000, 4) => Op::Sh2addUw,
                (0b0010000, 6) => Op::Sh3addUw,
                (0b0110000, 1) => Op::Rolw,
                (0b0110000, 5) => Op::Rorw,
                _ => return bad(),
            };
        }
        group::MISC_MEM => {
            instr.op = match funct3 {
                0b000 => Op::Fence,
                0b001 => Op::FenceI,
                0b010 => match funct12 {
                    0 => Op::CboInval,
                    1 => Op::CboClean,
                    2 => Op::CboFlush,
                    4 => Op::CboZero,
                    _ => return bad(),
                },
                _ => return bad(),
            };
        }
        group::AMO => {
            let funct5 = (raw >> 27) & 0b11111;
            // imm holds the aq and rl bits.
            instr.imm = ((raw >> 25) & 0b11) as i32;
            let w = match funct3 {
                2 => true,
                3 => false,
                _ => return bad(),
            };
            instr.op = match (funct5, w) {
                (0b00000, true) => Op::AmoaddW,
                (0b00000, false) => Op::AmoaddD,
                (0b00001, true) => Op::AmoswapW,
                (0b00001, false) => Op::AmoswapD,
                (0b00010, true) if instr.rs2 == 0 => Op::LrW,
                (0b00010, false) if instr.rs2 == 0 => Op::LrD,
                (0b00011, true) => Op::ScW,
                (0b00011, false) => Op::ScD,
                (0b00100, true) => Op::AmoxorW,
                (0b00100, false) => Op::AmoxorD,
                (0b01000, true) => Op::AmoorW,
                (0b01000, false) => Op::AmoorD,
                (0b01100, true) => Op::AmoandW,
                (0b01100, false) => Op::AmoandD,
                (0b10000, true) => Op::AmominW,
                (0b10000, false) => Op::AmominD,
                (0b10100, true) => Op::AmomaxW,
                (0b10100, false) => Op::AmomaxD,
                (0b11000, true) => Op::AmominuW,
                (0b11000, false) => Op::AmominuD,
                (0b11100, true) => Op::AmomaxuW,
                (0b11100, false) => Op::AmomaxuD,
                _ => {
                    return Err(format!(
                        "AMO/{:05b}/{:03b}: raw={:08x}",
                        funct5, funct3, raw
                    ))
                }
            };
        }
        group::LOAD_FP => {
            match funct3 {
                0b010 => {
                    instr.imm = imm_i(raw);
                    instr.op = Op::Flw;
                }
                0b011 => {
                    instr.imm = imm_i(raw);
                    instr.op = Op::Fld;
                }
                // Vector loads: imm holds bits 31:25 (nf, mew, mop, vm).
                0b000 => {
                    instr.imm = (raw >> 25) as i32;
                    instr.op = Op::Vle8V;
                }
                0b101 => {
                    instr.imm = (raw >> 25) as i32;
                    instr.op = Op::Vle16V;
                }
                0b110 => {
                    instr.imm = (raw >> 25) as i32;
                    instr.op = Op::Vle32V;
                }
                0b111 => {
                    instr.imm = (raw >> 25) as i32;
                    instr.op = Op::Vle64V;
                }
                _ => return bad(),
            }
        }
        group::STORE_FP => match funct3 {
            0b010 => {
                instr.imm = imm_s(raw);
                instr.op = Op::Fsw;
            }
            0b011 => {
                instr.imm = imm_s(raw);
                instr.op = Op::Fsd;
            }
            0b000 => {
                instr.imm = (raw >> 25) as i32;
                instr.op = Op::Vse8V;
            }
            0b101 => {
                instr.imm = (raw >> 25) as i32;
                instr.op = Op::Vse16V;
            }
            0b110 => {
                instr.imm = (raw >> 25) as i32;
                instr.op = Op::Vse32V;
            }
            0b111 => {
                instr.imm = (raw >> 25) as i32;
                instr.op = Op::Vse64V;
            }
            _ => return bad(),
        },
        group::OP_FP => {
            // imm holds the rounding mode.
            instr.imm = funct3 as i32;
            let rs2 = instr.rs2;
            instr.op = match funct7 {
                0b0000000 => Op::FaddS,
                0b0000001 => Op::FaddD,
                0b0000100 => Op::FsubS,
                0b0000101 => Op::FsubD,
                0b0001000 => Op::FmulS,
                0b0001001 => Op::FmulD,
                0b0001100 => Op::FdivS,
                0b0001101 => Op::FdivD,
                0b0101100 if rs2 == 0 => Op::FsqrtS,
                0b0101101 if rs2 == 0 => Op::FsqrtD,
                0b0010000 => match funct3 {
                    0 => Op::FsgnjS,
                    1 => Op::FsgnjnS,
                    2 => Op::FsgnjxS,
                    _ => return bad(),
                },
                0b0010001 => match funct3 {
                    0 => Op::FsgnjD,
                    1 => Op::FsgnjnD,
                    2 => Op::FsgnjxD,
                    _ => return bad(),
                },
                0b0010100 => match funct3 {
                    0 => Op::FminS,
                    1 => Op::FmaxS,
                    _ => return bad(),
                },
                0b0010101 => match funct3 {
                    0 => Op::FminD,
                    1 => Op::FmaxD,
                    _ => return bad(),
                },
                0b0100000 if rs2 == 1 => Op::FcvtSD,
                0b0100001 if rs2 == 0 => Op::FcvtDS,
                0b1010000 => match funct3 {
                    0 => Op::FleS,
                    1 => Op::FltS,
                    2 => Op::FeqS,
                    _ => return bad(),
                },
                0b1010001 => match funct3 {
                    0 => Op::FleD,
                    1 => Op::FltD,
                    2 => Op::FeqD,
                    _ => return bad(),
                },
                0b1100000 => match rs2 {
                    0 => Op::FcvtWS,
                    1 => Op::FcvtWuS,
                    2 => Op::FcvtLS,
                    3 => Op::FcvtLuS,
                    _ => return bad(),
                },
                0b1100001 => match rs2 {
                    0 => Op::FcvtWD,
                    1 => Op::FcvtWuD,
                    2 => Op::FcvtLD,
                    3 => Op::FcvtLuD,
                    _ => return bad(),
                },
                0b1101000 => match rs2 {
                    0 => Op::FcvtSW,
                    1 => Op::FcvtSWu,
                    2 => Op::FcvtSL,
                    3 => Op::FcvtSLu,
                    _ => return bad(),
                },
                0b1101001 => match rs2 {
                    0 => Op::FcvtDW,
                    1 => Op::FcvtDWu,
                    2 => Op::FcvtDL,
                    3 => Op::FcvtDLu,
                    _ => return bad(),
                },
                0b1110000 if rs2 == 0 => match funct3 {
                    0 => Op::FmvXW,
                    1 => Op::FclassS,
                    _ => return bad(),
                },
                0b1110001 if rs2 == 0 => match funct3 {
                    0 => Op::FmvXD,
                    1 => Op::FclassD,
                    _ => return bad(),
                },
                0b1111000 if rs2 == 0 && funct3 == 0 => Op::FmvWX,
                0b1111001 if rs2 == 0 && funct3 == 0 => Op::FmvDX,
                _ => return bad(),
            };
        }
        group::MADD | group::MSUB | group::NMSUB | group::NMADD => {
            let rs3 = (raw >> 27) & 0b11111;
            instr.imm = (rs3 | (funct3 << 5)) as i32;
            let double = match (raw >> 25) & 0b11 {
                0b00 => false,
                0b01 => true,
                _ => return bad(),
            };
            instr.op = match (grp, double) {
                (group::MADD, false) => Op::FmaddS,
                (group::MADD, true) => Op::FmaddD,
                (group::MSUB, false) => Op::FmsubS,
                (group::MSUB, true) => Op::FmsubD,
                (group::NMSUB, false) => Op::FnmsubS,
                (group::NMSUB, true) => Op::FnmsubD,
                (group::NMADD, false) => Op::FnmaddS,
                _ => Op::FnmaddD,
            };
        }
        group::OP_V => decode_opv(raw, funct3, funct7, &mut instr)?,
        _ => {
            if grp >> 2 == 0b111 {
                return Err("extended-length instructions not supported".to_string());
            }
            return Err(format!(
                "group {} not implemented yet: raw={:08x}",
                group::name(grp),
                raw
            ));
        }
    }

    Ok(instr)
}

fn decode_opv(raw: u32, funct3: u32, funct7: u32, instr: &mut Instr) -> Result<(), String> {
    let funct6 = funct7 >> 1;
    let vm = (funct7 & 1) as i32;
    let bad = || {
        Err(format!(
            "OP-V: funct3={:03b}, funct6={:06b}, raw={:08x}",
            funct3, funct6, raw
        ))
    };
    match funct3 {
        0b000 => {
            // OPIVV
            match funct6 {
                0b001011 => {
                    instr.imm = vm;
                    instr.op = Op::VxorVV;
                }
                0b010111 if vm == 1 && instr.rs2 == 0 => instr.op = Op::VmvVV,
                0b011001 => {
                    instr.imm = vm;
                    instr.op = Op::VmsneVV;
                }
                _ => return bad(),
            }
        }
        0b010 => {
            // OPMVV
            match funct6 {
                0b010100 if instr.rs1 == 0b10001 && instr.rs2 == 0 => {
                    instr.imm = vm;
                    instr.op = Op::VidV;
                }
                _ => return bad(),
            }
        }
        0b011 => {
            // OPIVI
            let simm5 = ((raw << 12) as i32) >> 27;
            match funct6 {
                0b000000 | 0b011000 => {
                    // Imm is sign-extended and shifted one bit left; bit
                    // 0 holds vm.
                    instr.imm = (simm5 << 1) | vm;
                    instr.op = if funct6 == 0 { Op::VaddVI } else { Op::VmseqVI };
                }
                0b010111 if vm == 1 && instr.rs2 == 0 => {
                    instr.imm = simm5;
                    instr.op = Op::VmvVI;
                }
                0b100101 | 0b101001 => {
                    // Imm holds uimm with vm in bit 31.
                    let mut imm = instr.rs1 as u32;
                    if vm != 0 {
                        imm |= 1 << 31;
                    }
                    instr.imm = imm as i32;
                    instr.op = if funct6 == 0b100101 {
                        Op::VsllVI
                    } else {
                        Op::VsraVI
                    };
                }
                _ => return bad(),
            }
        }
        0b100 => {
            // OPIVX
            match funct6 {
                0b001001 => {
                    instr.imm = vm;
                    instr.op = Op::VandVX;
                }
                0b010111 if vm == 1 && instr.rs2 == 0 => {
                    instr.imm = vm;
                    instr.op = Op::VmvVX;
                }
                _ => return bad(),
            }
        }
        0b111 => {
            if raw & (1 << 31) == 0 {
                instr.imm = ((raw >> 20) & 0b111_11111111) as i32;
                instr.op = Op::Vsetvli;
            } else if raw & (1 << 30) != 0 {
                // rs1 is uimm[4:0].
                instr.imm = ((raw >> 20) & 0b011_11111111) as i32;
                instr.op = Op::Vsetivli;
            } else {
                instr.op = Op::Vsetvl;
            }
        }
        _ => return bad(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_basic() {
        // addi a0,a0,-1
        let i = decode(0xfff50513).unwrap();
        assert_eq!(i.op, Op::Addi);
        assert_eq!(i.rd, 10);
        assert_eq!(i.rs1, 10);
        assert_eq!(i.imm, -1);

        // jal ra,-0x2c (fd5ff0ef)
        let i = decode(0xfd5ff0ef).unwrap();
        assert_eq!(i.op, Op::Jal);
        assert_eq!(i.rd, 1);
        assert_eq!(i.imm, -0x2c);

        // sd ra,8(sp)
        let i = decode(0x00113423).unwrap();
        assert_eq!(i.op, Op::Sd);
        assert_eq!(i.rs1, 2);
        assert_eq!(i.rs2, 1);
        assert_eq!(i.imm, 8);

        // bne a0,a1,-8: imm=-8 => 0xfeb51ce3
        let i = decode(0xfeb51ce3).unwrap();
        assert_eq!(i.op, Op::Bne);
        assert_eq!(i.imm, -8);
    }

    #[test]
    fn decode_compressed() {
        // c.addi sp,-16 => 0x1141
        let i = decode_c(0x1141).unwrap();
        assert_eq!(i.op, Op::Addi);
        assert_eq!(i.rd, 2);
        assert_eq!(i.imm, -16);

        // c.ldsp ra,8(sp) => 0x60a2
        let i = decode_c(0x60a2).unwrap();
        assert_eq!(i.op, Op::Ld);
        assert_eq!(i.rd, 1);
        assert_eq!(i.imm, 8);

        // c.ret => 0x8082
        let i = decode_c(0x8082).unwrap();
        assert_eq!(i.op, Op::Jalr);
        assert_eq!(i.rs1, 1);
        assert_eq!(i.rd, 0);

        // c.j -32 => 0xb7c5
        let i = decode_c(0xb7c5).unwrap();
        assert_eq!(i.op, Op::Jal);
        assert_eq!(i.imm, -32);
    }
}
