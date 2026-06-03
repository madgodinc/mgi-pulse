# ADR 0002 — mmap for files, owned bytes for streams

**Status:** accepted.
**Date:** 2026-06-01.

## Decision

- `FileProducer` uses `memmap2` and emits records as
  `RecordBytes::FileRef { source_id, offset, len }`.
- `StreamProducer` (stdin, decompressed input, the follow worker's
  `TailReader`) reads into a per-record `Box<[u8]>` and emits
  `RecordBytes::Owned`.

Two storage shapes, one `Predicate` machinery on top.

## Context

There are two natural shapes for log input:

- A bounded file on disk the user points us at. mmap is ideal —
  zero-copy, kernel-managed page cache, random access by
  `line_id`.
- A growing stream of unknown length: stdin, a decompressor's
  output, a TLS-wrapped socket. mmap doesn't apply; the bytes
  appear over time.

## What this split buys

- **mmap gets us ~12 GB/s scan throughput** on the indexer's
  `memchr::memchr_iter` line splitter. That sets the floor on
  which everything else's perf budget rests.
- **`FileRef` is `Copy`-cheap.** A `LineLoc { source_id, offset,
  len }` is 16 bytes; cloning the index for snapshotting or
  cross-thread sharing is essentially free.
- **`Owned` is `Send`-safe.** The follow worker can ship records
  through a channel without lifetime gymnastics.
- **One `Predicate` interface.** `FieldCache` doesn't care which
  variant the record carries; `Engine::line_bytes` resolves both
  transparently.

## What this split costs

- Two ingest paths in `pulse-tui::ingest_file` (the compression
  branch falls into the stream path because the decompressor
  can't give us a `Mmap`).
- `FileRef::source_id` plus the parallel `Vec<Arc<Mmap>>` is a
  discipline. Storing a `FileRef` from one source against another
  source's mmap would be a memory-safety bug. Single-source
  pipelines today don't exercise it, but k-way merge does.

## Revisit when

- Per-record `Box<[u8]>` allocation dominates the follow worker's
  CPU under heavy load. A pooled arena would help, but it isn't
  measured to matter yet.
- A platform without mmap support shows up (some sandboxed WASM
  runtimes). `FileProducer` would fall back to the stream path
  on that platform.

## Anti-decisions

- **Do not bring back `Cow<'static, [u8]>`.** The first design
  walked into it and ended up with a dangling-slice hazard the
  moment a thread boundary appeared.
- **Do not re-mmap on growing files.** `TailReader` reads via
  `BufRead` off a file handle; we don't remap mid-flight. Rotation
  is handled by reopening (see ADR 0003 for the modes covered).
