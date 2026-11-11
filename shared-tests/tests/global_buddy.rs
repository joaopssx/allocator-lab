//! The buddy allocator installed as the global allocator of a whole test
//! binary, so the test harness itself runs on it too.

use alloc_core::Arena;
use buddy::Buddy;

static ARENA: Arena<{ 4 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Buddy = Buddy::new(unsafe { ARENA.region() });

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
fn page_aligned_box() {
    #[repr(align(4096))]
    struct Page([u8; 4096]);
    let p = Box::new(Page([3; 4096]));
    assert_eq!((&*p as *const Page).addr() % 4096, 0);
    assert_eq!(p.0[4095], 3);
}
