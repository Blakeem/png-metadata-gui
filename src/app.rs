//! Main application state and UI.
//!
//! Layout:
//! - Top panel: open buttons, search box, pin chips (plus a label editor row
//!   while a pin is being renamed).
//! - Left panel: preview + metadata for the selected image. The tree is the
//!   only structural view: an active search prunes it to matching branches
//!   (still collapsible); "Pinned only" swaps it for the pinned-value list.
//! - Center panel: image table; one sortable column per pin, plus a Match
//!   column while a search is active.
//! - Bottom panel: status line.
//!
//! A pin selects rows by `pattern` + [`PinMode`]: a bare key (`cfg`) matches
//! every direct leaf with that key name; an exact path
//! (`prompt.183.inputs.cfg`) matches only that location; a title pin
//! ("Positive Prompt") matches the `inputs.*` values of every ComfyUI node
//! whose `_meta.title` contains the pattern — stable across workflows whose
//! node ids differ. Typed pins get `Auto` (key ∪ title). A pin may carry a
//! display `label` shown everywhere the pin appears; matching always uses
//! `pattern`. Multiple matches render color-coded in document order. Title
//! matching is gated by the ComfyUI-titles setting (⚙ in the toolbar).
//!
//! The find box in the metadata panel jumps to a field: it force-opens the
//! headers on the path to the first key/value match and scrolls to it (a hit
//! inside `_meta` retargets to the whole owning node); Enter cycles matches.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::{
    self, Align, Button, Color32, Key, Label, RichText, ScrollArea, Sense, TextureHandle,
    TextureOptions, Ui,
};
use egui_extras::{Column, TableBuilder};
use serde_json::Value;

use crate::cache;
use crate::full::{self, FullOutcome, FullPool};
use crate::index::{self, ChunkPayload};
use crate::lightbox::{
    self, Lightbox, LightboxDetail, LightboxId, LightboxNav, LightboxShared, LoadStatus,
    MAX_LIGHTBOXES, ViewCommand,
};
use crate::model::{human_size, ImageEntry, ParsedChunk, PinMode};
use crate::scan::{ScanEvent, ScanTarget, Scanner};
use crate::thumbs::ThumbPool;

const DEFAULT_PINS: [&str; 9] = [
    "seed",
    "noise_seed",
    "steps",
    "cfg",
    "denoise",
    "sampler_name",
    "scheduler",
    "created",
    "modified",
];
const PINS_STORAGE_KEY: &str = "pins_v4";
/// v3 stored plain pattern strings; v4 stores [`Pin`] structs with labels.
const LEGACY_PINS_STORAGE_KEY: &str = "pins_v3";
const SETTINGS_STORAGE_KEY: &str = "settings_v1";
const ROW_HEIGHT: f32 = 52.0;
const THUMB_CELL: f32 = 48.0;
pub const FLASH_SECONDS: f32 = 2.5;
const MAX_CELL_MATCHES: usize = 3;
/// Cap on the characters one value label lays out. A pinned card renders every
/// match, so a large cap here multiplies by the match count. The copy actions
/// always carry the complete value.
const MAX_VALUE_CHARS: usize = 2000;
/// Upper bound on live thumbnail textures. A folder of several thousand PNGs
/// would otherwise pin gigabytes of GPU memory. Eviction is least-recently-
/// drawn, so it can only ever take an off-screen texture; evicted rows
/// re-request their thumbnail when they scroll back into view.
const MAX_TEXTURES: usize = 512;

/// A pinned table column. `pattern` + `mode` select rows (see the module
/// docs); `label`, when set, replaces the derived name in chips, table
/// headers, and the pinned list.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Pin {
    pattern: String,
    label: Option<String>,
    /// `serde(default)` keeps pins saved before title pins existed loading as
    /// plain key pins.
    #[serde(default)]
    mode: PinMode,
    /// Per-pin copy button in table cells. `None` means the user never chose,
    /// so [`default_copy_enabled`] decides. `serde(default)` lets pins saved
    /// before the button existed load on that default.
    #[serde(default)]
    copy: Option<bool>,
}

impl Pin {
    fn key(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_string(),
            label: None,
            mode: PinMode::Key,
            copy: None,
        }
    }

    /// Typed pins: the user may have entered a key or a node title.
    fn auto(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_string(),
            label: None,
            mode: PinMode::Auto,
            copy: None,
        }
    }

    fn title(title: String) -> Self {
        Self {
            pattern: title.clone(),
            label: Some(title),
            mode: PinMode::Title,
            copy: None,
        }
    }
}

fn default_pins() -> Vec<Pin> {
    DEFAULT_PINS.iter().map(|p| Pin::key(p)).collect()
}

/// Copy-button default for a pin the user never toggled. Prompts and seeds are
/// the values people copy out of a workflow. The rule reads the displayed name,
/// so a renamed or exact-path pin follows what its column shows.
fn default_copy_enabled(display_label: &str) -> bool {
    let lowered = display_label.to_lowercase();
    lowered.contains("prompt") || lowered.contains("seed")
}

/// Whether this pin's table cells show a copy button. An explicit choice wins
/// over the default in both directions.
fn pin_copies(pin: &Pin) -> bool {
    pin.copy
        .unwrap_or_else(|| default_copy_enabled(&pin_label(pin)))
}

/// User-configurable options, persisted via eframe storage. `serde(default)`
/// lets fields be added later without invalidating stored settings.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    /// Title pins match ComfyUI `_meta.title` (substring) and show the node's
    /// `inputs` values; pinning a titled value from the tree pins its title
    /// instead of its node-id path. Inert for files without that structure.
    comfyui_titles: bool,
    /// The folder to reopen at launch when no path was passed on the
    /// command line. Updated on every folder open.
    last_folder: Option<PathBuf>,
    /// Table column widths by [`NAME_COLUMN_KEY`] or [`pin_column_key`].
    /// egui_extras keeps its own copy in egui memory, which never reaches disk
    /// and is discarded whenever the column set changes, so the widths live
    /// here instead. An unpinned column keeps its entry, so re-pinning it
    /// restores the width the user chose.
    column_widths: HashMap<String, f32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            comfyui_titles: true,
            last_folder: None,
            column_widths: HashMap::new(),
        }
    }
}

/// Progress of the scan currently streaming into the table. `total` is
/// `None` until the worker finishes listing the directory.
struct ScanState {
    generation: u64,
    done: usize,
    total: Option<usize>,
}

/// In-progress pin label edit, shown as an extra toolbar row.
struct PinRename {
    pin_index: usize,
    draft: String,
    /// Focus the text box on the first frame it appears.
    needs_focus: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SortColumn {
    Name,
    Pin(usize),
}

struct MatchInfo {
    first: String,
    count: usize,
}

/// UI events collected during a frame and applied afterwards, so widget
/// closures never need mutable access to the whole app.
#[derive(Default)]
struct UiActions {
    select: Option<usize>,
    sort: Option<SortColumn>,
    toggle_pin: Option<Pin>,
    move_pin: Option<(usize, isize)>,
    unpin: Option<usize>,
    start_rename: Option<usize>,
    apply_rename: Option<(usize, String)>,
    cancel_rename: bool,
    /// (pin index, new copy-button state) from the pin chip's context menu.
    set_pin_copy: Option<(usize, bool)>,
    copy: Option<(String, String)>, // (label, clipboard content)
    set_tree_expanded: Option<bool>,
    toggle_settings: bool,
    open_folder_dialog: bool,
    open_file_dialog: bool,
    /// Thumbnails a widget found missing this frame (never decoded, or evicted).
    request_thumbs: Vec<PathBuf>,
    /// Thumbnails a widget actually drew this frame, in draw order. Keeps them
    /// at the back of `texture_order` so the cap never evicts a visible one.
    used_thumbs: Vec<PathBuf>,
    /// Index into `images` whose lightbox window a click asked to open.
    open_lightbox: Option<usize>,
    /// This frame's resizable column widths, keyed for [`Settings`].
    column_widths: Option<Vec<(String, f32)>>,
}

pub struct ViewerApp {
    images: Vec<ImageEntry>,
    load_errors: Vec<String>,
    folder: Option<PathBuf>,
    selected: Option<usize>,
    search: String,
    pins: Vec<Pin>,
    rename: Option<PinRename>,
    new_pin: String,
    settings: Settings,
    settings_open: bool,
    /// Find box in the metadata panel: query text plus the rotating index
    /// that lets repeated Enter presses cycle through matches.
    find_text: String,
    find_last_query: String,
    find_index: usize,
    pinned_only: bool,
    sort: Option<(SortColumn, bool)>,
    visible: Vec<usize>,
    /// `visible` as paths, rebuilt by [`Self::refresh`]. Every lightbox is
    /// handed this `Arc`, and a refresh replaces the field rather than editing
    /// it, so an open window keeps the list it opened on.
    visible_paths: Arc<[PathBuf]>,
    matches: HashMap<usize, MatchInfo>,
    needs_refresh: bool,
    /// One-frame override forcing every tree section open or closed.
    force_tree_expanded: Option<bool>,
    scanner: Scanner,
    scanning: Option<ScanState>,
    thumbs: ThumbPool,
    textures: HashMap<PathBuf, TextureHandle>,
    /// Draw order of `textures`, least recently drawn first. The cap evicts
    /// from the front; drawing a thumbnail moves it to the back in
    /// `apply_actions`, so on-screen textures are never the eviction candidate.
    texture_order: VecDeque<PathBuf>,
    /// Paths whose decode failed. Without this, the re-request-on-miss path
    /// would retry an undecodable file every frame, forever.
    failed_thumbs: HashSet<PathBuf>,
    flash: Option<(String, Instant)>,
    pending_copy: Option<String>,
    /// Full-resolution decoder for lightbox windows. Separate from `thumbs`
    /// so opening one can never stall the table's thumbnails.
    full: FullPool,
    /// Open lightbox windows. Dropping one drops the last `Arc`, which drops
    /// its cloned `TextureHandle` and frees that texture.
    lightboxes: Vec<Lightbox>,
    /// Serial for the next lightbox. It only ever counts up, so two windows on
    /// one file never share a viewport id.
    next_lightbox_id: u64,
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_target: Option<PathBuf>) -> Self {
        configure_style(&cc.egui_ctx);
        let pins = cc
            .storage
            .and_then(|storage| eframe::get_value::<Vec<Pin>>(storage, PINS_STORAGE_KEY))
            .or_else(|| {
                // Migrate v3 pattern-only pins into label-less v4 pins.
                cc.storage
                    .and_then(|storage| {
                        eframe::get_value::<Vec<String>>(storage, LEGACY_PINS_STORAGE_KEY)
                    })
                    .map(|patterns| patterns.iter().map(|p| Pin::key(p)).collect())
            })
            .unwrap_or_else(default_pins);
        let settings = cc
            .storage
            .and_then(|storage| eframe::get_value::<Settings>(storage, SETTINGS_STORAGE_KEY))
            .unwrap_or_default();

        let mut app = Self {
            images: Vec::new(),
            load_errors: Vec::new(),
            folder: None,
            selected: None,
            search: String::new(),
            pins,
            rename: None,
            new_pin: String::new(),
            settings,
            settings_open: false,
            find_text: String::new(),
            find_last_query: String::new(),
            find_index: 0,
            pinned_only: false,
            sort: None,
            visible: Vec::new(),
            visible_paths: Arc::from(Vec::new()),
            matches: HashMap::new(),
            needs_refresh: false,
            force_tree_expanded: None,
            scanner: Scanner::new(cc.egui_ctx.clone()),
            scanning: None,
            thumbs: ThumbPool::new(cc.egui_ctx.clone()),
            textures: HashMap::new(),
            texture_order: VecDeque::new(),
            failed_thumbs: HashSet::new(),
            flash: None,
            pending_copy: None,
            full: FullPool::new(cc.egui_ctx.clone()),
            lightboxes: Vec::new(),
            next_lightbox_id: 0,
        };
        // A command-line path wins; otherwise reopen the last folder.
        let startup_target = initial_target.or_else(|| {
            app.settings
                .last_folder
                .clone()
                .filter(|dir| dir.is_dir())
        });
        if let Some(target) = startup_target {
            app.open_target(target);
        }
        app
    }

    // ---- data loading ----

    fn open_target(&mut self, target: PathBuf) {
        if target.is_dir() {
            self.open_folder(target);
        } else {
            self.open_files(vec![target]);
        }
    }

    fn open_folder(&mut self, dir: PathBuf) {
        self.folder = Some(dir.clone());
        self.settings.last_folder = Some(dir.clone());
        self.begin_scan(ScanTarget::Folder(dir));
    }

    fn open_files(&mut self, paths: Vec<PathBuf>) {
        self.folder = None;
        self.begin_scan(ScanTarget::Files(paths));
    }

    /// Clears the current view and hands the target to the background
    /// scanner; entries stream back through [`ScanEvent`] messages.
    fn begin_scan(&mut self, target: ScanTarget) {
        self.images.clear();
        self.load_errors.clear();
        self.textures.clear();
        self.texture_order.clear();
        self.failed_thumbs.clear();
        self.selected = None;
        self.needs_refresh = true;
        let generation = self.scanner.start(target, cache::default_cache_dir());
        self.scanning = Some(ScanState {
            generation,
            done: 0,
            total: None,
        });
    }

    /// Applies the scanner's streamed messages. Messages from a superseded
    /// scan (older generation) are dropped.
    fn poll_scanner(&mut self) {
        for update in self.scanner.poll() {
            let Some(state) = &mut self.scanning else {
                continue;
            };
            if update.generation != state.generation {
                continue;
            }
            match update.event {
                ScanEvent::Started { total } => state.total = Some(total),
                ScanEvent::Entry(entry) => {
                    state.done += 1;
                    self.images.push(*entry);
                    self.needs_refresh = true;
                }
                ScanEvent::Error(err) => {
                    state.done += 1;
                    self.load_errors.push(err);
                }
                ScanEvent::Done => self.scanning = None,
            }
        }
    }

    fn open_dropped(&mut self, dropped: Vec<PathBuf>) {
        if dropped.len() == 1 {
            self.open_target(dropped.into_iter().next().expect("checked len"));
            return;
        }
        let pngs: Vec<PathBuf> = dropped
            .into_iter()
            .filter(|p| {
                p.is_dir()
                    || p.extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
            })
            .collect();
        if pngs.is_empty() {
            // Nothing openable in the drop: keep the current folder instead of
            // silently resetting the view to an empty scan.
            self.flash = Some(("No PNG files in drop".to_string(), Instant::now()));
            return;
        }
        match pngs.iter().find(|p| p.is_dir()) {
            Some(dir) => self.open_folder(dir.clone()),
            None => self.open_files(pngs),
        }
    }

    // ---- filtering and sorting ----

    fn refresh(&mut self) {
        self.needs_refresh = false;
        let query = self.search.trim().to_lowercase();
        self.matches.clear();

        self.visible = (0..self.images.len())
            .filter(|&idx| {
                if query.is_empty() {
                    return true;
                }
                match match_image(&self.images[idx], &query) {
                    Some(info) => {
                        _ = self.matches.insert(idx, info);
                        true
                    }
                    None => false,
                }
            })
            .collect();

        if let Some((column, ascending)) = self.sort {
            self.sort_visible(column, ascending);
        }
        self.visible_paths = self
            .visible
            .iter()
            .map(|&idx| self.images[idx].path.clone())
            .collect();

        let selection_visible = self
            .selected
            .is_some_and(|sel| self.visible.contains(&sel));
        if !selection_visible {
            self.selected = self.visible.first().copied();
        }
    }

    /// Sorts `visible` by the active column. A pin sort decorates each entry
    /// with its first match once (one `rows_for_pin` per entry) instead of
    /// re-scanning both entries inside every comparison.
    fn sort_visible(&mut self, column: SortColumn, ascending: bool) {
        let images = &self.images;
        match column {
            SortColumn::Name => self.visible.sort_by(|&a, &b| {
                let ord = images[a].file_name_lc.cmp(&images[b].file_name_lc);
                if ascending { ord } else { ord.reverse() }
            }),
            SortColumn::Pin(pin_idx) => {
                let Some(pin) = self.pins.get(pin_idx) else {
                    return;
                };
                let use_titles = self.settings.comfyui_titles;
                let mut decorated: Vec<(Option<&crate::index::MetaRow>, usize)> = self
                    .visible
                    .iter()
                    .map(|&idx| {
                        let rows = images[idx].rows_for_pin(&pin.pattern, pin.mode, use_titles);
                        (rows.first().copied(), idx)
                    })
                    .collect();
                decorated.sort_by(|&(a, _), &(b, _)| {
                    let ord = compare_pin_matches(a, b);
                    if ascending { ord } else { ord.reverse() }
                });
                self.visible = decorated.into_iter().map(|(_, idx)| idx).collect();
            }
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context, actions: UiActions) {
        if let Some(idx) = actions.select {
            self.selected = Some(idx);
        }
        if let Some(column) = actions.sort {
            self.sort = match self.sort {
                Some((current, true)) if current == column => Some((column, false)),
                Some((current, false)) if current == column => None,
                _ => Some((column, true)),
            };
            self.needs_refresh = true;
        }
        if let Some(pin) = actions.toggle_pin {
            let existing = self
                .pins
                .iter()
                .position(|p| p.pattern.eq_ignore_ascii_case(&pin.pattern));
            match existing {
                Some(idx) => {
                    _ = self.pins.remove(idx);
                }
                None => self.pins.push(pin),
            }
            self.rename = None;
            self.sort = None;
            self.needs_refresh = true;
        }
        if let Some((idx, delta)) = actions.move_pin {
            let target = idx as isize + delta;
            if target >= 0 && (target as usize) < self.pins.len() {
                self.pins.swap(idx, target as usize);
                self.rename = None;
                self.sort = None;
                self.needs_refresh = true;
            }
        }
        if let Some(widths) = actions.column_widths {
            for (key, width) in widths {
                if self.settings.column_widths.get(&key) != Some(&width) {
                    _ = self.settings.column_widths.insert(key, width);
                }
            }
        }
        if let Some(idx) = actions.unpin
            && idx < self.pins.len()
        {
            _ = self.pins.remove(idx);
            self.rename = None;
            self.sort = None;
            self.needs_refresh = true;
        }
        if let Some(idx) = actions.start_rename
            && idx < self.pins.len()
        {
            self.rename = Some(PinRename {
                pin_index: idx,
                draft: self.pins[idx].label.clone().unwrap_or_default(),
                needs_focus: true,
            });
        }
        if let Some((idx, label)) = actions.apply_rename {
            if idx < self.pins.len() {
                let trimmed = label.trim();
                self.pins[idx].label = (!trimmed.is_empty()).then(|| trimmed.to_string());
            }
            self.rename = None;
        }
        if actions.cancel_rename {
            self.rename = None;
        }
        if let Some((idx, enabled)) = actions.set_pin_copy
            && idx < self.pins.len()
        {
            self.pins[idx].copy = Some(enabled);
        }
        if let Some((label, content)) = actions.copy {
            self.flash = Some((format!("Copied {label}"), Instant::now()));
            self.pending_copy = Some(content);
        }
        if let Some(idx) = actions.open_lightbox {
            self.open_lightbox(ctx, idx);
        }
        if let Some(expanded) = actions.set_tree_expanded {
            self.force_tree_expanded = Some(expanded);
        }
        if actions.toggle_settings {
            self.settings_open = !self.settings_open;
        }
        for path in actions.request_thumbs {
            // Already-in-flight paths are no-ops, so re-asking every frame is
            // cheap; paths whose decode already failed must never be re-asked.
            if !self.failed_thumbs.contains(&path) {
                self.thumbs.request(path);
            }
        }
        // Thumbnails drawn this frame become the most recently used: move them
        // to the back of `texture_order` so the cap evicts an off-screen
        // texture rather than one the user is looking at.
        if !actions.used_thumbs.is_empty() {
            touch_texture_order(&mut self.texture_order, &self.textures, &actions.used_thumbs);
        }
        if actions.open_folder_dialog
            && let Some(dir) = rfd::FileDialog::new().pick_folder()
        {
            self.open_folder(dir);
        }
        if actions.open_file_dialog
            && let Some(files) = rfd::FileDialog::new()
                .add_filter("PNG images", &["png"])
                .pick_files()
        {
            self.open_files(files);
        }
    }

    // ---- lightbox windows ----

    /// Opens a lightbox for `index`, or refuses past the cap with a flash. The
    /// window is handed the filtered list as it stands now, so a later search
    /// or re-sort cannot move it.
    fn open_lightbox(&mut self, ctx: &egui::Context, index: usize) {
        if !lightbox::can_open(self.lightboxes.len(), MAX_LIGHTBOXES) {
            self.flash = Some((
                format!("{MAX_LIGHTBOXES} lightbox windows already open"),
                Instant::now(),
            ));
            return;
        }
        let Some(entry) = self.images.get(index) else {
            return;
        };
        let detail = self.lightbox_detail(entry);
        let nav = LightboxNav {
            paths: Arc::clone(&self.visible_paths),
            // `visible_paths` is built from `visible` in that same order, so a
            // listed image sits at one position in both.
            cursor: self
                .visible
                .iter()
                .position(|&visible_idx| visible_idx == index)
                .unwrap_or(0),
        };
        // Cloning the handle retains the texture, so the `MAX_TEXTURES` sweep
        // dropping the map entry can never blank an open lightbox.
        let texture = self.textures.get(&detail.path).cloned();
        let id = LightboxId(self.next_lightbox_id);
        self.next_lightbox_id += 1;
        let source_size = detail.source_size;
        let path = detail.path.clone();
        self.lightboxes.push(Lightbox {
            id,
            shared: Arc::new(Mutex::new(LightboxShared {
                texture,
                detail,
                status: LoadStatus::Loading,
                decoded_size: [0, 0],
                scene_rect: egui::Rect::ZERO,
                pending_view: None,
                fit_mode: true,
                zoom: 1.0,
                nav,
                nav_request: None,
                copied: None,
                close_requested: false,
            })),
        });
        self.full
            .request(id, path, source_size, Self::texture_side_limit(ctx));
    }

    /// What a lightbox window shows about one listed image, strip included.
    fn lightbox_detail(&self, entry: &ImageEntry) -> LightboxDetail {
        let candidates = strip_candidates(entry, &self.pins, self.settings.comfyui_titles);
        LightboxDetail {
            path: entry.path.clone(),
            file_name: entry.file_name.clone(),
            source_size: [entry.width, entry.height],
            file_size: entry.file_size,
            strip: lightbox::build_strip(
                &candidates,
                lightbox::MAX_STRIP_ITEMS,
                lightbox::MAX_STRIP_CHARS,
            ),
        }
    }

    /// [`Self::lightbox_detail`] for a path a window navigated to. A frozen
    /// list outlives the scan that produced it, and the file itself still
    /// displays, so a path the app no longer lists falls back to its name.
    fn lightbox_detail_for_path(&self, path: &Path) -> LightboxDetail {
        match self.images.iter().find(|entry| entry.path == path) {
            Some(entry) => self.lightbox_detail(entry),
            None => LightboxDetail {
                file_name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: path.to_path_buf(),
                source_size: [0, 0],
                file_size: 0,
                strip: Vec::new(),
            },
        }
    }

    /// The largest texture side the driver accepts, held under the guard the
    /// decoder enforces. The worker has no context, so this is read here.
    fn texture_side_limit(ctx: &egui::Context) -> u32 {
        ctx.input(|input| input.max_texture_side)
            .min(full::TEXTURE_SIDE_GUARD as usize) as u32
    }

    /// Drops every lightbox whose window asked to close, releases the decode
    /// each one had reserved, then moves the survivors that asked to navigate.
    fn sync_lightboxes(&mut self, ctx: &egui::Context) {
        let closed: Vec<LightboxId> = self
            .lightboxes
            .iter()
            .filter(|lightbox| {
                // The guard drops at the end of this expression, before
                // `retain` drops the lightbox, so nothing is freed under its
                // own lock.
                lightbox
                    .shared
                    .lock()
                    .expect("lightbox state poisoned")
                    .close_requested
            })
            .map(|lightbox| lightbox.id)
            .collect();
        if !closed.is_empty() {
            self.lightboxes
                .retain(|lightbox| !closed.contains(&lightbox.id));
            for id in closed {
                self.full.forget(id);
            }
        }
        self.apply_lightbox_nav(ctx);
    }

    /// Moves each window that asked for a step onto its new image: new detail,
    /// cleared texture, fresh decode. Each one is woken explicitly, because the
    /// child pass that wrote the request has already finished and the window
    /// would otherwise paint the old image for the whole length of the decode.
    fn apply_lightbox_nav(&mut self, ctx: &egui::Context) {
        // The snapshot travels with the request: the detail below is built
        // outside the lock, and the window's own next pass may run in between.
        let steps: Vec<(LightboxId, Arc<[PathBuf]>, usize)> = self
            .lightboxes
            .iter()
            .filter_map(|lightbox| {
                let mut state = lightbox.shared.lock().expect("lightbox state poisoned");
                let cursor = state.nav_request.take()?;
                Some((lightbox.id, Arc::clone(&state.nav.paths), cursor))
            })
            .collect();
        if steps.is_empty() {
            return;
        }

        let max_side = Self::texture_side_limit(ctx);
        for (id, paths, cursor) in steps {
            // A zero step is the clamp: it answers `None` only for an empty
            // list, which is the one case with nothing to index.
            let Some(cursor) = lightbox::step_cursor(cursor, paths.len(), 0) else {
                continue;
            };
            let path = paths[cursor].clone();
            let detail = self.lightbox_detail_for_path(&path);
            let source_size = detail.source_size;
            let Some(lightbox) = self.lightboxes.iter().find(|open| open.id == id) else {
                continue;
            };
            let shared = Arc::clone(&lightbox.shared);
            {
                let mut state = shared.lock().expect("lightbox state poisoned");
                state.detail = detail;
                state.nav.cursor = cursor;
                // Dropping the old texture is what puts the spinner up on the
                // window's next pass rather than when the decode lands.
                state.texture = None;
                state.decoded_size = [0, 0];
                state.status = LoadStatus::Loading;
                state.pending_view = Some(ViewCommand::Fit);
                state.fit_mode = true;
            }
            self.full.request(id, path, source_size, max_side);
            ctx.request_repaint_of(id.viewport_id());
        }
    }

    /// Hands each finished decode to the window that asked for it. A result
    /// whose window has closed, or whose path that window no longer wants, is
    /// dropped.
    fn poll_full(&mut self, ctx: &egui::Context) {
        for result in self.full.poll() {
            let Some(lightbox) = self.lightboxes.iter().find(|open| open.id == result.id) else {
                continue;
            };
            let shared = Arc::clone(&lightbox.shared);
            let wanted = shared
                .lock()
                .expect("lightbox state poisoned")
                .detail
                .path
                .clone();
            if wanted != result.path {
                continue;
            }
            // Upload outside the lock: a child pass blocks on this same mutex,
            // and `load_texture` is the slow part.
            let (texture, decoded_size, status) = match result.outcome {
                FullOutcome::Superseded => continue,
                FullOutcome::Ready { image, size } => (
                    Some(ctx.load_texture(
                        format!("lightbox-{}", result.id.0),
                        image,
                        TextureOptions::LINEAR,
                    )),
                    size,
                    LoadStatus::Ready,
                ),
                FullOutcome::Refused(message) => (None, [0, 0], LoadStatus::Refused(message)),
                FullOutcome::Failed(message) => (None, [0, 0], LoadStatus::Failed(message)),
            };
            {
                let mut state = shared.lock().expect("lightbox state poisoned");
                if let Some(texture) = texture {
                    state.texture = Some(texture);
                    state.decoded_size = decoded_size;
                    // The decode replaces a stand-in thumbnail, so the window
                    // starts over from a fit rather than keeping that view.
                    state.pending_view = Some(ViewCommand::Fit);
                    state.fit_mode = true;
                }
                state.status = status;
            }
            ctx.request_repaint_of(result.id.viewport_id());
        }
    }

    /// Re-registers every surviving lightbox. A window not registered in a
    /// pass is destroyed at the end of it, so this must run every frame.
    fn show_lightboxes(&self, ctx: &egui::Context) {
        for lightbox in &self.lightboxes {
            // Clone the title and drop the guard first: under the embedded
            // fallback `show_viewport_deferred` runs the callback inline, and
            // the callback locks this same mutex.
            let title = lightbox
                .shared
                .lock()
                .expect("lightbox state poisoned")
                .detail
                .file_name
                .clone();
            let shared = Arc::clone(&lightbox.shared);
            ctx.show_viewport_deferred(
                lightbox.id.viewport_id(),
                egui::ViewportBuilder::default()
                    .with_title(title)
                    .with_inner_size(lightbox::INITIAL_SIZE),
                move |ui, class| lightbox::lightbox_ui(ui, class, &shared),
            );
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(selected) = self.selected else {
            self.selected = self.visible.first().copied();
            return;
        };
        let Some(pos) = self.visible.iter().position(|&idx| idx == selected) else {
            return;
        };
        let new_pos = pos as isize + delta;
        if new_pos >= 0 && (new_pos as usize) < self.visible.len() {
            self.selected = Some(self.visible[new_pos as usize]);
        }
    }

    fn take_pending_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }
}

fn match_image(entry: &ImageEntry, query: &str) -> Option<MatchInfo> {
    let mut first: Option<String> = None;
    let mut count = 0usize;

    if entry.file_name_lc.contains(query) {
        first = Some(format!("name = {}", entry.file_name));
        count += 1;
    }
    for row in &entry.rows {
        if index::row_matches(row, query) {
            if first.is_none() {
                first = Some(format!("{} = {}", row.key, single_line(&row.value, 200)));
            }
            count += 1;
        }
    }
    first.map(|first| MatchInfo { first, count })
}

/// Orders two entries by their first match for the sorted pin column; entries
/// with no match sort last (before the ascending/descending flip).
fn compare_pin_matches(
    a: Option<&crate::index::MetaRow>,
    b: Option<&crate::index::MetaRow>,
) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => compare_rows(x, y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_rows(a: &crate::index::MetaRow, b: &crate::index::MetaRow) -> Ordering {
    match (a.sort_key, b.sort_key) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => compare_values(&a.value, &b.value),
    }
}

/// Numeric comparison when both values parse as numbers, string otherwise.
fn compare_values(a: &str, b: &str) -> Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.cmp(b),
    }
}

/// Leading `max_chars` characters, with an ellipsis when anything was cut.
fn excerpt(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    // One pass: the iterator is left just past the kept characters, so a
    // remaining character means the text was truncated.
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Excerpt for a single-line (truncating) label, where a line break would
/// otherwise end the visible text early.
pub fn single_line(text: &str, max_chars: usize) -> String {
    excerpt(text, max_chars).replace(['\n', '\r'], " ")
}

/// Characters of the monospace text style that fit in `width`, plus slack so
/// the label's own truncation places the ellipsis. Without this cap a
/// multi-kilobyte prompt would be laid out in full to show one line of it.
fn fitting_chars(ui: &Ui, width: f32) -> usize {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    // A zero width means the font has no '0' glyph. 1pt per character then
    // caps generously instead of to nothing.
    let glyph = ui
        .ctx()
        .fonts_mut(|fonts| fonts.glyph_width(&font, '0'))
        .max(1.0);
    // An unbounded container would make `width` infinite, and `as usize`
    // saturates rather than overflowing, so the clamp is what bounds layout.
    ((width.max(0.0) / glyph).ceil() as usize)
        .saturating_add(8)
        .min(MAX_VALUE_CHARS)
}

/// Storage key for the Name column's width. The `col:` and `pin:` namespaces
/// keep a pin whose pattern is literally "name" from claiming this entry.
const NAME_COLUMN_KEY: &str = "col:name";

/// Storage key for a pin column's width. Keyed by pattern, not position, so a
/// width follows its pin through a reorder.
fn pin_column_key(pin: &Pin) -> String {
    format!("pin:{}", pin.pattern.to_lowercase())
}

/// Display form of a pin: an explicit label wins; otherwise bare keys show
/// as-is and exact-path pins show a location marker plus the final segment
/// (full pattern in tooltips).
fn pin_label(pin: &Pin) -> String {
    if let Some(label) = &pin.label {
        return label.clone();
    }
    match pin.pattern.rsplit_once('.') {
        Some((_, last)) => format!("⌖ {last}"),
        None => pin.pattern.clone(),
    }
}

/// The lightbox strip's input: one entry per (pin, match) pair in
/// pin-then-document order, each carrying its pin's copy flag.
/// [`lightbox::build_strip`] is what drops the copy-disabled ones, so the flag
/// travels with the value rather than being filtered here.
fn strip_candidates(
    entry: &ImageEntry,
    pins: &[Pin],
    use_titles: bool,
) -> Vec<(String, bool, String)> {
    let mut candidates = Vec::new();
    for pin in pins {
        let label = pin_label(pin);
        let copies = pin_copies(pin);
        for row in entry.rows_for_pin(&pin.pattern, pin.mode, use_titles) {
            candidates.push((label.clone(), copies, row.value.clone()));
        }
    }
    candidates
}

/// Full-value hover text, capped so multi-kilobyte prompts don't fill the
/// screen. The copy actions always carry the complete value.
fn tooltip_excerpt(value: &str) -> String {
    const MAX_CHARS: usize = 1200;
    let total = value.chars().count();
    if total <= MAX_CHARS {
        return value.to_string();
    }
    let head: String = value.chars().take(MAX_CHARS).collect();
    format!("{head}…\n\n({} more characters)", total - MAX_CHARS)
}

/// Widens the hovered scroll bar and lengthens its handle, since egui's
/// defaults (10pt wide, 12pt minimum handle) are hard to see and hard to hit.
/// The bar also reserves its own width, because egui senses it over the full
/// `bar_width` even while painting the dormant 2pt sliver, and an overlapping
/// strip would swallow clicks on the value labels underneath. Space is
/// reserved only while an area is actually scrollable.
fn configure_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let scroll = &mut style.spacing.scroll;
        scroll.bar_width = 16.0;
        scroll.floating_allocated_width = 16.0;
        // egui's own style editor accepts up to 32.
        scroll.handle_min_length = 32.0;
    });
}

/// Stable color per match ordinal, so "first cfg" and "second cfg" (e.g.
/// generation vs upscale) are visually distinct everywhere they appear.
fn ordinal_color(ui: &Ui, ordinal: usize) -> Color32 {
    match ordinal {
        0 => ui.visuals().text_color(),
        1 => Color32::from_rgb(255, 179, 102), // orange: second match
        _ => Color32::from_rgb(122, 184, 255), // blue: third and later
    }
}

impl eframe::App for ViewerApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, PINS_STORAGE_KEY, &self.pins);
        eframe::set_value(storage, SETTINGS_STORAGE_KEY, &self.settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let force_tree = self.force_tree_expanded.take();

        // Lightbox frame order is fixed: `sync_lightboxes` here, then the
        // panels, then `apply_actions`, then `show_lightboxes` last. Syncing
        // first means a lightbox closed this frame is simply not re-registered,
        // so egui destroys its window at the end of this same pass. Applying
        // actions before showing means a lightbox opened by this frame's click
        // is registered in this same pass, instead of waiting for an unrelated
        // repaint that nothing schedules.
        self.sync_lightboxes(&ctx);

        self.poll_scanner();

        // Upload finished thumbnails. Eviction runs after every insert, not
        // once per batch: a stalled UI thread (blocked file dialog, window
        // drag) lets thousands of queued decodes arrive in one poll, and a
        // batch-level sweep would upload them all before trimming — the exact
        // multi-gigabyte spike `MAX_TEXTURES` exists to prevent.
        let finished = self.thumbs.poll();
        if !finished.is_empty() {
            let current: HashSet<&Path> = self.images.iter().map(|e| e.path.as_path()).collect();
            for (path, image) in finished {
                // A decode still in flight when the folder changed belongs to
                // no listed image; dropping it keeps `textures` in step.
                if !current.contains(path.as_path()) {
                    continue;
                }
                let Some(image) = image else {
                    _ = self.failed_thumbs.insert(path);
                    continue;
                };
                let name = path.to_string_lossy().into_owned();
                let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
                if self.textures.insert(path.clone(), texture).is_none() {
                    self.texture_order.push_back(path);
                }
                evict_stale_textures(&mut self.texture_order, &mut self.textures, MAX_TEXTURES);
            }
        }

        self.poll_full(&ctx);

        // Dropped files.
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.open_dropped(dropped);
        }

        // Keyboard navigation when no widget has focus.
        let no_focus = ctx.memory(|m| m.focused().is_none());
        if no_focus {
            let delta = ctx.input(|input| {
                if input.key_pressed(Key::ArrowDown) {
                    1
                } else if input.key_pressed(Key::ArrowUp) {
                    -1
                } else {
                    0
                }
            });
            if delta != 0 {
                self.move_selection(delta);
            }
        }

        if self.needs_refresh {
            self.refresh();
        }

        let mut actions = UiActions::default();

        _ = egui::Panel::top("toolbar").show(ui, |ui| {
            self.toolbar_ui(ui, &mut actions);
        });

        _ = egui::Panel::bottom("status").show(ui, |ui| {
            self.status_ui(ui);
        });

        _ = egui::Panel::left("metadata_panel")
            .resizable(true)
            .default_size(400.0)
            .min_size(280.0)
            .show(ui, |ui| {
                self.metadata_panel_ui(ui, &mut actions, force_tree);
            });

        _ = egui::CentralPanel::default().show(ui, |ui| {
            if !self.images.is_empty() {
                self.table_ui(ui, &mut actions);
            } else if let Some(state) = &self.scanning {
                loading_state_ui(ui, state);
            } else {
                empty_state_ui(ui, &mut actions);
            }
        });

        if self.settings_open {
            // `open` is a local so the window's close button doesn't need a
            // second mutable borrow of self alongside the checkbox binding.
            let mut open = self.settings_open;
            let mut changed = false;
            _ = egui::Window::new("Settings")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.set_max_width(320.0);
                    changed |= ui
                        .checkbox(&mut self.settings.comfyui_titles, "ComfyUI title pins")
                        .changed();
                    _ = ui.label(
                        RichText::new(
                            "A pin can match a ComfyUI node by part of its title and \
                             show that node's input values. Node titles stay the same \
                             across workflows where node ids differ. Typed pins match \
                             keys and titles. Pin a titled value from the tree to pin \
                             its node title. Files without ComfyUI structure are \
                             unaffected.",
                        )
                        .weak()
                        .small(),
                    );
                });
            self.settings_open = open;
            if changed {
                // Title matching feeds pin columns and pin-column sorts.
                self.needs_refresh = true;
            }
        }

        self.apply_actions(&ctx, actions);
        self.show_lightboxes(&ctx);
        if let Some(content) = self.take_pending_copy() {
            ctx.copy_text(content);
        }
        if let Some((_, when)) = self.flash {
            if when.elapsed().as_secs_f32() > FLASH_SECONDS {
                self.flash = None;
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
    }
}

// ---- panel rendering ----

impl ViewerApp {
    fn toolbar_ui(&mut self, ui: &mut Ui, actions: &mut UiActions) {
        _ = ui.horizontal_wrapped(|ui| {
            if ui.button("📂 Open Folder…").clicked() {
                actions.open_folder_dialog = true;
            }
            if ui.button("🖼 Open Files…").clicked() {
                actions.open_file_dialog = true;
            }
            _ = ui.separator();

            _ = ui.label("Search:");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("key, value, or dotted.path…")
                    .desired_width(220.0),
            );
            if response.changed() {
                self.needs_refresh = true;
            }
            if ui
                .add_enabled(!self.search.is_empty(), Button::new("✕"))
                .on_hover_text("Clear search")
                .clicked()
            {
                self.search.clear();
                self.needs_refresh = true;
            }
            _ = ui.separator();

            _ = ui.label("Pins:");
            for (idx, pin) in self.pins.iter().enumerate() {
                let mode_note = match pin.mode {
                    PinMode::Title => "ComfyUI node title",
                    _ if pin.pattern.contains('.') => "exact path",
                    PinMode::Key => "key",
                    PinMode::Auto => "key or node title",
                };
                let copies = pin_copies(pin);
                let chip = ui.small_button(pin_label(pin)).on_hover_text(format!(
                    "{}  ({mode_note})\n\nColumn in the table, in this order.\nRight-click to rename, reorder, or unpin.",
                    pin.pattern,
                ));
                _ = chip.context_menu(|ui| {
                    if ui.button("✏ Rename…").clicked() {
                        actions.start_rename = Some(idx);
                        ui.close();
                    }
                    let copy_entry = if copies {
                        "📋 Hide copy button"
                    } else {
                        "📋 Show copy button"
                    };
                    if ui.button(copy_entry).clicked() {
                        actions.set_pin_copy = Some((idx, !copies));
                        ui.close();
                    }
                    if ui.button("◀ Move left").clicked() {
                        actions.move_pin = Some((idx, -1));
                        ui.close();
                    }
                    if ui.button("▶ Move right").clicked() {
                        actions.move_pin = Some((idx, 1));
                        ui.close();
                    }
                    if ui.button("✕ Unpin").clicked() {
                        actions.unpin = Some(idx);
                        ui.close();
                    }
                });
            }
            let pin_hint = if self.settings.comfyui_titles {
                "add pin (key or node title)…"
            } else {
                "add pin…"
            };
            let pin_edit = ui.add(
                egui::TextEdit::singleline(&mut self.new_pin)
                    .hint_text(pin_hint)
                    .desired_width(120.0),
            );
            let submitted =
                pin_edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if submitted && !self.new_pin.trim().is_empty() {
                actions.toggle_pin = Some(Pin::auto(self.new_pin.trim()));
                self.new_pin.clear();
                pin_edit.request_focus();
            }
            if ui.button("⚙").on_hover_text("Settings").clicked() {
                actions.toggle_settings = true;
            }
        });
        self.rename_row_ui(ui, actions);
    }

    /// Label editor row, shown under the toolbar while a pin rename is open.
    fn rename_row_ui(&mut self, ui: &mut Ui, actions: &mut UiActions) {
        let Some(rename) = &mut self.rename else {
            return;
        };
        let pattern = self
            .pins
            .get(rename.pin_index)
            .map(|p| p.pattern.clone())
            .unwrap_or_default();
        _ = ui.horizontal(|ui| {
            _ = ui.label(format!("Label for '{pattern}':"));
            let edit = ui.add(
                egui::TextEdit::singleline(&mut rename.draft)
                    .hint_text("display name (empty = default)")
                    .desired_width(200.0),
            );
            if rename.needs_focus {
                edit.request_focus();
                rename.needs_focus = false;
            }
            let submitted = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let apply_clicked = ui.small_button("Apply").clicked();
            let cancel_clicked = ui.small_button("Cancel").clicked();
            if submitted || apply_clicked {
                actions.apply_rename = Some((rename.pin_index, rename.draft.clone()));
            }
            if cancel_clicked || ui.input(|i| i.key_pressed(Key::Escape)) {
                actions.cancel_rename = true;
            }
        });
    }

    fn status_ui(&mut self, ui: &mut Ui) {
        _ = ui.horizontal(|ui| {
            _ = match &self.folder {
                Some(folder) => ui.label(folder.display().to_string()),
                None => ui.label(RichText::new("no folder").weak()),
            };
            _ = ui.separator();
            _ = ui.label(format!(
                "{} of {} images",
                self.visible.len(),
                self.images.len()
            ));
            if let Some(state) = &self.scanning {
                _ = ui.separator();
                _ = ui.spinner();
                _ = match state.total {
                    Some(total) => ui.label(format!(
                        "reading metadata {} / {total}…",
                        state.done
                    )),
                    None => ui.label("listing folder…"),
                };
            }
            if self.thumbs.pending() > 0 {
                _ = ui.separator();
                _ = ui.spinner();
                _ = ui.label(format!("decoding {} thumbnails…", self.thumbs.pending()));
            }
            if !self.load_errors.is_empty() {
                _ = ui.separator();
                _ = ui.colored_label(ui.visuals().warn_fg_color, format!(
                    "{} unreadable",
                    self.load_errors.len()
                ))
                .on_hover_text(self.load_errors.join("\n"));
            }
            _ = ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if let Some((message, _)) = &self.flash {
                    _ = ui.colored_label(ui.visuals().hyperlink_color, message);
                }
            });
        });
    }

    fn metadata_panel_ui(&mut self, ui: &mut Ui, actions: &mut UiActions, force_tree: Option<bool>) {
        let Some(selected) = self.selected else {
            _ = ui.centered_and_justified(|ui| {
                _ = ui.label(RichText::new("Select an image").weak());
            });
            return;
        };

        // Preview block.
        let preview_path = self.images[selected].path.clone();
        if let Some(texture) = self.textures.get(&preview_path) {
            actions.used_thumbs.push(preview_path.clone());
            let available = ui.available_width();
            _ = ui.vertical_centered(|ui| {
                let preview = ui
                    .add(
                        egui::Image::new(texture)
                            .max_size(egui::vec2(available, 240.0))
                            .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Open in a window");
                if preview.clicked() {
                    actions.open_lightbox = Some(selected);
                }
            });
        } else {
            actions.request_thumbs.push(preview_path.clone());
            _ = ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                _ = ui.spinner();
                ui.add_space(100.0);
            });
        }
        ui.add_space(4.0);
        _ = ui.horizontal_wrapped(|ui| {
            _ = ui.label(RichText::new(&self.images[selected].file_name).strong());
        });
        _ = ui.horizontal(|ui| {
            let entry = &self.images[selected];
            _ = ui.label(format!(
                "{}×{}  ·  {}",
                entry.width,
                entry.height,
                human_size(entry.file_size)
            ));
            if ui.small_button("copy path").clicked() {
                actions.copy = Some(("path".to_string(), entry.path.display().to_string()));
            }
        });
        _ = ui.separator();

        // View mode controls. The tree stays the structural view during a
        // search (pruned to matches); "Pinned only" swaps in the pinned list.
        let query = self.search.trim().to_lowercase();
        let searching = !query.is_empty();
        let tree_visible = !self.pinned_only;
        let mut find_requested = false;
        _ = ui.horizontal_wrapped(|ui| {
            let checkbox = ui.checkbox(&mut self.pinned_only, "Pinned only");
            if checkbox.changed() {
                ui.ctx().request_repaint();
            }
            _ = ui.separator();
            if ui
                .add_enabled(tree_visible, Button::new("Expand all").small())
                .clicked()
            {
                actions.set_tree_expanded = Some(true);
            }
            if ui
                .add_enabled(tree_visible, Button::new("Collapse all").small())
                .clicked()
            {
                actions.set_tree_expanded = Some(false);
            }
            _ = ui.separator();
            _ = ui.label("Find:");
            let find_edit = ui.add_enabled(
                tree_visible,
                egui::TextEdit::singleline(&mut self.find_text)
                    .hint_text("jump to field…")
                    .desired_width(110.0),
            );
            if find_edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                find_requested = true;
                // Keep focus so Enter cycles onward through the matches.
                find_edit.request_focus();
            }
            if searching {
                let note = if self.pinned_only {
                    "search filters the table only"
                } else {
                    "filtered by search"
                };
                _ = ui.label(RichText::new(note).weak());
            }
        });
        ui.add_space(2.0);

        // Resolve a find request into this frame's jump target: the first
        // (then next, on repeated Enter) row matching the find text; a hit
        // inside `_meta` (usually the title) retargets to the owning node so
        // the whole node — its inputs included — opens.
        let mut jump: Option<String> = None;
        if find_requested {
            let find_query = self.find_text.trim().to_lowercase();
            if !find_query.is_empty() {
                if find_query != self.find_last_query {
                    self.find_last_query = find_query.clone();
                    self.find_index = 0;
                }
                let matched_paths: Vec<&str> = self.images[selected]
                    .rows
                    .iter()
                    .filter(|row| index::row_matches(row, &find_query))
                    .map(|row| row.path.as_str())
                    .collect();
                if matched_paths.is_empty() {
                    self.flash = Some((
                        format!("No match for '{}'", self.find_text.trim()),
                        Instant::now(),
                    ));
                } else {
                    let target = matched_paths[self.find_index % matched_paths.len()];
                    let node = match target.find("._meta.") {
                        Some(idx) => &target[..idx],
                        None => target,
                    };
                    jump = Some(node.to_string());
                    self.find_index = self.find_index.wrapping_add(1);
                }
            }
        }

        let entry = &self.images[selected];
        _ = ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.pinned_only {
                    pinned_list_ui(ui, entry, &self.pins, self.settings.comfyui_titles, actions);
                } else {
                    let tree_ctx = TreeCtx {
                        entry,
                        pins: &self.pins,
                        force_open: force_tree,
                        query: &query,
                        use_titles: self.settings.comfyui_titles,
                        jump: jump.as_deref(),
                    };
                    chunk_tree_ui(ui, &tree_ctx, actions);
                }
            });
    }

    fn table_ui(&mut self, ui: &mut Ui, actions: &mut UiActions) {
        let has_search = !self.search.trim().is_empty();
        let sort = self.sort;

        // Many pins can exceed the panel width; let the whole table scroll
        // sideways rather than clipping the rightmost columns away.
        _ = ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.table_inner_ui(ui, has_search, sort, actions);
            });
    }

    fn table_inner_ui(
        &self,
        ui: &mut Ui,
        has_search: bool,
        sort: Option<(SortColumn, bool)>,
        actions: &mut UiActions,
    ) {
        // Inputs: the saved width per column, and a layout id that changes with
        // the pin set. egui_extras discards its own widths on any column-set
        // change, so the salt makes it start fresh from the saved widths rather
        // than from a stale set it would otherwise reuse after a copy toggle.
        let saved = &self.settings.column_widths;
        let layout_id: String = self
            .pins
            .iter()
            .map(|pin| format!("{}{}", pin_copies(pin) as u8, pin_column_key(pin)))
            .collect::<Vec<_>>()
            .join("\u{1f}");
        let mut column_keys: Vec<String> = Vec::with_capacity(self.pins.len() + 1);
        column_keys.push(NAME_COLUMN_KEY.to_string());

        let name_width = saved.get(NAME_COLUMN_KEY).copied().unwrap_or(230.0);
        let mut builder = TableBuilder::new(ui)
            .id_salt(("image_table", layout_id))
            .striped(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(Align::Center))
            .column(Column::exact(THUMB_CELL + 8.0))
            .column(Column::initial(name_width).at_least(120.0).clip(true).resizable(true));
        for pin in &self.pins {
            let key = pin_column_key(pin);
            // A frameless copy button plus a value does not fit 110pt. A width
            // the user chose outranks either default.
            let default = if pin_copies(pin) { 150.0 } else { 110.0 };
            let width = saved.get(&key).copied().unwrap_or(default);
            column_keys.push(key);
            builder = builder
                .column(Column::initial(width).at_least(60.0).clip(true).resizable(true));
        }
        if has_search {
            builder = builder.column(Column::remainder().clip(true));
        }

        _ = builder
            .header(22.0, |mut header| {
                _ = header.col(|_ui| {});
                _ = header.col(|ui| sort_header_ui(ui, "Name", SortColumn::Name, sort, actions));
                for (idx, pin) in self.pins.iter().enumerate() {
                    _ = header.col(|ui| {
                        sort_header_ui(ui, &pin_label(pin), SortColumn::Pin(idx), sort, actions)
                    });
                }
                if has_search {
                    _ = header.col(|ui| {
                        _ = ui.strong("Match");
                    });
                }
            })
            .body(|body| {
                // Reflects the previous frame's drag, which is soon enough to
                // outlive a restart and invisible to the user.
                actions.column_widths = Some(
                    column_keys
                        .into_iter()
                        .zip(body.widths().iter().skip(1).copied())
                        .collect(),
                );
                body.rows(ROW_HEIGHT, self.visible.len(), |mut row| {
                    let image_idx = self.visible[row.index()];
                    let entry = &self.images[image_idx];
                    row.set_selected(self.selected == Some(image_idx));

                    _ = row.col(|ui| {
                        match self.textures.get(&entry.path) {
                            Some(texture) => {
                                let thumb = ui
                                    .add(
                                        egui::Image::new(texture)
                                            .max_size(egui::vec2(THUMB_CELL, THUMB_CELL))
                                            .sense(Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text("Open in a window");
                                // A clickable image takes the hit-test from the
                                // row, so the row's own click never fires here
                                // and this must select the row itself.
                                if thumb.clicked() {
                                    actions.open_lightbox = Some(image_idx);
                                    actions.select = Some(image_idx);
                                }
                                actions.used_thumbs.push(entry.path.clone());
                            }
                            None => {
                                actions.request_thumbs.push(entry.path.clone());
                                _ = ui.spinner();
                            }
                        }
                    });
                    _ = row.col(|ui| {
                        _ = ui.add(Label::new(&entry.file_name).truncate())
                            .on_hover_text(entry.path.display().to_string());
                    });
                    for pin in &self.pins {
                        _ = row.col(|ui| {
                            pin_cell_ui(ui, entry, pin, self.settings.comfyui_titles, actions);
                        });
                    }
                    if has_search {
                        _ = row.col(|ui| {
                            if let Some(info) = self.matches.get(&image_idx) {
                                let text = if info.count > 1 {
                                    format!("{}  (+{})", info.first, info.count - 1)
                                } else {
                                    info.first.clone()
                                };
                                _ = ui.add(Label::new(RichText::new(text).monospace()).truncate());
                            }
                        });
                    }

                    if row.response().clicked() {
                        actions.select = Some(image_idx);
                    }
                });
            });
    }
}

// ---- thumbnail cache bookkeeping (pure: no egui borrow) ----

// Generic over the cached value so the ordering rules can be tested without a
// render context to build a `TextureHandle` with.

/// Moves the paths drawn this frame to the back of `order`, de-duplicated and
/// in draw order. Paths absent from `live` are ignored, keeping `order` and
/// `live` one-to-one.
fn touch_texture_order<T>(
    order: &mut VecDeque<PathBuf>,
    live: &HashMap<PathBuf, T>,
    drawn: &[PathBuf],
) {
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut ordered: Vec<PathBuf> = Vec::with_capacity(drawn.len());
    for path in drawn {
        if live.contains_key(path) && seen.insert(path.as_path()) {
            ordered.push(path.clone());
        }
    }
    order.retain(|path| !seen.contains(path.as_path()));
    order.extend(ordered);
}

/// Drops least-recently-drawn entries until at most `cap` remain.
fn evict_stale_textures<T>(
    order: &mut VecDeque<PathBuf>,
    live: &mut HashMap<PathBuf, T>,
    cap: usize,
) {
    while order.len() > cap {
        let Some(oldest) = order.pop_front() else {
            break;
        };
        _ = live.remove(&oldest);
    }
}

// ---- widget helpers (free functions: no app borrow) ----

/// Central-panel placeholder while a scan runs and nothing has arrived yet.
fn loading_state_ui(ui: &mut Ui, state: &ScanState) {
    _ = ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.35);
        _ = ui.spinner();
        ui.add_space(8.0);
        let text = match state.total {
            Some(total) => format!("Reading metadata… {} / {total}", state.done),
            None => "Reading folder…".to_string(),
        };
        _ = ui.label(RichText::new(text).size(16.0).weak());
    });
}

fn empty_state_ui(ui: &mut Ui, actions: &mut UiActions) {
    _ = ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.35);
        _ = ui.label(RichText::new("Drop a folder or PNG files here").size(20.0).weak());
        ui.add_space(8.0);
        if ui.button("📂 Open Folder…").clicked() {
            actions.open_folder_dialog = true;
        }
    });
}

fn sort_header_ui(
    ui: &mut Ui,
    label: &str,
    column: SortColumn,
    current: Option<(SortColumn, bool)>,
    actions: &mut UiActions,
) {
    let arrow = match current {
        Some((active, true)) if active == column => " ▲",
        Some((active, false)) if active == column => " ▼",
        _ => "",
    };
    let text = RichText::new(format!("{label}{arrow}")).strong();
    if ui
        .add(Button::new(text).frame(false))
        .on_hover_text("Click to sort")
        .clicked()
    {
        actions.sort = Some(column);
    }
}

/// Table cell for one pin: every match in document order, color-coded by
/// ordinal so repeated keys (generation vs upscale cfg) stay tellable apart.
fn pin_cell_ui(
    ui: &mut Ui,
    entry: &ImageEntry,
    pin: &Pin,
    use_titles: bool,
    actions: &mut UiActions,
) {
    let matches = entry.rows_for_pin(&pin.pattern, pin.mode, use_titles);
    if matches.is_empty() {
        _ = ui.label(RichText::new("—").weak());
        return;
    }
    let copies = pin_copies(pin);
    for (i, row) in matches.iter().take(MAX_CELL_MATCHES).enumerate() {
        if i > 0 {
            _ = ui.label(RichText::new("·").weak());
        }
        if copies
            && ui
                .add(Button::new("📋").small().frame(false))
                .on_hover_text("Copy full value")
                .clicked()
        {
            actions.copy = Some((pin_label(pin), row.value.clone()));
        }
        let color = ordinal_color(ui, i);
        let text = single_line(&row.value, fitting_chars(ui, ui.available_width()));
        _ = ui
            .add(Label::new(RichText::new(text).monospace().color(color)))
            // Built inside the hover closure: every visible cell would
            // otherwise format every match on every frame.
            .on_hover_ui(|ui| {
                ui.set_max_width(ui.spacing().tooltip_width);
                let tooltip: String = matches
                    .iter()
                    .enumerate()
                    .map(|(n, r)| format!("{}. {} = {}", n + 1, r.path, single_line(&r.value, 200)))
                    .collect::<Vec<_>>()
                    .join("\n");
                _ = ui.label(tooltip);
            });
    }
    if matches.len() > MAX_CELL_MATCHES {
        _ = ui.label(RichText::new(format!("+{}", matches.len() - MAX_CELL_MATCHES)).weak());
    }
}

/// Pinned-only view: one card per pin — pin name, then each matching value
/// (ordinal-colored, copy button + click to copy) with its location underneath.
fn pinned_list_ui(
    ui: &mut Ui,
    entry: &ImageEntry,
    pins: &[Pin],
    use_titles: bool,
    actions: &mut UiActions,
) {
    if pins.is_empty() {
        _ = ui.label(RichText::new("Nothing pinned. Pin keys from the tree view.").weak());
        return;
    }
    for pin in pins {
        _ = ui.horizontal(|ui| {
            _ = ui.label(RichText::new(pin_label(pin)).strong());
            let tag = match pin.mode {
                PinMode::Title => Some("node title"),
                _ if pin.pattern.contains('.') => Some("exact path"),
                _ => None,
            };
            if let Some(tag) = tag {
                _ = ui.label(RichText::new(tag).small().weak())
                    .on_hover_text(pin.pattern.clone());
            }
        });
        let matches = entry.rows_for_pin(&pin.pattern, pin.mode, use_titles);
        if matches.is_empty() {
            _ = ui.indent((pin.pattern.as_str(), 0usize), |ui| {
                _ = ui.label(RichText::new("—").weak());
            });
        }
        for (i, row) in matches.iter().enumerate() {
            _ = ui.indent((pin.pattern.as_str(), i), |ui| {
                _ = ui.horizontal_top(|ui| {
                    if ui
                        .add(Button::new("📋").small().frame(false))
                        .on_hover_text("Copy full value")
                        .clicked()
                    {
                        actions.copy = Some((pin_label(pin), row.value.clone()));
                    }
                    let color = ordinal_color(ui, i);
                    let response = ui
                        .add(
                            Label::new(
                                RichText::new(excerpt(&row.value, MAX_VALUE_CHARS))
                                    .monospace()
                                    .color(color),
                            )
                            .wrap()
                            .sense(Sense::click()),
                        )
                        .on_hover_text("Click to copy full value");
                    if response.clicked() {
                        actions.copy = Some((pin_label(pin), row.value.clone()));
                    }
                });
                _ = ui.label(RichText::new(&row.path).small().weak());
            });
        }
        ui.add_space(8.0);
    }
}

/// Read-only inputs threaded through the metadata tree renderer.
#[derive(Clone, Copy)]
struct TreeCtx<'a> {
    entry: &'a ImageEntry,
    pins: &'a [Pin],
    /// One-frame override forcing every section open or closed.
    force_open: Option<bool>,
    /// Lowercased search query. Empty renders the full tree; non-empty prunes
    /// branches with no match and defaults the surviving branches open.
    query: &'a str,
    /// ComfyUI-titles setting: gates title-pin creation and pin indicators.
    use_titles: bool,
    /// One-frame find target (a row or node path): every header on the path
    /// to it or under it is forced open, and the target scrolls into view.
    jump: Option<&'a str>,
}

/// True when `header_path` is the jump target itself, an ancestor of it (the
/// header must open to reveal the target), or a descendant (the target is a
/// node whose whole subtree should open).
fn jump_opens(header_path: &str, target: &str) -> bool {
    if header_path == target {
        return true;
    }
    let is_ancestor = target.len() > header_path.len()
        && target.starts_with(header_path)
        && target.as_bytes()[header_path.len()] == b'.';
    let is_descendant = header_path.len() > target.len()
        && header_path.starts_with(target)
        && header_path.as_bytes()[target.len()] == b'.';
    is_ancestor || is_descendant
}

/// The `.open(..)` override for a header at `path`: a jump hit forces open,
/// otherwise the expand/collapse-all override applies.
fn header_open_override(ctx: &TreeCtx<'_>, path: &str) -> Option<bool> {
    match ctx.jump {
        Some(target) if jump_opens(path, target) => Some(true),
        _ => ctx.force_open,
    }
}

/// True when any flattened row belonging to `chunk` matches the query.
fn chunk_matches(entry: &ImageEntry, chunk: &ParsedChunk, query: &str) -> bool {
    match &chunk.payload {
        ChunkPayload::Json(value) => {
            index::subtree_matches(&chunk.keyword, &chunk.keyword, value, query)
        }
        ChunkPayload::Plain(_) => {
            let keyword_lc = chunk.keyword.to_lowercase();
            let prefix = format!("{keyword_lc}.");
            entry
                .rows
                .iter()
                .filter(|row| row.path_lc == keyword_lc || row.path_lc.starts_with(&prefix))
                .any(|row| index::row_matches(row, query))
        }
    }
}

fn chunk_tree_ui(ui: &mut Ui, ctx: &TreeCtx<'_>, actions: &mut UiActions) {
    let searching = !ctx.query.is_empty();
    let mut shown_any = false;

    let file_rows: Vec<_> = ctx
        .entry
        .file_rows()
        .filter(|row| !searching || index::row_matches(row, ctx.query))
        .collect();
    if !file_rows.is_empty() {
        shown_any = true;
        _ = egui::CollapsingHeader::new(RichText::new("file").strong())
            .id_salt(("file-metadata", ctx.query))
            .default_open(true)
            .open(header_open_override(ctx, "file"))
            .show(ui, |ui| {
                for row in file_rows {
                    leaf_row_ui(ui, ctx, &row.path, &row.key, &row.value, true, actions);
                }
            });
    }

    if ctx.entry.chunks.is_empty() && !searching {
        _ = ui.label(RichText::new("No text chunks in this image").weak());
        return;
    }

    for (chunk_index, chunk) in ctx.entry.chunks.iter().enumerate() {
        // A query hit on the chunk keyword shows the whole chunk unfiltered;
        // otherwise prune to matching subtrees and skip chunks with no match.
        let mut chunk_query = ctx.query;
        if searching {
            if chunk.keyword.to_lowercase().contains(ctx.query) {
                chunk_query = "";
            } else if !chunk_matches(ctx.entry, chunk, ctx.query) {
                continue;
            }
        }
        shown_any = true;
        let chunk_ctx = TreeCtx {
            query: chunk_query,
            ..*ctx
        };
        let title = format!("{}  ({})", chunk.keyword, human_size(chunk.byte_len as u64));
        let header = egui::CollapsingHeader::new(RichText::new(title).strong())
            .id_salt(("chunk", chunk_index, &chunk.keyword, chunk_ctx.query))
            .default_open(true)
            .open(header_open_override(ctx, &chunk.keyword))
            .show(ui, |ui| match &chunk.payload {
                ChunkPayload::Json(value) => {
                    json_tree_ui(ui, &chunk_ctx, &chunk.keyword, &chunk.keyword, value, 1, actions);
                }
                ChunkPayload::Plain(text) => {
                    value_label_ui(ui, &chunk.keyword, text, actions);
                }
            });
        // A plain chunk renders one text blob, so any jump inside it lands on
        // the chunk header; a JSON chunk scrolls at the inner row instead.
        let scroll_here = match (&chunk.payload, ctx.jump) {
            (_, Some(target)) if target == chunk.keyword => true,
            (ChunkPayload::Plain(_), Some(target)) => {
                jump_opens(&chunk.keyword, target)
            }
            _ => false,
        };
        if scroll_here {
            header.header_response.scroll_to_me(Some(Align::Center));
        }
    }

    if searching && !shown_any {
        _ = ui.label(RichText::new("No matches in this image").weak());
    }
}

#[allow(clippy::too_many_arguments)]
fn json_tree_ui(
    ui: &mut Ui,
    ctx: &TreeCtx<'_>,
    path: &str,
    key: &str,
    value: &Value,
    depth: usize,
    actions: &mut UiActions,
) {
    let searching = !ctx.query.is_empty();
    match value {
        Value::Object(map) => {
            let children: Vec<(&String, &Value)> = map
                .iter()
                .filter(|(child_key, child)| {
                    !searching
                        || index::subtree_matches(
                            &format!("{path}.{child_key}"),
                            child_key,
                            child,
                            ctx.query,
                        )
                })
                .collect();
            // The chunk's own collapsing header already wraps the root object,
            // so only nested objects get their own header.
            if depth == 1 {
                for (child_key, child) in children {
                    let child_path = format!("{path}.{child_key}");
                    json_tree_ui(ui, ctx, &child_path, child_key, child, depth + 1, actions);
                }
            } else {
                let header = format!("{key}  {{{}}}", children.len());
                let response = egui::CollapsingHeader::new(header)
                    .id_salt((path, ctx.query))
                    .default_open(searching)
                    .open(header_open_override(ctx, path))
                    .show(ui, |ui| {
                        for (child_key, child) in children {
                            let child_path = format!("{path}.{child_key}");
                            json_tree_ui(ui, ctx, &child_path, child_key, child, depth + 1, actions);
                        }
                    });
                if ctx.jump == Some(path) {
                    response.header_response.scroll_to_me(Some(Align::Center));
                }
            }
        }
        Value::Array(items) => {
            let children: Vec<(usize, &Value)> = items
                .iter()
                .enumerate()
                .filter(|(i, child)| {
                    !searching
                        || index::subtree_matches(&format!("{path}.{i}"), key, child, ctx.query)
                })
                .collect();
            let header = format!("{key}  [{}]", children.len());
            let response = egui::CollapsingHeader::new(header)
                .id_salt((path, ctx.query))
                .default_open(searching)
                .open(header_open_override(ctx, path))
                .show(ui, |ui| {
                    for (i, child) in children {
                        let child_path = format!("{path}.{i}");
                        let child_key = format!("{key}[{i}]");
                        // Array elements are non-direct: their display key is
                        // synthetic, so key pins must not be offered on them.
                        json_tree_ui(ui, ctx, &child_path, &child_key, child, depth + 1, actions);
                    }
                });
            if ctx.jump == Some(path) {
                response.header_response.scroll_to_me(Some(Align::Center));
            }
        }
        leaf => {
            let text = match leaf {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // A synthetic `key[i]` display key marks an array element; those
            // rows are non-direct and never satisfy bare key pins.
            let direct = !key.ends_with(']');
            leaf_row_ui(ui, ctx, path, key, &text, direct, actions);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn leaf_row_ui(
    ui: &mut Ui,
    ctx: &TreeCtx<'_>,
    path: &str,
    key: &str,
    value: &str,
    direct: bool,
    actions: &mut UiActions,
) {
    let row = ui.horizontal(|ui| {
        // Map-backed lowercase title: cheap enough for the per-frame pinned
        // indicator. The original-case title (a row walk) is fetched only on
        // click or when the context menu is open.
        let title_lc = if ctx.use_titles && direct {
            ctx.entry.node_title_lc(path)
        } else {
            None
        };
        let key_pinned = direct
            && ctx.pins.iter().any(|p| {
                matches!(p.mode, PinMode::Key | PinMode::Auto)
                    && p.pattern.eq_ignore_ascii_case(key)
            });
        let path_pinned = ctx.pins.iter().any(|p| p.pattern.eq_ignore_ascii_case(path));
        let title_pinned = title_lc.is_some_and(|title| {
            ctx.pins.iter().any(|p| {
                let title_matchable = match p.mode {
                    PinMode::Title => true,
                    PinMode::Auto => !p.pattern.contains('.'),
                    PinMode::Key => false,
                };
                title_matchable
                    && !p.pattern.is_empty()
                    && title.contains(&p.pattern.to_lowercase())
            })
        });
        let pinned = key_pinned || path_pinned || title_pinned;
        let pin_text = if pinned {
            RichText::new("📌")
        } else {
            RichText::new("📌").weak()
        };
        let hover = if title_lc.is_some() {
            "Left-click: pin/unpin by node title (survives workflow changes)\nRight-click: pin options"
        } else if direct {
            "Left-click: pin/unpin key (all locations)\nRight-click: pin options"
        } else {
            "Left-click: pin/unpin this exact location\nRight-click: pin options"
        };
        let pin_button = ui
            .add(Button::new(pin_text).small().frame(false))
            .on_hover_text(hover);
        if pin_button.clicked() {
            let node_title = if ctx.use_titles {
                ctx.entry.node_title_for(path)
            } else {
                None
            };
            let target = match (path_pinned, direct, node_title) {
                // Toggling removes the existing exact-path pin.
                (true, _, _) => Pin {
                    pattern: path.to_string(),
                    label: None,
                    mode: PinMode::Key,
                    copy: None,
                },
                // A title pin only ever resolves `inputs.*` rows, so create one
                // only where the indicator and hover text say so (`title_lc`);
                // any other direct leaf gets the key pin its hover advertises.
                (false, true, Some(title)) if title_lc.is_some() => Pin::title(title),
                (false, true, _) => Pin::key(key),
                (false, false, maybe_title) => Pin {
                    pattern: path.to_string(),
                    label: maybe_title,
                    mode: PinMode::Key,
                    copy: None,
                },
            };
            actions.toggle_pin = Some(target);
        }
        _ = pin_button.context_menu(|ui| {
            if direct {
                let key_text = if key_pinned {
                    format!("Unpin key '{key}'")
                } else {
                    format!("Pin key '{key}'  (all locations)")
                };
                if ui.button(key_text).clicked() {
                    actions.toggle_pin = Some(Pin::key(key));
                    ui.close();
                }
            }
            let node_title = ctx.entry.node_title_for(path);
            // Same `title_lc` gate as the click: offer a title pin only where a
            // title pin can actually match. The exact-path label below keeps
            // using the unrestricted title.
            if title_lc.is_some()
                && let Some(title) = &node_title
            {
                let title_text = if title_pinned {
                    format!("Unpin node title '{title}'")
                } else {
                    format!("Pin node title '{title}'  (all workflows)")
                };
                if ui.button(title_text).clicked() {
                    actions.toggle_pin = Some(Pin::title(title.clone()));
                    ui.close();
                }
            }
            let path_text = if path_pinned {
                "Unpin this exact path"
            } else {
                "Pin exact path  (this location only)"
            };
            if ui.button(path_text).clicked() {
                actions.toggle_pin = Some(Pin {
                    pattern: path.to_string(),
                    label: if path_pinned { None } else { node_title },
                    mode: PinMode::Key,
                    copy: None,
                });
                ui.close();
            }
        });
        _ = ui.label(RichText::new(key).strong());
        if ui
            .add(Button::new("📋").small().frame(false))
            .on_hover_text("Copy full value")
            .clicked()
        {
            actions.copy = Some((key.to_string(), value.to_string()));
        }
        let text = single_line(value, fitting_chars(ui, ui.available_width()));
        let response = ui
            .add(
                Label::new(RichText::new(text).monospace())
                    .truncate()
                    .sense(Sense::click()),
            )
            .on_hover_text(format!(
                "{path}\n\n{}\n\n(click to copy)",
                tooltip_excerpt(value)
            ));
        if response.clicked() {
            actions.copy = Some((key.to_string(), value.to_string()));
        }
    });
    if ctx.jump == Some(path) {
        row.response.scroll_to_me(Some(Align::Center));
    }
}

/// Plain-text chunk body: an explicit copy button above the (clickable) text.
fn value_label_ui(ui: &mut Ui, key: &str, value: &str, actions: &mut UiActions) {
    if ui
        .add(Button::new("📋 Copy").small())
        .on_hover_text("Copy full text")
        .clicked()
    {
        actions.copy = Some((key.to_string(), value.to_string()));
    }
    let response = ui
        .add(
            Label::new(RichText::new(excerpt(value, MAX_VALUE_CHARS)).monospace())
                .wrap()
                .sense(Sense::click()),
        )
        .on_hover_text("Click to copy full value");
    if response.clicked() {
        actions.copy = Some((key.to_string(), value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count` cached thumbnails, inserted oldest (`img0`) to newest.
    fn cache(count: usize) -> (VecDeque<PathBuf>, HashMap<PathBuf, ()>) {
        let mut order = VecDeque::new();
        let mut live = HashMap::new();
        for i in 0..count {
            let path = PathBuf::from(format!("img{i}.png"));
            _ = live.insert(path.clone(), ());
            order.push_back(path);
        }
        (order, live)
    }

    #[test]
    fn a_pin_named_name_cannot_claim_the_name_column_width() {
        // Both keys share one map, so the namespaces are what keep a pin
        // pattern from overwriting a fixed column's width.
        assert_ne!(pin_column_key(&Pin::key("name")), NAME_COLUMN_KEY);
    }

    #[test]
    fn a_pin_column_width_key_ignores_pattern_case() {
        // Pin matching is case-insensitive, so two spellings of one pin must
        // not end up with two different saved widths.
        assert_eq!(pin_column_key(&Pin::key("CFG")), pin_column_key(&Pin::key("cfg")));
    }

    #[test]
    fn excerpt_marks_only_a_real_truncation() {
        // The ellipsis must track whether anything was actually cut, not
        // whether the text reached the cap.
        assert_eq!(excerpt("abcd", 4), "abcd");
        assert_eq!(excerpt("abcde", 4), "abcd…");
        assert_eq!(excerpt("", 0), "");
        assert_eq!(excerpt("a", 0), "…");
    }

    #[test]
    fn single_line_flattens_line_breaks() {
        assert_eq!(single_line("a\r\nb", 10), "a  b");
    }

    #[test]
    fn eviction_drops_the_least_recently_drawn_entry() {
        let (mut order, mut live) = cache(4);
        evict_stale_textures(&mut order, &mut live, 3);
        assert_eq!(order.len(), 3);
        assert_eq!(live.len(), 3);
        assert!(!live.contains_key(Path::new("img0.png")));
    }

    #[test]
    fn drawing_an_old_thumbnail_protects_it_from_eviction() {
        // `img0` is the oldest insertion, so age-ordered eviction would drop
        // exactly the thumbnail that is on screen. Drawing it must not.
        let (mut order, mut live) = cache(4);
        let drawn = [PathBuf::from("img0.png"), PathBuf::from("img0.png")];
        touch_texture_order(&mut order, &live, &drawn);
        // Repeats collapse: one entry per path, so the cap still counts textures.
        assert_eq!(order.len(), 4);
        assert_eq!(order.back().map(PathBuf::as_path), Some(Path::new("img0.png")));
        evict_stale_textures(&mut order, &mut live, 3);
        assert!(live.contains_key(Path::new("img0.png")));
        assert!(!live.contains_key(Path::new("img1.png")));
    }

    #[test]
    fn drawn_order_becomes_the_new_tail_order() {
        let (mut order, live) = cache(3);
        let drawn = [PathBuf::from("img2.png"), PathBuf::from("img0.png")];
        touch_texture_order(&mut order, &live, &drawn);
        let tail: Vec<&Path> = order.iter().map(PathBuf::as_path).collect();
        assert_eq!(
            tail,
            vec![
                Path::new("img1.png"),
                Path::new("img2.png"),
                Path::new("img0.png"),
            ]
        );
    }

    #[test]
    fn touching_a_path_with_no_texture_does_not_grow_the_order() {
        let (mut order, live) = cache(2);
        touch_texture_order(&mut order, &live, &[PathBuf::from("gone.png")]);
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn copy_defaults_on_for_prompt_and_seed_labels() {
        assert!(default_copy_enabled("seed"));
        assert!(default_copy_enabled("noise_seed"));
        assert!(default_copy_enabled("Positive Prompt"));
        assert!(default_copy_enabled("PROMPT"));
    }

    #[test]
    fn copy_defaults_off_for_every_other_label() {
        assert!(!default_copy_enabled("steps"));
        assert!(!default_copy_enabled("cfg"));
        assert!(!default_copy_enabled("sampler_name"));
        // The rule reads the displayed label, so an exact path inside the
        // `prompt` chunk shows only its final segment and stays off.
        assert!(!default_copy_enabled("⌖ text"));
    }

    #[test]
    fn an_explicit_copy_choice_beats_the_default() {
        let mut prompt_pin = Pin::key("prompt");
        let mut cfg_pin = Pin::key("cfg");
        assert!(pin_copies(&prompt_pin));
        assert!(!pin_copies(&cfg_pin));
        prompt_pin.copy = Some(false);
        cfg_pin.copy = Some(true);
        assert!(!pin_copies(&prompt_pin));
        assert!(pin_copies(&cfg_pin));
    }

    #[test]
    fn only_the_seed_default_pins_copy() {
        let pins = default_pins();
        let copying: Vec<&str> = pins
            .iter()
            .filter(|pin| pin_copies(pin))
            .map(|pin| pin.pattern.as_str())
            .collect();
        assert_eq!(copying, vec!["seed", "noise_seed"]);
    }

    /// One ComfyUI-shaped image: a titled prompt node plus two `cfg` values.
    /// Built through the public construction path, so `file.*` rows are
    /// present here exactly as they are at runtime.
    fn comfy_entry() -> ImageEntry {
        let text = r#"{"6": {"inputs": {"value": "a cat"}, "_meta": {"title": "Positive Prompt"}},
             "9": {"inputs": {"cfg": 4.5}}, "12": {"inputs": {"cfg": 1.0}}}"#;
        ImageEntry::from_parts(
            PathBuf::from("test.png"),
            crate::model::FileMeta {
                size: 0,
                created: None,
                modified: None,
                width: 8,
                height: 8,
            },
            vec![crate::chunks::TextChunk {
                keyword: "prompt".to_string(),
                text: text.to_string(),
            }],
        )
    }

    /// The three pins the strip has to tell apart: copy-enabled with one
    /// match, copy-disabled with two, and copy-enabled with none.
    fn strip_pins() -> Vec<Pin> {
        vec![
            Pin::title("Positive Prompt".to_string()),
            Pin::key("cfg"),
            Pin::key("seed"),
        ]
    }

    #[test]
    fn a_candidate_is_one_pin_match_pair_in_pin_then_document_order() {
        let entry = comfy_entry();
        let candidates = strip_candidates(&entry, &strip_pins(), true);
        let listed: Vec<(&str, bool, &str)> = candidates
            .iter()
            .map(|(label, copies, value)| (label.as_str(), *copies, value.as_str()))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("Positive Prompt", true, "a cat"),
                ("cfg", false, "4.5"),
                ("cfg", false, "1.0"),
            ]
        );
    }

    #[test]
    fn only_the_copy_enabled_pins_reach_the_strip() {
        // The bar never shows a bare label with nothing after it, so a pin
        // with no match contributes nothing either.
        let entry = comfy_entry();
        let candidates = strip_candidates(&entry, &strip_pins(), true);
        let strip = lightbox::build_strip(
            &candidates,
            lightbox::MAX_STRIP_ITEMS,
            lightbox::MAX_STRIP_CHARS,
        );
        let listed: Vec<(&str, &str)> = strip
            .iter()
            .map(|item| (item.label.as_str(), item.value.as_str()))
            .collect();
        assert_eq!(listed, vec![("Positive Prompt", "a cat")]);
    }

    #[test]
    fn turning_a_pins_copy_button_off_takes_it_off_the_strip() {
        let entry = comfy_entry();
        let mut pins = vec![Pin::title("Positive Prompt".to_string())];
        assert_eq!(strip_candidates(&entry, &pins, true).len(), 1);
        let strip_on = lightbox::build_strip(
            &strip_candidates(&entry, &pins, true),
            lightbox::MAX_STRIP_ITEMS,
            lightbox::MAX_STRIP_CHARS,
        );
        assert_eq!(strip_on.len(), 1);

        pins[0].copy = Some(false);
        let strip_off = lightbox::build_strip(
            &strip_candidates(&entry, &pins, true),
            lightbox::MAX_STRIP_ITEMS,
            lightbox::MAX_STRIP_CHARS,
        );
        assert!(strip_off.is_empty());
    }

    #[test]
    fn a_pin_saved_without_the_copy_field_loads_as_unset() {
        let stored = r#"{"pattern":"cfg","label":null,"mode":"Key"}"#;
        let pin: Pin = serde_json::from_str(stored).expect("pre-copy pin should still parse");
        assert_eq!(pin.copy, None);
        assert_eq!(pin, Pin::key("cfg"));
    }
}
