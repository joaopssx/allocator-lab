//! In-band free-list nodes and the free bitmap, both stored in region memory.
//!
//! The only module in the crate that dereferences region pointers.

use alloc_core::Region;

/// End of a list.
pub(crate) const NIL: usize = 0;

/// Stored at the start of every free block. Doubly linked so a block can be
/// unlinked in O(1) when its buddy merges with it.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Node {
    pub next: usize,
    pub prev: usize,
}

const _: () = assert!(size_of::<Node>() <= crate::MIN);

/// Reads the node of the free block at `addr`.
///
/// # Safety
///
/// `addr` must be a free block of this allocator.
pub(crate) unsafe fn node(region: &Region, addr: usize) -> Node {
    // SAFETY: free blocks are at least MIN bytes, aligned to MIN and start
    // with a `Node` (caller contract).
    unsafe { region.ptr(addr).cast::<Node>().read() }
}

/// Writes the node of the block at `addr`.
///
/// # Safety
///
/// `addr` must be a block of this allocator that is free or being freed, so
/// no live data sits under the node.
pub(crate) unsafe fn set_node(region: &Region, addr: usize, n: Node) {
    // SAFETY: aligned and in bounds as in `node`, and not live (caller
    // contract).
    unsafe { region.ptr(addr).cast::<Node>().write(n) }
}

/// Sets bit `idx` of the bitmap starting at `map`.
///
/// # Safety
///
/// `map + idx / 8` must be inside the bitmap area of the region.
pub(crate) unsafe fn set_bit(region: &Region, map: usize, idx: usize, on: bool) {
    let p = region.ptr(map + idx / 8);
    let mask = 1u8 << (idx % 8);
    // SAFETY: the byte is inside the bitmap (caller contract), which no
    // block ever overlaps.
    unsafe {
        let b = p.read();
        p.write(if on { b | mask } else { b & !mask });
    }
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
