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

Features: `stats` enables `Bump::stats()`. `debug-checks` verifies the invariants
from the design doc after every operation and aborts with a message if one
breaks. It is slow and meant for tests.

Design and invariants: [docs/bump.md](../docs/bump.md).
