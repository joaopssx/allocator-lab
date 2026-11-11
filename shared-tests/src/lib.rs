//! Harness for tests that run against all four allocators.
//!
//! [`Lab`] is the small common surface the tests need on top of
//! `GlobalAlloc`: building an allocator over a region, and enough accounting
//! to check that no byte goes missing. [`with`] gives each test a fresh
//! page-aligned buffer and an allocator over it, and guarantees the buffer
//! outlives the allocator.

use alloc_core::Region;
use buddy::Buddy;
use bump::Bump;
use freelist::{Fit, FreeList};
use slab::Slab;
use std::alloc::{GlobalAlloc, Layout, alloc, dealloc};
use std::ptr::NonNull;

/// What the shared tests need from an allocator.
pub trait Lab: GlobalAlloc + Sync + Sized {
    /// Name used in assertion messages.
    const NAME: &'static str;

    /// Number of configurations worth testing, passed to [`Lab::build`].
    const VARIANTS: usize = 1;

    /// Whether `dealloc` gives memory back. When it does not, the
    /// accounting check has nothing to compare.
    const FREES: bool = true;

    /// Builds an allocator over `region`, in configuration `variant`.
    fn build(region: Region, variant: usize) -> Self;

    /// The region the allocator owns.
    fn region(&self) -> &Region;

    /// Bytes available when nothing is allocated.
    fn capacity(&self) -> usize;

    /// Bytes available right now.
    fn free_bytes(&self) -> usize;

    /// How many bytes a live block of `layout` takes out of
    /// [`Lab::free_bytes`], or `None` for a layout the allocator does not
    /// serve or when [`Lab::FREES`] is false.
    fn cost(layout: Layout) -> Option<usize>;

    /// Whether the allocator serves `layout` at all. Requests it does not
    /// serve must return null rather than misbehave.
    fn serves(layout: Layout) -> bool;
}

impl Lab for Bump {
    const NAME: &'static str = "bump";
    const FREES: bool = false;

    fn build(region: Region, _: usize) -> Self {
        Bump::new(region)
    }
    fn region(&self) -> &Region {
        Bump::region(self)
    }
    fn capacity(&self) -> usize {
        Bump::region(self).size()
    }
    fn free_bytes(&self) -> usize {
        Bump::region(self).size() - self.used()
    }
    fn cost(_: Layout) -> Option<usize> {
        None
    }
    fn serves(_: Layout) -> bool {
        true
    }
}

impl Lab for FreeList {
    const NAME: &'static str = "freelist";
    const VARIANTS: usize = 3;

    fn build(region: Region, variant: usize) -> Self {
        let fit = [Fit::First, Fit::Best, Fit::Next][variant];
        FreeList::new(region, fit)
    }
    fn region(&self) -> &Region {
        FreeList::region(self)
    }
    fn capacity(&self) -> usize {
        FreeList::capacity(self)
    }
    fn free_bytes(&self) -> usize {
        FreeList::free_bytes(self)
    }
    fn cost(layout: Layout) -> Option<usize> {
        // A 16-byte header plus the payload rounded to 16. Alignment padding
        // in front of the header stays on the free list.
        Some(16 + layout.size().max(1).next_multiple_of(16))
    }
    fn serves(_: Layout) -> bool {
        true
    }
}

impl Lab for Slab {
    const NAME: &'static str = "slab";

    fn build(region: Region, _: usize) -> Self {
        Slab::new(region)
    }
    fn region(&self) -> &Region {
        Slab::region(self)
    }
    fn capacity(&self) -> usize {
        Slab::capacity(self)
    }
    fn free_bytes(&self) -> usize {
        Slab::free_bytes(self)
    }
    fn cost(layout: Layout) -> Option<usize> {
        slab::class::of(layout).map(slab::class::size)
    }
    fn serves(layout: Layout) -> bool {
        slab::class::of(layout).is_some()
    }
}

impl Lab for Buddy {
    const NAME: &'static str = "buddy";

    fn build(region: Region, _: usize) -> Self {
        Buddy::new(region)
    }
    fn region(&self) -> &Region {
        Buddy::region(self)
    }
    fn capacity(&self) -> usize {
        Buddy::capacity(self)
    }
    fn free_bytes(&self) -> usize {
        Buddy::free_bytes(self)
    }
    fn cost(layout: Layout) -> Option<usize> {
        buddy::order_of(layout).map(buddy::block_size)
    }
    fn serves(layout: Layout) -> bool {
        buddy::order_of(layout).is_some()
    }
}

/// Runs `f` with an allocator of type `A`, in configuration `variant`, over a
/// fresh `size`-byte buffer aligned to 4096.
pub fn with<A: Lab, R>(size: usize, variant: usize, f: impl FnOnce(&A) -> R) -> R {
    let layout = Layout::from_size_align(size, 4096).expect("buffer layout");
    // SAFETY: callers pass a non-zero size.
    let ptr = NonNull::new(unsafe { alloc(layout) }).expect("test buffer");
    // SAFETY: the buffer is valid for `size` bytes until the `dealloc` below,
    // and only this allocator, which is dropped first, ever touches it.
    let a = A::build(unsafe { Region::new(ptr, size) }, variant);
    let out = f(&a);
    drop(a);
    // SAFETY: allocated above with this layout; the allocator is gone.
    unsafe { dealloc(ptr.as_ptr(), layout) };
    out
}

/// A block handed out by an allocator under test.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    /// Start of the block.
    pub ptr: *mut u8,
    /// Layout it currently has.
    pub layout: Layout,
}

/// Checks the guarantees every allocator gives for its live blocks: each is
/// aligned, inside the region, and disjoint from every other one.
pub fn check_blocks<A: Lab>(a: &A, live: &[Block]) {
    let mut spans: Vec<_> = live
        .iter()
        .map(|b| {
            let addr = b.ptr.addr();
            let len = b.layout.size().max(1);
            assert_eq!(addr % b.layout.align(), 0, "{}: misaligned {b:?}", A::NAME);
            assert!(
                a.region().contains(addr, len),
                "{}: outside region {b:?}",
                A::NAME
            );
            (addr, addr + len)
        })
        .collect();
    spans.sort_unstable();
    for w in spans.windows(2) {
        assert!(w[0].1 <= w[1].0, "{}: blocks overlap: {w:?}", A::NAME);
    }
}

/// Checks that free bytes plus what the live blocks cost add up to the
/// capacity. Skipped for allocators that never give memory back.
pub fn check_accounting<A: Lab>(a: &A, live: &[Block]) {
    if !A::FREES {
        return;
    }
    let used: usize = live
        .iter()
        .map(|b| A::cost(b.layout).expect("live block with an unserved layout"))
        .sum();
    assert_eq!(
        a.free_bytes() + used,
        a.capacity(),
        "{}: free + used does not match capacity",
        A::NAME
    );
}

/// Byte pattern for block number `n`, so a block overwritten by another
/// block or by allocator metadata shows up when it is checked.
pub fn tag(n: usize) -> u8 {
    (n as u8).wrapping_mul(31).wrapping_add(7)
}

/// Fills the block with `t`.
///
/// # Safety
///
/// `b` must be a live block of `b.layout.size()` bytes.
pub unsafe fn fill(b: Block, t: u8) {
    // SAFETY: live block (caller contract).
    unsafe { b.ptr.write_bytes(t, b.layout.size()) }
}

/// Checks that the first `len` bytes of the block are all `t`.
///
/// # Safety
///
/// `b` must be a live block of at least `len` bytes.
pub unsafe fn holds(b: Block, len: usize, t: u8) -> bool {
    // SAFETY: live block (caller contract).
    (0..len).all(|i| unsafe { b.ptr.add(i).read() } == t)
}
