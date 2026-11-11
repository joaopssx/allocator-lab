//! Binary buddy allocator over a fixed [`Region`].
//!
//! Blocks have power-of-two sizes from [`MIN`] bytes up. A block of order `k`
//! is `MIN << k` bytes and sits at an offset from the base that is a multiple
//! of its size. Splitting a block gives two halves of the next lower order,
//! which are each other's buddies.
//!
//! All bookkeeping lives in a bitmap at the end of the region: one bit per
//! possible block of each order, set while that block is free. The allocator
//! never reads or writes the memory of a block, free or live. Allocation
//! scans the bitmap of the needed order, and freeing checks the buddy's bit
//! to decide whether to merge.

#![no_std]

mod raw;

use alloc_core::align::{align_down, align_up};
use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

#[cfg(feature = "stats")]
use alloc_core::Stats;

/// Size of an order-0 block.
pub const MIN: usize = 16;
const MIN_SHIFT: usize = MIN.trailing_zeros() as usize;

/// Largest alignment served. The base of the block area is aligned to this,
/// so any block of at least this size is aligned to it too.
pub const MAX_ALIGN: usize = 4096;

/// Number of orders a `usize` address space can need.
const ORDERS: usize = usize::BITS as usize - MIN_SHIFT;

/// Order of the smallest block that fits `layout`, or `None` if the
/// alignment is above [`MAX_ALIGN`] or the size has no power of two.
pub fn order_of(layout: Layout) -> Option<usize> {
    if layout.align() > MAX_ALIGN {
        return None;
    }
    let need = layout.size().max(layout.align()).max(MIN);
    let bits = need.checked_next_power_of_two()?.trailing_zeros() as usize;
    Some(bits - MIN_SHIFT)
}

/// Size in bytes of a block of order `k`.
pub const fn block_size(k: usize) -> usize {
    MIN << k
}

/// Bytes of bitmap needed for a block area of `space` bytes.
///
/// A region of `space + bitmap_bytes(space)` bytes, starting at a
/// 4096-aligned address, gives a block area of exactly `space` bytes.
pub fn bitmap_bytes(space: usize) -> usize {
    bit_layout(space).1 / 8
}

/// Bit index where each order starts, and the total number of bits. Each
/// order starts on a 64-bit boundary so it can be scanned a word at a time.
fn bit_layout(space: usize) -> ([usize; ORDERS], usize) {
    let mut starts = [0; ORDERS];
    let mut idx = 0;
    for (k, s) in starts.iter_mut().enumerate() {
        *s = idx;
        idx += (space >> (MIN_SHIFT + k)).next_multiple_of(64);
    }
    (starts, idx)
}

/// A buddy allocator.
pub struct Buddy {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Set once the bitmap is cleared and the initial blocks are marked
    /// free. Done lazily because `new` is a `const fn`.
    ready: bool,
    geo: Geometry,
    /// Number of free blocks of each order.
    counts: [usize; ORDERS],
    /// Per order, a bitmap word at or before the lowest set bit. Scans start
    /// here instead of at the beginning of the order.
    hints: [usize; ORDERS],
    #[cfg(feature = "stats")]
    stats: Stats,
}

/// Where things live inside the region. Computed once, from the region alone.
#[derive(Clone, Copy)]
struct Geometry {
    /// Start of the block area, aligned to `MAX_ALIGN`.
    base: usize,
    /// Size of the block area, a multiple of `MIN`.
    space: usize,
    /// Start of the bitmap, right after the block area. 16-byte aligned.
    map: usize,
    /// Bit index of the first bit of each order, a multiple of 64.
    starts: [usize; ORDERS],
}

impl Geometry {
    const EMPTY: Geometry = Geometry {
        base: 0,
        space: 0,
        map: 0,
        starts: [0; ORDERS],
    };

    fn new(region: &Region) -> Geometry {
        let Some(base) = align_up(region.start(), MAX_ALIGN) else {
            return Geometry::EMPTY;
        };
        if base >= region.end() {
            return Geometry::EMPTY;
        }
        let total = region.end() - base;
        // The largest area whose bitmap still fits behind it. Taking the
        // bitmap of the whole region off the top always fits, since a smaller
        // area never needs a larger bitmap; then grow back while the slack
        // allows. The bitmap is about 1/64 of the area, so both loops are
        // short.
        let fits = |s: usize| s + bitmap_bytes(s) <= total;
        let mut space = align_down(total - bitmap_bytes(align_down(total, MIN)).min(total), MIN);
        while !fits(space) {
            space -= MIN;
        }
        while fits(space + MIN) {
            space += MIN;
        }
        Geometry {
            base,
            space,
            map: base + space,
            starts: bit_layout(space).0,
        }
    }

    /// Number of order-`k` blocks that fit in the area.
    fn count(&self, k: usize) -> usize {
        self.space >> (MIN_SHIFT + k)
    }

    /// Bit that says whether the order-`k` block at `addr` is free.
    fn bit(&self, k: usize, addr: usize) -> usize {
        self.starts[k] + ((addr - self.base) >> (MIN_SHIFT + k))
    }

    /// Address of the buddy of the order-`k` block at `addr`.
    ///
    /// The two halves of a block of order `k + 1` differ only in bit
    /// `MIN_SHIFT + k` of their offset, so flipping that bit maps each half
    /// to the other. The result may lie past the end of the area, when the
    /// block is a root whose buddy was never part of it.
    fn buddy(&self, k: usize, addr: usize) -> usize {
        self.base + ((addr - self.base) ^ block_size(k))
    }

    /// Whether the order-`k` block at `addr` is the upper half of its parent.
    fn is_upper(&self, k: usize, addr: usize) -> bool {
        (addr - self.base) & block_size(k) != 0
    }

    /// Whether an order-`k` block at `addr` would lie entirely in the area.
    fn fits(&self, k: usize, addr: usize) -> bool {
        addr + block_size(k) <= self.base + self.space
    }
}

impl Buddy {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> Buddy {
        Buddy {
            region,
            state: SpinLock::new(State {
                ready: false,
                geo: Geometry::EMPTY,
                counts: [0; ORDERS],
                hints: [0; ORDERS],
                #[cfg(feature = "stats")]
                stats: Stats::new(),
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Size of the block area: the region minus the alignment of its start
    /// and the bitmap at its end.
    pub fn capacity(&self) -> usize {
        Geometry::new(&self.region).space
    }

    /// Total size of all free blocks.
    pub fn free_bytes(&self) -> usize {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        st.counts
            .iter()
            .enumerate()
            .map(|(k, &n)| n * block_size(k))
            .sum()
    }

    /// Size of the largest free block.
    pub fn largest_free(&self) -> usize {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        (0..ORDERS)
            .rev()
            .find(|&k| st.counts[k] > 0)
            .map_or(0, block_size)
    }

    /// Snapshot of the allocation counters.
    #[cfg(feature = "stats")]
    pub fn stats(&self) -> Stats {
        self.state.lock().stats
    }

    /// Allocates the smallest block that fits `layout`.
    ///
    /// Fails with [`AllocError::Unsupported`] if the alignment is above
    /// [`MAX_ALIGN`], and with [`AllocError::Exhausted`] if no free block is
    /// large enough.
    pub fn try_alloc(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        let res = match order_of(layout) {
            Some(k) => self.take(&mut st, k),
            None if layout.align() > MAX_ALIGN => Err(AllocError::Unsupported),
            None => Err(AllocError::Exhausted),
        };
        #[cfg(feature = "stats")]
        match res {
            Ok(_) => st.stats.on_alloc(layout.size()),
            Err(_) => st.stats.on_failure(),
        }
        #[cfg(feature = "debug-checks")]
        self.verify(&st);
        res.map(|addr| NonNull::new(self.region.ptr(addr)).unwrap_or(NonNull::dangling()))
    }

    /// Checks invariants 2 to 4 listed in `docs/buddy.md`. Invariant 1 holds
    /// by construction of the bit index.
    #[cfg(feature = "debug-checks")]
    fn verify(&self, st: &State) {
        use alloc_core::broken;
        let geo = st.geo;
        for k in 0..ORDERS {
            let n = geo.count(k);
            let (first, last) = (geo.starts[k] / 64, (geo.starts[k] + n).div_ceil(64));
            let mut set = 0;
            let mut lowest = None;
            for w in first..last {
                // SAFETY: `w` is a word of order `k`'s part of the bitmap.
                let mut bits = unsafe { raw::word(&self.region, geo.map, w) };
                if bits != 0 && lowest.is_none() {
                    lowest = Some(w);
                }
                while bits != 0 {
                    let idx = w * 64 + bits.trailing_zeros() as usize - geo.starts[k];
                    bits &= bits - 1;
                    set += 1;
                    if idx >= n {
                        broken("buddy: free bit past the last block of its order");
                    }
                    let addr = geo.base + idx * block_size(k);
                    if self.is_free(st, k, geo.buddy(k, addr)) {
                        broken("buddy: two free buddies not merged");
                    }
                    for j in k + 1..ORDERS {
                        let up = geo.base + ((addr - geo.base) & !(block_size(j) - 1));
                        if !geo.fits(j, up) {
                            break;
                        }
                        if self.is_free(st, j, up) {
                            broken("buddy: free block inside another free block");
                        }
                    }
                }
            }
            if set != st.counts[k] {
                broken("buddy: free count disagrees with the bitmap");
            }
            if lowest.is_some_and(|l| st.hints[k] > l) {
                broken("buddy: scan hint past a free block");
            }
        }
    }

    /// Takes a free block of order `want`, splitting a larger one if needed.
    fn take(&self, st: &mut State, want: usize) -> Result<usize, AllocError> {
        let mut k = (want..ORDERS)
            .find(|&k| st.counts[k] > 0)
            .ok_or(AllocError::Exhausted)?;
        let addr = self.lowest_free(st, k);
        self.mark(st, k, addr, false);
        // Keep the lower half each time and mark the upper half free one
        // order down.
        while k > want {
            k -= 1;
            self.mark(st, k, addr + block_size(k), true);
        }
        Ok(addr)
    }

    /// Address of the lowest free block of order `k`, which must have at
    /// least one.
    fn lowest_free(&self, st: &mut State, k: usize) -> usize {
        let geo = st.geo;
        let first = geo.starts[k] / 64;
        let last = (geo.starts[k] + geo.count(k)).div_ceil(64);
        for w in st.hints[k].max(first)..last {
            // SAFETY: `w` is a word of order `k`'s part of the bitmap.
            let bits = unsafe { raw::word(&self.region, geo.map, w) };
            if bits != 0 {
                st.hints[k] = w;
                let idx = w * 64 + bits.trailing_zeros() as usize - geo.starts[k];
                return geo.base + idx * block_size(k);
            }
        }
        alloc_core::broken("buddy: free count and bitmap disagree");
    }

    /// Sets or clears the free bit of the order-`k` block at `addr`, keeping
    /// the count and the scan hint in step.
    fn mark(&self, st: &mut State, k: usize, addr: usize, free: bool) {
        let idx = st.geo.bit(k, addr);
        let (w, mask) = (idx / 64, 1u64 << (idx % 64));
        // SAFETY: the block is inside the area, so its bit is inside the
        // bitmap.
        unsafe {
            let bits = raw::word(&self.region, st.geo.map, w);
            if (cfg!(debug_assertions) || cfg!(feature = "debug-checks"))
                && (bits & mask != 0) == free
            {
                alloc_core::broken("buddy: block already in that state (double free?)");
            }
            let bits = if free { bits | mask } else { bits & !mask };
            raw::set_word(&self.region, st.geo.map, w, bits);
        }
        if free {
            st.counts[k] += 1;
            st.hints[k] = st.hints[k].min(w);
        } else {
            st.counts[k] -= 1;
        }
    }

    /// Whether the order-`k` block at `addr` is free.
    fn is_free(&self, st: &State, k: usize, addr: usize) -> bool {
        if !st.geo.fits(k, addr) {
            return false;
        }
        let idx = st.geo.bit(k, addr);
        // SAFETY: the block is inside the area, so its bit is inside the
        // bitmap.
        let bits = unsafe { raw::word(&self.region, st.geo.map, idx / 64) };
        bits & (1 << (idx % 64)) != 0
    }

    /// Frees the order-`k` block at `addr`, merging it with its buddy for as
    /// long as the buddy is free too.
    fn release(&self, st: &mut State, mut k: usize, mut addr: usize) {
        while k + 1 < ORDERS {
            let b = st.geo.buddy(k, addr);
            // A free buddy of the same order means both halves of the parent
            // are free, and the parent is a real block: buddies at the same
            // order never straddle two roots (see `prepare`).
            if !self.is_free(st, k, b) {
                break;
            }
            self.mark(st, k, b, false);
            addr = addr.min(b);
            k += 1;
        }
        self.mark(st, k, addr, true);
    }

    /// Resizes the order-`k` block at `addr` to order `want` without moving
    /// it, if possible. Shrinking always works. Growing works when the block
    /// is the lower half at every level up to `want` and every upper half
    /// on the way is free.
    fn resize(&self, st: &mut State, addr: usize, k: usize, want: usize) -> bool {
        if want <= k {
            for j in (want..k).rev() {
                self.mark(st, j, addr + block_size(j), true);
            }
            return true;
        }
        let ok = (k..want)
            .all(|j| !st.geo.is_upper(j, addr) && self.is_free(st, j, st.geo.buddy(j, addr)));
        if ok {
            for j in k..want {
                let b = st.geo.buddy(j, addr);
                self.mark(st, j, b, false);
            }
        }
        ok
    }

    fn prepare(&self, st: &mut State) {
        if st.ready {
            return;
        }
        st.ready = true;
        st.geo = Geometry::new(&self.region);
        let geo = st.geo;
        for (k, h) in st.hints.iter_mut().enumerate() {
            *h = geo.starts[k] / 64;
        }
        // SAFETY: `map..map + bitmap_bytes(space)` is the area `Geometry`
        // reserved past the block area, inside the region.
        unsafe { raw::clear(&self.region, geo.map, bitmap_bytes(geo.space)) };

        // Cover the block area with the largest blocks that fit. Taking sizes
        // in decreasing order keeps every block aligned to its own size. The
        // buddy of one of these roots is either past the end of the area or
        // the start of a smaller root, so it can never be a free block of the
        // root's order, and merging never crosses from one root into another.
        let mut off = 0;
        while geo.space - off >= MIN {
            let k = (geo.space - off).ilog2() as usize - MIN_SHIFT;
            self.mark(st, k, geo.base + off, true);
            off += block_size(k);
        }
    }
}

// SAFETY: a block is handed out only after its free bit is cleared, and the
// free bits only ever describe disjoint blocks: the initial roots tile the
// area, splitting replaces a block by its two halves, and merging replaces
// two free buddies by their parent. Blocks of order `k` sit at multiples of
// `block_size(k)` from a base aligned to `MAX_ALIGN`, and `order_of` picks
// `k` so that `block_size(k)` covers both the size and the alignment. The
// allocator only writes to the bitmap, which lies past the block area.
// Everything happens under the lock.
unsafe impl GlobalAlloc for Buddy {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.try_alloc(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(k) = order_of(layout) else {
            if cfg!(debug_assertions) || cfg!(feature = "debug-checks") {
                alloc_core::broken("buddy: dealloc with a layout it never serves");
            }
            return;
        };
        let mut st = self.state.lock();
        self.release(&mut st, k, ptr.addr());
        #[cfg(feature = "stats")]
        st.stats.on_dealloc(layout.size());
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
        // SAFETY: the caller guarantees `new_size`, rounded up to the
        // alignment, does not overflow `isize`.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let (Some(k), Some(want)) = (order_of(layout), order_of(new)) else {
            return ptr::null_mut();
        };
        let mut st = self.state.lock();
        let res = if self.resize(&mut st, ptr.addr(), k, want) {
            Ok(ptr)
        } else {
            self.take(&mut st, want).map(|dst| {
                let dst = self.region.ptr(dst);
                // SAFETY: `resize` only fails when growing, so `dst` is a
                // distinct free block larger than the old one, which is live
                // for `layout.size()` bytes (caller contract).
                unsafe { ptr::copy_nonoverlapping(ptr, dst, layout.size()) };
                self.release(&mut st, k, ptr.addr());
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
    use std::vec::Vec;

    fn l(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// A page-aligned buffer and a buddy allocator whose area is `space`.
    fn with(space: usize, f: impl FnOnce(&Buddy, usize)) {
        let size = space + bitmap_bytes(space);
        let layout = Layout::from_size_align(size, 4096).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `b` and only `b` touches it.
        let b = Buddy::new(unsafe { Region::new(base, size) });
        assert_eq!(b.capacity(), space);
        f(&b, base.as_ptr().addr());
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }

    /// Orders that have at least one free block.
    fn orders_in_use(b: &Buddy) -> Vec<usize> {
        let mut st = b.state.lock();
        b.prepare(&mut st);
        (0..ORDERS).filter(|&k| st.counts[k] > 0).collect()
    }

    #[test]
    fn orders() {
        assert_eq!(order_of(l(0, 1)), Some(0));
        assert_eq!(order_of(l(16, 1)), Some(0));
        assert_eq!(order_of(l(17, 1)), Some(1));
        assert_eq!(order_of(l(1, 4096)), Some(8));
        assert_eq!(order_of(l(4097, 8)), Some(9));
        assert_eq!(order_of(l(1, 8192)), None);
        assert_eq!(order_of(l(isize::MAX as usize, 1)), Some(59));
        assert_eq!(block_size(8), 4096);
    }

    #[test]
    fn area_is_the_largest_that_leaves_room_for_the_bitmap() {
        for total in [16, 64, 100, 4096, 4160, 12288, 100_000, 1 << 20] {
            let layout = Layout::from_size_align(total, 4096).unwrap();
            // SAFETY: non-zero layout.
            let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
            // SAFETY: the buffer outlives `b` and only `b` touches it.
            let b = Buddy::new(unsafe { Region::new(base, total) });
            let c = b.capacity();
            assert!(c + bitmap_bytes(c) <= total, "total {total}");
            assert!(c + MIN + bitmap_bytes(c + MIN) > total, "total {total}");
            // SAFETY: allocated above with this layout.
            unsafe { dealloc(base.as_ptr(), layout) };
        }
    }

    #[test]
    fn odd_sized_area_is_tiled_by_decreasing_blocks() {
        with(12096, |b, _| {
            assert_eq!(b.free_bytes(), 12096);
            // 12096 = 8192 + 2048 + 1024 + 512 + 256 + 64: one root per set
            // bit of the area size, in units of MIN.
            let want: Vec<usize> = (0..ORDERS)
                .filter(|k| (12096usize >> 4) & (1 << k) != 0)
                .collect();
            assert_eq!(orders_in_use(b), want);
            assert_eq!(want, [2, 4, 5, 6, 7, 9]);
        });
    }

    #[test]
    fn buddy_is_one_bit_flip_away() {
        let g = Geometry {
            base: 0x10000,
            space: 4096,
            map: 0x11000,
            starts: [0; ORDERS],
        };
        assert_eq!(g.buddy(0, 0x10000), 0x10010);
        assert_eq!(g.buddy(0, 0x10010), 0x10000);
        assert_eq!(g.buddy(1, 0x10020), 0x10000);
        assert_eq!(g.buddy(4, 0x10100), 0x10000);
        assert_eq!(g.buddy(4, 0x10300), 0x10200);
        // A root's buddy can fall outside the area.
        assert_eq!(g.buddy(8, 0x10000), 0x11000);
        assert!(!g.fits(8, 0x11000));
        assert!(g.is_upper(4, 0x10300));
        assert!(g.is_upper(5, 0x10200));
        assert!(!g.is_upper(5, 0x10400));
        // The buddy relation is an involution at every order.
        for k in 0..8 {
            for i in 0..(4096 >> (4 + k)) {
                let a = 0x10000 + i * block_size(k);
                assert_eq!(g.buddy(k, g.buddy(k, a)), a);
            }
        }
    }

    #[test]
    fn alloc_splits_down_to_the_right_order() {
        // Roots of 4096 down to 128. A small request splits the smallest
        // root that fits, the 128 one at the end, rather than breaking up
        // the big blocks.
        with(8064, |b, start| {
            let p = b.try_alloc(l(16, 1)).unwrap().as_ptr().addr();
            let q = b.try_alloc(l(16, 1)).unwrap().as_ptr().addr();
            let r = b.try_alloc(l(32, 1)).unwrap().as_ptr().addr();
            assert_eq!((p - start, q - start, r - start), (7936, 7952, 7968));
            assert_eq!(b.free_bytes(), 8064 - 64);

            let page = b.try_alloc(l(1, 4096)).unwrap().as_ptr().addr();
            assert_eq!(page % 4096, 0);
            assert!(b.try_alloc(l(1, 8192)).is_err());
        });
    }

    #[test]
    fn realloc_grows_into_free_buddies_and_shrinks_in_place() {
        with(4096, |b, _| {
            // SAFETY: each pointer is used with the layout it currently has.
            unsafe {
                let p = b.alloc(l(16, 1));
                p.write_bytes(0x77, 16);
                // Everything above `p` is free buddies, so it grows in place.
                let q = b.realloc(p, l(16, 1), 1000);
                assert_eq!(q, p);
                assert!((0..16).all(|i| p.add(i).read() == 0x77));
                assert_eq!(b.free_bytes(), 4096 - 1024);

                // Shrinking hands the upper halves back.
                assert_eq!(b.realloc(p, l(1000, 1), 20), p);
                assert_eq!(b.free_bytes(), 4096 - 32);

                // An upper half cannot grow in place: it has to move.
                let r = b.alloc(l(32, 1));
                assert_eq!(r.addr(), p.addr() + 32);
                r.write(5);
                let r2 = b.realloc(r, l(32, 1), 64);
                assert_ne!(r2, r);
                assert_eq!(r2.read(), 5);
            }
        });
    }

    #[test]
    fn freeing_everything_merges_back_to_the_roots() {
        with(12096, |b, _| {
            let roots = orders_in_use(b);
            let sizes = [16, 100, 16, 700, 32, 3000, 16, 64, 250, 16];
            // SAFETY: each block is freed once with its own layout.
            unsafe {
                let ps: Vec<_> = sizes.iter().map(|&s| (b.alloc(l(s, 1)), s)).collect();
                assert!(ps.iter().all(|(p, _)| !p.is_null()));
                for i in [3, 0, 9, 5, 1, 7, 2, 8, 6, 4] {
                    b.dealloc(ps[i].0, l(ps[i].1, 1));
                }
            }
            assert_eq!(b.free_bytes(), b.capacity());
            assert_eq!(orders_in_use(b), roots);
        });
    }

    #[test]
    fn merge_stops_at_a_live_buddy() {
        with(4096, |b, _| {
            // SAFETY: each block is freed once with its own layout.
            unsafe {
                let p = b.alloc(l(16, 1));
                let q = b.alloc(l(16, 1));
                b.dealloc(p, l(16, 1));
                // `q` is still live, so `p` stays an order-0 block.
                assert_eq!(orders_in_use(b), (0..8).collect::<Vec<_>>());
                b.dealloc(q, l(16, 1));
                assert_eq!(orders_in_use(b), [8]);
            }
        });
    }

    #[test]
    fn scan_hint_finds_blocks_freed_below_it() {
        with(4096, |b, _| {
            let lay = l(16, 1);
            // SAFETY: each block is freed once with its layout.
            unsafe {
                let ps: Vec<_> = (0..200).map(|_| b.alloc(lay)).collect();
                // Free one low block after the hint has moved past it.
                b.dealloc(ps[3], lay);
                assert_eq!(b.alloc(lay), ps[3]);
                for p in ps {
                    b.dealloc(p, lay);
                }
            }
            assert_eq!(orders_in_use(b), [8]);
        });
    }
}
