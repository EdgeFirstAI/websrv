// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Pure, IO-free editing of systemd `EnvironmentFile` style configuration
//! files (`/etc/default/<service>`).
//!
//! The entry point is [`plan_edit`], which validates a submitted JSON map and
//! returns the complete prospective file content together with a per-key
//! [`Disposition`]. Nothing in this module touches the filesystem, which is
//! what makes all-or-nothing writes possible: on rejection the caller simply
//! never writes.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

/// What happened to one submitted key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Disposition {
    /// An active `KEY=` line was rewritten.
    Updated,
    /// A new active line was inserted below the last `#KEY=` line.
    Inserted,
    /// A new active line was appended at end of file. The key appeared
    /// nowhere in the original, active or commented.
    Appended,
    /// An active line was commented out, because the value was JSON null.
    Unset,
    /// The file already expresses this value; no line changed.
    Unchanged,
}

/// Why one submitted key was refused. Any rejection fails the whole request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Reject {
    /// The key is not a valid environment variable name.
    InvalidKey(String),
    /// The value cannot be represented in an `EnvironmentFile`.
    InvalidValue(String),
    /// The JSON type has no `EnvironmentFile` representation.
    UnsupportedType(String),
}

/// One validated entry, normalized to the form it will be written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Uppercased key, as it will appear in the file.
    pub key: String,
    /// `None` means unset: comment the key out.
    pub value: Option<String>,
}

/// Escape a value for inclusion inside a double-quoted `EnvironmentFile` value.
///
/// systemd unescapes `\"` to `"` and `\\` to `\` inside double quotes, and
/// needs no other escaping, so those two characters are the complete set.
#[allow(dead_code)]
pub(crate) fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// True when `key` is a valid environment variable name.
#[allow(dead_code)]
fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Render a JSON scalar as a string, or `None` if it is not a scalar.
#[allow(dead_code)]
fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Name a JSON type for use in rejection messages.
#[allow(dead_code)]
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Convert one JSON value into the string that will be written, or `None`
/// to unset the key.
#[allow(dead_code)]
fn coerce(value: &Value) -> Result<Option<String>, Reject> {
    match value {
        Value::Null => Ok(None),
        Value::Object(_) => Err(Reject::UnsupportedType(
            "objects have no EnvironmentFile representation".to_string(),
        )),
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                let Some(part) = scalar_to_string(item) else {
                    return Err(Reject::UnsupportedType(format!(
                        "array elements must be string, number or boolean, got {}",
                        type_name(item)
                    )));
                };
                if part.chars().any(char::is_whitespace) {
                    return Err(Reject::InvalidValue(format!(
                        "array element {part:?} contains whitespace and would not \
                         survive the round trip"
                    )));
                }
                parts.push(part);
            }
            Ok(Some(parts.join(" ")))
        }
        scalar => Ok(Some(
            scalar_to_string(scalar).expect("non-null, non-array, non-object is scalar"),
        )),
    }
}

/// Reject values that would corrupt the file if written.
///
/// A newline is the dangerous case: it would inject additional `KEY=VALUE`
/// lines into a file systemd feeds to services running as root.
#[allow(dead_code)]
fn check_writable(value: &str) -> Result<(), Reject> {
    if let Some(c) = value.chars().find(|c| c.is_control()) {
        return Err(Reject::InvalidValue(format!(
            "value contains control character U+{:04X}",
            c as u32
        )));
    }
    Ok(())
}

/// Validate and normalize every submitted entry.
///
/// Returns `Err` with a rejection for every offending key if any key is
/// invalid, so the caller can report all problems in a single response and
/// leave the file untouched.
#[allow(dead_code)]
pub(crate) fn validate(
    updates: &Map<String, Value>,
) -> Result<Vec<Entry>, BTreeMap<String, Reject>> {
    let mut entries = Vec::with_capacity(updates.len());
    let mut rejects = BTreeMap::new();
    // Uppercased key -> the original key that claimed it, for collision
    // reports.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();

    for (key, value) in updates {
        if !valid_key(key) {
            rejects.insert(
                key.clone(),
                Reject::InvalidKey("must match [A-Za-z_][A-Za-z0-9_]*".to_string()),
            );
            continue;
        }

        let upper = key.to_uppercase();
        if let Some(previous) = seen.get(&upper) {
            rejects.insert(
                key.clone(),
                Reject::InvalidKey(format!("collides with {previous:?} after uppercasing")),
            );
            rejects.insert(
                previous.clone(),
                Reject::InvalidKey(format!("collides with {key:?} after uppercasing")),
            );
            continue;
        }
        seen.insert(upper.clone(), key.clone());

        match coerce(value) {
            Err(reject) => {
                rejects.insert(key.clone(), reject);
            }
            Ok(None) => entries.push(Entry {
                key: upper,
                value: None,
            }),
            Ok(Some(rendered)) => match check_writable(&rendered) {
                Err(reject) => {
                    rejects.insert(key.clone(), reject);
                }
                Ok(()) => entries.push(Entry {
                    key: upper,
                    value: Some(rendered),
                }),
            },
        }
    }

    if rejects.is_empty() {
        Ok(entries)
    } else {
        Err(rejects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a `serde_json::Map` from a JSON object literal.
    fn map(v: Value) -> Map<String, Value> {
        v.as_object().expect("test input must be an object").clone()
    }

    fn ok(v: Value) -> Vec<Entry> {
        validate(&map(v)).expect("expected validation to succeed")
    }

    fn err(v: Value) -> BTreeMap<String, Reject> {
        validate(&map(v)).expect_err("expected validation to fail")
    }

    fn entry(key: &str, value: Option<&str>) -> Entry {
        Entry {
            key: key.to_string(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn strings_pass_through_and_keys_uppercase() {
        assert_eq!(
            ok(json!({ "rust_log": "debug" })),
            vec![entry("RUST_LOG", Some("debug"))]
        );
    }

    #[test]
    fn numbers_and_bools_are_coerced_not_blanked() {
        assert_eq!(
            ok(json!({ "A": 200, "B": 1.5, "C": true, "D": false })),
            vec![
                entry("A", Some("200")),
                entry("B", Some("1.5")),
                entry("C", Some("true")),
                entry("D", Some("false")),
            ]
        );
    }

    #[test]
    fn arrays_join_on_single_spaces() {
        assert_eq!(
            ok(json!({ "TF_VEC": ["0.1", "0", "-0.05"] })),
            vec![entry("TF_VEC", Some("0.1 0 -0.05"))]
        );
    }

    #[test]
    fn mixed_scalar_arrays_are_allowed() {
        assert_eq!(
            ok(json!({ "AZIMUTH": [0, 360] })),
            vec![entry("AZIMUTH", Some("0 360"))]
        );
    }

    #[test]
    fn empty_string_is_written_literally_not_unset() {
        assert_eq!(
            ok(json!({ "LIDAR_OUTPUT_TOPIC": "" })),
            vec![entry("LIDAR_OUTPUT_TOPIC", Some(""))]
        );
    }

    #[test]
    fn empty_array_becomes_empty_string() {
        assert_eq!(ok(json!({ "A": [] })), vec![entry("A", Some(""))]);
    }

    #[test]
    fn null_is_the_only_unset() {
        assert_eq!(ok(json!({ "TARGET": null })), vec![entry("TARGET", None)]);
    }

    #[test]
    fn objects_are_unsupported() {
        let r = err(json!({ "A": { "nested": 1 } }));
        assert!(matches!(r["A"], Reject::UnsupportedType(_)));
    }

    #[test]
    fn nested_arrays_are_unsupported() {
        let r = err(json!({ "A": [["x"]] }));
        assert!(matches!(r["A"], Reject::UnsupportedType(_)));
    }

    #[test]
    fn array_elements_with_whitespace_are_rejected() {
        let r = err(json!({ "A": ["a b"] }));
        assert!(matches!(r["A"], Reject::InvalidValue(_)));
    }

    #[test]
    fn newlines_and_control_characters_are_rejected() {
        for bad in ["info\nEVIL=pwned", "a\rb", "a\u{0}b", "a\tb"] {
            let r = err(json!({ "A": bad }));
            assert!(
                matches!(r["A"], Reject::InvalidValue(_)),
                "expected {bad:?} to be rejected"
            );
        }
    }

    /// Build a single-entry map. Used where the key is a variable, which the
    /// `json!` macro cannot take in key position.
    fn one(key: &str, value: Value) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(key.to_string(), value);
        m
    }

    #[test]
    fn invalid_key_names_are_rejected() {
        for bad in ["BAD KEY", "1LEADING", "has-dash", "", "a.b"] {
            let r = validate(&one(bad, json!("x"))).expect_err("expected rejection");
            assert!(
                matches!(r[bad], Reject::InvalidKey(_)),
                "expected key {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn keys_differing_only_in_case_reject_both() {
        let r = err(json!({ "MODE": "peer", "mode": "client" }));
        assert!(matches!(r["MODE"], Reject::InvalidKey(_)));
        assert!(matches!(r["mode"], Reject::InvalidKey(_)));
    }

    #[test]
    fn every_bad_key_is_reported_in_one_pass() {
        let r = err(json!({ "GOOD": "x", "BAD KEY": "y", "ALSO": { "o": 1 } }));
        assert_eq!(r.len(), 2, "only the two bad keys should be reported");
        assert!(!r.contains_key("GOOD"));
    }

    #[test]
    fn escaping_covers_backslash_and_quote_only() {
        assert_eq!(escape_value(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_value(r"a\b"), r"a\\b");
        assert_eq!(escape_value("plain 0.1 0"), "plain 0.1 0");
    }
}
