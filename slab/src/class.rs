//! Size classes.
//!
//! Classes are the powers of two from 16 to 2048 bytes. A request goes to the
//! smallest class that is at least as large as both its size and its
//! alignment. Blocks of a class sit at multiples of the class size inside a
//! 4096-aligned slab, so that choice also takes care of alignment.

use core::alloc::Layout;

/// Smallest class, in bytes. Big enough to hold a free-list link.
pub const MIN: usize = 16;
/// Largest class, in bytes. Requests above it are not served.
pub const MAX: usize = 2048;
/// Number of classes: 16, 32, 64, 128, 256, 512, 1024, 2048.
pub const COUNT: usize = (MAX.trailing_zeros() - MIN.trailing_zeros() + 1) as usize;

/// Index of the class that serves `layout`, or `None` if no class can.
#[inline]
pub fn of(layout: Layout) -> Option<usize> {
    let need = layout.size().max(layout.align()).max(MIN);
    if need > MAX {
        return None;
    }
    // `need <= MAX`, so rounding up to a power of two cannot overflow.
    let bits = need.next_power_of_two().trailing_zeros();
    Some((bits - MIN.trailing_zeros()) as usize)
}

/// Block size of class `idx`.
#[inline]
pub const fn size(idx: usize) -> usize {
    MIN << idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn boundaries() {
        assert_eq!(COUNT, 8);
        assert_eq!(of(l(0, 1)), Some(0));
        assert_eq!(of(l(1, 1)), Some(0));
        assert_eq!(of(l(16, 1)), Some(0));
        assert_eq!(of(l(17, 1)), Some(1));
        assert_eq!(of(l(1024, 1)), Some(6));
        assert_eq!(of(l(1025, 1)), Some(7));
        assert_eq!(of(l(2048, 1)), Some(7));
        assert_eq!(of(l(2049, 1)), None);
        assert_eq!(size(COUNT - 1), MAX);
    }

    #[test]
    fn alignment_picks_a_larger_class() {
        assert_eq!(of(l(8, 64)), Some(2));
        assert_eq!(of(l(1, 2048)), Some(7));
        assert_eq!(of(l(1, 4096)), None);
    }
}
