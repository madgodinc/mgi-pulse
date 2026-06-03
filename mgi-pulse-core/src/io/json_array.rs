//! JSON-array input adapter.
//!
//! Some exports (REST API responses, BigQuery dumps, audit log
//! exports) come as a single top-level JSON array of objects:
//!
//! ```json
//! [
//!   {"ts":"2026-06-01T12:00:00Z","level":"info","msg":"a"},
//!   {"ts":"2026-06-01T12:00:01Z","level":"warn","msg":"b"}
//! ]
//! ```
//!
//! We re-emit those as NDJSON (one JSON object per line) so the rest
//! of the engine indexes them the same way it indexes a real NDJSON
//! file.
//!
//! ## Memory cost — read this before opening big files
//!
//! This adapter is NOT a streaming parser. The whole file is loaded
//! and `serde_json::from_slice::<Value>` materialises an owned
//! `Value` tree — every object becomes a `Map<String, Value>` with
//! heap-allocated keys and values. Then `to_writer` re-serialises
//! that tree into a second owned buffer.
//!
//! Realistic peak RSS = `raw bytes + owned Value tree (~3-5× of raw)
//! + NDJSON output (~raw)`. A 100 MB array can reach ~500 MB-1 GB
//! resident before the engine even starts indexing. A 200 MB array
//! is on the edge of OOM on a 4 GB machine.
//!
//! The hard cap in `ingest_file` is set at 64 MB for this reason —
//! anything bigger gets a clear "use `jq -c '.[]' file.json |
//! mgi-pulse -`" message. A proper streaming parser
//! (`StreamDeserializer` / `jiter`) could lift the cap to "limited
//! by disk", but it's deferred until the use case shows up — the
//! `jq` workaround covers the gap.
//!
//! Detection: the first non-whitespace byte is `[` and the second
//! non-whitespace byte is `{` or `]`. Anything else hands back to
//! the original NDJSON path.

/// Detect whether the file at `bytes` looks like a JSON array of
/// objects. Cheap byte scan, bounded by the probe window.
pub fn looks_like_json_array(bytes: &[u8]) -> bool {
    let mut i = 0;
    // Skip leading whitespace.
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'[' {
        return false;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // The next non-whitespace must be `{` (object inside) or `]` (empty array).
    matches!(bytes.get(i), Some(&b'{') | Some(&b']'))
}

/// Convert a top-level JSON-array body into newline-separated JSON
/// objects. Returns an owned `Vec<u8>` because the records are
/// re-encoded; the original `bytes` can be dropped after the call.
///
/// On any parse error the result is `Err(String)` with a hint about
/// the offending position. The caller can choose to fall back to
/// treating the file as raw NDJSON (which will probably error too,
/// but at the user's chosen pipeline rather than here).
pub fn flatten_to_ndjson(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let val: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("JSON array parse error: {}", e))?;
    let arr = match val {
        serde_json::Value::Array(a) => a,
        _ => return Err("top-level value is not an array".to_string()),
    };
    let mut out = Vec::with_capacity(bytes.len());
    for el in arr {
        // Only objects make sense as log records. Skip primitives /
        // nested arrays — they'd index as parse errors anyway.
        if !el.is_object() {
            continue;
        }
        serde_json::to_writer(&mut out, &el)
            .map_err(|e| format!("serialise array element: {}", e))?;
        out.push(b'\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_array_of_objects() {
        assert!(looks_like_json_array(b"[{\"a\":1}]"));
        assert!(looks_like_json_array(b"  [\n  {\"a\":1}"));
        assert!(looks_like_json_array(b"[]"));
    }

    #[test]
    fn rejects_ndjson_and_plain_text() {
        assert!(!looks_like_json_array(b"{\"a\":1}\n{\"b\":2}"));
        assert!(!looks_like_json_array(b"plain text"));
        // Array of strings is not a recognised array-of-objects shape
        // (we want logging records). The leading `[` followed by `"`
        // currently returns false, which is the conservative choice.
        assert!(!looks_like_json_array(b"[\"hello\"]"));
    }

    #[test]
    fn flattens_array_to_ndjson_lines() {
        let input =
            b"[{\"ts\":\"2026-06-01T12:00:00Z\",\"level\":\"info\",\"msg\":\"a\"},\
              {\"ts\":\"2026-06-01T12:00:01Z\",\"level\":\"warn\",\"msg\":\"b\"}]";
        let out = flatten_to_ndjson(input).unwrap();
        // Two newline-terminated records.
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 2);
        // Each record is parseable JSON.
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let v: serde_json::Value = serde_json::from_slice(line).unwrap();
            assert!(v.is_object());
        }
    }

    #[test]
    fn empty_array_produces_empty_output() {
        let out = flatten_to_ndjson(b"[]").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn malformed_array_returns_error() {
        let out = flatten_to_ndjson(b"[{,}]");
        assert!(out.is_err());
    }

    #[test]
    fn non_array_returns_error() {
        let out = flatten_to_ndjson(b"{\"a\":1}");
        assert!(out.is_err());
    }

    #[test]
    fn non_object_elements_dropped() {
        // String elements aren't log records; they get skipped, the
        // object element survives.
        let out = flatten_to_ndjson(b"[\"junk\", {\"a\":1}, 42]").unwrap();
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1);
    }
}
