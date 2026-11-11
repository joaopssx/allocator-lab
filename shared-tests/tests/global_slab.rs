//! The slab allocator installed as the global allocator of a whole binary.
//!
//! This one runs without the libtest harness: the harness itself makes
//! allocations above 2048 bytes, which the slab allocator refuses by design,
//! and that aborts the process before any test runs. A plain `main` keeps
//! every allocation small.

use alloc_core::Arena;
use slab::Slab;
use std::alloc::Layout;

static ARENA: Arena<{ 4 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Slab = Slab::new(unsafe { ARENA.region() });

fn main() {
    shared_tests::exercise_std(2048);

    let b = Box::new(5u64);
    let addr = (&*b as *const u64).addr();
    assert!(HEAP.region().contains(addr, 8));

    assert!(
        HEAP.try_alloc(Layout::from_size_align(4096, 8).unwrap())
            .is_err()
    );

    // Spawning a thread makes std free a `Box` while it is still a protected
    // argument. An earlier version wrote its free list into freed blocks and
    // Miri flagged exactly this path, so keep it covered.
    std::thread::spawn(|| shared_tests::exercise_std(1024))
        .join()
        .unwrap();

    println!("global_slab: ok");
}
