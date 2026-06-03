# Changelog

All notable changes to mgi-pulse will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Generic regex-extraction format.** `--pattern='...'` (implies
  `--format=regex`) lets users open any plain-text log by supplying a
  named-capture regex. `ts`, `level`, and any other capture become
  projectable fields the DSL and table can read. `ts` is parsed as
  RFC3339 with prefix-padding (`(?P<ts>\d{4})` works), `level` is
  mapped to a severity name. Lines that don't match land in the
  untimed bucket. Per-source pattern stored in `Engine::source_regex`
  (Arc-shared); `recompute_regex_ts_level` walks the index after
  ingestion to fill `(ts, severity)` from the pattern. New
  `FieldCache::with_regex` for predicate-side projection.
- **Java logback / log4j2 default format.** `--format=logback`
  (aliases: `log4j`, `java`). Parses the canonical Spring Boot /
  log4j2 console pattern: `YYYY-MM-DD HH:MM:SS[.,]mmm LEVEL [thread]
  logger - msg`. Stack-trace continuations (`\tat ...`, `Caused by:
  ...`, `\t... 12 more`) fold into the previous record. Field
  projection: `level`, `thread`, `logger`, `msg`, `ts`. Auto-detect
  goes ahead of generic NDJSON/logfmt.
- **systemd journalctl JSON format.** `--format=journalctl` (aliases:
  `journal`, `systemd`). NDJSON under the hood but with
  `__REALTIME_TIMESTAMP` (microseconds-since-epoch string) for time
  and `PRIORITY` (syslog 0-7) for severity. Field aliases:
  `msg` → `MESSAGE`, `host` → `_HOSTNAME`, `unit` → `_SYSTEMD_UNIT`,
  `ident` → `SYSLOG_IDENTIFIER`. Auto-detect via
  `__REALTIME_TIMESTAMP`/`PRIORITY` byte-scan, ahead of generic NDJSON.

### Changed

- **`looks_like_access` tightened.** The old heuristic ("opening `[`
  after 3 SP-separated tokens") collided with logback's
  `[thread-name]` block. Now requires the bracket contents to look
  like an Apache date (`DD/MMM/YYYY:HH:MM:SS` shape — slashes plus
  colons), eliminating the false-positive without changing the
  positive-case behaviour.

### Added (continued)

- **Native `--follow` mode.** `mgi-pulse --follow app.log` backfills
  the existing file synchronously (same path as a static open), then
  hands off to a background worker that owns a `TailReader` seeked
  to EOF. The worker streams new records through a bounded
  crossbeam channel; the UI loop drains the channel on every tick
  and ingests records into the engine via the new `Engine::ingest_one`.
  Filters re-evaluate against the growing index — the `+N live`
  count surfaces in the status bar as records arrive. Inode-based
  rotation detection is inherited from `TailReader`, so `logrotate`
  doesn't kill the session. New `SendableProducer` marker trait
  enforces the `Send` bound where worker code needs it. Closes #6
  (covering the historical #2 merge: native follow). The huge-file
  background-index acceptance criterion from #6 is intentionally
  not addressed here — the historical backfill is still
  synchronous; see new follow-up issue.
- **Timeline scrub and zoom.** `<` / `>` move a scrub cursor across
  the histogram bins (Shift jumps 10), `+` / `-` zoom the visible
  range (halve / double, anchored on the cursor), `Enter` applies the
  selection as a time-range filter on the active view. Single bin if
  no zoom is active, the full zoom window otherwise. `Esc` cancels
  the scrub on first press; a second `Esc` (or `Esc` with no scrub)
  clears all filters. The histogram cache is invalidated on every
  zoom change so the bars actually reflect the new window. New
  `View::time_range` field composes with the existing
  regex/field/severity/DSL filters via AND, and persists in the
  filter stack until cleared. `Histogram::build_over_range` for the
  per-window build path. Closes #3.
- **Apache / nginx access log format** (Common + Combined Log Format).
  `--format=access` parses the CLF `host - user [date] "request" status
  bytes` shape and the Combined extension that adds `"referer"
  "user_agent"`. Apache time format `[DD/MMM/YYYY:HH:MM:SS ±HHMM]` is
  converted to RFC3339 internally. Severity is synthesised from the
  HTTP status code: 5xx → error, 4xx → warn, 2xx/3xx → info,
  everything else → unknown. Auto-detect picks Access when the
  `[date]`-after-3-tokens signature matches ≥2 lines. Field projection
  exposes `ip`/`host`, `user`, `logname`, `request`, `method`, `uri`,
  `protocol`, `status`, `bytes`, `referer`, `user_agent`, `level`.
  Closes #4.
- **Format auto-detect wired into the CLI.** Running `mgi-pulse foo.log`
  without `--format` now reads a ~16 KiB probe (up to 64 lines) from
  the first file, feeds it to `LogFormat::detect`, and uses the verdict.
  Stdin still defaults to NDJSON — buffering and replaying the stream
  to sniff its shape is a separate concern. The probe order in detect
  is: syslog > NDJSON > EDN > logfmt > NDJSON-fallback > TSV > CSV.
  CSV/TSV are tested last because their signature (≥2 delimiters
  outside quotes) is the loosest and would otherwise eat free-form
  prose with commas. Closes #12.
- **CSV and TSV input.** `--format=csv` / `--format=tsv`. RFC 4180
  quoting (`""` escape inside `"`-quoted values, embedded delimiters
  honoured). First row treated as the column header — column names
  resolve to typed fields for predicates and the DSL. Falls back to
  positional addressing via `_N` (1-based) when no header matches.
  `Engine::capture_csv_headers` captures the header from the first
  record per CSV/TSV source and re-derives `ts` / `level` for every
  data row in a second pass. Closes #9.
- **Syslog RFC 5424 format.** `--format=syslog` parses the standard
  `<PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG`
  shape. PRI's lower 3 bits map to severity (0-2 → fatal, 3 → error,
  4 → warn, 5-6 → info, 7 → debug); facility is ignored. The header
  fields project as `host`, `app`, `procid`, `msgid`, `msg`; structured
  data exposes both bare `SD-ID` membership (`audit=""`) and
  `SD-ID.key` lookups (`origin.ip=10.0.0.1`). Multi-line records fold
  when the continuation lacks a `<` opener. Closes #5.
- **Persistent bookmarks across sessions** for single-file sources.
  Sidecar lives at `$XDG_DATA_HOME/mgi-pulse/bookmarks.json` (default
  `~/.local/share/mgi-pulse/bookmarks.json`). Keyed by inode + size so
  a rotated or truncated file drops its saved bookmarks automatically.
  Stdin and merged sources skip persistence (no stable identity).
  Flush happens once on clean quit; the in-memory bookmarks during the
  session remain the source of truth. Capped at 256 sources with LRU
  eviction. Closes #7.
- **DSL boolean composition: `OR`, `NOT`, parentheses.** The parser is
  now a recursive-descent grammar with conventional precedence (`NOT`
  binds tightest, then `AND`, then `OR`; parens override). Closes #8.
  Keywords are uppercase ASCII so they never collide with field names
  like `and_count`. Examples:
  - `(level=error OR level=warn) AND NOT logger=health-check`
  - `level=error AND (msg~/timeout/ OR msg~/refused/)`
  - `NOT logger=health-check`
- **`OrPredicate`** in `mgi-pulse-core` — mirror of `AndPredicate`,
  short-circuits on first match. Empty composition is vacuously false
  (matches nothing), symmetric to `AndPredicate`'s vacuous true.
- **`NO_COLOR` / `TERM=dumb` / non-tty stdout** force the `nocolor` theme
  regardless of `--theme` and `MGI_PULSE_THEME`. Follows the
  [no-color.org](https://no-color.org/) convention; the precedence is
  env-override > `--theme` flag > `MGI_PULSE_THEME` > `dark` default.
  Closes #10.
- **`;` opens the DSL prompt** as an alternative to `:`, for keyboard
  layouts where typing `:` needs an awkward modifier (Russian, some Mac
  layouts).
- **`bench/gen-ndjson-bursty.sh`** committed to the repo. Time-varying
  severity distribution used for the README hero screenshots so the
  timeline histogram has visible structure instead of a flat strip.

### Changed

- DSL parser rewritten from a flat clause-AND-clause loop into a
  recursive-descent expression tree. Bare-token values now stop at `)`
  as well as whitespace, so `(level=error OR level=warn)` parses the
  inner `warn` as `warn`, not `warn)`. Use a quoted value if a literal
  trailing paren is needed.

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

[0.2.0]: https://github.com/madgodinc/mgi-pulse/releases/tag/v0.2.0
[0.1.0]: https://github.com/madgodinc/mgi-pulse/releases/tag/v0.1.0
