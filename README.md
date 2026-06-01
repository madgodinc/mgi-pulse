# mgi-pulse

> Not browse logs, navigate them.

A TUI navigator for local NDJSON application logs. Zero-config, single static
binary, Rust. v0.1 is in active development.

The category Toolong stopped maintaining in 2025 (Textualize wound down). The
goal here is not "Toolong on Rust" but a different organizing principle: a log
is not a text file to scroll, it is a structured event stream to navigate by
time, by structure, and (eventually) by causal units.

## Status

**v0.1 in progress — M0 skeleton committed.**

| Milestone | What it delivers |
|---|---|
| M0 | Workspace skeleton, decisions on paper, CI seed |
| M1 | mmap file source + stdin source, line/time/severity indexes, `/search` |
| M1.5 | k-way merge of multiple sources by timestamp |
| M2 | Auto-columns from NDJSON, click-filter, detail pane |
| M3 | Time histogram + keyboard scrub, v0.1 public release |

## What v0.1 will not do

- Native follow / inotify — use `tail -F file | mgi-pulse -` for live.
- Plain text without structure (logfmt arrives in v0.2 at the earliest).
- Remote, multi-host, server, multi-user.
- An error-tracker. Different product.
- macOS / Windows binaries (Linux musl static only at release).

## Build

Requires a recent stable Rust toolchain (1.83+ at time of writing).

```sh
cargo build --workspace
```

## Layout

```
mgi-pulse-core/   # engine, IO, schema, indexes. no UI deps.
pulse-tui/        # ratatui binary 'mgi-pulse'.
```

## License

Apache-2.0.
