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
        let value_lc = value.to_lowercase();
        Self {
            path,
            key,
            value,
            path_lc,
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
    let mut out = String::with_capacity(text.len());
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
            out.push(b as char);
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push('"');
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
                out.push_str("null");
                replaced = true;
                i += len;
                prev_significant = b'l';
                continue;
            }
        }
        if !b.is_ascii_whitespace() {
            prev_significant = b;
        }
        out.push(b as char);
        i += 1;
    }

    replaced.then_some(out)
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
        }
    }
    if !found_pairs {
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
    fn free_text_becomes_single_row() {
        let payload = parse_payload("a photo of a cat, masterpiece");
        let rows = flatten("parameters", &payload);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "parameters");
    }
}
