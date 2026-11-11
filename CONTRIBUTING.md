# Contributing

This is a personal study project, but issues and pull requests are welcome,
especially ones that find a soundness problem.

## Before opening a pull request

These are the same checks CI runs:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test --workspace --exclude bench
```

If you change an allocator's internals, also run the full property profile
with the invariant checks on:

```
LAB_FULL=1 cargo test --release -p shared-tests --features debug-checks --test props
```

## Unsafe code

- Every `unsafe` block needs a `SAFETY:` comment that names the invariant
  it relies on, not one that restates what the code does. Clippy enforces
  that the comment exists. Whether it says something true is up to review.
- Every `unsafe fn` needs a `# Safety` section with its contract.
- Pointer dereferences belong in the allocator's `raw` module.
- If a change adds or moves an `unsafe` site, update
  [docs/UNSAFE_AUDIT.md](docs/UNSAFE_AUDIT.md).
- If it changes what an allocator guarantees, update the invariants in its
  design doc and the `verify` function behind `debug-checks`, together.

## Commits

Conventional Commits, imperative mood, at most 72 characters in the subject:
`fix(slab): ...`, `test(buddy): ...`, `docs: ...`.
