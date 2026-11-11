//! Slab allocator over a fixed [`Region`].
//!
//! Requests are rounded up to one of eight power-of-two size classes, from 16
//! to 2048 bytes. Anything larger, or aligned to more than 2048 bytes, is not
//! served: allocation returns null.
//!
//! The region is cut into 4096-byte slabs, handed to size classes on demand.
//! A slab belongs to one class for good and is split into blocks of that
//! class's size. Freed blocks go on a per-class list, linked through the
//! blocks themselves, and are reused before any new slab is carved.
//!
//! Used blocks have no header: `dealloc` finds the class again from the
//! `Layout`, which `GlobalAlloc` guarantees is the one used to allocate.

#![no_std]

pub mod class;
mod raw;

use alloc_core::align::{align_down, align_up};
use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use raw::NIL;

#[cfg(feature = "stats")]
use alloc_core::Stats;

/// Size of a slab, and its alignment inside the region.
pub const SLAB: usize = 4096;

/// A slab allocator.
pub struct Slab {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Number of slabs handed to classes so far, counted from the start of
    /// the span.
    carved: usize,
    classes: [Class; class::COUNT],
    #[cfg(feature = "stats")]
    stats: Stats,
}

#[derive(Clone, Copy)]
struct Class {
    /// Most recently freed block of this class, or `NIL`.
    free: usize,
    /// Next never-used block in the class's newest slab.
    fresh: usize,
    /// End of that slab. `fresh == end` means it is used up.
    end: usize,
}

impl Slab {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> Slab {
        Slab {
            region,
            state: SpinLock::new(State {
                carved: 0,
                classes: [Class {
                    free: NIL,
                    fresh: 0,
                    end: 0,
                }; class::COUNT],
                #[cfg(feature = "stats")]
                stats: Stats::new(),
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The slab-aligned part of the region, as `(start, end)`.
    fn span(&self) -> (usize, usize) {
        let start = align_up(self.region.start(), SLAB).unwrap_or(usize::MAX);
        let end = align_down(self.region.end(), SLAB);
        if end > start {
            (start, end)
        } else {
            (start, start)
        }
    }

    /// Bytes that can be turned into slabs.
    pub fn capacity(&self) -> usize {
        let (start, end) = self.span();
        end - start
    }

    /// Number of slabs handed to size classes so far.
    pub fn slabs(&self) -> usize {
        self.state.lock().carved
    }

    /// Bytes that can still be handed out: uncarved slabs, the unused tail
    /// of each class's newest slab, and every block on a free list.
    ///
    /// Free bytes in one class cannot serve another, so this overstates what
    /// a given request can get.
    pub fn free_bytes(&self) -> usize {
        let st = self.state.lock();
        let (start, end) = self.span();
        let mut total = end - start - st.carved * SLAB;
        for (idx, c) in st.classes.iter().enumerate() {
            total += c.end - c.fresh;
            let mut at = c.free;
            while at != NIL {
                total += class::size(idx);
                // SAFETY: `at` is on a free list.
                at = unsafe { raw::next(&self.region, at) };
            }
        }
        total
    }

    /// Snapshot of the allocation counters.
    #[cfg(feature = "stats")]
    pub fn stats(&self) -> Stats {
        self.state.lock().stats
    }

    /// Allocates a block for `layout` from the matching size class.
    ///
    /// Fails with [`AllocError::Unsupported`] if the size or alignment is
    /// above 2048 bytes, and with [`AllocError::Exhausted`] if the class has
    /// no free block and the region has no slab left to give it.
    pub fn try_alloc(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let mut st = self.state.lock();
        let res = class::of(layout)
            .ok_or(AllocError::Unsupported)
            .and_then(|idx| self.take(&mut st, idx));
        #[cfg(feature = "stats")]
        match res {
            Ok(_) => st.stats.on_alloc(layout.size()),
            Err(_) => st.stats.on_failure(),
        }
        res.map(|addr| NonNull::new(self.region.ptr(addr)).unwrap_or(NonNull::dangling()))
    }

    fn take(&self, st: &mut State, idx: usize) -> Result<usize, AllocError> {
        let free = st.classes[idx].free;
        if free != NIL {
            // SAFETY: `free` heads the class's free list.
            st.classes[idx].free = unsafe { raw::next(&self.region, free) };
            return Ok(free);
        }
        if st.classes[idx].fresh == st.classes[idx].end {
            let slab = self.carve(st)?;
            st.classes[idx].fresh = slab;
            st.classes[idx].end = slab + SLAB;
        }
        let c = &mut st.classes[idx];
        let addr = c.fresh;
        c.fresh += class::size(idx);
        Ok(addr)
    }

    /// Pushes the block at `addr` onto the free list of class `idx`.
    ///
    /// # Safety
    ///
    /// `addr` must be a live block of class `idx`, and the caller gives it up.
    unsafe fn give(&self, st: &mut State, idx: usize, addr: usize) {
        // SAFETY: the block is being released (caller contract).
        unsafe { raw::set_next(&self.region, addr, st.classes[idx].free) };
        st.classes[idx].free = addr;
    }

    /// Takes the next unused slab from the region.
    fn carve(&self, st: &mut State) -> Result<usize, AllocError> {
        let (start, end) = self.span();
        let slab = start + st.carved * SLAB;
        if slab >= end {
            return Err(AllocError::Exhausted);
        }
        st.carved += 1;
        Ok(slab)
    }
}

// SAFETY: every block lies in a slab that belongs to exactly one class, at an
// offset that is a multiple of the class size, so it is aligned to the class
// size, which is at least the requested size and alignment. A block is either
// on its class's free list, past the class's `fresh` cursor, or handed out,
// never two at once, so blocks never overlap. All of it happens under the
// lock.
unsafe impl GlobalAlloc for Slab {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.try_alloc(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // A layout with no class could not have produced a pointer.
        let Some(idx) = class::of(layout) else {
            debug_assert!(false, "dealloc with a layout slab never serves");
            return;
        };
        let mut st = self.state.lock();
        // SAFETY: `ptr` is live and was allocated with `layout` (caller
        // contract), so it is a block of class `idx`.
        unsafe { self.give(&mut st, idx, ptr.addr()) };
        #[cfg(feature = "stats")]
        st.stats.on_dealloc(layout.size());
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = self
            .try_alloc(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr);
        if !p.is_null() {
            // SAFETY: fresh block of at least `layout.size()` bytes.
            unsafe { ptr::write_bytes(p, 0, layout.size()) };
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees `new_size`, rounded up to the
        // alignment, does not overflow `isize`.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let mut st = self.state.lock();
        let res = match (class::of(layout), class::of(new)) {
            // The block already has room for anything in its class.
            (Some(old), Some(new)) if old == new => Ok(ptr),
            (Some(old), Some(new)) => self.take(&mut st, new).map(|dst| {
                let dst = self.region.ptr(dst);
                // SAFETY: both blocks are live and distinct, `dst` holds at
                // least `new_size` bytes and `ptr` at least `layout.size()`.
                // The old block is released after the copy.
                unsafe {
                    ptr::copy_nonoverlapping(ptr, dst, layout.size().min(new_size));
                    self.give(&mut st, old, ptr.addr());
                }
                dst
            }),
            _ => Err(AllocError::Unsupported),
        };
        #[cfg(feature = "stats")]
        match res {
            Ok(_) => st.stats.on_realloc(layout.size(), new_size),
            Err(_) => st.stats.on_failure(),
        }
        res.unwrap_or(ptr::null_mut())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::alloc::{alloc, dealloc};

    #[test]
    fn classes_get_their_own_slabs() {
        let layout = Layout::from_size_align(3 * SLAB, SLAB).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `s` and only `s` touches it.
        let s = Slab::new(unsafe { Region::new(base, 3 * SLAB) });
        let start = base.as_ptr().addr();

        let a = s.try_alloc(Layout::new::<u8>()).unwrap().as_ptr().addr();
        let b = s.try_alloc(Layout::new::<u8>()).unwrap().as_ptr().addr();
        assert_eq!((a - start, b - start), (0, 16));

        let big = Layout::from_size_align(2048, 8).unwrap();
        let c = s.try_alloc(big).unwrap().as_ptr().addr();
        let d = s.try_alloc(big).unwrap().as_ptr().addr();
        assert_eq!((c - start, d - start), (SLAB, SLAB + 2048));
        assert_eq!(s.slabs(), 2);

        // The third slab is the last one and holds two more.
        assert!(s.try_alloc(big).is_ok());
        assert!(s.try_alloc(big).is_ok());
        assert_eq!(s.slabs(), 3);
        assert_eq!(s.try_alloc(big), Err(AllocError::Exhausted));
        assert_eq!(
            s.try_alloc(Layout::from_size_align(4096, 1).unwrap()),
            Err(AllocError::Unsupported)
        );

        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }

    #[test]
    fn freed_blocks_are_reused_lifo() {
        let layout = Layout::from_size_align(2 * SLAB, SLAB).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `s` and only `s` touches it.
        let s = Slab::new(unsafe { Region::new(base, 2 * SLAB) });
        let l = Layout::from_size_align(100, 4).unwrap();

        // SAFETY: each pointer is freed once with its layout.
        unsafe {
            let a = s.alloc(l);
            let b = s.alloc(l);
            s.dealloc(a, l);
            s.dealloc(b, l);
            assert_eq!(s.alloc(l), b);
            assert_eq!(s.alloc(l), a);

            // Staying inside the 128 class keeps the pointer.
            assert_eq!(s.realloc(a, l, 128), a);
            let moved = s.realloc(a, Layout::from_size_align(128, 4).unwrap(), 129);
            assert_ne!(moved, a);
            assert!(!moved.is_null());
        }
        // The move needed a second slab for the 256 class.
        assert_eq!(s.slabs(), 2);
        assert_eq!(s.free_bytes(), 2 * SLAB - 128 - 256);
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }
}
