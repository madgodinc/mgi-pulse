# ADR 0002 — mmap for files, owned bytes for streams

**Status:** accepted.
**Date:** 2026-06-01.

## Decision

`FileProducer` uses `memmap2` and emits records as
`RecordBytes::FileRef { source_id, offset, len }`. `StreamProducer`
(stdin, decompressed input, the follow worker's `TailReader`) reads
into a per-record `Box<[u8]>` and emits `RecordBytes::Owned`.

## Context

There are two natural shapes for log input:

- A bounded, on-disk file the user pointed us at. mmap is ideal —
  zero-copy, kernel-managed page cache, random access by `line_id`.
- A growing stream of arbitrary length: stdin, a TLS-wrapped TCP
  socket, a decompressor's output. mmap doesn't apply; the bytes
  are produced on-the-fly.

## Why this split

- **mmap gets us 12 GB/s scan throughput** on the indexer's
  `memchr::memchr_iter` line splitter. That's the floor on which
  the rest of the engine's performance budget rests.
- **`FileRef` is `Copy`-cheap.** A `LineLoc { source_id, offset,
  len }` is 16 bytes; cloning the index for snapshotting or
  cross-thread sharing is essentially free.
- **`Owned` is safe to cross threads.** The follow worker can ship
  `Owned` bytes through `crossbeam-channel::Sender<RawRecord>`
  without lifetime gymnastics.
- **Both share the `Predicate` machinery.** `FieldCache` doesn't
  care which variant the record carries; `Engine::line_bytes`
  resolves both transparently.

## Costs accepted

- Slight code complexity in `Engine::line_bytes` (it has to check
  whether the line is in `owned_lines` or in the `mmaps` snapshot).
- Two ingest paths in `pulse-tui::ingest_file` (the compression
  branch falls into the stream path because the decompressor can't
  give us a `Mmap`).
- `FileRef::source_id` plus the parallel `Vec<Arc<Mmap>>` keyed by
  source_id is a discipline; getting it wrong (storing a `FileRef`
  from one source against another source's mmap) would be a memory-
  safety bug. The single-source pipelines today don't exercise it,
  but k-way merge and the future stream multiplexing rely on it.

## Revisit if

- Per-record `Box<[u8]>` allocation dominates the follow worker's
  CPU under heavy load. A pooled arena would help, but isn't
  measured to matter yet.
- A platform shows up that doesn't support mmap (some sandboxed
  WASM runtimes). Then `FileProducer` would fall back to the
  stream path too.

## Anti-decisions

- **No `Cow<'static, [u8]>`.** The first design walked into this and
  it ended up as a dangling-slice hazard the moment we crossed a
  thread boundary. Don't bring it back.
- **No re-mmap on growing files.** `TailReader` reads via `BufRead`
  off a file handle; we don't try to re-map the file mid-flight.
  Rotation is handled by reopening the file.
