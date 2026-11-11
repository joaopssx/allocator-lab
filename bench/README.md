# bench

Benchmark harness: nine workloads, run against every allocator and against
the system allocator, each pair in its own process.

    cargo run --release -p bench             # print the tables
    cargo run --release -p bench -- --write  # also update docs/

How it works and what the numbers mean: [docs/BENCHMARKS.md](../docs/BENCHMARKS.md).
