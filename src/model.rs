//! Per-image data: PNG header info plus parsed chunks and flattened rows.
//!
//! File metadata (size, dimensions, created/modified dates) is synthesized
//! into `file.*` rows so it pins, searches, and sorts exactly like tEXt data.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::chunks::{self, TextChunk};
use crate::index::{self, ChunkPayload, MetaRow};

/// How a pin's pattern selects rows. `Key` and `Auto` treat a pattern
/// containing `.` as an exact-path pin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PinMode {
    /// Bare key equality on direct leaves (dotted pattern: exact path).
    #[default]
    Key,
    /// ComfyUI node-title substring: matches the direct `inputs.*` leaves of
    /// every node whose `_meta.title` contains the pattern. Node ids differ
    /// between workflows; titles are the stable handle.
    Title,
    /// Union of `Key` and `Title` — for typed pins, where the user may have
    /// entered either a key or a node title.
    Auto,
}

pub struct ParsedChunk {
    pub keyword: String,
    pub byte_len: usize,
    pub payload: ChunkPayload,
}

pub struct ImageEntry {
    pub path: PathBuf,
    pub file_name: String,
    pub file_name_lc: String,
    pub file_size: u64,
    pub width: u32,
    pub height: u32,
    pub chunks: Vec<ParsedChunk>,
    pub rows: Vec<MetaRow>,
    /// Lowercased node path prefix (`prompt.160`) → lowercased `_meta.title`
    /// value, built once at load for title-pin matching.
    node_titles: HashMap<String, String>,
}

/// File-level facts an [`ImageEntry`] is built from — read from disk on a
/// fresh scan, or replayed from the folder cache on a hit.
#[derive(Clone, Copy, Debug)]
pub struct FileMeta {
    pub size: u64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub width: u32,
    pub height: u32,
}

impl ImageEntry {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let metadata = std::fs::metadata(&path).ok();
        let header = chunks::read_png_header(&path)?;
        let meta = FileMeta {
            size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
            created: metadata.as_ref().and_then(|m| m.created().ok()),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            width: header.width,
            height: header.height,
        };
        Ok(Self::from_parts(path, meta, header.text_chunks))
    }

    /// Builds an entry from already-known parts. This is the single
    /// construction path — a disk read and a cache replay must produce
    /// byte-identical rows, or cached folders would render differently.
    pub fn from_parts(path: PathBuf, meta: FileMeta, text_chunks: Vec<TextChunk>) -> Self {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut rows =
            build_file_rows(meta.size, meta.width, meta.height, meta.created, meta.modified);
        let mut parsed_chunks = Vec::new();
        for TextChunk { keyword, text } in text_chunks {
            let payload = index::parse_payload(&text);
            rows.extend(index::flatten(&keyword, &payload));
            parsed_chunks.push(ParsedChunk {
                keyword,
                byte_len: text.len(),
                payload,
            });
        }

        Self {
            file_name_lc: file_name.to_lowercase(),
            file_name,
            path,
            file_size: meta.size,
            width: meta.width,
            height: meta.height,
            chunks: parsed_chunks,
            node_titles: build_node_titles(&rows),
            rows,
        }
    }

    /// Rows matching a pin pattern under the given mode. For `Key` and `Auto`
    /// a pattern containing `.` is an exact-path pin; otherwise `Key` matches
    /// direct leaves by key name, `Title` matches the direct `inputs.*` leaves
    /// of nodes whose `_meta.title` contains the pattern, and `Auto` is the
    /// union of both. `use_titles` gates title matching (the ComfyUI-titles
    /// setting); with it off, `Title` pins match nothing and `Auto` behaves
    /// like `Key`.
    pub fn rows_for_pin(&self, pattern: &str, mode: PinMode, use_titles: bool) -> Vec<&MetaRow> {
        let pattern_lc = pattern.to_lowercase();
        let key_like = matches!(mode, PinMode::Key | PinMode::Auto);
        let title_like = matches!(mode, PinMode::Title | PinMode::Auto) && use_titles;

        if key_like && pattern.contains('.') {
            return self
                .rows
                .iter()
                .filter(|row| row.path_lc == pattern_lc)
                .collect();
        }
        self.rows
            .iter()
            .filter(|row| {
                if !row.is_direct {
                    return false;
                }
                let key_hit = key_like && row.key_lc == pattern_lc;
                let title_hit = title_like
                    && self
                        .title_of_lc(&row.path_lc)
                        .is_some_and(|title| title.contains(&pattern_lc));
                key_hit || title_hit
            })
            .collect()
    }

    /// The lowercased ComfyUI node title owning a row at `path_lc`, resolved
    /// through the precomputed map. Only `{node}.inputs.…` paths have one.
    fn title_of_lc(&self, path_lc: &str) -> Option<&str> {
        let idx = path_lc.find(".inputs.")?;
        self.node_titles.get(&path_lc[..idx]).map(String::as_str)
    }

    /// [`Self::title_of_lc`] for a display-cased leaf path from the tree view.
    pub fn node_title_lc(&self, leaf_path: &str) -> Option<&str> {
        let path_lc = leaf_path.to_lowercase();
        let idx = path_lc.find(".inputs.")?;
        self.node_titles.get(&path_lc[..idx]).map(String::as_str)
    }

    /// Rows synthesized from file metadata, for the tree view's File section.
    pub fn file_rows(&self) -> impl Iterator<Item = &MetaRow> {
        self.rows.iter().filter(|row| row.path_lc.starts_with("file."))
    }

    /// ComfyUI stores a human-readable node title at `{node}._meta.title`.
    /// Walk the ancestors of `leaf_path` (nearest first) and return the first
    /// such title, e.g. `prompt.6.inputs.value` → the value at
    /// `prompt.6._meta.title`. Used to auto-label exact-path pins.
    pub fn node_title_for(&self, leaf_path: &str) -> Option<String> {
        let mut prefix = leaf_path;
        while let Some((parent, _)) = prefix.rsplit_once('.') {
            let title_path = format!("{parent}._meta.title");
            if let Some(row) = self.rows.iter().find(|row| row.path == title_path) {
                return Some(row.value.clone());
            }
            prefix = parent;
        }
        None
    }
}

/// Collects `{node}._meta.title` rows into a prefix → title map (both sides
/// lowercased) so title-pin matching is one hash lookup per row.
fn build_node_titles(rows: &[MetaRow]) -> HashMap<String, String> {
    const SUFFIX: &str = "._meta.title";
    let mut titles = HashMap::new();
    for row in rows {
        if row.is_direct && row.path_lc.ends_with(SUFFIX) {
            let prefix = row.path_lc[..row.path_lc.len() - SUFFIX.len()].to_string();
            _ = titles.insert(prefix, row.value_lc.clone());
        }
    }
    titles
}

fn build_file_rows(
    file_size: u64,
    width: u32,
    height: u32,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
) -> Vec<MetaRow> {
    let mut rows = vec![
        MetaRow::new(
            "file.size".to_string(),
            "size".to_string(),
            human_size(file_size),
            true,
        )
        .with_sort_key(file_size as f64),
        MetaRow::new(
            "file.width".to_string(),
            "width".to_string(),
            width.to_string(),
            true,
        )
        .with_sort_key(width as f64),
        MetaRow::new(
            "file.height".to_string(),
            "height".to_string(),
            height.to_string(),
            true,
        )
        .with_sort_key(height as f64),
        MetaRow::new(
            "file.dimensions".to_string(),
            "dimensions".to_string(),
            format!("{width}×{height}"),
            true,
        )
        .with_sort_key(width as f64 * height as f64),
    ];
    if let Some((text, epoch)) = format_timestamp(created) {
        rows.push(
            MetaRow::new("file.created".to_string(), "created".to_string(), text, true)
                .with_sort_key(epoch),
        );
    }
    if let Some((text, epoch)) = format_timestamp(modified) {
        rows.push(
            MetaRow::new("file.modified".to_string(), "modified".to_string(), text, true)
                .with_sort_key(epoch),
        );
    }
    rows
}

/// Local-time display string plus epoch seconds for sorting.
fn format_timestamp(time: Option<SystemTime>) -> Option<(String, f64)> {
    let time = time?;
    let local: chrono::DateTime<chrono::Local> = time.into();
    Some((
        local.format("%Y-%m-%d %H:%M:%S").to_string(),
        local.timestamp() as f64,
    ))
}

pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with_rows(rows: Vec<MetaRow>) -> ImageEntry {
        ImageEntry {
            path: PathBuf::from("test.png"),
            file_name: "test.png".to_string(),
            file_name_lc: "test.png".to_string(),
            file_size: 0,
            width: 0,
            height: 0,
            chunks: Vec::new(),
            node_titles: build_node_titles(&rows),
            rows,
        }
    }

    #[test]
    fn key_pin_skips_link_array_elements() {
        let payload = index::parse_payload(
            r#"{"93": {"inputs": {"steps": ["123", 0]}}, "170": {"inputs": {"steps": 9}}}"#,
        );
        let entry = entry_with_rows(index::flatten("prompt", &payload));
        let matches = entry.rows_for_pin("steps", PinMode::Key, false);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].value, "9");
    }

    #[test]
    fn path_pin_matches_exact_location_only() {
        let payload = index::parse_payload(
            r#"{"109": {"inputs": {"cfg": 1.0}}, "183": {"inputs": {"cfg": 4.5}}}"#,
        );
        let entry = entry_with_rows(index::flatten("prompt", &payload));
        assert_eq!(entry.rows_for_pin("cfg", PinMode::Key, false).len(), 2);
        let exact = entry.rows_for_pin("prompt.183.inputs.cfg", PinMode::Key, false);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].value, "4.5");
    }

    #[test]
    fn title_pin_matches_inputs_of_titled_nodes() {
        let payload = index::parse_payload(
            r#"{"107": {"inputs": {"value": 1.0}, "_meta": {"title": "Float"}}, "160": {"inputs": {"value": "a cat"}, "_meta": {"title": "Positive Prompt"}}}"#,
        );
        let entry = entry_with_rows(index::flatten("prompt", &payload));
        // Title pins match by `_meta.title` substring, inputs leaves only —
        // node 107's numeric `value` must not appear.
        let hits = entry.rows_for_pin("positive prompt", PinMode::Title, true);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value, "a cat");
        // With the ComfyUI setting off, a title pin matches nothing.
        assert!(entry.rows_for_pin("positive prompt", PinMode::Title, false).is_empty());
        // Key pins ignore titles: bare `value` still matches both nodes.
        assert_eq!(entry.rows_for_pin("value", PinMode::Key, true).len(), 2);
        // Auto is the union of key and title matching.
        assert_eq!(entry.rows_for_pin("value", PinMode::Auto, true).len(), 2);
        assert_eq!(entry.rows_for_pin("positive prompt", PinMode::Auto, true).len(), 1);
        // Map-backed title lookup used by the tree view's pin indicator.
        assert_eq!(entry.node_title_lc("prompt.160.inputs.value"), Some("positive prompt"));
        assert_eq!(entry.node_title_lc("prompt.160._meta.title"), None);
    }

    #[test]
    fn node_title_lookup_walks_ancestors() {
        let payload = index::parse_payload(
            r#"{"6": {"inputs": {"value": "a cat"}, "_meta": {"title": "Positive Prompt"}}, "7": {"inputs": {"seed": 1}}}"#,
        );
        let entry = entry_with_rows(index::flatten("prompt", &payload));
        assert_eq!(
            entry.node_title_for("prompt.6.inputs.value").as_deref(),
            Some("Positive Prompt")
        );
        assert_eq!(entry.node_title_for("prompt.7.inputs.seed"), None);
    }

    #[test]
    fn file_rows_carry_sort_keys() {
        let rows = build_file_rows(2_097_152, 768, 1024, None, None);
        let size_row = rows.iter().find(|r| r.path == "file.size").expect("size row");
        assert_eq!(size_row.value, "2.0 MB");
        assert_eq!(size_row.sort_key, Some(2_097_152.0));
        let dims_row = rows
            .iter()
            .find(|r| r.path == "file.dimensions")
            .expect("dimensions row");
        assert_eq!(dims_row.value, "768×1024");
    }
}
