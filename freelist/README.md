# freelist

General-purpose allocator over a fixed region. Free chunks live in an
address-ordered linked list stored inside the free memory, and adjacent free
chunks merge on `dealloc`. Placement is first fit, best fit or next fit,
chosen at construction.

```rust
use alloc_core::Arena;
use freelist::{Fit, FreeList};

static ARENA: Arena<{ 1 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: FreeList = FreeList::new(unsafe { ARENA.region() }, Fit::First);
```

Every block costs a 16-byte header, and sizes round up to 16 bytes. Freeing is
O(free chunks), because of the walk to the insertion point.

Features: `stats` enables `FreeList::stats()`.

Design and invariants: [docs/freelist.md](../docs/freelist.md).
