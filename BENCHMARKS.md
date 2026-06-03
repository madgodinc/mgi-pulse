# Benchmarks

Reference performance numbers. The intent is to spot regressions
between releases, not to compete with specialised processors.

## Headline

- **End-to-end index of a 2 GB / 11 M-record synthetic NDJSON: ~2.8 s**
  on an i5-12400F (6c/12t, ext4, NVMe). Throughput ~ 700 MB/s,
  4 M records/s.
- **First-byte to first-paint: < 100 ms** on the same workload — the
  schema warmup runs on the first 10k lines only, the rest of the
  index streams in afterwards (this number is informal; needs a
  real measurement harness).

These are personal-dev-box numbers, not a server benchmark. Real
loads on real boxes are expected to vary by maybe 2× either way.

## Reproducing

```sh
# Generate the synthetic fixture (one-time, ~1 minute, ~2 GB output).
./bench/gen-ndjson.sh /tmp/bench.ndjson 11000000

# Run the bench.
cargo run --release -p parse-bench --bin indexer-bench -- /tmp/bench.ndjson
```

Bursty variant (the README hero shots use this) — same shape, time-
varying severity distribution so the timeline has visible structure:

```sh
./bench/gen-ndjson-bursty.sh /tmp/bench-bursty.ndjson
```

## Parser hot-path numbers (M1 measurement)

From `parse-bench` (not the indexer bench above) — the cost of
extracting `ts` and `level` only, from the same 2 GB file:

| Strategy | Throughput | Notes |
|---|---|---|
| `memchr` line splitter | ~ 12 GB/s | floor; the rest must beat this |
| serde-borrow with `&RawValue` ts + `&str` level | ~ 905 MB/s | shipped default |
| `simd-json` mutable parse | ~ 1.1 GB/s | considered, not adopted — wins ~20% but adds a build dep |

Cost of full parser dispatch per format (`LogFormat::parse_ts_level`)
was measured at < 1 % in the indexer total — the match arm doesn't
show up in profiles.

## What's NOT measured

- **First-paint latency under follow load.** The follow worker
  batches records into the channel; the UI drains up to 4096 per
  tick. We don't have a measurement of perceived latency at
  10k-events/s sustained writes.
- **Memory footprint at 10 GB+.** Index size scales with record
  count (line/time/severity arrays). For 100 M records the index
  is roughly 1.5 GB in RAM; the engine doesn't evict.
- **Cold cache.** All numbers are second-run with the page cache
  warm. First run on a freshly-mounted disk is dominated by I/O,
  not parse.

## CI regression guard (not yet wired)

The plan is to gate releases on a `cargo run -p parse-bench --bin
indexer-bench` run that asserts < 3.5 s for the 2 GB fixture
(margin over the ~2.8 s baseline). Not enforced today; flagged in
the v1.0 roadmap.
