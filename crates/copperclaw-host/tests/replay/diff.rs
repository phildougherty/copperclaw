//! Substitution-aware JSONL diff for replay fixtures.
//!
//! The harness produces four streams of captured JSON values and the
//! fixture ships four matching streams of expected values. For each
//! stream, the diff:
//!
//! 1. Serializes both sides to canonical strings (`serde_json::to_string`
//!    with `preserve_order` so field order matches insertion order).
//! 2. Applies the manifest's substitutions (regex → replacement) to both
//!    sides in lexicographic-regex order.
//! 3. Parses the substituted strings back to `serde_json::Value` and
//!    walks the trees to report path-qualified mismatches.
//!
//! Substituting the serialized form (rather than walking the tree) keeps
//! the operator's mental model simple — they write the regex against the
//! same text they see in `expected/*.jsonl`.
#![allow(dead_code)]

use anyhow::Result;
use regex::Regex;
use std::collections::BTreeMap;

/// A single mismatch.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub stream: &'static str,
    pub path: String,
    pub expected: String,
    pub actual: String,
}

/// Diff report. `is_clean()` returns true when every stream matched
/// exactly after substitutions.
#[derive(Debug, Clone, Default)]
pub struct DiffReport {
    pub mismatches: Vec<Mismatch>,
}

impl DiffReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }

    pub fn extend(&mut self, other: DiffReport) {
        self.mismatches.extend(other.mismatches);
    }
}

impl std::fmt::Display for DiffReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.mismatches.is_empty() {
            return writeln!(f, "clean");
        }
        writeln!(f, "{} mismatch(es):", self.mismatches.len())?;
        for m in &self.mismatches {
            writeln!(
                f,
                "  [{}] {}\n      expected: {}\n      actual:   {}",
                m.stream, m.path, m.expected, m.actual
            )?;
        }
        Ok(())
    }
}

/// Pre-compiled substitution table. Construction validates the regex
/// patterns so the harness fails fast on a malformed manifest.
#[derive(Debug, Clone, Default)]
pub struct Substitutions {
    rules: Vec<(Regex, String)>,
}

impl Substitutions {
    pub fn compile(map: &BTreeMap<String, String>) -> Result<Self> {
        let mut rules = Vec::with_capacity(map.len());
        for (pat, repl) in map {
            let re = Regex::new(pat)?;
            rules.push((re, repl.clone()));
        }
        Ok(Self { rules })
    }

    pub fn apply(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (re, repl) in &self.rules {
            out = re.replace_all(&out, repl.as_str()).into_owned();
        }
        out
    }
}

/// Compare two streams of JSON values. Both sides are first stringified,
/// then substituted, then reparsed and walked. Length mismatches surface
/// as a single mismatch at the missing/extra index.
pub fn diff_stream(
    stream: &'static str,
    expected: &[serde_json::Value],
    actual: &[serde_json::Value],
    subs: &Substitutions,
) -> DiffReport {
    let mut report = DiffReport::default();
    let len = expected.len().max(actual.len());
    for i in 0..len {
        let exp_raw = expected
            .get(i)
            .map(|v| serde_json::to_string(v).expect("serialize expected"));
        let act_raw = actual
            .get(i)
            .map(|v| serde_json::to_string(v).expect("serialize actual"));
        match (exp_raw, act_raw) {
            (None, Some(a)) => {
                report.mismatches.push(Mismatch {
                    stream,
                    path: format!("[{i}]"),
                    expected: "<missing>".into(),
                    actual: subs.apply(&a),
                });
            }
            (Some(e), None) => {
                report.mismatches.push(Mismatch {
                    stream,
                    path: format!("[{i}]"),
                    expected: subs.apply(&e),
                    actual: "<missing>".into(),
                });
            }
            (Some(e), Some(a)) => {
                let exp_norm = subs.apply(&e);
                let act_norm = subs.apply(&a);
                let exp_v: serde_json::Value =
                    serde_json::from_str(&exp_norm).expect("reparse substituted expected");
                let act_v: serde_json::Value =
                    serde_json::from_str(&act_norm).expect("reparse substituted actual");
                walk(stream, &format!("[{i}]"), &exp_v, &act_v, &mut report);
            }
            (None, None) => unreachable!(),
        }
    }
    report
}

fn walk(
    stream: &'static str,
    path: &str,
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    report: &mut DiffReport,
) {
    use serde_json::Value;
    match (expected, actual) {
        (Value::Null, Value::Null) => {}
        (Value::Bool(le), Value::Bool(re)) if le == re => {}
        (Value::Number(le), Value::Number(re)) if le == re => {}
        (Value::String(le), Value::String(re)) if le == re => {}
        (Value::Array(la), Value::Array(ra)) => {
            walk_array(stream, path, la, ra, report);
        }
        (Value::Object(lo), Value::Object(ro)) => {
            walk_object(stream, path, lo, ro, report);
        }
        (left, right) => {
            report.mismatches.push(Mismatch {
                stream,
                path: path.to_string(),
                expected: left.to_string(),
                actual: right.to_string(),
            });
        }
    }
}

fn walk_array(
    stream: &'static str,
    path: &str,
    expected: &[serde_json::Value],
    actual: &[serde_json::Value],
    report: &mut DiffReport,
) {
    let n = expected.len().max(actual.len());
    for i in 0..n {
        let child = format!("{path}/{i}");
        match (expected.get(i), actual.get(i)) {
            (Some(le), Some(re)) => walk(stream, &child, le, re, report),
            (Some(le), None) => report.mismatches.push(Mismatch {
                stream,
                path: child,
                expected: le.to_string(),
                actual: "<missing>".into(),
            }),
            (None, Some(re)) => report.mismatches.push(Mismatch {
                stream,
                path: child,
                expected: "<missing>".into(),
                actual: re.to_string(),
            }),
            (None, None) => unreachable!(),
        }
    }
}

fn walk_object(
    stream: &'static str,
    path: &str,
    expected: &serde_json::Map<String, serde_json::Value>,
    actual: &serde_json::Map<String, serde_json::Value>,
    report: &mut DiffReport,
) {
    let mut keys: Vec<&String> = expected.keys().chain(actual.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let child = format!("{path}/{key}");
        match (expected.get(key), actual.get(key)) {
            (Some(le), Some(re)) => walk(stream, &child, le, re, report),
            (Some(le), None) => report.mismatches.push(Mismatch {
                stream,
                path: child,
                expected: le.to_string(),
                actual: "<missing>".into(),
            }),
            (None, Some(re)) => report.mismatches.push(Mismatch {
                stream,
                path: child,
                expected: "<missing>".into(),
                actual: re.to_string(),
            }),
            (None, None) => unreachable!(),
        }
    }
}
