# Slab allocator

Eight size classes, the powers of two from 16 to 2048 bytes. A request goes to
the smallest class that covers both its size and its alignment, so a 24-byte
request takes a 32-byte block and an 8-byte request aligned to 64 takes a
64-byte block. Anything over 2048 bytes, in size or in alignment, gets a null
pointer. That limit is deliberate: in this design large objects belong to a
different allocator, and the slab only ever sees small ones.

## Slabs and blocks

The region is cut into 4096-byte slabs, aligned to 4096. Slabs are taken in
address order, the first time a class needs one, and a slab then belongs to
that class forever.

```
 region (slab-aligned span)
 +----------------+----------------+----------------+----------------+
 | slab 0: 16 B   | slab 1: 2048 B | slab 2: 16 B   |   uncarved     |
 |[][][][][][]... |[      ][      ]|[][][][]        |                |
 +----------------+----------------+----------------+----------------+
                                            ^
                                            fresh cursor of class 16
```

Each class keeps three things:

- `free`: a LIFO list of freed blocks, linked through the first word of each
  block.
- `fresh` and `end`: a cursor into the class's newest slab. Blocks past the
  cursor have never been handed out.

Allocation pops from `free` first, then advances `fresh`, and only when both
are empty does it carve a new slab. Carving does not touch the slab's memory.
Blocks are not pushed onto the free list one by one when the slab is created,
so a fresh slab costs nothing until it is used.

Blocks sit at multiples of the class size inside a 4096-aligned slab, so every
block is aligned to its class size. Alignment comes for free from the class
choice, with no padding computation on the fast path.

## No headers

A used block has no metadata. `dealloc` and `realloc` get the `Layout` back
from the caller (`GlobalAlloc` requires it to be the same one used to
allocate), and `class::of(layout)` recomputes the class. This is what makes a
16-byte allocation actually cost 16 bytes, while the free-list allocator
spends 32 on it.

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

Giving fully free slabs back to the region would fix it, but that needs a
count of live blocks per slab, which means either a per-slab header (and
blocks no longer aligned to the start of the slab) or a side table sized by
the region. I left it out. The workloads where a slab allocator shines
(many objects of a few fixed sizes) rarely hit this case.

## Invariants

When the lock is not held:

1. `carved * 4096 <= capacity()`, and slab `i` starts at
   `span.start + i * 4096`.
2. Every block on the free list of class `c` is inside a slab carved for `c`,
   at an offset from the slab start that is a multiple of `size(c)`.
3. For every class, `fresh <= end`, and either `fresh == end` or `fresh` is
   inside the class's newest slab, at a multiple of `size(c)`.
4. A block is in exactly one of three states: on its class's free list, past
   its class's `fresh` cursor, or live. No free list contains a block twice.

Invariant 4 is the one the `GlobalAlloc` safety comment rests on. A double
free breaks it immediately, because it puts the same block on the list twice
and the next two allocations return the same pointer. The `debug-checks`
feature walks each list and checks 2 and the "no duplicates" part of 4.
