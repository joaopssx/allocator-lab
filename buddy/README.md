# buddy

Binary buddy allocator over a fixed region. Blocks are powers of two from 16
bytes, split on allocation and merged with their buddy on free, and the buddy
of a block is one XOR away. All state lives in a bitmap at the end of the
region (about 1/64 of it), so the allocator never reads or writes the memory
it hands out.

```rust
use alloc_core::Arena;
use buddy::Buddy;

static ARENA: Arena<{ 1 << 20 }> = Arena::new();

#[global_allocator]
// SAFETY: the only region ever taken from ARENA.
static HEAP: Buddy = Buddy::new(unsafe { ARENA.region() });
```

Alignments up to 4096 are supported. Above that, allocation returns null.
`buddy::bitmap_bytes(space)` gives the extra bytes a region needs for a block
area of `space` bytes.

Features: `stats` enables `Buddy::stats()`.

Design, the bit math and invariants: [docs/buddy.md](../docs/buddy.md).
