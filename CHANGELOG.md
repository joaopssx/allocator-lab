# Changelog

## 0.1.0 (unreleased)

First version.

- `alloc-core`: `Region`, `Arena`, `SpinLock`, alignment helpers,
  `AllocError`, `Stats` (feature `stats`) and `broken` for aborting from
  inside an allocator.
- `bump`: bump allocator with `reset` and in-place realloc of the last block.
- `freelist`: address-ordered free list with coalescing, first, best and
  next fit, and in-place realloc.
- `slab`: eight power-of-two classes from 16 to 2048 bytes, per-slab free
  bitmaps in a table at the end of the region.
- `buddy`: binary buddy allocator with all state in a bitmap.
- `debug-checks` feature on every allocator.
- Shared test suite, property tests, global allocator tests, all passing
  under Miri.
- Benchmark harness with nine scenarios, Markdown report and SVG charts.

Fixed during development, before any release:

- `SpinLock::try_lock` could fail spuriously (it used a weak exchange).
- Rustdoc failed on `alloc-core` because of an intra-doc link to a raw
  pointer method. The commit that fixed it says "only builds on nightly",
  which is not accurate: that kind of link is experimental everywhere.
- `buddy` and `slab` wrote free-list links into freed blocks, which Miri
  reports as undefined behavior when they are the global allocator. Both
  were redesigned to keep their state outside block memory.
