# mgi-pulse

A local-only TUI navigator for NDJSON logs. Not browse logs, navigate them.

![demo](docs/demo.gif)

## Why

Most log tools either tail text (`less`, `tail -F`) or ship a pipeline (Loki,
Vector, an OTEL collector). `mgi-pulse` sits in the gap: open one or many
NDJSON files locally, get a typed table with severity tabs and a regex search,
quit. No daemon, no Docker, no index on disk, no config file. The category
Textualize's Toolong used to occupy is now empty; this is an attempt to fill
it with a different organising idea — a log is not a text file to scroll, it
is a structured event stream to navigate by time, by structure, and by
severity.

## Install

```sh
cargo install --git https://github.com/madgodinc/mgi-pulse pulse-tui
```

Pre-built Linux musl binaries will land with the v0.1.0 GitHub Release.
macOS and Windows builds are CI-checked but not shipped yet.

## Quickstart

```sh
mgi-pulse app.log.ndjson            # open one file
mgi-pulse a.ndjson b.ndjson         # k-way merge by timestamp
tail -F live.log | mgi-pulse -      # stream from stdin
```

Inside the TUI: `Tab` cycles severity views, `/` opens regex search, `f`
opens a `field=value` filter, `d` toggles the detail pane, `q` quits.

## Features (v0.1)

- mmap + memchr line indexer; serde-borrow parse of `ts` and `level` only.
- k-way merge of multiple NDJSON files by timestamp.
- Schema inference over the first 10k records, with auto-derived columns.
- Filters: regex, `field=value`, severity (strict or `min+`), composed with AND.
- Five tabs at startup: `All`, `Error+`, `Warn`, `Info`, `Debug`. `Ctrl-T`
  opens a fresh `All` tab; `Ctrl-W` closes the active one.
- Timeline pane: overview histogram, severity-coloured stacked bars.
- Detail pane: pretty-printed record under the cursor.
- Status bar surfaces parse errors and untimed-record counts.
- Single static binary, ~3 MB stripped, zero config files.

## Keyboard reference

| Key | Action | Notes |
|---|---|---|
| `q` | Quit | Or `Ctrl-C`. |
| `/` | Open regex search | `Enter` applies, `Esc` cancels. |
| `f` | Open `field=value` filter | Composes with regex (AND). |
| `d` | Toggle detail pane | Pretty-printed JSON for the cursor row. |
| `m` | Toggle severity strict / min-mode | `Warn` vs `Warn+`. |
| `0` | Clear severity filter | On the active tab. |
| `1` | Severity = Error+Fatal | Quick filter. |
| `2` | Severity = Warn | Quick filter. |
| `3` | Severity = Info | Quick filter. |
| `4` | Severity = Debug+Trace | Quick filter. |
| `Esc` | Clear all filters on this tab | Regex + field + severity. |
| `Tab` | Next tab | `Shift-Tab` for previous. |
| `Ctrl-T` | Open new tab | Always starts as `All`. |
| `Ctrl-W` | Close active tab | Quits if it was the last. |
| `Up` / `Down` | Move cursor | One row. |
| `PageUp` / `PageDown` | Move cursor | 20 rows. |
| `g` / `G` | Jump to start / end | |
| Mouse wheel | Scroll | Opt-in with `--mouse`. |

## What it doesn't do (yet)

- **Live follow.** No native `tail -F` or inotify. Pipe it in:
  `tail -F file | mgi-pulse -`.
- **Timeline scrubbing or zoom.** The histogram is a static overview; you can't
  yet click or scroll along the time axis to jump.
- **Other log formats.** NDJSON only, plus a raw fallback for lines that don't
  parse. logfmt, plain text with regex extraction, CEE/syslog — not yet.
- **Stack-trace folding.** Multi-line Go / Rust / Java tracebacks are not
  collapsed into one row.
- **Themes.** Colours are fixed (severity-coded).
- **Remote, multi-host, persistence.** Different product.

## Status

v0.1.0, single-developer hobby project. The on-disk format is none (nothing
is persisted), but the key bindings and CLI surface are not stable yet —
breaking changes between 0.1.x are possible. Feedback and bug reports are
welcome on the issue tracker.

## License

Apache-2.0. See [LICENSE](LICENSE).

## Acknowledgments

- [ratatui](https://github.com/ratatui/ratatui) — the TUI rendering.
- [memmap2](https://github.com/RazrFalcon/memmap2-rs) — mmap.
- [serde](https://serde.rs/) / [serde_json](https://github.com/serde-rs/json)
  — borrowed JSON parsing of `ts` and `level`.
- [regex](https://github.com/rust-lang/regex) — search.
