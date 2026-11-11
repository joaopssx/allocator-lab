# alloc-core

The pieces every allocator in the workspace shares. Nothing here allocates.

- `Region`: exclusive ownership of a range of bytes. Its constructor is the
  one place where a caller promises the memory is valid and unshared.
- `Arena<N>`: a 4096-aligned static buffer that hands out its `Region` once.
- `SpinLock`: test-and-test-and-set lock on an `AtomicBool`. Unfair and not
  reentrant, on purpose; see the module docs.
- `align`: `align_up`, `align_down`, `is_aligned`, with overflow reported as
  `None`.
- `AllocError`, and `broken()` to abort from inside a `GlobalAlloc` method
  that must not unwind.
- `Stats`, behind the `stats` feature.

The directory is `core/`, but the package is called `alloc-core`, so it
does not shadow the language's own `core` crate.
