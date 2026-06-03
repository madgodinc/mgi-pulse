//! Mini query DSL — compiles a one-line expression into a predicate
//! tree.
//!
//! Grammar:
//!
//! ```text
//! expr    := or_expr
//! or_expr := and_expr ("OR" and_expr)*
//! and_expr:= unary ("AND" unary)*
//! unary   := "NOT" unary | atom
//! atom    := "(" expr ")" | clause
//! clause  := field op value
//! op      := "=" | "!=" | "~" | ">" | ">=" | "<" | "<="
//! value   := /[^ ]+/   |   "\"" [^"]* "\""   |   "/" [^/]* "/"
//! ```
//!
//! Precedence: `NOT` binds tightest, then `AND`, then `OR`. Parens
//! override. Operator keywords are case-sensitive ASCII (`AND` / `OR`
//! / `NOT` — lowercase would be ambiguous with field names like
//! `and_count`).
//!
//! Examples:
//!
//! ```text
//! level=error
//! level=error AND msg~/timeout/
//! (level=error OR level=warn) AND NOT logger=health-check
//! level=error AND (msg~/timeout/ OR msg~/refused/)
//! ts>=2026-06-01 AND level=error
//! ```
//!
//! Operators map to:
//! - `=` and `!=` → `FieldEqualsPredicate` (and a `Not` wrapper for `!=`).
//! - `~` → `FieldRegexPredicate` (regex against the projected field).
//! - `>`, `>=`, `<`, `<=` apply only to `ts` and compile to
//!   `TimeRangePredicate`. Other fields with a comparison op are a
//!   parse error.

use crate::engine::parse::parse_rfc3339_micros;
use crate::engine::predicate::{
    AndPredicate, FieldEqualsPredicate, FieldRegexPredicate, NotPredicate, OrPredicate, Predicate,
    TimeRangePredicate,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Regex,
    Gt,
    Ge,
    Lt,
    Le,
}

/// Compile `source` into a boxed predicate tree. Returns a clear error
/// when the input doesn't match the grammar.
pub fn compile(source: &str) -> Result<Box<dyn Predicate>, String> {
    let mut tokens = Tokenizer::new(source);
    if tokens.is_empty() {
        return Err("empty query".to_string());
    }
    let pred = parse_or(&mut tokens)?;
    if !tokens.is_empty() {
        return Err(format!("unexpected trailing input: `{}`", tokens.rest()));
    }
    Ok(pred)
}

/// or_expr := and_expr ("OR" and_expr)*
fn parse_or(tokens: &mut Tokenizer) -> Result<Box<dyn Predicate>, String> {
    let first = parse_and(tokens)?;
    if !matches!(tokens.peek_kw(), Some("OR")) {
        return Ok(first);
    }
    let mut or = OrPredicate::new();
    or.push(first);
    while matches!(tokens.peek_kw(), Some("OR")) {
        tokens.consume_kw("OR");
        let next = parse_and(tokens)?;
        or.push(next);
    }
    Ok(Box::new(or))
}

/// and_expr := unary ("AND" unary)*
fn parse_and(tokens: &mut Tokenizer) -> Result<Box<dyn Predicate>, String> {
    let first = parse_unary(tokens)?;
    if !matches!(tokens.peek_kw(), Some("AND")) {
        return Ok(first);
    }
    let mut and = AndPredicate::new();
    and.push(first);
    while matches!(tokens.peek_kw(), Some("AND")) {
        tokens.consume_kw("AND");
        let next = parse_unary(tokens)?;
        and.push(next);
    }
    Ok(Box::new(and))
}

/// unary := "NOT" unary | atom
fn parse_unary(tokens: &mut Tokenizer) -> Result<Box<dyn Predicate>, String> {
    if matches!(tokens.peek_kw(), Some("NOT")) {
        tokens.consume_kw("NOT");
        let inner = parse_unary(tokens)?;
        return Ok(Box::new(NotPredicate::new(inner)));
    }
    parse_atom(tokens)
}

/// atom := "(" expr ")" | clause
fn parse_atom(tokens: &mut Tokenizer) -> Result<Box<dyn Predicate>, String> {
    tokens.skip_ws();
    if tokens.peek_byte() == Some(b'(') {
        tokens.advance(1);
        let inner = parse_or(tokens)?;
        tokens.skip_ws();
        if tokens.peek_byte() != Some(b')') {
            return Err(format!(
                "expected `)` to close group, got `{}`",
                tokens.rest()
            ));
        }
        tokens.advance(1);
        return Ok(inner);
    }
    parse_clause(tokens)
}

fn parse_clause(tokens: &mut Tokenizer) -> Result<Box<dyn Predicate>, String> {
    let field = tokens.read_field()?;
    let op = tokens.read_op()?;
    let value = tokens.read_value(op)?;
    build_predicate(&field, op, &value)
}

fn build_predicate(field: &str, op: Op, value: &str) -> Result<Box<dyn Predicate>, String> {
    match op {
        Op::Eq => Ok(Box::new(FieldEqualsPredicate::new(
            field.to_string(),
            value.to_string(),
        ))),
        Op::Ne => {
            let inner = FieldEqualsPredicate::new(field.to_string(), value.to_string());
            Ok(Box::new(NotPredicate::new(Box::new(inner))))
        }
        Op::Regex => {
            let re = FieldRegexPredicate::new(field.to_string(), value)
                .map_err(|e| format!("regex error on `{}`: {}", field, e))?;
            Ok(Box::new(re))
        }
        Op::Gt | Op::Ge | Op::Lt | Op::Le => {
            if field != "ts" {
                return Err(format!(
                    "comparison operators are only supported on `ts`, got `{}`",
                    field
                ));
            }
            let ts = parse_partial_rfc3339(value)
                .ok_or_else(|| format!("could not parse timestamp `{}`", value))?;
            let pred = match op {
                Op::Gt => TimeRangePredicate::greater_than(ts),
                Op::Ge => TimeRangePredicate::at_or_after(ts),
                Op::Lt => TimeRangePredicate::less_than(ts),
                Op::Le => TimeRangePredicate::at_or_before(ts),
                _ => unreachable!(),
            };
            Ok(Box::new(pred))
        }
    }
}

/// Accept any RFC3339 prefix and pad to a full timestamp before parsing.
/// Same helper logic as the App's `t` jump prompt.
fn parse_partial_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let padded: String = match s.len() {
        4 => format!("{}-01-01T00:00:00Z", s),
        7 => format!("{}-01T00:00:00Z", s),
        10 => format!("{}T00:00:00Z", s),
        13 => format!("{}:00:00Z", s),
        16 => format!("{}:00Z", s),
        19 => format!("{}Z", s),
        _ => s.to_string(),
    };
    parse_rfc3339_micros(&padded)
}

struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Self {
        let mut t = Self { src, pos: 0 };
        t.skip_ws();
        t
    }

    fn is_empty(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.src.len()
    }

    fn rest(&self) -> &str {
        &self.src[self.pos..]
    }

    fn skip_ws(&mut self) {
        let bytes = self.src.as_bytes();
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek_byte(&mut self) -> Option<u8> {
        self.skip_ws();
        self.src.as_bytes().get(self.pos).copied()
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
        self.skip_ws();
    }

    /// Peek the next keyword (alphabetic run), uppercased ASCII. Doesn't
    /// advance the cursor.
    fn peek_kw(&self) -> Option<&str> {
        let rest = self.rest().trim_start();
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        if end == 0 {
            None
        } else {
            Some(&rest[..end])
        }
    }

    fn consume_kw(&mut self, expected: &str) {
        self.skip_ws();
        self.pos += expected.len();
        self.skip_ws();
    }

    fn read_field(&mut self) -> Result<String, String> {
        self.skip_ws();
        // Boolean keywords can't be field names — they'd shadow operators
        // and produce confusing error messages downstream. Surface the
        // collision here, where we have token context.
        if let Some(kw) = self.peek_kw() {
            if matches!(kw, "AND" | "OR" | "NOT") {
                return Err(format!("unexpected `{}` — expected a field name", kw));
            }
        }
        let bytes = self.src.as_bytes();
        let start = self.pos;
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'@' || b == b'-' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(format!("expected field name at `{}`", self.rest()));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn read_op(&mut self) -> Result<Op, String> {
        self.skip_ws();
        let rest = self.rest();
        let bytes = rest.as_bytes();
        if bytes.is_empty() {
            return Err("expected operator".to_string());
        }
        let (op, len) = match bytes[0] {
            b'=' => (Op::Eq, 1),
            b'~' => (Op::Regex, 1),
            b'!' if bytes.get(1) == Some(&b'=') => (Op::Ne, 2),
            b'>' if bytes.get(1) == Some(&b'=') => (Op::Ge, 2),
            b'<' if bytes.get(1) == Some(&b'=') => (Op::Le, 2),
            b'>' => (Op::Gt, 1),
            b'<' => (Op::Lt, 1),
            _ => {
                return Err(format!("expected operator at `{}`", rest));
            }
        };
        self.pos += len;
        Ok(op)
    }

    fn read_value(&mut self, op: Op) -> Result<String, String> {
        self.skip_ws();
        let bytes = self.src.as_bytes();
        if self.pos >= bytes.len() {
            return Err("expected value".to_string());
        }
        // Regex syntax: /pattern/. We consume up to the closing `/`.
        if op == Op::Regex && bytes[self.pos] == b'/' {
            self.pos += 1;
            let start = self.pos;
            while self.pos < bytes.len() && bytes[self.pos] != b'/' {
                if bytes[self.pos] == b'\\' && self.pos + 1 < bytes.len() {
                    self.pos += 2;
                } else {
                    self.pos += 1;
                }
            }
            let value = &self.src[start..self.pos];
            if self.pos < bytes.len() {
                self.pos += 1; // skip closing /
            }
            return Ok(value.to_string());
        }
        // Quoted string.
        if bytes[self.pos] == b'"' {
            self.pos += 1;
            let start = self.pos;
            while self.pos < bytes.len() && bytes[self.pos] != b'"' {
                if bytes[self.pos] == b'\\' && self.pos + 1 < bytes.len() {
                    self.pos += 2;
                } else {
                    self.pos += 1;
                }
            }
            let value = &self.src[start..self.pos];
            if self.pos < bytes.len() {
                self.pos += 1;
            }
            return Ok(value.to_string());
        }
        // Bare token — runs until whitespace or a closing paren. The
        // paren stop is what lets `(level=error OR level=warn)` parse
        // the second `warn` as `warn`, not `warn)`. Use `"warn)"` (a
        // quoted value) if a literal trailing paren is needed.
        let start = self.pos;
        while self.pos < bytes.len()
            && !bytes[self.pos].is_ascii_whitespace()
            && bytes[self.pos] != b')'
        {
            self.pos += 1;
        }
        Ok(self.src[start..self.pos].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiles(input: &str) -> bool {
        compile(input).is_ok()
    }

    #[test]
    fn empty_query_is_rejected() {
        assert!(compile("").is_err());
    }

    #[test]
    fn single_field_equals() {
        assert!(compiles("level=error"));
    }

    #[test]
    fn quoted_value() {
        assert!(compiles(r#"msg="hello world""#));
    }

    #[test]
    fn regex_value() {
        assert!(compiles("msg~/db timeout/"));
    }

    #[test]
    fn and_composition() {
        assert!(compiles("level=error AND msg~/boom/"));
        assert!(compiles("logger=app AND level=error AND msg~/timeout/"));
    }

    #[test]
    fn comparison_on_ts() {
        assert!(compiles("ts>2026-06-01"));
        assert!(compiles("ts>=2026-06-01T12:00 AND ts<2026-06-01T13:00"));
    }

    #[test]
    fn comparison_on_non_ts_is_rejected() {
        let err = compile("level>error").err().unwrap();
        assert!(err.contains("only supported on `ts`"));
    }

    #[test]
    fn invalid_regex_is_rejected() {
        // Unbalanced `[` is invalid.
        let err = compile("msg~/[unbalanced/").err().unwrap();
        assert!(err.contains("regex error"));
    }

    #[test]
    fn time_prefix_is_padded() {
        // ts>2026 should pad to 2026-01-01T00:00:00Z and parse.
        assert!(compiles("ts>2026"));
        // ts>not-a-date should error.
        let err = compile("ts>not-a-date").err().unwrap();
        assert!(err.contains("parse timestamp"));
    }

    // --- Boolean composition (OR / NOT / parens) ---

    use crate::engine::format::{FieldCache, LogFormat};
    use crate::engine::record::{severity, RawRecord, RecordBytes};

    fn rec_with_severity(sev: u8) -> RawRecord {
        RawRecord {
            source_id: 0,
            line_id: 0,
            ts_micros: 0,
            severity: sev,
            bytes: RecordBytes::Owned(Box::from([])),
        }
    }

    fn matches(query: &str, line: &[u8]) -> bool {
        let p = compile(query).expect("query should compile");
        let r = rec_with_severity(severity::INFO);
        let mut cache = FieldCache::new(LogFormat::Ndjson, line);
        p.matches(&r, &mut cache)
    }

    #[test]
    fn or_composes() {
        assert!(compiles("level=error OR level=warn"));
        let err_line = br#"{"level":"error","msg":"x"}"#;
        let warn_line = br#"{"level":"warn","msg":"x"}"#;
        let info_line = br#"{"level":"info","msg":"x"}"#;
        assert!(matches("level=error OR level=warn", err_line));
        assert!(matches("level=error OR level=warn", warn_line));
        assert!(!matches("level=error OR level=warn", info_line));
    }

    #[test]
    fn not_negates_clause() {
        assert!(compiles("NOT level=error"));
        let err_line = br#"{"level":"error","msg":"x"}"#;
        let info_line = br#"{"level":"info","msg":"x"}"#;
        assert!(!matches("NOT level=error", err_line));
        assert!(matches("NOT level=error", info_line));
    }

    #[test]
    fn parens_group() {
        assert!(compiles("(level=error OR level=warn) AND msg~/boom/"));
        let line_err_boom = br#"{"level":"error","msg":"boom"}"#;
        let line_warn_boom = br#"{"level":"warn","msg":"boom"}"#;
        let line_info_boom = br#"{"level":"info","msg":"boom"}"#;
        let line_err_quiet = br#"{"level":"error","msg":"quiet"}"#;
        let q = "(level=error OR level=warn) AND msg~/boom/";
        assert!(matches(q, line_err_boom));
        assert!(matches(q, line_warn_boom));
        assert!(!matches(q, line_info_boom));
        assert!(!matches(q, line_err_quiet));
    }

    #[test]
    fn precedence_not_tighter_than_and() {
        // `NOT a AND b` parses as `(NOT a) AND b`, not `NOT (a AND b)`.
        let line = br#"{"level":"info","logger":"app","msg":"x"}"#;
        // info AND logger=app → false (NOT level=error) AND logger=app → true
        assert!(matches("NOT level=error AND logger=app", line));
    }

    #[test]
    fn precedence_and_tighter_than_or() {
        // `a OR b AND c` parses as `a OR (b AND c)`.
        // line is level=info logger=app → first arm fails (level!=error),
        // second arm: level=info AND logger=app → succeeds.
        let line = br#"{"level":"info","logger":"app"}"#;
        assert!(matches(
            "level=error OR level=info AND logger=app",
            line
        ));
        // logger mismatch breaks the AND arm; OR arm also fails.
        let line2 = br#"{"level":"info","logger":"other"}"#;
        assert!(!matches(
            "level=error OR level=info AND logger=app",
            line2
        ));
    }

    #[test]
    fn parens_override_default_precedence() {
        // (a OR b) AND c — distinct from a OR (b AND c).
        let line = br#"{"level":"warn","logger":"app"}"#;
        assert!(matches(
            "(level=error OR level=warn) AND logger=app",
            line
        ));
        // line is logger=other → AND arm fails on both sides.
        let line2 = br#"{"level":"warn","logger":"other"}"#;
        assert!(!matches(
            "(level=error OR level=warn) AND logger=app",
            line2
        ));
    }

    #[test]
    fn unbalanced_parens_rejected() {
        let err = compile("(level=error").err().unwrap();
        assert!(err.contains("expected `)`"));
    }

    #[test]
    fn dangling_operator_rejected() {
        let err = compile("level=error AND").err().unwrap();
        assert!(err.contains("expected"));
    }

    #[test]
    fn double_not_collapses_logically() {
        // NOT NOT a should match the same things as a.
        let line = br#"{"level":"error"}"#;
        assert_eq!(
            matches("level=error", line),
            matches("NOT NOT level=error", line)
        );
    }

    #[test]
    fn lowercase_and_is_not_an_operator() {
        // `and` lowercase is a field name, not the AND keyword. This
        // protects fields like `and_count` and matches the explicit
        // case-sensitivity in the grammar doc.
        let err = compile("level=error and level=warn").err().unwrap();
        // It tries to read `and` as a field, then expects an operator
        // after `level=warn`'s second clause — either way it errors.
        assert!(!err.is_empty());
    }

    #[test]
    fn trailing_garbage_rejected() {
        let err = compile("level=error )").err().unwrap();
        assert!(err.contains("trailing"));
    }
}
