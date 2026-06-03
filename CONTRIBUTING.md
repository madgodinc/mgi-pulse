# Contributing to mgi-pulse

Thanks for the interest. This file is the short version of how the
project is structured; the long version lives in the `docs/adr/`
directory (architecture-decision records) and the inline module docs.

## Building

```sh
cargo build              # debug binary at target/debug/mgi-pulse
cargo build --release    # release binary, ~3-4 MB stripped
cargo test               # full test suite (currently ~245 tests)
cargo clippy             # lints; passes clean on stable
```

The workspace has two crates:

- `mgi-pulse-core` — engine, IO producers, parsers, schema, stats.
  No `ratatui` or terminal dependencies; reusable from any frontend.
- `pulse-tui` — the binary, the `App` struct, panes, key handlers.
  Depends on `mgi-pulse-core`.

MSRV is the workspace's `rust-version` (currently 1.83). Stay on
stable; nightly features are out of scope.

## Layering rules

These are baked into the architecture; please don't quietly invert
them:

- **Engine owns all data. Renderer only reads.** Panes never mutate
  engine state; if they need to, they emit an action and `App`
  applies it.
- **Bytes are either `Owned` or `FileRef{source_id, offset, len}`.**
  Never a borrowed slice with a synthetic lifetime crossing thread
  boundaries.
- **mmap is an optimisation inside `FileProducer`, never the
  fundament.** Live streams (stdin, growing tails) go through the
  stream path with owned buffers.
- **No async runtime.** `std::thread` + `crossbeam-channel`. The
  follow worker is the only background thread today.
- **Predicates are byte-level.** `regex::bytes::Regex`,
  `FieldEqualsPredicate`, `TimeRangePredicate`, etc. UTF-8 lossy
  conversion only at the render boundary so non-UTF-8 logs don't
  drop records.

## Adding a new log format

Two files and four match arms:

1. Add `parse_<name>.rs` to `mgi-pulse-core/src/engine/`. Implement
   `ts_and_level`, `project_field`, and a `looks_like_<name>`
   heuristic. Mirror an existing parser (`parse_logback.rs` for
   line-oriented formats, `parse_syslog.rs` for structured headers,
   `parse_csv.rs` for stateful per-source data).
2. Add the variant to `LogFormat` in `engine/format.rs` and wire it
   into the four match arms there: `parse_ts_level`, `project_field`,
   `is_continuation`, `severity_from_level` / `parse_timestamp`. Also
   add it to `LogFormat::detect` if your heuristic supports it.
3. Add a `--format=<name>` arm to `pulse-tui/src/main.rs`.
4. Write unit tests in the parser module covering: canonical line,
   each severity, malformed input, every recognised projection key,
   the detect heuristic.

If the format needs per-source state (a header, a regex, a config),
extend `Engine::source_<thing>: Vec<Option<...>>` and a
`capture_<thing>` / `recompute_<thing>_ts_level` pair, mirroring
`source_headers` (CSV) or `source_regex` (Regex).

## Adding a new UI feature

The `App` struct owns all UI state. Add fields there, add a key
handler arm in `run_loop`, and add a render call in the `terminal.draw`
closure. Keep panes pure renderers — they take `&Engine + &View` and
return widgets, never mutate.

If the feature has user-typed text (a prompt), extend the `Input`
enum and the prompt label match in the draw closure.

## Tests

Unit tests live next to the code (`#[cfg(test)] mod tests`).
Integration tests for the binary live in `pulse-tui/tests/cli.rs`
and exercise the real `mgi-pulse` binary via `assert_cmd` against
fixtures in `pulse-tui/tests/fixtures/`.

Adding a parser: write the unit tests in the parser module, then
add at least one integration test that asserts the dry-run summary
for a fixture file. Adding a UI feature: unit tests on the App
methods are usually enough; full TUI integration would need a pty
harness which is out of scope today.

## Style

- `rustfmt` is the source of truth — pre-commit hooks aren't enforced,
  please run `cargo fmt` before sending a patch.
- Doc comments on `pub` items. Module-level `//!` doc on every new
  file.
- Prefer dedicated tools to shell-outs. Prefer `regex::bytes::Regex`
  to UTF-8 conversions.
- Don't add `Co-Authored-By: Claude` (or any other AI assistant) to
  commit messages. Authorship lives with the human merging the
  change.

## Bug reports & feature requests

- Bug report: a minimal log fixture that reproduces, plus the
  `mgi-pulse --version` output and your terminal.
- Feature request: describe the workflow, not just the feature — a
  shape "I want X" makes prioritisation easier than "add X".
- New format: a real-world fixture (5-10 lines) plus a one-line
  reference to the spec / canonical generator.

## License

Apache 2.0. See [LICENSE](LICENSE). By submitting a patch you agree
to license it under the same terms.
