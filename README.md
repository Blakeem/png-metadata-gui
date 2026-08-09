# PNG Metadata GUI

A fast, cross-platform desktop viewer for the text-chunk metadata (`tEXt`,
`zTXt`, `iTXt`) that image generators embed in PNG files — built for browsing
ComfyUI / Stable Diffusion outputs, useful for any PNG with text chunks.

Open a folder and every image appears in a table: thumbnail, file name, and one
sortable column per **pinned field** (seed, steps, cfg, denoise, …). Select an
image to see its full metadata as a collapsible tree where any value copies
with a click. Type in the search box to filter the folder to images whose
metadata matches, with the matching value shown per row.

Written in Rust with [egui](https://github.com/emilk/egui) — a single small
binary with instant startup, no browser engine, no runtime dependencies.

## Why

When A/B testing generation settings you end up with a folder of near-identical
images and a question like "which of these used denoise 0.35?" Dropping each
image into ComfyUI or a single-file inspector one at a time is slow. This shows
the settings for the whole folder at once, in columns you choose.

## Features

- **Folder table** — thumbnails plus a sortable column per pinned field; sort
  by any pin, file name, or file date.
- **Pins** — pin a *key* (`cfg`, matches every metadata style that uses that
  key) or an *exact path* (`prompt.183.inputs.cfg`, one location only —
  right-click any 📌 in the tree). When a key appears more than once (e.g.
  generation vs upscale `cfg`), all values show, color-coded by position, and
  the tooltip lists each value's full path. Pins persist between launches.
- **Search** — live substring filter over every key, path, and value in every
  image; the table shrinks to matching images and shows what matched.
- **Metadata tree** — chunks rendered as collapsible JSON with expand/collapse
  all; click any value to copy it; "Pinned only" mode shows just your pinned
  fields with their locations.
- **File metadata as data** — size, dimensions, and created/modified dates are
  ordinary pinnable rows, so date columns and size sorting work like any tag.
- **Drag and drop** — drop a folder or a single PNG onto the window; also
  works via `Open with` or a path argument.
- **Robust parsing** — reads `tEXt`/`zTXt`/`iTXt` without decoding pixel data
  (metadata for a folder of 8 MB PNGs loads instantly), handles ComfyUI's
  `prompt`/`workflow` JSON including Python-style `NaN`, plain `key=value`
  chunks, and arbitrary custom keywords.

## Install

**Download a binary** from the [Releases](../../releases) page — Windows,
macOS (Apple Silicon and Intel), and Linux. Unzip and run; there is nothing to
install.

**Or build from source** (needs [Rust](https://rustup.rs)):

```bash
git clone https://github.com/blakeem/png-metadata-gui
cd png-metadata-gui
cargo build --release
# binary at target/release/png-metadata-gui
```

Linux builds need GTK3 headers for the file dialog: `sudo apt install libgtk-3-dev`.

## Usage

```bash
png-metadata-gui                     # opens empty; drop a folder or use Open Folder
png-metadata-gui path/to/folder      # open a folder directly
png-metadata-gui image.png           # inspect a single file
png-metadata-gui --dump image.png    # print all flattened path=value rows (debug builds)
```

- Click a row to select it; arrow keys navigate.
- Click a 📌 in the tree to pin that key; right-click the 📌 for exact-path
  pinning. Right-click a pin chip in the toolbar to reorder or remove it.
- Click any value (tree, table cell, or pinned list) to copy it.

## Development

```bash
cargo test
cargo clippy --all-targets   # lints are deny-by-default; keep it clean
cargo build --release
```

Releases are built by CI: pushing a tag like `v0.1.0` builds all platforms and
attaches the binaries to a GitHub Release.

## License

[MIT](LICENSE)
