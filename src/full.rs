//! Background full-resolution decoding for lightbox windows.
//!
//! One worker decodes one image at a time, so a lightbox can never take a
//! thumbnail slot in [`crate::thumbs`]. Requests are keyed by [`LightboxId`]
//! rather than by path, because two windows may show one file. Every enqueued
//! request produces exactly one [`FullResult`], superseded ones included, so a
//! window always reaches a terminal state.
//!
//! Two clamps sit between a file and the GPU. [`decode_size`] shrinks an image
//! to a size the driver accepts, and [`source_too_large`] refuses a file
//! outright before `image::open` allocates it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::lightbox::LightboxId;

/// Hard ceiling on either side of an uploaded texture. An oversized upload
/// panics inside wgpu, and a release build leaves no frame of ours on that
/// stack, so the guard lives here instead. 8192 is the side every backend this
/// ships on accepts.
pub const TEXTURE_SIDE_GUARD: u32 = 8192;

/// Pixel budget for one lightbox texture. RGBA8 costs four bytes a pixel, so
/// this is roughly 96 MB a window and 576 MB across the
/// [`crate::lightbox::MAX_LIGHTBOXES`] cap. It answers a memory question,
/// which is a different question from the crash [`TEXTURE_SIDE_GUARD`]
/// prevents.
pub const MAX_LIGHTBOX_PIXELS: u64 = 24_000_000;

/// Source size past which a file is refused instead of decoded. `image::open`
/// materializes the whole image before any clamp can shrink it, so a 400
/// megapixel PNG would cost about 1.6 GB of RAM just to reject it. This caps
/// that transient allocation near 400 MB.
pub const MAX_SOURCE_PIXELS: u64 = 100_000_000;

/// One queued decode. `source_size` comes from the caller's IHDR data, so the
/// worker can refuse an image without first allocating it.
#[derive(Clone, Debug)]
struct FullRequest {
    id: LightboxId,
    path: PathBuf,
    source_size: [u32; 2],
    max_side: u32,
}

/// Terminal state of one decode. `Superseded` carries no pixels: a later
/// request for the same window replaced this one, and the variant exists only
/// so every request still answers exactly once.
pub enum FullOutcome {
    Ready {
        image: egui::ColorImage,
        size: [u32; 2],
    },
    Refused(String),
    Failed(String),
    Superseded,
}

/// One finished decode. No `Debug`: `ColorImage` has no `Debug` impl.
pub struct FullResult {
    pub id: LightboxId,
    pub path: PathBuf,
    pub outcome: FullOutcome,
}

/// Owns the decode worker and the channel to it. One long-lived instance in
/// the app.
pub struct FullPool {
    request_tx: Sender<FullRequest>,
    result_rx: Receiver<FullResult>,
    /// The path each open window currently wants. The worker re-reads it after
    /// dequeuing, so a burst of requests for one window decodes only the last.
    wanted: Arc<Mutex<HashMap<LightboxId, PathBuf>>>,
}

impl FullPool {
    pub fn new(ctx: egui::Context) -> Self {
        let (request_tx, request_rx) = channel::<FullRequest>();
        let (result_tx, result_rx) = channel::<FullResult>();
        let wanted: Arc<Mutex<HashMap<LightboxId, PathBuf>>> = Arc::new(Mutex::new(HashMap::new()));

        let worker_wanted = Arc::clone(&wanted);
        _ = std::thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let still_wanted = worker_wanted
                    .lock()
                    .expect("full decode map poisoned")
                    .get(&request.id)
                    .is_some_and(|path| path == &request.path);
                let outcome = if still_wanted {
                    // A panic inside the decoder would kill the only worker and
                    // starve every later request, so catch it here.
                    std::panic::catch_unwind(|| decode_full(&request)).unwrap_or_else(|_| {
                        FullOutcome::Failed("The decoder panicked on this image.".to_string())
                    })
                } else {
                    FullOutcome::Superseded
                };
                let result = FullResult {
                    id: request.id,
                    path: request.path,
                    outcome,
                };
                if result_tx.send(result).is_err() {
                    return;
                }
                // Only the root pass drains this channel, and a bare
                // request_repaint() resolves against whichever viewport the UI
                // thread is inside at that instant.
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            }
        });

        Self {
            request_tx,
            result_rx,
            wanted,
        }
    }

    /// Queues a decode for `id`, replacing whatever that window asked for
    /// before. Unlike [`crate::thumbs::ThumbPool`] this never de-duplicates by
    /// key: a second request for one window is a navigation step, and dropping
    /// it would leave that window showing the wrong file.
    pub fn request(&mut self, id: LightboxId, path: PathBuf, source_size: [u32; 2], max_side: u32) {
        _ = self
            .wanted
            .lock()
            .expect("full decode map poisoned")
            .insert(id, path.clone());
        let request = FullRequest {
            id,
            path,
            source_size,
            max_side,
        };
        if self.request_tx.send(request).is_err() {
            // The worker only exits as the process tears down, so nothing is
            // left to answer this id.
            _ = self
                .wanted
                .lock()
                .expect("full decode map poisoned")
                .remove(&id);
        }
    }

    /// Forgets a closed window. Without this its `wanted` entry leaks.
    pub fn forget(&mut self, id: LightboxId) {
        _ = self
            .wanted
            .lock()
            .expect("full decode map poisoned")
            .remove(&id);
    }

    pub fn poll(&mut self) -> Vec<FullResult> {
        self.result_rx.try_iter().collect()
    }
}

/// Whether an image is too large to decode at all. The caller checks this
/// before `image::open`, which is the call that would allocate it.
pub fn source_too_large(src: [u32; 2], max_pixels: u64) -> bool {
    u64::from(src[0]) * u64::from(src[1]) > max_pixels
}

/// The size to decode `src` at: within `max_side` on each axis and within
/// `max_pixels` in total, aspect preserved, never enlarged, each axis at least
/// one pixel.
pub fn decode_size(src: [u32; 2], max_side: u32, max_pixels: u64) -> [u32; 2] {
    if src[0] == 0 || src[1] == 0 {
        return src;
    }
    fit_to_pixels(fit_to_side(src, max_side), max_pixels)
}

/// Fits both axes under `max_side` in integer math, so the long axis lands on
/// the limit exactly rather than a rounding step below it.
fn fit_to_side(src: [u32; 2], max_side: u32) -> [u32; 2] {
    let [width, height] = src;
    let limit = max_side.max(1);
    if width <= limit && height <= limit {
        return src;
    }
    if width >= height {
        [limit, shrink(height, limit, width)]
    } else {
        [shrink(width, limit, height), limit]
    }
}

/// Fits the total pixel count under `max_pixels`. The scale floors rather than
/// rounds, because rounding both axes up can push the result back over budget.
fn fit_to_pixels(src: [u32; 2], max_pixels: u64) -> [u32; 2] {
    let pixels = u64::from(src[0]) * u64::from(src[1]);
    if pixels <= max_pixels {
        return src;
    }
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    [scale_axis(src[0], scale), scale_axis(src[1], scale)]
}

fn shrink(value: u32, numerator: u32, denominator: u32) -> u32 {
    let scaled = u64::from(value) * u64::from(numerator) / u64::from(denominator.max(1));
    (scaled as u32).max(1)
}

fn scale_axis(value: u32, scale: f64) -> u32 {
    ((f64::from(value) * scale).floor() as u32).clamp(1, value.max(1))
}

/// Reads the pixel dimensions from the file header. `image::open` would decode
/// the pixels too, which is the allocation the refusal cap exists to avoid.
fn header_size(path: &Path) -> Result<[u32; 2], String> {
    let reader = image::ImageReader::open(path).map_err(|err| err.to_string())?;
    let (width, height) = reader.into_dimensions().map_err(|err| err.to_string())?;
    Ok([width, height])
}

fn decode_full(request: &FullRequest) -> FullOutcome {
    // Inputs: the caller's IHDR size, or the header when its entry is gone.
    let source = if request.source_size[0] > 0 && request.source_size[1] > 0 {
        request.source_size
    } else {
        match header_size(&request.path) {
            Ok(size) => size,
            Err(err) => {
                return FullOutcome::Failed(format!("The image header could not be read. {err}"));
            }
        }
    };

    // Refuse before the decoder allocates, then clamp and resize.
    if source_too_large(source, MAX_SOURCE_PIXELS) {
        return FullOutcome::Refused(format!(
            "This image is {}×{} pixels. The viewer refuses anything above {} megapixels.",
            source[0],
            source[1],
            MAX_SOURCE_PIXELS / 1_000_000
        ));
    }
    let decoded = match image::open(&request.path) {
        Ok(decoded) => decoded,
        Err(err) => return FullOutcome::Failed(format!("The image could not be decoded. {err}")),
    };
    let decoded_source = [decoded.width(), decoded.height()];
    let target = decode_size(decoded_source, request.max_side, MAX_LIGHTBOX_PIXELS);
    // Resampling an image already inside both limits would cost a full pass and
    // soften it for nothing.
    let rgba = if target == decoded_source {
        decoded.into_rgba8()
    } else {
        decoded.thumbnail_exact(target[0], target[1]).into_rgba8()
    };

    let size = [rgba.width() as usize, rgba.height() as usize];
    FullOutcome::Ready {
        size: [rgba.width(), rgba.height()],
        image: egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget large enough that only the side limit can bind.
    const NO_BUDGET: u64 = u64::MAX;

    /// Writes a real PNG, since the worker decodes from disk.
    fn write_png(path: &std::path::Path, width: u32, height: u32) {
        let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 9, 9, 255]));
        buffer.save(path).expect("write test png");
    }

    /// Drains the pool until it answers, so the test fails by timing out
    /// rather than by hanging the suite.
    fn wait_for_result(pool: &mut FullPool) -> FullResult {
        for _ in 0..600 {
            if let Some(result) = pool.poll().into_iter().next() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the decode worker never answered");
    }

    #[test]
    fn a_second_request_for_one_window_still_answers() {
        // A navigation step is a second request for a window that already has
        // one. Dropping it would leave that window on a spinner forever.
        let dir = std::env::temp_dir().join("png-metadata-gui-full-pool");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let first = dir.join("first.png");
        let second = dir.join("second.png");
        write_png(&first, 8, 8);
        write_png(&second, 16, 4);

        let mut pool = FullPool::new(egui::Context::default());
        let id = crate::lightbox::LightboxId(1);

        pool.request(id, first.clone(), [8, 8], 4096);
        let opened = wait_for_result(&mut pool);
        assert_eq!(opened.path, first);
        assert!(matches!(opened.outcome, FullOutcome::Ready { .. }));

        pool.request(id, second.clone(), [16, 4], 4096);
        let stepped = wait_for_result(&mut pool);
        assert_eq!(stepped.path, second);
        assert!(
            matches!(stepped.outcome, FullOutcome::Ready { .. }),
            "the step must decode, not report itself superseded"
        );
    }

    #[test]
    fn the_long_side_lands_on_the_limit_and_the_ratio_holds() {
        // 12000×4000 is 3:1, and 8192/3 floors to 2730.
        assert_eq!(decode_size([12000, 4000], 8192, NO_BUDGET), [8192, 2730]);
    }

    #[test]
    fn an_image_under_the_limit_is_never_enlarged() {
        // `image::thumbnail` upscales here. That was the thumbnail bug.
        assert_eq!(decode_size([512, 512], 4096, NO_BUDGET), [512, 512]);
        assert_eq!(decode_size([100, 40], 8192, NO_BUDGET), [100, 40]);
    }

    #[test]
    fn an_extreme_ratio_keeps_its_short_axis_at_one_pixel() {
        // 3 × 8192/16000 is 1.536, and a zero-height texture is not uploadable.
        assert_eq!(decode_size([16000, 3], 8192, NO_BUDGET), [8192, 1]);
    }

    #[test]
    fn the_pixel_budget_alone_shrinks_a_square_image() {
        assert_eq!(decode_size([4000, 4000], 8192, 1_000_000), [1000, 1000]);
    }

    #[test]
    fn the_tighter_of_the_two_limits_wins() {
        assert_eq!(decode_size([20000, 20000], 8192, NO_BUDGET), [8192, 8192]);
        // The same source under a 4 megapixel budget is cut further.
        assert_eq!(decode_size([20000, 20000], 8192, 4_000_000), [2000, 2000]);
    }

    #[test]
    fn a_zero_sized_source_does_not_panic() {
        assert_eq!(decode_size([0, 0], 8192, NO_BUDGET), [0, 0]);
        assert_eq!(decode_size([0, 100], 8192, 1000), [0, 100]);
        assert_eq!(decode_size([100, 0], 8192, 1000), [100, 0]);
    }

    #[test]
    fn no_source_size_ever_produces_a_side_over_the_limit() {
        // This is the property the process depends on: an oversized upload
        // panics inside wgpu, not in code we can see.
        let sizes = [
            [1, 1],
            [512, 512],
            [8192, 8192],
            [8193, 1],
            [12000, 4000],
            [16000, 3],
            [30000, 30000],
            [65535, 65535],
            [1, 65535],
        ];
        for src in sizes {
            for max_side in [1, 64, 2048, 4096, 8192] {
                let result = decode_size(src, max_side, MAX_LIGHTBOX_PIXELS);
                assert!(
                    result[0] <= max_side && result[1] <= max_side,
                    "{src:?} at max_side {max_side} produced {result:?}"
                );
                assert!(result[0] >= 1 && result[1] >= 1);
                assert!(result[0] <= src[0] && result[1] <= src[1]);
            }
        }
    }

    #[test]
    fn the_pixel_budget_binds_on_every_clamped_result() {
        for src in [[8192, 8192], [20000, 20000], [65535, 4]] {
            let result = decode_size(src, TEXTURE_SIDE_GUARD, MAX_LIGHTBOX_PIXELS);
            let pixels = u64::from(result[0]) * u64::from(result[1]);
            assert!(pixels <= MAX_LIGHTBOX_PIXELS, "{src:?} produced {result:?}");
        }
    }

    #[test]
    fn the_refusal_cap_admits_up_to_itself_and_no_further() {
        // 10000×10000 is exactly 100 megapixels.
        assert!(!source_too_large([10000, 10000], MAX_SOURCE_PIXELS));
        assert!(source_too_large([10000, 10001], MAX_SOURCE_PIXELS));
        assert!(!source_too_large([4096, 4096], MAX_SOURCE_PIXELS));
        // The product overflows u32, so the check has to widen first.
        assert!(source_too_large([65535, 65535], MAX_SOURCE_PIXELS));
    }
}
