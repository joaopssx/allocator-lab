# bump

Bump allocator over a fixed region. Allocation moves a cursor forward,
`dealloc` is a no-op and `reset` releases everything at once. `realloc` grows
the most recent block in place.

```rust
use alloc_core::Arena;
use bump::Bump;

static ARENA: Arena<{ 1 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Bump = Bump::new(unsafe { ARENA.region() });
```

Features: `stats` enables `Bump::stats()`.

Design and invariants: [docs/bump.md](../docs/bump.md).
