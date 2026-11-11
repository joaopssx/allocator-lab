//! Split, coalesce and alignment cases for the free-list allocator, run
//! against every placement policy.

use alloc_core::Region;
use freelist::{Fit, FreeList};
use std::alloc::{GlobalAlloc, Layout, alloc, dealloc};
use std::ptr::NonNull;

const FITS: [Fit; 3] = [Fit::First, Fit::Best, Fit::Next];

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

    fn list(&self, fit: Fit) -> FreeList {
        // SAFETY: each test drops the allocator before the buffer, and builds
        // one allocator per buffer at a time.
        FreeList::new(unsafe { Region::new(self.ptr, self.layout.size()) }, fit)
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

/// Small deterministic generator, so failures reproduce without a seed file.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn large_alignments() {
    for fit in FITS {
        let buf = Buf::new(5 * 4096);
        let fl = buf.list(fit);
        // Push the free chunk off page alignment first.
        fl.try_alloc(layout(24, 8)).unwrap();
        for align in [32, 64, 512, 4096] {
            let p = fl.try_alloc(layout(10, align)).unwrap();
            assert_eq!(p.as_ptr().addr() % align, 0, "{fit:?} align {align}");
        }
    }
}

#[test]
fn front_padding_is_not_lost() {
    for fit in FITS {
        let buf = Buf::new(8192);
        let fl = buf.list(fit);
        let l = layout(1, 1);
        let small = fl.try_alloc(l).unwrap();
        let page = layout(16, 4096);
        let big = fl.try_alloc(page).unwrap();
        // SAFETY: both blocks are live and freed with their own layouts.
        unsafe {
            fl.dealloc(small.as_ptr(), l);
            fl.dealloc(big.as_ptr(), page);
        }
        assert_eq!(fl.free_bytes(), fl.capacity(), "{fit:?}");
    }
}

#[test]
fn zero_size_and_oversized_requests() {
    for fit in FITS {
        let buf = Buf::new(4096);
        let fl = buf.list(fit);
        let a = fl.try_alloc(layout(0, 1)).unwrap();
        let b = fl.try_alloc(layout(0, 1)).unwrap();
        assert_ne!(a, b);
        assert!(fl.try_alloc(layout(4096, 1)).is_err());
        assert!(
            fl.try_alloc(layout(isize::MAX as usize - 4095, 4096))
                .is_err()
        );
        assert!(fl.try_alloc(layout(isize::MAX as usize, 1)).is_err());
    }
}

#[test]
fn region_too_small_for_any_block() {
    let buf = Buf::new(4096);
    // SAFETY: a prefix of the buffer, used by this allocator only.
    let fl = FreeList::new(unsafe { Region::new(buf.ptr, 31) }, Fit::First);
    assert_eq!(fl.capacity(), 16);
    assert!(fl.try_alloc(layout(1, 1)).is_err());
}

#[test]
fn fragmentation_can_fail_with_enough_free_bytes() {
    let buf = Buf::new(1024);
    let fl = buf.list(Fit::First);
    let l = layout(48, 16);
    // SAFETY: each pointer is freed once with its layout.
    unsafe {
        let ps: Vec<_> = (0..16).map(|_| fl.alloc(l)).collect();
        assert!(ps.iter().all(|p| !p.is_null()));
        for p in ps.iter().step_by(2) {
            fl.dealloc(*p, l);
        }
        // Half the region is free, but in 64-byte pieces.
        assert_eq!(fl.free_bytes(), 512);
        assert!(fl.alloc(layout(100, 16)).is_null());
        // Freeing one neighbor merges three holes into one.
        fl.dealloc(ps[1], l);
        assert!(!fl.alloc(layout(100, 16)).is_null());
    }
}

#[test]
fn zeroed_after_reuse() {
    let buf = Buf::new(1024);
    let fl = buf.list(Fit::First);
    let l = layout(64, 8);
    // SAFETY: the dirty block is freed before the zeroed one reuses it.
    unsafe {
        let p = fl.alloc(l);
        p.write_bytes(0xee, 64);
        fl.dealloc(p, l);
        let q = fl.alloc_zeroed(l);
        assert_eq!(p, q);
        assert!((0..64).all(|i| q.add(i).read() == 0));
    }
}

#[test]
fn realloc_failure_keeps_old_block() {
    let buf = Buf::new(256);
    let fl = buf.list(Fit::First);
    let l = layout(32, 8);
    // SAFETY: `p` stays live and is used with its current layout.
    unsafe {
        let p = fl.alloc(l);
        p.write_bytes(9, 32);
        assert!(fl.realloc(p, l, 1000).is_null());
        assert!((0..32).all(|i| p.add(i).read() == 9));
        fl.dealloc(p, l);
    }
    assert_eq!(fl.free_bytes(), fl.capacity());
}

/// Random alloc, free and realloc. Every block is filled with a byte derived
/// from its index and checked before it goes away, so metadata written over a
/// payload shows up as a mismatch.
#[test]
fn random_workload_returns_to_one_chunk() {
    let rounds = if cfg!(miri) { 150 } else { 5000 };
    for fit in FITS {
        let buf = Buf::new(1 << 16);
        let fl = buf.list(fit);
        let mut rng = Lcg(0x1234 + fit as u64);
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();

        for i in 0..rounds {
            let op = rng.below(10);
            if op < 5 || live.is_empty() {
                let l = layout(1 + rng.below(300), [1, 8, 16, 64, 256, 4096][rng.below(6)]);
                // SAFETY: valid non-zero layout.
                let p = unsafe { fl.alloc(l) };
                if !p.is_null() {
                    let tag = i as u8;
                    // SAFETY: fresh block of `l.size()` bytes.
                    unsafe { p.write_bytes(tag, l.size()) };
                    live.push((p, l, tag));
                }
            } else if op < 8 {
                let (p, l, tag) = live.swap_remove(rng.below(live.len()));
                // SAFETY: `p` is live with layout `l`, filled with `tag`.
                unsafe {
                    assert!((0..l.size()).all(|k| p.add(k).read() == tag));
                    fl.dealloc(p, l);
                }
            } else {
                let k = rng.below(live.len());
                let (p, l, tag) = live[k];
                let new = 1 + rng.below(600);
                // SAFETY: `p` is live with layout `l`.
                let q = unsafe { fl.realloc(p, l, new) };
                if !q.is_null() {
                    let nl = layout(new, l.align());
                    // SAFETY: `q` holds the old prefix and has `new` bytes.
                    unsafe {
                        let kept = l.size().min(new);
                        assert!((0..kept).all(|j| q.add(j).read() == tag));
                        q.write_bytes(tag, new);
                    }
                    live[k] = (q, nl, tag);
                }
            }

            let mut spans: Vec<_> = live
                .iter()
                .map(|(p, l, _)| (p.addr(), p.addr() + l.size()))
                .collect();
            spans.sort_unstable();
            assert!(
                spans.windows(2).all(|w| w[0].1 <= w[1].0),
                "{fit:?}: overlap"
            );
            for (p, l, _) in &live {
                assert_eq!(p.addr() % l.align(), 0);
                assert!(fl.region().contains(p.addr(), l.size()));
            }
        }

        for (p, l, _) in live {
            // SAFETY: still live with layout `l`.
            unsafe { fl.dealloc(p, l) };
        }
        assert_eq!(fl.free_bytes(), fl.capacity(), "{fit:?}");
        // Everything merged: the whole capacity fits in one block again.
        assert!(
            fl.try_alloc(layout(fl.capacity() - 16, 16)).is_ok(),
            "{fit:?}"
        );
    }
}

#[test]
fn concurrent_alloc_and_free() {
    let buf = Buf::new(1 << 16);
    let fl = buf.list(Fit::First);
    let rounds = if cfg!(miri) { 20 } else { 2000 };
    std::thread::scope(|s| {
        for t in 0..4 {
            let fl = &fl;
            s.spawn(move || {
                let l = layout(8 + t * 8, 8);
                for _ in 0..rounds {
                    // SAFETY: each block is written and freed by this thread
                    // only, with its own layout.
                    unsafe {
                        let p = fl.alloc(l);
                        assert!(!p.is_null());
                        p.write_bytes(t as u8, l.size());
                        assert!((0..l.size()).all(|k| p.add(k).read() == t as u8));
                        fl.dealloc(p, l);
                    }
                }
            });
        }
    });
    assert_eq!(fl.free_bytes(), fl.capacity());
}
