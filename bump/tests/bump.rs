//! Unit tests for the bump allocator.

use alloc_core::{AllocError, Region};
use bump::Bump;
use std::alloc::{GlobalAlloc, Layout, alloc, dealloc};
use std::ptr::NonNull;

/// Page-aligned heap buffer that outlives the allocator built on it.
struct Buf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Buf {
    fn new(size: usize) -> Buf {
        let layout = Layout::from_size_align(size, 4096).unwrap();
        // SAFETY: every caller passes a non-zero size.
        let ptr = NonNull::new(unsafe { alloc(layout) }).expect("test buffer");
        Buf { ptr, layout }
    }

    fn bump(&self) -> Bump {
        // SAFETY: each test builds exactly one allocator per buffer and drops
        // it before the buffer.
        Bump::new(unsafe { Region::new(self.ptr, self.layout.size()) })
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        // SAFETY: allocated in `Buf::new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

#[test]
fn blocks_are_aligned_disjoint_and_inside() {
    let buf = Buf::new(4096);
    let a = buf.bump();
    let mut prev_end = a.region().start();
    for (size, align) in [(1, 1), (3, 2), (8, 8), (5, 16), (100, 64), (7, 4)] {
        let p = a.try_alloc(layout(size, align)).unwrap().as_ptr().addr();
        assert_eq!(p % align, 0);
        assert!(p >= prev_end, "block overlaps the previous one");
        assert!(a.region().contains(p, size));
        prev_end = p + size;
    }
    assert_eq!(a.used(), prev_end - a.region().start());
}

#[test]
fn page_alignment() {
    let buf = Buf::new(3 * 4096);
    let a = buf.bump();
    a.try_alloc(layout(1, 1)).unwrap();
    let p = a.try_alloc(layout(10, 4096)).unwrap();
    assert_eq!(p.as_ptr().addr() % 4096, 0);
    assert_eq!(a.used(), 4096 + 10);
}

#[test]
fn zero_size_requests_get_distinct_pointers() {
    let buf = Buf::new(64);
    let a = buf.bump();
    let p = a.try_alloc(layout(0, 1)).unwrap();
    let q = a.try_alloc(layout(0, 1)).unwrap();
    assert_ne!(p, q);
}

#[test]
fn exhaustion_and_oversized_requests() {
    let buf = Buf::new(64);
    let a = buf.bump();
    assert_eq!(a.try_alloc(layout(65, 1)), Err(AllocError::Exhausted));
    a.try_alloc(layout(64, 1)).unwrap();
    assert_eq!(a.try_alloc(layout(1, 1)), Err(AllocError::Exhausted));
    // SAFETY: valid non-zero layout.
    assert!(unsafe { a.alloc(layout(1, 1)) }.is_null());
}

#[test]
fn huge_layouts_fail_without_overflowing() {
    let buf = Buf::new(64);
    let a = buf.bump();
    let max = isize::MAX as usize;
    assert!(a.try_alloc(layout(max - 4095, 4096)).is_err());
    assert!(a.try_alloc(layout(max, 1)).is_err());
    assert_eq!(a.used(), 0);
}

#[test]
fn realloc_grows_last_block_in_place() {
    let buf = Buf::new(256);
    let a = buf.bump();
    let l = layout(8, 8);
    // SAFETY: every pointer below comes from `a` with the layout passed back.
    unsafe {
        let p = a.alloc(l);
        p.write_bytes(0xab, 8);
        let q = a.realloc(p, l, 64);
        assert_eq!(p, q);
        assert_eq!(a.used(), 64);
        assert_eq!(q.add(7).read(), 0xab);
    }
}

#[test]
fn realloc_moves_when_not_last() {
    let buf = Buf::new(256);
    let a = buf.bump();
    let l = layout(8, 8);
    // SAFETY: as above.
    unsafe {
        let p = a.alloc(l);
        for i in 0..8 {
            p.add(i).write(i as u8);
        }
        let _other = a.alloc(l);
        let q = a.realloc(p, l, 32);
        assert_ne!(p, q);
        for i in 0..8 {
            assert_eq!(q.add(i).read(), i as u8);
        }
    }
}

#[test]
fn realloc_shrinks_in_place() {
    let buf = Buf::new(256);
    let a = buf.bump();
    // SAFETY: as above.
    unsafe {
        let p = a.alloc(layout(32, 8));
        let q = a.alloc(layout(64, 8));
        // Shrinking a block that is not last keeps the space.
        assert_eq!(a.realloc(p, layout(32, 8), 8), p);
        assert_eq!(a.used(), 96);
        // Shrinking the last one gives the tail back.
        assert_eq!(a.realloc(q, layout(64, 8), 16), q);
        assert_eq!(a.used(), 48);
    }
}

#[test]
fn realloc_failure_keeps_old_block() {
    let buf = Buf::new(64);
    let a = buf.bump();
    // SAFETY: as above.
    unsafe {
        let p = a.alloc(layout(16, 8));
        p.write(7);
        assert!(a.realloc(p, layout(16, 8), 65).is_null());
        assert_eq!(p.read(), 7);
        assert_eq!(a.used(), 16);
    }
}

#[test]
fn reset_starts_over_and_zeroed_clears_dirty_memory() {
    let buf = Buf::new(128);
    let a = buf.bump();
    // SAFETY: the block is dead before `reset`.
    unsafe {
        let p = a.alloc(layout(128, 1));
        p.write_bytes(0xff, 128);
        a.reset();
        assert_eq!(a.used(), 0);
        let q = a.alloc_zeroed(layout(128, 1));
        assert_eq!(p, q);
        assert!((0..128).all(|i| q.add(i).read() == 0));
    }
}

#[test]
fn concurrent_allocations_do_not_overlap() {
    let buf = Buf::new(1 << 16);
    let a = buf.bump();
    let per_thread = if cfg!(miri) { 20 } else { 500 };

    let mut blocks: Vec<usize> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    (0..per_thread)
                        .map(|_| a.try_alloc(layout(16, 16)).unwrap().as_ptr().addr())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    blocks.sort_unstable();
    assert!(blocks.windows(2).all(|w| w[1] - w[0] >= 16));
    assert_eq!(a.used(), 4 * per_thread * 16);
}

#[cfg(feature = "stats")]
#[test]
fn stats_follow_requests() {
    let buf = Buf::new(256);
    let a = buf.bump();
    // SAFETY: as above.
    unsafe {
        let p = a.alloc(layout(10, 1));
        a.realloc(p, layout(10, 1), 40);
        a.dealloc(a.alloc(layout(20, 1)), layout(20, 1));
        assert!(a.alloc(layout(1000, 1)).is_null());
    }
    let s = a.stats();
    assert_eq!((s.allocs, s.deallocs, s.reallocs, s.failures), (2, 1, 1, 1));
    assert_eq!(s.live_bytes, 40);
    assert_eq!(s.peak_bytes, 60);
}
