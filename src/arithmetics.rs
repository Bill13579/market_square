#[cfg(feature = "small-gen")]
use core::sync::atomic::AtomicU32;
#[cfg(feature = "small-gen")]
pub type AtomicType = AtomicU32;
#[cfg(feature = "small-gen")]
pub type NumericType = u32;

#[cfg(not(feature = "small-gen"))]
use core::sync::atomic::AtomicU64;
#[cfg(not(feature = "small-gen"))]
pub type AtomicType = AtomicU64;
#[cfg(not(feature = "small-gen"))]
pub type NumericType = u64;

pub const MSB: NumericType = NumericType::MAX - (NumericType::MAX >> 1); // MSB set, all other bits clear

/// Wrapping-aware distance (a - b) between two numbers.
/// Handles the case where `a` has wrapped past MAX but `b` hasn't.
#[inline]
pub fn gen_dist_msb_masked(a: NumericType, b: NumericType) -> NumericType {
    a.wrapping_sub(b) & !MSB
}

/// Wrapping-aware comparison: is `a` ahead of or equal to `b`?
/// Returns true if a >= b in the circular sense (distance < MAX/**4**).
#[inline]
pub fn gen_gte_msb_masked(a: NumericType, b: NumericType) -> bool {
    // If a == b, distance is 0.
    // If a is "ahead" of b (even across wrap), wrapping_sub gives a small positive number.
    // If a is "behind" b, wrapping_sub gives a huge number (> MAX/4).
    a.wrapping_sub(b) & !MSB <= (NumericType::MAX / 4)
}

/// Wrapping-aware addition: add `a` and `b` together, ignoring the MSB of the result.
#[inline]
pub fn gen_add_msb_masked(a: NumericType, b: NumericType) -> NumericType {
    a.wrapping_add(b) & !MSB
}

/// Wrapping-aware comparison: is `a` behind or equal to `b`?
/// Returns true if a <= b in the circular sense (distance < MAX/**4**).
#[inline]
pub fn gen_lte_msb_masked(a: NumericType, b: NumericType) -> bool {
    gen_gte_msb_masked(b, a)
}

/// Wrapping-aware comparison: is `a` strictly ahead of `b`?
#[inline]
pub fn gen_gt_msb_masked(a: NumericType, b: NumericType) -> bool {
    a != b && gen_gte_msb_masked(a, b)
}

/// Wrapping-aware comparison: is `a` strictly behind `b`?
#[inline]
pub fn gen_lt_msb_masked(a: NumericType, b: NumericType) -> bool {
    gen_gt_msb_masked(b, a)
}
