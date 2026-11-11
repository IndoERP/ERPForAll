//! Private bounded natural numbers for [`ExactRatio`](super::ExactRatio).
//!
//! A `Nat` is a little-endian `u64` limb vector without trailing zero limbs. The type itself does
//! not enforce the 4096-bit bound: [`ExactRatio`](super::ExactRatio) checks every stored value and
//! every product before it allocates (see `ratio.rs`). A division by zero returns `None`; every
//! index below is bounded by a length established in the same function.

use std::cmp::Ordering;

const LIMB_BITS: u32 = u64::BITS;
/// `10^19`, the largest power of ten that fits in a limb; text conversion works in these chunks.
const TEN_POW_19: u64 = 10_000_000_000_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub(super) struct Nat {
    limbs: Vec<u64>,
}

impl Nat {
    pub(super) fn zero() -> Self {
        Self { limbs: Vec::new() }
    }

    pub(super) fn from_u128(value: u128) -> Self {
        let mut limbs = vec![value as u64, (value >> LIMB_BITS) as u64];
        trim(&mut limbs);
        Self { limbs }
    }

    pub(super) fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    pub(super) fn is_one(&self) -> bool {
        self.limbs.as_slice() == [1]
    }

    /// Number of significant bits (`0` for zero).
    pub(super) fn bits(&self) -> u32 {
        match self.limbs.last() {
            None => 0,
            Some(top) => {
                // The limb count is bounded by the callers' 4096-bit checks, far below u32::MAX / 64.
                (self.limbs.len() as u32 - 1) * LIMB_BITS + (LIMB_BITS - top.leading_zeros())
            }
        }
    }

    pub(super) fn is_odd(&self) -> bool {
        self.limbs.first().is_some_and(|l| l & 1 == 1)
    }

    /// The value, if it fits in 128 bits.
    pub(super) fn to_u128(&self) -> Option<u128> {
        match self.limbs.as_slice() {
            [] => Some(0),
            [a] => Some(u128::from(*a)),
            [a, b] => Some(u128::from(*a) | (u128::from(*b) << LIMB_BITS)),
            _ => None,
        }
    }

    pub(super) fn add(&self, other: &Nat) -> Nat {
        let (long, short) = if self.limbs.len() >= other.limbs.len() {
            (&self.limbs, &other.limbs)
        } else {
            (&other.limbs, &self.limbs)
        };
        let mut out = Vec::with_capacity(long.len() + 1);
        let mut carry = false;
        for (i, &a) in long.iter().enumerate() {
            let b = short.get(i).copied().unwrap_or(0);
            let (s1, c1) = a.overflowing_add(b);
            let (s2, c2) = s1.overflowing_add(u64::from(carry));
            out.push(s2);
            carry = c1 || c2;
        }
        if carry {
            out.push(1);
        }
        Nat { limbs: out }
    }

    /// `self - other`, or `None` when `other > self`.
    pub(super) fn checked_sub(&self, other: &Nat) -> Option<Nat> {
        if self.cmp(other) == Ordering::Less {
            return None;
        }
        let mut out = Vec::with_capacity(self.limbs.len());
        let mut borrow = false;
        for (i, &a) in self.limbs.iter().enumerate() {
            let b = other.limbs.get(i).copied().unwrap_or(0);
            let (d1, b1) = a.overflowing_sub(b);
            let (d2, b2) = d1.overflowing_sub(u64::from(borrow));
            out.push(d2);
            borrow = b1 || b2;
        }
        trim(&mut out);
        Some(Nat { limbs: out })
    }

    pub(super) fn mul(&self, other: &Nat) -> Nat {
        if self.is_zero() || other.is_zero() {
            return Nat::zero();
        }
        let mut out = vec![0u64; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry: u128 = 0;
            for (j, &b) in other.limbs.iter().enumerate() {
                // a*b + out + carry <= (2^64-1)^2 + 2*(2^64-1) = 2^128 - 1: never overflows
                let cur = u128::from(a) * u128::from(b) + u128::from(out[i + j]) + carry;
                out[i + j] = cur as u64;
                carry = cur >> LIMB_BITS;
            }
            out[i + other.limbs.len()] = carry as u64;
        }
        trim(&mut out);
        Nat { limbs: out }
    }

    pub(super) fn mul_small(&self, factor: u64) -> Nat {
        if factor == 0 || self.is_zero() {
            return Nat::zero();
        }
        let mut out = Vec::with_capacity(self.limbs.len() + 1);
        let mut carry: u128 = 0;
        for &a in &self.limbs {
            let cur = u128::from(a) * u128::from(factor) + carry;
            out.push(cur as u64);
            carry = cur >> LIMB_BITS;
        }
        if carry != 0 {
            out.push(carry as u64);
        }
        Nat { limbs: out }
    }

    /// `(self / d, self % d)` for a limb-sized non-zero divisor.
    fn divrem_small(&self, d: u64) -> (Nat, u64) {
        debug_assert!(d != 0);
        let mut out = vec![0u64; self.limbs.len()];
        let mut rem: u128 = 0;
        for (i, &limb) in self.limbs.iter().enumerate().rev() {
            let cur = (rem << LIMB_BITS) | u128::from(limb);
            out[i] = (cur / u128::from(d)) as u64;
            rem = cur % u128::from(d);
        }
        trim(&mut out);
        (Nat { limbs: out }, rem as u64)
    }

    /// `(self / d, self % d)`; `None` when `d` is zero (Knuth, TAOCP 4.3.1, algorithm D).
    pub(super) fn divrem(&self, d: &Nat) -> Option<(Nat, Nat)> {
        if d.is_zero() {
            return None;
        }
        if self.cmp(d) == Ordering::Less {
            return Some((Nat::zero(), self.clone()));
        }
        if let [single] = d.limbs.as_slice() {
            let (q, r) = self.divrem_small(*single);
            return Some((q, Nat::from_u128(u128::from(r))));
        }
        let n = d.limbs.len();
        // self >= d, so self has at least n limbs; n >= 2 here
        let m = self.limbs.len() - n;
        let shift = d.limbs.last().map_or(0, |top| top.leading_zeros());
        let v = shl_limbs(&d.limbs, shift, false);
        let mut u = shl_limbs(&self.limbs, shift, true);
        let mut q = vec![0u64; m + 1];
        let base: u128 = 1 << LIMB_BITS;
        let v_top = u128::from(v[n - 1]);
        let v_next = u128::from(v[n - 2]);
        for j in (0..=m).rev() {
            let num = (u128::from(u[j + n]) << LIMB_BITS) | u128::from(u[j + n - 1]);
            let mut qhat = num / v_top;
            let mut rhat = num % v_top;
            while qhat >= base || qhat * v_next > ((rhat << LIMB_BITS) | u128::from(u[j + n - 2])) {
                qhat -= 1;
                rhat += v_top;
                if rhat >= base {
                    break;
                }
            }
            // multiply and subtract qhat * v from u[j..=j+n]
            let mut borrow = 0u64;
            let mut carry = 0u64;
            for i in 0..n {
                let p = qhat * u128::from(v[i]) + u128::from(carry);
                carry = (p >> LIMB_BITS) as u64;
                let (t, b1) = u[i + j].overflowing_sub(p as u64);
                let (t, b2) = t.overflowing_sub(borrow);
                u[i + j] = t;
                borrow = u64::from(b1) + u64::from(b2);
            }
            let (t, b1) = u[j + n].overflowing_sub(carry);
            let (t, b2) = t.overflowing_sub(borrow);
            u[j + n] = t;
            if b1 || b2 {
                // qhat was one too large: add the divisor back
                qhat -= 1;
                let mut c = 0u128;
                for i in 0..n {
                    let s = u128::from(u[i + j]) + u128::from(v[i]) + c;
                    u[i + j] = s as u64;
                    c = s >> LIMB_BITS;
                }
                u[j + n] = u[j + n].wrapping_add(c as u64);
            }
            q[j] = qhat as u64;
        }
        trim(&mut q);
        u.truncate(n);
        let mut rem = shr_limbs(&u, shift);
        trim(&mut rem);
        Some((Nat { limbs: q }, Nat { limbs: rem }))
    }

    /// Greatest common divisor (Euclid); `gcd(0, x) = x`.
    pub(super) fn gcd(&self, other: &Nat) -> Nat {
        let mut a = self.clone();
        let mut b = other.clone();
        while !b.is_zero() {
            let Some((_, r)) = a.divrem(&b) else { break };
            a = b;
            b = r;
        }
        a
    }

    /// `10^exp`.
    pub(super) fn pow10(exp: u32) -> Nat {
        let mut out = Nat::from_u128(1);
        let mut left = exp;
        while left > 0 {
            let step = left.min(19);
            out = out.mul_small(10u64.pow(step));
            left -= step;
        }
        out
    }

    /// Canonical decimal digits (no sign, no leading zeros, `"0"` for zero).
    pub(super) fn to_decimal_string(&self) -> String {
        if self.is_zero() {
            return "0".to_owned();
        }
        let mut chunks: Vec<u64> = Vec::new();
        let mut cur = self.clone();
        while !cur.is_zero() {
            let (q, r) = cur.divrem_small(TEN_POW_19);
            chunks.push(r);
            cur = q;
        }
        let mut out = String::new();
        for (i, chunk) in chunks.iter().rev().enumerate() {
            if i == 0 {
                out.push_str(&chunk.to_string());
            } else {
                out.push_str(&format!("{chunk:019}"));
            }
        }
        out
    }
}

impl PartialOrd for Nat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Nat {
    fn cmp(&self, other: &Self) -> Ordering {
        self.limbs
            .len()
            .cmp(&other.limbs.len())
            .then_with(|| self.limbs.iter().rev().cmp(other.limbs.iter().rev()))
    }
}

fn trim(limbs: &mut Vec<u64>) {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
}

/// `limbs << shift` for `shift < 64`; with `extra` the result has one more (possibly zero) limb.
fn shl_limbs(limbs: &[u64], shift: u32, extra: bool) -> Vec<u64> {
    let mut out = Vec::with_capacity(limbs.len() + 1);
    if shift == 0 {
        out.extend_from_slice(limbs);
        if extra {
            out.push(0);
        }
        return out;
    }
    let mut carry = 0u64;
    for &l in limbs {
        out.push((l << shift) | carry);
        carry = l >> (LIMB_BITS - shift);
    }
    if extra {
        out.push(carry);
    }
    out
}

/// `limbs >> shift` for `shift < 64`.
fn shr_limbs(limbs: &[u64], shift: u32) -> Vec<u64> {
    if shift == 0 {
        return limbs.to_vec();
    }
    let mut out = Vec::with_capacity(limbs.len());
    for (i, &l) in limbs.iter().enumerate() {
        let high = limbs.get(i + 1).copied().unwrap_or(0);
        out.push((l >> shift) | (high << (LIMB_BITS - shift)));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn nat(value: u128) -> Nat {
        Nat::from_u128(value)
    }

    fn big(parts: &[u64]) -> Nat {
        let mut limbs = parts.to_vec();
        trim(&mut limbs);
        Nat { limbs }
    }

    #[test]
    fn small_values_match_u128() {
        for (a, b) in [
            (0u128, 5u128),
            (7, 3),
            (u128::MAX, 1),
            (u128::MAX, u128::from(u64::MAX)),
        ] {
            let (q, r) = nat(a).divrem(&nat(b)).unwrap();
            assert_eq!((q.to_u128(), r.to_u128()), (Some(a / b), Some(a % b)));
        }
        assert!(nat(1).divrem(&Nat::zero()).is_none());
    }

    #[test]
    fn decimal_text_of_wide_values() {
        let two_pow_192 = big(&[0, 0, 0, 1]);
        assert_eq!(
            two_pow_192.to_decimal_string(),
            "6277101735386680763835789423207666416102355444464034512896"
        );
        assert_eq!(
            Nat::pow10(30).to_decimal_string(),
            format!("1{}", "0".repeat(30))
        );
        assert_eq!(Nat::zero().to_decimal_string(), "0");
    }

    /// Limbs biased to the edge values that exercise the quotient-estimate correction.
    fn edge_limb() -> impl Strategy<Value = u64> {
        prop_oneof![
            Just(0u64),
            Just(1u64),
            Just(u64::MAX),
            Just(1u64 << 63),
            Just((1u64 << 63) - 1),
            any::<u64>()
        ]
    }

    #[test]
    fn division_add_back_vector() {
        let u = big(&[0, 0xffff_ffff_ffff_fffe, 0x8000_0000_0000_0000]);
        let v = big(&[0xffff_ffff_ffff_ffff, 0x8000_0000_0000_0000]);
        let (q, r) = u.divrem(&v).unwrap();
        assert!(r < v);
        assert_eq!(q.mul(&v).add(&r), u);
    }

    prop_compose! {
        fn limbs(max: usize)(v in proptest::collection::vec(edge_limb(), 0..max)) -> Nat {
            let mut v = v;
            trim(&mut v);
            Nat { limbs: v }
        }
    }

    proptest! {
        #[test]
        fn division_identity(a in limbs(10), b in limbs(6)) {
            prop_assume!(!b.is_zero());
            let (q, r) = a.divrem(&b).unwrap();
            prop_assert!(r < b);
            prop_assert_eq!(q.mul(&b).add(&r), a);
        }

        #[test]
        fn gcd_divides_and_scales(a in limbs(5), b in limbs(5), k in limbs(3)) {
            prop_assume!(!a.is_zero() && !b.is_zero() && !k.is_zero());
            let g = a.gcd(&b);
            prop_assert!(a.divrem(&g).unwrap().1.is_zero());
            prop_assert!(b.divrem(&g).unwrap().1.is_zero());
            prop_assert_eq!(k.mul(&a).gcd(&k.mul(&b)), k.mul(&g));
        }

        #[test]
        fn add_then_sub(a in limbs(6), b in limbs(6)) {
            prop_assert_eq!(a.add(&b).checked_sub(&b), Some(a));
        }
    }
}
