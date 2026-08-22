//! Background thumbnail decoding: worker threads decode PNGs at reduced size
//! and hand RGBA buffers back to the UI thread for texture upload.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::full;

pub const THUMB_MAX_DIM: u32 = 512;
const WORKER_COUNT: usize = 3;

type ThumbResult = (PathBuf, Option<egui::ColorImage>);

pub struct ThumbPool {
    request_tx: Sender<PathBuf>,
    result_rx: Receiver<ThumbResult>,
    /// Paths queued but not yet returned. Doubles as request de-duplication
    /// (the UI re-requests evicted thumbnails every frame) and as the pending
    /// count, so the two can never drift apart.
    in_flight: HashSet<PathBuf>,
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
                    // A panic inside the decoder would kill this worker and
                    // starve the request of its result; catch it so every
                    // request still produces exactly one result.
                    let image = std::panic::catch_unwind(|| decode_thumbnail(&path))
                        .ok()
                        .flatten();
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
            in_flight: HashSet::new(),
        }
    }

    /// Queue a decode. A path already in flight is a no-op, so the UI may
    /// re-request a missing thumbnail every frame without piling up work.
    pub fn request(&mut self, path: PathBuf) {
        if self.in_flight.contains(&path) {
            return;
        }
        if self.request_tx.send(path.clone()).is_ok() {
            _ = self.in_flight.insert(path);
        }
    }

    pub fn poll(&mut self) -> Vec<ThumbResult> {
        let results: Vec<ThumbResult> = self.result_rx.try_iter().collect();
        for (path, _) in &results {
            _ = self.in_flight.remove(path);
        }
        results
    }

    pub fn pending(&self) -> usize {
        self.in_flight.len()
    }
}

fn decode_thumbnail(path: &Path) -> Option<egui::ColorImage> {
    let decoded = image::open(path).ok()?;
    let source = [decoded.width(), decoded.height()];
    // `DynamicImage::thumbnail` enlarges anything under the box, which wasted
    // memory and blurred small images. `decode_size` never enlarges.
    let target = full::decode_size(source, THUMB_MAX_DIM, u64::MAX);
    let thumb = if target == source {
        decoded.into_rgba8()
    } else {
        decoded.thumbnail_exact(target[0], target[1]).into_rgba8()
    };
    let size = [thumb.width() as usize, thumb.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()))
}
