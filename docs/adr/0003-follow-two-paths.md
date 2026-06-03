# ADR 0003 — `tail -F | pulse -` AND `--follow` both supported

**Status:** accepted.
**Date:** 2026-06-03.

## Decision

mgi-pulse intentionally supports two ways to follow a live log:

1. The pipe form — `tail -F live.log | mgi-pulse -`. stdin path,
   no rotation handling on our side, `tail -F` does the heavy
   lifting.
2. Native `--follow` — `mgi-pulse --follow live.log`. Synchronous
   backfill of the existing content, then a background worker
   owning a `TailReader` streams new records through a
   crossbeam-channel.

Neither is the canonical one. The pipe form is the simplest and
the most portable; the native form is more ergonomic for the
common single-file follow case.

## Why both

- **Pipe form is the safety net.** Some environments (Windows
  shells, containerised init systems, weird filesystems) handle
  rotation in surprising ways that `tail -F` has already debugged
  for us. Keeping the pipe path means there's always a fallback.
- **Native is what users reach for.** Typing
  `mgi-pulse --follow app.log` is the obvious shape; refusing to
  support it because `tail -F` exists would be hostile.
- **They share zero code.** The native worker is ~50 lines of
  glue (`spawn_follow_worker` + `drain_live_channel`). It doesn't
  complicate the pipe path at all.

## What we don't support

- **`--follow` for stdin.** Pipes don't have an inode and don't
  rotate, so re-implementing that semantics for stdin would just
  duplicate the existing stream path with extra blocking. Use the
  pipe form.
- **Multi-file `--follow`.** k-way merge of multiple growing files
  hasn't shown up as a need. Pipe form covers it via process
  substitution or a wrapper script.
- **inotify / kqueue / ReadDirectoryChangesW.** Rotation detection
  is poll-based (`TailReader::check_rotation` checks the inode
  every 500 ms). notify-crate would cut the latency but adds a
  cross-platform dependency we don't otherwise need. The 500 ms
  is well within "feels live" for human review.

## Revisit if

- Someone files an issue that 500 ms rotation latency loses
  events. The fix is either tighter polling or notify-crate.
- A real-world use case for multi-file `--follow` shows up. Then
  it's a per-file worker + k-way merge channel.
