# Contributing to mgi-pulse

Short version: tests pass, `cargo fmt` ran, CHANGELOG updated, no AI
co-authors in the commit. Long version below.

## Workspace layout

Two crates:

- `mgi-pulse-core` — the engine. IO producers, parsers, schema, stats,
  predicates, histogram. No `ratatui`, no terminal code. If you want
  to embed pulse-style indexing in another frontend, this is the
  entry point.
- `pulse-tui` — the binary. `App` state, panes, key handlers,
  command-line parsing. Depends on `mgi-pulse-core`.

```sh
cargo build              # debug binary at target/debug/mgi-pulse
cargo build --release    # release binary, ~3-4 MB stripped
cargo test               # 245 tests at the time of writing
cargo clippy             # clean on stable
cargo fmt                # required before sending a patch
```

MSRV is the workspace's `rust-version` (currently 1.83). Stable only.

## Layering rules

Two rules that the architecture leans on heavily — please don't quietly
invert them:

1. **The engine owns all data, the renderer only reads.** Panes never
   mutate engine state. If a user action needs to change something,
   the key handler in `App` does the mutation; the pane re-renders
   from the new state on the next tick.

2. **Bytes are either `Owned(Box<[u8]>)` or `FileRef { source_id, offset,
   len }`.** Never a borrowed slice with a synthetic lifetime crossing
   thread boundaries. The first design tried `Cow<'static, [u8]>` and
   it ended up as a dangling-slice hazard the moment we introduced a
   worker thread — see ADR 0002.

Two related conventions:

- **No async runtime** — `std::thread` + `crossbeam-channel`. The
  follow worker is the only background thread today. See ADR 0001
  for why.
- **Predicates are byte-level.** `regex::bytes::Regex`,
  `FieldEqualsPredicate`, `TimeRangePredicate` all work on `&[u8]`.
  UTF-8 conversion happens at render boundaries only, so non-UTF-8
  payloads don't drop records.

## Adding a log format

A new format is two files plus four match arms.

1. **Parser module:** add `parse_<name>.rs` to
   `mgi-pulse-core/src/engine/`. Implement `ts_and_level`,
   `project_field`, and a `looks_like_<name>` heuristic for
   auto-detect. Mirror an existing parser:
   - Line-oriented with a header (Logback, Python) — copy
     `parse_logback.rs`.
   - Structured per-line (NDJSON, syslog) — copy `parse_syslog.rs`.
   - Stateful per-source (CSV, regex) — copy `parse_csv.rs` and the
     `recompute_<name>_ts_level` pattern in `engine/mod.rs`.

2. **Format enum:** add the variant to `LogFormat` in
   `engine/format.rs` and wire it into the four match arms there —
   `parse_ts_level`, `project_field`, `is_continuation`,
   `severity_from_level`. Add to `LogFormat::detect` if the heuristic
   is reliable.

3. **CLI flag:** add a `--format=<name>` arm to
   `pulse-tui/src/main.rs`.

4. **Tests:** unit tests in the parser module covering the canonical
   line, each severity bucket, malformed input, every recognised
   projection key, and the detect heuristic. Plus at least one
   integration test against a fixture in `pulse-tui/tests/fixtures/`
   so the dry-run output is asserted end-to-end.

## Adding a UI feature

`App` owns all UI state. Add fields there, add a key handler arm in
`run_loop`, add a render call in the `terminal.draw` closure. Panes
stay pure renderers — they take `&Engine + &View` and return widgets,
they never mutate.

If the feature needs user-typed text (a prompt), extend the `Input`
enum and the prompt-label match in the draw closure. The existing
`Input::Search` / `Filter` / `JumpTime` / `Dsl` / `Save` paths share
the same Backspace / Char handling — extend the catch-all match arms
to include your new variant.

## Tests

Unit tests live next to the code (`#[cfg(test)] mod tests`).
Integration tests for the binary live in `pulse-tui/tests/cli.rs` and
exercise the real `mgi-pulse` binary via `assert_cmd` against
fixtures in `pulse-tui/tests/fixtures/`. The dry-run flag is the
standard probe — `--dry-run` indexes the file, prints a one-line
summary, and exits, so integration tests can assert on that summary
without spawning a terminal.

Full TUI rendering tests would need a pty harness — out of scope
today, but the engine-level coverage is the load-bearing part.

## Style

- `rustfmt` is the source of truth — pre-commit hooks aren't
  enforced, please run `cargo fmt` before sending a patch.
- Doc comments on `pub` items. Module-level `//!` doc on every new
  file with one paragraph explaining the module's role.
- Prefer `regex::bytes::Regex` over UTF-8 lossy conversions when
  working with raw record bytes.
- Don't add `Co-Authored-By: Claude` (or any other AI assistant) to
  commit messages. Authorship lives with the human merging the
  change.

## Filing issues

- **Bug:** a minimal log fixture that reproduces (5-10 lines is
  ideal), the `mgi-pulse --version` output, and your terminal.
- **Feature:** describe the workflow, not just the feature. "I'm
  trying to do X, the existing way is Y, here's why Y doesn't fit"
  is much easier to act on than "add Z".
- **New format:** a real-world fixture (5-10 lines, ideally with at
  least one error record so severity mapping can be tested), plus
  a link to the spec or canonical emitter.

The `.github/ISSUE_TEMPLATE/` files have the same shape pre-filled.

## License

Apache-2.0. By submitting a patch you agree to license it under the
same terms.
