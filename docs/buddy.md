# Buddy allocator

Every block is a power of two in size, from 16 bytes up, and sits at an
offset (from the base of the block area) that is a multiple of its own size.
Order `k` means `16 << k` bytes. A block of order `k + 1` splits into two
blocks of order `k`, and those two are each other's buddy.

## The bit trick

Take a block of order `k` at offset `off`. Because `off` is a multiple of
`16 << k`, the bits of `off` below position `4 + k` are all zero. The parent
block, of order `k + 1`, has the lower half at an offset whose bit `4 + k` is
0 and the upper half at an offset where it is 1. Everything else is the same.
So:

```
buddy(off, k)    = off ^ (16 << k)
is_upper(off, k) = off & (16 << k) != 0
parent(off, k)   = off & !(16 << k)      // = min(off, buddy)
```

```
order 2 (64 B)  |               0                |
order 1 (32 B)  |       0       |      32        |
order 0 (16 B)  |   0   |  16   |  32   |  48    |

buddy(16, 0) = 16 ^ 16 = 0      buddy(32, 1) = 32 ^ 32 = 0
buddy(48, 0) = 48 ^ 16 = 32     buddy(0, 1)  = 0 ^ 32  = 32
```

This only works on offsets, not addresses, unless the base is aligned to the
largest block. The code keeps `base` separate and XORs `addr - base`.

## Layout

```
 region
 +--+----------------------------------------------------+--------+--+
 |  |                 block area (space)                 | bitmap |  |
 +--+----------------------------------------------------+--------+--+
  ^  ^                                                    ^
  |  base, aligned to 4096                                map
  lost to alignment
```

The base is aligned to 4096 (`MAX_ALIGN`). Every block of order 8 or more is
aligned to 4096, and smaller blocks are aligned to their own size. An
allocation aligned to `a` therefore just needs a block of at least `a` bytes.
Alignments above 4096 are not supported, and those requests fail with
`AllocError::Unsupported`.

The area does not have to be a power of two. When the allocator starts, it
tiles the area with the largest blocks that fit, in decreasing size. For a
12096-byte area that gives roots of 8192, 2048, 1024, 512, 256 and 64. Each
root is aligned to its own size because the offsets before it are sums of
larger powers of two. A root's buddy at the root's own order is never a real
free block: either it lies past the end of the area, or it is the start of a
smaller root. So merging can never join two roots.

## The bitmap

Merging needs to answer one question: is the buddy free, as a block of this
exact order? I first planned to answer it from the free block itself, with an
order and a magic number in the node. That would mean deciding "is this free?"
by reading memory that might belong to a live block, and a user could store
the magic number there. It is unlikely, but a correct allocator cannot rely on
"unlikely".

So there is a bitmap: one bit per possible block per order, set while that
block is on its free list. Order `k` needs `space >> (4 + k)` bits. Summed over
all orders that is under `space / 8` bits, i.e. under `space / 64` bytes, so
the allocator reserves 1/65 of the region for the bitmap and the rest is block
area. The bitmap is cleared once in `prepare`, and after that it is only
touched by `push` and `remove`, which keep it in sync with the lists.

The free lists are doubly linked, so the buddy found through the bitmap can be
unlinked in O(1). The node (`next`, `prev`) is 16 bytes, which is why the
smallest block is 16 bytes.

## Operations

**alloc**: find the smallest order `>= want` with a non-empty list, pop a
block, and split it down: keep the lower half, push the upper half one order
lower, repeat. Small requests split the smallest root that fits first, so the
large blocks stay intact.

**dealloc**: the order comes back from the `Layout`, with no header. Then loop:
if the buddy at this order is free, unlink it, keep the lower of the two
addresses, go up one order. Push whatever remains.

**realloc**: shrinking is always in place, and the upper halves go back on the
lists. Growing is in place when the block is the lower half at every level up
to the target order and every buddy on the way is free. Otherwise it moves:
take a new block, copy, free the old one with merging.

## Invariants

When the lock is not held:

1. Every block on list `k` lies in the block area, at an offset that is a
   multiple of `16 << k`.
2. A block is on list `k` if and only if its bit for order `k` is set.
3. Blocks on the lists are pairwise disjoint and disjoint from every live
   block.
4. No two buddies of the same order are both on list `k`. If they were, they
   would have been merged.

The `debug-checks` feature walks every list and checks 1, 2 (in the "on list
implies bit set" direction, and by counting set bits against list lengths) and
4.

## Costs

Internal fragmentation is the price: everything rounds to a power of two, so
a 1025-byte request takes 2048 bytes. External fragmentation stays limited
because merging is eager and exact. The bitmap costs about 1.5% of the region,
and the alignment of the base can cost up to 4095 bytes at the start.
