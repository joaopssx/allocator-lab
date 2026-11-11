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
12096-byte area that gives roots of 8192, 2048, 1024, 512, 256 and 64, one
per set bit of `12096 / 16`. Each root is aligned to its own size because the
offsets before it are sums of larger powers of two. A root's buddy at the
root's own order is never a real free block: either it lies past the end of
the area, or it is the start of a smaller root. So merging can never join two
roots.

## Only a bitmap

All the bookkeeping is a bitmap after the block area: one bit per possible
block of each order, set while that block is free. Order `k` gets
`space >> (4 + k)` bits, rounded up to a multiple of 64 so that it can be
scanned a whole `u64` at a time. Together that is about `space / 64` bytes
plus a word per order. `Geometry::new` picks the largest area whose bitmap
still fits behind it, and `bitmap_bytes(space)` tells a caller how big a
region needs to be for a given area.

Next to the bitmap, the state keeps a count of free blocks per order and a
scan hint per order: a word index that is never past the lowest set bit of
that order.

This is the second version. The first one kept a doubly linked free list per
order inside the free blocks themselves, with the bitmap only answering "is my
buddy free?". It passed every test, including Miri on allocator instances.
Then the integration tests installed it as `#[global_allocator]`, and Miri
reported undefined behavior under both Stacked Borrows and Tree Borrows. Std
frees a `Box<ThreadInit>` while that box is still a protected argument of the
function freeing it, and `dealloc` wrote the free-list node into those same
bytes through the region's pointer. From the aliasing model's point of view,
that is a foreign write to memory a live `Box` still protects.

The fix was to stop writing into blocks at all. With all the state in the
bitmap, the allocator never touches the memory it hands out, free or live.
`alloc_zeroed` is the only code that writes to a block, and only to a block
it has just allocated.

## Operations

**alloc**: find the smallest order `>= want` with a non-zero free count, scan
that order's bitmap from its hint for the first set bit, clear it, and split
down. Splitting keeps the lower half each time and sets the bit of the upper
half one order lower. Small requests split the smallest root that fits
first, so the large blocks stay intact.

**dealloc**: the order comes back from the `Layout`, with no header. Then loop:
if the buddy's bit at this order is set, clear it, keep the lower of the two
addresses and go up one order. Set the bit of whatever remains.

**realloc**: shrinking is always in place, and the upper halves are marked
free. Growing is in place when the block is the lower half at every level up
to the target order and every buddy on the way is free. Otherwise it moves:
take a new block, copy, free the old one with merging.

## Invariants

When the lock is not held:

1. A set bit for order `k` describes a block that lies in the block area at
   an offset that is a multiple of `16 << k`.
2. `counts[k]` equals the number of set bits in order `k`'s part of the
   bitmap, and `hints[k]` is at or before the word holding the lowest one.
3. Free blocks (set bits, at any order) are pairwise disjoint and disjoint
   from every live block.
4. No two buddies of the same order are both free. If they were, they would
   have been merged.

The `debug-checks` feature verifies 2 by counting bits, and 4 by checking the
buddy of every set bit. It verifies 3 by checking that no set bit has a set
bit in a smaller order inside it.

## Costs

Internal fragmentation is the price: everything rounds to a power of two, so
a 1025-byte request takes 2048 bytes. External fragmentation stays limited
because merging is eager and exact. The bitmap costs about 1.6% of the
region, and the alignment of the base can cost up to 4095 bytes at the start.

Allocation used to pop a list head. Now it scans a bitmap. The hint keeps
the common case short, but a pattern that frees low blocks and allocates high
ones can make a scan walk many empty words. The benchmark report shows what
this costs in practice.
