//! Lightbox windows: one image shown in its own OS window.
//!
//! Each open lightbox is a deferred egui viewport. The parent
//! ([`crate::app::ViewerApp`]) owns the list and re-registers every surviving
//! window each frame. A window it stops registering is destroyed at the end of
//! that pass. State crosses the boundary through [`LightboxShared`] behind a
//! mutex, because the viewport callback outlives the frame that built it.
//!
//! Each window also carries a frozen copy of the main window's filtered image
//! list, so arrow keys walk the list the window opened on. A later search or
//! re-sort in the main window builds a new list and leaves open windows alone.
//!
//! The view math is pure and lives in this module. It works in two units.
//! [`scene_scale`] and [`scene_rect_for_scale`] take the panel in whatever unit
//! the caller measures it in. Everything above them passes the panel in
//! physical pixels, which makes the scale a zoom: screen pixels per source
//! image pixel, one of each at native size.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, CursorIcon, Key, Pos2, Rangef, Rect, RichText, Sense, TextureHandle, Ui, Vec2,
    ViewportClass, ViewportCommand, ViewportId, vec2,
};

use crate::app::{single_line, FLASH_SECONDS};
use crate::model::human_size;

/// Upper bound on lightboxes open at once. These are OS windows rather than
/// textures, and each one holds a full-resolution decode, so the ceiling is
/// deliberately low.
pub const MAX_LIGHTBOXES: usize = 6;

/// Starting window size. Large enough to show a thumbnail-sized image without
/// a resize, small enough to leave the main window reachable.
pub const INITIAL_SIZE: [f32; 2] = [900.0, 700.0];

/// Smallest zoom the view allows, in screen pixels per source pixel. Low enough
/// that the largest file the decoder accepts still fits a small window.
pub const MIN_ZOOM: f32 = 0.02;

/// Largest zoom the view allows. Past this a source pixel is a block.
pub const MAX_ZOOM: f32 = 16.0;

/// Factor one zoom step multiplies the zoom by.
pub const ZOOM_STEP: f32 = 1.25;

/// Scroll points that make one zoom step. egui scales one wheel notch to 40
/// points on desktop, so a notch is a step and a trackpad glides between them.
const WHEEL_POINTS_PER_STEP: f32 = 40.0;

/// Width held for the zoom readout, so the buttons beside it do not shift as
/// the number changes width.
const ZOOM_READOUT_WIDTH: f32 = 64.0;

/// Width held for the position readout, so the buttons beside it do not shift
/// as the number changes width.
const POSITION_WIDTH: f32 = 72.0;

/// Upper bound on the strip's height. Past this the values scroll, so a
/// multi-kilobyte prompt cannot push the control row off screen.
const STRIP_HEIGHT: f32 = 108.0;

/// Values one strip lists. A workflow can match a pin many times, and the bar
/// is a summary rather than the metadata panel.
pub const MAX_STRIP_ITEMS: usize = 8;

/// Characters one strip value lays out. The copy button always carries the
/// complete value.
pub const MAX_STRIP_CHARS: usize = 400;

/// Serial number of one lightbox window, unique for the process lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LightboxId(pub u64);

impl LightboxId {
    /// The viewport this lightbox draws into. Hashing the serial rather than
    /// the file path keeps two windows opened on the same image distinct.
    pub fn viewport_id(self) -> ViewportId {
        ViewportId::from_hash_of(("lightbox", self.0))
    }
}

/// What one lightbox window shows about its file. Every field is owned: the
/// viewport callback outlives the frame, and `ImageEntry` is neither `Clone`
/// nor borrowable for that long.
#[derive(Clone, Debug)]
pub struct LightboxDetail {
    pub path: PathBuf,
    pub file_name: String,
    pub source_size: [u32; 2],
    pub file_size: u64,
    /// The copy-enabled pin values this file carries, in pin-then-document
    /// order.
    pub strip: Vec<StripItem>,
}

/// One value on a window's bottom strip. `shown` is the single-line truncated
/// form the bar lays out, and `value` is the whole text, because the clipboard
/// must get the complete prompt rather than the excerpt on screen.
#[derive(Clone, Debug)]
pub struct StripItem {
    pub label: String,
    pub shown: String,
    pub value: String,
}

/// The image list one window walks, and where it sits in it. The `Arc` is a
/// snapshot handed over at open time, so two windows can hold two different
/// filters.
#[derive(Clone, Debug)]
pub struct LightboxNav {
    pub paths: Arc<[PathBuf]>,
    pub cursor: usize,
}

/// How far this window's full-resolution decode got. The three terminal
/// states are what stop a window spinning forever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadStatus {
    Loading,
    Ready,
    Refused(String),
    Failed(String),
}

/// A view change the window has been asked for. `ZoomBy` counts steps, and
/// takes a fraction of one because a trackpad reports a fraction of a notch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViewCommand {
    Fit,
    Native,
    ZoomBy(f32),
}

/// State the parent app and one lightbox window both touch. No `Debug`:
/// `TextureHandle` has no `Debug` impl.
pub struct LightboxShared {
    pub texture: Option<TextureHandle>,
    pub detail: LightboxDetail,
    pub status: LoadStatus,
    /// Size the texture was decoded at, which the clamps in [`crate::full`]
    /// may put below `detail.source_size`.
    pub decoded_size: [u32; 2],
    /// The part of the image on screen, in source pixels. The bar and the image
    /// panel both run inside one child pass, so this field and the three below
    /// need none of the cross-viewport repaints `close_requested` does.
    pub scene_rect: Rect,
    /// A view change a bar button asked for. Only the image panel can resolve
    /// one, because the panel size is what turns a command into a rect.
    pub pending_view: Option<ViewCommand>,
    /// Whether the view follows the window size. A resize refits while this
    /// holds, and any pan, zoom, or native-size action drops it.
    pub fit_mode: bool,
    /// Zoom the last drawn frame resolved to. The bar is built before the image
    /// panel, so a stored number is all it can read.
    pub zoom: f32,
    pub nav: LightboxNav,
    /// A list position this window's keys or buttons asked for. Only the parent
    /// can resolve one, because reading the folder and starting a decode both
    /// live there.
    pub nav_request: Option<usize>,
    /// This window's own copy note and when it was set. The main window's
    /// status bar is not where the viewer is looking.
    pub copied: Option<(String, Instant)>,
    pub close_requested: bool,
}

/// One open lightbox window, as the parent app tracks it.
pub struct Lightbox {
    pub id: LightboxId,
    pub shared: Arc<Mutex<LightboxShared>>,
}

/// Whether another lightbox may open. Split out from the app so the cap
/// boundary is testable without a render context.
pub fn can_open(open: usize, max: usize) -> bool {
    open < max
}

/// The list position `delta` steps from `cursor`. The ends stop rather than
/// wrap, and `None` says there is nowhere to go, so the caller can skip a
/// decode it does not need. A cursor past the end clamps back into range
/// first, which makes a zero step the clamp on its own.
pub fn step_cursor(cursor: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = isize::try_from(cursor.min(len - 1)).ok()?;
    let target = usize::try_from(current.checked_add(delta)?).ok()?;
    (target < len).then_some(target)
}

/// The strip rows for one image. `candidates` holds one entry per (pin, match)
/// pair in pin-then-document order, each carrying its pin's copy flag. The cap
/// counts flattened items, because that count is what bounds the bar's height.
pub fn build_strip(
    candidates: &[(String, bool, String)],
    max_items: usize,
    max_chars: usize,
) -> Vec<StripItem> {
    candidates
        .iter()
        .filter(|(_, copies, _)| *copies)
        .take(max_items)
        .map(|(label, _, value)| StripItem {
            label: label.clone(),
            shown: single_line(value, max_chars),
            value: value.clone(),
        })
        .collect()
}

/// Scale of a view, in outer units per scene unit. `Scene` fits the whole rect
/// inside the panel, so the axis with less room decides. `None` marks a rect
/// `Scene` would reject, which is the signal to refit.
pub fn scene_scale(outer_size: Vec2, scene_rect: Rect) -> Option<f32> {
    if !outer_size.is_finite() || !scene_rect.is_finite() {
        return None;
    }
    let size = scene_rect.size();
    if outer_size.x <= 0.0 || outer_size.y <= 0.0 || size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    let scale = (outer_size.x / size.x).min(outer_size.y / size.y);
    scale.is_finite().then_some(scale)
}

/// Exact inverse of [`scene_scale`]: the rect centred on `center` that fills
/// the panel at `scale`. Its aspect matches the panel, so neither axis
/// letterboxes and the scale reads back unchanged.
pub fn scene_rect_for_scale(outer_size: Vec2, center: Pos2, scale: f32) -> Rect {
    Rect::from_center_size(center, outer_size / scale)
}

/// The stored view reshaped for the panel it is about to be drawn in, held at
/// the zoom the last frame measured. `Scene` re-derives its scale from the rect
/// against the panel, so a resized window would otherwise magnify a view nobody
/// touched. A view that recorded no usable zoom keeps the one it measures.
/// `None` marks a rect `Scene` would reject, which is the signal to refit.
pub fn scene_rect_for_panel(outer_size: Vec2, scene_rect: Rect, zoom: f32) -> Option<Rect> {
    let measured = scene_scale(outer_size, scene_rect)?;
    let held = if zoom.is_finite() && zoom > 0.0 {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        measured
    };
    Some(scene_rect_for_scale(outer_size, scene_rect.center(), held))
}

/// Zoom that shows the whole image with no crop. A panel with no area can fit
/// nothing, so that answers native size.
pub fn fit_zoom(outer_size: Vec2, image_size: Vec2, pixels_per_point: f32) -> f32 {
    let outer = outer_size * sane_pixels_per_point(pixels_per_point);
    if !outer.is_finite() || !image_size.is_finite() {
        return 1.0;
    }
    if outer.x <= 0.0 || outer.y <= 0.0 || image_size.x <= 0.0 || image_size.y <= 0.0 {
        return 1.0;
    }
    (outer.x / image_size.x)
        .min(outer.y / image_size.y)
        .clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Zoom moved `steps` notches, held inside the limits. A zoom that is not a
/// positive number cannot be stepped, so that answers native size.
pub fn zoom_step(zoom: f32, steps: f32) -> f32 {
    if !zoom.is_finite() || zoom <= 0.0 || !steps.is_finite() {
        return 1.0;
    }
    (zoom * ZOOM_STEP.powf(steps)).clamp(MIN_ZOOM, MAX_ZOOM)
}

/// The view a command resolves to. `outer_size` is the panel in points, which
/// only the image panel knows, so a bar button records the command and this
/// runs a moment later.
pub fn view_scene_rect(
    command: ViewCommand,
    current: Rect,
    outer_size: Vec2,
    image_size: Vec2,
    pixels_per_point: f32,
) -> Rect {
    let outer = outer_size * sane_pixels_per_point(pixels_per_point);
    let image_center = Rect::from_min_size(Pos2::ZERO, image_size).center();
    let current_zoom = scene_scale(outer, current);
    let held_center = match current_zoom {
        Some(_) => current.center(),
        None => image_center,
    };

    // A command with no readable view under it has nothing to zoom, so it
    // resolves to a fit the way the opening frame does.
    let (zoom, center) = match command {
        ViewCommand::Fit => (
            fit_zoom(outer_size, image_size, pixels_per_point),
            image_center,
        ),
        ViewCommand::Native => (1.0, held_center),
        ViewCommand::ZoomBy(steps) => match current_zoom {
            Some(zoom) => (zoom_step(zoom, steps), held_center),
            None => (
                fit_zoom(outer_size, image_size, pixels_per_point),
                image_center,
            ),
        },
    };

    scene_rect_for_scale(outer, center, zoom)
}

/// Keeps the view over the image. An axis the whole image fits on stays centred
/// on it, and a zoomed-in axis stops at the image edge. The size never changes,
/// so clamping cannot alter the zoom.
pub fn clamp_scene_rect(scene_rect: Rect, image_rect: Rect) -> Rect {
    if !scene_rect.is_finite() || !image_rect.is_finite() {
        return scene_rect;
    }
    let size = scene_rect.size();
    let min = Pos2::new(
        clamp_axis(scene_rect.min.x, size.x, image_rect.min.x, image_rect.max.x),
        clamp_axis(scene_rect.min.y, size.y, image_rect.min.y, image_rect.max.y),
    );
    Rect::from_min_size(min, size)
}

/// The zoom as a whole percentage. Anything but a positive zoom reads as zero,
/// and any positive zoom reads as at least one percent.
pub fn zoom_percent(zoom: f32) -> u32 {
    if !zoom.is_finite() || zoom <= 0.0 {
        return 0;
    }
    (zoom * 100.0).round().max(1.0) as u32
}

/// Whether the view spreads the decoded texture over more screen pixels than it
/// has, which is when the image on screen is softer than the file.
pub fn is_soft(zoom: f32, source_width: u32, decoded_width: u32) -> bool {
    if !zoom.is_finite() || source_width == 0 || decoded_width == 0 {
        return false;
    }
    zoom * source_width as f32 > decoded_width as f32
}

/// One axis of [`clamp_scene_rect`].
fn clamp_axis(min: f32, extent: f32, low: f32, high: f32) -> f32 {
    let span = high - low;
    if extent >= span {
        return low + (span - extent) / 2.0;
    }
    min.clamp(low, high - extent)
}

/// Guards the divisions the view math does by the screen scale.
fn sane_pixels_per_point(pixels_per_point: f32) -> f32 {
    if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    }
}

/// Draws one lightbox window.
///
/// **The wake rule, both directions.** A child pass never runs
/// `ViewerApp::ui`, so every write to a [`LightboxShared`] field the parent
/// reads must be followed by `request_repaint_of(ViewportId::ROOT)`, or the
/// flag sits unread and the widget that set it does nothing. eframe does
/// repaint the parent on the OS close path, but it queues both repaints at
/// event time with no ordering between them, so a root pass that ran first
/// would read `close_requested` as false and re-register the window. One
/// surplus repaint costs far less than a window that will not close. The
/// mirror holds the other way: anything the parent writes into
/// [`LightboxShared`] must be followed by
/// `ctx.request_repaint_of(id.viewport_id())`, because repaints are per
/// viewport and the child would otherwise paint stale state until some
/// unrelated event woke it.
pub fn lightbox_ui(ui: &mut Ui, class: ViewportClass, shared: &Mutex<LightboxShared>) {
    // One lock for the whole body is safe: a child pass and the parent's
    // `ui()` run on the same OS thread and never nest, and nothing below
    // re-enters the parent.
    let mut state = shared.lock().expect("lightbox state poisoned");
    // The embedded fallback draws inside the main window, so a viewport
    // command or a close event there would act on the main window itself.
    let native = class != ViewportClass::EmbeddedWindow;
    let fullscreen = native && ui.ctx().input(|i| i.viewport().fullscreen).unwrap_or(false);

    if native {
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            state.close_requested = true;
            wake_parent(ui.ctx());
        }
        if ui.ctx().input(|i| i.key_pressed(Key::Escape)) {
            if fullscreen {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::Fullscreen(false));
            } else {
                state.close_requested = true;
                wake_parent(ui.ctx());
            }
        }
    }

    // `ctx.input` inside a viewport callback reads this window's own input, so
    // the arrow keys need no routing from the main window.
    let step = ui.ctx().input(|input| {
        if input.key_pressed(Key::ArrowRight) || input.key_pressed(Key::ArrowDown) {
            1
        } else if input.key_pressed(Key::ArrowLeft) || input.key_pressed(Key::ArrowUp) {
            -1
        } else {
            0
        }
    });
    if step != 0 {
        let target = step_cursor(state.nav.cursor, state.nav.paths.len(), step);
        request_nav(ui.ctx(), &mut state, target);
    }

    // Read what the image panel needs out of the guard here: the bar is built
    // first and takes the guard itself, mutably.
    let texture = state.texture.clone();
    let message = match &state.status {
        LoadStatus::Refused(message) | LoadStatus::Failed(message) => Some(message.clone()),
        LoadStatus::Loading | LoadStatus::Ready => None,
    };
    let loading = state.status == LoadStatus::Loading;
    // A refusal covers the image even when a stand-in thumbnail is loaded, and
    // there is nothing to zoom under it.
    let has_scene = message.is_none() && texture.is_some();

    // The bar id is scoped to this window's root Ui: panel state lives in one
    // Context-wide map shared by every viewport, so a bare name would make two
    // open lightboxes fight over one entry.
    _ = egui::Panel::bottom(ui.id().with("lightbox_bar")).show(ui, |ui| {
        // The strip goes above the control row so it grows upward. Beneath the
        // row it would push every control up the window on any image carrying
        // more pinned values than the last one.
        show_strip(ui, &mut state);
        _ = ui.horizontal(|ui| {
            // Every control is anchored to the right edge and claims its width
            // before the file name gets any, so paging through a folder cannot
            // shift a button out from under the pointer. The name takes what is
            // left and truncates into it.
            _ = ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    state.close_requested = true;
                    wake_parent(ui.ctx());
                }
                if native {
                    // The label reads the state back from the viewport, so a
                    // fullscreen toggle from outside the app stays in step.
                    let label = if fullscreen {
                        "Exit fullscreen"
                    } else {
                        "Fullscreen"
                    };
                    if ui.button(label).clicked() {
                        ui.ctx()
                            .send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
                    }
                }
                if has_scene {
                    _ = ui.separator();
                    show_zoom_controls(ui, &mut state);
                }
                show_nav_controls(ui, &mut state);
                _ = ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                    _ = ui
                        .add(
                            egui::Label::new(RichText::new(&state.detail.file_name).strong())
                                .truncate(),
                        )
                        .on_hover_text(state.detail.path.display().to_string());
                    _ = ui.separator();
                    _ = ui.label(format!(
                        "{}×{}  ·  {}",
                        state.detail.source_size[0],
                        state.detail.source_size[1],
                        human_size(state.detail.file_size)
                    ));
                    show_copied_note(ui, &mut state);
                });
            });
        });
    });

    _ = egui::CentralPanel::default().show(ui, |ui| {
        if let Some(message) = message {
            _ = ui.centered_and_justified(|ui| ui.label(RichText::new(message).weak()));
            return;
        }
        // The table's thumbnail stands in until the full decode lands, so a
        // window opened on a decoded row is never blank.
        if let Some(texture) = texture {
            show_scene(ui, &mut state, &texture);
            return;
        }
        _ = ui.centered_and_justified(|ui| {
            if loading {
                // Deliberately not `ui.spinner()`. A spinner asks for a repaint
                // every pass, and a child viewport repainting at full rate
                // starves the root of redraws. The root is what drains this
                // window's close, its arrow keys, and its finished decodes, so
                // an animated spinner here strands the window on that spinner
                // and refuses to close it. A still label lets the child sleep
                // until `poll_full` wakes it with the image.
                _ = ui.label(RichText::new("Loading…").weak());
            } else {
                _ = ui.label(RichText::new("This image has no pixels to show.").weak());
            }
        });
    });
}

/// Previous and next, with the list position between them. The buttons
/// disable at the ends, which is the only thing telling the viewer the list
/// stops rather than wraps. Added right to left, like the zoom controls beside
/// them, so they read out as previous, position, next.
fn show_nav_controls(ui: &mut Ui, state: &mut LightboxShared) {
    let len = state.nav.paths.len();
    if len == 0 {
        return;
    }
    let cursor = state.nav.cursor.min(len - 1);
    let previous = step_cursor(cursor, len, -1);
    let next = step_cursor(cursor, len, 1);

    _ = ui.separator();
    if ui
        .add_enabled(next.is_some(), egui::Button::new("▶"))
        .on_hover_text("Next image")
        .clicked()
    {
        request_nav(ui.ctx(), state, next);
    }
    // A fixed width, so the count crossing a digit boundary moves nothing.
    _ = ui.add_sized(
        vec2(POSITION_WIDTH, ui.spacing().interact_size.y),
        egui::Label::new(format!("{} / {len}", cursor + 1)).extend(),
    );
    if ui
        .add_enabled(previous.is_some(), egui::Button::new("◀"))
        .on_hover_text("Previous image")
        .clicked()
    {
        request_nav(ui.ctx(), state, previous);
    }
}

/// Records a move for the parent to apply. The parent alone can read the
/// folder and start a decode, and a child pass never runs its `ui()`, so the
/// write has to wake it.
fn request_nav(ctx: &egui::Context, state: &mut LightboxShared, target: Option<usize>) {
    let Some(target) = target else {
        return;
    };
    state.nav_request = Some(target);
    wake_parent(ctx);
}

/// This window's transient copy note, cleared once it has aged out. The
/// repaint request is what brings that clearing frame around.
fn show_copied_note(ui: &mut Ui, state: &mut LightboxShared) {
    let expired = state
        .copied
        .as_ref()
        .is_some_and(|(_, when)| when.elapsed().as_secs_f32() > FLASH_SECONDS);
    if expired {
        state.copied = None;
    }
    let Some((label, _)) = &state.copied else {
        return;
    };
    _ = ui.separator();
    _ = ui.colored_label(ui.visuals().hyperlink_color, format!("Copied {label}"));
    ui.ctx().request_repaint_after(Duration::from_millis(200));
}

/// The copy-enabled pin values, above the control row and capped in height, so
/// neither a long prompt nor a pin-heavy image can move a control.
fn show_strip(ui: &mut Ui, state: &mut LightboxShared) {
    if state.detail.strip.is_empty() {
        return;
    }
    let mut copied: Option<(String, String)> = None;

    _ = egui::ScrollArea::vertical()
        .max_height(STRIP_HEIGHT)
        .show(ui, |ui| {
            for item in &state.detail.strip {
                _ = ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("📋").small().frame(false))
                        .on_hover_text("Copy full value")
                        .clicked()
                    {
                        copied = Some((item.label.clone(), item.value.clone()));
                    }
                    _ = ui.label(RichText::new(&item.label).strong());
                    _ = ui.add(egui::Label::new(RichText::new(&item.shown).monospace()).truncate());
                });
            }
        });
    _ = ui.separator();

    if let Some((label, value)) = copied {
        ui.ctx().copy_text(value);
        state.copied = Some((label, Instant::now()));
    }
}

/// The zoom controls, added right to left, so they read out as zoom out,
/// percentage, zoom in, native size, fit.
fn show_zoom_controls(ui: &mut Ui, state: &mut LightboxShared) {
    if ui
        .button("Fit")
        .on_hover_text("Fit the whole image in the window")
        .clicked()
    {
        state.pending_view = Some(ViewCommand::Fit);
    }
    if ui
        .button("1:1")
        .on_hover_text("Show one image pixel per screen pixel")
        .clicked()
    {
        state.pending_view = Some(ViewCommand::Native);
    }
    if ui.button("+").on_hover_text("Zoom in").clicked() {
        state.pending_view = Some(ViewCommand::ZoomBy(1.0));
    }
    show_zoom_readout(ui, state);
    if ui.button("−").on_hover_text("Zoom out").clicked() {
        state.pending_view = Some(ViewCommand::ZoomBy(-1.0));
    }
}

/// The zoom percentage, marked when the view is magnifying the decode past its
/// own pixels.
fn show_zoom_readout(ui: &mut Ui, state: &LightboxShared) {
    let soft = is_soft(
        state.zoom,
        state.detail.source_size[0],
        state.decoded_size[0],
    );
    let marker = if soft { " *" } else { "" };
    let text = format!("{}%{marker}", zoom_percent(state.zoom));
    let size = vec2(ZOOM_READOUT_WIDTH, ui.spacing().interact_size.y);
    let response = ui.add_sized(size, egui::Label::new(text).extend());
    if soft {
        _ = response.on_hover_text(format!(
            "Decoded at {}×{} pixels. The view is magnifying that decode.",
            state.decoded_size[0], state.decoded_size[1]
        ));
    }
}

/// Draws the image in a scene that pans and zooms. Scene coordinates are source
/// image pixels, which is what keeps the zoom readout honest whatever size the
/// decode came back at.
fn show_scene(ui: &mut Ui, state: &mut LightboxShared, texture: &TextureHandle) {
    // Inputs: the panel, the source size, and this frame's wheel.
    let pixels_per_point = sane_pixels_per_point(ui.pixels_per_point());
    let outer_size = ui.available_size_before_wrap();
    let outer_pixels = outer_size * pixels_per_point;
    let image_size = scene_image_size(state, texture);
    let image_rect = Rect::from_min_size(Pos2::ZERO, image_size);
    let wheel_steps = take_wheel_steps(ui);

    // Process: hold the view through whatever the panel did to it, resolve one
    // view change on top of that, then pull the result over the image before
    // `Scene` sees it. A bar button outranks the wheel, and both outrank the
    // standing refit.
    let held = scene_rect_for_panel(outer_pixels, state.scene_rect, state.zoom);
    let command = state
        .pending_view
        .take()
        .or_else(|| wheel_steps.map(ViewCommand::ZoomBy))
        .or_else(|| state.fit_mode.then_some(ViewCommand::Fit))
        // A rect `Scene` would reject carries no view to keep, which is where
        // the opening frame starts.
        .or_else(|| held.is_none().then_some(ViewCommand::Fit));
    if let Some(held) = held {
        state.scene_rect = held;
    }
    if let Some(command) = command {
        state.fit_mode = command == ViewCommand::Fit;
        state.scene_rect = view_scene_rect(
            command,
            state.scene_rect,
            outer_size,
            image_size,
            pixels_per_point,
        );
    }
    state.scene_rect = clamp_scene_rect(state.scene_rect, image_rect);

    // Output: draw, then take back whatever a drag wrote. `Scene` has no pan
    // limits of its own, so the same clamp runs on the way out.
    let mut scene_rect = state.scene_rect;
    let response = egui::Scene::new()
        .zoom_range(Rangef::new(
            MIN_ZOOM / pixels_per_point,
            MAX_ZOOM / pixels_per_point,
        ))
        // Focus here would swallow the arrow keys the window wants.
        .sense(Sense::DRAG)
        .max_inner_size(image_size)
        .show(ui, &mut scene_rect, |ui| {
            _ = ui.put(
                image_rect,
                egui::Image::new(texture).fit_to_exact_size(image_size),
            );
        })
        .response;
    if response.changed() {
        state.fit_mode = false;
    }
    state.scene_rect = clamp_scene_rect(scene_rect, image_rect);
    show_grab_cursor(ui, &response);
    record_zoom(ui, state, outer_pixels);
}

/// The size the scene is drawn in. IHDR is the truth. The decode stands in for
/// a file whose header never reached the entry.
fn scene_image_size(state: &LightboxShared, texture: &TextureHandle) -> Vec2 {
    for size in [state.detail.source_size, state.decoded_size] {
        if size[0] > 0 && size[1] > 0 {
            return vec2(size[0] as f32, size[1] as f32);
        }
    }
    texture.size_vec2()
}

/// Turns this frame's wheel into zoom steps and takes the scroll delta with it.
/// `Scene` pans on a plain wheel otherwise, and this window zooms instead.
/// Ctrl-scroll and pinch never reach here: egui reports those as a zoom.
fn take_wheel_steps(ui: &Ui) -> Option<f32> {
    if !ui.rect_contains_pointer(ui.available_rect_before_wrap()) {
        return None;
    }
    let delta = ui.ctx().input(|input| input.smooth_scroll_delta);
    if delta.y == 0.0 {
        return None;
    }
    ui.ctx().input_mut(|input| {
        input.smooth_scroll_delta = Vec2::ZERO;
    });
    Some(delta.y / WHEEL_POINTS_PER_STEP)
}

/// The hand is the only thing telling the viewer the image can be dragged.
fn show_grab_cursor(ui: &Ui, response: &egui::Response) {
    if response.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }
}

/// Records the zoom for the bar, which is built before this panel and so drew
/// the number this frame just replaced.
fn record_zoom(ui: &Ui, state: &mut LightboxShared, outer_pixels: Vec2) {
    let Some(zoom) = scene_scale(outer_pixels, state.scene_rect) else {
        return;
    };
    if zoom_percent(zoom) != zoom_percent(state.zoom) {
        ui.ctx().request_repaint();
    }
    state.zoom = zoom;
}

fn wake_parent(ctx: &egui::Context) {
    ctx.request_repaint_of(ViewportId::ROOT);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANEL: Vec2 = Vec2::new(800.0, 600.0);
    const WIDE_IMAGE: Vec2 = Vec2::new(1600.0, 400.0);
    const TALL_IMAGE: Vec2 = Vec2::new(400.0, 2400.0);

    /// One (pin, match) pair as the app's flattener hands it over.
    fn candidate(label: &str, copies: bool, value: &str) -> (String, bool, String) {
        (label.to_string(), copies, value.to_string())
    }

    fn assert_close(actual: f32, expected: f32, what: &str) {
        let tolerance = 1e-4 * expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= tolerance,
            "{what}: {actual} is not {expected}"
        );
    }

    /// Zoom of a resolved view: the scale measured against the panel in
    /// physical pixels.
    fn zoom_of(outer_size: Vec2, scene_rect: Rect, pixels_per_point: f32) -> f32 {
        scene_scale(outer_size * pixels_per_point, scene_rect).expect("the view has a scale")
    }

    #[test]
    fn distinct_serials_get_distinct_viewports() {
        // Two windows on one file differ only by serial, so a path-derived id
        // would collapse them into a single viewport.
        assert_ne!(LightboxId(1).viewport_id(), LightboxId(2).viewport_id());
    }

    #[test]
    fn one_serial_maps_to_one_viewport() {
        // The id is rebuilt every frame. A drifting hash would re-create the
        // window each pass.
        assert_eq!(LightboxId(7).viewport_id(), LightboxId(7).viewport_id());
    }

    #[test]
    fn the_cap_admits_up_to_the_limit_and_no_further() {
        assert!(can_open(0, MAX_LIGHTBOXES));
        assert!(can_open(MAX_LIGHTBOXES - 1, MAX_LIGHTBOXES));
        assert!(!can_open(MAX_LIGHTBOXES, MAX_LIGHTBOXES));
        assert!(!can_open(MAX_LIGHTBOXES + 1, MAX_LIGHTBOXES));
    }

    #[test]
    fn a_rect_built_for_a_scale_reads_that_scale_back() {
        // Every zoom goes out through one of these and comes back through the
        // other, so a lossy pair would drift the view a step at a time.
        let center = Pos2::new(120.0, -40.0);
        for scale in [0.05, 1.0, 8.0] {
            let rect = scene_rect_for_scale(PANEL, center, scale);
            let read_back = scene_scale(PANEL, rect).expect("a rect with area has a scale");
            assert_close(read_back, scale, "scale round trip");
            assert_close(rect.center().x, center.x, "centre x held");
            assert_close(rect.center().y, center.y, "centre y held");
        }
    }

    #[test]
    fn a_rect_scene_would_reject_has_no_scale() {
        assert!(scene_scale(PANEL, Rect::ZERO).is_none());
        assert!(scene_scale(PANEL, Rect::NAN).is_none());
        assert!(scene_scale(PANEL, Rect::EVERYTHING).is_none());
        let good = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 100.0));
        assert!(scene_scale(vec2(0.0, 600.0), good).is_none());
        assert!(scene_scale(vec2(f32::NAN, 600.0), good).is_none());
    }

    #[test]
    fn a_window_resize_holds_the_zoom_the_view_was_left_at() {
        // The rect is shaped to the panel it was resolved against, so a larger
        // panel has to reveal more image rather than magnify what is on screen.
        let large = vec2(1920.0, 1000.0);
        let native = view_scene_rect(ViewCommand::Native, Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        let zoom = zoom_of(PANEL, native, 1.0);
        assert_close(zoom, 1.0, "native zoom");

        let resized = scene_rect_for_panel(large, native, zoom).expect("the view has a scale");
        assert_close(
            zoom_of(large, resized, 1.0),
            zoom,
            "zoom held across a resize",
        );
        assert_close(resized.center().x, native.center().x, "centre x held");
        assert_close(resized.center().y, native.center().y, "centre y held");
        assert!(
            resized.width() > native.width(),
            "the larger panel shows more image: {resized:?}"
        );

        // Shrinking must not drift the other way either.
        let shrunk =
            scene_rect_for_panel(vec2(320.0, 240.0), resized, zoom).expect("the view has a scale");
        assert_close(
            zoom_of(vec2(320.0, 240.0), shrunk, 1.0),
            zoom,
            "zoom held across a shrink",
        );
    }

    #[test]
    fn a_view_with_no_recorded_zoom_keeps_the_one_it_measures() {
        // The zoom is a number the previous frame stored, so a view that never
        // recorded one has only its own rect to go on.
        let rect = scene_rect_for_scale(PANEL, Pos2::new(50.0, 25.0), 0.4);
        for zoom in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let held = scene_rect_for_panel(PANEL, rect, zoom).expect("the view has a scale");
            assert_close(zoom_of(PANEL, held, 1.0), 0.4, "measured scale kept");
        }
        // A zoom past the limits is held inside them.
        let far = scene_rect_for_panel(PANEL, rect, MAX_ZOOM * 4.0).expect("the view has a scale");
        assert_close(zoom_of(PANEL, far, 1.0), MAX_ZOOM, "held inside the limits");
    }

    #[test]
    fn a_view_scene_would_reject_asks_to_be_refitted() {
        assert!(scene_rect_for_panel(PANEL, Rect::ZERO, 1.0).is_none());
        assert!(scene_rect_for_panel(PANEL, Rect::NAN, 1.0).is_none());
        assert!(scene_rect_for_panel(PANEL, Rect::EVERYTHING, 1.0).is_none());
        let good = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 100.0));
        assert!(scene_rect_for_panel(Vec2::ZERO, good, 1.0).is_none());
    }

    #[test]
    fn fitting_binds_on_the_axis_with_less_room() {
        // The wide image runs out of width first, the tall one out of height.
        assert_close(fit_zoom(PANEL, WIDE_IMAGE, 1.0), 0.5, "wide image fit");
        assert_close(fit_zoom(PANEL, TALL_IMAGE, 1.0), 0.25, "tall image fit");
        // The panel is measured in points, so twice the pixel density fits the
        // same image at twice the zoom.
        assert_close(fit_zoom(PANEL, WIDE_IMAGE, 2.0), 1.0, "wide image on hidpi");
        // Nothing fits in a panel with no area.
        assert_close(fit_zoom(Vec2::ZERO, WIDE_IMAGE, 1.0), 1.0, "empty panel");
        assert_close(fit_zoom(PANEL, Vec2::ZERO, 1.0), 1.0, "empty image");
    }

    #[test]
    fn fitting_centres_the_image_and_spans_the_binding_axis() {
        let rect = view_scene_rect(ViewCommand::Fit, Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        assert_close(rect.center().x, WIDE_IMAGE.x / 2.0, "centred on x");
        assert_close(rect.center().y, WIDE_IMAGE.y / 2.0, "centred on y");
        assert_close(rect.width(), WIDE_IMAGE.x, "spans the binding axis");
        assert!(
            rect.height() > WIDE_IMAGE.y,
            "the other axis keeps its spare room: {rect:?}"
        );
        assert_close(zoom_of(PANEL, rect, 1.0), 0.5, "fit zoom");
    }

    #[test]
    fn native_size_is_one_source_pixel_per_screen_pixel() {
        let start = view_scene_rect(ViewCommand::Fit, Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        // The screen scale must not reach the number the readout shows.
        for pixels_per_point in [1.0, 2.0] {
            let rect = view_scene_rect(
                ViewCommand::Native,
                start,
                PANEL,
                WIDE_IMAGE,
                pixels_per_point,
            );
            assert_close(zoom_of(PANEL, rect, pixels_per_point), 1.0, "native zoom");
            assert_close(rect.center().x, start.center().x, "centre x held");
            assert_close(rect.center().y, start.center().y, "centre y held");
        }
    }

    #[test]
    fn zooming_in_and_back_out_returns_the_same_view() {
        let start = view_scene_rect(ViewCommand::Fit, Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        let closer = view_scene_rect(ViewCommand::ZoomBy(1.0), start, PANEL, WIDE_IMAGE, 1.0);
        let back = view_scene_rect(ViewCommand::ZoomBy(-1.0), closer, PANEL, WIDE_IMAGE, 1.0);
        let start_zoom = zoom_of(PANEL, start, 1.0);
        assert_close(
            zoom_of(PANEL, closer, 1.0),
            start_zoom * ZOOM_STEP,
            "one step in",
        );
        assert_close(
            zoom_of(PANEL, back, 1.0),
            start_zoom,
            "back to where it began",
        );
        assert_close(back.center().x, start.center().x, "centre x held");
        assert_close(back.center().y, start.center().y, "centre y held");
    }

    #[test]
    fn zooming_saturates_at_both_limits() {
        let start = view_scene_rect(ViewCommand::Fit, Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        let far_in = view_scene_rect(ViewCommand::ZoomBy(100.0), start, PANEL, WIDE_IMAGE, 1.0);
        let far_out = view_scene_rect(ViewCommand::ZoomBy(-100.0), start, PANEL, WIDE_IMAGE, 1.0);
        assert_close(zoom_of(PANEL, far_in, 1.0), MAX_ZOOM, "all the way in");
        assert_close(zoom_of(PANEL, far_out, 1.0), MIN_ZOOM, "all the way out");
    }

    #[test]
    fn zooming_from_a_view_with_no_scale_falls_back_to_fitting() {
        // This is the opening frame, before any pass has sized the panel.
        let zoomed = view_scene_rect(ViewCommand::ZoomBy(1.0), Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        let fitted = view_scene_rect(ViewCommand::Fit, Rect::ZERO, PANEL, WIDE_IMAGE, 1.0);
        assert_eq!(zoomed, fitted);
    }

    #[test]
    fn one_step_multiplies_and_one_step_back_divides() {
        assert_close(zoom_step(1.0, 1.0), ZOOM_STEP, "one step in");
        assert_close(zoom_step(1.0, -1.0), 1.0 / ZOOM_STEP, "one step out");
        assert_close(zoom_step(0.5, 0.0), 0.5, "no steps at all");
        assert_close(zoom_step(MAX_ZOOM, 1.0), MAX_ZOOM, "already at the top");
        assert_close(zoom_step(MIN_ZOOM, -1.0), MIN_ZOOM, "already at the bottom");
        assert_close(zoom_step(f32::NAN, 1.0), 1.0, "no zoom to step from");
    }

    #[test]
    fn clamping_holds_the_view_over_the_image_and_keeps_its_size() {
        let image = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 500.0));

        // Both axes have room to spare, so the view centres on the image.
        let loose = Rect::from_center_size(Pos2::new(5000.0, -20.0), vec2(2000.0, 900.0));
        let centred = clamp_scene_rect(loose, image);
        assert_close(centred.center().x, 500.0, "centred on x");
        assert_close(centred.center().y, 250.0, "centred on y");
        assert_close(centred.width(), loose.width(), "width held");
        assert_close(centred.height(), loose.height(), "height held");

        // Zoomed in past the right edge on x, with spare room still on y.
        let past = Rect::from_min_size(Pos2::new(900.0, -300.0), vec2(400.0, 900.0));
        let held = clamp_scene_rect(past, image);
        assert_close(held.max.x, 1000.0, "stopped at the right edge");
        assert_close(held.center().y, 250.0, "centred on the axis that fits");
        assert_close(held.width(), past.width(), "width held");
        assert_close(held.height(), past.height(), "height held");

        // Off the left edge on x, already inside the image on y.
        let before = Rect::from_min_size(Pos2::new(-800.0, 100.0), vec2(400.0, 200.0));
        let pulled = clamp_scene_rect(before, image);
        assert_close(pulled.min.x, 0.0, "pulled to the left edge");
        assert_close(pulled.min.y, 100.0, "left alone inside the image");
        assert_close(pulled.width(), before.width(), "width held");
        assert_close(pulled.height(), before.height(), "height held");
    }

    #[test]
    fn a_view_that_outruns_the_decode_is_soft() {
        // A 4000 pixel source decoded to 2000 goes soft above half size.
        assert!(is_soft(0.6, 4000, 2000));
        assert!(!is_soft(0.5, 4000, 2000));
        assert!(!is_soft(0.4, 4000, 2000));
        // A decode at full size is soft only past native size.
        assert!(is_soft(1.01, 800, 800));
        assert!(!is_soft(1.0, 800, 800));
        // Nothing has been decoded yet, so there is nothing to outrun.
        assert!(!is_soft(4.0, 800, 0));
        assert!(!is_soft(4.0, 0, 0));
    }

    #[test]
    fn stepping_stops_at_both_ends_instead_of_wrapping() {
        assert_eq!(step_cursor(0, 5, -1), None);
        assert_eq!(step_cursor(4, 5, 1), None);
        assert_eq!(step_cursor(0, 5, 1), Some(1));
        assert_eq!(step_cursor(4, 5, -1), Some(3));
        assert_eq!(step_cursor(2, 5, 1), Some(3));
    }

    #[test]
    fn an_empty_list_has_nowhere_to_step() {
        // A window opened before the first scan finished has no list yet, and
        // `len - 1` would underflow.
        assert_eq!(step_cursor(0, 0, 1), None);
        assert_eq!(step_cursor(0, 0, -1), None);
        assert_eq!(step_cursor(9, 0, 0), None);
    }

    #[test]
    fn a_cursor_past_the_end_clamps_back_into_range() {
        // The parent clamps with a zero step before indexing the list.
        assert_eq!(step_cursor(99, 5, 0), Some(4));
        assert_eq!(step_cursor(99, 5, -1), Some(3));
        assert_eq!(step_cursor(99, 5, 1), None);
    }

    #[test]
    fn the_strip_holds_its_order_and_drops_the_copy_disabled_values() {
        let candidates = [
            candidate("Positive Prompt", true, "a cat"),
            candidate("cfg", false, "4.5"),
            candidate("seed", true, "12"),
        ];
        let strip = build_strip(&candidates, MAX_STRIP_ITEMS, MAX_STRIP_CHARS);
        let listed: Vec<(&str, &str)> = strip
            .iter()
            .map(|item| (item.label.as_str(), item.value.as_str()))
            .collect();
        assert_eq!(listed, vec![("Positive Prompt", "a cat"), ("seed", "12")]);
    }

    #[test]
    fn one_pin_matching_twice_lists_both_values_under_one_label() {
        // A repeated key (generation vs upscale cfg) reaches the bar as two
        // entries sharing a label, so neither may collapse into the other.
        let candidates = [candidate("cfg", true, "4.5"), candidate("cfg", true, "1.0")];
        let strip = build_strip(&candidates, MAX_STRIP_ITEMS, MAX_STRIP_CHARS);
        assert_eq!(strip.len(), 2);
        assert_eq!(strip[0].value, "4.5");
        assert_eq!(strip[1].value, "1.0");
    }

    #[test]
    fn the_item_cap_bounds_the_flattened_list() {
        let candidates: Vec<(String, bool, String)> = (0..10)
            .map(|i| candidate("seed", true, &i.to_string()))
            .collect();
        assert_eq!(build_strip(&candidates, 3, MAX_STRIP_CHARS).len(), 3);
        assert!(build_strip(&candidates, 0, MAX_STRIP_CHARS).is_empty());
    }

    #[test]
    fn the_shown_form_is_one_capped_line_and_the_value_stays_whole() {
        let value = format!("line one\nline two {}", "x".repeat(500));
        let strip = build_strip(&[candidate("Positive Prompt", true, &value)], 8, 40);
        let item = strip.first().expect("the copy-enabled candidate");
        assert!(!item.shown.contains('\n'), "shown was {}", item.shown);
        // The cap plus the ellipsis `single_line` adds when it truncates.
        assert!(item.shown.chars().count() <= 41, "shown was {}", item.shown);
        // The clipboard gets the whole prompt, not the excerpt on screen.
        assert_eq!(item.value, value);
    }

    #[test]
    fn the_readout_rounds_and_never_reads_zero_for_a_real_zoom() {
        assert_eq!(zoom_percent(1.0), 100);
        assert_eq!(zoom_percent(0.125), 13);
        assert_eq!(zoom_percent(MIN_ZOOM), 2);
        assert_eq!(zoom_percent(0.0001), 1);
        assert_eq!(zoom_percent(0.0), 0);
        assert_eq!(zoom_percent(f32::NAN), 0);
    }
}
