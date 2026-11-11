//! Bump allocator over a fixed [`Region`].
//!
//! Allocation moves a cursor forward. Freeing an individual block does
//! nothing; memory comes back only when the whole allocator is reset.
//!
//! Zero-sized requests are served as one-byte allocations, so every pointer
//! this allocator returns is unique and inside the region.
//!
//! ```
//! use alloc_core::{Arena, Region};
//! use bump::Bump;
//! use core::alloc::Layout;
//!
//! static ARENA: Arena<1024> = Arena::new();
//! // SAFETY: the only region ever taken from ARENA.
//! static HEAP: Bump = Bump::new(unsafe { ARENA.region() });
//!
//! let a = HEAP.try_alloc(Layout::new::<u8>()).unwrap();
//! let b = HEAP.try_alloc(Layout::new::<u64>()).unwrap();
//! assert_eq!(b.as_ptr() as usize % 8, 0);
//! assert!(b.as_ptr() > a.as_ptr());
//! assert_eq!(HEAP.used(), 16);
//! ```

#![no_std]

use alloc_core::align::align_up;
use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

#[cfg(feature = "stats")]
use alloc_core::Stats;

/// A bump allocator.
pub struct Bump {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Offset of the first unused byte. Never exceeds the region size.
    next: usize,
    #[cfg(feature = "stats")]
    stats: Stats,
}

impl Bump {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> Bump {
        Bump {
            region,
            state: SpinLock::new(State {
                next: 0,
                #[cfg(feature = "stats")]
                stats: Stats::new(),
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Bytes consumed so far, alignment padding included.
    pub fn used(&self) -> usize {
        self.state.lock().next
    }

    /// Snapshot of the allocation counters.
    #[cfg(feature = "stats")]
    pub fn stats(&self) -> Stats {
        self.state.lock().stats
    }

    /// Allocates a block for `layout`.
    ///
    /// Fails with [`AllocError::Exhausted`] when the block does not fit in
    /// what is left of the region, and with [`AllocError::Overflow`] if the
    /// address math overflows.
    pub fn try_alloc(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let mut st = self.state.lock();
        let res = self.bump(&mut st, layout);
        #[cfg(feature = "stats")]
        match res {
            Ok(_) => st.stats.on_alloc(layout.size()),
            Err(_) => st.stats.on_failure(),
        }
        res
    }

    fn bump(&self, st: &mut State, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let size = layout.size().max(1);
        // `start + next` cannot overflow: `next <= size` of a valid region.
        let addr =
            align_up(self.region.start() + st.next, layout.align()).ok_or(AllocError::Overflow)?;
        let end = addr.checked_add(size).ok_or(AllocError::Overflow)?;
        if end > self.region.end() {
            return Err(AllocError::Exhausted);
        }
        st.next = end - self.region.start();
        NonNull::new(self.region.ptr(addr)).ok_or(AllocError::Exhausted)
    }
}

// SAFETY: every block comes from `bump`, which only hands out ranges in
// `[start + next, end)` and then moves `next` past them, all under the lock.
// So blocks never overlap, are aligned as requested and stay inside the
// region, which the `Region` contract keeps valid. Nothing is reused until
// `reset`, whose own contract requires all blocks to be dead.
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.try_alloc(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    /// Does nothing besides updating the counters. See the crate docs.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        #[cfg(feature = "stats")]
        self.state.lock().stats.on_dealloc(_layout.size());
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // The region is not guaranteed to start zeroed, and `reset` hands
        // out dirty memory again, so zero every time.
        let p = self
            .try_alloc(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr);
        if !p.is_null() {
            // SAFETY: `p` was just allocated with room for `layout.size()`
            // bytes and nobody else has seen it yet.
            unsafe { ptr::write_bytes(p, 0, layout.size()) };
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees `new_size` rounded to the alignment
        // does not overflow `isize`, which is all `Layout` asks for.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let Ok(dst) = self.try_alloc(new) else {
            return ptr::null_mut();
        };
        // SAFETY: `ptr` is live for `layout.size()` bytes (caller contract),
        // `dst` is a fresh block of at least `new_size` bytes, and blocks
        // never overlap.
        unsafe {
            ptr::copy_nonoverlapping(ptr, dst.as_ptr(), layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        dst.as_ptr()
    }
}
