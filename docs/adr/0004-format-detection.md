# ADR 0004 — Per-source format dispatch and auto-detect

**Status:** accepted.
**Date:** 2026-06-01 (initial), extended 2026-06-03.

## Decision

- A log format is a property of the **source**, not the line. One
  file is one format. `Engine::source_formats[source_id]` holds
  the format; parsers and predicates dispatch through a match on
  it.
- Auto-detect runs at probe time on the first ~16 KiB of the
  first file. The result becomes the format for every record from
  that source. The user can override with `--format=<name>`.

## Per-line format dispatch is out of scope

Mixed-format inputs (an NDJSON file with a plain-text banner at
the top, for example) come up in the wild. We don't dispatch
per-line because:

- The hot indexer path would add a branch per record.
- "Banner" lines are usually a handful at the start; the multi-
  line / continuation rules already fold most of them into the
  preceding record.
- The existing raw-passthrough fallback handles the rare cases
  adequately.

If a real workflow needs per-line dispatch, a `format_hint` from
the producer to the indexer is a small change — but waiting for
the workflow to actually appear.

## Auto-detect heuristic

`LogFormat::detect` votes across the first ~64 non-empty lines.
A format needs ≥ 2 supporting lines AND a strict majority over
its rivals to win. Single-line files or ambiguous samples fall
back to `LogFormat::Ndjson`.

Precedence by signature specificity, most specific first:

1. **Syslog 5424** — `<DIGITS>1 ` opener. Unambiguous.
2. **Access log** — `[DD/MMM/YYYY:HH:MM:SS ±HHMM]` block with
   slashes plus colons (distinguishes from logback's
   `[thread-name]`).
3. **Logback / log4j2** — `YYYY-MM-DD HH:MM:SS[.,]mmm LEVEL`
   shape. Goes before generic NDJSON because the prefix is a
   digit and would otherwise fall through.
4. **journalctl JSON** — JSON containing `__REALTIME_TIMESTAMP`
   or `PRIORITY` (byte-scan, no full parse).
5. **NDJSON** — `{` opener, `}` closer (or EDN by inner sigil).
6. **logfmt** — `key=value` pairs, ≥ 2 per line.
7. **TSV / CSV** — `delim_vote` heuristic. Last because "≥ 2
   delimiters outside quotes" is the loosest signature and would
   otherwise claim free-form prose with commas.

## Fallback is NDJSON, not less-mode

When no format reaches the 2-vote threshold (single-line files,
genuinely plain text, ambiguous samples), the fallback is
NDJSON. Rationale:

- NDJSON sources with one line of probe content land on the
  right parser.
- Plain text fails to parse as NDJSON and the failure surfaces
  in the dry-run summary (`json errors: N`), giving the user a
  clear signal to pass `--format` or `--pattern`.
- Falling back to less-mode silently would hide the misdetection.

## Probe window vs full file

Only the head of the file is sampled. A file that opens with a
banner of a different shape than the body can fool the detector.
Mitigations:

- The user can force the format with `--format=...`.
- The `R` key rescans the **schema** (column derivation) over
  the middle of the current filtered view. It does NOT re-run
  format detection — the format decision is committed at ingest
  time and isn't revisited. If the banner misled detect, restart
  with `--format`.

## Stateful formats

CSV/TSV need the column header from the first record; regex
extraction needs the user's `--pattern`. Both park untimed
records on the indexer pass, then `Engine::recompute_<thing>_ts_level`
walks back over the index once the per-source state is bound. The
cost is one extra pass over the file at startup.

## Revisit when

- Mixed-format files show up as a real workflow. Per-record
  dispatch via a `format_hint` becomes a small refactor.
- Auto-detect false positives accumulate. The fix is per-line
  validation (e.g. parse the first three voted lines' timestamps
  before accepting the vote) — a heuristic refinement, not a
  redesign.
