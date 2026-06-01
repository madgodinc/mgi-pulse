# parse-bench — M1 parse-cost measurement

This is the M1 measurement step from the mgi-pulse plan. The indexer hot path
only needs two fields per NDJSON record: `ts` and `level`. The plan picked
`serde_json::from_slice` with a borrow-struct over `&str` as the primary
strategy, with simd-json/jiter held in reserve "if profiling says so". This
bench is what makes profiling say.

## Reproduce

```sh
bench/gen-ndjson.sh 11000000 > synth-2gb.ndjson      # one-time, ~32s
cargo build --release -p parse-bench
./target/release/parse-bench synth-2gb.ndjson
```

The generator emits ~194 B/line of Aurora-shaped logs (ts, level, logger, msg,
request_id, payload). 11M lines ≈ 2 GB on disk. Each strategy is run twice;
the first pass warms the OS page cache, the second is reported.

## Results — 2026-06-01, i5-12400F, 48 GB RAM, /dev/sda1 ext4

```
file = synth-2gb.ndjson (2040 MB)
          raw-scan: 11000000 lines  2040 MB in 0.17s =>  12178 MB/s   65.6M lines/s
      serde-borrow: 11000000 lines  2040 MB in 2.26s =>    905 MB/s    4.88M lines/s
         simd-json: 11000000 lines  2040 MB in 10.97s =>   186 MB/s    1.00M lines/s
```

Sanity checks pass: ts/level checksum and severity tally are identical between
serde-borrow and simd-json. Both parsers saw the same bytes; the time gap is
real.

## What this tells us

- **Serde borrow wins by 5×.** `to_borrowed_value` in simd-json parses the
  full object (including nested `payload`), while we only want two top-level
  string fields. To beat serde, simd-json would have to be driven through its
  low-level tape API. That's backlog territory.
- **Parsing is not the bottleneck.** 2 GB indexed in 2.26 s on one core, with
  zero allocations beyond the parser's internal state.
- **M1.b regression target.** 2 GB / 2.26 s on serde-borrow is the floor.
  Total indexer time on 2 GB should land in the 3-4 s range once line-index +
  time-index + severity-index writes are added. If a future change drags this
  past ~6 s we want to know.

## Floor / ceiling sanity

- raw-scan (memchr newline split only) is 12 GB/s — well above any disk we'll
  reasonably encounter, so the file is fully cached after the warmup pass.
  The reported timings are CPU-bound, not IO-bound. That is what we want to
  measure here.

## Future passes

Re-run when:

- A new parser candidate appears (jiter, sonic-rs).
- The indexer is real and we want to measure parse + index combined.
- Real Aurora logs are added as a second fixture — synthetic is uniform; real
  logs have variance in field cardinality that may matter.

Do NOT re-run after every refactor. This is a baseline, not a CI gate.
