# buddy

Binary buddy allocator over a fixed region. Blocks are powers of two from 16
bytes, split on allocation and merged with their buddy on free. The buddy of
a block is one XOR away. A bitmap at the end of the region (about 1/65 of it)
tracks which blocks are free, so merging never trusts the contents of a block.

```rust
use alloc_core::Arena;
use buddy::Buddy;

static ARENA: Arena<{ 1 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Buddy = Buddy::new(unsafe { ARENA.region() });
```

Alignments up to 4096 are supported. Above that, allocation returns null.

Features: `stats` enables `Buddy::stats()`.

Design, the bit math and invariants: [docs/buddy.md](../docs/buddy.md).
