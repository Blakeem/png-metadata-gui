//! Main application state and UI.
//!
//! Layout:
//! - Top panel: open buttons, search box, pin chips.
//! - Left panel: preview + metadata for the selected image (full tree,
//!   search-match list, or pinned-only list).
//! - Center panel: image table; one sortable column per pin, plus a Match
//!   column while a search is active.
//! - Bottom panel: status line.
//!
//! Pins come in two forms: a bare key (`cfg`) matches every direct leaf with
//! that key name across all metadata styles; an exact path
//! (`prompt.183.inputs.cfg`) matches only that location. Multiple matches in
//! one image render as color-coded values in document order.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{
    self, Align, Button, Color32, Key, Label, RichText, ScrollArea, Sense, TextureHandle,
    TextureOptions, Ui,
};
use egui_extras::{Column, TableBuilder};
use serde_json::Value;

use crate::index::ChunkPayload;
use crate::model::{human_size, ImageEntry};
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
const PINS_STORAGE_KEY: &str = "pins_v3";
const ROW_HEIGHT: f32 = 52.0;
const THUMB_CELL: f32 = 48.0;
const FLASH_SECONDS: f32 = 2.5;
const MAX_CELL_MATCHES: usize = 3;

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
    toggle_pin: Option<String>,
    move_pin: Option<(usize, isize)>,
    unpin: Option<usize>,
    copy: Option<(String, String)>, // (label, clipboard content)
    set_tree_expanded: Option<bool>,
    open_folder_dialog: bool,
    open_file_dialog: bool,
}

pub struct ViewerApp {
    images: Vec<ImageEntry>,
    load_errors: Vec<String>,
    folder: Option<PathBuf>,
    selected: Option<usize>,
    search: String,
    pins: Vec<String>,
    new_pin: String,
    pinned_only: bool,
    sort: Option<(SortColumn, bool)>,
    visible: Vec<usize>,
    matches: HashMap<usize, MatchInfo>,
    needs_refresh: bool,
    /// One-frame override forcing every tree section open or closed.
    force_tree_expanded: Option<bool>,
    thumbs: ThumbPool,
    textures: HashMap<PathBuf, TextureHandle>,
    flash: Option<(String, Instant)>,
    pending_copy: Option<String>,
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_target: Option<PathBuf>) -> Self {
        let pins = cc
            .storage
            .and_then(|storage| eframe::get_value::<Vec<String>>(storage, PINS_STORAGE_KEY))
            .unwrap_or_else(|| DEFAULT_PINS.iter().map(|p| p.to_string()).collect());

        let mut app = Self {
            images: Vec::new(),
            load_errors: Vec::new(),
            folder: None,
            selected: None,
            search: String::new(),
            pins,
            new_pin: String::new(),
            pinned_only: false,
            sort: None,
            visible: Vec::new(),
            matches: HashMap::new(),
            needs_refresh: false,
            force_tree_expanded: None,
            thumbs: ThumbPool::new(cc.egui_ctx.clone()),
            textures: HashMap::new(),
            flash: None,
            pending_copy: None,
        };
        if let Some(target) = initial_target {
            app.open_target(target);
        }
        app
    }

    // ---- data loading ----

    fn open_target(&mut self, target: PathBuf) {
        if target.is_dir() {
            self.open_folder(target);
        } else {
            self.load_paths(vec![target]);
        }
    }

    fn open_folder(&mut self, dir: PathBuf) {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        self.folder = Some(dir);
        self.load_paths(paths);
    }

    fn load_paths(&mut self, paths: Vec<PathBuf>) {
        self.images.clear();
        self.load_errors.clear();
        self.textures.clear();
        self.selected = None;
        for path in paths {
            match ImageEntry::load(path.clone()) {
                Ok(entry) => {
                    self.thumbs.request(path);
                    self.images.push(entry);
                }
                Err(err) => self.load_errors.push(format!("{}: {err}", path.display())),
            }
        }
        if !self.images.is_empty() {
            self.selected = Some(0);
        }
        self.needs_refresh = true;
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
        match pngs.iter().find(|p| p.is_dir()) {
            Some(dir) => self.open_folder(dir.clone()),
            None => {
                self.folder = None;
                self.load_paths(pngs);
            }
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
            let images = &self.images;
            let pins = &self.pins;
            self.visible.sort_by(|&a, &b| {
                let ord = compare_entries(&images[a], &images[b], column, pins);
                if ascending { ord } else { ord.reverse() }
            });
        }

        let selection_visible = self
            .selected
            .is_some_and(|sel| self.visible.contains(&sel));
        if !selection_visible {
            self.selected = self.visible.first().copied();
        }
    }

    fn apply_actions(&mut self, actions: UiActions) {
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
        if let Some(key) = actions.toggle_pin {
            let existing = self
                .pins
                .iter()
                .position(|p| p.eq_ignore_ascii_case(&key));
            match existing {
                Some(idx) => {
                    _ = self.pins.remove(idx);
                }
                None => self.pins.push(key),
            }
            self.sort = None;
            self.needs_refresh = true;
        }
        if let Some((idx, delta)) = actions.move_pin {
            let target = idx as isize + delta;
            if target >= 0 && (target as usize) < self.pins.len() {
                self.pins.swap(idx, target as usize);
                self.sort = None;
                self.needs_refresh = true;
            }
        }
        if let Some(idx) = actions.unpin
            && idx < self.pins.len()
        {
            _ = self.pins.remove(idx);
            self.sort = None;
            self.needs_refresh = true;
        }
        if let Some((label, content)) = actions.copy {
            self.flash = Some((format!("Copied {label}"), Instant::now()));
            self.pending_copy = Some(content);
        }
        if let Some(expanded) = actions.set_tree_expanded {
            self.force_tree_expanded = Some(expanded);
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
            self.folder = None;
            self.load_paths(files);
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
        if row.path_lc.contains(query) || row.value_lc.contains(query) {
            if first.is_none() {
                first = Some(format!("{} = {}", row.key, single_line(&row.value, 200)));
            }
            count += 1;
        }
    }
    first.map(|first| MatchInfo { first, count })
}

fn compare_entries(a: &ImageEntry, b: &ImageEntry, column: SortColumn, pins: &[String]) -> Ordering {
    match column {
        SortColumn::Name => a.file_name_lc.cmp(&b.file_name_lc),
        SortColumn::Pin(pin_idx) => {
            let Some(pin) = pins.get(pin_idx) else {
                return Ordering::Equal;
            };
            let rows_a = a.rows_for_pin(pin);
            let rows_b = b.rows_for_pin(pin);
            match (rows_a.first(), rows_b.first()) {
                (Some(x), Some(y)) => compare_rows(x, y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
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

fn single_line(text: &str, max_chars: usize) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(max_chars)
        .collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Display form of a pin: bare keys show as-is; exact-path pins show a
/// location marker plus the final segment (full path in tooltips).
fn pin_label(pin: &str) -> String {
    match pin.rsplit_once('.') {
        Some((_, last)) => format!("⌖ {last}"),
        None => pin.to_string(),
    }
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let force_tree = self.force_tree_expanded.take();

        // Upload finished thumbnails.
        for (path, image) in self.thumbs.poll() {
            if let Some(image) = image {
                let name = path.to_string_lossy().into_owned();
                _ = self
                    .textures
                    .insert(path, ctx.load_texture(name, image, TextureOptions::LINEAR));
            }
        }

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
            if self.images.is_empty() {
                empty_state_ui(ui, &mut actions);
            } else {
                self.table_ui(ui, &mut actions);
            }
        });

        self.apply_actions(actions);
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
                    .hint_text("key, path, or value…")
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
                let chip = ui.small_button(pin_label(pin)).on_hover_text(format!(
                    "{pin}\n\nColumn in the table, in this order.\nRight-click to reorder or unpin.",
                ));
                _ = chip.context_menu(|ui| {
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
            let pin_edit = ui.add(
                egui::TextEdit::singleline(&mut self.new_pin)
                    .hint_text("add pin…")
                    .desired_width(80.0),
            );
            let submitted =
                pin_edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if submitted && !self.new_pin.trim().is_empty() {
                actions.toggle_pin = Some(self.new_pin.trim().to_string());
                self.new_pin.clear();
                pin_edit.request_focus();
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
            let available = ui.available_width();
            _ = ui.vertical_centered(|ui| {
                _ = ui.add(
                    egui::Image::new(texture)
                        .max_size(egui::vec2(available, 240.0)),
                );
            });
        } else {
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

        // View mode controls. Search overrides the checkbox; make that visible.
        let query = self.search.trim().to_lowercase();
        let tree_visible = query.is_empty() && !self.pinned_only;
        _ = ui.horizontal(|ui| {
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
            if !query.is_empty() {
                _ = ui.label(RichText::new("showing search matches").weak());
            }
        });
        ui.add_space(2.0);

        let entry = &self.images[selected];
        _ = ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if !query.is_empty() {
                    match_list_ui(ui, entry, &query, actions);
                } else if self.pinned_only {
                    pinned_list_ui(ui, entry, &self.pins, actions);
                } else {
                    chunk_tree_ui(ui, entry, &self.pins, force_tree, actions);
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
        let mut builder = TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(Align::Center))
            .column(Column::exact(THUMB_CELL + 8.0))
            .column(Column::initial(230.0).at_least(120.0).clip(true).resizable(true));
        for _ in &self.pins {
            builder = builder
                .column(Column::initial(110.0).at_least(60.0).clip(true).resizable(true));
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
                body.rows(ROW_HEIGHT, self.visible.len(), |mut row| {
                    let image_idx = self.visible[row.index()];
                    let entry = &self.images[image_idx];
                    row.set_selected(self.selected == Some(image_idx));

                    _ = row.col(|ui| {
                        match self.textures.get(&entry.path) {
                            Some(texture) => {
                                _ = ui.add(
                                    egui::Image::new(texture)
                                        .max_size(egui::vec2(THUMB_CELL, THUMB_CELL)),
                                );
                            }
                            None => {
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
                            pin_cell_ui(ui, entry, pin, actions);
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

// ---- widget helpers (free functions: no app borrow) ----

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
fn pin_cell_ui(ui: &mut Ui, entry: &ImageEntry, pin: &str, actions: &mut UiActions) {
    let matches = entry.rows_for_pin(pin);
    if matches.is_empty() {
        _ = ui.label(RichText::new("—").weak());
        return;
    }
    let tooltip: String = matches
        .iter()
        .enumerate()
        .map(|(i, row)| format!("{}. {} = {}", i + 1, row.path, single_line(&row.value, 200)))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n\n(click a value to copy it)";
    for (i, row) in matches.iter().take(MAX_CELL_MATCHES).enumerate() {
        if i > 0 {
            _ = ui.label(RichText::new("·").weak());
        }
        let color = ordinal_color(ui, i);
        let response = ui
            .add(
                Label::new(RichText::new(single_line(&row.value, 28)).monospace().color(color))
                    .sense(Sense::click()),
            )
            .on_hover_text(tooltip.clone());
        if response.clicked() {
            actions.copy = Some((pin.to_string(), row.value.clone()));
        }
    }
    if matches.len() > MAX_CELL_MATCHES {
        _ = ui.label(RichText::new(format!("+{}", matches.len() - MAX_CELL_MATCHES)).weak());
    }
}

fn match_list_ui(ui: &mut Ui, entry: &ImageEntry, query: &str, actions: &mut UiActions) {
    let mut shown = 0usize;
    if entry.file_name_lc.contains(query) {
        _ = ui.label(RichText::new("file name matches").weak().small());
        shown += 1;
    }
    for row in &entry.rows {
        if row.path_lc.contains(query) || row.value_lc.contains(query) {
            _ = ui.label(RichText::new(&row.path).weak().small());
            value_label_ui(ui, &row.key, &row.value, actions);
            ui.add_space(4.0);
            shown += 1;
        }
    }
    if shown == 0 {
        _ = ui.label(RichText::new("No matches in this image").weak());
    }
}

/// Pinned-only view: one card per pin — pin name, then each matching value
/// (ordinal-colored, click to copy) with its location underneath.
fn pinned_list_ui(ui: &mut Ui, entry: &ImageEntry, pins: &[String], actions: &mut UiActions) {
    if pins.is_empty() {
        _ = ui.label(RichText::new("Nothing pinned. Pin keys from the tree view.").weak());
        return;
    }
    for pin in pins {
        _ = ui.horizontal(|ui| {
            _ = ui.label(RichText::new(pin_label(pin)).strong());
            if pin.contains('.') {
                _ = ui.label(RichText::new("exact path").small().weak())
                    .on_hover_text(pin.clone());
            }
        });
        let matches = entry.rows_for_pin(pin);
        if matches.is_empty() {
            _ = ui.indent((pin.as_str(), 0usize), |ui| {
                _ = ui.label(RichText::new("—").weak());
            });
        }
        for (i, row) in matches.iter().enumerate() {
            _ = ui.indent((pin.as_str(), i), |ui| {
                let color = ordinal_color(ui, i);
                let response = ui
                    .add(
                        Label::new(
                            RichText::new(single_line(&row.value, 300)).monospace().color(color),
                        )
                        .sense(Sense::click()),
                    )
                    .on_hover_text("Click to copy full value");
                if response.clicked() {
                    actions.copy = Some((row.key.clone(), row.value.clone()));
                }
                _ = ui.label(RichText::new(&row.path).small().weak());
            });
        }
        ui.add_space(8.0);
    }
}

fn chunk_tree_ui(
    ui: &mut Ui,
    entry: &ImageEntry,
    pins: &[String],
    force_open: Option<bool>,
    actions: &mut UiActions,
) {
    _ = egui::CollapsingHeader::new(RichText::new("file").strong())
        .id_salt("file-metadata")
        .default_open(true)
        .open(force_open)
        .show(ui, |ui| {
            for row in entry.file_rows() {
                leaf_row_ui(ui, &row.path, &row.key, &row.value, pins, actions);
            }
        });
    if entry.chunks.is_empty() {
        _ = ui.label(RichText::new("No text chunks in this image").weak());
        return;
    }
    for chunk in &entry.chunks {
        let title = format!("{}  ({})", chunk.keyword, human_size(chunk.byte_len as u64));
        _ = egui::CollapsingHeader::new(RichText::new(title).strong())
            .id_salt(("chunk", &chunk.keyword))
            .default_open(true)
            .open(force_open)
            .show(ui, |ui| match &chunk.payload {
                ChunkPayload::Json(value) => {
                    json_tree_ui(
                        ui,
                        &chunk.keyword,
                        &chunk.keyword,
                        value,
                        1,
                        pins,
                        force_open,
                        actions,
                    );
                }
                ChunkPayload::Plain(text) => {
                    value_label_ui(ui, &chunk.keyword, text, actions);
                }
            });
    }
}

#[allow(clippy::too_many_arguments)]
fn json_tree_ui(
    ui: &mut Ui,
    path: &str,
    key: &str,
    value: &Value,
    depth: usize,
    pins: &[String],
    force_open: Option<bool>,
    actions: &mut UiActions,
) {
    match value {
        Value::Object(map) => {
            // The chunk's own collapsing header already wraps the root object,
            // so only nested objects get their own header.
            if depth == 1 {
                for (child_key, child) in map {
                    let child_path = format!("{path}.{child_key}");
                    json_tree_ui(ui, &child_path, child_key, child, depth + 1, pins, force_open, actions);
                }
            } else {
                let header = format!("{key}  {{{}}}", map.len());
                _ = egui::CollapsingHeader::new(header)
                    .id_salt(path)
                    .default_open(false)
                    .open(force_open)
                    .show(ui, |ui| {
                        for (child_key, child) in map {
                            let child_path = format!("{path}.{child_key}");
                            json_tree_ui(ui, &child_path, child_key, child, depth + 1, pins, force_open, actions);
                        }
                    });
            }
        }
        Value::Array(items) => {
            let header = format!("{key}  [{}]", items.len());
            _ = egui::CollapsingHeader::new(header)
                .id_salt(path)
                .default_open(false)
                .open(force_open)
                .show(ui, |ui| {
                    for (i, child) in items.iter().enumerate() {
                        let child_path = format!("{path}.{i}");
                        let child_key = format!("{key}[{i}]");
                        json_tree_ui(ui, &child_path, &child_key, child, depth + 1, pins, force_open, actions);
                    }
                });
        }
        leaf => {
            let text = match leaf {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            leaf_row_ui(ui, path, key, &text, pins, actions);
        }
    }
}

fn leaf_row_ui(
    ui: &mut Ui,
    path: &str,
    key: &str,
    value: &str,
    pins: &[String],
    actions: &mut UiActions,
) {
    _ = ui.horizontal(|ui| {
        let key_pinned = pins.iter().any(|p| p.eq_ignore_ascii_case(key));
        let path_pinned = pins.iter().any(|p| p.eq_ignore_ascii_case(path));
        let pinned = key_pinned || path_pinned;
        let pin_text = if pinned {
            RichText::new("📌")
        } else {
            RichText::new("📌").weak()
        };
        let pin_button = ui
            .add(Button::new(pin_text).small().frame(false))
            .on_hover_text("Left-click: pin/unpin key (all locations)\nRight-click: pin options");
        if pin_button.clicked() {
            let target = if path_pinned { path } else { key };
            actions.toggle_pin = Some(target.to_string());
        }
        _ = pin_button.context_menu(|ui| {
            let key_text = if key_pinned {
                format!("Unpin key '{key}'")
            } else {
                format!("Pin key '{key}'  (all locations)")
            };
            if ui.button(key_text).clicked() {
                actions.toggle_pin = Some(key.to_string());
                ui.close();
            }
            let path_text = if path_pinned {
                "Unpin this exact path".to_string()
            } else {
                "Pin exact path  (this location only)".to_string()
            };
            if ui.button(path_text).clicked() {
                actions.toggle_pin = Some(path.to_string());
                ui.close();
            }
        });
        _ = ui.label(RichText::new(key).strong());
        let response = ui
            .add(
                Label::new(RichText::new(single_line(value, 120)).monospace())
                    .truncate()
                    .sense(Sense::click()),
            )
            .on_hover_text(format!("{path}\n\n{value}\n\n(click to copy)"));
        if response.clicked() {
            actions.copy = Some((key.to_string(), value.to_string()));
        }
    });
}

fn value_label_ui(ui: &mut Ui, key: &str, value: &str, actions: &mut UiActions) {
    let response = ui
        .add(
            Label::new(RichText::new(single_line(value, 300)).monospace())
                .sense(Sense::click()),
        )
        .on_hover_text("Click to copy full value");
    if response.clicked() {
        actions.copy = Some((key.to_string(), value.to_string()));
    }
}
