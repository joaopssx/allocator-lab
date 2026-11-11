# Testing

There are four layers of tests, and all of them run natively and under Miri.

**Per-crate tests** (`*/src/lib.rs` test modules and `*/tests/`) know the
internals of one allocator: which block a request should land in, what the
free list looks like after a merge, how the bitmap is laid out. These tests
are the ones that pin down exact addresses and byte counts.

**The shared suite** (`shared-tests/tests/suite.rs`) runs the same
behavioral checks against all four allocators through a small trait, `Lab`:
mixed sizes stay disjoint, every alignment up to 4096 is honored or refused
cleanly, oversized and near-`isize::MAX` requests return null without
consuming anything, realloc keeps contents, zeroed memory is zero even after
reuse, and four threads never get the same block. The free list runs every
test once per placement policy.

**Property tests** (`shared-tests/tests/props.rs`) generate random sequences
of alloc, free and realloc with proptest. After every step they check that:

- no two live blocks overlap;
- every block is aligned and inside the region;
- free bytes plus what the live blocks cost add up to the capacity (the
  cost is exact per allocator: header plus rounding for the free list, the
  class size for slab, the block size for buddy; bump is skipped since it
  never frees);
- every block still holds the byte pattern last written to it.

The last check is what catches allocator metadata written over a live
block, which the other three would miss.

**Global allocator tests** (`shared-tests/tests/global_*.rs`) install each
allocator as `#[global_allocator]` of a whole test binary and use `Vec`,
`String`, `Box` and `HashMap` normally, so the libtest harness itself runs
on the allocator. The slab binary is the exception: it uses
`harness = false`, because libtest allocates more than 2048 bytes. It still
spawns a thread on purpose, since that is the path that exposed the
original buddy and slab bug (see [buddy.md](buddy.md)).

## Profiles

The default profile is sized for Miri. The whole workspace takes about 80
seconds under it on my machine. Under `cfg!(miri)`, loops and areas shrink
and property tests run 3 cases of fewer than 25 steps, with blocks under 600
bytes. Natively, the property tests run 256 cases of fewer than 200 steps.

The full profile is opt-in with `LAB_FULL=1`: 4096 property cases natively,
64 under Miri. Miri isolates the environment, so the variable has to be
forwarded:

```
LAB_FULL=1 cargo test --release -p shared-tests --test props
LAB_FULL=1 MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-env-forward=LAB_FULL" \
    cargo +nightly miri test -p shared-tests --test props
```

## Miri

```
rustup toolchain install nightly --component miri,rust-src
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test --workspace --exclude bench
```

Strict provenance makes Miri reject any pointer built from a bare integer.
Every allocator derives its pointers from the region's base with
`with_addr`, so this costs nothing and checks that the derivation is never
skipped. CI also runs the global allocator tests under Tree Borrows
(`-Zmiri-tree-borrows`), since those are the tests where the aliasing model
made a difference.

`bench` is excluded: it has no tests, and its six 16 MiB static arenas make
even starting it slow under the interpreter.

## Invariant checks

`--all-features` turns on `debug-checks` in every allocator, and each
operation then ends with a full check of the invariants in its design doc.
The free list walks the whole list, slab walks every class list and its
table, and buddy scans every bit. It is slow, but it turns a corrupted
structure into an immediate abort at the operation that caused it, instead
of a wrong pointer three hundred steps later.

```
cargo test --workspace --all-features
LAB_FULL=1 cargo test --release -p shared-tests --features debug-checks --test props
```

The checks abort instead of panicking, through `alloc_core::broken`, because
a `GlobalAlloc` method must not unwind.
