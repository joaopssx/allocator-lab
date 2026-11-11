//! Split and merge round trips for the buddy allocator.

use alloc_core::{AllocError, Region};
use buddy::{Buddy, MAX_ALIGN, bitmap_bytes, block_size, order_of};
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

    fn buddy(&self) -> Buddy {
        // SAFETY: one allocator per buffer, dropped before the buffer.
        Buddy::new(unsafe { Region::new(self.ptr, self.layout.size()) })
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

/// Region size whose block area is exactly `space` bytes.
fn sized_for(space: usize) -> usize {
    space + bitmap_bytes(space)
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

#[test]
fn whole_area_as_one_block_after_round_trip() {
    // Every 16-byte block of the area, so keep it small under Miri.
    let space = if cfg!(miri) { 1 << 11 } else { 1 << 16 };
    let buf = Buf::new(sized_for(space));
    let b = buf.buddy();
    assert_eq!(b.capacity(), space);

    for _ in 0..3 {
        let l = layout(16, 1);
        let mut ps = Vec::new();
        while let Ok(p) = b.try_alloc(l) {
            ps.push(p);
        }
        assert_eq!(ps.len(), space / 16);
        // Free the odd blocks first, so no merge can happen until the even
        // ones come back.
        let (odd, even): (Vec<_>, Vec<_>) = ps.iter().enumerate().partition(|(i, _)| i % 2 == 1);
        for (_, p) in odd.into_iter().chain(even) {
            // SAFETY: each block is freed once with its layout.
            unsafe { b.dealloc(p.as_ptr(), l) };
        }
        let all = b.try_alloc(layout(space, 1)).expect("everything merged");
        // SAFETY: as above.
        unsafe { b.dealloc(all.as_ptr(), layout(space, 1)) };
    }
}

#[test]
fn alignment_up_to_max() {
    let buf = Buf::new(sized_for(1 << 16));
    let b = buf.buddy();
    b.try_alloc(layout(16, 1)).unwrap();
    let mut a = 1;
    while a <= MAX_ALIGN {
        let p = b.try_alloc(layout(3, a)).unwrap().as_ptr().addr();
        assert_eq!(p % a, 0, "align {a}");
        a *= 2;
    }
    assert_eq!(
        b.try_alloc(layout(3, MAX_ALIGN * 2)),
        Err(AllocError::Unsupported)
    );
}

#[test]
fn oversized_and_zero_size_requests() {
    let buf = Buf::new(sized_for(4096));
    let b = buf.buddy();
    assert!(b.try_alloc(layout(4097, 1)).is_err());
    assert!(b.try_alloc(layout(isize::MAX as usize, 1)).is_err());
    let p = b.try_alloc(layout(0, 1)).unwrap();
    let q = b.try_alloc(layout(0, 1)).unwrap();
    assert_ne!(p, q);
    assert_eq!(b.free_bytes(), 4096 - 32);
}

#[test]
fn region_too_small() {
    let buf = Buf::new(4096);
    // SAFETY: a prefix of the buffer, used by this allocator only.
    let b = Buddy::new(unsafe { Region::new(buf.ptr, 16) });
    assert_eq!(b.capacity(), 0);
    assert!(b.try_alloc(layout(1, 1)).is_err());
}

#[test]
fn internal_fragmentation_is_rounding_to_a_power_of_two() {
    let buf = Buf::new(sized_for(1 << 14));
    let b = buf.buddy();
    // 1025 bytes cost a 2048 block; 2049 cost 4096.
    b.try_alloc(layout(1025, 1)).unwrap();
    assert_eq!(b.free_bytes(), (1 << 14) - 2048);
    b.try_alloc(layout(2049, 1)).unwrap();
    assert_eq!(b.free_bytes(), (1 << 14) - 2048 - 4096);
    assert_eq!(block_size(order_of(layout(2049, 1)).unwrap()), 4096);
}

#[test]
fn realloc_chain_keeps_data() {
    let buf = Buf::new(sized_for(1 << 15));
    let b = buf.buddy();
    // SAFETY: each pointer is used with the layout it currently has.
    unsafe {
        let mut p = b.alloc(layout(8, 8));
        p.cast::<u64>().write(0x0123_4567_89ab_cdef);
        let mut size = 8;
        for new in [16, 17, 4000, 5000, 100, 9000, 8] {
            p = b.realloc(p, layout(size, 8), new);
            assert!(!p.is_null());
            assert_eq!(
                p.cast::<u64>().read(),
                0x0123_4567_89ab_cdef,
                "{size} -> {new}"
            );
            size = new;
        }
        assert!(b.realloc(p, layout(size, 8), 1 << 16).is_null());
        assert_eq!(p.cast::<u64>().read(), 0x0123_4567_89ab_cdef);
        b.dealloc(p, layout(size, 8));
    }
    assert_eq!(b.free_bytes(), b.capacity());
}

#[test]
fn random_round_trips() {
    let rounds = if cfg!(miri) { 150 } else { 5000 };
    let buf = Buf::new(sized_for(1 << 16));
    let b = buf.buddy();
    let mut rng = Lcg(42);
    let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();

    for i in 0..rounds {
        if rng.below(10) < 6 || live.is_empty() {
            let l = layout(1 + rng.below(2000), 1 << rng.below(13));
            // SAFETY: valid non-zero layout.
            let p = unsafe { b.alloc(l) };
            if !p.is_null() {
                // SAFETY: fresh block of `l.size()` bytes.
                unsafe { p.write_bytes(i as u8, l.size()) };
                live.push((p, l, i as u8));
            }
        } else {
            let (p, l, tag) = live.swap_remove(rng.below(live.len()));
            // SAFETY: live block with layout `l`, filled with `tag`.
            unsafe {
                assert!((0..l.size()).all(|k| p.add(k).read() == tag));
                b.dealloc(p, l);
            }
        }

        let mut spans: Vec<_> = live
            .iter()
            .map(|(p, l, _)| (p.addr(), p.addr() + l.size()))
            .collect();
        spans.sort_unstable();
        assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0), "overlap");
        assert!(
            live.iter()
                .all(|(p, l, _)| p.addr() % l.align() == 0
                    && b.region().contains(p.addr(), l.size()))
        );
    }

    for (p, l, _) in live {
        // SAFETY: still live with layout `l`.
        unsafe { b.dealloc(p, l) };
    }
    assert_eq!(b.free_bytes(), b.capacity());
    assert!(b.try_alloc(layout(1 << 16, 1)).is_ok());
}

#[test]
fn concurrent_round_trips() {
    let buf = Buf::new(sized_for(1 << 18));
    let b = buf.buddy();
    let rounds = if cfg!(miri) { 20 } else { 2000 };
    std::thread::scope(|s| {
        for t in 0..4usize {
            let b = &b;
            s.spawn(move || {
                let l = layout(16 << t, 8);
                for _ in 0..rounds {
                    // SAFETY: each block is owned by this thread and freed
                    // once with its layout.
                    unsafe {
                        let p = b.alloc(l);
                        assert!(!p.is_null());
                        p.write_bytes(t as u8, l.size());
                        assert!((0..l.size()).all(|k| p.add(k).read() == t as u8));
                        b.dealloc(p, l);
                    }
                }
            });
        }
    });
    assert_eq!(b.free_bytes(), b.capacity());
    assert!(b.try_alloc(layout(1 << 18, 1)).is_ok());
}
