// SPDX-License-Identifier: MIT-0
//! Windows resources of this game's executable: the icon Explorer and the taskbar show, and the
//! version info of the exe's Properties > Details tab (written by `regia new` and
//! `regia upgrade-project`, ADR-2645).
//!
//! Only a Windows target embeds anything. The script reads `Regia.toml` (`[project] name`,
//! `version`, `organization`; `[platforms.windows] icon`, else `assets/branding/icon.png` or
//! `assets/icon.png`), turns the icon PNG into a multi-size ICO (16, 32, 48 and 256 pixels,
//! PNG-compressed entries) in `OUT_DIR`, and hands both to `winresource`. A project with no icon
//! gets a placeholder (a release build warns). With no resource compiler (`rc.exe` from the
//! Windows SDK, or `llvm-rc` when cross-compiling) the script prints a cargo warning and the build
//! goes on without resources; `regia package --platform windows` then reports what is missing.
//!
//! It also gives the exe's main thread an 8 MiB stack, the size Linux gives it. Windows reserves
//! 1 MiB unless the exe asks for more, and a debug build of a template game overflows that while
//! it starts (seen on Windows 11, MSVC, 2026-11-10).
//!
//! Its helpers are `pub(crate)` only so the engine's own tests can reach them.
//!
//! This file is yours: `regia upgrade-project` adds it only where there is no build script and
//! never rewrites one you changed.

use std::path::{Path, PathBuf};

/// The icon sizes written into the ICO, smallest first (Windows picks the nearest per DPI).
pub(crate) const ICON_SIZES: [u32; 4] = [16, 32, 48, 256];

/// The largest icon PNG side the script decodes.
pub(crate) const MAX_ICON_SIDE: u32 = 4096;

/// What a larger icon is pre-resampled to before the icon sizes are made: the largest size is
/// 256, so a 1024 source loses nothing, and the premultiplied f32 copy stays at 16 MB instead
/// of ~270 MB for a 4096 icon in a build script.
pub(crate) const ICON_SOURCE_CAP: u32 = 1024;

/// The side `ico_from_rgba` resamples the fitted icon from: at most [`ICON_SOURCE_CAP`].
pub(crate) fn icon_source_side(width: u32) -> u32 {
    width.min(ICON_SOURCE_CAP)
}

/// The main thread's stack reserve in bytes (the Linux default; Windows' own default is 1 MiB).
pub(crate) const MAIN_STACK_BYTES: u32 = 8 << 20;

/// The project facts the resources carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectInfo {
    /// `ProductName` and `FileDescription`.
    pub(crate) name: String,
    /// `FileVersion` and `ProductVersion` (also the fixed binary version).
    pub(crate) version: String,
    /// `CompanyName`; empty leaves it out.
    pub(crate) organization: String,
    /// `[platforms.windows] icon`, relative to the project root.
    pub(crate) icon: Option<String>,
}

/// An 8-bit RGBA image, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

#[cfg(not(test))]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if let Some(arg) = main_stack_link_arg(&target_env) {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    if let Err(err) = embed(&manifest_dir) {
        println!(
            "cargo:warning=Windows resources not embedded ({err}): the exe has no icon and no \
             version info; `regia package --platform windows` reports it"
        );
    }
}

/// Writes the ICO and the version info and runs the resource compiler.
#[cfg(not(test))]
fn embed(manifest_dir: &Path) -> Result<(), String> {
    let env = |key: &str| std::env::var(key).unwrap_or_default();
    let project_file = find_project_manifest(manifest_dir);
    let text = match &project_file {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            std::fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))?
        }
        None => String::new(),
    };
    let info = ProjectInfo::read(&text, &env("CARGO_PKG_NAME"), &env("CARGO_PKG_VERSION"))?;
    let root = project_file
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(manifest_dir);
    let release = env("PROFILE") == "release";
    // A branding icon added later reruns the script (the directory, not all of `assets/`).
    let branding = root.join("assets/branding");
    if branding.is_dir() {
        println!("cargo:rerun-if-changed={}", branding.display());
    }

    let image = match icon_path(root, info.icon.as_deref()) {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            match std::fs::read(&path)
                .map_err(|err| err.to_string())
                .and_then(|bytes| decode_png(&bytes))
            {
                Ok(image) => image,
                Err(err) => {
                    println!(
                        "cargo:warning=icon {} not used ({err}); the exe gets the placeholder icon",
                        path.display()
                    );
                    placeholder_icon()
                }
            }
        }
        None => {
            if release {
                println!(
                    "cargo:warning=no project icon (set [platforms.windows] icon in Regia.toml to a \
                     square PNG, 256 pixels or larger); the exe gets the placeholder icon"
                );
            }
            placeholder_icon()
        }
    };
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let ico_path = out_dir.join("regia_app.ico");
    std::fs::write(&ico_path, ico_from_rgba(&image)?)
        .map_err(|err| format!("writing {}: {err}", ico_path.display()))?;
    let ico = ico_path.to_str().ok_or("OUT_DIR is not valid UTF-8")?;

    let words = version_words(&info.version);
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico)
        .set("ProductName", &info.name)
        .set("FileDescription", &info.name)
        .set("FileVersion", &info.version)
        .set("ProductVersion", &info.version)
        .set_version_info(winresource::VersionInfo::FILEVERSION, words)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, words);
    if !info.organization.is_empty() {
        res.set("CompanyName", &info.organization);
    }
    res.compile().map_err(|err| format!("resource compiler: {err}"))
}

/// The linker argument that sets the exe's main-thread stack reserve to [`MAIN_STACK_BYTES`]:
/// `/STACK` for the MSVC linker, `--stack` for MinGW `ld` and `lld`. Another toolchain gets none.
pub(crate) fn main_stack_link_arg(target_env: &str) -> Option<String> {
    match target_env {
        "msvc" => Some(format!("/STACK:{MAIN_STACK_BYTES}")),
        "gnu" => Some(format!("-Wl,--stack,{MAIN_STACK_BYTES}")),
        _ => None,
    }
}

/// The project manifest next to the crate (the quick template) or one level up (a workspace member).
pub(crate) fn find_project_manifest(manifest_dir: &Path) -> Option<PathBuf> {
    let parent = manifest_dir.parent();
    [Some(manifest_dir), parent]
        .into_iter()
        .flatten()
        .flat_map(|dir| [dir.join("Regia.toml"), dir.join("Ferrite.toml")])
        .find(|path| path.is_file())
}

/// The configured icon (relative to the project root), else the conventional locations.
pub(crate) fn icon_path(root: &Path, configured: Option<&str>) -> Option<PathBuf> {
    if let Some(rel) = configured {
        return Some(root.join(rel));
    }
    ["assets/branding/icon.png", "assets/icon.png"]
        .into_iter()
        .map(|rel| root.join(rel))
        .find(|path| path.is_file())
}

impl ProjectInfo {
    /// Reads `[project]` and `[platforms.windows]` of a `Regia.toml`; a missing field takes the
    /// cargo package's name or version.
    pub(crate) fn read(text: &str, package_name: &str, package_version: &str) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|err| format!("Regia.toml: {err}"))?;
        let project = table.get("project").and_then(toml::Value::as_table);
        let field = |key: &str| {
            project
                .and_then(|p| p.get(key))
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let icon = table
            .get("platforms")
            .and_then(|p| p.get("windows"))
            .and_then(|w| w.get("icon"))
            .and_then(toml::Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned);
        Ok(Self {
            name: field("name").unwrap_or_else(|| package_name.to_owned()),
            version: field("version").unwrap_or_else(|| package_version.to_owned()),
            organization: field("organization").unwrap_or_default(),
            icon,
        })
    }
}

/// `major.minor.patch[.build]` packed as the 64-bit `VS_FIXEDFILEINFO` version (16 bits each,
/// most significant first). A pre-release or build suffix (`-beta`, `+git`) is dropped; a part
/// that is not a number, or does not fit 16 bits, is 0.
pub(crate) fn version_words(version: &str) -> u64 {
    let core = version.split(['-', '+']).next().unwrap_or_default();
    core.trim()
        .split('.')
        .chain(std::iter::repeat("0"))
        .take(4)
        .fold(0u64, |packed, part| {
            (packed << 16) | u64::from(part.trim().parse::<u16>().unwrap_or(0))
        })
}

/// Decodes a PNG of any colour type and depth to 8-bit RGBA.
pub(crate) fn decode_png(bytes: &[u8]) -> Result<Rgba, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|err| format!("not a PNG: {err}"))?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width > MAX_ICON_SIDE || height > MAX_ICON_SIDE {
        return Err(format!(
            "the icon is {width}x{height}; use a PNG of at most {MAX_ICON_SIDE} pixels a side"
        ));
    }
    let size = reader.output_buffer_size().ok_or("the icon PNG is too large")?;
    let mut buffer = vec![0; size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|err| format!("decoding the icon: {err}"))?;
    let data = buffer.get(..frame.buffer_size()).ok_or("short PNG frame")?;
    let channels = frame.color_type.samples();
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for p in data.chunks_exact(channels) {
        let rgba = match *p {
            [g] => [g, g, g, 255],
            [g, a] => [g, g, g, a],
            [r, g, b] => [r, g, b, 255],
            [r, g, b, a] => [r, g, b, a],
            _ => return Err(format!("unexpected PNG colour type {:?}", frame.color_type)),
        };
        pixels.extend_from_slice(&rgba);
    }
    if pixels.len() != width as usize * height as usize * 4 {
        return Err("the icon PNG has a short image".to_owned());
    }
    Ok(Rgba {
        width,
        height,
        pixels,
    })
}

/// Encodes 8-bit RGBA as a PNG.
pub(crate) fn encode_png(image: &Rgba) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|err| err.to_string())?;
    writer
        .write_image_data(&image.pixels)
        .map_err(|err| err.to_string())?;
    writer.finish().map_err(|err| err.to_string())?;
    Ok(out)
}

/// The ICO file of `image` (Microsoft's ICONDIR layout: a 6-byte header, one 16-byte entry per
/// size, then each image; every image is a PNG stream, which Windows reads since Vista). A
/// non-square image is centred on a transparent square first.
pub(crate) fn ico_from_rgba(image: &Rgba) -> Result<Vec<u8>, String> {
    let square = fit_square(image);
    // Pre-resample a large source once so each size pass works from at most 1024 px (see
    // `ICON_SOURCE_CAP`): memory stays at build-script scale.
    let square = if square.width > ICON_SOURCE_CAP {
        resize(&square, icon_source_side(square.width))
    } else {
        square
    };
    let mut images = Vec::with_capacity(ICON_SIZES.len());
    for size in ICON_SIZES {
        images.push((size, encode_png(&resize(&square, size))?));
    }
    let count = u16::try_from(images.len()).map_err(|_| "too many icon sizes")?;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type 1: icon
    out.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for (size, png) in &images {
        // A side of 256 is stored as 0.
        let side = u8::try_from(*size).unwrap_or(0);
        out.extend_from_slice(&[side, side, 0, 0]); // width, height, palette colours, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        let length = u32::try_from(png.len()).map_err(|_| "icon image too large")?;
        let at = u32::try_from(offset).map_err(|_| "icon file too large")?;
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        offset += png.len();
    }
    for (_, png) in &images {
        out.extend_from_slice(png);
    }
    Ok(out)
}

/// `image` centred on a transparent square of its longer side.
pub(crate) fn fit_square(image: &Rgba) -> Rgba {
    let side = image.width.max(image.height);
    if image.width == image.height {
        return image.clone();
    }
    let mut pixels = vec![0u8; side as usize * side as usize * 4];
    let (dx, dy) = ((side - image.width) / 2, (side - image.height) / 2);
    let row = image.width as usize * 4;
    for (y, src) in image.pixels.chunks_exact(row).enumerate() {
        let at = ((y + dy as usize) * side as usize + dx as usize) * 4;
        pixels[at..at + row].copy_from_slice(src);
    }
    Rgba {
        width: side,
        height: side,
        pixels,
    }
}

/// The source pixels and weights of each destination pixel along one axis: the covered area
/// when shrinking, linear interpolation between the two nearest centres when growing.
pub(crate) fn axis_weights(from: u32, to: u32) -> Vec<Vec<(usize, f32)>> {
    let scale = f64::from(from) / f64::from(to);
    (0..to)
        .map(|d| {
            if from >= to {
                let (start, end) = (f64::from(d) * scale, f64::from(d + 1) * scale);
                let mut taps = Vec::new();
                let mut s = start.floor();
                while s < end {
                    let cover = (s + 1.0).min(end) - s.max(start);
                    if cover > 0.0 {
                        taps.push((s as usize, (cover / scale) as f32));
                    }
                    s += 1.0;
                }
                taps
            } else {
                let centre = ((f64::from(d) + 0.5) * scale - 0.5).clamp(0.0, f64::from(from - 1));
                let left = centre.floor();
                let t = (centre - left) as f32;
                let right = (left as usize + 1).min(from as usize - 1);
                vec![(left as usize, 1.0 - t), (right, t)]
            }
        })
        .collect()
}

/// A square `image` resampled to `size` x `size` in premultiplied alpha (so transparent pixels
/// add no colour fringe).
pub(crate) fn resize(image: &Rgba, size: u32) -> Rgba {
    if image.width == size {
        return image.clone();
    }
    let side = image.width as usize;
    let premultiplied: Vec<[f32; 4]> = image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let a = f32::from(p[3]) / 255.0;
            [f32::from(p[0]) * a, f32::from(p[1]) * a, f32::from(p[2]) * a, a]
        })
        .collect();
    let weights = axis_weights(image.width, size);
    let n = size as usize;
    // Rows first (source height x destination width), then columns.
    let mut rows = vec![[0f32; 4]; side * n];
    for y in 0..side {
        for (x, taps) in weights.iter().enumerate() {
            let mut sum = [0f32; 4];
            for &(s, w) in taps {
                let p = premultiplied[y * side + s];
                for (acc, v) in sum.iter_mut().zip(p) {
                    *acc += v * w;
                }
            }
            rows[y * n + x] = sum;
        }
    }
    let mut pixels = Vec::with_capacity(n * n * 4);
    for taps in &weights {
        for x in 0..n {
            let mut sum = [0f32; 4];
            for &(s, w) in taps {
                for (acc, v) in sum.iter_mut().zip(rows[s * n + x]) {
                    *acc += v * w;
                }
            }
            let a = sum[3].clamp(0.0, 1.0);
            let colour = |c: f32| {
                if a > 0.0 {
                    (c / a).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                }
            };
            pixels.extend_from_slice(&[
                colour(sum[0]),
                colour(sum[1]),
                colour(sum[2]),
                (a * 255.0).round() as u8,
            ]);
        }
    }
    Rgba {
        width: size,
        height: size,
        pixels,
    }
}

/// The placeholder icon of a project without one: a slate rounded square with a light "play"
/// triangle, 256 pixels, edges anti-aliased by 4x4 supersampling.
pub(crate) fn placeholder_icon() -> Rgba {
    const SIDE: u32 = 256;
    let inside = |x: f32, y: f32| -> (bool, bool) {
        // Rounded square: inset 8, corner radius 48.
        let (lo, hi, r) = (8.0, 248.0, 48.0);
        let cx = x.clamp(lo + r, hi - r);
        let cy = y.clamp(lo + r, hi - r);
        let in_square = (lo..hi).contains(&x)
            && (lo..hi).contains(&y)
            && (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r;
        // Triangle pointing right: (96, 72), (96, 184), (188, 128).
        let in_triangle = x >= 96.0 && (y - 128.0).abs() <= (188.0 - x) * (56.0 / 92.0);
        (in_square, in_square && in_triangle)
    };
    let mut pixels = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    for y in 0..SIDE {
        // A vertical gradient from #34466a to #1c2538.
        let t = y as f32 / (SIDE - 1) as f32;
        let back = [
            52.0 + (28.0 - 52.0) * t,
            70.0 + (37.0 - 70.0) * t,
            106.0 + (56.0 - 106.0) * t,
        ];
        let front = [236.0f32, 240.0, 246.0];
        for x in 0..SIDE {
            let (mut square, mut triangle) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let (px, py) = (
                        x as f32 + (sx as f32 + 0.5) / 4.0,
                        y as f32 + (sy as f32 + 0.5) / 4.0,
                    );
                    let (s, t) = inside(px, py);
                    square += u32::from(s);
                    triangle += u32::from(t);
                }
            }
            if square == 0 {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let mix = triangle as f32 / square as f32;
            let c = |i: usize| (back[i] + (front[i] - back[i]) * mix).round() as u8;
            pixels.extend_from_slice(&[c(0), c(1), c(2), (square * 255 / 16) as u8]);
        }
    }
    Rgba {
        width: SIDE,
        height: SIDE,
        pixels,
    }
}
