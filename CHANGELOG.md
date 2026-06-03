# Changelog

All notable changes to mgi-pulse will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-06-03

Big jump from 0.2.0. The intermediate development happened locally
across what would have been several minor releases — they're
reconstructed here as "phases" so the scope of each step is
visible. The version skips to 0.9.0 because the project is now
**feature-complete for v1.0**; what's left is dogfooding and bug
fixes from real-world use, not new functionality.

What this release covers in one sentence: 7 new log formats (now
11 total), native `--follow`, timeline scrub, persistent
bookmarks, a generic regex-extraction fallback for arbitrary
plain-text logs, plus stats overlay, save-to-file, runtime
columns, and the tech-debt foundation (CONTRIBUTING, ADRs,
benchmarks, semver-promise) needed to take v1.0 seriously.

Tests grew from 125 (in 0.2.0) to 244.

### Phase A — Format coverage

The 0.2.0 → 0.9.0 jump multiplies format support: 4 → 11.

- **Syslog RFC 5424.** `--format=syslog`. Full header
  (`<PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID
  STRUCTURED-DATA MSG`). PRI's lower 3 bits map to severity (0-2 →
  fatal, 3 → error, 4 → warn, 5-6 → info, 7 → debug). Structured
  data exposes both bare `SD-ID` membership and `SD-ID.key` value
  projection. Multi-line records fold when continuations lack a
  `<` opener. Closes #5.
- **CSV / TSV.** `--format=csv` / `--format=tsv`. RFC 4180
  quoting. The first row is captured as the column header per
  source; named columns drive `ts` / `level` extraction and
  predicate projection. Falls back to positional `_N` (1-based)
  when no header matches. Closes #9.
- **Apache / nginx access logs.** `--format=access`. Common Log
  Format + the Combined extension with `"referer"` and
  `"user_agent"`. Apache `[DD/MMM/YYYY:HH:MM:SS ±HHMM]` time format
  is normalised to RFC3339 internally. Severity is synthesised from
  HTTP status (5xx → error, 4xx → warn, 2xx/3xx → info). Field
  projection: `ip`/`host`, `user`, `request`, `method`, `uri`,
  `protocol`, `status`, `bytes`, `referer`, `user_agent`. Closes #4.
- **Java logback / log4j2 default.** `--format=logback` (aliases:
  `log4j`, `java`). Parses the canonical Spring Boot / log4j2
  console pattern. Stack-trace continuations (`\tat ...`, `Caused
  by: ...`, `\t... 12 more`) fold via the "first byte not a digit"
  rule.
- **systemd `journalctl -o json`.** `--format=journalctl` (aliases:
  `journal`, `systemd`). NDJSON with `__REALTIME_TIMESTAMP` (micros
  since epoch as a string) for time and `PRIORITY` (syslog 0-7)
  for severity. Field aliases: `msg` → `MESSAGE`, `host` →
  `_HOSTNAME`, `unit` → `_SYSTEMD_UNIT`, `ident` → `SYSLOG_IDENTIFIER`.
- **Generic regex extraction.** `--pattern='regex with named
  captures'` (implies `--format=regex`). The escape hatch for
  anything the canonical parsers don't cover — ML runtime logs,
  custom log4j patterns, plain-text scripts. Named captures `ts`,
  `level`, and any other group become projectable fields. `ts`
  parses as RFC3339 with prefix padding; `level` maps to a
  severity name.
- **JSON-array input adapter.** Files whose first non-whitespace
  bytes look like `[{` are loaded into memory, flattened to NDJSON,
  and indexed through the normal stream path. Caps at 256 MB;
  bigger arrays should pipe through `jq -c '.[]'`. Auto-detected.
- **Format auto-detect wired into the CLI.** Running `mgi-pulse
  foo.log` without `--format` reads a ~16 KiB probe (up to 64
  lines) from the first file, feeds it to `LogFormat::detect`, and
  uses the verdict. Probe precedence: syslog > access > logback >
  journalctl > NDJSON > EDN > logfmt > TSV > CSV.
- **`looks_like_access` tightened.** Now requires Apache-date shape
  (`DD/MMM/YYYY:HH:MM:SS` — slashes plus colons) inside the
  brackets, eliminating false positives on logback's
  `[thread-name]`.

### Phase B — UI / UX

- **Native `--follow` mode.** `mgi-pulse --follow app.log`
  backfills the existing file synchronously, then hands off to a
  background worker that owns a `TailReader` seeked to EOF. The
  worker streams new records through a bounded crossbeam channel;
  the UI loop drains the channel on every tick and ingests records
  via the new `Engine::ingest_one`. Filters re-evaluate against the
  growing index. Inode-based rotation survives `logrotate`. Closes
  #6.
- **Timeline scrub and zoom.** `<` / `>` move a scrub cursor across
  the histogram bins (Shift jumps 10), `+` / `-` zoom the visible
  range (halve / double, anchored on the cursor), `Enter` applies
  the selection as a time-range filter. `Esc` cancels the scrub on
  first press; a second `Esc` clears all filters. Closes #3.
- **DSL boolean composition: `OR`, `NOT`, parentheses.**
  Recursive-descent parser, conventional precedence (`NOT` >
  `AND` > `OR`, parens override). Keywords are uppercase ASCII so
  they never collide with field names like `and_count`. Closes #8.
- **Persistent bookmarks across sessions** for single-file sources.
  Sidecar at `$XDG_DATA_HOME/mgi-pulse/bookmarks.json` keyed by
  inode + size; a rotated or truncated file drops its saved
  bookmarks. Flush-on-quit. Capped at 256 sources with LRU
  eviction. Closes #7.
- **Save filtered view to a file** with `s`. Opens a `save:`
  prompt; Enter writes the current view's records to the given
  path (one record per line), Esc cancels.
- **Stats overlay with `?`.** Sidebar summarising the current
  filtered view: total records, per-severity counts, untimed
  bucket, time span, top-8 values of the primary auto-column.
  Single-pass scan, bounded buckets (1024 distinct values max).
- **Runtime auto-columns cap with `]` / `[`.** Widen / narrow the
  visible auto-column count without restarting.
- **`NO_COLOR` / `TERM=dumb` / non-tty stdout** force `nocolor`
  regardless of `--theme` and `MGI_PULSE_THEME`, per
  [no-color.org](https://no-color.org/). Closes #10.
- **`;` opens the DSL prompt** as an alternative to `:`, for
  keyboard layouts where typing `:` needs an awkward modifier
  (Russian, some Mac layouts).
- **`bench/gen-ndjson-bursty.sh`** committed to the repo. Used for
  the README hero screenshots.

### Phase C — Documentation, benchmarks, project hygiene

Everything needed so v1.0 is a real commitment, not a vibe.

- **CONTRIBUTING.md** — build, layering rules, how to add a
  format, how to add a UI feature, style notes.
- **Architecture Decision Records** in `docs/adr/`:
  - 0001 — No async runtime (std::thread + crossbeam).
  - 0002 — mmap for files, owned bytes for streams.
  - 0003 — `tail -F | pulse -` and `--follow` are both supported.
  - 0004 — Per-source format dispatch + auto-detect heuristic.
- **BENCHMARKS.md** with the headline 2 GB / ~2.8 s end-to-end
  number, parser hot-path measurements, and explicit "what is NOT
  measured" list.
- **`indexer-bench` binary** in `bench/parse-bench/` — drives the
  real indexer path against any NDJSON file and reports throughput.
- **README "Stability promise" section** — lists what becomes
  stable at v1.0 (CLI flags, keybindings, `--format` values,
  projection field names, DSL grammar) and what's explicitly NOT
  (bookmark sidecar on-disk format, `mgi-pulse-core` library
  surface, perf numbers).
- **GitHub issue / PR templates** (`.github/ISSUE_TEMPLATE/`)
  covering bugs, feature requests, and new formats. PR template
  includes the no-AI-coauthor rule.

### Internals — what moved under the hood

- **`OrPredicate`** mirror of `AndPredicate`, short-circuits on
  first match.
- **`Engine::ingest_one`** for single-record append; used by the
  follow worker.
- **`Engine::source_headers` / `source_regex`** per-source state for
  CSV/TSV (header row) and Regex (compiled pattern). Two-pass
  ingest with `recompute_csv_ts_level` / `recompute_regex_ts_level`
  fills `(ts, severity)` once the per-source state is bound.
- **`FieldCache::with_headers` / `with_regex`** extends the
  predicate-side field projection for stateful formats.
- **`Histogram::build_over_range`** for zoom-windowed histogram
  rendering.
- **`io::SendableProducer`** marker trait for producers that move
  into worker threads.

### Changed

- DSL parser rewritten from a flat clause-AND-clause loop into a
  recursive-descent expression tree.
- Bare-token DSL values now stop at `)` as well as whitespace, so
  `(level=error OR level=warn)` parses correctly.

### Known limitations (carried into v1.0 dogfooding)

- The synchronous backfill path is unchanged — opening a 30 GB
  file still blocks until the indexer finishes. Filed as a
  follow-up issue; the channel + `Engine::ingest_one` infrastructure
  from `--follow` is the foundation for a real background indexer.
- Plain-text logs without a `--pattern` still fall into less-mode.
- `--follow` works for a single file only; multi-file follow needs
  k-way merging on the channel side and isn't done.

### What's left for v1.0

Just dogfooding. The feature set is frozen; what comes next is
real-world use catching real-world bugs. The semver promise in
README starts applying once v1.0 ships, which is gated on two
months of continuous use without a breaking CLI / DSL /
keybinding change.

## [Unreleased]

- Nothing yet — open against `main`.

## [0.2.0] - 2026-06-01

Multi-format pass. NDJSON-only became NDJSON + logfmt + EDN + Python,
plus compressed input, multi-line records, themes, bookmarks, and a
one-line query DSL. Same binary, ~3.4 MB stripped (was 3.0 MB before
the decompressors landed).

### Added

- **Format dispatch.** `LogFormat` enum per source, with auto-detect
  by content for the first three formats. `--format=ndjson|logfmt|edn|python`
  forces a specific parser; unknown values fail with a clear error.
- **logfmt parser.** Go / Heroku `key=value key="quoted"` lines, with
  quoted-string escapes. `LogfmtPairs` iterator yields borrowed slices
  for the common unescaped case.
- **EDN parser.** Clojure `{:key value}` records including namespaced
  keywords (`:log/ts`), `#inst` and `#uuid` tagged literals, and
  nested-map skip. Closes [#1](https://github.com/madgodinc/mgi-pulse/issues/1).
- **Python parser.** `logging.basicConfig()` default format, including
  the comma-millisecond timestamp quirk (PEP 282). Continuation rule
  is non-digit-first-byte so tracebacks fold even when they don't start
  with whitespace.
- **gzip and zstd input.** Detected by magic bytes (not extension);
  decompression is stream-mode so a 6-GB gzip never has to fit in RAM
  uncompressed.
- **Multi-line records.** `MultiLineProducer` wraps any producer and
  folds continuation lines into the preceding record. Format-aware
  via `LogFormat::is_continuation`. Contiguous file-backed continuations
  stay zero-copy through extended-length FileRefs.
- **Query DSL.** Press `:` to enter a one-line expression that compiles
  into the same `AndPredicate` machinery the table filters already
  use. Operators: `=`, `!=`, `~/regex/`, `>`, `>=`, `<`, `<=`. AND
  composition. Time-prefix padding (`ts>2026` works).
- **`FieldCache` / per-record parse-once.** Multi-field predicates
  (regex + field-equals + DSL clauses) share one parse pass per record.
- **Bookmarks.** `b` toggles, `B` cycles. Per-tab, yellow `★` in the
  gutter. Survives filter changes.
- **Themes.** `--theme=dark|light|nocolor` (or `MGI_PULSE_THEME` env
  var). `nocolor` uses only modifiers so output stays readable through
  `script` or on terminals without ANSI colour.
- **`R` schema rescan.** Re-derives auto-columns over the middle of
  the current filtered view. Useful when the initial 10k were a boot
  banner with a different shape than the steady-state log.
- **CLI overrides.** `--time-field=@timestamp`, `--level-field=severity_text`,
  `--columns=N` for non-default schemas.
- **`--no-mouse`.** Disable the default mouse capture for terminals
  where Shift+drag selection isn't enough.
- **Tail infrastructure.** `io::tail::TailReader` implements blocking
  `BufRead` over a file with inode-based rotation detection. Behind
  `--follow` in the CLI, but the synchronous indexer can't open the
  UI in tail mode yet; the flag exits with a pointer to the
  `tail -F | mgi-pulse -` alternative. Native follow lands when a
  background indexer arrives in 0.3.

### Improved

- **Histogram cache key** now `(generation, bars)` instead of
  `(filtered_view.len(), bars)`. Closes a real correctness bug where
  two different predicate sets that happened to keep the same record
  count would render each other's histogram.
- **Owned stream bytes** moved from `HashMap<u64, Box<[u8]>>` to a
  dense `Vec<Box<[u8]>>` indexed by `line_id - stream_base`. Drops
  the hash-lookup overhead on the predicate hot path. Files don't
  touch the storage at all (saves ~176 MB on the 11M-record bench).
- **Less-mode threshold** is now strict majority (`timed*2 > total`)
  instead of "any timed record at all". A stray ISO-shaped line in a
  plain-text log no longer flips the whole UI into structured mode.
- **DetailPane long-line cap** at 256 KB with a `… +Nk more` marker.
  A 200 MB serialized record on one line no longer wedges the
  renderer.
- **mouse-click tab switching.** Click a tab in the tab bar to jump.
- **interaction integration tests.** `pulse-tui/tests/cli.rs` runs the
  real binary against golden fixtures (logfmt, EDN, structured NDJSON,
  ECS-shaped, plain-text, gzip round-trip, theme accept/reject).

### Performance

End-to-end index of the 2 GB / 11 M-record synthetic NDJSON fixture is
unchanged at 2.8-2.9 s on the same dev box (i5-12400F). The format
dispatch indirection didn't cost a measurable cycle in the single-format
case; the FieldCache pays off when two or more predicates touch the
same field on the same record.

Binary grew from 3.0 MB to 3.4 MB stripped, almost entirely from
flate2 + zstd. Memory footprint of the index is down by ~30 % on the
file-only path thanks to the dense stream storage rework.

### Tests

125 total (up from 44 in 0.1.0). Coverage spans every new parser
module, the format-dispatch fallback paths, the cache regression test
from the V01_REVIEW round 1 pass, and a small integration suite that
runs the real binary against golden fixtures.

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

[0.9.0]: https://github.com/madgodinc/mgi-pulse/releases/tag/v0.9.0
[0.2.0]: https://github.com/madgodinc/mgi-pulse/releases/tag/v0.2.0
[0.1.0]: https://github.com/madgodinc/mgi-pulse/releases/tag/v0.1.0
