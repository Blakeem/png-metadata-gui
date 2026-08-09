//! Background thumbnail decoding: worker threads decode PNGs at reduced size
//! and hand RGBA buffers back to the UI thread for texture upload.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui;

pub const THUMB_MAX_DIM: u32 = 512;
const WORKER_COUNT: usize = 3;

type ThumbResult = (PathBuf, Option<egui::ColorImage>);

pub struct ThumbPool {
    request_tx: Sender<PathBuf>,
    result_rx: Receiver<ThumbResult>,
    pending: usize,
}

impl ThumbPool {
    pub fn new(ctx: egui::Context) -> Self {
        let (request_tx, request_rx) = channel::<PathBuf>();
        let (result_tx, result_rx) = channel::<ThumbResult>();
        let shared_rx = Arc::new(Mutex::new(request_rx));

        for _ in 0..WORKER_COUNT {
            let worker_rx = Arc::clone(&shared_rx);
            let worker_tx = result_tx.clone();
            let worker_ctx = ctx.clone();
            _ = std::thread::spawn(move || {
                loop {
                    let request = {
                        let guard = worker_rx.lock().expect("thumb queue poisoned");
                        guard.recv()
                    };
                    let Ok(path) = request else { return };
                    let image = decode_thumbnail(&path);
                    if worker_tx.send((path, image)).is_err() {
                        return;
                    }
                    worker_ctx.request_repaint();
                }
            });
        }

        Self {
            request_tx,
            result_rx,
            pending: 0,
        }
    }

    pub fn request(&mut self, path: PathBuf) {
        if self.request_tx.send(path).is_ok() {
            self.pending += 1;
        }
    }

    pub fn poll(&mut self) -> Vec<ThumbResult> {
        let results: Vec<ThumbResult> = self.result_rx.try_iter().collect();
        self.pending = self.pending.saturating_sub(results.len());
        results
    }

    pub fn pending(&self) -> usize {
        self.pending
    }
}

fn decode_thumbnail(path: &Path) -> Option<egui::ColorImage> {
    let decoded = image::open(path).ok()?;
    let thumb = decoded.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM).to_rgba8();
    let size = [thumb.width() as usize, thumb.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()))
}
