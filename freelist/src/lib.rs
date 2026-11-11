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

/// How the allocator picks a free chunk for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// The lowest-addressed chunk that fits. Cheap, and tends to keep large
    /// chunks at the top of the region.
    First,
    /// The smallest chunk that fits. Walks the whole list unless it finds an
    /// exact fit, but leaves fewer mid-sized holes.
    Best,
    /// The first chunk that fits at or after the end of the previous
    /// allocation, wrapping around. Spreads allocations over the region
    /// instead of piling them up at the bottom.
    Next,
}

/// A free-list allocator.
pub struct FreeList {
    region: Region,
    fit: Fit,
    state: SpinLock<State>,
}

struct State {
    /// Address of the lowest free chunk, or `NIL`.
    head: usize,
    /// Whether the initial free chunk has been written. Done lazily because
    /// `new` is a `const fn` and cannot touch region memory.
    ready: bool,
    /// Where next fit resumes its search. Just an address, not necessarily
    /// a chunk, so merging and splitting never have to update it.
    rover: usize,
    /// Total size of all used chunks, kept only to check that used and free
    /// chunks together cover the span.
    #[cfg(feature = "debug-checks")]
    used: usize,
    #[cfg(feature = "stats")]
    stats: Stats,
}

impl FreeList {
    /// Creates an allocator that owns `region` and places blocks with `fit`.
    pub const fn new(region: Region, fit: Fit) -> FreeList {
        FreeList {
            region,
            fit,
            state: SpinLock::new(State {
                head: NIL,
                ready: false,
                rover: 0,
                #[cfg(feature = "debug-checks")]
                used: 0,
                #[cfg(feature = "stats")]
                stats: Stats::new(),
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The placement policy chosen at construction.
    pub fn fit(&self) -> Fit {
        self.fit
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

    /// Size of the largest free chunk, header included. A request whose
    /// header plus rounded payload exceed this cannot succeed.
    pub fn largest_free(&self) -> usize {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        let mut best = 0;
        let mut at = st.head;
        while at != NIL {
            // SAFETY: `at` comes from the free list.
            let n = unsafe { raw::node(&self.region, at) };
            best = best.max(n.size);
            at = n.next;
        }
        best
    }

    /// Snapshot of the allocation counters.
    #[cfg(feature = "stats")]
    pub fn stats(&self) -> Stats {
        self.state.lock().stats
    }

    /// Allocates a block for `layout`, choosing the chunk according to the
    /// allocator's [`Fit`].
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
        #[cfg(feature = "debug-checks")]
        self.verify(&st);
        res.map(|p| self.nonnull(p))
    }

    /// Checks the invariants listed in `docs/freelist.md`.
    #[cfg(feature = "debug-checks")]
    fn verify(&self, st: &State) {
        use alloc_core::broken;
        let (start, end) = self.span();
        let mut at = st.head;
        let mut prev_end: Option<usize> = None;
        let mut free = 0;
        let mut steps = 0;
        while at != NIL {
            if !at.is_multiple_of(UNIT) || at < start || at >= end {
                broken("freelist: free chunk misaligned or outside the span");
            }
            // SAFETY: just checked that `at` is a chunk start inside the span.
            let n = unsafe { raw::node(&self.region, at) };
            if n.size < UNIT || !n.size.is_multiple_of(UNIT) || n.size > end - at {
                broken("freelist: free chunk with a bad size");
            }
            match prev_end {
                Some(pe) if at < pe => broken("freelist: free list out of order"),
                Some(pe) if at == pe => broken("freelist: adjacent free chunks not merged"),
                _ => {}
            }
            prev_end = Some(at + n.size);
            free += n.size;
            steps += 1;
            if steps > (end - start) / UNIT {
                broken("freelist: free list has a cycle");
            }
            at = n.next;
        }
        if free + st.used != end - start {
            broken("freelist: free and used chunks do not cover the span");
        }
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
        let mut pick: Option<Pick> = None;
        let mut prev = NIL;
        let mut at = st.head;
        while at != NIL {
            // SAFETY: `at` comes from the free list.
            let n = unsafe { raw::node(&self.region, at) };
            if let Some((payload, end)) = place(at, n.size, size, align) {
                let here = Pick {
                    prev,
                    at,
                    n,
                    payload,
                    end,
                };
                match self.fit {
                    Fit::First => {
                        pick = Some(here);
                        break;
                    }
                    Fit::Best => {
                        if pick.is_none_or(|p| n.size < p.n.size) {
                            pick = Some(here);
                        }
                        if payload - UNIT == at && end == at + n.size {
                            break;
                        }
                    }
                    Fit::Next => {
                        if at >= st.rover {
                            pick = Some(here);
                            break;
                        }
                        // Keep the lowest fit in case nothing past the rover
                        // fits and the search has to wrap around.
                        pick.get_or_insert(here);
                    }
                }
            }
            prev = at;
            at = n.next;
        }
        let p = pick.ok_or(AllocError::Exhausted)?;
        self.carve(st, p.prev, p.at, p.n, p.payload, p.end);
        st.rover = p.end;
        Ok(p.payload)
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
        #[cfg(feature = "debug-checks")]
        {
            st.used += end - hdr;
        }
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
        if (cfg!(debug_assertions) || cfg!(feature = "debug-checks")) && h.tag != TAG {
            alloc_core::broken("freelist: freed pointer has no valid header");
        }
        #[cfg(feature = "debug-checks")]
        {
            st.used -= h.size;
        }
        self.insert(st, hdr, h.size);
    }

    /// Returns the free chunks just below and at-or-above `addr` as
    /// `(prev, next)`, either of which may be `NIL`.
    fn neighbors(&self, st: &State, addr: usize) -> (usize, usize) {
        let mut prev = NIL;
        let mut next = st.head;
        while next != NIL && next < addr {
            prev = next;
            // SAFETY: `next` comes from the free list.
            next = unsafe { raw::node(&self.region, next) }.next;
        }
        (prev, next)
    }

    /// Links the chunk `at..at + size` into the list, keeping address order
    /// and merging it with the free chunks right before and after it.
    ///
    /// After this no two free chunks touch, which is what keeps one large
    /// request from failing just because the free space is split in pieces
    /// that happen to be adjacent.
    fn insert(&self, st: &mut State, at: usize, mut size: usize) {
        let (prev, next) = self.neighbors(st, at);
        let mut link = next;
        if next != NIL && at + size == next {
            // SAFETY: `next` is on the free list.
            let n = unsafe { raw::node(&self.region, next) };
            size += n.size;
            link = n.next;
        }
        if prev != NIL {
            // SAFETY: `prev` is on the free list.
            let p = unsafe { raw::node(&self.region, prev) };
            if prev + p.size == at {
                let merged = Node {
                    size: p.size + size,
                    next: link,
                };
                // SAFETY: growing `prev` in place over the chunk being freed
                // and, maybe, `next`; none of it is live.
                unsafe { raw::set_node(&self.region, prev, merged) };
                return;
            }
        }
        // SAFETY: `at..at + size` was a used chunk that the caller gave up,
        // plus maybe the free `next`, so writing a node over it aliases
        // nothing live.
        unsafe { raw::set_node(&self.region, at, Node { size, next: link }) };
        self.relink(st, prev, at);
    }

    /// Resizes the block at `payload` without moving it, if possible.
    /// Shrinking always works; growing works when the chunk right after this
    /// one is free and large enough.
    ///
    /// # Safety
    ///
    /// `payload` must be a live block of this allocator.
    unsafe fn resize(&self, st: &mut State, payload: usize, new_size: usize) -> bool {
        let hdr = payload - UNIT;
        // SAFETY: live payloads are preceded by their header.
        let h = unsafe { raw::header(&self.region, hdr) };
        let end = hdr + h.size;
        let Some(need) = payload
            .checked_add(new_size)
            .and_then(|e| align_up(e, UNIT))
        else {
            return false;
        };

        if need <= end {
            if need < end {
                // SAFETY: the chunk keeps `hdr..need`, a prefix of itself.
                unsafe { self.set_size(hdr, need - hdr) };
                self.insert(st, need, end - need);
                #[cfg(feature = "debug-checks")]
                {
                    st.used -= end - need;
                }
            }
            return true;
        }

        let (prev, next) = self.neighbors(st, end);
        if next != end {
            return false;
        }
        // SAFETY: `next` is on the free list.
        let n = unsafe { raw::node(&self.region, next) };
        if end + n.size < need {
            return false;
        }
        let mut link = n.next;
        if need < end + n.size {
            let rest = Node {
                size: end + n.size - need,
                next: link,
            };
            // SAFETY: `need..end + n.size` is the unused tail of the free
            // chunk being absorbed.
            unsafe { raw::set_node(&self.region, need, rest) };
            link = need;
        }
        self.relink(st, prev, link);
        // SAFETY: `end..need` was just unlinked from the free list, so the
        // chunk now owns `hdr..need`.
        unsafe { self.set_size(hdr, need - hdr) };
        #[cfg(feature = "debug-checks")]
        {
            st.used += need - end;
        }
        true
    }

    /// Rewrites the size in the header at `hdr`.
    ///
    /// # Safety
    ///
    /// `hdr` must be the header of a live chunk, and `hdr..hdr + size` must
    /// be memory that chunk owns after the resize.
    unsafe fn set_size(&self, hdr: usize, size: usize) {
        let h = Header { size, tag: TAG };
        // SAFETY: `hdr` is a live header (caller contract).
        unsafe { raw::set_header(&self.region, hdr, h) };
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

/// A free chunk chosen by `take`, with the block's position inside it.
#[derive(Clone, Copy)]
struct Pick {
    prev: usize,
    at: usize,
    n: Node,
    payload: usize,
    end: usize,
}

/// Where a block of `size` bytes aligned to `align` would go inside the free
/// chunk `at..at + len`: returns `(payload, end)`, or `None` if it does not
/// fit. Any overflow also means it does not fit.
fn place(at: usize, len: usize, size: usize, align: usize) -> Option<(usize, usize)> {
    let payload = align_up(at.checked_add(UNIT)?, align)?;
    let end = align_up(payload.checked_add(size)?, UNIT)?;
    (end <= at + len).then_some((payload, end))
}

// SAFETY: blocks are carved out of chunks on the free list only, and a chunk
// leaves the list before any part of it is handed out, so blocks never
// overlap each other or the list nodes. `place` puts the payload at an
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
        #[cfg(feature = "debug-checks")]
        self.verify(&st);
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
        let mut st = self.state.lock();
        // SAFETY: `ptr` is a live block of this allocator (caller contract).
        let res = if unsafe { self.resize(&mut st, ptr.addr(), new_size) } {
            Ok(ptr)
        } else {
            // SAFETY: the caller guarantees `new_size`, rounded up to the
            // alignment, does not overflow `isize`.
            let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            self.take(&mut st, new).map(|dst| {
                let dst = self.region.ptr(dst);
                // SAFETY: `resize` only fails when growing, so the old block
                // has `layout.size()` bytes, all of which fit in `dst`. The
                // new block came off the free list, so it does not overlap
                // the old one. Releasing afterwards gives the old chunk back.
                unsafe {
                    ptr::copy_nonoverlapping(ptr, dst, layout.size());
                    self.release(&mut st, ptr.addr());
                }
                dst
            })
        };
        #[cfg(feature = "stats")]
        match res {
            Ok(_) => st.stats.on_realloc(layout.size(), new_size),
            Err(_) => st.stats.on_failure(),
        }
        #[cfg(feature = "debug-checks")]
        self.verify(&st);
        res.unwrap_or(ptr::null_mut())
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
        let fl = FreeList::new(unsafe { Region::new(base8, 1000) }, Fit::First);

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
        let fl = FreeList::new(unsafe { Region::new(base, 1024) }, Fit::First);
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

    /// Builds holes of 64, 32 and 64 bytes separated by used blocks, then
    /// asks for a 32-byte chunk and returns where it landed, relative to the
    /// region start, along with the offset just past the last separator.
    fn place_in_holes(fit: Fit) -> (usize, usize) {
        let layout = Layout::from_size_align(1024, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base, 1024) }, fit);
        let big = Layout::from_size_align(48, 16).unwrap();
        let small = Layout::from_size_align(16, 16).unwrap();
        let start = base.as_ptr().addr();

        // SAFETY: each pointer is freed at most once, with its own layout.
        let (got, rover) = unsafe {
            let a = fl.alloc(big);
            fl.alloc(small);
            let b = fl.alloc(small);
            fl.alloc(small);
            let c = fl.alloc(big);
            let last = fl.alloc(small);
            fl.dealloc(a, big);
            fl.dealloc(b, small);
            fl.dealloc(c, big);
            assert_eq!(chunks(&fl).len(), 4);
            (fl.alloc(small).addr() - start, last.addr() + 16 - start)
        };
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
        (got, rover)
    }

    #[test]
    fn policies_pick_different_holes() {
        // Chunks: a 0..64, sep 64..96, b 96..128, sep 128..160, c 160..224,
        // last 224..256. Payloads start one header past the chunk.
        assert_eq!(place_in_holes(Fit::First).0, 16);
        assert_eq!(place_in_holes(Fit::Best).0, 96 + 16);
        let (got, rover) = place_in_holes(Fit::Next);
        assert_eq!(rover, 256);
        assert_eq!(got, 256 + 16);
    }

    #[test]
    fn next_fit_wraps_around() {
        let layout = Layout::from_size_align(256, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base, 256) }, Fit::Next);
        let l = Layout::from_size_align(48, 16).unwrap();
        // SAFETY: each pointer is freed once, with its own layout.
        unsafe {
            let ps: std::vec::Vec<_> = (0..4).map(|_| fl.alloc(l)).collect();
            assert!(fl.alloc(l).is_null());
            fl.dealloc(ps[1], l);
            // Nothing fits past the rover at the end, so it wraps to `ps[1]`.
            assert_eq!(fl.alloc(l), ps[1]);
        }
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }

    #[test]
    fn freeing_everything_coalesces_back_to_one_chunk() {
        let layout = Layout::from_size_align(1024, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base, 1024) }, Fit::First);
        let l = Layout::from_size_align(48, 16).unwrap();

        // SAFETY: each pointer is freed once, with its own layout.
        unsafe {
            let ps: std::vec::Vec<_> = (0..8).map(|_| fl.alloc(l)).collect();
            // Middle first, then both neighbors, then the rest: exercises
            // merging with the next chunk, the previous one, and both.
            for i in [3, 2, 4, 0, 7, 1, 5, 6] {
                fl.dealloc(ps[i], l);
            }
        }
        assert_eq!(chunks(&fl), [(base.as_ptr().addr(), 1024)]);
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }

    #[test]
    fn realloc_grows_into_free_neighbor_and_shrinks_in_place() {
        let layout = Layout::from_size_align(1024, 16).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `fl` and only `fl` touches it.
        let fl = FreeList::new(unsafe { Region::new(base, 1024) }, Fit::First);
        let l = Layout::from_size_align(16, 16).unwrap();

        // SAFETY: pointers are used with the layouts they currently have.
        unsafe {
            let a = fl.alloc(l);
            let b = fl.alloc(l);
            a.write_bytes(0x5a, 16);
            fl.dealloc(b, l);

            // `b` is free and adjacent, so `a` grows over it.
            let a2 = fl.realloc(a, l, 100);
            assert_eq!(a2, a);
            assert!((0..16).all(|i| a.add(i).read() == 0x5a));
            assert_eq!(fl.free_bytes(), 1024 - 16 - 112);

            // Shrinking gives the tail back and it merges with what follows.
            let l100 = Layout::from_size_align(100, 16).unwrap();
            assert_eq!(fl.realloc(a, l100, 8), a);
            assert_eq!(fl.free_bytes(), 1024 - 32);
            assert_eq!(chunks(&fl).len(), 1);

            // With a used block in the way, growing has to move.
            let c = fl.alloc(l);
            let a3 = fl.realloc(a, l, 64);
            assert_ne!(a3, a);
            assert!((0..8).all(|i| a3.add(i).read() == 0x5a));
            fl.dealloc(c, l);
            fl.dealloc(a3, Layout::from_size_align(64, 16).unwrap());
        }
        assert_eq!(fl.free_bytes(), 1024);
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }
}
