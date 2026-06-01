//! End-to-end integration tests for the `mgi-pulse` binary.
//!
//! These exercise the producer → indexer → engine → schema pipeline by
//! running the real binary against golden NDJSON / plain-text fixtures
//! and parsing the `--dry-run` summary line. They catch interaction bugs
//! that unit tests miss — for example the histogram-cache bug from the
//! V01_REVIEW pass survived 42 unit tests but would fail this kind of
//! pipeline check.

use assert_cmd::Command;
use predicates::str::contains;

fn fixture(name: &str) -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/tests/fixtures/{}", dir, name)
}

#[test]
fn structured_ndjson_indexes_with_defaults() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("structured.ndjson"))
        .assert()
        .success()
        .stdout(contains("indexed 4 records"))
        .stdout(contains("untimed: 0"))
        .stdout(contains("json errors: 0"))
        .stdout(contains("auto-columns: [\"logger\", \"msg\"]"));
}

#[test]
fn ecs_style_needs_overrides_to_avoid_untimed() {
    // Without overrides: every record lands in untimed because @timestamp
    // and severity_text are not the defaults.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("ecs.ndjson"))
        .assert()
        .success()
        .stdout(contains("indexed 2 records"))
        .stdout(contains("untimed: 2"));
}

#[test]
fn ecs_style_with_overrides_parses_cleanly() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--time-field=@timestamp")
        .arg("--level-field=severity_text")
        .arg(fixture("ecs.ndjson"))
        .assert()
        .success()
        .stdout(contains("indexed 2 records"))
        .stdout(contains("untimed: 0"));
}

#[test]
fn plain_text_falls_into_less_mode() {
    // Every line fails the JSON parse, so the indexer counts them all as
    // json errors and bumps the untimed counter. No auto-columns.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("plain.log"))
        .assert()
        .success()
        .stdout(contains("indexed 5 records"))
        .stdout(contains("json errors: 5"))
        .stdout(contains("auto-columns: []"));
}

#[test]
fn columns_cap_is_accepted() {
    // --columns clips the count in the renderer (run-time, not in dry-run
    // output). The point here is just that the flag is accepted without
    // a clap-level error.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--columns=1")
        .arg(fixture("structured.ndjson"))
        .assert()
        .success();
}

#[test]
fn no_args_with_empty_stdin_completes_cleanly() {
    // No files, empty stdin pipe → indexer drains 0 records and exits.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .write_stdin("")
        .assert()
        .success();
}
