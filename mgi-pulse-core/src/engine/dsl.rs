//! Mini query DSL — compiles a one-line expression into an
//! `AndPredicate`.
//!
//! Grammar (v0.2):
//!
//! ```text
//! query  := clause (WS+ "AND" WS+ clause)*
//! clause := field op value
//! op     := "=" | "!=" | "~" | ">" | ">=" | "<" | "<="
//! value  := /[^ ]+/   |   "\"" [^"]* "\""   |   "/" [^/]* "/"
//! ```
//!
//! Examples:
//!
//! ```text
//! level=error
//! level=error AND msg~/timeout/
//! ts>=2026-06-01 AND level=error
//! logger=my.app AND msg~/conn(ection)? lost/
//! ```
//!
//! Operators map to:
//! - `=` and `!=` → `FieldEqualsPredicate` (and a `Not` wrapper for `!=`).
//! - `~` → `FieldRegexPredicate` (regex against the projected field).
//! - `>`, `>=`, `<`, `<=` apply only to `ts` and compile to
//!   `TimeRangePredicate`. Other fields with a comparison op are a
//!   parse error in v0.2.
//!
//! Backlog: parentheses, `OR`, `NOT`, numeric comparisons on arbitrary
//! fields.

use crate::engine::parse::parse_rfc3339_micros;
use crate::engine::predicate::{
    AndPredicate, FieldEqualsPredicate, FieldRegexPredicate, NotPredicate, Predicate,
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

/// Compile `source` into a boxed `AndPredicate`. Returns a clear error
/// when the input doesn't match the grammar.
pub fn compile(source: &str) -> Result<Box<dyn Predicate>, String> {
    let mut and = AndPredicate::new();
    let mut tokens = Tokenizer::new(source);
    let mut first = true;
    loop {
        // After the first clause, require AND between clauses.
        if !first {
            match tokens.peek_kw() {
                Some("AND") => {
                    tokens.consume_kw("AND");
                }
                Some(other) => {
                    return Err(format!("expected `AND` between clauses, got `{}`", other));
                }
                None => break,
            }
        }
        if tokens.is_empty() {
            if first {
                return Err("empty query".to_string());
            }
            break;
        }
        let clause = parse_clause(&mut tokens)?;
        and.push(clause);
        first = false;
    }
    Ok(Box::new(and))
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

    fn is_empty(&self) -> bool {
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
        // Bare token — runs until whitespace.
        let start = self.pos;
        while self.pos < bytes.len() && !bytes[self.pos].is_ascii_whitespace() {
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
    fn bad_operator_after_clause_is_rejected() {
        let err = compile("level=error OR level=warn").err().unwrap();
        assert!(err.contains("expected `AND`"));
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
}
