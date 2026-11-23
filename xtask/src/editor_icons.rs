// SPDX-License-Identifier: MPL-2.0
//! `editor-icons`: the Regia Editor's app icons, drawn from one SVG (ADR-20000).
//!
//! ```text
//! cargo xtask editor-icons            # (re)write the icon set beside the source
//! cargo xtask editor-icons --check    # exit 1 when a committed file differs from a fresh draw
//! ```
//!
//! The source is `crates/regia_editor/assets/icon/regia-editor.svg`, a copy of the brand master
//! logomark on its Ink tile. The command draws it with `resvg` (no text, fonts or raster images)
//! and writes, next to it:
//!
//! - `regia-editor-<N>.png` for N in [`PNG_SIZES`] (the Linux hicolor sizes, the editor's window
//!   icon, the Windows shortcut fallback);
//! - `regia-editor.ico`: [`ICO_SIZES`], 32-bit DIB entries below 256 and a PNG-compressed 256
//!   (Microsoft's ICONDIR layout);
//! - `regia-editor.icns`: the macOS icon family ([`ICNS_ENTRIES`], PNG payloads). macOS 13 does not
//!   mask an `.icns`, so these images put the tile on [`MAC_TILE`] of the canvas, centered, the
//!   margin Finder and the Dock give every other app; the other platforms draw the tile full bleed.
//!
//! It also refreshes the copy `regia` embeds for `regia editor install-launcher`
//! ([`CLI_ICON_DIR`]: the SVG and every file but the 1024-pixel PNG), because a crate compiles
//! from its own folder (ADR-1485).
//!
//! Swapping the mark (a per-app Regia mark later) is: replace the SVG, run this command, commit.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use resvg::{tiny_skia, usvg};

/// The PNG sizes written beside the source, smallest first.
pub const PNG_SIZES: [u32; 9] = [16, 24, 32, 48, 64, 128, 256, 512, 1024];

/// The sizes inside the Windows `.ico`, smallest first (256 is stored PNG-compressed).
pub const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// The `.icns` entries: OSType and pixel side (`ic11`..`ic14` and `ic10` are the @2x slots).
pub const ICNS_ENTRIES: [(&[u8; 4], u32); 10] = [
    (b"icp4", 16),
    (b"ic11", 32),
    (b"icp5", 32),
    (b"ic12", 64),
    (b"ic07", 128),
    (b"ic13", 256),
    (b"ic08", 256),
    (b"ic14", 512),
    (b"ic09", 512),
    (b"ic10", 1024),
];

/// The share of the macOS canvas the tile covers (824 of 1024, the margin of the macOS icon grid).
pub const MAC_TILE: f32 = 824.0 / 1024.0;

/// The icon directory, relative to the workspace root.
pub const ICON_DIR: &str = "crates/regia_editor/assets/icon";

/// The source file name inside [`ICON_DIR`].
pub const SOURCE: &str = "regia-editor.svg";

/// Command-line options of `cargo xtask editor-icons`.
#[derive(clap::Args)]
pub struct Args {
    /// Draw the set in memory and compare it with the committed files instead of writing them.
    #[arg(long)]
    pub check: bool,
    /// The source SVG (default: `crates/regia_editor/assets/icon/regia-editor.svg`).
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// The output directory (default: the source's directory).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// An 8-bit straight-alpha RGBA image, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    /// Side in pixels (the icons are square).
    pub side: u32,
    /// `side * side * 4` bytes.
    pub pixels: Vec<u8>,
}

/// The copy `regia editor install-launcher` embeds, relative to the workspace root: a crate
/// compiles from its own folder (ADR-1485), so `regia_cli` cannot read [`ICON_DIR`].
pub const CLI_ICON_DIR: &str = "crates/regia_cli/assets/editor-icon";

/// Whether the CLI's copy holds `name` (everything but the 1024-pixel PNG, plus the source SVG).
#[must_use]
pub fn in_cli_copy(name: &str) -> bool {
    name != "regia-editor-1024.png"
}

/// Runs the command.
///
/// # Errors
///
/// The source does not parse, or a file cannot be read or written.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let source = args
        .source
        .clone()
        .unwrap_or_else(|| root.join(ICON_DIR).join(SOURCE));
    let out = match &args.out {
        Some(dir) => dir.clone(),
        None => source
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("{} has no parent directory", source.display()))?,
    };
    let svg = std::fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
    let files = icon_set(&svg)?;
    // The default run also refreshes the CLI's copy (the source SVG included).
    let cli_copy = args.out.is_none().then(|| {
        let mut copy: Vec<(String, Vec<u8>)> =
            files.iter().filter(|(n, _)| in_cli_copy(n)).cloned().collect();
        copy.push((SOURCE.to_owned(), svg));
        (root.join(CLI_ICON_DIR), copy)
    });
    let mut targets = vec![(out, files)];
    targets.extend(cli_copy);
    if args.check {
        let mut stale = Vec::new();
        for (dir, files) in &targets {
            for (name, bytes) in files {
                let path = dir.join(name);
                if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                    stale.push(path.display().to_string());
                }
            }
        }
        if stale.is_empty() {
            let count: usize = targets.iter().map(|(_, f)| f.len()).sum();
            println!("editor-icons: {count} files up to date");
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!(
            "editor-icons: stale or missing (run `cargo xtask editor-icons`): {}",
            stale.join(", ")
        );
        return Ok(ExitCode::FAILURE);
    }
    for (dir, files) in &targets {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        for (name, bytes) in files {
            let path = dir.join(name);
            let tmp = dir.join(format!(".{name}.tmp"));
            std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
            println!("{} ({} bytes)", path.display(), bytes.len());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Every generated file of the set: (file name, bytes), in a fixed order.
///
/// # Errors
///
/// The SVG does not parse, or an encoder fails.
pub fn icon_set(svg: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let tree = parse(svg)?;
    let mut files = Vec::new();
    let mut full = std::collections::BTreeMap::new();
    for side in PNG_SIZES {
        let image = draw(&tree, side, 1.0)?;
        files.push((format!("regia-editor-{side}.png"), encode_png(&image)?));
        full.insert(side, image);
    }
    let ico_images: Vec<&Rgba> = ICO_SIZES.iter().filter_map(|side| full.get(side)).collect();
    files.push(("regia-editor.ico".to_owned(), ico(&ico_images)?));
    let mut mac = Vec::new();
    for (kind, side) in ICNS_ENTRIES {
        mac.push((*kind, encode_png(&draw(&tree, side, MAC_TILE)?)?));
    }
    files.push(("regia-editor.icns".to_owned(), icns(&mac)?));
    Ok(files)
}

/// Parses the SVG with no font database and no resource directory.
fn parse(svg: &[u8]) -> Result<usvg::Tree> {
    usvg::Tree::from_data(svg, &usvg::Options::default()).map_err(|e| anyhow!("parsing the icon SVG: {e}"))
}

/// Draws `tree` on a transparent `side` x `side` canvas: scaled to `tile` of the side, centered.
fn draw(tree: &usvg::Tree, side: u32, tile: f32) -> Result<Rgba> {
    let mut pixmap = tiny_skia::Pixmap::new(side, side).ok_or_else(|| anyhow!("bad icon side {side}"))?;
    let size = tree.size();
    let extent = size.width().max(size.height());
    if extent <= 0.0 {
        bail!("the icon SVG has no size");
    }
    let scale = side as f32 * tile / extent;
    let offset = side as f32 * (1.0 - tile) / 2.0;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(offset, offset);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    let mut pixels = Vec::with_capacity(side as usize * side as usize * 4);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Ok(Rgba { side, pixels })
}

/// Encodes `image` as an 8-bit RGBA PNG.
///
/// # Errors
///
/// The encoder fails.
pub fn encode_png(image: &Rgba) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.side, image.side);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.pixels)?;
    writer.finish()?;
    Ok(out)
}

/// The 32-bit DIB of an ICO entry: a `BITMAPINFOHEADER` with the doubled height the format asks
/// for, the BGRA rows bottom up, then the 1-bit AND mask (set where the pixel is fully transparent),
/// each mask row padded to 32 bits.
fn ico_dib(image: &Rgba) -> Vec<u8> {
    let side = image.side as usize;
    let mask_row = side.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + side * side * 4 + mask_row * side);
    let image_bytes = (side * side * 4 + mask_row * side) as u32;
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(image.side as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(2 * image.side as i32).to_le_bytes()); // biHeight: XOR + AND
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression: BI_RGB
    out.extend_from_slice(&image_bytes.to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0; 16]); // pixels per meter x/y, colors used, important
    let rows: Vec<&[u8]> = image.pixels.chunks(side * 4).collect();
    for row in rows.iter().rev() {
        for p in row.as_chunks::<4>().0 {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    for row in rows.iter().rev() {
        let mut bits = vec![0u8; mask_row];
        for (x, p) in row.as_chunks::<4>().0.iter().enumerate() {
            if p[3] == 0 {
                bits[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&bits);
    }
    out
}

/// The `.ico` of `images` (smallest first): a 6-byte ICONDIR, a 16-byte entry per image, then the
/// images. A 256-pixel image is a PNG stream (Windows reads those since Vista), smaller ones DIBs.
///
/// # Errors
///
/// An image is larger than 256 pixels, or the PNG encoder fails.
pub fn ico(images: &[&Rgba]) -> Result<Vec<u8>> {
    let mut blobs = Vec::with_capacity(images.len());
    for image in images {
        let blob = match image.side {
            256 => encode_png(image)?,
            1..256 => ico_dib(image),
            other => bail!("an .ico entry is at most 256 pixels, not {other}"),
        };
        blobs.push((image.side, blob));
    }
    let count = u16::try_from(blobs.len()).context("too many icon sizes")?;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type 1: icon
    out.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6 + 16 * blobs.len();
    for (side, blob) in &blobs {
        // A side of 256 is stored as 0.
        let byte = u8::try_from(*side).unwrap_or(0);
        out.extend_from_slice(&[byte, byte, 0, 0]); // width, height, palette colors, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&u32::try_from(blob.len())?.to_le_bytes());
        out.extend_from_slice(&u32::try_from(offset)?.to_le_bytes());
        offset += blob.len();
    }
    for (_, blob) in &blobs {
        out.extend_from_slice(blob);
    }
    Ok(out)
}

/// The `.icns` of `entries` (OSType, PNG bytes): the `icns` magic and the big-endian file length,
/// then per entry its OSType, the big-endian length of type + length + data, and the data.
///
/// # Errors
///
/// The file would pass 4 GiB.
pub fn icns(entries: &[([u8; 4], Vec<u8>)]) -> Result<Vec<u8>> {
    let body: usize = entries.iter().map(|(_, data)| 8 + data.len()).sum();
    let mut out = Vec::with_capacity(8 + body);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&u32::try_from(8 + body)?.to_be_bytes());
    for (kind, data) in entries {
        out.extend_from_slice(kind);
        out.extend_from_slice(&u32::try_from(8 + data.len())?.to_be_bytes());
        out.extend_from_slice(data);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &[u8] = include_bytes!("../../crates/regia_editor/assets/icon/regia-editor.svg");

    fn solid(side: u32, rgba: [u8; 4]) -> Rgba {
        Rgba {
            side,
            pixels: rgba.repeat(side as usize * side as usize),
        }
    }

    fn le16(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn le32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    #[test]
    fn ico_directory_lists_each_size_and_stores_256_as_png() {
        let images: Vec<Rgba> = [16, 48, 256].iter().map(|&s| solid(s, [1, 2, 3, 255])).collect();
        let refs: Vec<&Rgba> = images.iter().collect();
        let bytes = ico(&refs).expect("ico");
        assert_eq!(le16(&bytes, 2), 1, "type icon");
        assert_eq!(le16(&bytes, 4), 3, "count");
        let mut sides = Vec::new();
        for i in 0..3 {
            let entry = 6 + 16 * i;
            let (len, at) = (
                le32(&bytes, entry + 8) as usize,
                le32(&bytes, entry + 12) as usize,
            );
            let data = &bytes[at..at + len];
            sides.push(bytes[entry]);
            if bytes[entry] == 0 {
                assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n", "256 is a PNG");
            } else {
                assert_eq!(le32(data, 0), 40, "DIB header");
                assert_eq!(le32(data, 8), 2 * u32::from(bytes[entry]), "doubled height");
                // BGRA bottom-up: blue first.
                assert_eq!(&data[40..44], &[3, 2, 1, 255]);
            }
        }
        assert_eq!(sides, [16, 48, 0]);
        assert!(ico(&[&solid(512, [0; 4])]).is_err(), "512 does not fit an .ico");
    }

    #[test]
    fn ico_mask_marks_transparent_pixels() {
        let mut image = solid(16, [9, 9, 9, 255]);
        image.pixels[3] = 0; // the top-left pixel: the last DIB row, first bit
        let dib = ico_dib(&image);
        let mask = &dib[40 + 16 * 16 * 4..];
        assert_eq!(mask.len(), 16 * 4, "rows padded to 32 bits");
        assert_eq!(mask[15 * 4], 0x80, "top row is the last mask row");
        assert!(mask[..15 * 4].iter().all(|&b| b == 0));
    }

    #[test]
    fn icns_layout_is_big_endian_and_sums() {
        let bytes = icns(&[(*b"ic07", vec![7; 10]), (*b"ic10", vec![1; 3])]).expect("icns");
        assert_eq!(&bytes[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(bytes[4..8].try_into().expect("4")) as usize,
            bytes.len()
        );
        assert_eq!(&bytes[8..12], b"ic07");
        assert_eq!(u32::from_be_bytes(bytes[12..16].try_into().expect("4")), 18);
        assert_eq!(&bytes[26..30], b"ic10");
    }

    #[test]
    fn the_source_draws_a_white_mark_on_an_ink_tile() {
        let tree = parse(SVG).expect("parse");
        let image = draw(&tree, 108, 1.0).expect("draw");
        let px = |x: usize, y: usize| &image.pixels[(y * 108 + x) * 4..(y * 108 + x) * 4 + 4];
        assert_eq!(px(0, 0)[3], 0, "the rounded corner is transparent");
        assert_eq!(px(54, 4), &[0, 0, 0, 255], "Ink tile");
        assert_eq!(px(54, 40), &[255, 255, 255, 255], "the top X crosses at (54, 40)");
        assert_eq!(px(54, 88), &[255, 255, 255, 255], "underline");
        let mac = draw(&tree, 108, MAC_TILE).expect("draw");
        assert_eq!(
            mac.pixels[(54 * 108 + 4) * 4 + 3],
            0,
            "the macOS tile has a margin"
        );
    }

    #[test]
    fn icon_set_has_every_file() {
        let files = icon_set(SVG).expect("set");
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.len(), PNG_SIZES.len() + 2);
        assert!(names.contains(&"regia-editor-1024.png"));
        assert!(names.contains(&"regia-editor.ico"));
        assert!(names.contains(&"regia-editor.icns"));
    }
}
