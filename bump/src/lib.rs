//! Bump allocator over a fixed [`Region`].
//!
//! Allocation moves a cursor forward. Freeing an individual block does
//! nothing; memory comes back only when the whole allocator is reset.

#![no_std]

use alloc_core::{AllocError, Region, SpinLock};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

/// A bump allocator.
pub struct Bump {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Offset of the first unused byte.
    next: usize,
}

impl Bump {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> Bump {
        Bump {
            region,
            state: SpinLock::new(State { next: 0 }),
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

    /// Allocates a block for `layout`.
    pub fn try_alloc(&self, _layout: Layout) -> Result<ptr::NonNull<u8>, AllocError> {
        Err(AllocError::Exhausted)
    }
}

// SAFETY: not yet a working allocator; `alloc` always reports failure with a
// null pointer, which the trait permits.
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.try_alloc(layout)
            .map_or(ptr::null_mut(), ptr::NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded from the caller.
        unsafe { self.alloc(layout) }
    }

    unsafe fn realloc(&self, _ptr: *mut u8, _layout: Layout, _new_size: usize) -> *mut u8 {
        ptr::null_mut()
    }
}
