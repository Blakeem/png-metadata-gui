# PNG Metadata GUI

<img src="screenshot.webp" alt="PNG Metadata GUI showing a folder of ComfyUI images with pinned metadata columns, a preview, and the metadata tree" width="100%">

A fast, cross-platform desktop viewer for PNG text chunks (`tEXt`, `zTXt`,
`iTXt`), built for browsing ComfyUI and Stable Diffusion metadata.

Open a folder and every image appears in a table with a sortable column for
each pinned field (seed, steps, cfg, denoise, and so on). Select an image to
browse its metadata as a tree where any value copies with a click. Search
filters the folder to images whose metadata matches. This suits A/B testing,
since a whole folder's settings show at once instead of one image at a time.

Written in Rust with [egui](https://github.com/emilk/egui). Single small
binary, instant startup, no browser engine.

## Features

- Folder table with thumbnails and a sortable column per pinned field
- Pin a key (`cfg`, matches every location), an exact path
  (`prompt.183.inputs.cfg`), or a **node title** ("Positive Prompt") on
  ComfyUI images. A title pin follows the node's `_meta.title`, so it keeps
  working across workflows whose node ids differ
- Rename any pin chip (right-click → Rename) to control its column header
- Repeated keys (generation vs upscale `cfg`) show every value, color-coded,
  with full paths in the tooltip
- Live search over keys, values, and dotted paths. The metadata tree prunes
  to matches and stays collapsible
- Find box that jumps to a field in the tree. Matching a node title opens the
  whole node, and Enter cycles through matches
- Collapsible JSON tree with expand/collapse all, copy buttons, and
  click-to-copy values
- A copy button beside every pinned value in the table, on by default for any
  pin named for a prompt or a seed. Right-click a pin chip to turn it on or off
- Click a thumbnail to open the image in its own window. Drag to pan, and the
  bottom bar holds zoom, fit, native size, and fullscreen. Arrow keys step
  through the table, and the copy-enabled pins are listed under the image.
  Open as many windows as you like to compare images side by side
- Column widths persist across launches and pin changes
- File size, dimensions, and created/modified dates are pinnable rows, so
  date columns and size sorting work like any tag
- Folders scan in the background with progress. A per-folder metadata cache
  makes reopening a folder near-instant, and a file is re-read only when its
  size or modified time changes
- Reopens your last folder on launch
- Drag and drop a folder or PNG onto the window. It also works with `Open
  with` or a path argument
- Reads metadata without decoding pixels. Thumbnails decode in the background
  with a bounded texture cache
- Handles ComfyUI `prompt` and `workflow` JSON (including Python-style `NaN`
  and non-ASCII text), plain `key=value` chunks, and custom keywords

## Install

Download a binary from [Releases](../../releases) for Windows, macOS, or
Linux. Unzip and run.

Or build from source with [Rust](https://rustup.rs):

```bash
git clone https://github.com/Blakeem/png-metadata-gui
cd png-metadata-gui
cargo build --release
```

Linux needs GTK3 headers for the file dialog: `sudo apt install libgtk-3-dev`

## Usage

```bash
png-metadata-gui                     # open empty, then drop a folder in
png-metadata-gui path/to/folder      # open a folder
png-metadata-gui image.png           # inspect one file
png-metadata-gui --dump image.png    # print flattened rows (debug builds)
```

Click a row to select it. Arrow keys navigate. Click a value in the tree to
copy it, or use any 📋 button. Right-click pin chips in the toolbar to
rename, reorder, remove, or toggle their copy button. The ⚙ button next to
the pin box holds settings (ComfyUI title pins).

Settings, pins, and the folder metadata cache live in your per-user app-data
directory (`%APPDATA%\PNG Metadata GUI` on Windows). The executable stays
standalone.

## Development

```bash
cargo test
cargo clippy --all-targets   # lints are deny-by-default, keep it clean
```

Pushing a tag like `v0.1.0` builds all platforms and attaches the binaries to
a GitHub Release.

## License

[MIT](LICENSE)
