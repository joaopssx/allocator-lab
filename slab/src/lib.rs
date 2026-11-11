//! Slab allocator over a fixed [`Region`].
//!
//! Requests are rounded up to one of eight power-of-two size classes, from 16
//! to 2048 bytes. Anything larger, or aligned to more than 2048 bytes, is not
//! served: allocation returns null.
//!
//! The region is cut into 4096-byte slabs, handed to size classes on demand.
//! A slab belongs to one class for good and is split into blocks of that
//! class's size.

#![no_std]

pub mod class;

use alloc_core::align::{align_down, align_up};
use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::Layout;
use core::ptr::NonNull;

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
}

#[derive(Clone, Copy)]
struct Class {
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
                classes: [Class { fresh: 0, end: 0 }; class::COUNT],
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

    /// Allocates a block for `layout` from the matching size class.
    ///
    /// Fails with [`AllocError::Unsupported`] if the size or alignment is
    /// above 2048 bytes, and with [`AllocError::Exhausted`] if the class has
    /// no free block and the region has no slab left to give it.
    pub fn try_alloc(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let idx = class::of(layout).ok_or(AllocError::Unsupported)?;
        let mut st = self.state.lock();
        let addr = self.take(&mut st, idx)?;
        Ok(NonNull::new(self.region.ptr(addr)).unwrap_or(NonNull::dangling()))
    }

    fn take(&self, st: &mut State, idx: usize) -> Result<usize, AllocError> {
        if st.classes[idx].fresh == st.classes[idx].end {
            let slab = self.carve(st)?;
            st.classes[idx] = Class {
                fresh: slab,
                end: slab + SLAB,
            };
        }
        let c = &mut st.classes[idx];
        let addr = c.fresh;
        c.fresh += class::size(idx);
        Ok(addr)
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
}
