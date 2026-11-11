//! Free-list links stored inside free blocks.
//!
//! The only module in the crate that dereferences region pointers. A free
//! block holds the address of the next free block of the same class in its
//! first word; used blocks carry no metadata at all.

use alloc_core::Region;

/// Marks the end of a class's free list.
pub(crate) const NIL: usize = 0;

/// Reads the link stored in the free block at `addr`.
///
/// # Safety
///
/// `addr` must be a free block of this allocator.
pub(crate) unsafe fn next(region: &Region, addr: usize) -> usize {
    // SAFETY: blocks are at least 16 bytes and aligned to their size, so a
    // `usize` at the start is in bounds and aligned. The block is free, so
    // the word holds a link written by `set_next`.
    unsafe { region.ptr(addr).cast::<usize>().read() }
}

/// Stores `to` as the link of the block at `addr`.
///
/// # Safety
///
/// `addr` must be a block of this allocator that the caller has just given
/// up, so no live data is overwritten.
pub(crate) unsafe fn set_next(region: &Region, addr: usize, to: usize) {
    // SAFETY: in bounds and aligned as in `next`; the block is no longer
    // live (caller contract).
    unsafe { region.ptr(addr).cast::<usize>().write(to) }
}
