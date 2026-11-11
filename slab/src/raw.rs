//! Per-slab metadata, stored in a table at the end of the region.
//!
//! The only module in the crate that dereferences region pointers, and it
//! only touches the table. Block memory is never read or written by the
//! allocator: a block freed through a `Box` that is still a protected function
//! argument can be reused without the allocator writing into it.

use alloc_core::Region;

/// Bookkeeping for one slab.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Meta {
    /// One bit per block, set while the block is free. A slab has at most
    /// 4096 / 16 = 256 blocks.
    pub free: [u64; 4],
    /// Next slab of the same class that has free blocks, as index + 1, or 0.
    pub next: usize,
    /// Number of set bits in `free`.
    pub nfree: usize,
}

/// Reads the entry at `addr`.
///
/// # Safety
///
/// `addr` must be the entry of a carved slab in the table.
pub(crate) unsafe fn meta(region: &Region, addr: usize) -> Meta {
    // SAFETY: table entries are aligned to `Meta` (the table starts on a
    // slab boundary and entries are `size_of::<Meta>()` apart) and lie in the
    // table, which no block overlaps. Carved entries were written by
    // `set_meta`.
    unsafe { region.ptr(addr).cast::<Meta>().read() }
}

/// Writes the entry at `addr`.
///
/// # Safety
///
/// `addr` must be an entry of the table.
pub(crate) unsafe fn set_meta(region: &Region, addr: usize, m: Meta) {
    // SAFETY: as in `meta`.
    unsafe { region.ptr(addr).cast::<Meta>().write(m) }
}
