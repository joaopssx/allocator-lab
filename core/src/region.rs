//! The block of memory an allocator manages.
//!
//! A [`Region`] is an ownership token: whoever holds it is the only one allowed
//! to touch those bytes. That is why it is neither `Copy` nor `Clone`, and why
//! the constructors are `unsafe`. Once an allocator owns a region, every pointer
//! it hands out is derived from the region's base pointer with
//! `with_addr`, so provenance stays intact and Miri can check it.

use core::cell::UnsafeCell;
use core::ptr::NonNull;

/// A contiguous range of bytes owned by one allocator.
#[derive(Debug)]
pub struct Region {
    base: NonNull<u8>,
    size: usize,
}

// SAFETY: `Region` is an exclusive handle to its bytes (see `Region::new`), so
// moving it to another thread moves the only access path with it.
unsafe impl Send for Region {}

// SAFETY: through `&Region` one can only read the base address and size and
// build raw pointers; actually dereferencing them is `unsafe` and governed by
// whatever lock the owning allocator uses.
unsafe impl Sync for Region {}

impl Region {
    /// Wraps `size` bytes starting at `base`.
    ///
    /// # Safety
    ///
    /// - `base..base + size` must be valid for reads and writes for as long as
    ///   this region, or any pointer derived from it, is in use.
    /// - Nothing else may access those bytes during that time.
    pub const unsafe fn new(base: NonNull<u8>, size: usize) -> Region {
        Region { base, size }
    }

    /// Base pointer of the region.
    #[inline]
    pub const fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// Size of the region in bytes.
    #[inline]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Address of the first byte.
    #[inline]
    pub fn start(&self) -> usize {
        self.base.as_ptr().addr()
    }

    /// Address one past the last byte.
    ///
    /// Cannot overflow: the safety contract of [`Region::new`] requires the
    /// whole range to be a valid allocation, and no allocation wraps around
    /// the address space.
    #[inline]
    pub fn end(&self) -> usize {
        self.start() + self.size
    }

    /// Returns `true` if `addr..addr + len` lies entirely inside the region.
    pub fn contains(&self, addr: usize, len: usize) -> bool {
        match addr.checked_add(len) {
            Some(end) => addr >= self.start() && end <= self.end(),
            None => false,
        }
    }

    /// Offset of `addr` from the start of the region, if it is inside it.
    ///
    /// The one-past-the-end address is accepted and maps to `size`.
    pub fn offset(&self, addr: usize) -> Option<usize> {
        if addr >= self.start() && addr <= self.end() {
            Some(addr - self.start())
        } else {
            None
        }
    }

    /// Builds a pointer to `addr` carrying the region's provenance.
    ///
    /// This is safe because creating a pointer is safe; dereferencing it is
    /// only valid if `addr` is inside the region.
    #[inline]
    pub fn ptr(&self, addr: usize) -> *mut u8 {
        debug_assert!(self.offset(addr).is_some(), "address outside region");
        self.base.as_ptr().with_addr(addr)
    }
}

/// Static backing storage for a [`Region`], aligned to 4096 bytes.
///
/// Meant to live in a `static` next to a `#[global_allocator]`:
///
/// ```
/// use alloc_core::region::{Arena, Region};
///
/// static ARENA: Arena<4096> = Arena::new();
/// // SAFETY: this is the only call to `region` on ARENA.
/// static REGION: Region = unsafe { ARENA.region() };
///
/// assert_eq!(REGION.size(), 4096);
/// assert_eq!(REGION.start() % 4096, 0);
/// ```
#[repr(C, align(4096))]
pub struct Arena<const N: usize> {
    bytes: UnsafeCell<[u8; N]>,
}

// SAFETY: the arena never reads or writes its own bytes. All access goes
// through the single `Region` returned by `region`, which carries the
// exclusivity requirement.
unsafe impl<const N: usize> Sync for Arena<N> {}

impl<const N: usize> Arena<N> {
    /// Creates a zeroed arena.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Arena {
            bytes: UnsafeCell::new([0; N]),
        }
    }

    /// Hands out the region covering the whole arena.
    ///
    /// # Safety
    ///
    /// Must be called at most once per arena. Two regions over the same bytes
    /// would let two allocators hand out overlapping blocks.
    pub const unsafe fn region(&'static self) -> Region {
        let base = self.bytes.get().cast::<u8>();
        // SAFETY: `UnsafeCell::get` on a live static is never null. The bytes
        // are valid for 'static, and the caller promises this is the only
        // region built over them.
        unsafe { Region::new(NonNull::new_unchecked(base), N) }
    }
}
