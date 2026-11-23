# Regia Editor app icon

`regia-editor.svg` is the one source of the Regia Editor's app icons (ADR-20000). Every other file
here is drawn from it by `cargo xtask editor-icons`; do not edit them by hand.

| File | Used by |
|---|---|
| `regia-editor-<N>.png` (16, 24, 32, 48, 64, 128, 256, 512, 1024) | The Linux hicolor icons (`regia editor install-launcher`), the editor's window icon (128) and Windows taskbar icon (256). |
| `regia-editor.ico` (16 to 256; 256 PNG-compressed) | Embedded into `regia-editor.exe` by `build.rs`; the Windows Start Menu shortcut. |
| `regia-editor.icns` (16 to 1024) | The macOS app bundle `~/Applications/Regia Editor.app`. |

The same command writes a byte-identical copy of the SVG and every file but the 1024 PNG to
`crates/regia_cli/assets/editor-icon/`, which `regia editor install-launcher` embeds (a crate compiles
from its own folder, ADR-1485; a `regia_cli` test checks the copies match).

## Provenance

The SVG is a copy of the PiercingXX brand master logomark
(`piercingxx-branding/branding/assets/logomark.svg`, BRAND-GUIDE.md § 5 "The logomark" and the
new-project checklist item 6): the underlined-XX mark, Signal white on a rounded-square Ink tile
(corner radius about 20 % of the tile), no glow, shadow or recolor. The brand repository is the
master and is read-only from here.

## Swapping the mark

To give the editor its own mark (a per-app mark in the house glyph language, BRAND-GUIDE.md § 5
"Per-app marks"): replace `regia-editor.svg` (108 viewport, the full tile drawn in it), run
`cargo xtask editor-icons`, look at the PNGs, commit them all. `cargo xtask editor-icons --check`
fails while the drawn files are stale.
