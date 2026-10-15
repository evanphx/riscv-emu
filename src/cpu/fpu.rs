//! F and D extensions.

use super::softfloat::{self as sf, F32, F64};
use super::Cpu;
use crate::isa::{Exception, Instr, Op, RegStatus};

const BOX: u64 = 0xffff_ffff_0000_0000;

impl Cpu {
    /// Reads the single precision value of the register. Improperly
    /// NaN-boxed values read as the canonical NaN.
    #[inline]
    fn fs(&self, r: u8) -> u64 {
        let v = self.f[r as usize];
        if v & BOX == BOX {
            v & 0xffff_ffff
        } else {
            F32.nan()
        }
    }

    #[inline]
    fn set_fs(&mut self, r: u8, v: u64) {
        self.f[r as usize] = BOX | (v & 0xffff_ffff);
    }

    #[inline]
    fn fd(&self, r: u8) -> u64 {
        self.f[r as usize]
    }

    #[inline]
    fn set_fd(&mut self, r: u8, v: u64) {
        self.f[r as usize] = v;
    }

    /// Resolves the rounding mode of the instruction.
    fn rounding_mode(&self, i: &Instr, raw: u32) -> Result<u32, Exception> {
        let rm = i.rm();
        let rm = if rm == 7 { self.frm } else { rm };
        if rm > 4 {
            return Err(self.illegal(raw));
        }
        Ok(rm)
    }

    pub(super) fn exec_fp(&mut self, i: Instr, raw: u32) -> Result<(), Exception> {
        use Op::*;

        if self.mstatus.fs() == RegStatus::Off {
            return Err(self.illegal(raw));
        }

        let mut fl = 0u32;
        let (rd, rs1, rs2) = (i.rd, i.rs1, i.rs2);

        match i.op {
            Flw => {
                let v = self.load32(self.addr(&i))?;
                self.set_fs(rd, v as u64);
            }
            Fld => {
                let v = self.load64(self.addr(&i))?;
                self.set_fd(rd, v);
            }
            Fsw => {
                let v = self.f[rs2 as usize] as u32;
                self.store32(self.addr(&i), v)?;
                self.reservation_valid = false;
                return Ok(());
            }
            Fsd => {
                let v = self.f[rs2 as usize];
                self.store64(self.addr(&i), v)?;
                self.reservation_valid = false;
                return Ok(());
            }

            FaddS | FsubS | FmulS | FdivS => {
                let rm = self.rounding_mode(&i, raw)?;
                let (a, b) = (self.fs(rs1), self.fs(rs2));
                let v = match i.op {
                    FaddS => sf::add(F32, a, b, rm, &mut fl),
                    FsubS => sf::sub(F32, a, b, rm, &mut fl),
                    FmulS => sf::mul(F32, a, b, rm, &mut fl),
                    _ => sf::div(F32, a, b, rm, &mut fl),
                };
                self.set_fs(rd, v);
            }
            FaddD | FsubD | FmulD | FdivD => {
                let rm = self.rounding_mode(&i, raw)?;
                let (a, b) = (self.fd(rs1), self.fd(rs2));
                let v = match i.op {
                    FaddD => sf::add(F64, a, b, rm, &mut fl),
                    FsubD => sf::sub(F64, a, b, rm, &mut fl),
                    FmulD => sf::mul(F64, a, b, rm, &mut fl),
                    _ => sf::div(F64, a, b, rm, &mut fl),
                };
                self.set_fd(rd, v);
            }
            FsqrtS => {
                let rm = self.rounding_mode(&i, raw)?;
                let v = sf::sqrt(F32, self.fs(rs1), rm, &mut fl);
                self.set_fs(rd, v);
            }
            FsqrtD => {
                let rm = self.rounding_mode(&i, raw)?;
                let v = sf::sqrt(F64, self.fd(rs1), rm, &mut fl);
                self.set_fd(rd, v);
            }

            FmaddS | FmsubS | FnmsubS | FnmaddS => {
                let rm = self.rounding_mode(&i, raw)?;
                let (np, nc) = fma_negs(i.op);
                let rs3 = i.rs3() as u8;
                let v = sf::fma(
                    F32,
                    self.fs(rs1),
                    self.fs(rs2),
                    self.fs(rs3),
                    np,
                    nc,
                    rm,
                    &mut fl,
                );
                self.set_fs(rd, v);
            }
            FmaddD | FmsubD | FnmsubD | FnmaddD => {
                let rm = self.rounding_mode(&i, raw)?;
                let (np, nc) = fma_negs(i.op);
                let rs3 = i.rs3() as u8;
                let v = sf::fma(
                    F64,
                    self.fd(rs1),
                    self.fd(rs2),
                    self.fd(rs3),
                    np,
                    nc,
                    rm,
                    &mut fl,
                );
                self.set_fd(rd, v);
            }

            FsgnjS | FsgnjnS | FsgnjxS => {
                let (a, b) = (self.fs(rs1), self.fs(rs2));
                let sign = 1u64 << 31;
                let s = match i.op {
                    FsgnjS => b & sign,
                    FsgnjnS => !b & sign,
                    _ => (a ^ b) & sign,
                };
                self.set_fs(rd, (a & !sign) | s);
            }
            FsgnjD | FsgnjnD | FsgnjxD => {
                let (a, b) = (self.fd(rs1), self.fd(rs2));
                let sign = 1u64 << 63;
                let s = match i.op {
                    FsgnjD => b & sign,
                    FsgnjnD => !b & sign,
                    _ => (a ^ b) & sign,
                };
                self.set_fd(rd, (a & !sign) | s);
            }

            FminS | FmaxS => {
                let v = sf::min_max(F32, self.fs(rs1), self.fs(rs2), i.op == FmaxS, &mut fl);
                self.set_fs(rd, v);
            }
            FminD | FmaxD => {
                let v = sf::min_max(F64, self.fd(rs1), self.fd(rs2), i.op == FmaxD, &mut fl);
                self.set_fd(rd, v);
            }

            FeqS | FltS | FleS => {
                let (a, b) = (self.fs(rs1), self.fs(rs2));
                let v = match i.op {
                    FeqS => sf::eq(F32, a, b, &mut fl),
                    FltS => sf::lt(F32, a, b, &mut fl),
                    _ => sf::le(F32, a, b, &mut fl),
                };
                self.set_x(rd, v as u64);
                return self.fp_flags_only(fl);
            }
            FeqD | FltD | FleD => {
                let (a, b) = (self.fd(rs1), self.fd(rs2));
                let v = match i.op {
                    FeqD => sf::eq(F64, a, b, &mut fl),
                    FltD => sf::lt(F64, a, b, &mut fl),
                    _ => sf::le(F64, a, b, &mut fl),
                };
                self.set_x(rd, v as u64);
                return self.fp_flags_only(fl);
            }

            FcvtWS | FcvtWuS | FcvtLS | FcvtLuS | FcvtWD | FcvtWuD | FcvtLD | FcvtLuD => {
                let rm = self.rounding_mode(&i, raw)?;
                let (fmt, a) = match i.op {
                    FcvtWS | FcvtWuS | FcvtLS | FcvtLuS => (F32, self.fs(rs1)),
                    _ => (F64, self.fd(rs1)),
                };
                let v = match i.op {
                    FcvtWS | FcvtWD => {
                        sf::to_int(fmt, a, i32::MIN as i128, i32::MAX as i128, rm, &mut fl) as i32
                            as i64 as u64
                    }
                    FcvtWuS | FcvtWuD => {
                        sf::to_int(fmt, a, 0, u32::MAX as i128, rm, &mut fl) as u32 as i32 as i64
                            as u64
                    }
                    FcvtLS | FcvtLD => {
                        sf::to_int(fmt, a, i64::MIN as i128, i64::MAX as i128, rm, &mut fl) as i64
                            as u64
                    }
                    _ => sf::to_int(fmt, a, 0, u64::MAX as i128, rm, &mut fl) as u64,
                };
                self.set_x(rd, v);
                return self.fp_flags_only(fl);
            }

            FcvtSW | FcvtSWu | FcvtSL | FcvtSLu | FcvtDW | FcvtDWu | FcvtDL | FcvtDLu => {
                let rm = self.rounding_mode(&i, raw)?;
                let x = self.x[rs1 as usize];
                let n: i128 = match i.op {
                    FcvtSW | FcvtDW => x as i32 as i128,
                    FcvtSWu | FcvtDWu => x as u32 as i128,
                    FcvtSL | FcvtDL => x as i64 as i128,
                    _ => x as i128,
                };
                match i.op {
                    FcvtSW | FcvtSWu | FcvtSL | FcvtSLu => {
                        let v = sf::from_int(F32, n, rm, &mut fl);
                        self.set_fs(rd, v);
                    }
                    _ => {
                        let v = sf::from_int(F64, n, rm, &mut fl);
                        self.set_fd(rd, v);
                    }
                }
            }

            FcvtSD => {
                let rm = self.rounding_mode(&i, raw)?;
                let v = sf::convert(F64, F32, self.fd(rs1), rm, &mut fl);
                self.set_fs(rd, v);
            }
            FcvtDS => {
                let rm = self.rounding_mode(&i, raw)?;
                let v = sf::convert(F32, F64, self.fs(rs1), rm, &mut fl);
                self.set_fd(rd, v);
            }

            FmvXW => {
                let v = self.f[rs1 as usize] as u32 as i32 as i64 as u64;
                self.set_x(rd, v);
                return Ok(());
            }
            FmvXD => {
                let v = self.f[rs1 as usize];
                self.set_x(rd, v);
                return Ok(());
            }
            FmvWX => {
                let v = self.x[rs1 as usize];
                self.set_fs(rd, v);
            }
            FmvDX => {
                let v = self.x[rs1 as usize];
                self.set_fd(rd, v);
            }

            FclassS => {
                let v = sf::classify(F32, self.fs(rs1));
                self.set_x(rd, v);
                return Ok(());
            }
            FclassD => {
                let v = sf::classify(F64, self.fd(rs1));
                self.set_x(rd, v);
                return Ok(());
            }

            _ => {
                return Err(self.illegal(raw));
            }
        }

        self.fflags |= fl;
        self.set_fs_dirty();
        Ok(())
    }

    /// Accrues the exception flags of an instruction that writes only
    /// an integer register.
    fn fp_flags_only(&mut self, fl: u32) -> Result<(), Exception> {
        if fl != 0 {
            self.fflags |= fl;
            self.set_fs_dirty();
        }
        Ok(())
    }
}

/// Returns the product and addend negations of the fused
/// multiply-add instruction.
fn fma_negs(op: Op) -> (bool, bool) {
    match op {
        Op::FmaddS | Op::FmaddD => (false, false),
        Op::FmsubS | Op::FmsubD => (false, true),
        Op::FnmsubS | Op::FnmsubD => (true, false),
        _ => (true, true), // fnmadd
    }
}
