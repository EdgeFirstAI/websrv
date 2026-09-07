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

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
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
#[serde(rename_all = "snake_case")]
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
fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Upper bound on a submitted key's length.
///
/// `locate` compiles a `Regex` from every key via `regex::escape`, which only
/// guarantees the pattern parses, not that it compiles: `Regex::new` returns
/// `Err(CompiledTooBig)` once the resulting NFA exceeds regex's 10 MB default
/// size limit, which a single ~200 000-character key is enough to trigger.
/// This bound keeps the compiled matcher well clear of that ceiling; the
/// longest key in any shipped `.default` is well under 40 characters.
const MAX_KEY_LEN: usize = 128;

/// Render a JSON scalar as a string, or `None` if it is not a scalar.
fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Name a JSON type for use in rejection messages.
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
pub(crate) fn validate(
    updates: &Map<String, Value>,
) -> Result<Vec<Entry>, BTreeMap<String, Reject>> {
    let mut entries = Vec::with_capacity(updates.len());
    let mut rejects = BTreeMap::new();
    // Uppercased key -> the original key that claimed it, for collision
    // reports.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();

    for (key, value) in updates {
        if key.len() > MAX_KEY_LEN {
            rejects.insert(
                key.clone(),
                Reject::InvalidKey(format!("key exceeds the {MAX_KEY_LEN}-character limit")),
            );
            continue;
        }

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

/// Undo [`escape_value`] on the inner text of a quoted value.
///
/// This is the exact inverse of `escape_value`, which prefixes a backslash to
/// exactly two characters (`\` and `"`) and nothing else. Consuming `\\` and
/// `\"` in a single left-to-right pass is what the round trip requires; two
/// sequential `str::replace` calls would re-process their own output and get
/// the wrong answer on inputs like `\\"`.
///
/// Any other `\X` sequence (e.g. `\n`, `\t`) is left untouched, as a literal
/// backslash followed by that character. systemd itself may interpret other
/// C-style escapes on read, but this parser has never modelled those, and
/// widening it here would change how existing hand-written files are read —
/// out of scope for this fix.
fn unescape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next @ ('\\' | '"')) = chars.clone().next() {
                out.push(next);
                chars.next();
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Parse config file content into a JSON map.
///
/// Values that are whitespace-separated and unquoted become JSON arrays;
/// [`plan_edit`] joins arrays back with single spaces, so the two are
/// inverses.
pub fn parse_config_content(content: &str) -> Map<String, Value> {
    let mut config_map = Map::new();

    for line in content.lines() {
        let line = line.trim();
        // `man systemd.exec`: in an EnvironmentFile, "lines starting with
        // \";\" or \"#\" will be ignored". Both must be skipped here, not
        // just `#`: a `;`-commented line otherwise parses into a key like
        // ";OLD_KEY", and posting the GET response back would fail the whole
        // all-or-nothing save on a key `validate` rejects.
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let clean_key = key.trim();
            let raw_value = value.trim();

            // If the value is quoted, treat it as a single string value
            if raw_value.starts_with('"') && raw_value.ends_with('"') && raw_value.len() >= 2 {
                let unquoted = &raw_value[1..raw_value.len() - 1];
                config_map.insert(
                    clean_key.to_string(),
                    Value::String(unescape_value(unquoted)),
                );
            } else {
                // Unquoted: split on whitespace for multiple values
                let clean_value = raw_value.replace("\"", "");
                let parts: Vec<&str> = clean_value.split_whitespace().collect();

                if parts.len() > 1 {
                    config_map.insert(
                        clean_key.to_string(),
                        Value::Array(parts.iter().map(|s| Value::String(s.to_string())).collect()),
                    );
                } else {
                    config_map.insert(
                        clean_key.to_string(),
                        Value::String(clean_value.to_string()),
                    );
                }
            }
        }
    }

    config_map
}

/// Comment introducing keys appended because they appeared nowhere in the file.
pub const APPEND_MARKER: &str = "# --- Added by edgefirst-websrv ---";

/// A complete prospective edit of a configuration file.
///
/// Holding the whole new content rather than mutating a file is what makes
/// all-or-nothing writes trivial: the caller writes `content` or does nothing.
#[derive(Debug, Clone)]
pub struct EditPlan {
    /// What happened to each submitted key.
    pub dispositions: BTreeMap<String, Disposition>,
    /// Keys appended because they appeared nowhere in the original file,
    /// active or commented. Exactly the keys dispositioned
    /// [`Disposition::Appended`], surfaced as a set so callers can log and
    /// warn without walking the disposition map.
    pub unmatched: BTreeSet<String>,
    /// The complete new file content.
    pub content: String,
    /// False when `content` is byte-identical to the original.
    pub changed: bool,
}

/// Where one key was found in the original file.
#[derive(Default)]
struct Location {
    /// Indices of every active `KEY=` line, in order.
    active: Vec<usize>,
    /// Index of the last `#KEY=` line, if any.
    last_comment: Option<usize>,
}

/// Render one entry as the line that will be written.
fn render(entry: &Entry) -> String {
    match &entry.value {
        Some(value) => format!("{}=\"{}\"", entry.key, escape_value(value)),
        None => format!("#{}=", entry.key),
    }
}

/// Locate every submitted key in the original lines.
///
/// Both patterns are anchored and match the key exactly, so documentation
/// prose containing `=` is never mistaken for a definition.
fn locate(lines: &[&str], entries: &[Entry]) -> BTreeMap<String, Location> {
    let mut located = BTreeMap::new();

    for entry in entries {
        let escaped = regex::escape(&entry.key);
        let active = Regex::new(&format!(r"(?i)^\s*{escaped}\s*="))
            .expect("pattern is bounded: keys are escaped and length-capped by validate");
        // `#` and `;` are both EnvironmentFile comment prefixes (see
        // `parse_config_content`), so either marks a home for the new value.
        let commented = Regex::new(&format!(r"(?i)^\s*[#;]\s*{escaped}\s*="))
            .expect("pattern is bounded: keys are escaped and length-capped by validate");

        let mut location = Location::default();
        for (index, line) in lines.iter().enumerate() {
            if active.is_match(line) {
                location.active.push(index);
            } else if commented.is_match(line) {
                location.last_comment = Some(index);
            }
        }
        located.insert(entry.key.clone(), location);
    }

    located
}

/// Validate `updates` and produce the complete prospective file content.
///
/// Returns `Err` with a rejection for every offending key if any key is
/// invalid; in that case nothing should be written.
pub fn plan_edit(
    original: &str,
    updates: &Map<String, Value>,
) -> Result<EditPlan, BTreeMap<String, Reject>> {
    let entries = validate(updates)?;

    let lines: Vec<&str> = original.lines().collect();
    // `lines()` discards the terminator (both `\n` and a leading `\r`), so record
    // it and restore it at the end. A file containing any `\r\n` is emitted
    // entirely CRLF; mixed-ending files are pathological and normalizing them to
    // one kind is acceptable, but a uniform file of either kind must round-trip
    // byte-identically under an empty update.
    let terminator = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let ended_with_newline = original.is_empty() || original.ends_with('\n');

    let located = locate(&lines, &entries);
    let by_key: BTreeMap<&str, &Entry> = entries.iter().map(|e| (e.key.as_str(), e)).collect();

    // Line index -> the entry that owns (and replaces) that line.
    let mut owner: BTreeMap<usize, &Entry> = BTreeMap::new();
    // Line index -> the entry whose new line follows that (commented) line.
    let mut insert_after: BTreeMap<usize, &Entry> = BTreeMap::new();

    for (key, location) in &located {
        let entry = by_key[key.as_str()];
        for index in &location.active {
            owner.insert(*index, entry);
        }
        // A commented line only gains a sibling when no active line exists.
        if location.active.is_empty() && entry.value.is_some() {
            if let Some(index) = location.last_comment {
                insert_after.insert(index, entry);
            }
        }
    }

    let mut content = String::with_capacity(original.len() + 256);
    let mut dispositions: BTreeMap<String, Disposition> = BTreeMap::new();
    let mut unmatched = BTreeSet::new();

    for (index, line) in lines.iter().enumerate() {
        match owner.get(&index) {
            Some(entry) => {
                let rendered = render(entry);
                let disposition = if rendered == **line {
                    Disposition::Unchanged
                } else if entry.value.is_some() {
                    Disposition::Updated
                } else {
                    Disposition::Unset
                };
                content.push_str(&rendered);
                content.push_str(terminator);
                // With several active lines, any real change outranks Unchanged.
                dispositions
                    .entry(entry.key.clone())
                    .and_modify(|current| {
                        if *current == Disposition::Unchanged {
                            *current = disposition;
                        }
                    })
                    .or_insert(disposition);
            }
            None => {
                content.push_str(line);
                content.push_str(terminator);
            }
        }

        if let Some(entry) = insert_after.get(&index) {
            content.push_str(&render(entry));
            content.push_str(terminator);
            dispositions.insert(entry.key.clone(), Disposition::Inserted);
        }
    }

    let to_append: Vec<&Entry> = entries
        .iter()
        .filter(|entry| {
            let location = &located[&entry.key];
            entry.value.is_some() && location.active.is_empty() && location.last_comment.is_none()
        })
        .collect();

    if !to_append.is_empty() {
        // A line-wise check, not a substring search: the marker text could appear
        // inside a quoted value (e.g. a NOTE key) without the file actually having
        // a marker line.
        if !original.lines().any(|line| line.trim() == APPEND_MARKER) {
            if !content.is_empty() {
                content.push_str(terminator);
            }
            content.push_str(APPEND_MARKER);
            content.push_str(terminator);
        }
        for entry in to_append {
            content.push_str(&render(entry));
            content.push_str(terminator);
            dispositions.insert(entry.key.clone(), Disposition::Appended);
            unmatched.insert(entry.key.clone());
        }
    }

    // Anything not otherwise dispositioned had nothing to do: an unset for a
    // key with no active line.
    for entry in &entries {
        dispositions
            .entry(entry.key.clone())
            .or_insert(Disposition::Unchanged);
    }

    if !ended_with_newline && content.ends_with(terminator) {
        content.truncate(content.len() - terminator.len());
    }

    let changed = content != original;

    Ok(EditPlan {
        dispositions,
        unmatched,
        content,
        changed,
    })
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
    fn keys_longer_than_the_limit_are_rejected() {
        // Regression test for the panic in `locate`: a key long enough to
        // blow past regex's default 10 MB compiled-size limit must be
        // rejected by `validate`, not reach `Regex::new` at all. Exercise
        // `plan_edit`, the path that used to panic, rather than `validate`
        // alone.
        let key = "A".repeat(200_000);
        let e = plan_edit("A=\"1\"\n", &one(&key, json!("x"))).expect_err("expected rejection");
        assert!(matches!(e[&key], Reject::InvalidKey(_)));
    }

    #[test]
    fn keys_at_the_length_limit_are_accepted() {
        let key = "A".repeat(MAX_KEY_LEN);
        assert_eq!(
            validate(&one(&key, json!("x"))).expect("expected validation to succeed"),
            vec![entry(&key, Some("x"))]
        );
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

    fn plan(original: &str, updates: Value) -> EditPlan {
        plan_edit(original, &map(updates)).expect("expected planning to succeed")
    }

    #[test]
    fn empty_update_is_a_byte_identical_no_op() {
        let original = "A=\"1\"\n# comment\nB=\"2\"\n";
        let p = plan(original, json!({}));
        assert_eq!(p.content, original);
        assert!(!p.changed);
        assert!(p.dispositions.is_empty());
    }

    #[test]
    fn active_line_is_updated_in_place() {
        let p = plan(
            "# doc\nRUST_LOG=\"info\"\nOTHER=\"x\"\n",
            json!({ "RUST_LOG": "debug" }),
        );
        assert_eq!(p.content, "# doc\nRUST_LOG=\"debug\"\nOTHER=\"x\"\n");
        assert_eq!(p.dispositions["RUST_LOG"], Disposition::Updated);
        assert!(p.unmatched.is_empty());
    }

    #[test]
    fn lowercase_submitted_key_matches_uppercase_line() {
        let p = plan("RUST_LOG=\"info\"\n", json!({ "rust_log": "debug" }));
        assert_eq!(p.content, "RUST_LOG=\"debug\"\n");
        assert_eq!(p.dispositions["RUST_LOG"], Disposition::Updated);
    }

    #[test]
    fn new_line_is_inserted_below_the_last_commented_occurrence() {
        let original = "#TARGET=\ntail\n";
        let p = plan(original, json!({ "TARGET": "192.168.1.200" }));
        assert_eq!(p.content, "#TARGET=\nTARGET=\"192.168.1.200\"\ntail\n");
        assert_eq!(p.dispositions["TARGET"], Disposition::Inserted);
    }

    #[test]
    fn last_commented_occurrence_wins_over_earlier_ones() {
        let original = "#TF_VEC=\"first\"\nmiddle\n#TF_VEC=\"second\"\ntail\n";
        let p = plan(original, json!({ "TF_VEC": "0 0 0" }));
        assert_eq!(
            p.content,
            "#TF_VEC=\"first\"\nmiddle\n#TF_VEC=\"second\"\nTF_VEC=\"0 0 0\"\ntail\n"
        );
        assert_eq!(p.dispositions["TF_VEC"], Disposition::Inserted);
    }

    #[test]
    fn semicolon_commented_line_is_a_home_for_the_new_value() {
        // `man systemd.exec`: in an EnvironmentFile, "lines starting with
        // \";\" or \"#\" will be ignored". Both prefixes must count as a
        // commented occurrence of the key.
        let original = ";TARGET=\"old\"\ntail\n";
        let p = plan(original, json!({ "TARGET": "192.168.1.200" }));
        assert_eq!(
            p.content,
            ";TARGET=\"old\"\nTARGET=\"192.168.1.200\"\ntail\n"
        );
        assert_eq!(p.dispositions["TARGET"], Disposition::Inserted);
        assert!(
            p.unmatched.is_empty(),
            "the key had a commented home; it must not be reported unmatched"
        );
    }

    #[test]
    fn semicolon_comment_is_not_mistaken_for_an_active_line() {
        // The value must be inserted, never used to rewrite the comment
        // itself: the original line stays byte-identical.
        let p = plan(";A=\"1\"\n", json!({ "A": "2" }));
        assert!(p.content.starts_with(";A=\"1\"\n"), "got {:?}", p.content);
    }

    #[test]
    fn parse_skips_semicolon_comments() {
        // A `;`-commented line parsed as a setting yields the key ";OLD_KEY",
        // which `validate` rejects. Because a save is all-or-nothing, posting
        // back what GET returned would then fail the entire request.
        let parsed = parse_config_content("A=\"1\"\n;OLD_KEY=\"x\"\n; SPACED=\"y\"\n");
        assert_eq!(parsed.len(), 1, "got {parsed:?}");
        assert_eq!(parsed["A"], json!("1"));
    }

    #[test]
    fn active_line_wins_and_commented_alternative_is_untouched() {
        // This is lidarpub.default's Robosense/Ouster pattern.
        let original = "TF_VEC=\"0.1 0 -0.05\"\n#TF_VEC=\"0 0 -0.19\"\n";
        let p = plan(original, json!({ "TF_VEC": "0.2 0 -0.05" }));
        assert_eq!(p.content, "TF_VEC=\"0.2 0 -0.05\"\n#TF_VEC=\"0 0 -0.19\"\n");
        assert_eq!(p.dispositions["TF_VEC"], Disposition::Updated);
    }

    #[test]
    fn key_absent_everywhere_is_appended_under_a_marker() {
        let p = plan("A=\"1\"\n", json!({ "NEW_KEY": "x" }));
        assert_eq!(
            p.content,
            format!("A=\"1\"\n\n{APPEND_MARKER}\nNEW_KEY=\"x\"\n")
        );
        assert_eq!(p.dispositions["NEW_KEY"], Disposition::Appended);
        assert!(p.unmatched.contains("NEW_KEY"));
    }

    #[test]
    fn marker_is_never_emitted_twice() {
        let first = plan("A=\"1\"\n", json!({ "B": "2" })).content;
        let second = plan(&first, json!({ "C": "3" })).content;
        assert_eq!(second.matches(APPEND_MARKER).count(), 1);
        assert!(second.contains("C=\"3\"\n"));
    }

    #[test]
    fn marker_text_inside_a_value_does_not_suppress_a_real_marker_line() {
        // The gate must be line-wise, not a substring search over the whole file:
        // the marker text appearing inside a quoted value is not a marker line.
        let original = format!("NOTE=\"{APPEND_MARKER}\"\n");
        let p = plan(&original, json!({ "NEW_KEY": "x" }));
        assert_eq!(p.content.matches(APPEND_MARKER).count(), 2);
        assert!(p
            .content
            .contains(&format!("\n{APPEND_MARKER}\nNEW_KEY=\"x\"\n")));
    }

    #[test]
    fn null_comments_out_every_active_line() {
        let p = plan(
            "DURATION=\"300\"\nx\nDURATION=\"600\"\n",
            json!({ "DURATION": null }),
        );
        assert_eq!(p.content, "#DURATION=\nx\n#DURATION=\n");
        assert_eq!(p.dispositions["DURATION"], Disposition::Unset);
    }

    #[test]
    fn null_on_a_key_with_no_active_line_changes_nothing() {
        let original = "#TARGET=\n";
        let p = plan(original, json!({ "TARGET": null }));
        assert_eq!(p.content, original);
        assert!(!p.changed);
        assert_eq!(p.dispositions["TARGET"], Disposition::Unchanged);
    }

    #[test]
    fn null_on_a_key_absent_entirely_changes_nothing() {
        let p = plan("A=\"1\"\n", json!({ "NOPE": null }));
        assert_eq!(p.content, "A=\"1\"\n");
        assert!(!p.changed);
        assert_eq!(p.dispositions["NOPE"], Disposition::Unchanged);
    }

    #[test]
    fn identical_value_reports_unchanged() {
        let original = "RUST_LOG=\"info\"\n";
        let p = plan(original, json!({ "RUST_LOG": "info" }));
        assert_eq!(p.content, original);
        assert!(!p.changed);
        assert_eq!(p.dispositions["RUST_LOG"], Disposition::Unchanged);
    }

    #[test]
    fn respacing_an_equals_sign_counts_as_a_change() {
        // fusion.default ships entirely in this spaced form.
        let p = plan("RUST_LOG = \"info\"\n", json!({ "RUST_LOG": "info" }));
        assert_eq!(p.content, "RUST_LOG=\"info\"\n");
        assert!(p.changed);
        assert_eq!(p.dispositions["RUST_LOG"], Disposition::Updated);
    }

    #[test]
    fn every_duplicate_active_line_is_rewritten() {
        // systemd takes the last definition, so a stale duplicate would win.
        let p = plan("MODE=\"a\"\nx\nMODE=\"b\"\n", json!({ "MODE": "peer" }));
        assert_eq!(p.content, "MODE=\"peer\"\nx\nMODE=\"peer\"\n");
        assert_eq!(p.dispositions["MODE"], Disposition::Updated);
    }

    #[test]
    fn an_unchanged_line_is_promoted_when_a_later_duplicate_really_changes() {
        // Pins the `and_modify` promotion in `plan_edit`: with several active
        // lines for one key, an earlier line that already matched must not
        // leave the key dispositioned `Unchanged` once a later line proves a
        // real change happened somewhere. The first line here already reads
        // the submitted value; the second does not.
        let p = plan("KEY=\"1\"\nx\nKEY=\"2\"\n", json!({ "KEY": "1" }));
        assert_eq!(p.content, "KEY=\"1\"\nx\nKEY=\"1\"\n");
        assert_eq!(p.dispositions["KEY"], Disposition::Updated);
    }

    #[test]
    fn a_later_unchanged_line_does_not_undo_an_earlier_promotion() {
        // The reverse order: the first line is the one that changes, the
        // second already matches. The key must still end up `Updated`, not
        // demoted back to `Unchanged` by processing the matching line last.
        let p = plan("KEY=\"2\"\nx\nKEY=\"1\"\n", json!({ "KEY": "1" }));
        assert_eq!(p.content, "KEY=\"1\"\nx\nKEY=\"1\"\n");
        assert_eq!(p.dispositions["KEY"], Disposition::Updated);
    }

    #[test]
    fn documentation_prose_containing_equals_is_never_matched() {
        let original =
            "# Examples: \"info\", \"debug\", \"warn\", \"edgefirst_lidarpub=debug,info\"\n\
             RUST_LOG=\"info\"\n";
        let p = plan(original, json!({ "RUST_LOG": "debug" }));
        assert_eq!(
            p.content,
            "# Examples: \"info\", \"debug\", \"warn\", \"edgefirst_lidarpub=debug,info\"\n\
             RUST_LOG=\"debug\"\n"
        );
        assert_eq!(p.content.matches("RUST_LOG=").count(), 1);
    }

    #[test]
    fn a_key_that_is_a_prefix_of_another_is_not_confused() {
        // TF is a prefix of TF_VEC; the anchored `=` must not let TF match the
        // TF_VEC line, so TF is appended and TF_VEC is left untouched.
        let p = plan("TF_VEC=\"a\"\n", json!({ "TF": "b" }));
        assert_eq!(p.dispositions["TF"], Disposition::Appended);
        assert!(p.content.contains("TF_VEC=\"a\"\n"));
    }

    #[test]
    fn values_are_escaped_on_write() {
        let p = plan("A=\"x\"\n", json!({ "A": r#"say "hi" \ ok"# }));
        assert_eq!(p.content, "A=\"say \\\"hi\\\" \\\\ ok\"\n");
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_not_having_one() {
        let original = "A=\"1\"";
        assert_eq!(plan(original, json!({})).content, original);
        assert_eq!(plan(original, json!({ "A": "2" })).content, "A=\"2\"");
    }

    #[test]
    fn crlf_file_survives_an_empty_update_unchanged() {
        let original = "A=\"1\"\r\nB=\"2\"\r\n";
        let p = plan(original, json!({}));
        assert_eq!(p.content, original);
        assert!(!p.changed);
    }

    #[test]
    fn crlf_file_keeps_crlf_when_edited() {
        let original = "A=\"1\"\r\nB=\"2\"\r\n";
        let p = plan(original, json!({ "A": "3" }));
        assert_eq!(p.content, "A=\"3\"\r\nB=\"2\"\r\n");
    }

    #[test]
    fn crlf_file_without_trailing_newline_keeps_not_having_one() {
        let original = "A=\"1\"\r\nB=\"2\"";
        assert_eq!(plan(original, json!({})).content, original);
    }

    #[test]
    fn planning_is_idempotent() {
        let original = "#CLUSTERING=\"\"\nRUST_LOG=\"info\"\n";
        let updates = json!({ "CLUSTERING": "voxel", "RUST_LOG": "debug", "NEW": "1" });
        let once = plan(original, updates.clone()).content;
        let twice = plan(&once, updates);
        assert_eq!(twice.content, once);
        assert!(!twice.changed);
    }

    #[test]
    fn rejections_propagate_and_produce_no_plan() {
        let e = plan_edit("A=\"1\"\n", &map(json!({ "A": "ok\nEVIL=1" })))
            .expect_err("expected rejection");
        assert!(matches!(e["A"], Reject::InvalidValue(_)));
    }

    // ========================================================================
    // Config content parsing tests
    // ========================================================================

    #[test]
    fn test_parse_config_content_simple() {
        let content = r#"
KEY1=value1
KEY2=value2
"#;
        let result = parse_config_content(content);
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("KEY1").unwrap(), "value1");
        assert_eq!(result.get("KEY2").unwrap(), "value2");
    }

    #[test]
    fn test_parse_config_content_with_quotes() {
        let content = r#"
PATH="/usr/local/bin"
NAME="My Service"
"#;
        let result = parse_config_content(content);
        assert_eq!(result.get("PATH").unwrap(), "/usr/local/bin");
        assert_eq!(result.get("NAME").unwrap(), "My Service");
    }

    #[test]
    fn parse_unescapes_backslash_and_quote() {
        let content = r#"K="a\"b\\c""#;
        let result = parse_config_content(content);
        assert_eq!(result.get("K").unwrap(), r#"a"b\c"#);
    }

    #[test]
    fn parse_render_round_trip_is_lossless_for_escaped_values() {
        let value = r#"a"b\c"#;
        let entry = entry("K", Some(value));
        let rendered = render(&entry);
        let parsed = parse_config_content(&rendered);
        assert_eq!(parsed.get("K").unwrap(), value);
    }

    #[test]
    fn saving_an_escaped_value_twice_is_idempotent() {
        let original = "K=\"1\"\n";
        let value = json!(r#"a"b\c"#);

        let first = plan(original, json!({ "K": value.clone() }));
        assert!(first.changed);

        let parsed = parse_config_content(&first.content);
        let second = plan(&first.content, Value::Object(parsed));
        assert_eq!(
            second.content, first.content,
            "saving twice must be a no-op"
        );
        assert!(!second.changed, "second save must report no change");
    }

    #[test]
    fn parse_leaves_other_backslash_sequences_alone() {
        let content = r#"K="a\nb""#;
        let result = parse_config_content(content);
        assert_eq!(result.get("K").unwrap(), "a\\nb");
    }

    #[test]
    fn test_parse_config_content_with_comments() {
        let content = r#"
# This is a comment
KEY1=value1
# Another comment
KEY2=value2
"#;
        let result = parse_config_content(content);
        assert_eq!(result.len(), 2);
        assert!(!result.contains_key("# This is a comment"));
    }

    #[test]
    fn test_parse_config_content_empty_lines() {
        let content = r#"
KEY1=value1

KEY2=value2

"#;
        let result = parse_config_content(content);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_parse_config_content_multiple_values() {
        let content = r#"TOPICS=topic1 topic2 topic3"#;
        let result = parse_config_content(content);
        let topics = result.get("TOPICS").unwrap().as_array().unwrap();
        assert_eq!(topics.len(), 3);
        assert_eq!(topics[0], "topic1");
        assert_eq!(topics[1], "topic2");
        assert_eq!(topics[2], "topic3");
    }

    #[test]
    fn test_parse_config_content_empty() {
        let content = "";
        let result = parse_config_content(content);
        assert!(result.is_empty());
    }

    // ========================================================================
    // Golden fixture tests: six real shipped service config files
    // ========================================================================

    const FIXTURES: [&str; 6] = [
        "lidarpub", "recorder", "radarpub", "camera", "fusion", "model",
    ];

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/{name}.default",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn every_fixture_survives_an_empty_update_unchanged() {
        for name in FIXTURES {
            let original = fixture(name);
            let p = plan(&original, json!({}));
            assert_eq!(p.content, original, "{name}.default was modified");
            assert!(!p.changed, "{name}.default reported changed");
        }
    }

    #[test]
    fn round_tripping_parsed_values_preserves_meaning() {
        for name in FIXTURES {
            let original = fixture(name);
            let parsed = parse_config_content(&original);
            let p = plan(&original, Value::Object(parsed.clone()));
            assert_eq!(
                parse_config_content(&p.content),
                parsed,
                "{name}.default lost or changed a value on round trip"
            );
        }
    }

    #[test]
    fn round_trip_is_byte_identical_for_canonically_formatted_fixtures() {
        // fusion.default is written entirely as `KEY = "v"` and is expected to
        // renormalize; every other fixture must not move a single byte.
        for name in ["lidarpub", "recorder", "radarpub", "camera", "model"] {
            let original = fixture(name);
            let p = plan(&original, Value::Object(parse_config_content(&original)));
            assert_eq!(p.content, original, "{name}.default was reformatted");
        }
    }

    #[test]
    fn fusion_renormalizes_only_the_spaces_around_equals() {
        let original = fixture("fusion");
        let p = plan(&original, Value::Object(parse_config_content(&original)));
        assert!(p.changed);
        assert_eq!(
            p.content.replace(" = \"", "=\""),
            original.replace(" = \"", "=\""),
            "fusion.default changed by more than the spacing around '='"
        );
    }

    #[test]
    fn lidarpub_commented_keys_all_become_active_in_their_own_sections() {
        let original = fixture("lidarpub");
        let before = parse_config_content(&original);

        // Every key that ships commented out, per the ticket.
        let commented: Vec<String> = original
            .lines()
            .filter_map(|line| {
                let rest = line.trim_start().strip_prefix('#')?;
                let (key, _) = rest.split_once('=')?;
                let key = key.trim();
                (!key.is_empty()
                    && key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    && key
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    && !before.contains_key(key))
                .then(|| key.to_string())
            })
            .collect();
        assert!(
            commented.len() >= 15,
            "expected lidarpub.default to ship many commented keys, found {}",
            commented.len()
        );

        let updates: Map<String, Value> = commented
            .iter()
            .map(|k| (k.clone(), json!("test-value")))
            .collect();
        let p = plan_edit(&original, &updates).expect("planning should succeed");

        let after = parse_config_content(&p.content);
        for key in &commented {
            assert_eq!(
                after.get(key.as_str()),
                Some(&json!("test-value")),
                "{key} did not become active"
            );
            assert_eq!(p.dispositions[key], Disposition::Inserted, "{key}");
        }
        assert!(
            p.unmatched.is_empty(),
            "no lidarpub key should be unmatched: {:?}",
            p.unmatched
        );
        assert!(
            !p.content.contains(APPEND_MARKER),
            "every key had a commented home; nothing should be appended"
        );
    }

    #[test]
    fn fusion_page_payload_reports_its_three_stray_keys() {
        // Characterization test for EDGEAI-732: webui/src/config/fusion.html
        // posts three keys that match neither a fusion arg nor fusion.default.
        // Expected to change when EDGEAI-732 fixes the page.
        let original = fixture("fusion");
        let p = plan(
            &original,
            json!({
                "rust_log": "info",
                "radar_input_topic": "radar/pcd",
                "occ_angle_limit": "-55 55",
                "occ_range_limit": "0 16",
                "threshold": "0.5",
            }),
        );
        assert_eq!(
            p.unmatched.iter().cloned().collect::<Vec<_>>(),
            vec!["OCC_ANGLE_LIMIT", "OCC_RANGE_LIMIT", "RADAR_INPUT_TOPIC"]
        );
        assert_eq!(p.dispositions["RUST_LOG"], Disposition::Updated);
        assert_eq!(p.dispositions["THRESHOLD"], Disposition::Updated);
    }
}
