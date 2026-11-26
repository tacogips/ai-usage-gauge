# Plan rust-08-tray-icon-renderer: Pure-Rust 2x Retina macOS Template Tray Status Icon Rasterizer

- **planId**: `rust-08-tray-icon-renderer`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `src-tauri/assets`
  - `src-tauri/src/tray_icons.rs`
  - `src-tauri/tests/tray_icons_tests.rs`
- **sharedPaths**: `[]`
- **artifactRoots**: `[]`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/design-subscription-rate-limits.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Extract the embedded Claude (`72x72`) and ChatGPT/Codex (`36x36`) template PNG assets from Swift into `src-tauri/assets/` and implement the pure-Rust 2x Retina RGBA8 template icon strip rasterizer in `src-tauri/src/tray_icons.rs` with area-proportional bottom-to-top fill geometry (so narrow star tips visibly fill at small percentages), budget pie icon + warning triangle, inline monospaced ratio percentage glyphs, and anti-flicker change-detection keys.

Existing Swift references to imitate:
- `Sources/CCUsageGaugeMenuBar/MenuBarProviderIcons.swift`: exact base64 payloads for `claudeTemplate` (`72x72` PNG) and `chatGPTTemplate` (`36x36` PNG).
- `Sources/CCUsageGaugeMenuBar/MenuBarUsageIcon.swift`: crop to opaque bounding box (`alpha > 5`), scale to `36x36`, morphological minimum erosion mask (radius `max(1, width / 18 * 0.5)` matching `CIMorphologyMinimum`) separating `outline = solid - interior` and `interior`, bottom-to-top `cumulative_interior_by_row` CDF, `fill_height_fraction(f)` area-proportional fill, Gemini 4-point cubic Bezier star `(18,35)->(35,18)->(18,1)->(1,18)->(18,35)`, unavailable diagonal slash `(2pt, 2pt)` to `(16pt, 16pt)`.
- `Sources/CCUsageGaugeMenuBar/MenuBarPieIcon.swift`: `36x36` with `2pt` (`4px`) inset, warning triangle + exclamation mark, `0.35`-alpha circular rail + clockwise wedge from 12 o'clock, 90-degree unknown wedge when budget configured without snapshot, vertical center bar when budget unset.
- `Sources/CCUsageGaugeMenuBar/MenuBarPieStrip.swift` & `MenuBarStatusImage.swift`: canonical `PieChartId` order, `18pt x 18pt` (`36x36px`) icons, optional budget-icon omission when only budget ratio text is enabled, `3pt` (`6px`) icon-to-ratio gap, `7pt` (`14px`) inter-entry gap.

## Non-Goals

- Do NOT wire Tauri app startup, menus, or IPC here (handled in `rust-12-tauri-tray-app`).
- Do NOT link AppKit/CoreImage or invoke Swift.
- Do NOT touch `src-tauri/Cargo.toml`, `src-tauri/src/lib.rs`, or `src-tauri/src/main.rs`.

## Target Files, Symbols, and Contracts

1. **`src-tauri/assets/claude-template.png` & `src-tauri/assets/chatgpt-template.png`**:
   - Decoded binary PNG files extracted verbatim from `Sources/CCUsageGaugeMenuBar/MenuBarProviderIcons.swift` (verified `72x72` and `36x36` RGBA PNGs) embedded via `include_bytes!("../assets/...")`.
2. **`src-tauri/src/tray_icons.rs`**:
   - `pub struct RenderedTrayIcon { pub rgba: Vec<u8>, pub width: u32, pub height: u32 }`.
   - `pub struct TrayRenderState { pub entries: Vec<MenuBarStatusEntry>, pub is_usage_unavailable: bool, pub has_warnings: bool }` deriving `Clone, PartialEq, Eq` for anti-flicker dedup.
   - `pub struct TrayIconRenderer` (precomputing once at startup via `OnceLock` or instance fields the cropped/scaled `36x36` `solid`, `interior`, `outline`, and `cumulative_interior_by_row` CDF masks for `Claude`, `Codex`, and `Gemini`).
   - `TrayIconRenderer::render_strip(&self, state: &TrayRenderState) -> Option<RenderedTrayIcon>`:
     - Height fixed at `36px` (`18pt @ 2x`).
     - Template RGBA convention: RGB is black `(0, 0, 0)` everywhere; alpha channel carries opacity (0..=255) for `TrayIcon::set_icon_with_as_template(Some(image), true)`.
     - Area-proportional `fill_height_fraction(fraction: f64) -> f64` mapping `f` clamped to `[0.0, 1.0]` via `cumulative_interior_by_row` so `f = 0.05` visibly fills the narrow bottom tip of Claude/Gemini stars.
     - Budget pie icon (`tiny-skia`): warning triangle when unavailable/warnings, else `0.35`-alpha rail + clockwise wedge from 12 o'clock / 90-degree unknown wedge / vertical bar.
     - Inline monospaced ratio labels (`"0%"`..`"100%"`, `"--"`): crisp 2x anti-aliased glyph rasterizer in pure Rust (zero system-font platform nondeterminism on Linux/macOS tests).
   - `pub struct TrayPresentationDedup`: caches `(applied_icon_state, applied_title, applied_tooltip)` and returns `bool` indicating whether each surface actually changed.

## Key Invariants and Pitfalls

- **Premultiplied vs straight RGBA**: `tauri::image::Image::new_owned(rgba, width, height)` expects **straight (non-premultiplied)** RGBA8 bytes, whereas `tiny-skia::Pixmap` stores premultiplied RGBA; convert cleanly and keep RGB `(0, 0, 0)` with exact alpha!
- **Empty strip handling**: When `menuBarCostOnly` / no icons and no ratio labels are active, `render_strip` returns `None` cleanly (zero panic on `width == 0`).
- **Narrow tip fill visibility**: Verify mathematically in tests that at `fraction = 0.05`, `fill_height_fraction` is substantially greater than `0.05` for the Gemini 4-point star because the bottom tip has small horizontal cross-section!

## Test Cases (`src-tauri/tests/tray_icons_tests.rs`)

- Decode embedded `claude-template.png` (`72x72`) and `chatgpt-template.png` (`36x36`) -> non-empty solid/outline/interior masks and monotonic `cumulative_interior_by_row`.
- Gemini star area-proportional fill -> `fill_height_fraction(0.05) > 0.15`, `fill_height_fraction(0.0) == 0.0`, `fill_height_fraction(1.0) == 1.0`; rendered pixel count increases monotonically with `f` and stays strictly RGB `(0, 0, 0)`.
- Unavailable provider (`None`) -> draws outline + diagonal slash distinct from `Some(0.0)`.
- Budget pie states -> normal fraction (`0.0`, `0.5`, `1.0`), unknown budget (`budget configured, no snapshot`), unset budget (vertical bar), and warning state (`is_usage_unavailable` or `has_warnings`) each produce distinct non-empty `36x36` RGBA images.
- Multi-entry strip with inline ratio labels (`"45%"`, `"--"`) and budget-text-without-icon mode -> exact expected strip width (`36px` height, `3pt`/`7pt` gaps) and non-zero alpha in glyph regions.
- `TrayPresentationDedup` -> identical state skips update (`false`), changed icon/title/tooltip triggers update (`true`) independently.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check src-tauri/src/tray_icons.rs src-tauri/tests/tray_icons_tests.rs
cargo test --locked -p ccusage-gauge-app --test tray_icons_tests
```

Done when both commands exit `0` and all rasterization, area-proportional fill, budget pie, ratio-label, and dedup tests pass. (Crate-wide `cargo clippy` runs in serial Wave 6 join plan `rust-13`.)

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
