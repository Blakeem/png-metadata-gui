//! Per-folder metadata cache: lets a folder scan skip re-reading PNG headers
//! for files whose (name, size, mtime) triple is unchanged since last time.
//!
//! One gzip-compressed JSON file per folder — named by a hash of the folder
//! path — inside `metadata-cache/` next to eframe's own storage (the per-user
//! app-data directory). A cache stores exactly what
//! [`crate::model::ImageEntry::from_parts`] needs: file facts plus the raw
//! text chunks, so a hit rebuilds an entry without touching the PNG.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

/// Bump whenever the schema — or anything upstream that shapes the cached
/// data — changes incompatibly. Old caches are discarded, never migrated.
const CACHE_VERSION: u32 = 1;

/// Everything cached for one folder.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FolderCache {
    version: u32,
    /// The folder this cache describes — guards against path-hash collisions.
    folder: PathBuf,
    /// Keyed by file name; the folder prefix is implied.
    pub files: HashMap<String, CachedFile>,
}

/// One PNG's cached facts. `size` + `modified_epoch` are the change detector:
/// same name, same size, same mtime → the text chunks are assumed current.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CachedFile {
    pub size: u64,
    pub modified_epoch: Option<i64>,
    pub width: u32,
    pub height: u32,
    pub chunks: Vec<CachedChunk>,
}

/// A raw text chunk, exactly as decoded from the PNG.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CachedChunk {
    pub keyword: String,
    pub text: String,
}

impl FolderCache {
    pub fn empty(folder: &Path) -> Self {
        Self {
            version: CACHE_VERSION,
            folder: folder.to_path_buf(),
            files: HashMap::new(),
        }
    }

    /// Loads the cache for `folder`, or an empty one on any failure — a
    /// missing, corrupt, version-mismatched, or hash-collided cache all just
    /// mean "scan everything fresh".
    pub fn load(cache_dir: &Path, folder: &Path) -> Self {
        let path = cache_file_path(cache_dir, folder);
        let Ok(file) = std::fs::File::open(&path) else {
            return Self::empty(folder);
        };
        let decoder = GzDecoder::new(std::io::BufReader::new(file));
        match serde_json::from_reader::<_, Self>(decoder) {
            Ok(cache) if cache.version == CACHE_VERSION && cache.folder == folder => cache,
            _ => Self::empty(folder),
        }
    }

    /// Persists the cache. Written to a sibling temp file first, then renamed
    /// over the target, so a crash mid-write cannot leave a torn cache.
    pub fn save(&self, cache_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(cache_dir)
            .map_err(|e| format!("create cache dir failed: {e}"))?;
        let path = cache_file_path(cache_dir, &self.folder);
        // Pid-suffixed temp name: two app instances saving the same folder's
        // cache concurrently must not clobber each other's half-written file.
        let tmp_path = path.with_extension(format!("tmp-{}", std::process::id()));
        {
            let file = std::fs::File::create(&tmp_path)
                .map_err(|e| format!("create cache file failed: {e}"))?;
            let mut encoder =
                GzEncoder::new(std::io::BufWriter::new(file), Compression::fast());
            serde_json::to_writer(&mut encoder, self)
                .map_err(|e| format!("encode cache failed: {e}"))?;
            let mut writer = encoder
                .finish()
                .map_err(|e| format!("finish cache failed: {e}"))?;
            writer
                .flush()
                .map_err(|e| format!("flush cache failed: {e}"))?;
        }
        std::fs::rename(&tmp_path, &path).map_err(|e| format!("replace cache failed: {e}"))
    }

    /// The cached record for `file_name`, but only when the change detector
    /// still matches. An unknown mtime never hits — it cannot be validated.
    pub fn lookup(
        &self,
        file_name: &str,
        size: u64,
        modified_epoch: Option<i64>,
    ) -> Option<&CachedFile> {
        let cached = self.files.get(file_name)?;
        let valid = modified_epoch.is_some()
            && cached.modified_epoch == modified_epoch
            && cached.size == size;
        valid.then_some(cached)
    }
}

/// Stable cache file path for a folder. Windows paths are case-insensitive,
/// so the hash input is lowercased.
pub fn cache_file_path(cache_dir: &Path, folder: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    folder.to_string_lossy().to_lowercase().hash(&mut hasher);
    cache_dir.join(format!("folder-{:016x}.json.gz", hasher.finish()))
}

/// The cache directory beside eframe's own storage for this app. `None` when
/// no per-user storage directory resolves — caching is skipped then.
pub fn default_cache_dir() -> Option<PathBuf> {
    eframe::storage_dir(crate::APP_NAME).map(|dir| dir.join("metadata-cache"))
}

/// `SystemTime` → Unix seconds, the cache's on-disk time representation.
/// Whole-second truncation is deliberate: the comparison happens between two
/// values that both passed through this function, so precision loss cancels.
pub fn epoch_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "png-metadata-gui-test-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_file() -> CachedFile {
        CachedFile {
            size: 1234,
            modified_epoch: Some(1_700_000_000),
            width: 512,
            height: 768,
            chunks: vec![CachedChunk {
                keyword: "prompt".to_string(),
                text: r#"{"160": {"inputs": {"value": "café 日本"}}}"#.to_string(),
            }],
        }
    }

    #[test]
    fn cache_roundtrips_through_disk() {
        let cache_dir = unique_temp_dir("roundtrip");
        let folder = PathBuf::from(r"C:\images\test");
        let mut cache = FolderCache::empty(&folder);
        _ = cache.files.insert("a.png".to_string(), sample_file());
        cache.save(&cache_dir).expect("save cache");

        let loaded = FolderCache::load(&cache_dir, &folder);
        assert_eq!(loaded.files.len(), 1);
        let file = loaded.files.get("a.png").expect("cached file");
        assert_eq!(file.chunks[0].text, r#"{"160": {"inputs": {"value": "café 日本"}}}"#);

        // A different folder must not read this cache, even at the same dir.
        let other = FolderCache::load(&cache_dir, Path::new(r"C:\images\other"));
        assert!(other.files.is_empty());
        std::fs::remove_dir_all(&cache_dir).expect("cleanup");
    }

    #[test]
    fn lookup_validates_size_and_mtime() {
        let folder = PathBuf::from(r"C:\images\test");
        let mut cache = FolderCache::empty(&folder);
        _ = cache.files.insert("a.png".to_string(), sample_file());

        assert!(cache.lookup("a.png", 1234, Some(1_700_000_000)).is_some());
        assert!(cache.lookup("a.png", 1235, Some(1_700_000_000)).is_none()); // size changed
        assert!(cache.lookup("a.png", 1234, Some(1_700_000_001)).is_none()); // mtime changed
        assert!(cache.lookup("a.png", 1234, None).is_none()); // unverifiable
        assert!(cache.lookup("b.png", 1234, Some(1_700_000_000)).is_none()); // unknown file
    }

    #[test]
    fn epoch_seconds_handles_both_sides_of_the_epoch() {
        assert_eq!(epoch_seconds(UNIX_EPOCH + Duration::from_secs(5)), 5);
        assert_eq!(epoch_seconds(UNIX_EPOCH - Duration::from_secs(5)), -5);
        assert_eq!(epoch_seconds(UNIX_EPOCH), 0);
    }
}
