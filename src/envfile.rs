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
            .expect("key is escaped, so the pattern is valid");
        let commented = Regex::new(&format!(r"(?i)^\s*#\s*{escaped}\s*="))
            .expect("key is escaped, so the pattern is valid");

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
    // `lines()` discards the terminator, so record it and restore it at the end.
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
                content.push('\n');
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
                content.push('\n');
            }
        }

        if let Some(entry) = insert_after.get(&index) {
            content.push_str(&render(entry));
            content.push('\n');
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
        if !original.contains(APPEND_MARKER) {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(APPEND_MARKER);
            content.push('\n');
        }
        for entry in to_append {
            content.push_str(&render(entry));
            content.push('\n');
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

    if !ended_with_newline && content.ends_with('\n') {
        content.pop();
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
        let p = plan(
            "MODE=\"peer\"\nLIDAR_MODE=\"1024x10\"\n",
            json!({ "MODE": "client" }),
        );
        assert_eq!(p.content, "MODE=\"client\"\nLIDAR_MODE=\"1024x10\"\n");
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
}
