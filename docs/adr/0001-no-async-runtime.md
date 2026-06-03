# ADR 0001 — No async runtime

**Status:** accepted.
**Date:** 2026-06-01 (initial), reaffirmed 2026-06-03.

## Decision

`std::thread` plus `crossbeam-channel`. No tokio, no async-std, no
smol.

## Context

The project has exactly one background-thread use case so far —
the `--follow` worker that pulls records from a `TailReader` and
ships them to the UI through a channel. Everything else (file
indexing, query scans, render loop) runs synchronously on the main
thread.

## Why not async?

- **Single worker.** The runtime overhead of an executor would buy
  nothing — we have one thread doing one thing.
- **No network IO.** Async pays off when you can keep many sockets
  in flight. We read local files and one optional follow source.
- **Binary size.** tokio + its runtime crates pulls ~1 MB into a
  ~3 MB binary. Worth it for a server, not for a viewer.
- **Cancellation.** `std::thread` doesn't have a free shutdown
  signal; we use channel disconnects as the signal, which is
  cooperative and good enough for the follow worker's "the UI
  quit" case.
- **Producers stay sync.** `RecordProducer::next` is a blocking
  call. Making it async would force every parser to be `async fn`,
  which is a colossal blast radius for zero throughput win on
  bounded file IO.

## Why crossbeam-channel?

- Sync-friendly (no async wrappers).
- MPMC out of the box if we ever need multiple workers.
- Selectable (`Select::new`) for the future case where the worker
  needs to listen on multiple sources.

## Revisit if

- We add a server / daemon mode that holds many tail sources at
  once. Then the per-source thread cost dominates and a small
  async pool is the right shape.
- A future format has an async-only library we can't avoid (e.g.
  an OAuth-protected cloud log source). Then async is justified
  for that adapter, not the engine.

## Not in scope

- Async-style code that's actually thread-pool work (rayon for
  parallel parsing) — that's a separate ADR if it ever lands.
