# Changelog

All notable changes to mgi-pulse will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-06-01

First public release. Single-source and merged NDJSON navigation with
severity tabs, regex / field / severity filters, detail pane and a static
timeline histogram.

### Added

- **Indexer.** mmap + memchr line splitter with a serde-borrow parse of
  `ts` and `level` only — full payloads are never materialised in the hot
  path.
- **Sources.** `FileProducer` (mmap'd files) and `StreamProducer` (stdin /
  pipes via `BufRead`). CLI accepts files, `-` for stdin, or several files
  for merge.
- **k-way merge.** Multiple NDJSON files merged by `ts_micros` into a
  single time-sorted stream; `line_id` becomes the global merged order.
- **Schema inference.** First 10k records scanned for field presence and
  cardinality; auto-columns derived for the table view, with a raw fallback
  for schema-poor inputs.
- **Filters.** Regex (`/`), field-equals (`f field=value`), severity
  (`1`-`4` quick keys, `0` to clear). Strict vs. min-mode toggle (`m`)
  changes whether `Warn` means exactly Warn or Warn-and-above. All three
  axes compose with AND.
- **DetailPane.** Pretty-printed JSON of the record under the cursor;
  toggled with `d`. (`Tab` is reserved for tab switching, not detail.)
- **Tabs.** Five at startup — `All`, `Error+`, `Warn`, `Info`, `Debug`.
  `Ctrl-T` opens a new `All` tab; `Ctrl-W` closes the active one. Each
  tab has its own filters, cursor and scroll position.
- **Timeline pane.** Overview histogram across the full time range,
  severity-coloured stacked bars. Static (no scrub yet).
- **Status bar.** Surfaces parse errors, untimed-record counts and the
  active input prompt.
- **Tests.** 44 unit tests across the indexer, parser, predicates,
  schema, merge and table panes.

### Performance

Measured on an i5-12400F, 48 GB RAM, ext4 (see
[`bench/parse-bench/BENCH.md`](bench/parse-bench/BENCH.md)).

- Raw scan (memchr only): ~12 GB/s, 65.6 M lines/s.
- Serde-borrow parse (`ts` + `level`): ~905 MB/s, 4.88 M lines/s.
- End-to-end index of a synthetic 2 GB / 11 M-record NDJSON file: ~2.5-2.9 s
  cold-cached on the dev box.
- Release binary: ~3 MB stripped, no dynamic deps beyond libc (musl planned).

### Known limitations

- No native live-follow / inotify — use `tail -F file | mgi-pulse -` for now;
  planned for a later 0.x.
- No timeline scrub or zoom — overview only in v0.1; planned for 0.2.
- NDJSON-only structured parsing — logfmt and regex-extracted plain text are
  on the 0.2+ backlog.
- No multi-line stack-trace folding (Go / Rust / Java) — backlog.
- No themes — severity colours are fixed.
- Pre-built binaries: Linux musl only at release time. macOS and Windows are
  CI-checked but not shipped.

[0.1.0]: https://github.com/madgodinc/mgi-pulse/releases/tag/v0.1.0
