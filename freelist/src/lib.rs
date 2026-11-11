//! Free-list allocator over a fixed [`Region`].
//!
//! The region is split into chunks. Free chunks form a singly linked list kept
//! in address order, with the links stored inside the free memory itself.
//! Every used chunk starts with a small header recording its size, so
//! `dealloc` can find the whole chunk from the payload pointer.
//!
//! ```text
//!   used chunk                         free chunk
//!   +--------+---------------------+   +--------+--------------------+
//!   | Header |  payload ...        |   |  Node  |  (unused)          |
//!   +--------+---------------------+   +--------+--------------------+
//!   ^        ^                         ^
//!   chunk    pointer returned           linked from the previous free
//!   start    to the caller              chunk, in address order
//! ```

#![no_std]

mod raw;

use alloc_core::align::{align_down, align_up};
use alloc_core::{Region, SpinLock};
use raw::{NIL, Node, UNIT};

/// A free-list allocator.
pub struct FreeList {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Address of the lowest free chunk, or `NIL`.
    head: usize,
    /// Whether the initial free chunk has been written. Done lazily because
    /// `new` is a `const fn` and cannot touch region memory.
    ready: bool,
}

impl FreeList {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> FreeList {
        FreeList {
            region,
            state: SpinLock::new(State {
                head: NIL,
                ready: false,
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The part of the region that can hold chunks: the region trimmed to
    /// UNIT boundaries on both ends. Empty if the region is too small.
    fn span(&self) -> (usize, usize) {
        let start = align_up(self.region.start(), UNIT).unwrap_or(usize::MAX);
        let end = align_down(self.region.end(), UNIT);
        if end >= start + UNIT {
            (start, end)
        } else {
            (start, start)
        }
    }

    /// Bytes the allocator can hand out, headers included, when everything is
    /// free.
    pub fn capacity(&self) -> usize {
        let (start, end) = self.span();
        end - start
    }

    /// Total size of all free chunks.
    pub fn free_bytes(&self) -> usize {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        let mut total = 0;
        let mut at = st.head;
        while at != NIL {
            // SAFETY: `at` comes from the free list, so it is a free chunk.
            let n = unsafe { raw::node(&self.region, at) };
            total += n.size;
            at = n.next;
        }
        total
    }

    fn prepare(&self, st: &mut State) {
        if st.ready {
            return;
        }
        st.ready = true;
        let (start, end) = self.span();
        if end > start {
            let n = Node {
                size: end - start,
                next: NIL,
            };
            // SAFETY: `start..end` is UNIT-aligned, at least UNIT long, inside
            // the region, and nothing has been handed out yet.
            unsafe { raw::set_node(&self.region, start, n) };
            st.head = start;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::ptr::NonNull;
    use std::alloc::{Layout, alloc, dealloc};

    #[test]
    fn fresh_list_is_one_chunk_trimmed_to_units() {
        let layout = Layout::from_size_align(1024, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // Start 8 bytes in so the region is not UNIT-aligned on either end.
        let base8 = NonNull::new(base.as_ptr().wrapping_add(8)).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base8, 1000) });

        assert_eq!(fl.capacity(), 992);
        assert_eq!(fl.free_bytes(), 992);
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }
}
