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

// --- Interaction tests added in round 2 (V01_REVIEW critic pass).
// These exist to catch bug classes the happy-path tests above miss.

#[test]
fn mixed_timed_majority_keeps_structured_mode() {
    // 4 timed records + 2 plain → majority is timed → has_timestamps()
    // stays true → less-mode does NOT kick in. Regression test for the
    // ">50% threshold" rule (Q6 in V01_REVIEW). Untimed in this fixture
    // are the 2 plain-text lines that fail JSON parse entirely.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("mixed-timed.ndjson"))
        .assert()
        .success()
        .stdout(contains("indexed 6 records"))
        .stdout(contains("untimed: 2"))
        .stdout(contains("json errors: 2"))
        // schema warmup saw the structured majority, so auto-columns are
        // derived from the JSON fields. msg is the most-present.
        .stdout(contains("\"msg\""));
}

#[test]
fn logfmt_input_with_force_flag_parses_ts_and_severity() {
    // logfmt: 5 records, all timed, severity=info/warn/error distributed.
    // Without --format the line wouldn't be JSON and would land in
    // less-mode; with --format=logfmt all 5 lines parse correctly.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--format=logfmt")
        .arg(fixture("logfmt.log"))
        .assert()
        .success()
        .stdout(contains("indexed 5 records"))
        .stdout(contains("untimed: 0"))
        // schema scanner is NDJSON-only in v0.1.x; logfmt schema lands in
        // a later step. Auto-columns stay empty for now.
        .stdout(contains("auto-columns: []"));
}

#[test]
fn logfmt_without_flag_falls_into_less_mode() {
    // Without --format=logfmt the parser treats every line as JSON,
    // every parse fails, every record lands in less-mode.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("logfmt.log"))
        .assert()
        .success()
        .stdout(contains("indexed 5 records"))
        .stdout(contains("json errors: 5"));
}

#[test]
fn edn_input_with_force_flag_parses_clojure_records() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--format=edn")
        .arg(fixture("clojure.edn"))
        .assert()
        .success()
        .stdout(contains("indexed 4 records"))
        .stdout(contains("untimed: 0"))
        .stdout(contains("json errors: 0"));
}

#[test]
fn edn_without_flag_falls_into_less_mode() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("clojure.edn"))
        .assert()
        .success()
        .stdout(contains("indexed 4 records"))
        // Without --format=edn the parser treats every line as JSON
        // and bails on the keyword sigil immediately.
        .stdout(contains("json errors: 4"));
}

#[test]
fn gzip_file_is_decompressed_transparently() {
    use std::io::Write;
    // Build a temp .gz on the fly so the test doesn't depend on shipping
    // a binary fixture under git.
    let dir = env!("CARGO_MANIFEST_DIR");
    let plain = std::fs::read(format!("{}/tests/fixtures/structured.ndjson", dir)).unwrap();
    let mut tmp = std::env::temp_dir();
    tmp.push("mgi-pulse-test-decompress.ndjson.gz");
    let f = std::fs::File::create(&tmp).unwrap();
    // Use the same flate2 crate the binary uses; it's already a build dep.
    let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    enc.write_all(&plain).unwrap();
    drop(enc);

    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(&tmp)
        .assert()
        .success()
        .stdout(contains("indexed 4 records"))
        .stdout(contains("untimed: 0"))
        .stdout(contains("json errors: 0"));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn theme_flag_accepts_known_values() {
    for theme in &["dark", "light", "nocolor", "mono"] {
        Command::cargo_bin("mgi-pulse")
            .unwrap()
            .arg("--dry-run")
            .arg(format!("--theme={}", theme))
            .arg(fixture("structured.ndjson"))
            .assert()
            .success();
    }
}

#[test]
fn unknown_theme_value_is_rejected() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--theme=neon")
        .arg(fixture("structured.ndjson"))
        .assert()
        .failure()
        .stderr(contains("unknown --theme"));
}

#[test]
fn unknown_format_value_is_rejected() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--format=protobuf")
        .arg(fixture("structured.ndjson"))
        .assert()
        .failure()
        .stderr(contains("unknown --format"));
}

#[test]
fn csv_with_header_indexes_ts_and_level() {
    // data.csv has 4 lines: 1 header + 3 data rows. After
    // capture_csv_headers the 3 data rows have their ts and level
    // resolved by column name, so untimed should be exactly 1 (the
    // header row itself).
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg("--format=csv")
        .arg(fixture("data.csv"))
        .assert()
        .success()
        .stdout(contains("indexed 4 records"))
        .stdout(contains("untimed: 1"));
}

#[test]
fn mostly_plain_minority_json_falls_into_less_mode() {
    // One JSON line out of seven → minority → has_timestamps() / has_severity()
    // both stay false → less-mode wins. The single JSON line still appears in
    // the table as raw, just no timeline or severity tabs.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("mostly-plain.log"))
        .assert()
        .success()
        .stdout(contains("indexed 7 records"))
        .stdout(contains("untimed: 7"));
}
