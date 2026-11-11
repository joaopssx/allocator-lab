//! Slab allocator over a fixed [`Region`].
//!
//! Requests are rounded up to one of eight power-of-two size classes, from 16
//! to 2048 bytes. Anything larger, or aligned to more than 2048 bytes, is not
//! served: allocation returns null.
//!
//! The region is cut into 4096-byte slabs, handed to size classes on demand.
//! A slab belongs to one class for good and is split into blocks of that
//! class's size. Each slab has an entry in a table at the end of the region,
//! with a bitmap of its free blocks. Slabs of a class that still have free
//! blocks form a list through those entries.
//!
//! Neither used nor free blocks carry metadata, and the allocator never
//! writes to block memory. `dealloc` finds the class from the `Layout`, which
//! `GlobalAlloc` guarantees is the one used to allocate, and the slab from
//! the address.

#![no_std]

pub mod class;
mod raw;

use alloc_core::align::{align_down, align_up};
use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use raw::Meta;

#[cfg(feature = "stats")]
use alloc_core::Stats;

/// Size of a slab, and its alignment inside the region.
pub const SLAB: usize = 4096;

/// Region size, starting at a 4096-aligned address, that holds exactly
/// `slabs` slabs and their table entries.
pub const fn region_for(slabs: usize) -> usize {
    slabs * (SLAB + size_of::<Meta>())
}

/// A slab allocator.
pub struct Slab {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Whether `geo` has been computed. Done lazily because `new` is a
    /// `const fn` and the region's address is not known at compile time.
    ready: bool,
    geo: Geometry,
    /// Number of slabs handed to classes so far, counted from the start.
    carved: usize,
    /// Per class, the first slab with free blocks, as index + 1, or 0.
    partial: [usize; class::COUNT],
    #[cfg(feature = "stats")]
    stats: Stats,
}

#[derive(Clone, Copy)]
struct Geometry {
    /// First slab, aligned to `SLAB`.
    start: usize,
    /// Number of slabs that fit along with their table.
    slabs: usize,
    /// Start of the table, right after the last slab.
    table: usize,
}

impl Geometry {
    fn new(region: &Region) -> Geometry {
        let start = align_up(region.start(), SLAB).unwrap_or(usize::MAX);
        let end = align_down(region.end(), 8);
        let slabs = end.saturating_sub(start) / (SLAB + size_of::<Meta>());
        Geometry {
            start,
            slabs,
            table: start.saturating_add(slabs * SLAB),
        }
    }

    fn slab(&self, i: usize) -> usize {
        self.start + i * SLAB
    }

    fn entry(&self, i: usize) -> usize {
        self.table + i * size_of::<Meta>()
    }
}

impl Slab {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> Slab {
        Slab {
            region,
            state: SpinLock::new(State {
                ready: false,
                geo: Geometry {
                    start: 0,
                    slabs: 0,
                    table: 0,
                },
                carved: 0,
                partial: [0; class::COUNT],
                #[cfg(feature = "stats")]
                stats: Stats::new(),
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Bytes that can be turned into slabs.
    pub fn capacity(&self) -> usize {
        Geometry::new(&self.region).slabs * SLAB
    }

    /// Number of slabs handed to size classes so far.
    pub fn slabs(&self) -> usize {
        self.state.lock().carved
    }

    /// Bytes that can still be handed out: uncarved slabs plus the free
    /// blocks of every slab on a class's list.
    ///
    /// Free bytes in one class cannot serve another, so this overstates what
    /// a given request can get.
    pub fn free_bytes(&self) -> usize {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        let mut total = (st.geo.slabs - st.carved) * SLAB;
        for (idx, &head) in st.partial.iter().enumerate() {
            let mut at = head;
            while at != 0 {
                // SAFETY: slabs on a class list are carved.
                let m = unsafe { raw::meta(&self.region, st.geo.entry(at - 1)) };
                total += m.nfree * class::size(idx);
                at = m.next;
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
        self.prepare(&mut st);
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

    fn prepare(&self, st: &mut State) {
        if !st.ready {
            st.ready = true;
            st.geo = Geometry::new(&self.region);
        }
    }

    /// Takes the lowest free block of the first slab on class `idx`'s list,
    /// carving a new slab if the list is empty.
    fn take(&self, st: &mut State, idx: usize) -> Result<usize, AllocError> {
        if st.partial[idx] == 0 {
            let s = self.carve(st, idx)?;
            st.partial[idx] = s + 1;
        }
        let s = st.partial[idx] - 1;
        let e = st.geo.entry(s);
        // SAFETY: `s` is on the class list, so it is carved.
        let mut m = unsafe { raw::meta(&self.region, e) };
        // A slab is on the list only while `nfree > 0`, so some word is set.
        let w = m.free.iter().position(|&x| x != 0).unwrap_or(0);
        let bit = m.free[w].trailing_zeros() as usize;
        m.free[w] &= !(1 << bit);
        m.nfree -= 1;
        if m.nfree == 0 {
            st.partial[idx] = m.next;
            m.next = 0;
        }
        // SAFETY: rewriting the entry of a carved slab.
        unsafe { raw::set_meta(&self.region, e, m) };
        Ok(st.geo.slab(s) + (w * 64 + bit) * class::size(idx))
    }

    /// Returns the block at `addr` to its slab, putting the slab back on its
    /// class's list if it was full.
    ///
    /// # Safety
    ///
    /// `addr` must be a live block of class `idx`.
    unsafe fn give(&self, st: &mut State, idx: usize, addr: usize) {
        let s = (addr - st.geo.start) / SLAB;
        let i = (addr - st.geo.slab(s)) / class::size(idx);
        let e = st.geo.entry(s);
        // SAFETY: a live block lies in a carved slab (caller contract).
        let mut m = unsafe { raw::meta(&self.region, e) };
        debug_assert!(m.free[i / 64] & (1 << (i % 64)) == 0, "double free");
        m.free[i / 64] |= 1 << (i % 64);
        m.nfree += 1;
        if m.nfree == 1 {
            m.next = st.partial[idx];
            st.partial[idx] = s + 1;
        }
        // SAFETY: rewriting the entry of a carved slab.
        unsafe { raw::set_meta(&self.region, e, m) };
    }

    /// Hands the next unused slab to class `idx`, with every block free.
    fn carve(&self, st: &mut State, idx: usize) -> Result<usize, AllocError> {
        if st.carved == st.geo.slabs {
            return Err(AllocError::Exhausted);
        }
        let s = st.carved;
        st.carved += 1;
        let n = SLAB / class::size(idx);
        let mut free = [0u64; 4];
        for (w, x) in free.iter_mut().enumerate() {
            let bits = n.saturating_sub(w * 64).min(64);
            *x = if bits == 64 { !0 } else { (1 << bits) - 1 };
        }
        let m = Meta {
            free,
            next: 0,
            nfree: n,
        };
        // SAFETY: `s < slabs`, so its entry is inside the table.
        unsafe { raw::set_meta(&self.region, st.geo.entry(s), m) };
        Ok(s)
    }
}

// SAFETY: every block lies in a slab that belongs to exactly one class, at an
// offset that is a multiple of the class size, so it is aligned to the class
// size, which is at least the requested size and alignment. A block is handed
// out only after its free bit is cleared, and a bit is set again only by
// `dealloc` or `realloc` giving the block back, so blocks never overlap. The
// allocator only writes to the table, which lies past the last slab. All of
// it happens under the lock.
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
                // The old block is given back after the copy.
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

    /// A page-aligned buffer with room for `slabs` slabs.
    fn with(slabs: usize, f: impl FnOnce(&Slab, usize)) {
        let size = region_for(slabs);
        let layout = Layout::from_size_align(size, SLAB).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `s` and only `s` touches it.
        let s = Slab::new(unsafe { Region::new(base, size) });
        assert_eq!(s.capacity(), slabs * SLAB);
        f(&s, base.as_ptr().addr());
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }

    #[test]
    fn classes_get_their_own_slabs() {
        with(3, |s, start| {
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
        });
    }

    #[test]
    fn freed_blocks_are_reused_lowest_first() {
        with(2, |s, _| {
            let l = Layout::from_size_align(100, 4).unwrap();
            // SAFETY: each pointer is freed once with its layout.
            unsafe {
                let a = s.alloc(l);
                let b = s.alloc(l);
                s.dealloc(b, l);
                s.dealloc(a, l);
                assert_eq!(s.alloc(l), a);
                assert_eq!(s.alloc(l), b);

                // Staying inside the 128 class keeps the pointer.
                assert_eq!(s.realloc(a, l, 128), a);
                let moved = s.realloc(a, Layout::from_size_align(128, 4).unwrap(), 129);
                assert_ne!(moved, a);
                assert!(!moved.is_null());
            }
            // The move needed a second slab for the 256 class.
            assert_eq!(s.slabs(), 2);
            assert_eq!(s.free_bytes(), 2 * SLAB - 128 - 256);
        });
    }

    #[test]
    fn full_slab_goes_back_on_the_list() {
        with(1, |s, _| {
            let l = Layout::from_size_align(2048, 8).unwrap();
            // SAFETY: each pointer is freed once with its layout.
            unsafe {
                let a = s.alloc(l);
                let b = s.alloc(l);
                assert!(s.alloc(l).is_null());
                assert_eq!(s.free_bytes(), 0);
                s.dealloc(a, l);
                assert_eq!(s.free_bytes(), 2048);
                assert_eq!(s.alloc(l), a);
                s.dealloc(b, l);
                s.dealloc(a, l);
            }
            assert_eq!(s.free_bytes(), SLAB);
        });
    }
}
