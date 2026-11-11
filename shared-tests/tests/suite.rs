//! The same behavioral checks, run against all four allocators.
//!
//! Each test is generic over [`Lab`] and the `suite!` macro instantiates it
//! once per allocator, so a failure names the allocator in the test path.

use shared_tests::{Block, Lab, check_accounting, check_blocks, fill, holds, tag, with};
use std::alloc::Layout;

const SIZE: usize = 1 << 16;

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// Allocates `layout` and returns the block, or `None` if the allocator
/// returned null.
fn get<A: Lab>(a: &A, layout: Layout) -> Option<Block> {
    // SAFETY: test layouts are never zero-sized when going through
    // `GlobalAlloc`.
    let ptr = unsafe { a.alloc(layout) };
    (!ptr.is_null()).then_some(Block { ptr, layout })
}

/// Frees a block obtained from `get`.
fn put<A: Lab>(a: &A, b: Block) {
    // SAFETY: `b` is live and has the layout it was allocated with.
    unsafe { a.dealloc(b.ptr, b.layout) }
}

fn mixed_sizes_stay_disjoint<A: Lab>() {
    for v in 0..A::VARIANTS {
        with::<A, _>(SIZE, v, |a| {
            let mut live = Vec::new();
            for (n, size) in [1, 7, 16, 33, 100, 255, 512, 1000, 2048, 24, 8]
                .into_iter()
                .enumerate()
            {
                let b = get(a, layout(size, 8)).expect("small request");
                // SAFETY: fresh block.
                unsafe { fill(b, tag(n)) };
                live.push(b);
                check_blocks(a, &live);
                check_accounting(a, &live);
            }
            for (n, b) in live.iter().enumerate() {
                // SAFETY: still live.
                assert!(unsafe { holds(*b, b.layout.size(), tag(n)) }, "{}", A::NAME);
            }
            for b in live.drain(..) {
                put(a, b);
            }
            check_accounting(a, &live);
        });
    }
}

fn alignments_up_to_a_page<A: Lab>() {
    with::<A, _>(SIZE, 0, |a| {
        let mut live = Vec::new();
        for shift in 0..=12 {
            let l = layout(24, 1 << shift);
            match get(a, l) {
                Some(b) => live.push(b),
                None => assert!(!A::serves(l), "{}: refused {l:?}", A::NAME),
            }
        }
        check_blocks(a, &live);
        check_accounting(a, &live);
    });
}

fn tiny_blocks_are_unique<A: Lab>() {
    with::<A, _>(SIZE, 0, |a| {
        // `GlobalAlloc` forbids zero sizes. Zero-sized `try_alloc` is covered
        // by each crate's own tests; here the smallest legal request must
        // still give distinct pointers.
        let l = layout(1, 1);
        let x = get(a, l).unwrap();
        let y = get(a, l).unwrap();
        assert_ne!(x.ptr, y.ptr, "{}", A::NAME);
    });
}

fn larger_than_region_returns_null<A: Lab>() {
    with::<A, _>(SIZE, 0, |a| {
        assert!(get(a, layout(SIZE + 1, 1)).is_none(), "{}", A::NAME);
        assert!(get(a, layout(isize::MAX as usize - 4095, 4096)).is_none());
        assert!(get(a, layout(isize::MAX as usize, 1)).is_none());
        // A refused request must not have consumed anything.
        assert_eq!(a.free_bytes(), a.capacity(), "{}", A::NAME);
    });
}

fn exhaustion_returns_null_then_recovers<A: Lab>() {
    with::<A, _>(1 << 14, 0, |a| {
        let l = layout(64, 8);
        let mut live = Vec::new();
        while let Some(b) = get(a, l) {
            live.push(b);
            assert!(live.len() < 1 << 14, "{}: never ran out", A::NAME);
        }
        check_blocks(a, &live);
        if A::FREES {
            put(a, live.pop().unwrap());
            assert!(get(a, l).is_some(), "{}: freed block not reusable", A::NAME);
        }
    });
}

fn realloc_keeps_contents<A: Lab>() {
    for v in 0..A::VARIANTS {
        with::<A, _>(SIZE, v, |a| {
            let mut b = get(a, layout(10, 8)).unwrap();
            // SAFETY: fresh block.
            unsafe { fill(b, 0xa5) };
            let mut kept = 10;
            for new in [12, 64, 200, 2000, 30, 1500, 16] {
                // SAFETY: `b` is live with layout `b.layout`.
                let p = unsafe { a.realloc(b.ptr, b.layout, new) };
                if p.is_null() {
                    assert!(!A::serves(layout(new, 8)), "{}: realloc to {new}", A::NAME);
                    continue;
                }
                b = Block {
                    ptr: p,
                    layout: layout(new, 8),
                };
                kept = kept.min(new);
                // SAFETY: the block holds at least `kept` old bytes.
                assert!(unsafe { holds(b, kept, 0xa5) }, "{}: {new}", A::NAME);
                // SAFETY: refill so the next round checks the whole prefix.
                unsafe { fill(b, 0xa5) };
                kept = new;
                check_blocks(a, &[b]);
                check_accounting(a, &[b]);
            }
            put(a, b);
        });
    }
}

fn zeroed_is_zero_even_after_reuse<A: Lab>() {
    with::<A, _>(SIZE, 0, |a| {
        let l = layout(256, 16);
        let b = get(a, l).unwrap();
        // SAFETY: fresh block.
        unsafe { fill(b, 0xff) };
        put(a, b);
        // SAFETY: valid layout.
        let p = unsafe { a.alloc_zeroed(l) };
        assert!(!p.is_null());
        let z = Block { ptr: p, layout: l };
        // SAFETY: fresh block of 256 bytes.
        assert!(unsafe { holds(z, 256, 0) }, "{}", A::NAME);
    });
}

fn threads_never_share_a_block<A: Lab>() {
    let rounds = if cfg!(miri) { 10 } else { 1000 };
    with::<A, _>(1 << 20, 0, |a| {
        std::thread::scope(|s| {
            for t in 0..4usize {
                s.spawn(move || {
                    let l = layout(8 + 24 * t, 8);
                    for i in 0..rounds {
                        let b = get(a, l).expect("enough room");
                        let x = tag(t * rounds + i);
                        // SAFETY: the block belongs to this thread only.
                        unsafe {
                            fill(b, x);
                            std::thread::yield_now();
                            assert!(holds(b, l.size(), x), "{}", A::NAME);
                        }
                        put(a, b);
                    }
                });
            }
        });
    });
}

macro_rules! suite {
    ($($name:ident: $ty:ty),* $(,)?) => {$(
        mod $name {
            #[test]
            fn mixed_sizes_stay_disjoint() {
                super::mixed_sizes_stay_disjoint::<$ty>();
            }
            #[test]
            fn alignments_up_to_a_page() {
                super::alignments_up_to_a_page::<$ty>();
            }
            #[test]
            fn tiny_blocks_are_unique() {
                super::tiny_blocks_are_unique::<$ty>();
            }
            #[test]
            fn larger_than_region_returns_null() {
                super::larger_than_region_returns_null::<$ty>();
            }
            #[test]
            fn exhaustion_returns_null_then_recovers() {
                super::exhaustion_returns_null_then_recovers::<$ty>();
            }
            #[test]
            fn realloc_keeps_contents() {
                super::realloc_keeps_contents::<$ty>();
            }
            #[test]
            fn zeroed_is_zero_even_after_reuse() {
                super::zeroed_is_zero_even_after_reuse::<$ty>();
            }
            #[test]
            fn threads_never_share_a_block() {
                super::threads_never_share_a_block::<$ty>();
            }
        }
    )*};
}

suite! {
    bump: ::bump::Bump,
    freelist: ::freelist::FreeList,
    slab: ::slab::Slab,
    buddy: ::buddy::Buddy,
}
