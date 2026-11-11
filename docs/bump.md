# Bump allocator

The simplest strategy in the lab and the baseline the others get compared to.
The allocator keeps one number, the offset of the first unused byte, and every
allocation rounds that offset up to the requested alignment and moves it past
the new block.

```
 region.start                                              region.end
 |                                                                 |
 [ A ][pad][   B   ][ C ][pad][      D      ]                      ]
                                            ^
                                            next
                                ^
                                last (start of D)
```

There is no per-block header and no free list, so allocation is a handful of
integer operations under the lock. The price is that `dealloc` does nothing.
Memory only comes back through `reset`, which sets `next` to zero and makes the
whole region available again.

## Invariants

These hold whenever the lock is not held:

1. `next <= region.size()`.
2. Every live block lies in `[region.start, region.start + next)`.
3. Live blocks do not overlap. Blocks are handed out strictly in increasing
   address order and `next` only grows, except in two cases below.
4. If `last` is `Some(off)`, `off` is the start of the most recently handed
   out block and `off < next`. Every block takes at least one byte, so the
   inequality is strict.
5. `next` decreases in only two places: `reset`, whose safety contract says
   every block is dead, and shrinking the block at `last`, which only gives
   back bytes past the new end of that same block.

Invariant 3 is what the `GlobalAlloc` safety argument rests on. The property
tests in `shared-tests` check it directly by recording every live block and
looking for overlaps after each operation.

## Decisions

**Zero-sized requests take one byte.** `GlobalAlloc` callers never pass a zero
size, but the safe `try_alloc` can be called with one. I could have returned a
dangling aligned pointer without touching `next`, but then `dealloc` and
`realloc` would need to recognize it. Rounding up to one byte keeps every
pointer unique and inside the region, and the four allocators all do the same,
so the shared tests can treat them uniformly.

**`realloc` only grows the last block in place.** That is the case a growing
`Vec` hits when nothing else is allocated in between, and it costs a single
comparison. Growing anything else means alloc, copy and leaving the old block
behind, because there is no way to free it. Shrinking is always in place; when
the block is the last one the tail goes back to the allocator.

**`reset` is `unsafe` and takes `&self`.** A version taking `&mut self` would be
safe to call, but it would be useless for the main use case: an allocator in a
`static`, reset once per frame of a game loop. The contract is simple to state
(no block from before the reset is touched again) and impossible to check, so
it stays on the caller.

**Memory is not zeroed on reset.** `alloc_zeroed` always clears the block
instead. Tracking a high-water mark would let fresh memory skip the
`write_bytes`, but the region is not guaranteed to start zeroed anyway, and
the extra state did not seem worth it.

## When it fits

Phase-based workloads where everything dies together: per-frame scratch data,
parsing a request, building a tree that is thrown away whole. Anything with
long-lived and short-lived objects mixed together will run out of memory,
since nothing is ever reclaimed.
