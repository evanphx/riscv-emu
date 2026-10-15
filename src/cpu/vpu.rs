//! Vector extension (subset).

use super::Cpu;
use crate::isa::{Exception, Instr, Op, RegStatus, Sew, VType};

/// Maximum element width.
pub const ELEN: u64 = 64;

/// Vector processing unit state.
pub struct Vpu {
    /// The 32 vector registers stored as a flat byte array; register
    /// n occupies bytes [n*VLEN/8, (n+1)*VLEN/8).
    pub vregs: Vec<u8>,
    pub vtype: VType,
    pub vl: u64,
    pub vstart: u64,
    pub vxrm: u8,
    pub vxsat: bool,
    /// Register width in bits.
    pub vlen: u64,
}

impl Default for Vpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Vpu {
    pub fn new() -> Vpu {
        let vlen = 128;
        Vpu {
            vregs: vec![0; (32 * vlen / 8) as usize],
            vtype: VType(0),
            vl: 0,
            vstart: 0,
            vxrm: 0,
            vxsat: false,
            vlen,
        }
    }

    /// Returns the byte offset of element n of size sew in the
    /// register group starting at vreg, or None if it is outside the
    /// register file.
    #[inline]
    fn elt(&self, sew: Sew, vreg: u64, n: u64) -> Option<usize> {
        let elts_per_reg = (self.vlen / 8) / sew.bytes();
        let reg = vreg + n / elts_per_reg;
        if reg >= 32 {
            return None;
        }
        Some((reg * (self.vlen / 8) + (n % elts_per_reg) * sew.bytes()) as usize)
    }

    fn get(&self, sew: Sew, vreg: u64, n: u64) -> Option<u64> {
        let o = self.elt(sew, vreg, n)?;
        let b = &self.vregs[o..o + sew.bytes() as usize];
        let mut v = 0u64;
        for (i, x) in b.iter().enumerate() {
            v |= (*x as u64) << (8 * i);
        }
        Some(v)
    }

    fn set(&mut self, sew: Sew, vreg: u64, n: u64, v: u64) -> Option<()> {
        let o = self.elt(sew, vreg, n)?;
        let len = sew.bytes() as usize;
        for i in 0..len {
            self.vregs[o + i] = (v >> (8 * i)) as u8;
        }
        Some(())
    }

    fn mask_elt(&self, vreg: u64, n: u64) -> bool {
        let o = (vreg * (self.vlen / 8) + n / 8) as usize;
        (self.vregs.get(o).copied().unwrap_or(0) >> (n % 8)) & 1 == 1
    }

    fn set_mask_elt(&mut self, vreg: u64, n: u64, v: bool) {
        let o = (vreg * (self.vlen / 8) + n / 8) as usize;
        if let Some(b) = self.vregs.get_mut(o) {
            let bit = 1u8 << (n % 8);
            if v {
                *b |= bit;
            } else {
                *b &= !bit;
            }
        }
    }
}

fn sext(v: u64, sew: Sew) -> i64 {
    let shift = 64 - sew.0;
    ((v << shift) as i64) >> shift
}

fn mask(sew: Sew) -> u64 {
    if sew.0 == 64 {
        u64::MAX
    } else {
        (1u64 << sew.0) - 1
    }
}

impl Cpu {
    fn require_align(&self, raw: u32, val: u64, pos: u64) -> Result<(), Exception> {
        if pos == 0 || val & (pos - 1) == 0 {
            return Ok(());
        }
        Err(self.illegal(raw))
    }

    fn vsew_checked(&self, raw: u32) -> Result<Sew, Exception> {
        let sew = self.vpu.vtype.vsew();
        if sew < Sew::E8 || sew > Sew::E64 || self.vpu.vtype.0 >> 63 != 0 {
            return Err(self.illegal(raw));
        }
        Ok(sew)
    }

    fn set_vl(&mut self, rd: u8, rs1: u8, req_vl: u64, new_type: VType) {
        let vlen = self.vpu.vlen;
        let vl;
        if self.vpu.vtype != new_type {
            let vlmul = new_type.vlmul();
            let vsew_max = (vlmul.min(1.0) * ELEN as f32) as u32;
            let max_vl = ((vlen / new_type.vsew().bytes() / 8) as f32 * vlmul) as u64;

            let vill = !(0.125..=8.0).contains(&vlmul)
                || new_type.vsew().0 > vsew_max
                || (new_type.0 >> 9) != 0
                || new_type.altfmt()
                || (rd == 0 && rs1 == 0 && self.vpu.vl != max_vl);
            if vill {
                vl = 0;
                self.vpu.vtype = VType(1 << 63);
            } else {
                vl = max_vl;
                self.vpu.vtype = new_type;
            }
        } else {
            let vlmul = new_type.vlmul();
            vl = ((vlen / new_type.vsew().bytes() / 8) as f32 * vlmul) as u64;
        }

        if vl == 0 {
            self.vpu.vl = 0;
        } else if rd == 0 && rs1 == 0 {
            // Retain current VL.
        } else if rd != 0 && rs1 == 0 {
            self.vpu.vl = vl;
        } else {
            self.vpu.vl = vl.min(req_vl);
        }

        self.set_x(rd, self.vpu.vl);
        self.vpu.vstart = 0;
    }

    pub(super) fn exec_vector(&mut self, i: Instr, raw: u32) -> Result<(), Exception> {
        use Op::*;

        if self.mstatus.vs() == RegStatus::Off {
            return Err(self.illegal(raw));
        }

        let vd = i.rd as u64;
        let vs1 = i.rs1 as u64;
        let vs2 = i.rs2 as u64;
        let bad = |cpu: &Cpu| cpu.illegal(raw);

        match i.op {
            Vsetvl => {
                let t = VType(self.x[i.rs2 as usize]);
                self.set_vl(i.rd, i.rs1, self.x[i.rs1 as usize], t);
            }
            Vsetvli => {
                self.set_vl(
                    i.rd,
                    i.rs1,
                    self.x[i.rs1 as usize],
                    VType(i.imm as u32 as u64),
                );
            }
            Vsetivli => {
                // rs1 = 0xff marks the immediate AVL form.
                self.set_vl(i.rd, 0xff, i.rs1 as u64, VType(i.imm as u32 as u64));
            }

            VmvVV | VmvVX | VmvVI | VandVX | VxorVV | VidV | VaddVI | VsllVI | VsraVI | VmsneVV => {
                let vlmul = self.vpu.vtype.vlmul();
                if vlmul > 1.0 {
                    self.require_align(raw, vd, vlmul as u64)?;
                    if matches!(i.op, VmvVV | VmsneVV) {
                        self.require_align(raw, vs1, vlmul as u64)?;
                    }
                }
                let sew = self.vsew_checked(raw)?;
                let m = mask(sew);

                let (vm, imm) = match i.op {
                    VandVX | VxorVV | VidV | VmsneVV => (i.imm & 1 != 0, 0i64),
                    VaddVI => (i.imm & 1 != 0, (i.imm >> 1) as i64),
                    VsllVI | VsraVI => (
                        (i.imm as u32) >> 31 != 0,
                        ((i.imm as u32) & !(1 << 31)) as i64,
                    ),
                    VmvVI => (true, i.imm as i64),
                    _ => (true, 0),
                };

                let rs1v = self.x[i.rs1 as usize];
                let mut n = self.vpu.vstart;
                while n < self.vpu.vl {
                    if !vm && !self.vpu.mask_elt(0, n) {
                        n += 1;
                        continue;
                    }
                    let a = |cpu: &Cpu, r: u64| cpu.vpu.get(sew, r, n).ok_or_else(|| bad(cpu));
                    let res = match i.op {
                        VmvVV => a(self, vs1)?,
                        VmvVX => rs1v,
                        VmvVI => imm as u64,
                        VandVX => a(self, vs2)? & rs1v,
                        VxorVV => a(self, vs1)? ^ a(self, vs2)?,
                        VidV => n,
                        VaddVI => (sext(a(self, vs2)?, sew).wrapping_add(imm)) as u64,
                        VsllVI => a(self, vs2)? << (imm as u64 & (sew.0 as u64 - 1)),
                        VsraVI => {
                            (sext(a(self, vs2)?, sew) >> (imm as u64 & (sew.0 as u64 - 1))) as u64
                        }
                        VmsneVV => {
                            let ne = a(self, vs1)? != a(self, vs2)?;
                            self.vpu.set_mask_elt(vd, n, ne);
                            n += 1;
                            continue;
                        }
                        _ => unreachable!(),
                    };
                    self.vpu.set(sew, vd, n, res & m).ok_or_else(|| bad(self))?;
                    n += 1;
                }
                self.vpu.vstart = 0;
            }

            Vle8V | Vle16V | Vle32V | Vle64V | Vse8V | Vse16V | Vse32V | Vse64V => {
                let vm = i.imm & 1 != 0;
                let mop = (i.imm >> 1) & 0b111;
                let nf = ((i.imm >> 4) & 0b111) as u64 + 1;
                if mop != 0 || nf != 1 {
                    return Err(bad(self));
                }
                let (eew, load) = match i.op {
                    Vle8V => (Sew::E8, true),
                    Vle16V => (Sew::E16, true),
                    Vle32V => (Sew::E32, true),
                    Vle64V => (Sew::E64, true),
                    Vse8V => (Sew::E8, false),
                    Vse16V => (Sew::E16, false),
                    Vse32V => (Sew::E32, false),
                    _ => (Sew::E64, false),
                };
                let elt_size = eew.bytes();
                let vemul = eew.0 as f32 / self.vpu.vtype.vsew().0 as f32 * self.vpu.vtype.vlmul();
                let emul = if vemul > 1.0 { vemul as u64 } else { 1 };

                let vl = self.vpu.vl;
                let base = self.x[i.rs1 as usize];

                let mut n = self.vpu.vstart;
                while n < vl {
                    if !vm && !self.vpu.mask_elt(0, n) {
                        n += 1;
                        continue;
                    }
                    self.vpu.vstart = n;
                    for f in 0..nf {
                        let addr = base.wrapping_add((n * nf + f) * elt_size);
                        let reg = vd + f * emul;
                        if load {
                            let v = match elt_size {
                                1 => self.load8(addr)? as u64,
                                2 => self.load16(addr)? as u64,
                                4 => self.load32(addr)? as u64,
                                _ => self.load64(addr)?,
                            };
                            self.vpu.set(eew, reg, n, v).ok_or_else(|| bad(self))?;
                        } else {
                            let v = self.vpu.get(eew, reg, n).ok_or_else(|| bad(self))?;
                            match elt_size {
                                1 => self.store8(addr, v as u8)?,
                                2 => self.store16(addr, v as u16)?,
                                4 => self.store32(addr, v as u32)?,
                                _ => self.store64(addr, v)?,
                            }
                        }
                    }
                    n += 1;
                }
                self.vpu.vstart = 0;
            }

            _ => return Err(bad(self)),
        }

        self.mstatus.set_vs(RegStatus::Dirty);
        self.mstatus.set_sd(true);
        Ok(())
    }
}
