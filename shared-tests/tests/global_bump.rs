//! The bump allocator installed as the global allocator of a whole test
//! binary, so the test harness itself runs on it too.

use alloc_core::Arena;
use bump::Bump;

static ARENA: Arena<{ 16 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Bump = Bump::new(unsafe { ARENA.region() });

#[test]
fn std_collections_work() {
    shared_tests::exercise_std(if cfg!(miri) { 4096 } else { 1 << 16 });
}

#[test]
fn allocations_come_from_the_arena() {
    let b = Box::new(5u64);
    let addr = (&*b as *const u64).addr();
    assert!(HEAP.region().contains(addr, 8));
}

#[test]
fn freeing_does_not_reclaim() {
    let before = HEAP.used();
    drop(vec![0u8; 100]);
    assert!(HEAP.used() >= before + 100);
}
