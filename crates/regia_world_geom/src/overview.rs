// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/src/tunnel_view.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1-135, mark: adapt (Stage A: unchanged).
// Edits: net-client cache and shell loaders dropped; plain UnitView flags; radar ranges are settings.
// Portions copied from Direct Order 27 (tools/maps/bake_minimap.py, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1-515, mark: requirements (rewritten in Rust; cases live in tests/minimap_bake.rs).
// Edits: stored-deflate PNG (no zlib crate); team palette keys 1..=4; cover overlay is Regia's, via covered_at.
// Source files are unchanged from regia-absorb-2 (e6c84ba6d).

//! Covered overview and minimap bake, and the per-unit tunnel view flags (ADR-8333, spec 121 § 5.6).
//!
//! The bake is the north-up picture of a map directory (`heightfield.json`, `layout.json`, optional
//! `tunnels.json`). [`hide_covered_ground`](crate::overview::hide_covered_ground) paints covered
//! ground in [`HIDDEN_RGB`](crate::overview::HIDDEN_RGB) and redraws portal mouths in
//! [`SIGNAL_WHITE`](crate::overview::SIGNAL_WHITE). The caller supplies the cover test, so this
//! module stays free of the `stage_b` query feature: a host passes `covered_at` (spec 121 § 5.3).
//!
//! [`mark_rock_hidden`](crate::overview::mark_rock_hidden) sets the view flag bits on a plain
//! flags field. An overview blip uses
//! [`crate::compat::sense::satellite_shows`]: entrances show, other underground units do not.

use std::path::{Path, PathBuf};

use regia_detsim::float::{det_clamp_f64, det_max_f64};
use regia_detsim::math::{cos_f64, sin_f64, sqrt_f64};
use serde_json::Value;

use crate::compat::state::ROCK_RADAR_M;

/// Overview width and height in pixels when the caller does not set one.
pub const FULL_SIZE: u32 = 1024;
/// The minimap is the overview divided by this on each side.
pub const MINI_DIVISOR: u32 = 4;
/// Grey frame thickness, in pixels.
pub const FRAME_PX: u32 = 3;
/// Largest overview side accepted (a host setting, not a copied cap).
pub const MAX_SIDE: u32 = 2048;
/// Radar through rock for an unlit unit, metres. Same value as [`ROCK_RADAR_M`].
pub const ROCK_RADAR_VIEW_M: f32 = ROCK_RADAR_M;
/// Radar through rock for a lit unit, metres (a host setting).
pub const ROCK_RADAR_LIT_M: f32 = 300.0;

/// View flag: rock hides this unit from radar and the level map.
pub const ROCK_HIDDEN: u32 = 0x100;
/// View flag: the unit counts as lit (lamps, a flare, or a live sweep).
pub const LIT: u32 = 0x200;
/// View flag: lamps are on while the unit is underground.
pub const HEADLIGHT_ON: u32 = 0x400;
/// View flag: a sweep is live while the unit is underground.
pub const PING_FLASH: u32 = 0x800;

/// The four view bits this module writes. Other flag bits are left alone.
pub const VIEW_FLAG_MASK: u32 = ROCK_HIDDEN | LIT | HEADLIGHT_ON | PING_FLASH;

/// Ink under the hillshade.
pub const GRAPHITE: [u8; 3] = [0x13, 0x13, 0x16];
/// Frame grey.
pub const FRAME_RGB: [u8; 3] = [128, 128, 128];
/// Water tint.
pub const WATER_RGB: [u8; 3] = [0x2E, 0x6F, 0x9E];
/// Ice tint.
pub const ICE_RGB: [u8; 3] = [0xCF, 0xE2, 0xEA];
/// Lava tint.
pub const LAVA_RGB: [u8; 3] = [0xB8, 0x3F, 0x1A];
/// Geyser accent.
pub const RARE_GREEN: [u8; 3] = [0x61, 0xFF, 0xCA];
/// Portals, scrap, the base outline, and entrance mouths.
pub const SIGNAL_WHITE: [u8; 3] = [255, 255, 255];
/// Covered ground after [`hide_covered_ground`]. Darker than any shaded ink pixel.
pub const HIDDEN_RGB: [u8; 3] = [8, 8, 10];

const TEAM_1: [u8; 3] = [0x82, 0xE2, 0xFF];
const TEAM_2: [u8; 3] = [0xFF, 0x67, 0x67];
const TEAM_3: [u8; 3] = [0xA2, 0x77, 0xFF];
const MUTED_SOLID: [u8; 3] = [137, 137, 139];

/// Fixed light, azimuth 315 degrees and altitude 42 degrees (libm, recorded once).
const LIGHT_X: f64 = -0.525_482_745_498_759;
const LIGHT_Y: f64 = 0.525_482_745_498_758_7;
const LIGHT_Z: f64 = 0.669_130_606_358_858_2;
const PI: f64 = std::f64::consts::PI;

/// A handled input problem. [`OverviewError::exit_code`] is 2.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OverviewError {
    /// The map inputs are missing or not the bake's JSON.
    #[error("overview_bake_input: {0}")]
    Input(String),
}

impl OverviewError {
    /// Process status. Input problems are 2, matching the ported tool.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Input(_) => 2,
        }
    }
}

fn input(message: impl Into<String>) -> OverviewError {
    OverviewError::Input(message.into())
}

/// One unit on the overview, with a plain flags field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitView {
    /// Stable id.
    pub id: u32,
    /// Team number. Team 0 is skipped by the radar hide.
    pub team: u32,
    /// World position, metres.
    pub pos: [f32; 3],
    /// View flags. [`mark_rock_hidden`] rewrites [`VIEW_FLAG_MASK`] and keeps other bits.
    pub flags: u32,
    /// Lamps, a flare, or a live sweep: the lit radar range.
    pub lit: bool,
    /// Lamps on. Sets [`HEADLIGHT_ON`] when `underground` is set.
    pub headlights: bool,
    /// A live sweep. Sets [`PING_FLASH`] when `underground` is set.
    pub ping: bool,
    /// An entrance marker. The overview shows it even when `underground` is set.
    pub entrance: bool,
    /// The caller decided this unit is in a covered gallery (`covered_at` at its position).
    pub underground: bool,
}

/// Bake knobs. Contour step 0 disables contour lines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BakeOptions {
    /// Overview width and height. Must be at least 2, at most [`MAX_SIDE`], and a multiple of [`MINI_DIVISOR`].
    pub side: u32,
    /// Contour spacing in metres. Zero disables contours.
    pub contour_step_m: f64,
    /// Rewrite the PNGs when they already exist.
    pub force: bool,
}

impl Default for BakeOptions {
    fn default() -> Self {
        Self {
            side: FULL_SIZE,
            contour_step_m: 5.0,
            force: false,
        }
    }
}

/// What [`bake_directory`] wrote, or found and left in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakeReport {
    /// `overview_<id>.png`.
    pub overview: PathBuf,
    /// `minimap_<id>.png`.
    pub minimap: PathBuf,
    /// Overview side, pixels.
    pub side: u32,
    /// Minimap side, pixels.
    pub mini_side: u32,
    /// True when both files existed and `force` was false.
    pub skipped: bool,
}

/// Map layout. Every list may be empty. `surface` defaults to rock.
#[derive(Debug, Clone, PartialEq)]
pub struct MapLayout {
    /// `rock`, `water`, `ice`, or `lava`.
    pub surface: String,
    /// Tint every sample below this height. Absent means no water line.
    pub water_level_m: Option<f64>,
    /// Geyser positions, metres.
    pub geysers: Vec<(f64, f64)>,
    /// Scrap pools: x, z, count (at least 1).
    pub scrap_pools: Vec<(f64, f64, i64)>,
    /// Spawns: x, z, team.
    pub spawns: Vec<(f64, f64, i64)>,
    /// Bases: x, z, team.
    pub bases: Vec<(f64, f64, i64)>,
    /// Tunnel mouths, metres. Filled from `tunnels.json` when baking a directory.
    pub portals: Vec<(f64, f64)>,
}

/// Heightfield grid. Row 0 is the south edge. Heights are metres.
#[derive(Debug, Clone, PartialEq)]
pub struct Heightfield {
    /// World width and depth, metres.
    pub size_m: f64,
    /// Samples on a side.
    pub cells: usize,
    /// `cells` rows of `cells` heights.
    pub heights: Vec<Vec<f64>>,
}

/// World metres to continuous pixels. North (+Z) is up, east (+X) is right. Off-map points clamp.
#[must_use]
pub fn world_to_px(x: f64, z: f64, half: f64, side: u32) -> (f64, f64) {
    let half = det_max_f64(half, 1.0);
    let u = det_clamp_f64((x + half) / (2.0 * half), 0.0, 1.0);
    let v = 1.0 - det_clamp_f64((z + half) / (2.0 * half), 0.0, 1.0);
    let span = f64::from(side.saturating_sub(1));
    (u * span, v * span)
}

/// Inverse of [`world_to_px`] at an integer pixel (the pixel's own u, v, not its center).
#[must_use]
pub fn pixel_world(px: usize, py: usize, size_m: f64, side: usize) -> (f64, f64) {
    let half = size_m / 2.0;
    let span = (side.saturating_sub(1)) as f64;
    let u = if side > 1 { px as f64 / span } else { 0.0 };
    let v = if side > 1 { py as f64 / span } else { 0.0 };
    (u * size_m - half, half - v * size_m)
}

/// Read `heightfield.json`.
///
/// # Errors
///
/// The file is missing, not the expected object, or holds a non-finite height.
pub fn load_heightfield(path: &Path) -> Result<Heightfield, OverviewError> {
    let raw = read_json(path, "heightfield")?;
    let size_m = num_field(&raw, "size_m").ok_or_else(|| {
        input(format!(
            "bad heightfield {}: need size_m/cells/heights",
            path.display()
        ))
    })?;
    let cells_n = raw.get("cells").and_then(Value::as_u64).ok_or_else(|| {
        input(format!(
            "bad heightfield {}: need size_m/cells/heights",
            path.display()
        ))
    })?;
    let rows = raw.get("heights").and_then(Value::as_array).ok_or_else(|| {
        input(format!(
            "bad heightfield {}: need size_m/cells/heights",
            path.display()
        ))
    })?;
    if size_m <= 0.0 || !(2..=8192).contains(&cells_n) {
        return Err(input(format!(
            "bad heightfield {}: size_m and cells must be positive",
            path.display()
        )));
    }
    let cells = cells_n as usize;
    if rows.len() != cells {
        return Err(input(format!(
            "bad heightfield {}: heights must be cells x cells",
            path.display()
        )));
    }
    let scale = if raw.get("unit").and_then(Value::as_str) == Some("count") {
        1.0 / 32.0
    } else {
        1.0
    };
    let mut heights = Vec::with_capacity(cells);
    for row in rows {
        let Some(cols) = row.as_array() else {
            return Err(input(format!(
                "bad heightfield {}: heights must be cells x cells",
                path.display()
            )));
        };
        if cols.len() != cells {
            return Err(input(format!(
                "bad heightfield {}: heights must be cells x cells",
                path.display()
            )));
        }
        let mut out = Vec::with_capacity(cells);
        for value in cols {
            let Some(h) = value.as_f64() else {
                return Err(input(format!(
                    "bad heightfield {}: heights must be numbers",
                    path.display()
                )));
            };
            let metres = h * scale;
            if !metres.is_finite() {
                return Err(input(format!(
                    "bad heightfield {}: heights must be finite",
                    path.display()
                )));
            }
            out.push(metres);
        }
        heights.push(out);
    }
    Ok(Heightfield {
        size_m,
        cells,
        heights,
    })
}

/// Read `layout.json`. Unknown `surface` values fail.
///
/// # Errors
///
/// The file is missing, not an object, or a listed point is not a pair of numbers.
pub fn load_layout(path: &Path) -> Result<MapLayout, OverviewError> {
    let raw = read_json(path, "layout")?;
    if !raw.is_object() {
        return Err(input(format!(
            "bad layout {}: top level must be an object",
            path.display()
        )));
    }
    let surface = raw
        .get("surface")
        .and_then(Value::as_str)
        .unwrap_or("rock")
        .to_string();
    if !matches!(surface.as_str(), "rock" | "water" | "ice" | "lava") {
        return Err(input(format!(
            "bad layout {}: surface must be rock/water/ice/lava",
            path.display()
        )));
    }
    let water_level_m = match raw.get("water_level_m") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(|| {
            input(format!(
                "bad layout {}: water_level_m must be a number",
                path.display()
            ))
        })?),
    };
    if water_level_m.is_some_and(|level| !level.is_finite()) {
        return Err(input(format!(
            "bad layout {}: water_level_m must be a number",
            path.display()
        )));
    }
    Ok(MapLayout {
        surface,
        water_level_m,
        geysers: points(raw.get("geysers"), path, "geysers")?,
        scrap_pools: pools(raw.get("scrap_pools"), path)?,
        spawns: teamed(raw.get("spawns"), path, "spawns")?,
        bases: teamed(raw.get("bases"), path, "bases")?,
        portals: Vec::new(),
    })
}

/// Read tunnel mouths. A missing file means the map has no tunnels.
///
/// # Errors
///
/// The file exists but is not an object, or a portal lacks x/z numbers.
pub fn load_portals(path: &Path) -> Result<Vec<(f64, f64)>, OverviewError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = read_json(path, "tunnels")?;
    if !raw.is_object() {
        return Err(input(format!(
            "bad tunnels {}: top level must be an object",
            path.display()
        )));
    }
    points(raw.get("portals"), path, "portals")
}

/// Render the full-size scene. North is up. Does not hide covered ground.
///
/// # Errors
///
/// `side` is below 2 or above [`MAX_SIDE`], or the heightfield is not rectangular.
pub fn bake_pixels(
    field: &Heightfield,
    layout: &MapLayout,
    side: u32,
    contour_step: f64,
) -> Result<Vec<[u8; 3]>, OverviewError> {
    if !(2..=MAX_SIDE).contains(&side) {
        return Err(input(format!("--size {side} is outside 2..={MAX_SIDE}")));
    }
    let side_n = side as usize;
    let heights = resample_north_up(field, side_n)?;
    let cell = field.size_m / f64::from(side);
    let (lo, hi, lava_mark) = elevation_stops(&heights);
    let span = det_max_f64(hi - lo, 1.0e-6);
    let mut px = shade_and_tint(&heights, side_n, cell, lo, span, lava_mark, layout);
    if contour_step > 0.0 {
        apply_contours(&mut px, &heights, side_n, contour_step);
    }
    paint_markers(&mut px, side_n, field.size_m / 2.0, layout);
    paint_frame(&mut px, side_n, FRAME_PX as usize, FRAME_RGB);
    Ok(quantize(&px))
}

/// Box-average `side / divisor`. `side` must divide evenly.
#[must_use]
pub fn downsample(px: &[[u8; 3]], side: usize, divisor: usize) -> Vec<[u8; 3]> {
    if divisor == 0 || !side.is_multiple_of(divisor) || px.len() != side * side {
        return Vec::new();
    }
    let small = side / divisor;
    let mut out = Vec::with_capacity(small * small);
    for py in 0..small {
        for pxx in 0..small {
            let mut rs = 0u32;
            let mut gs = 0u32;
            let mut bs = 0u32;
            for dy in 0..divisor {
                for dx in 0..divisor {
                    let sample = px[(py * divisor + dy) * side + pxx * divisor + dx];
                    rs += u32::from(sample[0]);
                    gs += u32::from(sample[1]);
                    bs += u32::from(sample[2]);
                }
            }
            let n = (divisor * divisor) as u32;
            out.push([(rs / n) as u8, (gs / n) as u8, (bs / n) as u8]);
        }
    }
    out
}

/// Pixels of portal rings, in overview pixel order. Mouths are redrawn from this mask.
#[must_use]
pub fn entrance_mask(side: usize, size_m: f64, portals: &[(f64, f64)]) -> Vec<bool> {
    let mut mask = vec![false; side.saturating_mul(side)];
    if side < 2 || mask.len() != side * side {
        return mask;
    }
    let half = size_m / 2.0;
    for &(x, z) in portals {
        let (cx, cy) = world_to_px(x, z, half, side as u32);
        stamp_ring(&mut mask, side, cx, cy, det_max_f64(side as f64 / 85.0, 3.0));
        stamp_ring(&mut mask, side, cx, cy, det_max_f64(side as f64 / 170.0, 2.0));
    }
    mask
}

/// Hide covered ground and redraw mouths.
///
/// `covered_at(x, z)` is the host's cover test at the overview probe height (a standing eye under
/// the roof, typically 1 m). Every covered pixel becomes [`HIDDEN_RGB`]. Every portal-ring pixel
/// is then [`SIGNAL_WHITE`], so a mouth on a covered roof stays an entrance.
pub fn hide_covered_ground(
    pixels: &mut [[u8; 3]],
    side: usize,
    size_m: f64,
    portals: &[(f64, f64)],
    covered_at: impl Fn(f64, f64) -> bool,
) {
    if side < 2 || pixels.len() != side * side || !size_m.is_finite() || size_m <= 0.0 {
        return;
    }
    for py in 0..side {
        for px in 0..side {
            let (x, z) = pixel_world(px, py, size_m, side);
            if covered_at(x, z) {
                pixels[py * side + px] = HIDDEN_RGB;
            }
        }
    }
    let mask = entrance_mask(side, size_m, portals);
    for (pixel, mouth) in pixels.iter_mut().zip(mask) {
        if mouth {
            *pixel = SIGNAL_WHITE;
        }
    }
}

/// Bake one map directory into `overview_<id>.png` and `minimap_<id>.png`.
///
/// `covered_at`, when set, runs [`hide_covered_ground`] before the minimap downsample. The id is
/// the directory's file name.
///
/// # Errors
///
/// An input is missing or invalid, `side` does not divide by [`MINI_DIVISOR`], or a PNG cannot be written.
pub fn bake_directory(
    map_dir: &Path,
    out_dir: &Path,
    options: &BakeOptions,
    covered_at: Option<&dyn Fn(f64, f64) -> bool>,
) -> Result<BakeReport, OverviewError> {
    if !options.side.is_multiple_of(MINI_DIVISOR) {
        return Err(input(format!(
            "--size {} must divide evenly by {MINI_DIVISOR}",
            options.side
        )));
    }
    let id = map_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("map");
    let overview = out_dir.join(format!("overview_{id}.png"));
    let minimap = out_dir.join(format!("minimap_{id}.png"));
    let mini_side = options.side / MINI_DIVISOR;
    if !options.force && overview.is_file() && minimap.is_file() {
        return Ok(BakeReport {
            overview,
            minimap,
            side: options.side,
            mini_side,
            skipped: true,
        });
    }
    let field = load_heightfield(&map_dir.join("heightfield.json"))?;
    let mut layout = load_layout(&map_dir.join("layout.json"))?;
    layout.portals = load_portals(&map_dir.join("tunnels.json"))?;
    let mut full = bake_pixels(&field, &layout, options.side, options.contour_step_m)?;
    if let Some(pred) = covered_at {
        hide_covered_ground(
            &mut full,
            options.side as usize,
            field.size_m,
            &layout.portals,
            pred,
        );
    }
    let mini = downsample(&full, options.side as usize, MINI_DIVISOR as usize);
    std::fs::create_dir_all(out_dir)
        .map_err(|err| input(format!("cannot create {}: {err}", out_dir.display())))?;
    write_png(&overview, &full, options.side)?;
    write_png(&minimap, &mini, mini_side)?;
    Ok(BakeReport {
        overview,
        minimap,
        side: options.side,
        mini_side,
        skipped: false,
    })
}

/// Write an 8-bit RGB PNG. Compression is stored-deflate, so the bytes are deterministic without zlib.
///
/// # Errors
///
/// `pixels` length is not `side * side`, or the file cannot be written.
pub fn write_png(path: &Path, pixels: &[[u8; 3]], side: u32) -> Result<(), OverviewError> {
    let side_n = side as usize;
    if side < 1 || pixels.len() != side_n * side_n {
        return Err(input(format!(
            "png {side} does not match {} pixels",
            pixels.len()
        )));
    }
    let mut raw = Vec::with_capacity((1 + side_n * 3) * side_n);
    for py in 0..side_n {
        raw.push(0);
        for px in 0..side_n {
            raw.extend_from_slice(&pixels[py * side_n + px]);
        }
    }
    let blob = zlib_store(&raw);
    let mut png = Vec::with_capacity(8 + 8 + 13 + 12 + blob.len() + 12 + 12);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &blob);
    push_chunk(&mut png, b"IEND", &[]);
    write_atomic(path, &png)
}

/// Decode an 8-bit RGB PNG written by [`write_png`] (stored deflate, filter 0).
///
/// # Errors
///
/// The bytes are not that PNG form.
pub fn decode_rgb_png(bytes: &[u8]) -> Result<(u32, Vec<[u8; 3]>), OverviewError> {
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(input("png: bad signature"));
    }
    let mut pos = 8usize;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut idat = Vec::new();
    let mut saw_ihdr = false;
    while pos + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
        let tag = &bytes[pos + 4..pos + 8];
        let data_end = pos + 8 + len;
        if data_end + 4 > bytes.len() {
            return Err(input("png: truncated chunk"));
        }
        let data = &bytes[pos + 8..data_end];
        let stored = u32::from_be_bytes(bytes[data_end..data_end + 4].try_into().unwrap_or([0; 4]));
        if crc32_tag(tag, data) != stored {
            return Err(input("png: bad crc"));
        }
        if tag == b"IHDR" {
            if data.len() != 13 {
                return Err(input("png: bad ihdr"));
            }
            width = u32::from_be_bytes(data[0..4].try_into().unwrap_or([0; 4]));
            height = u32::from_be_bytes(data[4..8].try_into().unwrap_or([0; 4]));
            if data[8] != 8 || data[9] != 2 || data[10..13] != [0, 0, 0] {
                return Err(input("png: only 8-bit RGB"));
            }
            saw_ihdr = true;
        } else if tag == b"IDAT" {
            idat.extend_from_slice(data);
        }
        pos = data_end + 4;
        if tag == b"IEND" {
            break;
        }
    }
    if !saw_ihdr || width == 0 || height != width {
        return Err(input("png: expected a square RGB image"));
    }
    let raw = inflate_stored(&idat)?;
    let stride = 1 + width as usize * 3;
    if raw.len() != stride * height as usize {
        return Err(input("png: inflated size"));
    }
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for py in 0..height as usize {
        let row = &raw[py * stride..py * stride + stride];
        if row[0] != 0 {
            return Err(input("png: only filter 0"));
        }
        for px in 0..width as usize {
            let i = 1 + px * 3;
            pixels.push([row[i], row[i + 1], row[i + 2]]);
        }
    }
    Ok((width, pixels))
}

/// Set [`ROCK_HIDDEN`] and [`LIT`] on enemies rock hides, and lamp bits on underground units.
///
/// Team 0, `team`, and `controlled` are never radar-hidden. Allies still get lamp bits when
/// `underground` is set. `viewer_underground` is the camera or the controlled body (`covered_at`
/// at that point). The viewer's position is the controlled unit when that id is in `units`, and
/// `viewer` otherwise.
pub fn mark_rock_hidden(
    units: &mut [UnitView],
    controlled: Option<u32>,
    team: u32,
    viewer: [f32; 3],
    viewer_underground: bool,
) {
    let viewer_pos = match controlled {
        Some(id) => units
            .iter()
            .find(|unit| unit.id == id)
            .map(|unit| unit.pos)
            .unwrap_or(viewer),
        None => viewer,
    };
    for unit in units.iter_mut() {
        unit.flags &= !VIEW_FLAG_MASK;
        let enemy = unit.team != 0 && unit.team != team && controlled != Some(unit.id);
        if enemy
            && rock_hides(
                viewer_underground,
                unit.underground,
                viewer_pos,
                unit.pos,
                unit.lit,
            )
        {
            unit.flags |= ROCK_HIDDEN;
            if unit.lit {
                unit.flags |= LIT;
            }
        } else if enemy && unit.lit {
            unit.flags |= LIT;
        }
        if unit.underground {
            if unit.headlights {
                unit.flags |= HEADLIGHT_ON;
            }
            if unit.ping {
                unit.flags |= PING_FLASH;
            }
        }
    }
}

/// True when exactly one of the two bodies is underground and they stand farther than the radar range.
#[must_use]
pub fn rock_hides(
    viewer_underground: bool,
    target_underground: bool,
    viewer: [f32; 3],
    target: [f32; 3],
    target_lit: bool,
) -> bool {
    if viewer_underground == target_underground {
        return false;
    }
    let range = if target_lit {
        ROCK_RADAR_LIT_M
    } else {
        ROCK_RADAR_VIEW_M
    };
    let dx = viewer[0] - target[0];
    let dy = viewer[1] - target[1];
    let dz = viewer[2] - target[2];
    dx * dx + dy * dy + dz * dz > range * range
}

fn read_json(path: &Path, kind: &str) -> Result<Value, OverviewError> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| input(format!("cannot read {kind} {}: {err}", path.display())))?;
    serde_json::from_str(&text).map_err(|err| input(format!("cannot read {kind} {}: {err}", path.display())))
}

fn num_field(raw: &Value, key: &str) -> Option<f64> {
    raw.get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn points(value: Option<&Value>, path: &Path, key: &str) -> Result<Vec<(f64, f64)>, OverviewError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(input(format!(
            "bad layout {}: {key} must be a list",
            path.display()
        )));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (Some(x), Some(z)) = (
            item.get("x").and_then(Value::as_f64),
            item.get("z").and_then(Value::as_f64),
        ) else {
            return Err(input(format!(
                "bad layout {}: {key} entries need x/z numbers",
                path.display()
            )));
        };
        if !x.is_finite() || !z.is_finite() {
            return Err(input(format!(
                "bad layout {}: {key} entries need x/z numbers",
                path.display()
            )));
        }
        out.push((x, z));
    }
    Ok(out)
}

fn pools(value: Option<&Value>, path: &Path) -> Result<Vec<(f64, f64, i64)>, OverviewError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(input(format!(
            "bad layout {}: scrap_pools must be a list",
            path.display()
        )));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (Some(x), Some(z)) = (
            item.get("x").and_then(Value::as_f64),
            item.get("z").and_then(Value::as_f64),
        ) else {
            return Err(input(format!(
                "bad layout {}: scrap_pools entries need x/z/count",
                path.display()
            )));
        };
        let count = item.get("count").and_then(Value::as_i64).unwrap_or(1);
        if !x.is_finite() || !z.is_finite() {
            return Err(input(format!(
                "bad layout {}: scrap_pools entries need x/z/count",
                path.display()
            )));
        }
        let count = if count < 1 { 1 } else { count };
        out.push((x, z, count));
    }
    Ok(out)
}

fn teamed(value: Option<&Value>, path: &Path, key: &str) -> Result<Vec<(f64, f64, i64)>, OverviewError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(input(format!(
            "bad layout {}: {key} must be a list",
            path.display()
        )));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (Some(x), Some(z)) = (
            item.get("x").and_then(Value::as_f64),
            item.get("z").and_then(Value::as_f64),
        ) else {
            return Err(input(format!(
                "bad layout {}: {key} entries need x/z numbers",
                path.display()
            )));
        };
        if !x.is_finite() || !z.is_finite() {
            return Err(input(format!(
                "bad layout {}: {key} entries need x/z numbers",
                path.display()
            )));
        }
        let team = item.get("team").and_then(Value::as_i64).unwrap_or(0);
        out.push((x, z, team));
    }
    Ok(out)
}

fn resample_north_up(field: &Heightfield, side: usize) -> Result<Vec<f64>, OverviewError> {
    let cells = field.cells;
    if cells < 2 || field.heights.len() != cells {
        return Err(input("heightfield is not rectangular"));
    }
    let mut out = vec![0.0; side * side];
    for py in 0..side {
        let v = if side > 1 {
            py as f64 / (side - 1) as f64
        } else {
            0.0
        };
        let src_row = (1.0 - v) * (cells - 1) as f64;
        let r0 = index_pair(src_row, cells);
        let tr = src_row - r0 as f64;
        for px in 0..side {
            let u = if side > 1 {
                px as f64 / (side - 1) as f64
            } else {
                0.0
            };
            let src_col = u * (cells - 1) as f64;
            let c0 = index_pair(src_col, cells);
            let tc = src_col - c0 as f64;
            let a = field.heights[r0][c0];
            let b = field.heights[r0][c0 + 1];
            let c = field.heights[r0 + 1][c0];
            let d = field.heights[r0 + 1][c0 + 1];
            let top = a + (b - a) * tc;
            let bot = c + (d - c) * tc;
            out[py * side + px] = top + (bot - top) * tr;
        }
    }
    Ok(out)
}

fn index_pair(src: f64, cells: usize) -> usize {
    let truncated = src.floor() as usize;
    if truncated + 1 >= cells {
        cells - 2
    } else {
        truncated
    }
}

fn elevation_stops(heights: &[f64]) -> (f64, f64, f64) {
    let mut ordered = heights.to_vec();
    ordered.sort_by(|a, b| a.total_cmp(b));
    (
        percentile(&ordered, 0.01),
        percentile(&ordered, 0.99),
        percentile(&ordered, 0.25),
    )
}

fn percentile(sorted: &[f64], frac: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let pos = det_clamp_f64(frac, 0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = if lo + 1 >= sorted.len() {
        sorted.len() - 1
    } else {
        lo + 1
    };
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn shade_and_tint(
    heights: &[f64],
    side: usize,
    cell: f64,
    lo: f64,
    span: f64,
    lava_mark: f64,
    layout: &MapLayout,
) -> Vec<[f64; 3]> {
    let mut px = Vec::with_capacity(side * side);
    for py in 0..side {
        for pxx in 0..side {
            let gx = (sample_at(heights, side, pxx as i32 + 1, py as i32)
                - sample_at(heights, side, pxx as i32 - 1, py as i32))
                / (2.0 * cell);
            let gy = (sample_at(heights, side, pxx as i32, py as i32 + 1)
                - sample_at(heights, side, pxx as i32, py as i32 - 1))
                / (2.0 * cell);
            let nx = -gx;
            let ny = gy;
            let inv = 1.0 / sqrt_f64(nx * nx + ny * ny + 1.0);
            let shade = (nx * LIGHT_X + ny * LIGHT_Y + LIGHT_Z) * inv;
            let mut lum = det_clamp_f64(0.50 + (shade - LIGHT_Z) * 1.1, 0.0, 1.0);
            let elev = det_clamp_f64((heights[py * side + pxx] - lo) / span, 0.0, 1.0);
            lum *= 0.7 + 0.3 * elev;
            let base = 0.06 + 0.84 * lum;
            let mut rgb = [
                f64::from(GRAPHITE[0]) + f64::from(255 - GRAPHITE[0]) * base,
                f64::from(GRAPHITE[1]) + f64::from(255 - GRAPHITE[1]) * base,
                f64::from(GRAPHITE[2]) + f64::from(255 - GRAPHITE[2]) * base,
            ];
            tint(
                &mut rgb,
                layout,
                heights[py * side + pxx],
                elev,
                gx,
                gy,
                lava_mark,
            );
            px.push(rgb);
        }
    }
    px
}

fn tint(rgb: &mut [f64; 3], layout: &MapLayout, height: f64, elev: f64, gx: f64, gy: f64, lava_mark: f64) {
    if let Some(level) = layout.water_level_m
        && height < level
    {
        blend(rgb, WATER_RGB, 0.65);
    } else if layout.surface == "water" && elev < 0.25 {
        blend(rgb, WATER_RGB, 0.45);
    } else if layout.surface == "ice" && abs_f64(gx) + abs_f64(gy) < 0.08 {
        blend(rgb, ICE_RGB, 0.35);
    } else if layout.surface == "lava" && height < lava_mark {
        blend(rgb, LAVA_RGB, 0.55);
    }
}

fn blend(under: &mut [f64; 3], over: [u8; 3], alpha: f64) {
    for (channel, over_c) in under.iter_mut().zip(over) {
        *channel += (f64::from(over_c) - *channel) * alpha;
    }
}

fn apply_contours(px: &mut [[f64; 3]], heights: &[f64], side: usize, step: f64) {
    for py in 0..side {
        for pxx in 0..side {
            let band = (heights[py * side + pxx] / step).floor();
            let east = (sample_at(heights, side, pxx as i32 + 1, py as i32) / step).floor();
            let south = (sample_at(heights, side, pxx as i32, py as i32 + 1) / step).floor();
            if east != band || south != band {
                let rgb = &mut px[py * side + pxx];
                rgb[0] *= 0.7;
                rgb[1] *= 0.7;
                rgb[2] *= 0.7;
            }
        }
    }
}

fn sample_at(heights: &[f64], side: usize, ix: i32, iy: i32) -> f64 {
    let x = clamp_index(ix, side);
    let y = clamp_index(iy, side);
    heights[y * side + x]
}

fn clamp_index(i: i32, side: usize) -> usize {
    if i <= 0 {
        0
    } else if i as usize >= side {
        side - 1
    } else {
        i as usize
    }
}

fn paint_markers(px: &mut [[f64; 3]], side: usize, half: f64, layout: &MapLayout) {
    let side_u = side as u32;
    for &(x, z, _) in &layout.scrap_pools {
        let (cx, cy) = world_to_px(x, z, half, side_u);
        disc(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 256.0, 1.0),
            SIGNAL_WHITE,
        );
    }
    for &(x, z) in &layout.geysers {
        let (cx, cy) = world_to_px(x, z, half, side_u);
        ring(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 128.0, 2.0),
            RARE_GREEN,
        );
        disc(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 512.0, 1.0),
            RARE_GREEN,
        );
    }
    for &(x, z) in &layout.portals {
        let (cx, cy) = world_to_px(x, z, half, side_u);
        ring(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 85.0, 3.0),
            SIGNAL_WHITE,
        );
        ring(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 170.0, 2.0),
            SIGNAL_WHITE,
        );
    }
    for &(x, z, team) in &layout.spawns {
        let (cx, cy) = world_to_px(x, z, half, side_u);
        square(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 170.0, 2.0),
            team_rgb(team),
        );
    }
    for &(x, z, team) in &layout.bases {
        let (cx, cy) = world_to_px(x, z, half, side_u);
        let color = team_rgb(team);
        square(px, side, cx, cy, det_max_f64(side as f64 / 85.0, 3.0), color);
        square_outline(
            px,
            side,
            cx,
            cy,
            det_max_f64(side as f64 / 64.0, 4.0),
            SIGNAL_WHITE,
        );
    }
}

fn team_rgb(team: i64) -> [u8; 3] {
    match team {
        1 => TEAM_1,
        2 => TEAM_2,
        3 | 4 => TEAM_3,
        _ => MUTED_SOLID,
    }
}

fn paint_frame(px: &mut [[f64; 3]], side: usize, width: usize, color: [u8; 3]) {
    let band = if width < side { width } else { side };
    for y in 0..band {
        for x in 0..side {
            set_f(px, side, x as i32, y as i32, color);
            set_f(px, side, x as i32, (side - 1 - y) as i32, color);
        }
    }
    for x in 0..band {
        for y in 0..side {
            set_f(px, side, x as i32, y as i32, color);
            set_f(px, side, (side - 1 - x) as i32, y as i32, color);
        }
    }
}

fn disc(px: &mut [[f64; 3]], side: usize, cx: f64, cy: f64, rad: f64, color: [u8; 3]) {
    let r = rad.ceil() as i32;
    let limit = (rad + 0.5) * (rad + 0.5);
    let ox = cx.round() as i32;
    let oy = cy.round() as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            if (dx * dx + dy * dy) as f64 <= limit {
                set_f(px, side, ox + dx, oy + dy, color);
            }
        }
    }
}

fn ring(px: &mut [[f64; 3]], side: usize, cx: f64, cy: f64, rad: f64, color: [u8; 3]) {
    let steps_f = det_max_f64((2.0 * PI * rad * 2.0).floor(), 8.0);
    let steps = steps_f as i32;
    if steps <= 0 {
        return;
    }
    for i in 0..steps {
        let angle = 2.0 * PI * f64::from(i) / f64::from(steps);
        let x = (cx + rad * cos_f64(angle)).round() as i32;
        let y = (cy + rad * sin_f64(angle)).round() as i32;
        set_f(px, side, x, y, color);
    }
}

fn stamp_ring(mask: &mut [bool], side: usize, cx: f64, cy: f64, rad: f64) {
    let steps_f = det_max_f64((2.0 * PI * rad * 2.0).floor(), 8.0);
    let steps = steps_f as i32;
    if steps <= 0 {
        return;
    }
    for i in 0..steps {
        let angle = 2.0 * PI * f64::from(i) / f64::from(steps);
        let x = (cx + rad * cos_f64(angle)).round() as i32;
        let y = (cy + rad * sin_f64(angle)).round() as i32;
        if x >= 0 && (x as usize) < side && y >= 0 && (y as usize) < side {
            mask[y as usize * side + x as usize] = true;
        }
    }
}

fn square(px: &mut [[f64; 3]], side: usize, cx: f64, cy: f64, half_side: f64, color: [u8; 3]) {
    let r = half_side.ceil() as i32;
    let ox = cx.round() as i32;
    let oy = cy.round() as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            set_f(px, side, ox + dx, oy + dy, color);
        }
    }
}

fn square_outline(px: &mut [[f64; 3]], side: usize, cx: f64, cy: f64, half_side: f64, color: [u8; 3]) {
    let r = half_side.ceil() as i32;
    let ox = cx.round() as i32;
    let oy = cy.round() as i32;
    for dx in -r..=r {
        set_f(px, side, ox + dx, oy - r, color);
        set_f(px, side, ox + dx, oy + r, color);
    }
    for dy in -r..=r {
        set_f(px, side, ox - r, oy + dy, color);
        set_f(px, side, ox + r, oy + dy, color);
    }
}

fn set_f(px: &mut [[f64; 3]], side: usize, x: i32, y: i32, color: [u8; 3]) {
    if x >= 0 && (x as usize) < side && y >= 0 && (y as usize) < side {
        px[y as usize * side + x as usize] = [f64::from(color[0]), f64::from(color[1]), f64::from(color[2])];
    }
}

fn quantize(px: &[[f64; 3]]) -> Vec<[u8; 3]> {
    px.iter()
        .map(|rgb| {
            [
                quant_channel(rgb[0]),
                quant_channel(rgb[1]),
                quant_channel(rgb[2]),
            ]
        })
        .collect()
}

fn quant_channel(value: f64) -> u8 {
    let rounded = value.round();
    if rounded <= 0.0 {
        0
    } else if rounded >= 255.0 {
        255
    } else {
        rounded as u8
    }
}

fn abs_f64(value: f64) -> f64 {
    if value < 0.0 { -value } else { value }
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16 + data.len() / 65535);
    out.push(0x78);
    out.push(0x01);
    let mut offset = 0usize;
    loop {
        let end = if offset + 65535 < data.len() {
            offset + 65535
        } else {
            data.len()
        };
        let chunk = &data[offset..end];
        let last = end == data.len();
        out.push(if last { 0x01 } else { 0x00 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
        offset = end;
        if last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn inflate_stored(blob: &[u8]) -> Result<Vec<u8>, OverviewError> {
    if blob.len() < 6 || blob[0] != 0x78 {
        return Err(input("png: not a stored zlib stream"));
    }
    let mut i = 2usize;
    let mut out = Vec::new();
    loop {
        if i + 5 > blob.len() {
            return Err(input("png: truncated stored block"));
        }
        let header = blob[i];
        i += 1;
        if header & 0b110 != 0 {
            return Err(input("png: not a stored block"));
        }
        let len = u16::from_le_bytes([blob[i], blob[i + 1]]) as usize;
        let nlen = u16::from_le_bytes([blob[i + 2], blob[i + 3]]);
        i += 4;
        if nlen != !(len as u16) || i + len > blob.len() {
            return Err(input("png: stored length"));
        }
        out.extend_from_slice(&blob[i..i + len]);
        i += len;
        if header & 1 == 1 {
            break;
        }
    }
    if i + 4 != blob.len() {
        return Err(input("png: adler trailer"));
    }
    let got = u32::from_be_bytes([blob[i], blob[i + 1], blob[i + 2], blob[i + 3]]);
    if got != adler32(&out) {
        return Err(input("png: adler mismatch"));
    }
    Ok(out)
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32_tag(tag: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    crc = crc32_update(crc, tag);
    crc = crc32_update(crc, data);
    !crc
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

fn push_chunk(png: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    let len = data.len() as u32;
    png.extend_from_slice(&len.to_be_bytes());
    png.extend_from_slice(tag);
    png.extend_from_slice(data);
    png.extend_from_slice(&crc32_tag(tag, data).to_be_bytes());
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), OverviewError> {
    let tmp = path.with_extension("png.tmp");
    std::fs::write(&tmp, bytes).map_err(|err| input(format!("writing {}: {err}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        input(format!("renaming to {}: {err}", path.display()))
    })
}
