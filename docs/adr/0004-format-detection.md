# ADR 0004 — Per-source format dispatch + auto-detect

**Status:** accepted.
**Date:** 2026-06-01 (initial), extended 2026-06-03.

## Decision

- A log format is a property of the **source**, not the line. One
  file is always one format. `Engine::source_formats` holds the
  format per source_id; predicates and parsers dispatch through a
  match on the format.
- Auto-detect runs at probe time on the first ~16 KiB of the first
  file. The result is the format for every record from that source.
  The user can override with `--format=<name>`.

## Per-line format is out of scope

Mixed-format inputs (e.g. a single file with NDJSON + plain-text
banner lines) are common in the wild. We don't try to dispatch
per-line because:

- The hot indexer path would do an extra branch per record.
- "Banner" lines are typically a handful at the start; the multi-
  line / continuation rules already fold most of them into the
  preceding record.
- Real mixed-format streams are rare enough that the existing
  raw-passthrough fallback covers them adequately.

## Auto-detect heuristic

`LogFormat::detect` votes across the first ~64 non-empty lines:

1. `looks_like_syslog` — `<DIGITS>1 ` shape. Unambiguous.
2. `looks_like_access` — `[DD/MMM/YYYY:HH:MM:SS]` block.
   Slashes-plus-colons inside the brackets distinguishes from
   logback's `[thread]`.
3. `looks_like_logback` — `YYYY-MM-DD HH:MM:SS[.,]mmm LEVEL` shape.
   Goes before generic NDJSON because it's a digit-prefix line and
   would otherwise fall through.
4. `looks_like_journalctl` — JSON with `__REALTIME_TIMESTAMP` or
   `PRIORITY`.
5. Generic JSON braces (`{...}`) — NDJSON or EDN by inner
   signature.
6. Logfmt — `key=value` pairs.
7. CSV / TSV — `delim_vote` heuristic. Goes last because two
   commas outside quotes is the loosest signature.

Tie-breaking is hard-coded in `LogFormat::detect`'s precedence
chain. A vote needs ≥ 2 supporting lines to win — single-line
matches don't claim the format.

## Why probe at all (not just NDJSON default)

The original v0.1 fallback was NDJSON, which made
`./mgi-pulse syslog.log` index every line as a JSON parse error
without a hint to the user. The probe is cheap (16 KiB read + a
byte scan); for the cost of one I/O it eliminates 90 % of the
"why is my logfile broken" reports.

## Stateful formats

CSV/TSV need the column header. Regex extraction needs the user's
`--pattern`. Both park untimed records on the indexer pass and
then `Engine::recompute_<thing>_ts_level` walks back over the
index once the per-source state is bound. The cost is one extra
pass over the file at startup, which is acceptable for the v0.x
file sizes we target.

## Revisit if

- Mixed-format files show up as a real workflow. Then per-record
  dispatch via a `format_hint` from the producer is a small
  change to the indexer.
- Auto-detect false positives accumulate. The fix is per-line
  validation (e.g. parse the first 3 votes' timestamps and only
  vote if they all succeed) — a small heuristic refinement, not a
  redesign.
