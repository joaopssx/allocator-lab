//! The freelist allocator installed as the global allocator of a whole test
//! binary, so the test harness itself runs on it too.

use alloc_core::Arena;
use freelist::FreeList;

static ARENA: Arena<{ 4 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: FreeList = FreeList::new(unsafe { ARENA.region() }, freelist::Fit::First);

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
fn freed_memory_comes_back() {
    // 50 blocks of 100 KB add up to more than the 4 MiB arena, so this only
    // passes if `dealloc` really returns memory.
    for i in 0..50u8 {
        let v = vec![i; 100_000];
        assert_eq!(v[99_999], i);
    }
}
