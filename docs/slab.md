# Slab allocator

Eight size classes, the powers of two from 16 to 2048 bytes. A request goes to
the smallest class that covers both its size and its alignment, so a 24-byte
request takes a 32-byte block and an 8-byte request aligned to 64 takes a
64-byte block. Anything over 2048 bytes, in size or in alignment, gets a null
pointer. That limit is deliberate: in this design large objects belong to a
different allocator, and the slab only ever sees small ones.

## Slabs, blocks and the table

The region is cut into 4096-byte slabs, aligned to 4096. Slabs are taken in
address order, the first time a class needs one, and a slab then belongs to
that class forever. Every slab has a 48-byte entry in a table at the end of
the region:

```
 region
 +-------------+-------------+-------------+-----------+---------+
 | slab 0: 16B | slab 1: 2KB | slab 2: 16B | uncarved  |  table  |
 +-------------+-------------+-------------+-----------+---------+
                                                        | e0 e1 e2 ...
 entry:  free: [u64; 4]   one bit per block, set = free
         nfree            number of set bits
         next             next slab of this class with free blocks
```

A slab has at most 256 blocks (4096 / 16), so four words of bitmap always
suffice. Each class keeps the head of a list of its slabs that still have a
free block, linked through the `next` field of the entries.

Allocation takes the first slab on the class's list, finds the lowest set bit,
clears it, and drops the slab from the list if that was its last free block.
If the list is empty, a new slab is carved, with every bit set. Freeing sets
the bit again and, if the slab was full, pushes it back on the list. The slab
comes from `(addr - start) / 4096`, the block index from the offset inside
the slab, and the class from the `Layout`.

Blocks sit at multiples of the class size inside a 4096-aligned slab, so every
block is aligned to its class size. Alignment comes for free from the class
choice, with no padding computation on the fast path.

## Why the bookkeeping is out of the blocks

The first version kept freed blocks on a LIFO list, linked through the first
word of each free block. It was simpler and every per-instance test passed
under Miri. Running it as `#[global_allocator]` with a spawned thread showed
the problem. Std frees a `Box` while that box is still a protected argument
of the function doing the freeing. `dealloc` then wrote the list link into the
freed bytes through the region's pointer, and Miri reported that as undefined
behavior under both Stacked Borrows and Tree Borrows. The buddy allocator had
the same bug. [buddy.md](buddy.md) has the details.

With the table, the allocator never writes to block memory, free or live.
The cost is 48 bytes per 4096-byte slab (1.2%) and a scan of up to four words
per allocation instead of a pointer pop.

## realloc

If the new size maps to the same class, `realloc` returns the same pointer and
does nothing else. A `Vec<u8>` growing from 17 to 32 bytes, or shrinking from
2000 to 1100, never moves. Otherwise it allocates in the new class, copies and
frees the old block. Growing past 2048 fails and leaves the old block valid.

## Fragmentation

Internal fragmentation is the cost of rounding up. In the worst case, a
request one byte over a power of two wastes almost half its block: 1025 bytes
in a 2048-byte block. On average, with sizes spread uniformly, it is about a
quarter.

External fragmentation shows up in a different form than in the free-list
allocator. Free blocks never need merging, since every block of a class is
interchangeable. The problem is that a slab never changes class. A program
that allocates a million 16-byte objects, frees them all and then asks for
512-byte blocks will find all its memory locked in the 16-byte class:
`free_bytes()` reports it as free, but no 512-byte request can use it. The
test `exhaustion_is_per_class` shows the smallest version of this.

With `nfree` in the table, detecting a completely empty slab is now O(1).
Giving that slab back to the pool would still mean unlinking it from the
middle of its class's list, which needs a doubly linked list or a walk. I left
it out. The workloads where a slab allocator shines (many objects of a few
fixed sizes) rarely hit this case.

## Invariants

When the lock is not held:

1. `carved <= slabs`, slab `i` starts at `start + i * 4096`, and its entry is
   at `table + i * 48`.
2. For every carved slab, `nfree` equals the number of set bits in `free`,
   and no bit is set at or past the slab's block count.
3. A carved slab is on its class's list if and only if `nfree > 0`, it is on
   exactly one list, and lists have no cycles.
4. A block is live if and only if it is in a carved slab and its bit is clear.

Invariant 4 is the one the `GlobalAlloc` safety comment rests on. A double
free would set a bit that is already set. `dealloc` catches that with a debug
assertion, and the `debug-checks` feature checks 1 to 3 after every
operation.
