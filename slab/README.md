# slab

Slab allocator over a fixed region, with size classes 16, 32, ..., 2048 bytes.
The region is split into 4096-byte slabs, each owned by one class. A table at
the end of the region holds a bitmap of free blocks for every slab, so blocks
carry no header and the allocator never writes to block memory.

Requests larger than 2048 bytes, or aligned to more than 2048, return null.
That is part of the contract, not an out-of-memory condition.

```rust
use alloc_core::Arena;
use slab::Slab;

static ARENA: Arena<{ 1 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Slab = Slab::new(unsafe { ARENA.region() });
```

As a `#[global_allocator]`, this only works for programs that never allocate
more than 2048 bytes at once. The libtest harness is not one of them. It is
more useful as an explicit allocator for small objects. `slab::region_for(n)`
gives the region size for exactly `n` slabs.

Features: `stats` enables `Slab::stats()`.

Design, fragmentation trade-offs and invariants: [docs/slab.md](../docs/slab.md).
