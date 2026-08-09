//! Background folder scanning: directory listing, cache lookup, and PNG
//! header reads all happen off the UI thread, streaming entries and progress
//! back through a channel.
//!
//! Cancellation is generational: starting a new scan bumps a shared counter;
//! the superseded worker notices on its next file, stops, and skips its cache
//! write. The UI drops any message whose generation is stale.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;

use crate::cache::{self, CachedChunk, CachedFile, FolderCache};
use crate::chunks::{self, TextChunk};
use crate::model::{FileMeta, ImageEntry};

/// What to scan: a folder (listed and cached by the worker) or an explicit
/// file list (drag-drop / Open Files — never cached, the files may span
/// arbitrary folders).
pub enum ScanTarget {
    Folder(PathBuf),
    Files(Vec<PathBuf>),
}

/// One event in a scan's message stream. Every listed file produces exactly
/// one `Entry` or `Error`, so progress is `received / total`.
pub enum ScanEvent {
    /// Listing finished; this many files will be processed.
    Started { total: usize },
    Entry(Box<ImageEntry>),
    Error(String),
    Done,
}

/// A [`ScanEvent`] tagged with the generation of the scan that produced it.
pub struct ScanUpdate {
    pub generation: u64,
    pub event: ScanEvent,
}

/// Owns the channel and the generation counter; one long-lived instance in
/// the app, one short-lived worker thread per scan.
pub struct Scanner {
    tx: Sender<ScanUpdate>,
    rx: Receiver<ScanUpdate>,
    generation: Arc<AtomicU64>,
    ctx: egui::Context,
}

impl Scanner {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            generation: Arc::new(AtomicU64::new(0)),
            ctx,
        }
    }

    /// Starts a scan, superseding any scan still running. Returns the new
    /// generation; messages from older generations should be dropped.
    pub fn start(&self, target: ScanTarget, cache_dir: Option<PathBuf>) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let counter = Arc::clone(&self.generation);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        _ = std::thread::spawn(move || {
            let mut emit = |event: ScanEvent| {
                if counter.load(Ordering::SeqCst) != generation {
                    return false; // superseded — stop scanning
                }
                let alive = tx.send(ScanUpdate { generation, event }).is_ok();
                ctx.request_repaint();
                alive
            };
            run_scan(target, cache_dir, &mut emit);
        });
        generation
    }

    pub fn poll(&self) -> Vec<ScanUpdate> {
        self.rx.try_iter().collect()
    }
}

/// The scan itself, synchronous and channel-free so tests can drive it
/// directly. `emit` returns `false` to abort (superseded scan, closed
/// channel); an aborted scan must not write the cache — it is incomplete.
fn run_scan(
    target: ScanTarget,
    cache_dir: Option<PathBuf>,
    emit: &mut dyn FnMut(ScanEvent) -> bool,
) {
    let (folder, paths) = match target {
        ScanTarget::Files(paths) => (None, paths),
        ScanTarget::Folder(dir) => match list_pngs(&dir) {
            Ok(paths) => (Some(dir), paths),
            Err(err) => {
                _ = emit(ScanEvent::Error(format!("{}: {err}", dir.display())));
                _ = emit(ScanEvent::Done);
                return;
            }
        },
    };

    // Folder scans read and rebuild the cache; file-list scans skip it.
    let old_cache = match (&folder, &cache_dir) {
        (Some(dir), Some(cache_dir)) => Some(FolderCache::load(cache_dir, dir)),
        _ => None,
    };
    let mut fresh_cache = folder.as_deref().map(FolderCache::empty);
    let mut any_miss = false;

    if !emit(ScanEvent::Started { total: paths.len() }) {
        return;
    }

    for path in paths {
        let event = scan_one(&path, old_cache.as_ref(), fresh_cache.as_mut(), &mut any_miss);
        if !emit(event) {
            return; // superseded — the partial cache must not be written
        }
    }

    // Rewrite the cache only when this scan learned something: any miss, or
    // files that vanished since the cached set was recorded.
    if let (Some(fresh), Some(old), Some(cache_dir)) = (&fresh_cache, &old_cache, &cache_dir) {
        let vanished = old.files.len() != fresh.files.len();
        if (any_miss || vanished) && let Err(err) = fresh.save(cache_dir) {
            _ = emit(ScanEvent::Error(format!("metadata cache write failed: {err}")));
        }
    }
    _ = emit(ScanEvent::Done);
}

/// Processes one file: cache hit → rebuild without opening the PNG; miss →
/// read the header from disk and record it for the next cache.
fn scan_one(
    path: &std::path::Path,
    old_cache: Option<&FolderCache>,
    fresh_cache: Option<&mut FolderCache>,
    any_miss: &mut bool,
) -> ScanEvent {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stat = std::fs::metadata(path).ok();
    let size = stat.as_ref().map(|m| m.len()).unwrap_or(0);
    let created = stat.as_ref().and_then(|m| m.created().ok());
    let modified = stat.as_ref().and_then(|m| m.modified().ok());
    let modified_epoch = modified.map(cache::epoch_seconds);

    let hit = old_cache.and_then(|c| c.lookup(&file_name, size, modified_epoch));
    if let Some(cached) = hit {
        let meta = FileMeta {
            size,
            created,
            modified,
            width: cached.width,
            height: cached.height,
        };
        let text_chunks: Vec<TextChunk> = cached
            .chunks
            .iter()
            .map(|c| TextChunk {
                keyword: c.keyword.clone(),
                text: c.text.clone(),
            })
            .collect();
        if let Some(fresh) = fresh_cache {
            _ = fresh.files.insert(file_name, cached.clone());
        }
        return ScanEvent::Entry(Box::new(ImageEntry::from_parts(
            path.to_path_buf(),
            meta,
            text_chunks,
        )));
    }

    *any_miss = true;
    match chunks::read_png_header(path) {
        Ok(header) => {
            if let Some(fresh) = fresh_cache {
                _ = fresh.files.insert(
                    file_name,
                    CachedFile {
                        size,
                        modified_epoch,
                        width: header.width,
                        height: header.height,
                        chunks: header
                            .text_chunks
                            .iter()
                            .map(|c| CachedChunk {
                                keyword: c.keyword.clone(),
                                text: c.text.clone(),
                            })
                            .collect(),
                    },
                );
            }
            let meta = FileMeta {
                size,
                created,
                modified,
                width: header.width,
                height: header.height,
            };
            ScanEvent::Entry(Box::new(ImageEntry::from_parts(
                path.to_path_buf(),
                meta,
                header.text_chunks,
            )))
        }
        Err(err) => ScanEvent::Error(format!("{}: {err}", path.display())),
    }
}

/// All `.png` files directly in `dir`, sorted by path — the table's stable
/// initial order.
fn list_pngs(dir: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        })
        .collect();
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Minimal valid-enough PNG: signature + IHDR + one tEXt + IEND, with
    /// correct lengths and CRCs. No pixel data — the scanner never decodes.
    fn write_test_png(path: &Path, keyword: &str, text: &str, width: u32, height: u32) {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        push_chunk(&mut data, b"IHDR", &ihdr);
        let mut text_data = keyword.as_bytes().to_vec();
        text_data.push(0);
        text_data.extend_from_slice(text.as_bytes());
        push_chunk(&mut data, b"tEXt", &text_data);
        push_chunk(&mut data, b"IEND", &[]);
        std::fs::write(path, data).expect("write test png");
    }

    fn push_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], payload: &[u8]) {
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(chunk_type);
        out.extend_from_slice(payload);
        let mut crc = flate2::Crc::new();
        crc.update(chunk_type);
        crc.update(payload);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }

    /// Runs a folder scan to completion, returning (entries, errors).
    fn scan_to_end(dir: &Path, cache_dir: &Path) -> (Vec<ImageEntry>, Vec<String>) {
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        let mut emit = |event: ScanEvent| {
            match event {
                ScanEvent::Entry(entry) => entries.push(*entry),
                ScanEvent::Error(err) => errors.push(err),
                ScanEvent::Started { .. } | ScanEvent::Done => {}
            }
            true
        };
        run_scan(
            ScanTarget::Folder(dir.to_path_buf()),
            Some(cache_dir.to_path_buf()),
            &mut emit,
        );
        (entries, errors)
    }

    fn row_value<'a>(entry: &'a ImageEntry, path: &str) -> &'a str {
        entry
            .rows
            .iter()
            .find(|row| row.path == path)
            .map(|row| row.value.as_str())
            .unwrap_or("<missing>")
    }

    #[test]
    fn folder_scan_caches_and_invalidates_per_file() {
        let base = std::env::temp_dir().join(format!(
            "png-metadata-gui-scan-test-{}",
            std::process::id()
        ));
        let images = base.join("images");
        let cache_dir = base.join("cache");
        std::fs::create_dir_all(&images).expect("create images dir");
        write_test_png(&images.join("a.png"), "params", "seed=1", 64, 32);
        write_test_png(&images.join("b.png"), "params", "seed=2", 64, 32);

        // Cold scan: reads both files, writes the cache.
        let (entries, errors) = scan_to_end(&images, &cache_dir);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), 2);
        assert_eq!(row_value(&entries[0], "params.seed"), "1");
        assert_eq!(entries[0].width, 64);

        // Tamper with the cached chunk text. If the second scan really skips
        // the disk read, the tampered text is what shows up.
        let mut cache = FolderCache::load(&cache_dir, &images);
        assert_eq!(cache.files.len(), 2);
        let a = cache.files.get_mut("a.png").expect("cached a.png");
        a.chunks[0].text = "seed=99".to_string();
        cache.save(&cache_dir).expect("save tampered cache");

        let (entries, _) = scan_to_end(&images, &cache_dir);
        assert_eq!(row_value(&entries[0], "params.seed"), "99"); // cache hit
        assert_eq!(row_value(&entries[1], "params.seed"), "2");

        // Rewriting a.png changes size + mtime → miss → disk truth returns,
        // and the rewritten cache loses the tampered text.
        write_test_png(&images.join("a.png"), "params", "seed=333", 64, 32);
        let (entries, _) = scan_to_end(&images, &cache_dir);
        assert_eq!(row_value(&entries[0], "params.seed"), "333");
        let cache = FolderCache::load(&cache_dir, &images);
        assert_eq!(
            cache.files.get("a.png").expect("a.png").chunks[0].text,
            "seed=333"
        );

        // Deleting b.png shrinks both the scan result and the saved cache.
        std::fs::remove_file(images.join("b.png")).expect("remove b.png");
        let (entries, _) = scan_to_end(&images, &cache_dir);
        assert_eq!(entries.len(), 1);
        let cache = FolderCache::load(&cache_dir, &images);
        assert_eq!(cache.files.len(), 1);
        assert!(cache.files.contains_key("a.png"));

        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn file_list_scan_skips_cache_and_reports_bad_files() {
        let base = std::env::temp_dir().join(format!(
            "png-metadata-gui-files-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).expect("create dir");
        let good = base.join("good.png");
        let bad = base.join("bad.png");
        write_test_png(&good, "params", "seed=7", 16, 16);
        std::fs::write(&bad, b"not a png at all").expect("write bad file");

        let mut entries = Vec::new();
        let mut errors = Vec::new();
        let mut emit = |event: ScanEvent| {
            match event {
                ScanEvent::Entry(entry) => entries.push(*entry),
                ScanEvent::Error(err) => errors.push(err),
                _ => {}
            }
            true
        };
        run_scan(ScanTarget::Files(vec![good, bad]), None, &mut emit);
        assert_eq!(entries.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("bad.png"));

        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
