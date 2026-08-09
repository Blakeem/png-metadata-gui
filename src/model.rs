//! Per-image data: PNG header info plus parsed chunks and flattened rows.
//!
//! File metadata (size, dimensions, created/modified dates) is synthesized
//! into `file.*` rows so it pins, searches, and sorts exactly like tEXt data.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::chunks::{self, TextChunk};
use crate::index::{self, ChunkPayload, MetaRow};

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
}

impl ImageEntry {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let metadata = std::fs::metadata(&path).ok();
        let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let created = metadata.as_ref().and_then(|m| m.created().ok());
        let modified = metadata.as_ref().and_then(|m| m.modified().ok());
        let header = chunks::read_png_header(&path)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut rows = build_file_rows(file_size, header.width, header.height, created, modified);
        let mut parsed_chunks = Vec::new();
        for TextChunk { keyword, text } in header.text_chunks {
            let payload = index::parse_payload(&text);
            rows.extend(index::flatten(&keyword, &payload));
            parsed_chunks.push(ParsedChunk {
                keyword,
                byte_len: text.len(),
                payload,
            });
        }

        Ok(Self {
            file_name_lc: file_name.to_lowercase(),
            file_name,
            path,
            file_size,
            width: header.width,
            height: header.height,
            chunks: parsed_chunks,
            rows,
        })
    }

    /// Rows matching a pin. A pin containing `.` is an exact-path pin and
    /// matches only that location; a bare pin matches every direct leaf with
    /// that key name, across all chunks and metadata styles.
    pub fn rows_for_pin(&self, pin: &str) -> Vec<&MetaRow> {
        let pin_lc = pin.to_lowercase();
        if pin.contains('.') {
            self.rows.iter().filter(|row| row.path_lc == pin_lc).collect()
        } else {
            self.rows
                .iter()
                .filter(|row| row.is_direct && row.key.to_lowercase() == pin_lc)
                .collect()
        }
    }

    /// Rows synthesized from file metadata, for the tree view's File section.
    pub fn file_rows(&self) -> impl Iterator<Item = &MetaRow> {
        self.rows.iter().filter(|row| row.path_lc.starts_with("file."))
    }
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
            rows,
        }
    }

    #[test]
    fn key_pin_skips_link_array_elements() {
        let payload = index::parse_payload(
            r#"{"93": {"inputs": {"steps": ["123", 0]}}, "170": {"inputs": {"steps": 9}}}"#,
        );
        let entry = entry_with_rows(index::flatten("prompt", &payload));
        let matches = entry.rows_for_pin("steps");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].value, "9");
    }

    #[test]
    fn path_pin_matches_exact_location_only() {
        let payload = index::parse_payload(
            r#"{"109": {"inputs": {"cfg": 1.0}}, "183": {"inputs": {"cfg": 4.5}}}"#,
        );
        let entry = entry_with_rows(index::flatten("prompt", &payload));
        assert_eq!(entry.rows_for_pin("cfg").len(), 2);
        let exact = entry.rows_for_pin("prompt.183.inputs.cfg");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].value, "4.5");
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
