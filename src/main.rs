//! A fast, cross-platform viewer for the text chunks (`tEXt`, `zTXt`, `iTXt`)
//! embedded in PNG files, aimed at ComfyUI / Stable Diffusion image metadata.
//!
//! It shows a folder as a table with pinnable (and renamable) metadata
//! columns, searches keys, values, and dotted paths across the folder, and
//! renders each chunk's JSON as a tree where any value can be copied.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cache;
mod chunks;
mod index;
mod model;
mod scan;
mod thumbs;

use std::path::PathBuf;

use eframe::egui;

/// Doubles as eframe's storage app-id — [`cache::default_cache_dir`] resolves
/// the same per-user directory from it, so the two must stay one constant.
pub const APP_NAME: &str = "PNG Metadata GUI";

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("--dump") {
        run_dump(&args[1..]);
        return Ok(());
    }

    // A single path argument (folder or file) opens it at startup, which also
    // enables "Open with" from the OS shell.
    let initial_target: Option<PathBuf> = args.first().map(PathBuf::from);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([900.0, 560.0]);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(app::ViewerApp::new(cc, initial_target)))),
    )
}

/// Decodes the embedded window/taskbar icon. `None` (icon-less window) only
/// if the compiled-in PNG is somehow undecodable — never a startup failure.
fn load_window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon-256.png");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn run_dump(paths: &[String]) {
    for path in paths {
        match model::ImageEntry::load(PathBuf::from(path)) {
            Ok(entry) => {
                println!(
                    "=== {} ({}x{}, {} bytes) ===",
                    entry.file_name, entry.width, entry.height, entry.file_size
                );
                for row in &entry.rows {
                    println!("{} = {}", row.path, row.value);
                }
            }
            Err(err) => eprintln!("{path}: {err}"),
        }
    }
}
