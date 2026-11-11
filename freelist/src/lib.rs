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
use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use raw::{Header, NIL, Node, TAG, UNIT};

#[cfg(feature = "stats")]
use alloc_core::Stats;

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
    #[cfg(feature = "stats")]
    stats: Stats,
}

impl FreeList {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> FreeList {
        FreeList {
            region,
            state: SpinLock::new(State {
                head: NIL,
                ready: false,
                #[cfg(feature = "stats")]
                stats: Stats::new(),
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

    /// Snapshot of the allocation counters.
    #[cfg(feature = "stats")]
    pub fn stats(&self) -> Stats {
        self.state.lock().stats
    }

    /// Allocates a block for `layout`, taking the first free chunk that fits.
    ///
    /// Fails with [`AllocError::Exhausted`] when no single free chunk is big
    /// enough, even if the free space adds up to more than requested.
    pub fn try_alloc(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        let res = self.take(&mut st, layout);
        #[cfg(feature = "stats")]
        match res {
            Ok(_) => st.stats.on_alloc(layout.size()),
            Err(_) => st.stats.on_failure(),
        }
        res.map(|p| self.nonnull(p))
    }

    fn nonnull(&self, addr: usize) -> NonNull<u8> {
        // Region addresses come from a `NonNull` base and never wrap, so the
        // fallback is unreachable; it only avoids an `unsafe` block.
        NonNull::new(self.region.ptr(addr)).unwrap_or(NonNull::dangling())
    }

    /// Finds a chunk for `layout`, carves the block out of it and returns the
    /// payload address.
    fn take(&self, st: &mut State, layout: Layout) -> Result<usize, AllocError> {
        let size = layout.size().max(1);
        let align = layout.align().max(UNIT);
        let mut prev = NIL;
        let mut at = st.head;
        while at != NIL {
            // SAFETY: `at` comes from the free list.
            let n = unsafe { raw::node(&self.region, at) };
            if let Some((payload, end)) = fit(at, n.size, size, align) {
                self.carve(st, prev, at, n, payload, end);
                return Ok(payload);
            }
            prev = at;
            at = n.next;
        }
        Err(AllocError::Exhausted)
    }

    /// Turns the free chunk `at` into a used chunk ending at `end` whose
    /// payload starts at `payload`. Whatever is left in front of the header
    /// and after `end` goes back on the list in place of `at`.
    fn carve(&self, st: &mut State, prev: usize, at: usize, n: Node, payload: usize, end: usize) {
        let hdr = payload - UNIT;
        let chunk_end = at + n.size;
        let mut link = n.next;
        // `at`, `hdr`, `end` and `chunk_end` are all UNIT-aligned, so both
        // leftovers are multiples of UNIT: either empty or a valid chunk.
        if end < chunk_end {
            let tail = Node {
                size: chunk_end - end,
                next: link,
            };
            // SAFETY: `end..chunk_end` is part of the free chunk `at`, past
            // the new block.
            unsafe { raw::set_node(&self.region, end, tail) };
            link = end;
        }
        if hdr > at {
            let front = Node {
                size: hdr - at,
                next: link,
            };
            // SAFETY: `at..hdr` is part of the free chunk, before the block.
            unsafe { raw::set_node(&self.region, at, front) };
            link = at;
        }
        self.relink(st, prev, link);

        let h = Header {
            size: end - hdr,
            tag: TAG,
        };
        // SAFETY: `hdr..payload` is UNIT bytes inside the old free chunk and
        // does not overlap either leftover node written above.
        unsafe { raw::set_header(&self.region, hdr, h) };
    }

    /// Points `prev` (or the head, if `prev` is `NIL`) at `to`.
    fn relink(&self, st: &mut State, prev: usize, to: usize) {
        if prev == NIL {
            st.head = to;
        } else {
            // SAFETY: `prev` is a free chunk on the list.
            let mut p = unsafe { raw::node(&self.region, prev) };
            p.next = to;
            // SAFETY: rewriting the node of a free chunk in place.
            unsafe { raw::set_node(&self.region, prev, p) };
        }
    }

    /// Puts the used chunk whose payload starts at `payload` back on the list.
    ///
    /// # Safety
    ///
    /// `payload` must have been returned by this allocator and not freed
    /// since.
    unsafe fn release(&self, st: &mut State, payload: usize) {
        let hdr = payload - UNIT;
        // SAFETY: a live payload is always preceded by its header (caller
        // contract), which is UNIT-aligned and inside the region.
        let h = unsafe { raw::header(&self.region, hdr) };
        debug_assert_eq!(h.tag, TAG, "freed pointer has no valid header");
        self.insert(st, hdr, h.size);
    }

    /// Links the chunk `at..at + size` into the list, keeping address order.
    fn insert(&self, st: &mut State, at: usize, size: usize) {
        let mut prev = NIL;
        let mut next = st.head;
        while next != NIL && next < at {
            prev = next;
            // SAFETY: `next` comes from the free list.
            next = unsafe { raw::node(&self.region, next) }.next;
        }
        // SAFETY: `at..at + size` was a used chunk that the caller gave up,
        // so writing a node over its header aliases nothing live.
        unsafe { raw::set_node(&self.region, at, Node { size, next }) };
        self.relink(st, prev, at);
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

/// Where a block of `size` bytes aligned to `align` would go inside the free
/// chunk `at..at + len`: returns `(payload, end)`, or `None` if it does not
/// fit. Any overflow also means it does not fit.
fn fit(at: usize, len: usize, size: usize, align: usize) -> Option<(usize, usize)> {
    let payload = align_up(at.checked_add(UNIT)?, align)?;
    let end = align_up(payload.checked_add(size)?, UNIT)?;
    (end <= at + len).then_some((payload, end))
}

// SAFETY: blocks are carved out of chunks on the free list only, and a chunk
// leaves the list before any part of it is handed out, so blocks never
// overlap each other or the list nodes. `fit` places the payload at an
// address aligned to at least the requested alignment and keeps the whole
// block inside the chunk, which lies inside the region. All list updates
// happen under the lock.
unsafe impl GlobalAlloc for FreeList {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.try_alloc(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let mut st = self.state.lock();
        // SAFETY: the caller guarantees `ptr` came from this allocator and is
        // still live.
        unsafe { self.release(&mut st, ptr.addr()) };
        #[cfg(feature = "stats")]
        st.stats.on_dealloc(_layout.size());
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
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::alloc::{alloc, dealloc};

    #[test]
    fn first_fit_splits_and_respects_alignment() {
        let layout = Layout::from_size_align(1024, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // Start 8 bytes in so the region is not UNIT-aligned on either end.
        let base8 = NonNull::new(base.as_ptr().wrapping_add(8)).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base8, 1000) });

        assert_eq!(fl.capacity(), 992);
        assert_eq!(fl.free_bytes(), 992);

        // 8 bytes of payload plus the header round to 32.
        let p = fl.try_alloc(Layout::new::<u64>()).unwrap();
        assert_eq!(p.as_ptr().addr() % 16, 0);
        assert_eq!(fl.free_bytes(), 992 - 32);

        // A 256-aligned block leaves the gap in front of it on the list, so
        // only header plus payload are taken.
        let q = fl
            .try_alloc(Layout::from_size_align(16, 256).unwrap())
            .unwrap();
        assert_eq!(q.as_ptr().addr() % 256, 0);
        assert_eq!(fl.free_bytes(), 992 - 64);

        assert!(
            fl.try_alloc(Layout::from_size_align(1000, 1).unwrap())
                .is_err()
        );

        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }

    /// Addresses of the free chunks, in list order.
    fn chunks(fl: &FreeList) -> std::vec::Vec<(usize, usize)> {
        let st = fl.state.lock();
        let mut out = std::vec::Vec::new();
        let mut at = st.head;
        while at != NIL {
            // SAFETY: walking the list under the lock.
            let n = unsafe { raw::node(&fl.region, at) };
            out.push((at, n.size));
            at = n.next;
        }
        out
    }

    #[test]
    fn dealloc_keeps_list_in_address_order() {
        let layout = Layout::from_size_align(1024, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base, 1024) });
        let l = Layout::from_size_align(48, 16).unwrap();

        // SAFETY: each pointer is freed once, with its own layout.
        unsafe {
            let ps: std::vec::Vec<_> = (0..4).map(|_| fl.alloc(l)).collect();
            // Free out of order, leaving a used block between each pair.
            fl.dealloc(ps[2], l);
            fl.dealloc(ps[0], l);

            let list = chunks(&fl);
            assert!(list.windows(2).all(|w| w[0].0 < w[1].0));
            assert_eq!(list.len(), 3);
            assert_eq!(fl.free_bytes(), 1024 - 2 * 64);

            // First fit hands back the lowest freed block.
            assert_eq!(fl.alloc(l), ps[0]);
        }

        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }
}
