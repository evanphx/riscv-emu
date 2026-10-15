//! Software IEEE 754 binary32/binary64 arithmetic with RISC-V
//! semantics: all rounding modes, accrued exception flags, tininess
//! detection after rounding, and canonical NaN results.

/// Exception flags.
pub const NX: u32 = 1; // Inexact
pub const UF: u32 = 2; // Underflow
pub const OF: u32 = 4; // Overflow
pub const DZ: u32 = 8; // Divide by zero
pub const NV: u32 = 16; // Invalid operation

/// Rounding modes.
pub const RNE: u32 = 0;
pub const RTZ: u32 = 1;
pub const RDN: u32 = 2;
pub const RUP: u32 = 3;
pub const RMM: u32 = 4;

/// Floating point format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fmt {
    pub exp_bits: u32,
    pub frac_bits: u32,
}

pub const F32: Fmt = Fmt {
    exp_bits: 8,
    frac_bits: 23,
};

pub const F64: Fmt = Fmt {
    exp_bits: 11,
    frac_bits: 52,
};

impl Fmt {
    #[inline]
    fn bias(self) -> i32 {
        (1 << (self.exp_bits - 1)) - 1
    }

    #[inline]
    fn emax_biased(self) -> u64 {
        (1 << self.exp_bits) - 1
    }

    /// Precision in bits including the hidden bit.
    #[inline]
    fn prec(self) -> u32 {
        self.frac_bits + 1
    }

    /// Minimum normal binary exponent.
    #[inline]
    fn emin(self) -> i32 {
        1 - self.bias()
    }

    #[inline]
    fn sign_bit(self) -> u64 {
        1 << (self.exp_bits + self.frac_bits)
    }

    #[inline]
    fn frac_mask(self) -> u64 {
        (1 << self.frac_bits) - 1
    }

    /// The canonical NaN.
    pub fn nan(self) -> u64 {
        (self.emax_biased() << self.frac_bits) | (1 << (self.frac_bits - 1))
    }

    pub fn inf(self, sign: bool) -> u64 {
        ((sign as u64) * self.sign_bit()) | (self.emax_biased() << self.frac_bits)
    }

    pub fn zero(self, sign: bool) -> u64 {
        (sign as u64) * self.sign_bit()
    }

    fn max_finite(self, sign: bool) -> u64 {
        ((sign as u64) * self.sign_bit())
            | ((self.emax_biased() - 1) << self.frac_bits)
            | self.frac_mask()
    }

    pub fn is_nan(self, v: u64) -> bool {
        (v >> self.frac_bits) & self.emax_biased() == self.emax_biased() && v & self.frac_mask() != 0
    }

    pub fn is_snan(self, v: u64) -> bool {
        self.is_nan(v) && v & (1 << (self.frac_bits - 1)) == 0
    }

    pub fn sign(self, v: u64) -> bool {
        v & self.sign_bit() != 0
    }
}

/// Unpacked floating point value.
#[derive(Clone, Copy, Debug)]
enum Val {
    Nan,
    Inf(bool),
    Zero(bool),
    /// sign, exponent, significand: value = sig * 2^exp.
    Fin(bool, i32, u128),
}

fn unpack(fmt: Fmt, v: u64) -> Val {
    let sign = fmt.sign(v);
    let e = (v >> fmt.frac_bits) & fmt.emax_biased();
    let frac = v & fmt.frac_mask();
    if e == fmt.emax_biased() {
        if frac != 0 {
            Val::Nan
        } else {
            Val::Inf(sign)
        }
    } else if e == 0 {
        if frac == 0 {
            Val::Zero(sign)
        } else {
            Val::Fin(sign, fmt.emin() - fmt.frac_bits as i32, frac as u128)
        }
    } else {
        Val::Fin(
            sign,
            e as i32 - fmt.bias() - fmt.frac_bits as i32,
            (frac | (1 << fmt.frac_bits)) as u128,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rem {
    Zero,
    Below,
    Half,
    Above,
}

fn round_up(rm: u32, sign: bool, odd: bool, rem: Rem) -> bool {
    match rm {
        RNE => rem == Rem::Above || (rem == Rem::Half && odd),
        RTZ => false,
        RDN => sign && rem != Rem::Zero,
        RUP => !sign && rem != Rem::Zero,
        _ => rem == Rem::Half || rem == Rem::Above, // RMM
    }
}

/// Shifts sig right by shift bits. Returns the shifted value and the
/// remainder class. The sticky flag tells that there are nonzero
/// bits below sig's least significant bit.
fn shift_round(sig: u128, shift: i32, sticky: bool) -> (u128, Rem) {
    if shift <= 0 {
        let v = if shift <= -128 { 0 } else { sig << (-shift) };
        return (v, if sticky { Rem::Below } else { Rem::Zero });
    }
    if shift > 128 {
        let rem = if sig != 0 || sticky {
            Rem::Below
        } else {
            Rem::Zero
        };
        return (0, rem);
    }
    let (q, r, half) = if shift == 128 {
        (0u128, sig, 1u128 << 127)
    } else {
        (sig >> shift, sig & ((1u128 << shift) - 1), 1u128 << (shift - 1))
    };
    let rem = if r == 0 {
        if sticky {
            Rem::Below
        } else {
            Rem::Zero
        }
    } else if r < half {
        Rem::Below
    } else if r == half {
        if sticky {
            Rem::Above
        } else {
            Rem::Half
        }
    } else {
        Rem::Above
    };
    (q, rem)
}

#[inline]
fn msb(v: u128) -> i32 {
    127 - v.leading_zeros() as i32
}

/// Rounds and packs the value (-1)^sign * (sig + sticky) * 2^exp.
fn round_pack(fmt: Fmt, sign: bool, exp: i32, sig: u128, sticky: bool, rm: u32, flags: &mut u32) -> u64 {
    if sig == 0 {
        debug_assert!(!sticky);
        return fmt.zero(sign);
    }

    let p = fmt.prec() as i32;
    let e = exp + msb(sig); // value in [2^e, 2^(e+1))
    let emin = fmt.emin();

    let lsb_exp = if e >= emin { e - (p - 1) } else { emin - (p - 1) };
    let (mut q, rem) = shift_round(sig, lsb_exp - exp, sticky);
    let inexact = rem != Rem::Zero;
    if round_up(rm, sign, q & 1 == 1, rem) {
        q += 1;
    }
    let mut lsb_exp = lsb_exp;
    if q >> p != 0 {
        q >>= 1;
        lsb_exp += 1;
    }

    let normal = q >> (p - 1) != 0;
    let biased = if normal {
        (lsb_exp + p - 1 + fmt.bias()) as i64
    } else {
        0
    };

    if biased >= fmt.emax_biased() as i64 {
        *flags |= OF | NX;
        let to_inf = match rm {
            RNE | RMM => true,
            RTZ => false,
            RDN => sign,
            _ => !sign,
        };
        return if to_inf {
            fmt.inf(sign)
        } else {
            fmt.max_finite(sign)
        };
    }

    if inexact {
        *flags |= NX;
        if e < emin {
            // Tininess is detected after rounding: round with
            // unbounded exponent range and check if the result is
            // below the minimum normal.
            let ulsb = e - (p - 1);
            let (mut uq, urem) = shift_round(sig, ulsb - exp, sticky);
            if round_up(rm, sign, uq & 1 == 1, urem) {
                uq += 1;
            }
            let reaches_normal = e == emin - 1 && uq >> p != 0;
            if !reaches_normal {
                *flags |= UF;
            }
        }
    }

    let frac = (q as u64) & fmt.frac_mask();
    ((sign as u64) * fmt.sign_bit()) | ((biased as u64) << fmt.frac_bits) | frac
}

/// Adds two unpacked finite or zero operands given as (sign, exp,
/// sig). Zero significands are allowed.
fn add_parts(
    fmt: Fmt,
    (sa, ea, ma): (bool, i32, u128),
    (sb, eb, mb): (bool, i32, u128),
    rm: u32,
    flags: &mut u32,
) -> u64 {
    if ma == 0 && mb == 0 {
        let sign = if sa == sb { sa } else { rm == RDN };
        return fmt.zero(sign);
    }
    if ma == 0 {
        return round_pack(fmt, sb, eb, mb, false, rm, flags);
    }
    if mb == 0 {
        return round_pack(fmt, sa, ea, ma, false, rm, flags);
    }

    // Normalize both to have the msb at bit 125.
    let na = 125 - msb(ma);
    let nb = 125 - msb(mb);
    let (ma, ea) = (ma << na, ea - na);
    let (mb, eb) = (mb << nb, eb - nb);

    // Order by magnitude.
    let ((sa, ea, ma), (sb, eb, mb)) = if ea > eb || (ea == eb && ma >= mb) {
        ((sa, ea, ma), (sb, eb, mb))
    } else {
        ((sb, eb, mb), (sa, ea, ma))
    };

    // Align the smaller operand with jamming.
    let d = ea - eb;
    let mb = if d == 0 {
        mb
    } else if d >= 127 {
        1
    } else {
        let lost = mb & ((1u128 << d) - 1);
        (mb >> d) | (lost != 0) as u128
    };

    if sa == sb {
        round_pack(fmt, sa, ea, ma + mb, false, rm, flags)
    } else {
        let diff = ma - mb;
        if diff == 0 {
            return fmt.zero(rm == RDN);
        }
        round_pack(fmt, sa, ea, diff, false, rm, flags)
    }
}

fn nan_result(fmt: Fmt, ops: &[u64], flags: &mut u32) -> u64 {
    if ops.iter().any(|&v| fmt.is_snan(v)) {
        *flags |= NV;
    }
    fmt.nan()
}

fn parts(v: Val) -> (bool, i32, u128) {
    match v {
        Val::Zero(s) => (s, 0, 0),
        Val::Fin(s, e, m) => (s, e, m),
        _ => unreachable!(),
    }
}

/// a + b
pub fn add(fmt: Fmt, a: u64, b: u64, rm: u32, flags: &mut u32) -> u64 {
    let (ua, ub) = (unpack(fmt, a), unpack(fmt, b));
    match (ua, ub) {
        (Val::Nan, _) | (_, Val::Nan) => nan_result(fmt, &[a, b], flags),
        (Val::Inf(x), Val::Inf(y)) => {
            if x != y {
                *flags |= NV;
                fmt.nan()
            } else {
                fmt.inf(x)
            }
        }
        (Val::Inf(x), _) | (_, Val::Inf(x)) => fmt.inf(x),
        _ => add_parts(fmt, parts(ua), parts(ub), rm, flags),
    }
}

/// a - b
pub fn sub(fmt: Fmt, a: u64, b: u64, rm: u32, flags: &mut u32) -> u64 {
    if fmt.is_nan(b) {
        return nan_result(fmt, &[a, b], flags);
    }
    add(fmt, a, b ^ fmt.sign_bit(), rm, flags)
}

/// a * b
pub fn mul(fmt: Fmt, a: u64, b: u64, rm: u32, flags: &mut u32) -> u64 {
    let (ua, ub) = (unpack(fmt, a), unpack(fmt, b));
    let sign = fmt.sign(a) ^ fmt.sign(b);
    match (ua, ub) {
        (Val::Nan, _) | (_, Val::Nan) => nan_result(fmt, &[a, b], flags),
        (Val::Inf(_), Val::Zero(_)) | (Val::Zero(_), Val::Inf(_)) => {
            *flags |= NV;
            fmt.nan()
        }
        (Val::Inf(_), _) | (_, Val::Inf(_)) => fmt.inf(sign),
        (Val::Zero(_), _) | (_, Val::Zero(_)) => fmt.zero(sign),
        (Val::Fin(_, ea, ma), Val::Fin(_, eb, mb)) => {
            round_pack(fmt, sign, ea + eb, ma * mb, false, rm, flags)
        }
    }
}

/// a / b
pub fn div(fmt: Fmt, a: u64, b: u64, rm: u32, flags: &mut u32) -> u64 {
    let (ua, ub) = (unpack(fmt, a), unpack(fmt, b));
    let sign = fmt.sign(a) ^ fmt.sign(b);
    match (ua, ub) {
        (Val::Nan, _) | (_, Val::Nan) => nan_result(fmt, &[a, b], flags),
        (Val::Inf(_), Val::Inf(_)) | (Val::Zero(_), Val::Zero(_)) => {
            *flags |= NV;
            fmt.nan()
        }
        (Val::Inf(_), _) => fmt.inf(sign),
        (_, Val::Inf(_)) => fmt.zero(sign),
        (Val::Zero(_), _) => fmt.zero(sign),
        (_, Val::Zero(_)) => {
            *flags |= DZ;
            fmt.inf(sign)
        }
        (Val::Fin(_, ea, ma), Val::Fin(_, eb, mb)) => {
            let sa = 126 - msb(ma);
            let sb = 63 - msb(mb);
            let na = ma << sa;
            let nb = mb << sb;
            let q = na / nb;
            let r = na % nb;
            round_pack(fmt, sign, (ea - sa) - (eb - sb), q, r != 0, rm, flags)
        }
    }
}

fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    // Newton iteration starting from an overestimate.
    let mut x = 1u128 << ((msb(n) / 2) + 1);
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// sqrt(a)
pub fn sqrt(fmt: Fmt, a: u64, rm: u32, flags: &mut u32) -> u64 {
    match unpack(fmt, a) {
        Val::Nan => nan_result(fmt, &[a], flags),
        Val::Inf(false) => a,
        Val::Zero(_) => a,
        Val::Inf(true) | Val::Fin(true, _, _) => {
            *flags |= NV;
            fmt.nan()
        }
        Val::Fin(false, e, m) => {
            let mut shift = 125 - msb(m);
            if (e - shift) % 2 != 0 {
                shift += 1;
            }
            let n = m << shift;
            let r = isqrt(n);
            let rem = n - r * r;
            round_pack(fmt, false, (e - shift) / 2, r, rem != 0, rm, flags)
        }
    }
}

/// a * b + c with a single rounding. The neg_prod and neg_c flags
/// negate the product and the addend.
pub fn fma(fmt: Fmt, a: u64, b: u64, c: u64, neg_prod: bool, neg_c: bool, rm: u32, flags: &mut u32) -> u64 {
    let (ua, ub, uc) = (unpack(fmt, a), unpack(fmt, b), unpack(fmt, c));
    if matches!(ua, Val::Nan) || matches!(ub, Val::Nan) {
        return nan_result(fmt, &[a, b, c], flags);
    }
    let inf_zero = matches!(
        (ua, ub),
        (Val::Inf(_), Val::Zero(_)) | (Val::Zero(_), Val::Inf(_))
    );
    if inf_zero {
        *flags |= NV;
        if fmt.is_snan(c) {
            *flags |= NV;
        }
        return fmt.nan();
    }
    if matches!(uc, Val::Nan) {
        return nan_result(fmt, &[c], flags);
    }
    let psign = fmt.sign(a) ^ fmt.sign(b) ^ neg_prod;
    let csign = fmt.sign(c) ^ neg_c;
    let prod_inf = matches!(ua, Val::Inf(_)) || matches!(ub, Val::Inf(_));
    if prod_inf {
        if let Val::Inf(_) = uc {
            if csign != psign {
                *flags |= NV;
                return fmt.nan();
            }
        }
        return fmt.inf(psign);
    }
    if let Val::Inf(_) = uc {
        return fmt.inf(csign);
    }
    let prod = match (ua, ub) {
        (Val::Fin(_, ea, ma), Val::Fin(_, eb, mb)) => (psign, ea + eb, ma * mb),
        _ => (psign, 0, 0),
    };
    let (_, ec, mc) = parts(uc);
    add_parts(fmt, prod, (csign, ec, mc), rm, flags)
}

/// Converts between formats.
pub fn convert(from: Fmt, to: Fmt, a: u64, rm: u32, flags: &mut u32) -> u64 {
    match unpack(from, a) {
        Val::Nan => {
            nan_result(from, &[a], flags);
            to.nan()
        }
        Val::Inf(s) => to.inf(s),
        Val::Zero(s) => to.zero(s),
        Val::Fin(s, e, m) => round_pack(to, s, e, m, false, rm, flags),
    }
}

/// Converts an integer to floating point.
pub fn from_int(fmt: Fmt, v: i128, rm: u32, flags: &mut u32) -> u64 {
    if v == 0 {
        return fmt.zero(false);
    }
    let sign = v < 0;
    round_pack(fmt, sign, 0, v.unsigned_abs(), false, rm, flags)
}

/// Converts floating point to an integer in the range [min, max]
/// with the rounding mode.
pub fn to_int(fmt: Fmt, a: u64, min: i128, max: i128, rm: u32, flags: &mut u32) -> i128 {
    match unpack(fmt, a) {
        Val::Nan => {
            *flags |= NV;
            max
        }
        Val::Inf(s) => {
            *flags |= NV;
            if s {
                min
            } else {
                max
            }
        }
        Val::Zero(_) => 0,
        Val::Fin(s, e, m) => {
            let (mag, rem) = if e >= 0 {
                if e > 64 {
                    *flags |= NV;
                    return if s { min } else { max };
                }
                (m << e, Rem::Zero)
            } else {
                let (q, rem) = shift_round(m, -e, false);
                let q = if round_up(rm, s, q & 1 == 1, rem) {
                    q + 1
                } else {
                    q
                };
                (q, rem)
            };
            let v = if s { -(mag as i128) } else { mag as i128 };
            if v < min || v > max {
                *flags |= NV;
                return if s { min } else { max };
            }
            if rem != Rem::Zero {
                *flags |= NX;
            }
            v
        }
    }
}

/// Floating point comparison. Returns None for unordered operands.
fn compare(fmt: Fmt, a: u64, b: u64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if fmt.is_nan(a) || fmt.is_nan(b) {
        return None;
    }
    let mag_a = a & !fmt.sign_bit();
    let mag_b = b & !fmt.sign_bit();
    if mag_a == 0 && mag_b == 0 {
        return Some(Ordering::Equal);
    }
    let (sa, sb) = (fmt.sign(a), fmt.sign(b));
    Some(match (sa, sb) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => mag_a.cmp(&mag_b),
        (true, true) => mag_b.cmp(&mag_a),
    })
}

pub fn eq(fmt: Fmt, a: u64, b: u64, flags: &mut u32) -> bool {
    if fmt.is_snan(a) || fmt.is_snan(b) {
        *flags |= NV;
    }
    compare(fmt, a, b) == Some(std::cmp::Ordering::Equal)
}

pub fn lt(fmt: Fmt, a: u64, b: u64, flags: &mut u32) -> bool {
    match compare(fmt, a, b) {
        None => {
            *flags |= NV;
            false
        }
        Some(o) => o == std::cmp::Ordering::Less,
    }
}

pub fn le(fmt: Fmt, a: u64, b: u64, flags: &mut u32) -> bool {
    match compare(fmt, a, b) {
        None => {
            *flags |= NV;
            false
        }
        Some(o) => o != std::cmp::Ordering::Greater,
    }
}

/// fmin/fmax with the IEEE 754-2019 minimumNumber/maximumNumber
/// semantics used by RISC-V.
pub fn min_max(fmt: Fmt, a: u64, b: u64, max: bool, flags: &mut u32) -> u64 {
    if fmt.is_snan(a) || fmt.is_snan(b) {
        *flags |= NV;
    }
    match (fmt.is_nan(a), fmt.is_nan(b)) {
        (true, true) => return fmt.nan(),
        (true, false) => return b,
        (false, true) => return a,
        _ => {}
    }
    let ord = compare(fmt, a, b).unwrap();
    let a_first = match ord {
        std::cmp::Ordering::Less => !max,
        std::cmp::Ordering::Greater => max,
        std::cmp::Ordering::Equal => {
            // -0 < +0
            if max {
                !fmt.sign(a)
            } else {
                fmt.sign(a)
            }
        }
    };
    if a_first {
        a
    } else {
        b
    }
}

/// Classifies the value.
pub fn classify(fmt: Fmt, a: u64) -> u64 {
    let sign = fmt.sign(a);
    match unpack(fmt, a) {
        Val::Inf(true) => 1 << 0,
        Val::Inf(false) => 1 << 7,
        Val::Zero(true) => 1 << 3,
        Val::Zero(false) => 1 << 4,
        Val::Nan => {
            if fmt.is_snan(a) {
                1 << 8
            } else {
                1 << 9
            }
        }
        Val::Fin(_, _, _) => {
            let subnormal = (a >> fmt.frac_bits) & fmt.emax_biased() == 0;
            match (sign, subnormal) {
                (true, false) => 1 << 1,
                (true, true) => 1 << 2,
                (false, true) => 1 << 5,
                (false, false) => 1 << 6,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f64b(v: f64) -> u64 {
        v.to_bits()
    }

    #[test]
    fn basic_ops_match_host() {
        let vals = [
            0.0, -0.0, 1.0, -1.5, 3.25, 1e300, -1e-300, 2.2250738585072014e-308, 5e-324,
            123456.789, 0.1, 1.0 / 3.0,
        ];
        for &a in &vals {
            for &b in &vals {
                let mut fl = 0;
                assert_eq!(add(F64, f64b(a), f64b(b), RNE, &mut fl), f64b(a + b), "{}+{}", a, b);
                assert_eq!(mul(F64, f64b(a), f64b(b), RNE, &mut fl), f64b(a * b), "{}*{}", a, b);
                if b != 0.0 {
                    assert_eq!(div(F64, f64b(a), f64b(b), RNE, &mut fl), f64b(a / b), "{}/{}", a, b);
                }
                let c = 0.75;
                assert_eq!(
                    fma(F64, f64b(a), f64b(b), f64b(c), false, false, RNE, &mut fl),
                    f64b(a.mul_add(b, c)),
                    "fma {} {}",
                    a,
                    b
                );
            }
            if a >= 0.0 {
                let mut fl = 0;
                assert_eq!(sqrt(F64, f64b(a), RNE, &mut fl), f64b(a.sqrt()));
            }
        }
    }

    #[test]
    fn f32_ops() {
        let mut fl = 0;
        let a = 1.1f32.to_bits() as u64;
        let b = 2.2f32.to_bits() as u64;
        assert_eq!(add(F32, a, b, RNE, &mut fl), (1.1f32 + 2.2f32).to_bits() as u64);
        assert_eq!(div(F32, a, b, RNE, &mut fl), (1.1f32 / 2.2f32).to_bits() as u64);
        assert_eq!(sqrt(F32, b, RNE, &mut fl), 2.2f32.sqrt().to_bits() as u64);
    }

    #[test]
    fn flags() {
        let mut fl = 0;
        div(F64, f64b(1.0), f64b(0.0), RNE, &mut fl);
        assert_eq!(fl, DZ);
        fl = 0;
        div(F64, f64b(1.0), f64b(3.0), RNE, &mut fl);
        assert_eq!(fl, NX);
        fl = 0;
        mul(F64, f64b(1e300), f64b(1e300), RNE, &mut fl);
        assert_eq!(fl, OF | NX);
        fl = 0;
        let r = to_int(F64, f64b(-1.5), i32::MIN as i128, i32::MAX as i128, RNE, &mut fl);
        assert_eq!(r, -2);
        assert_eq!(fl, NX);
        fl = 0;
        let r = to_int(F64, f64b(-0.5), 0, u32::MAX as i128, RTZ, &mut fl);
        assert_eq!(r, 0);
        assert_eq!(fl, NX);
    }

    #[test]
    fn rounding_modes() {
        let mut fl = 0;
        let third_rz = div(F64, f64b(1.0), f64b(3.0), RTZ, &mut fl);
        let third_ru = div(F64, f64b(1.0), f64b(3.0), RUP, &mut fl);
        assert_eq!(third_ru, third_rz + 1);
        let neg_rd = div(F64, f64b(-1.0), f64b(3.0), RDN, &mut fl);
        assert_eq!(neg_rd & !F64.sign_bit(), third_ru);
    }
}
