# ADR 0003 — `tail -F | pulse -` and `--follow` are both supported

**Status:** accepted.
**Date:** 2026-06-03 (initial), copytruncate fix added 2026-06-03.

## Decision

Two ways to follow a live log, both first-class:

1. **Pipe form** — `tail -F live.log | mgi-pulse -`. stdin path,
   no rotation handling on our side, `tail -F` does the heavy
   lifting.
2. **Native `--follow`** — `mgi-pulse --follow live.log`.
   Synchronous backfill of the existing content, then a background
   worker owning a `TailReader` streams new records through a
   crossbeam channel.

Neither is the canonical one. The pipe form is the most portable
fallback. The native form is the obvious shape for the common
single-file case.

## Why both

- **Pipe form covers the corner cases first.** `tail -F` has
  debugged every rotation edge case across every Unix for the
  better part of forty years. Keeping the pipe path means there's
  always a fallback if our own logic misses a mode.
- **Native is what users reach for.** `mgi-pulse --follow app.log`
  is the obvious command shape. Refusing to support it because
  `tail -F` exists would be hostile.
- **They share zero code.** The native worker is ~50 lines of
  glue (`spawn_follow_worker` + `drain_live_channel`) and doesn't
  complicate the pipe path.

## Rotation handling in native `--follow`

Two modes covered by `TailReader::check_rotation`:

- **rename/create** (logrotate's default `create` mode, also what
  `mv app.log app.log.1 && touch app.log` produces). The inode
  changes; we open the new file and read from offset 0.
- **copytruncate** (logrotate's `copytruncate` mode, common for
  daemons that can't be signalled to reopen). The file is copied
  elsewhere, then truncated to 0 bytes in place. Inode is the
  same; size drops below our read cursor. We notice the shrink
  and re-open from offset 0.

Detection is poll-based (500 ms). `notify` / `inotify` /
`kqueue` / `ReadDirectoryChangesW` would cut latency but adds a
cross-platform dependency we don't otherwise need. 500 ms is
well within "feels live" for human review.

## What we don't support

- **`--follow` for stdin.** Pipes don't have an inode and don't
  rotate. Re-implementing the semantics for stdin would just
  duplicate the existing stream path with extra blocking. Use the
  pipe form.
- **Multi-file `--follow`.** k-way merge of multiple growing files
  hasn't shown up as a need. Process substitution or a wrapper
  script around `tail -F` covers it.

## Revisit when

- Someone files an issue that 500 ms rotation latency loses
  events. Fix is either tighter polling or `notify` crate.
- A real use case for multi-file `--follow` shows up. It's a
  per-file worker + k-way merge on the channel side.
