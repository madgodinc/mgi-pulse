# ADR 0001 — No async runtime

**Status:** accepted.
**Date:** 2026-06-01 (initial), reaffirmed 2026-06-03.

## Decision

`std::thread` plus `crossbeam-channel`. No tokio, no async-std, no
smol.

## Context

Today the project has one background-thread case: the `--follow`
worker pulling records from `TailReader` and shipping them to the
UI through a channel. File indexing, query scans, render loop —
all on the main thread.

## Reasons

- **Runtime overhead buys nothing here.** One worker doing
  `read_until` in a loop doesn't benefit from a scheduler.
- **No network IO.** Async pays off when you keep many sockets
  in flight. We read local files and one optional follow source.
- **Binary size.** tokio + its runtime crates pulls roughly 1 MB
  of dependencies into a ~3 MB release binary. Worth it for a
  server, not for a viewer.
- **Cancellation is cooperative.** `std::thread` doesn't have a
  free shutdown signal, but channel disconnects work fine for the
  follow worker — the UI dropping its receiver tells the worker
  to exit.
- **Producers stay sync.** `RecordProducer::next` is blocking.
  Making it `async fn` would push the `async` keyword through
  every parser for zero throughput on bounded file IO.

## Why crossbeam-channel specifically

- Sync-friendly (no executor wrapping).
- Multi-producer / multi-consumer out of the box, ready for the
  future case where multiple sources feed one indexer.
- `Select::new` for the case where a worker needs to listen on
  more than one channel.

## Revisit when

- A server / daemon mode that holds many tail sources at once
  shows up. Per-source thread cost dominates and a small async
  pool becomes the right shape.
- A required format only has an async-only library (e.g. an
  OAuth-protected cloud log source). Justifies async for that
  adapter, not the engine.

## Not in scope

Parallel parsing via a thread pool (rayon) is a separate ADR if
it ever ships — it's thread-pool work, not async, and the choice
is independent of this one.
