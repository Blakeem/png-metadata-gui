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

Data flows one way; UI state lives only in `app.rs`; disk work lives on
worker threads (scan.rs for metadata, thumbs.rs for pixels):

```
scan.rs (worker thread, one per scan)                      app.rs (UI thread)
  list dir → cache.rs lookup ── hit ─→ rebuild entry ──┐   entries stream in
              │ miss                                   ├─→ over a channel with
              └→ chunks.rs read → record in new cache ─┘   a generation tag

chunks.rs        index.rs           model.rs            thumbs.rs      app.rs
PNG chunk    →   flatten payload →  ImageEntry:      →  background  →  egui UI:
reader           to MetaRow list    file.* rows +       thumbnail      table, tree,
(seek-skips      (JSON tree or      chunk rows +        decode pool    pins, search,
pixel data)      key=value text)    pin matching        (LRU capped)   find, settings
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
- `cache.rs` — per-folder metadata cache: gzip'd JSON, one file per folder
  (path-hash name) under `metadata-cache/` in the eframe storage dir
  (`%APPDATA%\PNG Metadata GUI\`). Stores raw text chunks + the
  (size, mtime) change detector.
- `scan.rs` — background folder scans: listing, cache lookup, and PNG reads
  off the UI thread; streams `ScanEvent`s tagged with a generation (a new
  scan supersedes the old one, whose messages are dropped and whose cache
  write is skipped).
- `app.rs` — all UI. Widget closures collect `UiActions` per frame; mutations
  apply after rendering (never mutate app state inside a widget closure).
- `assets/` — `icon.svg` (source of truth) → `icon.ico` + `icon-256.png`,
  regenerated with `tools/icon-forge` (standalone dev crate:
  `cargo run -- ../../assets/icon.svg out icon` from its dir). `build.rs`
  embeds the .ico on Windows; `main.rs` sets the runtime window icon.
  Alternate designs live in `assets/icon/candidate-*.svg`.

## Key invariants

- **`is_direct`**: JSON array elements get `is_direct = false` and inherit the
  nearest named key for *search only*. Key pins match only direct rows —
  ComfyUI node links like `"steps": ["123", 0]` must never satisfy a `steps`
  pin (the `123` is a node id, not a value).
- **Pin forms**: a pin is `pattern` + `PinMode` (model.rs). `Key`: bare key
  equality on direct leaves, or an exact path when the pattern contains `.`.
  `Title`: ComfyUI node-title substring — matches the direct `inputs.*` leaves
  of every node whose `_meta.title` contains the pattern (node ids differ
  across workflows; titles are the stable handle; `ImageEntry.node_titles` is
  the precomputed prefix→title map). `Auto`: union of both — the mode for
  typed pins. Tree-pinning a titled value creates a `Title` pin; the default
  pins stay `Key` so e.g. a node titled "PerpNegGuider Cfg" can never pollute
  the `cfg` column. Title matching is gated by the `comfyui_titles` setting.
  A pin may carry a display `label`; matching always uses `pattern`, display
  always uses `pin_label`. Multiple matches render in document order,
  color-coded by ordinal (`ordinal_color`), in table cells and the pinned list.
- **Search forms**: a bare query matches key or value (`row_matches`); a query
  containing `.` matches the full path. Bare queries must never match paths —
  the chunk keyword prefixes every path, so path matching would make a query
  like `prompt` hit every row. The left-panel tree stays visible during
  search, pruned via `subtree_matches` (same predicate, pre-flatten).
- **JSON order**: serde_json's `preserve_order` feature is required — the tree
  and match ordering must reflect file order, not alphabetical.
- **Lenient JSON**: ComfyUI (Python `json.dumps`) emits bare `NaN`/`Infinity`.
  `replace_nonfinite_tokens` (string-literal-aware) maps them to `null`, and
  only runs when strict parsing fails. Don't "simplify" it into a regex — it
  must not touch string contents.
- **Pins persistence**: stored under a versioned key (`pins_v4`, a
  `Vec<Pin { pattern, label, mode }>`; `pins_v3` stored bare pattern strings
  and is migrated on load; `mode` is `#[serde(default)]` so pre-mode v4 data
  still loads). Changing the default pin set or a field's meaning requires a
  version bump; purely additive optional fields may use `serde(default)`
  instead. Settings live under `settings_v1` with struct-level
  `serde(default)` for the same reason.
- **Cache correctness**: `ImageEntry::from_parts` is the single construction
  path — a cache hit and a disk read must produce byte-identical rows. The
  cache stores *raw* chunk text, so changes to flattening/row-building do NOT
  require a cache bump (rows are rebuilt on load); bump `CACHE_VERSION` only
  when the `CachedFile` schema or chunk *decoding* changes. A hit requires
  name + size + mtime (whole seconds) all to match; unknown mtime never hits.
- **Scan generations**: every `ScanUpdate` carries its generation; the app
  drops messages that don't match the current `ScanState`. A superseded
  worker stops at its next emit and must not write its (incomplete) cache.
- **Find vs search**: the metadata-panel find box never filters — it computes
  a one-frame `jump` path, force-opens every header on the path to it or under
  it (`jump_opens`), and scrolls there; a match inside `_meta` retargets to
  the owning node so the whole node opens. Enter cycles matches. The global
  search box stays a pure filter.

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
