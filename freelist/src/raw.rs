//! In-band metadata, read and written straight from region memory.
//!
//! This is the only module in the crate that dereferences region pointers.
//! Every function takes an address and trusts the caller that a value of the
//! right type lives there, which the list invariants in `lib.rs` guarantee.

use alloc_core::Region;

/// Granularity of every chunk: chunk starts and sizes are multiples of this.
/// It is also the size of the header that precedes each used payload, and the
/// smallest free chunk, so every leftover piece can become a free chunk.
pub(crate) const UNIT: usize = 16;

/// Stored at the start of every free chunk.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Node {
    /// Size of the whole chunk, node included.
    pub size: usize,
    /// Address of the next free chunk, higher than this one, or `NIL`.
    pub next: usize,
}

/// Stored at the start of every used chunk, right before the payload.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Header {
    /// Size of the whole chunk, header and alignment padding included.
    pub size: usize,
    /// `TAG` while the chunk is in use. Lets debug checks spot frees of
    /// pointers this allocator never handed out.
    pub tag: usize,
}

pub(crate) const TAG: usize = 0x5ea1_ed00;
pub(crate) const NIL: usize = 0;

const _: () = assert!(size_of::<Node>() <= UNIT && align_of::<Node>() <= UNIT);
const _: () = assert!(size_of::<Header>() <= UNIT && align_of::<Header>() <= UNIT);

/// Reads the free-list node at `addr`.
///
/// # Safety
///
/// `addr` must be the start of a free chunk inside `region`.
pub(crate) unsafe fn node(region: &Region, addr: usize) -> Node {
    // SAFETY: free chunks are UNIT-aligned, at least UNIT bytes long and begin
    // with a `Node` (caller contract), so the read is aligned and in bounds.
    unsafe { region.ptr(addr).cast::<Node>().read() }
}

/// Writes a free-list node at `addr`.
///
/// # Safety
///
/// `addr..addr + UNIT` must be inside `region`, `addr` must be UNIT-aligned,
/// and no live payload may overlap that range.
pub(crate) unsafe fn set_node(region: &Region, addr: usize, n: Node) {
    // SAFETY: aligned, in bounds and not aliased by any payload (caller
    // contract).
    unsafe { region.ptr(addr).cast::<Node>().write(n) }
}

/// Reads the header of the used chunk at `addr`.
///
/// # Safety
///
/// `addr` must be the start of a used chunk inside `region`.
pub(crate) unsafe fn header(region: &Region, addr: usize) -> Header {
    // SAFETY: used chunks are UNIT-aligned and begin with a `Header` (caller
    // contract). The header is never part of a payload, so this read does
    // not race with the caller's data.
    unsafe { region.ptr(addr).cast::<Header>().read() }
}

/// Writes the header of a used chunk at `addr`.
///
/// # Safety
///
/// Same as [`set_node`].
pub(crate) unsafe fn set_header(region: &Region, addr: usize, h: Header) {
    // SAFETY: aligned, in bounds and not aliased by any payload (caller
    // contract).
    unsafe { region.ptr(addr).cast::<Header>().write(h) }
}
