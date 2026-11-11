//! Class boundaries and exhaustion for the slab allocator.

use alloc_core::{AllocError, Region};
use slab::{SLAB, Slab, region_for};
use std::alloc::{GlobalAlloc, Layout, alloc, dealloc};
use std::ptr::NonNull;

/// Page-aligned heap buffer that outlives the allocator built on it.
struct Buf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Buf {
    fn new(size: usize) -> Buf {
        let layout = Layout::from_size_align(size, SLAB).unwrap();
        // SAFETY: every caller passes a non-zero size.
        let ptr = NonNull::new(unsafe { alloc(layout) }).expect("test buffer");
        Buf { ptr, layout }
    }

    fn slab(&self) -> Slab {
        // SAFETY: one allocator per buffer, dropped before the buffer.
        Slab::new(unsafe { Region::new(self.ptr, self.layout.size()) })
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
fn each_boundary_lands_in_the_right_class() {
    // (request, class size): the edges on both sides of every class.
    let cases = [
        (1, 16),
        (16, 16),
        (17, 32),
        (32, 32),
        (33, 64),
        (64, 64),
        (65, 128),
        (128, 128),
        (129, 256),
        (256, 256),
        (257, 512),
        (512, 512),
        (513, 1024),
        (1024, 1024),
        (1025, 2048),
        (2048, 2048),
    ];
    for (size, class) in cases {
        let buf = Buf::new(region_for(1));
        let s = buf.slab();
        let a = s.try_alloc(layout(size, 1)).unwrap().as_ptr().addr();
        let b = s.try_alloc(layout(size, 1)).unwrap().as_ptr().addr();
        // Consecutive fresh blocks of a class are exactly one class apart.
        assert_eq!(b - a, class, "size {size}");
        assert_eq!(a % class, 0);
        assert_eq!(s.free_bytes(), SLAB - 2 * class);
    }
}

#[test]
fn unsupported_requests_return_null() {
    let buf = Buf::new(2 * SLAB);
    let s = buf.slab();
    assert_eq!(s.try_alloc(layout(2049, 1)), Err(AllocError::Unsupported));
    assert_eq!(s.try_alloc(layout(8, 4096)), Err(AllocError::Unsupported));
    assert_eq!(
        s.try_alloc(layout(isize::MAX as usize, 1)),
        Err(AllocError::Unsupported)
    );
    // SAFETY: valid layouts.
    unsafe {
        assert!(s.alloc(layout(4096, 8)).is_null());
        assert!(s.alloc_zeroed(layout(10_000, 8)).is_null());
    }
    // Nothing was carved for requests that were turned away.
    assert_eq!(s.slabs(), 0);
}

#[test]
fn alignment_up_to_2048() {
    // Twelve alignments land in eight classes, one slab each.
    let buf = Buf::new(region_for(8));
    let s = buf.slab();
    for shift in 0..=11 {
        let align = 1 << shift;
        let p = s.try_alloc(layout(3, align)).unwrap().as_ptr().addr();
        assert_eq!(p % align, 0, "align {align}");
    }
}

#[test]
fn zero_size_gets_a_real_block() {
    let buf = Buf::new(region_for(1));
    let s = buf.slab();
    let a = s.try_alloc(layout(0, 1)).unwrap();
    let b = s.try_alloc(layout(0, 1)).unwrap();
    assert_ne!(a, b);
    // SAFETY: both were allocated with this layout.
    unsafe {
        s.dealloc(a.as_ptr(), layout(0, 1));
        s.dealloc(b.as_ptr(), layout(0, 1));
    }
    assert_eq!(s.free_bytes(), SLAB);
}

#[test]
fn exhaustion_is_per_class() {
    let buf = Buf::new(region_for(2));
    let s = buf.slab();
    let small = layout(16, 1);
    let mut ps = Vec::new();
    while let Ok(p) = s.try_alloc(small) {
        ps.push(p);
    }
    assert_eq!(ps.len(), 2 * SLAB / 16);
    assert_eq!(s.free_bytes(), 0);

    // SAFETY: the block was allocated with `small`.
    unsafe { s.dealloc(ps.pop().unwrap().as_ptr(), small) };
    // 16 bytes are free again, but only the 16-byte class can use them.
    assert_eq!(s.free_bytes(), 16);
    assert_eq!(s.try_alloc(layout(17, 1)), Err(AllocError::Exhausted));
    assert!(s.try_alloc(small).is_ok());
}

#[test]
fn region_smaller_than_a_slab_or_misaligned() {
    let buf = Buf::new(region_for(3));
    // SAFETY: prefix of the buffer, owned by this allocator only.
    let tiny = Slab::new(unsafe { Region::new(buf.ptr, SLAB - 1) });
    assert_eq!(tiny.capacity(), 0);
    assert_eq!(tiny.try_alloc(layout(1, 1)), Err(AllocError::Exhausted));

    // Starting one byte in loses the first slab entirely.
    let off = NonNull::new(buf.ptr.as_ptr().wrapping_add(1)).unwrap();
    // SAFETY: the rest of the buffer, also owned by this allocator only;
    // `tiny` is never used again.
    let s = Slab::new(unsafe { Region::new(off, region_for(3) - 1) });
    assert_eq!(s.capacity(), 2 * SLAB);
    let p = s.try_alloc(layout(1, 1)).unwrap().as_ptr().addr();
    assert_eq!(p, buf.ptr.as_ptr().addr() + SLAB);
}

#[test]
fn realloc_across_classes_keeps_data() {
    let buf = Buf::new(region_for(4));
    let s = buf.slab();
    // SAFETY: each pointer is used with the layout it currently has.
    unsafe {
        let mut p = s.alloc(layout(10, 1));
        for i in 0..10 {
            p.add(i).write(i as u8);
        }
        let mut size = 10;
        for new in [16, 300, 2048, 40] {
            p = s.realloc(p, layout(size, 1), new);
            assert!(!p.is_null());
            assert!(
                (0..10).all(|i| p.add(i).read() == i as u8),
                "{size} -> {new}"
            );
            size = new;
        }
        // Growing past the largest class fails and leaves the block alone.
        assert!(s.realloc(p, layout(size, 1), 2049).is_null());
        assert!((0..10).all(|i| p.add(i).read() == i as u8));
        s.dealloc(p, layout(size, 1));
    }
}

#[test]
fn zeroed_after_reuse() {
    let buf = Buf::new(region_for(1));
    let s = buf.slab();
    let l = layout(64, 8);
    // SAFETY: the dirty block is freed before the zeroed one reuses it.
    unsafe {
        let p = s.alloc(l);
        p.write_bytes(0xcc, 64);
        s.dealloc(p, l);
        let q = s.alloc_zeroed(l);
        assert_eq!(p, q);
        assert!((0..64).all(|i| q.add(i).read() == 0));
    }
}

#[test]
fn concurrent_classes() {
    // Each thread keeps about two thirds of its blocks, up to 2000 blocks of
    // 128 bytes for the largest class, so 1 MiB leaves room for all four.
    let buf = Buf::new(region_for(256));
    let s = buf.slab();
    let rounds = if cfg!(miri) { 30 } else { 3000 };
    std::thread::scope(|sc| {
        for t in 0..4usize {
            let s = &s;
            sc.spawn(move || {
                let l = layout(16 << t, 8);
                let mut held = Vec::new();
                for i in 0..rounds {
                    // SAFETY: blocks are owned by this thread and freed once
                    // with their layout.
                    unsafe {
                        let p = s.alloc(l);
                        assert!(!p.is_null());
                        p.write_bytes(t as u8, l.size());
                        held.push(p);
                        if i % 3 == 0 {
                            let q = held.swap_remove(0);
                            assert!((0..l.size()).all(|k| q.add(k).read() == t as u8));
                            s.dealloc(q, l);
                        }
                    }
                }
                for p in held {
                    // SAFETY: as above.
                    unsafe { s.dealloc(p, l) };
                }
            });
        }
    });
    assert_eq!(s.free_bytes(), s.capacity());
}
