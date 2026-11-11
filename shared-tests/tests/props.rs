//! Property tests: random sequences of alloc, free and realloc against every
//! allocator, checking after each step that
//!
//! - no two live blocks overlap,
//! - every block is aligned and inside the region,
//! - free bytes plus what the live blocks cost add up to the capacity,
//! - every block still holds the bytes last written to it.
//!
//! The last one catches allocator metadata written over a live block, which
//! the first three would miss.

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::Config;
use shared_tests::{Block, Lab, check_accounting, check_blocks, fill, full, holds, tag, with};
use std::alloc::Layout;

#[derive(Debug, Clone)]
enum Op {
    /// Size and log2 of the alignment.
    Alloc(usize, u32),
    /// Index into the live blocks, taken modulo their count.
    Free(usize),
    /// Index into the live blocks and the new size.
    Realloc(usize, usize),
}

fn op() -> impl Strategy<Value = Op> {
    // Every step re-reads every live byte, which is what makes Miri slow, so
    // the default Miri profile keeps blocks small. Sizes past 2048 matter for
    // the slab allocator's refusal path, and the native runs cover them.
    let max: usize = if cfg!(miri) && !full() { 600 } else { 3000 };
    prop_oneof![
        4 => (1usize..max, 0u32..13).prop_map(|(s, a)| Op::Alloc(s, a)),
        3 => any::<usize>().prop_map(Op::Free),
        2 => (any::<usize>(), 1usize..max).prop_map(|(i, s)| Op::Realloc(i, s)),
    ]
}

fn config() -> Config {
    let cases = match (cfg!(miri), full()) {
        (true, false) => 3,
        (true, true) => 64,
        (false, false) => 256,
        (false, true) => 4096,
    };
    let mut c = Config::with_cases(cases);
    if cfg!(miri) {
        // Miri isolates the file system, so no regression files.
        c.failure_persistence = None;
    }
    c
}

fn steps() -> std::ops::Range<usize> {
    match (cfg!(miri), full()) {
        (true, false) => 1..25,
        (true, true) => 1..100,
        _ => 1..200,
    }
}

/// A live block and the byte it is filled with.
struct Live {
    block: Block,
    tag: u8,
}

fn run<A: Lab>(ops: &[Op]) {
    for v in 0..A::VARIANTS {
        with::<A, _>(1 << 16, v, |a| {
            let mut live: Vec<Live> = Vec::new();
            for (n, op) in ops.iter().enumerate() {
                apply(a, &mut live, n, op);
                let blocks: Vec<Block> = live.iter().map(|l| l.block).collect();
                check_blocks(a, &blocks);
                check_accounting(a, &blocks);
                for l in &live {
                    // SAFETY: every entry in `live` is a live block.
                    let ok = unsafe { holds(l.block, l.block.layout.size(), l.tag) };
                    assert!(ok, "{} v{v}: block corrupted after {op:?}", A::NAME);
                }
            }
            for l in live.drain(..) {
                // SAFETY: live block with its current layout.
                unsafe { a.dealloc(l.block.ptr, l.block.layout) };
            }
            if A::FREES {
                assert_eq!(a.free_bytes(), a.capacity(), "{} v{v}: leaked", A::NAME);
            }
        });
    }
}

fn apply<A: Lab>(a: &A, live: &mut Vec<Live>, n: usize, op: &Op) {
    match *op {
        Op::Alloc(size, shift) => {
            let layout = Layout::from_size_align(size, 1 << shift).unwrap();
            // SAFETY: non-zero size.
            let ptr = unsafe { a.alloc(layout) };
            if ptr.is_null() {
                return;
            }
            let block = Block { ptr, layout };
            // SAFETY: fresh block.
            unsafe { fill(block, tag(n)) };
            live.push(Live { block, tag: tag(n) });
        }
        Op::Free(i) if !live.is_empty() => {
            let l = live.swap_remove(i % live.len());
            // SAFETY: live block with its current layout.
            unsafe { a.dealloc(l.block.ptr, l.block.layout) };
        }
        Op::Realloc(i, size) if !live.is_empty() => {
            let i = i % live.len();
            let old = live[i].block;
            // SAFETY: live block with its current layout; `size` is non-zero
            // and far below `isize::MAX`.
            let ptr = unsafe { a.realloc(old.ptr, old.layout, size) };
            if ptr.is_null() {
                // The old block must be untouched; the content check after
                // this step verifies it.
                return;
            }
            let layout = Layout::from_size_align(size, old.layout.align()).unwrap();
            let block = Block { ptr, layout };
            let kept = old.layout.size().min(size);
            // SAFETY: the new block holds the old prefix.
            let ok = unsafe { holds(block, kept, live[i].tag) };
            assert!(ok, "{}: realloc lost data", A::NAME);
            // SAFETY: refill the grown part too.
            unsafe { fill(block, live[i].tag) };
            live[i].block = block;
        }
        Op::Free(_) | Op::Realloc(..) => {}
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn bump_random_ops(ops in vec(op(), steps())) {
        run::<bump::Bump>(&ops);
    }

    #[test]
    fn freelist_random_ops(ops in vec(op(), steps())) {
        run::<freelist::FreeList>(&ops);
    }

    #[test]
    fn slab_random_ops(ops in vec(op(), steps())) {
        run::<slab::Slab>(&ops);
    }

    #[test]
    fn buddy_random_ops(ops in vec(op(), steps())) {
        run::<buddy::Buddy>(&ops);
    }
}
