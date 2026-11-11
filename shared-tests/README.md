# shared-tests

Tests that run against all four allocators: a behavioral suite, proptest
properties, and one test binary per allocator installed as
`#[global_allocator]`.

The library part is the harness: the `Lab` trait, `with()` to get a fresh
allocator over a page-aligned buffer, and the checks for overlap,
alignment, bounds and accounting.

    cargo test -p shared-tests
    cargo test -p shared-tests --features debug-checks

Profiles, Miri and what each layer checks: [docs/TESTING.md](../docs/TESTING.md).
