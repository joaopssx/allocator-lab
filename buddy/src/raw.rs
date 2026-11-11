//! The free bitmap, stored at the end of the region.
//!
//! The only module in the crate that dereferences region pointers, and it
//! only ever touches the bitmap area. Block memory is never read or written
//! by the allocator, free or not: a block freed through a `Box` that is still
//! a protected function argument can be reused without the allocator writing
//! through a foreign pointer into it.

use alloc_core::Region;

/// Reads word `w` of the bitmap starting at `map`.
///
/// # Safety
///
/// `map` must be 8-byte aligned and `map + 8 * w + 8` inside the bitmap area.
pub(crate) unsafe fn word(region: &Region, map: usize, w: usize) -> u64 {
    // SAFETY: aligned and inside the bitmap (caller contract), which no block
    // ever overlaps.
    unsafe { region.ptr(map + 8 * w).cast::<u64>().read() }
}

/// Writes word `w` of the bitmap starting at `map`.
///
/// # Safety
///
/// Same as [`word`].
pub(crate) unsafe fn set_word(region: &Region, map: usize, w: usize, v: u64) {
    // SAFETY: as in `word`.
    unsafe { region.ptr(map + 8 * w).cast::<u64>().write(v) }
}

/// Clears `len` bytes of bitmap starting at `map`.
///
/// # Safety
///
/// `map..map + len` must be the bitmap area of the region.
pub(crate) unsafe fn clear(region: &Region, map: usize, len: usize) {
    // SAFETY: the range is the bitmap (caller contract).
    unsafe { region.ptr(map).write_bytes(0, len) }
}
