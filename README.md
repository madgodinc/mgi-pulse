# mgi-pulse

A local-only TUI navigator for NDJSON logs. Not browse logs, navigate them.

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

**Pre-built Linux x86_64 binary** (musl-static, no runtime deps):

```sh
curl -L https://github.com/madgodinc/mgi-pulse/releases/latest/download/mgi-pulse-v0.1.0-x86_64-unknown-linux-musl.tar.gz | tar -xz
./mgi-pulse --help
```

**Build from source** (any Rust toolchain ≥ 1.83):

```sh
cargo install --git https://github.com/madgodinc/mgi-pulse mgi-pulse
```

macOS and Windows builds are CI-checked on each commit but pre-built
binaries for those platforms are not shipped yet — build from source
on those platforms for now.

## Quickstart

```sh
mgi-pulse app.log.ndjson            # open one file
mgi-pulse a.ndjson b.ndjson         # k-way merge by timestamp
tail -F live.log | mgi-pulse -      # stream from stdin
mgi-pulse anything.log              # plain text works too — see "less-mode"
mgi-pulse app.log.gz                # gzip auto-detected by magic bytes
mgi-pulse app.log.zst               # zstd too
mgi-pulse --format=logfmt go.log    # Go / Heroku-style key=value pairs
mgi-pulse --format=edn clojure.edn  # Clojure {:key value} maps
```

Inside the TUI: `Tab` cycles severity views, `/` opens regex search, `f`
opens a `field=value` filter, `t` jumps to a timestamp, `d` toggles the
detail pane, `q` quits.

### Less-mode (plain-text fallback)

If the file has no parseable JSON structure (e.g. `log4j`/`logback`
defaults, raw stdout, Clojure println output), `mgi-pulse` collapses
into a `less`-style view: line numbers + raw payload across the full
width, no empty columns, no empty severity tabs. Regex search and
cursor navigation still work. The detail pane (`d`) becomes a
±5-line context viewer so multi-line stack traces read as a block.

That makes it useful as a no-config `less` replacement even when the
input is unstructured — just without the typed table you get on
NDJSON.

## Features (v0.1)

- mmap + memchr line indexer; serde-borrow parse of `ts` and `level` only.
- k-way merge of multiple NDJSON files by timestamp.
- Schema inference over the first 10k records, with auto-derived columns.
- Filters: regex, `field=value`, severity (strict or `min+`), composed with AND.
- Five tabs at startup: `All`, `Error`, `Warn`, `Info`, `Debug+Trace`.
  `Ctrl-T` opens a fresh `All` tab; `Ctrl-W` closes the active one.
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
| `t` | Jump to a timestamp | RFC3339 prefix, e.g. `2026-06-01T12:00`. |
| `d` | Toggle detail pane | Pretty-printed JSON for NDJSON, ±5 lines of context for plain-text. |
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
| Mouse wheel | Scroll | One row per tick. Enabled by default. |
| Mouse click | Click a tab to switch | Click in the tab bar only. |

### Mouse capture and terminal selection

Mouse capture is on by default so the wheel scrolls the table and you can
click tabs. That intercepts text selection too — to copy a line, hold
**Shift** while you drag the mouse and the terminal handles the selection
directly. (Standard TUI convention, works in WezTerm, Alacritty,
GNOME-Terminal, Konsole, iTerm2.) Pass `--no-mouse` to disable capture
entirely if you need the unmodified terminal selection back — useful over
SSH or with a copy-on-select setup.

### Static files vs live files (mmap safety)

`mgi-pulse` `mmap`s files for speed. **This is safe for static log
snapshots but unsafe for files that another process may truncate or
replace while you're viewing them** — reading past a truncated mmap
region delivers SIGBUS to the process, killing it. That's a Unix
mechanic, not a Rust error, so we can't catch it.

Two rules of thumb:

- **Static / archived logs:** open them directly. `mgi-pulse app.log`,
  `mgi-pulse error.log.1`, `mgi-pulse 2026-06-01.ndjson` — all safe.
- **Active / live logs:** **don't open them as files**. Pipe instead:

  ```sh
  tail -F /var/log/app.log | mgi-pulse -
  ```

  `tail -F` survives rotation and feeds `mgi-pulse` via stdin, which
  uses owned buffers (no mmap) and is robust to whatever the writer
  does to the file underneath.

If you can't tell whether the file is live, copy it first
(`cp app.log /tmp/snap.ndjson && mgi-pulse /tmp/snap.ndjson`). v0.2 will
ship a native follow mode (inotify / kqueue) so this footnote goes
away.

## What it doesn't do (yet)

- **Live follow.** No native `tail -F` or inotify. Pipe it in:
  `tail -F file | mgi-pulse -`.
- **Timeline scrubbing or zoom.** The histogram is a static overview; you can't
  yet click or scroll along the time axis to jump.
- **Other log formats.** NDJSON, logfmt, and EDN today (auto-detect by
  content; override with `--format=ndjson|logfmt|edn`). Plain text with
  regex extraction, CEE/syslog, Apache/nginx access logs — not yet.
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
