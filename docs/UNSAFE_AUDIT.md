# Unsafe audit

Every `unsafe` in the non-test code, what it relies on and what exercises it.
Line numbers are as of the commit that added this file.

Two lints keep this list honest. `clippy::undocumented_unsafe_blocks` is set
to deny, so every `unsafe` block and `unsafe impl` carries a `SAFETY:`
comment, and `clippy::missing_safety_doc` makes every public `unsafe fn`
document its contract under `# Safety`. `unsafe_op_in_unsafe_fn` is denied
too, so the body of an `unsafe fn` still needs its own blocks, each with its
own justification. Private `unsafe fn`s follow the same convention by hand.

## The shape of it

Each allocator crate has a `raw` module. It is the only code that turns an
address into a pointer and dereferences it. Everything above `raw` works with
plain `usize` addresses, and its unsafety is limited to "I promise this
address is what `raw` expects". The promises come from invariants written in
each design doc, and with `debug-checks` on, those invariants are verified
after every operation.

All pointers are derived from the region's base pointer with `with_addr`,
so provenance is preserved. Miri runs with `-Zmiri-strict-provenance` to
check that.

After the redesign described in [buddy.md](buddy.md) and [slab.md](slab.md),
only the free-list allocator writes into memory it once handed out. It
writes headers in front of payloads and free-list nodes in freed chunks, but
never inside a payload that a caller might still be protecting.

## Table

"Miri" below means `cargo +nightly miri test` with strict provenance, which
the CI runs on every crate except `bench`.

| Location | What | Why it is sound | Exercised by |
|---|---|---|---|
| core/src/region.rs:21, 26 | `unsafe impl Send`/`Sync for Region` | `Region` is an exclusive token. Through `&Region` you can only read two integers and build raw pointers, and dereferencing those is unsafe on its own. | Every allocator used from several threads: `threads_never_share_a_block` in the shared suite, and the per-crate concurrency tests, under Miri. |
| core/src/region.rs:36 | `unsafe fn Region::new` | Contract: the range is valid and nobody else touches it for as long as the region is in use. Every caller states which buffer it uses and why it is exclusive. | All tests build regions this way; Miri catches use of a freed buffer. |
| core/src/region.rs:120 | `unsafe impl Sync for Arena<N>` | The arena never touches its own bytes. Access goes through the one `Region` it hands out. | `global_*` tests, under Miri. |
| core/src/region.rs:137, 142 | `unsafe fn Arena::region`, `NonNull::new_unchecked` | `UnsafeCell::get` on a live static is never null. Calling it at most once is the caller's contract. | Doc test in region.rs; every `#[global_allocator]` static. |
| core/src/lock.rs:31 | `unsafe impl Sync for SpinLock<T>` | At most one guard exists at a time, so at most one `&mut T`. Requires `T: Send`. | `excludes_concurrent_writers` (4 threads, lost updates would fail the count), under Miri; I also ran it with several `-Zmiri-seed` values while writing the lock. |
| core/src/lock.rs:83, 91 | `&*`/`&mut *` on the `UnsafeCell` in the guard | A guard exists only between a successful `Acquire` exchange and its `Release` store. | Same test; Miri's data race detector would flag a missing lock. |
| bump/src/lib.rs:95 | `unsafe fn Bump::reset` | Contract: every block handed out before is dead. | `reset_starts_over_and_zeroed_clears_dirty_memory`, the `game-frame` bench. |
| bump/src/lib.rs:155 | `unsafe impl GlobalAlloc for Bump` | Blocks come from a cursor that only moves forward under the lock; see the SAFETY comment and docs/bump.md invariants. | bump/tests, shared suite, props, `global_bump`. |
| bump/src/lib.rs:176; freelist/src/lib.rs:535; slab/src/lib.rs:344; buddy/src/lib.rs:473 | `write_bytes` in `alloc_zeroed` | The block was just allocated with at least `layout.size()` bytes and has not been handed out yet. | `zeroed_is_zero_even_after_reuse` (shared suite) and per-crate zeroed tests. |
| bump/src/lib.rs:200; freelist/src/lib.rs:548; slab/src/lib.rs:352; buddy/src/lib.rs:481 | `Layout::from_size_align_unchecked` in `realloc` | The `GlobalAlloc::realloc` contract guarantees the rounded new size fits in `isize`, and the alignment comes from a valid layout. | Every realloc test; props generate random reallocs. |
| bump/src/lib.rs:205; freelist/src/lib.rs:555; slab/src/lib.rs:362; buddy/src/lib.rs:494 | `copy_nonoverlapping` in a moving `realloc` | Source is live for `layout.size()` bytes; destination is a fresh block, disjoint from it. Freelist and buddy only move when growing. | `realloc_keeps_contents` (shared), `realloc_*` per crate; props check every byte after each realloc. |
| freelist/src/raw.rs:46-84 | `node`, `set_node`, `header`, `set_header`: typed reads and writes | Chunks are 16-aligned, at least 16 bytes and inside the span (docs/freelist.md, invariants 1 to 3), so a 16-byte struct at their start is aligned and in bounds. Writes only hit free chunks or headers, never payloads. | All freelist tests under Miri; `debug-checks` verify the invariants they rely on. |
| freelist/src/lib.rs:125, 141, 187, 225, 352 | Walking the free list (`free_bytes`, `largest_free`, `verify`, `take`, `neighbors`) | Every address reached through `head`/`next` is a free chunk (invariant 3). | `random_workload_returns_to_one_chunk`, props, debug-checks. |
| freelist/src/lib.rs:283, 292, 303 | `carve`: writing leftover nodes and the new header | Front gap, block and tail partition the old free chunk exactly; all three boundaries are 16-aligned. | `first_fit_splits_and_respects_alignment`, `front_padding_is_not_lost`, `large_alignments`. |
| freelist/src/lib.rs:316, 319 | `relink`: rewriting the previous node | `prev` is on the list. | Every allocation that is not at the head. |
| freelist/src/lib.rs:329, 333 | `unsafe fn release`; reading the header | Contract: the payload is live, so its header sits 16 bytes before it. A tag check aborts on a bad pointer when checks are on. | All dealloc paths. |
| freelist/src/lib.rs:368, 374, 382, 389 | `insert`: reading neighbors, writing the merged node | The freed chunk and any neighbor it merges with are all free memory. | `freeing_everything_coalesces_back_to_one_chunk`, `fragmentation_can_fail_with_enough_free_bytes`. |
| freelist/src/lib.rs:400, 403, 415, 430, 442, 448, 462, 465 | `resize` and `set_size`: in-place realloc | The block owns `hdr..end`. Shrinking gives back a 16-aligned suffix. Growing first unlinks the free chunk right after it. | `realloc_grows_into_free_neighbor_and_shrinks_in_place`, props. |
| freelist/src/lib.rs:481 | `prepare`: writing the first node | Nothing has been handed out yet; the span is inside the region. | Every freelist test, first operation. |
| freelist/src/lib.rs:512, 522, 543 | `unsafe impl GlobalAlloc for FreeList` and its calls into `release`/`resize` | See the SAFETY comment above the impl; the calls forward the trait's caller contract. | freelist/tests, shared suite, props, `global_freelist`. |
| slab/src/raw.rs:28-43 | `meta`, `set_meta` | Table entries are 8-aligned (table starts on a slab boundary, entries are 48 bytes) and inside the region, past every slab. | All slab tests under Miri. |
| slab/src/lib.rs:139, 192, 217, 241, 252, 267, 280, 302 | Reading and writing table entries (`free_bytes`, `verify`, `take`, `give`, `carve`) | Only entries of carved slabs are read (invariants 1 and 3); `carve` writes an entry before the slab is used. | `classes_get_their_own_slabs`, `full_slab_goes_back_on_the_list`, `exhaustion_is_per_class`, debug-checks. |
| slab/src/lib.rs:262, 331 | `unsafe fn give` and its call from `dealloc` | Contract: a live block of that class, which `GlobalAlloc` guarantees through the layout. Double frees abort when checks are on. | All dealloc paths; the double-free check fired in a manual run. |
| slab/src/lib.rs:314 | `unsafe impl GlobalAlloc for Slab` | See the SAFETY comment; the allocator never writes to block memory. | slab/tests, shared suite, props, `global_slab` (including a spawned thread, the case that used to be UB). |
| buddy/src/raw.rs:16-39 | `word`, `set_word`, `clear` on the bitmap | The bitmap lies past the block area, 16-aligned, sized by `bitmap_bytes`; each order's bits start on a 64-bit boundary. | `area_is_the_largest_that_leaves_room_for_the_bitmap`, all buddy tests under Miri. |
| buddy/src/lib.rs:264, 323, 340, 366, 422 | Bitmap access in `verify`, `lowest_free`, `mark`, `is_free`, `prepare` | Indices come from `Geometry::bit` for blocks inside the area, or from scanning an order's own range. | `scan_hint_finds_blocks_freed_below_it`, `freeing_everything_merges_back_to_the_roots`, debug-checks. |
| buddy/src/lib.rs:446 | `unsafe impl GlobalAlloc for Buddy` | See the SAFETY comment; the allocator never writes to block memory. | buddy/tests, shared suite, props, `global_buddy` (libtest harness, threads), under Stacked and Tree Borrows. |
| bench/src/switch.rs:28-38 | `Arena::region` for the six benchmark allocators | One call per arena. | Every bench run. |
| bench/src/switch.rs:163-181 | `unsafe impl GlobalAlloc for Switch` | Forwards unchanged; pointers go back to the allocator whose region contains them, and regions are disjoint statics. | Every bench run; not under Miri (the six 16 MiB arenas make it too slow). |
| bench/src/switch.rs:236, 239; bench/src/scenarios.rs:74, 81 | `reset` between runs and frames | Scenarios free every block before a run or frame ends, and their bookkeeping lists belong to the system allocator. | `game-frame` and every bump run. |
| bench/src/scenarios.rs:148, 158, 281 | Raw `alloc`/`dealloc` in scenarios | Non-zero layouts; each pointer is freed once with the layout it was allocated with. | Every bench run. |
| shared-tests/src/lib.rs:163-170 | Test buffer and `Region::new` in `with` | The buffer outlives the allocator, which is dropped before it is freed. | Every shared test, under Miri. |
| shared-tests/src/lib.rs:285, 295 | `fill`, `holds` | Contract: a live block of at least that many bytes. | Props and the shared suite. |

Test code (`#[cfg(test)]` modules and `tests/`) uses `unsafe` the same way
throughout: allocating and freeing through `GlobalAlloc`, and reading and
writing the blocks it got back. Each block has a SAFETY comment. Miri is the
check that those comments are right.

## What Miri found

Two real findings so far, both described in the design docs:

1. `SpinLock::try_lock` used `compare_exchange_weak`, which may fail
   spuriously. Miri does simulate that, and `try_lock` reported a free lock
   as taken. Not memory unsafety, but a broken contract, caught before the
   first commit of the lock.
2. The first buddy and slab designs wrote free-list links into freed blocks.
   With either allocator installed as `#[global_allocator]`, std frees a
   `Box` that is still a protected function argument, and the link write
   was reported as undefined behavior under both Stacked Borrows and Tree
   Borrows. Fixed by keeping all their state outside block memory.
