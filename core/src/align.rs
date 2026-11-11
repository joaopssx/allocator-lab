//! Address and size alignment.
//!
//! Every function here expects `align` to be a power of two, which is what
//! [`core::alloc::Layout`] guarantees. Anything else is a caller bug and trips a
//! debug assertion. In release builds the result is garbage but never undefined
//! behavior: these functions only do integer math and never touch memory.

/// Rounds `addr` up to the next multiple of `align`.
///
/// Returns `None` when the rounded value does not fit in a `usize`.
///
/// ```
/// use alloc_core::align::align_up;
///
/// assert_eq!(align_up(13, 8), Some(16));
/// assert_eq!(align_up(16, 8), Some(16));
/// assert_eq!(align_up(usize::MAX, 8), None);
/// ```
#[inline]
pub const fn align_up(addr: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    let mask = align.wrapping_sub(1);
    match addr.checked_add(mask) {
        Some(v) => Some(v & !mask),
        None => None,
    }
}

/// Rounds `addr` down to the previous multiple of `align`.
///
/// This cannot overflow, so it returns the value directly.
#[inline]
pub const fn align_down(addr: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    addr & !align.wrapping_sub(1)
}

/// Returns `true` if `addr` is a multiple of `align`.
#[inline]
pub const fn is_aligned(addr: usize, align: usize) -> bool {
    debug_assert!(align.is_power_of_two());
    addr & align.wrapping_sub(1) == 0
}
