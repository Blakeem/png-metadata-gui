# png-metadata-gui

Fast cross-platform desktop viewer for PNG text chunks (tEXt/zTXt/iTXt), aimed at
ComfyUI / Stable Diffusion image metadata. Opens a folder into a thumbnail table
with pinnable metadata columns, search across all chunk data, and a JSON tree
with click-to-copy. Pure Rust, egui/eframe — single static binary, no webview.

## Commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # cargo is not on PATH in this environment

cargo test                              # unit tests (index + model)
cargo clippy --all-targets              # must be zero errors/warnings before commit
cargo build --release                   # optimized single exe (LTO, stripped)

# Run with a folder or file; also supports drag-and-drop and "Open with"
./target/release/png-metadata-gui.exe "C:\path\to\images"

# Release: push a tag like v0.1.0 — .github/workflows/release.yml builds
# Windows/macOS/Linux binaries and attaches them to a GitHub Release.
# Never commit binaries to the repo.

# Print all flattened path=value rows (debug build only — release detaches from console)
cargo run -- --dump "C:\path\to\image.png"
```

Test data: `C:\Users\Blake\Documents\ComfyUI\output\AB-Test-Images` contains both
metadata styles (ComfyUI `prompt`/`workflow` JSON and plain `key=value` chunks).

## Toolchain (Windows)

Use the **MSVC** toolchain (`stable-x86_64-pc-windows-msvc`, the rustup default
here). The windows-gnu toolchain cannot build this project: rustup's bundled
MinGW lacks `as.exe`, which `dlltool` needs for the `windows-*` crates'
raw-dylib import libraries.

## Architecture

Data flows one way; UI state lives only in `app.rs`:

```
chunks.rs        index.rs           model.rs            thumbs.rs      app.rs
PNG chunk    →   flatten payload →  ImageEntry:      →  background  →  egui UI:
reader           to MetaRow list    file.* rows +       thumbnail      table, tree,
(seek-skips      (JSON tree or      chunk rows +        decode pool    pins, search
pixel data)      key=value text)    pin matching
```

- `chunks.rs` — minimal PNG reader: IHDR dims + text chunks only, seeks past
  IDAT. UTF-8 first, Latin-1 fallback.
- `index.rs` — `MetaRow { path, key, value, is_direct, sort_key }`. JSON
  flattening plus the lenient-JSON sanitizer (see gotchas).
- `model.rs` — `ImageEntry` (one per file). Synthesizes file metadata
  (`file.size`, `file.dimensions`, `file.created`, …) as ordinary rows so it
  pins/searches/sorts like tEXt data. Owns pin matching.
- `thumbs.rs` — worker threads decode at ≤512px, send RGBA back for texture
  upload; every request produces exactly one result (even on failure) so the
  pending counter stays accurate.
- `app.rs` — all UI. Widget closures collect `UiActions` per frame; mutations
  apply after rendering (never mutate app state inside a widget closure).

## Key invariants

- **`is_direct`**: JSON array elements get `is_direct = false` and inherit the
  nearest named key for *search only*. Key pins match only direct rows —
  ComfyUI node links like `"steps": ["123", 0]` must never satisfy a `steps`
  pin (the `123` is a node id, not a value).
- **Pin forms**: a bare pin (`cfg`) matches every direct leaf with that key; a
  pin containing `.` is an exact-path pin (`prompt.183.inputs.cfg`) matching
  one location. Multiple matches render in document order, color-coded by
  ordinal (`ordinal_color`), consistently in table cells and the pinned list.
- **JSON order**: serde_json's `preserve_order` feature is required — the tree
  and match ordering must reflect file order, not alphabetical.
- **Lenient JSON**: ComfyUI (Python `json.dumps`) emits bare `NaN`/`Infinity`.
  `replace_nonfinite_tokens` (string-literal-aware) maps them to `null`, and
  only runs when strict parsing fails. Don't "simplify" it into a regex — it
  must not touch string contents.
- **Pins persistence**: stored under a versioned key (`pins_v3`). Changing the
  default pin set requires bumping the version, or existing installs never see
  the new defaults.

## egui 0.36 API notes (differs from older docs/examples)

- `eframe::App::ui(&mut self, ui: &mut egui::Ui, frame)` — not `update(ctx)`.
- Panels are unified: `egui::Panel::top/bottom/left(id)` shown on `ui`;
  `CentralPanel` still exists and goes last.
- `ui.close()` closes menus (`close_menu()` is gone).
- Dropped files: `DroppedFile` is a trait; use `.path()`.
- `Get-Process MainWindowHandle` can return a winit helper window, not the
  real one — enumerate windows by PID when driving the app externally.

## Lint policy

`Cargo.toml` denies `unsafe_code`, `missing_docs`, `unused_results`,
`missing_debug_implementations`, rustdoc `broken_intra_doc_links`, and clippy
`unwrap_used` (egui-style). Discard unused widget responses with `_ = ui.label(…);`
— never `#[allow]`, never downgrade the lints. Run `cargo clippy --all-targets`
and `cargo test` clean before every commit.
