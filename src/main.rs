//! A fast, cross-platform viewer for the text chunks (`tEXt`, `zTXt`, `iTXt`)
//! embedded in PNG files, aimed at ComfyUI / Stable Diffusion image metadata.
//!
//! It shows a folder as a table with pinnable metadata columns, searches every
//! key, path, and value across the folder, and renders each chunk's JSON as a
//! tree where any value can be clicked to copy it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chunks;
mod index;
mod model;
mod thumbs;

use std::path::PathBuf;

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("--dump") {
        run_dump(&args[1..]);
        return Ok(());
    }

    // A single path argument (folder or file) opens it at startup, which also
    // enables "Open with" from the OS shell.
    let initial_target: Option<PathBuf> = args.first().map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PNG tEXt Viewer",
        options,
        Box::new(move |cc| Ok(Box::new(app::ViewerApp::new(cc, initial_target)))),
    )
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
