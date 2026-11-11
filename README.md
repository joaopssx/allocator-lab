# allocator-lab

Four classic memory allocation strategies written from scratch in Rust: bump,
slab, buddy and free-list. Each one implements `GlobalAlloc` over a fixed
memory region, so it can be dropped in as `#[global_allocator]` and still run
deterministically under Miri.

I started this to understand what actually happens between `Box::new` and the
bytes it hands back, and to practice writing `unsafe` code that I can defend
line by line. The plan is to compare the strategies with real benchmarks
against the system allocator and to keep every `unsafe` block documented and
tested.

Work in progress.

## License

MIT, see [LICENSE](LICENSE).
