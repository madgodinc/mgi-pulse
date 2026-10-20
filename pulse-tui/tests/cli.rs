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
fn level_tagged_text_is_plain_not_json() {
    // `[INFO] ...` lines are read by the plain-text parser: no JSON
    // errors, no timestamps, no auto-columns, but levels.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("plain.log"))
        .assert()
        .success()
        .stdout(contains("indexed 5 records"))
        .stdout(contains("untimed: 5"))
        .stdout(contains("json errors: 0"))
        .stdout(contains("auto-columns: []"));
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args(["watch", "--once"])
        .arg(fixture("plain.log"))
        .assert()
        .success()
        .stdout(contains("error plain.log [errors] DB timeout"));
}

#[test]
fn timestamp_level_text_is_parsed() {
    let dir = std::env::temp_dir().join(format!("mgi-pulse-plain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plain.log");
    std::fs::write(
        &path,
        "2026-10-21T02:30:00+03:00 ERROR pulse: plain line\n\
         2026-10-21 02:30:01,123 ERROR [main] pulse: log4j style\n\
         2026/10/21 02:30:02 [warn] 7#0: nginx said\n\
         2026/10/21 02:30:03 go log line\n",
    )
    .unwrap();
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(&path)
        .assert()
        .success()
        .stdout(contains("untimed: 0"))
        .stdout(contains("json errors: 0"));
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args(["watch", "--once", "--from-start", "--collapse", "0"])
        .arg(&path)
        .assert()
        .success()
        .stdout(
            "2026-10-20T23:30:00.000Z error plain.log [errors] plain line\n\
             2026-10-21T02:30:01.123Z error plain.log [errors] log4j style\n",
        );
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args(["watch", "--once", "--rule", "level=warn"])
        .arg(&path)
        .assert()
        .success()
        .stdout(contains("warn  plain.log [rule1] 7#0: nginx said"));
    let _ = std::fs::remove_dir_all(&dir);
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
fn logfmt_without_flag_is_auto_detected() {
    // Auto-detect: the logfmt fixture's `key=value` shape gets the
    // logfmt vote, no --format needed. Previously this test asserted
    // the opposite (every line a JSON parse failure) because the
    // wire from LogFormat::detect to the CLI didn't exist yet.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("logfmt.log"))
        .assert()
        .success()
        .stdout(contains("indexed 5 records"))
        .stdout(contains("untimed: 0"));
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
fn edn_without_flag_is_auto_detected() {
    // EDN signature: `{:keyword value ...}` — the auto-detector picks
    // it up from the first lines and parses every record cleanly.
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("--dry-run")
        .arg(fixture("clojure.edn"))
        .assert()
        .success()
        .stdout(contains("indexed 4 records"))
        .stdout(contains("untimed: 0"));
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

#[test]
fn watch_once_prints_only_errors() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args(["watch", "--once"])
        .arg(fixture("structured.ndjson"))
        .assert()
        .success()
        .stdout("2026-06-01T12:00:02.000Z error structured.ndjson [errors] boom\n");
}

#[test]
fn watch_reads_journal_json_from_stdin_and_collapses() {
    let mut input = String::new();
    for i in 0..5 {
        input.push_str(&format!(
            "{{\"__REALTIME_TIMESTAMP\":\"17172354{i}0000000\",\"PRIORITY\":\"3\",\"MESSAGE\":\"disk {i} failed\",\"_SYSTEMD_UNIT\":\"raid.service\"}}\n"
        ));
    }
    input.push_str(
        "{\"__REALTIME_TIMESTAMP\":\"1717235499000000\",\"PRIORITY\":\"6\",\"MESSAGE\":\"fine\"}\n",
    );
    let out = Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args([
            "watch",
            "-",
            "--format",
            "json",
            "--rule",
            "unit=raid.service",
        ])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines: Vec<serde_json::Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["type"], "match");
    assert_eq!(lines[0]["msg"], "disk 0 failed");
    assert_eq!(lines[1]["type"], "repeat");
    assert_eq!(lines[1]["count"], 4);
}

#[test]
fn watch_bad_rule_exits_nonzero() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args(["watch", "--once", "--rule", "(level=error"])
        .arg(fixture("structured.ndjson"))
        .assert()
        .failure()
        .stderr(contains("rule `rule1`"));
}
