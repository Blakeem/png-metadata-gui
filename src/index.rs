//! Flattens chunk text into searchable `path = value` rows.
//!
//! A JSON payload like `{"31": {"inputs": {"seed": 42}}}` under keyword
//! `prompt` becomes one row: path `prompt.31.inputs.seed`, key `seed`,
//! value `42`. Plain-text payloads of `key=value` tokens flatten the same
//! way, so pins and search work identically across metadata styles.

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum ChunkPayload {
    Json(Value),
    Plain(String),
}

#[derive(Debug, Clone)]
pub struct MetaRow {
    pub path: String,
    pub key: String,
    pub value: String,
    pub path_lc: String,
    pub key_lc: String,
    pub value_lc: String,
    /// True when the leaf sits directly under its own named key. False for
    /// array elements (e.g. ComfyUI node links like `["123", 0]`), which
    /// inherit the nearest named ancestor as `key` for search purposes but
    /// must not satisfy key pins — a pinned `steps` should never show the
    /// linked node id `123`.
    pub is_direct: bool,
    /// Numeric sort override for values whose display text does not sort
    /// correctly (file sizes like "8.1 MB", timestamps).
    pub sort_key: Option<f64>,
}

impl MetaRow {
    pub fn new(path: String, key: String, value: String, is_direct: bool) -> Self {
        let path_lc = path.to_lowercase();
        let key_lc = key.to_lowercase();
        let value_lc = value.to_lowercase();
        Self {
            path,
            key,
            value,
            path_lc,
            key_lc,
            value_lc,
            is_direct,
            sort_key: None,
        }
    }

    pub fn with_sort_key(mut self, sort_key: f64) -> Self {
        self.sort_key = Some(sort_key);
        self
    }
}

/// Search predicate, shared by the image filter and the tree view. A query
/// containing `.` matches against the full dotted path; a bare query matches
/// against the key or the value. Matching the path with bare queries is
/// deliberately avoided: the chunk keyword (`prompt`) prefixes every path, so
/// path matching would make such queries match every row in the chunk.
/// `query` must already be lowercased.
pub fn row_matches(row: &MetaRow, query: &str) -> bool {
    if query.contains('.') {
        return row.path_lc.contains(query);
    }
    row.key_lc.contains(query) || row.value_lc.contains(query)
}

/// True when any leaf inside `value` would satisfy [`row_matches`]. Used by
/// the tree view to prune branches with no match while a search is active.
/// `path` and `nearest_key` mirror the naming used by [`flatten`].
pub fn subtree_matches(path: &str, nearest_key: &str, value: &Value, query: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(child_key, child)| {
            subtree_matches(&format!("{path}.{child_key}"), child_key, child, query)
        }),
        Value::Array(items) => items.iter().enumerate().any(|(i, child)| {
            subtree_matches(&format!("{path}.{i}"), nearest_key, child, query)
        }),
        Value::String(s) => leaf_matches(path, nearest_key, s, query),
        other => leaf_matches(path, nearest_key, &other.to_string(), query),
    }
}

/// Leaf-level form of [`row_matches`] for values that are not flattened yet.
fn leaf_matches(path: &str, key: &str, value: &str, query: &str) -> bool {
    if query.contains('.') {
        return path.to_lowercase().contains(query);
    }
    key.to_lowercase().contains(query) || value.to_lowercase().contains(query)
}

pub fn parse_payload(text: &str) -> ChunkPayload {
    match parse_json_lenient(text) {
        Some(value) if value.is_object() || value.is_array() => ChunkPayload::Json(value),
        _ => ChunkPayload::Plain(text.to_string()),
    }
}

fn parse_json_lenient(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    let sanitized = replace_nonfinite_tokens(text)?;
    serde_json::from_str::<Value>(&sanitized).ok()
}

/// Python's `json.dumps` (used by ComfyUI) emits bare `NaN`, `Infinity`, and
/// `-Infinity`, which strict JSON rejects. Replace those tokens with `null`,
/// skipping string literals. Returns `None` when nothing was replaced.
fn replace_nonfinite_tokens(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    // Bytes, not chars: `u8 as char` would re-encode every multi-byte UTF-8
    // sequence as Latin-1 mojibake. Everything inserted here is ASCII, so the
    // original UTF-8 survives byte-for-byte.
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut replaced = false;
    let mut in_string = false;
    let mut escaped = false;
    let mut prev_significant = b'\0';
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            out.push(b);
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push(b'"');
            prev_significant = b'"';
            i += 1;
            continue;
        }
        // Bare non-finite tokens can only appear in value position: after a
        // key colon, an array comma/bracket, or at the document start.
        let value_position = matches!(prev_significant, b':' | b',' | b'[' | b'\0');
        if value_position {
            let rest = &text[i..];
            let token_len = if rest.starts_with("-Infinity") {
                Some(9)
            } else if rest.starts_with("Infinity") {
                Some(8)
            } else if rest.starts_with("NaN") {
                Some(3)
            } else {
                None
            };
            if let Some(len) = token_len {
                out.extend_from_slice(b"null");
                replaced = true;
                i += len;
                prev_significant = b'l';
                continue;
            }
        }
        if !b.is_ascii_whitespace() {
            prev_significant = b;
        }
        out.push(b);
        i += 1;
    }

    replaced.then(|| String::from_utf8(out).ok()).flatten()
}

pub fn flatten(keyword: &str, payload: &ChunkPayload) -> Vec<MetaRow> {
    let mut rows = Vec::new();
    match payload {
        ChunkPayload::Json(value) => walk_json(keyword, keyword, value, true, &mut rows),
        ChunkPayload::Plain(text) => flatten_plain(keyword, text, &mut rows),
    }
    rows
}

/// Array elements keep the nearest named ancestor as their key, so searching
/// `text` also finds `prompt.6.inputs.text.0` — but such rows are marked
/// non-direct so key pins skip them.
fn walk_json(
    path: &str,
    nearest_key: &str,
    value: &Value,
    parent_is_object: bool,
    rows: &mut Vec<MetaRow>,
) {
    match value {
        Value::Object(map) => {
            for (child_key, child) in map {
                let child_path = format!("{path}.{child_key}");
                walk_json(&child_path, child_key, child, true, rows);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                let child_path = format!("{path}.{i}");
                walk_json(&child_path, nearest_key, child, false, rows);
            }
        }
        Value::String(s) => rows.push(MetaRow::new(
            path.to_string(),
            nearest_key.to_string(),
            s.clone(),
            parent_is_object,
        )),
        other => rows.push(MetaRow::new(
            path.to_string(),
            nearest_key.to_string(),
            other.to_string(),
            parent_is_object,
        )),
    }
}

fn flatten_plain(keyword: &str, text: &str, rows: &mut Vec<MetaRow>) {
    let mut found_pairs = false;
    let mut saw_non_pair = false;
    for token in text.split_whitespace() {
        if let Some((key, value)) = token.split_once('=')
            && !key.is_empty()
        {
            rows.push(MetaRow::new(
                format!("{keyword}.{key}"),
                key.to_string(),
                value.to_string(),
                true,
            ));
            found_pairs = true;
        } else {
            saw_non_pair = true;
        }
    }
    // Pure `key=value` chunks keep only their pair rows; anything with free
    // text also gets the whole text as one row, so no word is missing from
    // search or the tree.
    if !found_pairs || saw_non_pair {
        rows.push(MetaRow::new(
            keyword.to_string(),
            keyword.to_string(),
            text.to_string(),
            true,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_nested_json_with_leaf_keys() {
        let payload = parse_payload(r#"{"31": {"inputs": {"seed": 42, "text": ["160", 0]}}}"#);
        let rows = flatten("prompt", &payload);
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "prompt.31.inputs.seed",
                "prompt.31.inputs.text.0",
                "prompt.31.inputs.text.1"
            ]
        );
        assert_eq!(rows[0].key, "seed");
        assert_eq!(rows[0].value, "42");
        assert!(rows[0].is_direct);
        assert_eq!(rows[1].key, "text"); // array element keeps nearest named key
        assert!(!rows[1].is_direct); // …but is not a direct match for pins
    }

    #[test]
    fn flattens_key_value_tokens() {
        let payload = parse_payload("seed=42 denoise=0.35 sampler=euler");
        let rows = flatten("catr_ab_summary", &payload);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].path, "catr_ab_summary.seed");
        assert_eq!(rows[0].value, "42");
        assert_eq!(rows[2].key, "sampler");
    }

    #[test]
    fn parses_python_style_nan_json() {
        let payload =
            parse_payload(r#"{"118": {"is_changed": NaN, "title": "NaN keeper \" NaN"}, "b": -Infinity}"#);
        let rows = flatten("prompt", &payload);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].path, "prompt.118.is_changed");
        assert_eq!(rows[0].value, "null");
        assert_eq!(rows[1].value, "NaN keeper \" NaN"); // strings untouched
        assert_eq!(rows[2].value, "null");
    }

    #[test]
    fn nan_sanitizer_preserves_non_ascii_text() {
        let payload = parse_payload(r#"{"a": NaN, "p": "café 日本 — ✓"}"#);
        let rows = flatten("prompt", &payload);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value, "null");
        assert_eq!(rows[1].value, "café 日本 — ✓");
    }

    #[test]
    fn bare_query_matches_key_or_value_but_not_path_prefix() {
        let payload = parse_payload(
            r#"{"6": {"inputs": {"value": "a cat"}, "_meta": {"title": "Positive Prompt"}}}"#,
        );
        let rows = flatten("prompt", &payload);
        // "prompt" prefixes every path, but only the title *value* contains it.
        let hits: Vec<&MetaRow> = rows.iter().filter(|r| row_matches(r, "prompt")).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "prompt.6._meta.title");
        // Bare queries match keys; dotted queries match paths.
        assert!(rows.iter().any(|r| row_matches(r, "value")));
        assert!(rows.iter().any(|r| row_matches(r, "prompt.6.inputs.value")));
        assert!(!rows.iter().any(|r| row_matches(r, "prompt.7")));
    }

    #[test]
    fn subtree_matches_prunes_non_matching_branches() {
        let payload = parse_payload(
            r#"{"6": {"inputs": {"value": "a cat"}}, "7": {"inputs": {"seed": 42}}}"#,
        );
        let ChunkPayload::Json(root) = &payload else {
            panic!("expected JSON payload");
        };
        assert!(subtree_matches("prompt.6", "6", &root["6"], "cat"));
        assert!(!subtree_matches("prompt.7", "7", &root["7"], "cat"));
        assert!(subtree_matches("prompt.7", "7", &root["7"], "seed"));
        assert!(subtree_matches("prompt.7", "7", &root["7"], "prompt.7.inputs.seed"));
    }

    #[test]
    fn free_text_becomes_single_row() {
        let payload = parse_payload("a photo of a cat, masterpiece");
        let rows = flatten("parameters", &payload);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "parameters");
    }

    #[test]
    fn mixed_text_keeps_pairs_and_full_text() {
        let payload = parse_payload("a prompt with <lora:x=y> inside");
        let rows = flatten("parameters", &payload);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "parameters.<lora:x");
        assert_eq!(rows[0].value, "y>");
        assert_eq!(rows[1].path, "parameters");
        // The free-text words stay searchable instead of being dropped.
        assert!(rows.iter().any(|row| row_matches(row, "prompt")));
        assert!(rows.iter().any(|row| row_matches(row, "inside")));
    }
}
