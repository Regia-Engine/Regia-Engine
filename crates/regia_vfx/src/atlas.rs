// SPDX-License-Identifier: MPL-2.0
//! The built-in procedural sprites: a 512×256 atlas of 64 px tiles (8×4 grid, one level) for
//! everything small, and a separate 2048×256 fireball sheet of 256 px tiles with six mip levels.
//!
//! No texture assets exist yet (M4 imports textures; M5 owns material sampling), so emitter
//! sprites are generated deterministically on the CPU at startup and uploaded as
//! `Rgba8UnormSrgb` textures. Identity sprites occupy tiles 0..7. Tiles 8..24 are a 16-frame
//! puff flipbook ([`SpriteId::PuffAnim`]). Tiles 24..32 are an 8-frame flame sheet
//! ([`SpriteId::FireAnim`]): a teardrop, tip up, warped by value-noise FBM. Tile indices
//! 32..40 address the fireball sheet ([`SpriteId::FireballAnim`]): a domain-warped FBM billow
//! with a baked temperature ramp (hot core, orange body, sooty rim), flame tongues and soot
//! folds. Sparks, rain, dust and smoke stay on the 64 px set (cheap to sample for 100k
//! particles); only the few large fireball cards pay for 256 px tiles. The billboard shader
//! addresses the grid as `tile % 8`, `tile / 8` (`ATLAS_COLS` / `ATLAS_ROWS` in `shaders.rs`)
//! and switches to the sheet from tile 32. The generator is pure math (radial falloffs +
//! value noise from the engine PCG32), so the textures are byte-stable across platforms —
//! offscreen goldens can rely on them.

use regia_core::math::{Rng, det_exp, det_powf_nonnegative};

use crate::format::SpriteId;

/// Atlas width in pixels.
pub const ATLAS_SIZE: u32 = 512;
/// Atlas height in pixels (four rows of tiles).
pub const ATLAS_HEIGHT: u32 = 256;
/// Tiles per atlas row. The billboard shader's `ATLAS_COLS` must match.
pub const TILES_X: u32 = 8;
/// Tile rows. The billboard shader's `ATLAS_ROWS` must match.
pub const TILES_Y: u32 = 4;
/// Tile size in pixels.
pub const TILE: u32 = 64;
/// First tile of the 16-frame puff sheet.
pub const PUFF_ANIM_TILE: u32 = 8;
/// Frames in the puff sheet (tiles [`PUFF_ANIM_TILE`] .. [`PUFF_ANIM_TILE`] + this).
pub const PUFF_ANIM_FRAMES: u32 = 16;
/// First tile of the 8-frame flame sheet (the last atlas row).
pub const FIRE_ANIM_TILE: u32 = 24;
/// Frames in the flame sheet (tiles [`FIRE_ANIM_TILE`] .. [`FIRE_ANIM_TILE`] + this).
pub const FIRE_ANIM_FRAMES: u32 = 8;
/// Tile index of the first fireball frame; indices from here address the fireball sheet.
pub const FIREBALL_ANIM_TILE: u32 = 32;
/// Frames in the fireball-body sheet.
pub const FIREBALL_ANIM_FRAMES: u32 = 8;
/// Fireball sheet tile size in pixels (crisp at a few hundred pixels on screen).
pub const FIREBALL_TILE: u32 = 256;
/// Fireball sheet width (one row of [`FIREBALL_ANIM_FRAMES`] tiles).
pub const FIREBALL_SHEET_WIDTH: u32 = FIREBALL_TILE * FIREBALL_ANIM_FRAMES;
/// Fireball sheet height.
pub const FIREBALL_SHEET_HEIGHT: u32 = FIREBALL_TILE;
/// Fireball sheet mip levels (tiles shrink to 8 px at the last one).
pub const FIREBALL_MIP_LEVELS: u32 = 6;

/// The RGBA8 (sRGB) atlas, row-major, top-left origin (cached; this is a copy).
pub fn generate_atlas() -> Vec<u8> {
    atlas_rgba().to_vec()
}

/// The 64 px sprite atlas, built once per process.
pub fn atlas_rgba() -> &'static [u8] {
    static ATLAS: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    ATLAS.get_or_init(build_atlas)
}

/// The fireball sheet mip chain (level 0 first), built once per process and cached.
///
/// Each level halves the previous one with a 2×2 box filter on premultiplied linear colour,
/// so a frame's transparent border stays transparent and frames do not bleed together.
pub fn fireball_sheet_mips() -> &'static [Vec<u8>] {
    static MIPS: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    MIPS.get_or_init(|| {
        let mut levels = vec![build_fireball_sheet()];
        for level in 1..FIREBALL_MIP_LEVELS {
            let prev = &levels[level as usize - 1];
            let next = downsample(
                prev,
                FIREBALL_SHEET_WIDTH >> (level - 1),
                FIREBALL_SHEET_HEIGHT >> (level - 1),
            );
            levels.push(next);
        }
        levels
    })
}

/// Rasterises the fireball sheet (one thread per frame; the bytes do not depend on the
/// thread count).
fn build_fireball_sheet() -> Vec<u8> {
    let w = FIREBALL_SHEET_WIDTH;
    let mut rgba = vec![0u8; (w * FIREBALL_SHEET_HEIGHT * 4) as usize];
    // Frame f owns columns f*T..(f+1)*T of every row: split by rows, then each row by frame.
    let mut columns: Vec<Vec<(u32, &mut [u8])>> = (0..FIREBALL_ANIM_FRAMES).map(|_| Vec::new()).collect();
    for (y, row) in rgba.chunks_mut((w * 4) as usize).enumerate() {
        for (f, cell) in row.chunks_mut((FIREBALL_TILE * 4) as usize).enumerate() {
            columns[f].push((y as u32, cell));
        }
    }
    std::thread::scope(|scope| {
        for (frame, cells) in columns.into_iter().enumerate() {
            scope.spawn(move || {
                for (py, cell) in cells {
                    for px in 0..FIREBALL_TILE {
                        let u = (px as f32 + 0.5) / FIREBALL_TILE as f32 * 2.0 - 1.0;
                        let v = (py as f32 + 0.5) / FIREBALL_TILE as f32 * 2.0 - 1.0;
                        let (r, g, b, a) = fireball_frame(u, v, frame as u32);
                        let i = (px * 4) as usize;
                        cell[i] = r;
                        cell[i + 1] = g;
                        cell[i + 2] = b;
                        cell[i + 3] = a;
                    }
                }
            });
        }
    });
    rgba
}

fn srgb_byte_to_linear(b: u8) -> f32 {
    let c = f32::from(b) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        // The deterministic kernel: platform `powf` last digits would store different bytes.
        det_powf_nonnegative((c + 0.055) / 1.055, 2.4)
    }
}

/// 2×2 box filter of premultiplied linear RGBA (width and height even).
fn downsample(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width / 2, height / 2);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let i = (((y * 2 + dy) * width + x * 2 + dx) * 4) as usize;
                let a = f32::from(src[i + 3]) / 255.0;
                for c in 0..3 {
                    acc[c] += srgb_byte_to_linear(src[i + c]) * a;
                }
                acc[3] += a;
            }
            let o = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                let lin = if acc[3] > 1e-6 { acc[c] / acc[3] } else { 0.0 };
                out[o + c] = linear_to_srgb_byte(lin);
            }
            out[o + 3] = (acc[3] / 4.0 * 255.0).round() as u8;
        }
    }
    out
}

/// Rasterises every tile at full resolution (one thread per tile row; the bytes do not
/// depend on the thread count).
fn build_atlas() -> Vec<u8> {
    let mut rgba = vec![0u8; (ATLAS_SIZE * ATLAS_HEIGHT * 4) as usize];
    let row_bytes = (ATLAS_SIZE * TILE * 4) as usize;
    std::thread::scope(|scope| {
        for (row, slab) in rgba.chunks_mut(row_bytes).enumerate() {
            scope.spawn(move || {
                for col in 0..TILES_X {
                    raster_tile(slab, row as u32 * TILES_X + col, col * TILE);
                }
            });
        }
    });
    rgba
}

/// Writes tile `i` into `slab` (its tile row of the atlas), starting at pixel column `tx`.
fn raster_tile(slab: &mut [u8], i: u32, tx: u32) {
    for py in 0..TILE {
        for px in 0..TILE {
            {
                let u = (px as f32 + 0.5) / TILE as f32 * 2.0 - 1.0;
                let v = (py as f32 + 0.5) / TILE as f32 * 2.0 - 1.0;
                let (r, g, b, a) = if (FIRE_ANIM_TILE..FIRE_ANIM_TILE + FIRE_ANIM_FRAMES).contains(&i) {
                    fire_frame(u, v, i - FIRE_ANIM_TILE)
                } else if (PUFF_ANIM_TILE..PUFF_ANIM_TILE + PUFF_ANIM_FRAMES).contains(&i) {
                    let a = puff_frame_alpha(u, v, i - PUFF_ANIM_TILE);
                    (255, 255, 255, a)
                } else if i < PUFF_ANIM_TILE {
                    (255, 255, 255, tile_alpha(identity_sprite(i), u, v))
                } else {
                    (0, 0, 0, 0)
                };
                let x = tx + px;
                let idx = ((py * ATLAS_SIZE + x) * 4) as usize;
                slab[idx] = r;
                slab[idx + 1] = g;
                slab[idx + 2] = b;
                slab[idx + 3] = a;
            }
        }
    }
}

/// Atlas tile index of a sprite (u,v origin), for UV math in shaders and CPU tests.
pub fn tile_index(sprite: SpriteId) -> u32 {
    match sprite {
        SpriteId::SoftDisc => 0,
        SpriteId::HardDisc => 1,
        SpriteId::Ring => 2,
        SpriteId::Spark => 3,
        SpriteId::Puff => 4,
        SpriteId::Flare => 5,
        SpriteId::Shard => 6,
        SpriteId::PuffAnim => PUFF_ANIM_TILE,
        SpriteId::FireAnim => FIRE_ANIM_TILE,
        SpriteId::FireballAnim => FIREBALL_ANIM_TILE,
    }
}

fn identity_sprite(tile: u32) -> SpriteId {
    match tile {
        1 => SpriteId::HardDisc,
        2 => SpriteId::Ring,
        3 => SpriteId::Spark,
        4 => SpriteId::Puff,
        5 => SpriteId::Flare,
        6 => SpriteId::Shard,
        _ => SpriteId::SoftDisc,
    }
}

fn tile_alpha(sprite: SpriteId, u: f32, v: f32) -> u8 {
    let r = (u * u + v * v).sqrt();
    let a = match sprite {
        SpriteId::SoftDisc => (1.0 - r).clamp(0.0, 1.0).powi(2),
        SpriteId::HardDisc => {
            if r < 0.92 {
                1.0
            } else {
                0.0
            }
        }
        SpriteId::Ring => {
            let d = (r - 0.7).abs();
            (1.0 - d / 0.22).clamp(0.0, 1.0).powi(2)
        }
        SpriteId::Spark => {
            // Horizontal streak: exponential falloff, much slower along x than y.
            let core = det_exp(-(u.abs() * 3.0));
            let fall = det_exp(-v.abs() * 30.0);
            (core * fall).clamp(0.0, 1.0)
        }
        SpriteId::Puff => puff_density(u, v, 2) as f32 / 255.0,
        SpriteId::Flare => {
            // Tight core plus four spikes that still have ink at the tile edge.
            // A fat radial disc hides the star (the muzzle plate). The diagonal
            // stays dark so the arms read against the sky.
            let core = det_exp(-r * r * 18.0);
            // The thin axis is wide enough that the centre texel of a 64 px tile,
            // which sits half a pixel off u = 0, still carries the arm. The last
            // fraction of the tile fades to 0 so the quad does not cut a square.
            let edge_u = smoothstep((1.0 - u.abs()) / 0.16);
            let edge_v = smoothstep((1.0 - v.abs()) / 0.16);
            let spike_h = det_exp(-u.abs() * 1.05) * det_exp(-v.abs() * 9.0) * edge_u;
            let spike_v = det_exp(-v.abs() * 1.05) * det_exp(-u.abs() * 9.0) * edge_v;
            (core * edge_u * edge_v + spike_h * 0.95 + spike_v * 0.95).clamp(0.0, 1.0)
        }
        SpriteId::Shard => {
            let d = u.abs().max(v.abs());
            if d < 0.85 { (1.0 - d / 0.85).powi(1) } else { 0.0 }
        }
        SpriteId::PuffAnim => puff_frame_alpha(u, v, 0) as f32 / 255.0,
        SpriteId::FireAnim => fire_frame(u, v, 0).3 as f32 / 255.0,
        // Lives on the fireball sheet, not in this grid.
        SpriteId::FireballAnim => 0.0,
    };
    (a.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// One puff-sheet frame. The FBM sample drifts with `frame` so consecutive frames differ
/// (the flipbook test fails if every frame is a copy).
fn puff_frame_alpha(u: f32, v: f32, frame: u32) -> u8 {
    puff_density(u, v, frame)
}

/// Ragged puff. Noise minus radius (a density field, not a disc) so overlapping cards
/// read as a volume. A soft core keeps the middle opaque when several puffs stack.
fn puff_density(u: f32, v: f32, frame: u32) -> u8 {
    let shift = frame as f32 * 0.41;
    let n = fbm(u * 1.55 + shift, v * 1.55 + shift * 0.47, 40 + frame as u64);
    let lobe = fbm(u * 0.65 + 1.8 + shift * 0.15, v * 0.65 - 0.35, 90 + frame as u64);
    let r = (u * u + v * v).sqrt();
    let field = n * 0.72 + lobe * 0.45 - r * 0.62;
    let shell = smoothstep((field + 0.04) / 0.28);
    let core = (1.0 - r * 1.05).clamp(0.0, 1.0).powi(2);
    // Always clear before the tile edge. A noise peak that reaches the border
    // cuts a square when the quad ends.
    let mask = smoothstep((0.94 - r) / 0.2);
    let a = (shell * 0.9 + core * 0.62).clamp(0.0, 1.0) * mask;
    (a * 255.0).round() as u8
}

/// One flame-sheet frame. `v = −1` is the top of the tile, so the tip points up.
///
/// Returns sRGB RGBA. Alpha is a soft teardrop density (wide base, pinched tip)
/// with FBM holes, so a stack of cards reads as a volume instead of paper shards.
/// RGB is the same value on every channel: a bright tongue and a darker fringe.
/// A coloured ramp here would multiply the particle tint and push green under the
/// fire-family floor. Four-octave value noise shifts with `frame`.
fn fire_frame(u: f32, v: f32, frame: u32) -> (u8, u8, u8, u8) {
    let up = (1.0 - v) * 0.5;
    let phase = frame as f32 * 0.37;
    let n = fbm(u * 2.6 + phase, up * 2.2 - phase * 0.5, 210 + u64::from(frame));
    let lobe = fbm(u * 1.3 - phase * 0.35, up * 1.6 + 1.7, 40 + u64::from(frame) * 5);
    let wobble = (n - 0.5) * (0.20 + up * 0.18);
    let half = 0.07 + (1.0 - up) * 0.52;
    let along = (u - wobble) / half.max(0.05);
    let base = smoothstep((up + 0.03) / 0.14);
    let tip = smoothstep((1.05 - up) / 0.20);
    // Wide soft fringe. The interior is noisy, not a solid polygon.
    let fringe = (1.0 - along.abs()).clamp(0.0, 1.0);
    let soft = fringe * fringe * (3.0 - 2.0 * fringe);
    let holes = (0.25 + n * 0.85 + lobe * 0.45).clamp(0.15, 1.0);
    let tongue = soft * holes * base * tip;
    // Hot tongue wide enough that the 5 px bloom taps still see luma above 1.
    // The fringe stays the noisy `tongue` term, so the card is not a solid shard.
    // Wide enough that several 5 px bloom taps land on luma above 1. The
    // fringe term still feathers the silhouette.
    let hot = det_exp(-along * along * 0.45) * smoothstep((0.90 - up) / 0.55) * base;
    let mask = smoothstep((0.93 - u.abs()) / 0.16) * smoothstep((0.93 - v.abs()) / 0.16);
    let alpha = (tongue * 0.50 + hot).clamp(0.0, 1.0) * mask;
    // Equal channels. 255 at the tongue, falling toward the fringe.
    let shade = (0.78 + hot * 0.22).clamp(0.0, 1.0);
    let byte = (shade * 255.0).round() as u8;
    (byte, byte, byte, (alpha * 255.0).round() as u8)
}

/// Temperature stops of the fireball sheet, linear RGB. Every saturated stop keeps
/// `r >= g >= b` and `g >= 0.42 r` (the fire family), so a grey tint cannot leave it.
/// Brightness rises with temperature (an emitter at ~1300 K is far dimmer than one at
/// ~1800 K), so only the core goes above 1 after the tint and blooms.
const FIREBALL_RAMP: [(f32, [f32; 3]); 5] = [
    (0.0, [0.06, 0.03, 0.012]),
    (0.22, [0.32, 0.14, 0.03]),
    (0.47, [0.62, 0.285, 0.053]),
    (0.72, [0.86, 0.58, 0.19]),
    (0.92, [1.0, 0.9, 0.6]),
];

/// The fireball sheet's temperature stops (linear RGB), coolest first.
#[cfg(test)]
pub(crate) fn fireball_ramp_stops() -> impl Iterator<Item = [f32; 3]> {
    FIREBALL_RAMP.iter().map(|stop| stop.1)
}

fn fireball_ramp(t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    let mut lo = FIREBALL_RAMP[0];
    for hi in FIREBALL_RAMP.iter().skip(1) {
        if t <= hi.0 {
            let k = ((t - lo.0) / (hi.0 - lo.0).max(1e-5)).clamp(0.0, 1.0);
            return [
                lo.1[0] + (hi.1[0] - lo.1[0]) * k,
                lo.1[1] + (hi.1[1] - lo.1[1]) * k,
                lo.1[2] + (hi.1[2] - lo.1[2]) * k,
            ];
        }
        lo = *hi;
    }
    lo.1
}

fn linear_to_srgb_byte(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        // The deterministic kernel: platform `powf` last digits would store different bytes.
        1.055 * det_powf_nonnegative(c, 1.0 / 2.4) - 0.055
    };
    (v * 255.0).round() as u8
}

/// One fireball-body frame: sRGB RGBA, premultiplied by the shader. `v = −1` is the top.
///
/// A whole billowing fireball. The silhouette is a domain-warped FBM density (Perlin's
/// "Hypertexture", SIGGRAPH 1989; warp after Quilez's public notes), so the edge is
/// lobed, not a disc. Flame tongues lick off the top: a vertically stretched FBM, gated
/// to the upper half and thresholded into separate licks, adds density above the body.
/// Temperature falls from the centre and follows the billows, with thin hot ridges
/// (`1 − |2n − 1|`) inside; soot folds (a second warped FBM above a threshold, kept off
/// the core) pull the temperature down to dark smoke curling into the flame. It maps
/// through [`FIREBALL_RAMP`]. Frames advance the noise phase.
fn fireball_frame(u: f32, v: f32, frame: u32) -> (u8, u8, u8, u8) {
    let phase = frame as f32 * 0.45;
    let seed = 700 + u64::from(frame);
    let wx = fbm_octaves(u * 1.1 + phase * 0.3, v * 1.1 + 3.1, 501, 5) - 0.5;
    let wy = fbm_octaves(u * 1.1 - 2.3, v * 1.1 - phase * 0.3, 502, 5) - 0.5;
    let pu = u + wx * 0.42;
    let pv = v + wy * 0.42;
    let r = (pu * pu + pv * pv).sqrt();
    let billow = fbm_octaves(pu * 1.5 + phase * 0.2, pv * 1.5 - phase * 0.15, seed, 6);
    let ridge_n = fbm_octaves(pu * 2.6 - phase * 0.4, pv * 2.6 + phase * 0.25, seed + 50, 6);
    let ridge = (1.0 - (2.0 * ridge_n - 1.0).abs()).powi(3);
    let body = (1.0 - r / 0.74) + (billow - 0.5) * 0.95;
    // Tongues: tall, narrow licks on the upper half, reaching toward the top of the tile.
    let lick_n = fbm_octaves(u * 3.4 + wx * 0.9 + 11.0, v * 0.85 + phase * 0.7, seed + 90, 5);
    let licks = ((lick_n - 0.46) * 3.2).clamp(0.0, 1.0);
    let up = smoothstep((-pv - 0.05) / 0.45);
    let tongue = up * (licks * 1.35 - (r - 0.5) * 1.3);
    let density = body.max(tongue);
    // Clear before the tile edge so the quad never cuts a square.
    let ru = (u * u + v * v).sqrt();
    let mask = smoothstep((0.98 - ru) / 0.2)
        * smoothstep((0.98 - u.abs()) / 0.1)
        * smoothstep((0.98 - v.abs()) / 0.1);
    let alpha = smoothstep(density / 0.36) * mask;
    let core = (1.0 - r / 0.68).clamp(0.0, 1.0);
    // The billows break up the rim; the middle stays one hot core.
    let body_temp = (core * 0.9
        + (billow - 0.5) * 0.75 * (1.0 - core * 0.55)
        + ridge * 0.22 * (1.0 - r).max(0.0)
        + density.min(0.6) * 0.3
        - 0.06)
        .clamp(0.0, 1.0);
    // A tongue burns orange-yellow, hotter at its root.
    let tongue_temp = (0.38 + licks * 0.22 + (1.0 - r) * 0.2).clamp(0.0, 1.0);
    let in_tongue = smoothstep((tongue - body) / 0.15);
    let mut temperature = body_temp + (tongue_temp - body_temp) * in_tongue;
    // Soot folds curling into the flame from the rim, never over the core.
    let soot_n = fbm_octaves(
        pu * 2.1 + phase * 0.25 + 9.0,
        pv * 2.1 - phase * 0.3,
        seed + 33,
        6,
    );
    let folds = smoothstep((soot_n - 0.53) / 0.1) * smoothstep((r - 0.3) / 0.25);
    temperature *= 1.0 - 0.85 * folds;
    let rgb = fireball_ramp(temperature);
    (
        linear_to_srgb_byte(rgb[0]),
        linear_to_srgb_byte(rgb[1]),
        linear_to_srgb_byte(rgb[2]),
        (alpha * 255.0).round() as u8,
    )
}

/// Deterministic 2-D value noise: PCG-hashed lattice, smoothstep-interpolated, 4 octaves.
fn fbm(u: f32, v: f32, seed: u64) -> f32 {
    fbm_octaves(u, v, seed, 4)
}

/// [`fbm`] with `octaves` octaves (more detail for the 256 px fireball tiles).
fn fbm_octaves(u: f32, v: f32, seed: u64, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut freq = 2.4;
    for _ in 0..octaves {
        sum += value_noise(u * freq, v * freq, seed) * amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum
}

fn value_noise(x: f32, y: f32, seed: u64) -> f32 {
    let x0 = x.floor();
    let y0 = y.floor();
    let fx = smoothstep(x - x0);
    let fy = smoothstep(y - y0);
    let c = |ix: i32, iy: i32| -> f32 {
        let mut rng = Rng::with_stream(seed, (ix as i64 as u64) ^ ((iy as i64 as u64) << 32));
        rng.f32()
    };
    let x0i = x0 as i32;
    let y0i = y0 as i32;
    let a = c(x0i, y0i);
    let b = c(x0i + 1, y0i);
    let d = c(x0i, y0i + 1);
    let e = c(x0i + 1, y0i + 1);
    let top = a + (b - a) * fx;
    let bottom = d + (e - d) * fx;
    top + (bottom - top) * fy
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pixel distance authored on the 64 px tiles, at the current tile size.
    fn s(px64: u32) -> u32 {
        px64 * TILE / 64
    }

    #[test]
    fn atlas_is_sized_and_deterministic() {
        let a = generate_atlas();
        assert_eq!(a.len(), (ATLAS_SIZE * ATLAS_HEIGHT * 4) as usize);
        let b = build_atlas();
        assert_eq!(a, b, "atlas generation is byte-stable");
        let mips = fireball_sheet_mips();
        assert_eq!(mips.len() as u32, FIREBALL_MIP_LEVELS);
        assert_eq!(mips[0], build_fireball_sheet(), "fireball sheet is byte-stable");
        for (level, data) in mips.iter().enumerate() {
            let (w, h) = (FIREBALL_SHEET_WIDTH >> level, FIREBALL_SHEET_HEIGHT >> level);
            assert_eq!(data.len(), (w * h * 4) as usize, "mip {level}");
        }
        // Byte-stable across platforms, not just within this process: every transcendental in
        // the generator runs on the deterministic kernels (det_exp / det_powf_nonnegative), so a committed
        // FNV-1a hash pins all atlas and sheet bytes. Update the constants only with a reason
        // in the commit message (the values are the contract; never bless silently).
        assert_eq!(fnv1a(&a), ATLAS_FNV1A, "64 px atlas bytes changed");
        assert_eq!(
            fnv1a(&mips[0]),
            FIREBALL_SHEET_FNV1A,
            "fireball sheet bytes changed"
        );
    }

    /// FNV-1a over bytes (stable, cheap, public-domain algorithm).
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash
    }

    // Pinned hashes of the deterministic generator output (see the test above).
    const ATLAS_FNV1A: u64 = 0xfbc435ffd3145e5a; // pinned 2026-11-09 (deterministic-kernel generator)
    const FIREBALL_SHEET_FNV1A: u64 = 0x4dfc4e32af6c2b31; // pinned 2026-11-09

    /// The shader addresses an 8×4 grid (`tile % 8`, `tile / 8`). Every identity tile and
    /// every puff frame must have opaque content in the band the GPU samples.
    #[test]
    fn shader_row_addressing_hits_every_tile() {
        let a = generate_atlas();
        for tile in 0..(FIRE_ANIM_TILE + FIRE_ANIM_FRAMES) {
            let x0 = (tile % TILES_X) * TILE;
            let y0 = (tile / TILES_X) * TILE;
            let mut max_alpha = 0u8;
            for py in (0..TILE).step_by(2) {
                for px in (0..TILE).step_by(2) {
                    let idx = (((y0 + py) * ATLAS_SIZE + x0 + px) * 4 + 3) as usize;
                    max_alpha = max_alpha.max(a[idx]);
                }
            }
            assert!(
                max_alpha > 100,
                "tile {tile} never sampled by the shader (max alpha {max_alpha})"
            );
        }
    }

    #[test]
    fn soft_disc_tile_has_soft_edges_and_a_bright_centre() {
        // Centre pixel of tile 0 is opaque, corner is transparent.
        let a = generate_atlas();
        let centre = px(&a, 0, TILE / 2, TILE / 2);
        let corner = px(&a, 0, 1, 1);
        assert!(centre > 240, "center alpha {centre}");
        assert_eq!(corner, 0, "corner alpha {corner}");
    }

    #[test]
    fn spark_tile_is_wider_than_tall() {
        let a = generate_atlas();
        let mid = px(&a, 3, TILE / 2, TILE / 2); // centre
        let off_y = px(&a, 3, TILE / 2, TILE / 2 - s(6)); // 6 px above centre
        let off_x = px(&a, 3, TILE / 2 + s(20), TILE / 2); // 20 px right of centre
        assert!(off_x > off_y, "streak extends horizontally ({off_x} vs {off_y})");
        assert!(mid > 120, "center alpha {mid}");
    }

    #[test]
    fn puff_card_is_clear_at_the_tile_edge() {
        let a = generate_atlas();
        let tile = PUFF_ANIM_TILE;
        let corner = px(&a, tile, 1, 1);
        let edge = px(&a, tile, TILE - 2, TILE / 2);
        assert_eq!(corner, 0, "puff corner");
        assert!(edge < 8, "puff edge {edge} cuts a square card");
    }

    #[test]
    fn puff_flipbook_frames_are_not_copies() {
        let a = generate_atlas();
        let mut prev = Vec::new();
        for frame in 0..PUFF_ANIM_FRAMES {
            let tile = PUFF_ANIM_TILE + frame;
            let mut bytes = Vec::new();
            for y in (0..TILE).step_by(4) {
                for x in (0..TILE).step_by(4) {
                    bytes.push(px(&a, tile, x, y));
                }
            }
            if !prev.is_empty() {
                assert_ne!(bytes, prev, "puff frame {frame} matches the previous frame");
            }
            let sum: u32 = bytes.iter().map(|b| *b as u32).sum();
            assert!(sum > 100, "puff frame {frame} is empty");
            prev = bytes;
        }
    }

    #[test]
    fn flare_spikes_reach_the_edge_and_the_diagonal_stays_dark() {
        let a = generate_atlas();
        let centre = px(&a, 5, TILE / 2, TILE / 2);
        // A few pixels in from the right edge, on the horizontal arm.
        // TILE-5 is still on the arm; TILE-1 must be clear or the quad cuts a square.
        let arm = px(&a, 5, TILE - s(6), TILE / 2);
        let tip = px(&a, 5, TILE - 1, TILE / 2);
        let diag = px(&a, 5, TILE - s(8), s(8));
        assert!(centre > 200, "flare core {centre}");
        assert!(arm > 50, "flare arm fades before the tile edge ({arm})");
        assert!(tip < 12, "flare tip is a hard quad edge ({tip})");
        assert!(
            arm > diag + 25,
            "flare arm {arm} does not outrun the diagonal {diag}"
        );
    }

    #[test]
    fn fire_sheet_is_a_flame_not_a_disc_and_frames_differ() {
        let a = generate_atlas();
        let tile = FIRE_ANIM_TILE;
        assert_eq!(px(&a, tile, 1, 1), 0, "flame corner cuts a square");
        assert!(px(&a, tile, TILE - 2, TILE / 2) < 8, "flame side edge");
        // Base (low in the image, v > 0) is wider than the tip (high in the image).
        let mut base_span = 0u32;
        let mut tip_span = 0u32;
        let base_y = TILE - s(16);
        let tip_y = s(10);
        for x in 0..TILE {
            if px(&a, tile, x, base_y) > 40 {
                base_span += 1;
            }
            if px(&a, tile, x, tip_y) > 40 {
                tip_span += 1;
            }
        }
        assert!(
            base_span > tip_span + s(6),
            "tip {tip_span}px is not narrower than the base {base_span}px"
        );
        assert!(px(&a, tile, TILE / 2, base_y) > 80, "flame base is empty");
        let mut prev = Vec::new();
        for frame in 0..FIRE_ANIM_FRAMES {
            let mut bytes = Vec::new();
            for y in (0..TILE).step_by(4) {
                for x in (0..TILE).step_by(4) {
                    bytes.push(px(&a, FIRE_ANIM_TILE + frame, x, y));
                }
            }
            if !prev.is_empty() {
                assert_ne!(bytes, prev, "fire frame {frame} matches the previous frame");
            }
            let sum: u32 = bytes.iter().map(|b| *b as u32).sum();
            assert!(sum > 80, "fire frame {frame} is empty");
            prev = bytes;
        }
    }

    fn sheet_rgba(sheet: &[u8], frame: u32, x: u32, y: u32) -> [u8; 4] {
        let tx = frame * FIREBALL_TILE + x.min(FIREBALL_TILE - 1);
        let ty = y.min(FIREBALL_TILE - 1);
        let i = ((ty * FIREBALL_SHEET_WIDTH + tx) * 4) as usize;
        [sheet[i], sheet[i + 1], sheet[i + 2], sheet[i + 3]]
    }

    fn lin(b: u8) -> f32 {
        let c = f32::from(b) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Each fireball frame is one body: opaque, white-yellow core; orange-to-soot rim; lobed
    /// (not round) silhouette; clear tile edge; every pixel in the fire family; frames differ.
    #[test]
    fn fireball_sheet_is_one_hot_body_with_a_cool_lobed_rim() {
        let a = fireball_sheet_mips()[0].clone();
        if let Ok(dir) = regia_core::brand::env_var("REGIA_VFX_TEST_DUMP") {
            let _ = std::fs::create_dir_all(&dir);
            let path = std::path::Path::new(&dir).join("fireball_sheet.png");
            if let Ok(file) = std::fs::File::create(path) {
                let mut enc = png::Encoder::new(
                    std::io::BufWriter::new(file),
                    FIREBALL_SHEET_WIDTH,
                    FIREBALL_SHEET_HEIGHT,
                );
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                if let Ok(mut w) = enc.write_header() {
                    let _ = w.write_image_data(&a);
                }
            }
        }
        let mut prev = Vec::new();
        for frame in 0..FIREBALL_ANIM_FRAMES {
            let tile = frame;
            assert_eq!(sheet_rgba(&a, tile, 0, 0)[3], 0, "frame {frame} corner");
            assert!(
                sheet_rgba(&a, tile, FIREBALL_TILE - 1, FIREBALL_TILE / 2)[3] < 8,
                "frame {frame} side edge"
            );
            let c = sheet_rgba(&a, tile, FIREBALL_TILE / 2, FIREBALL_TILE / 2);
            assert!(c[3] > 230, "frame {frame} core alpha {}", c[3]);
            let core_gr = lin(c[1]) / lin(c[0]).max(1e-4);
            // Rim: mean green/red of the opaque pixels in the outer third.
            let mut rim_gr = 0.0f32;
            let mut rim_n = 0.0f32;
            let mut radii = [0.0f32; 16];
            for y in 0..FIREBALL_TILE {
                for x in 0..FIREBALL_TILE {
                    let p = sheet_rgba(&a, tile, x, y);
                    let rgb = [lin(p[0]), lin(p[1]), lin(p[2])];
                    if p[3] > 24 {
                        let max = rgb[0].max(rgb[1]).max(rgb[2]);
                        let sat = if max > 1e-4 {
                            (max - rgb[0].min(rgb[1]).min(rgb[2])) / max
                        } else {
                            0.0
                        };
                        assert!(
                            sat < 0.18
                                || (rgb[0] >= rgb[1] && rgb[1] >= rgb[2] * 0.85 && rgb[1] >= rgb[0] * 0.35),
                            "frame {frame} pixel ({x}, {y}) {rgb:?} leaves the fire family"
                        );
                        let du = (x as f32 + 0.5) / FIREBALL_TILE as f32 * 2.0 - 1.0;
                        let dv = (y as f32 + 0.5) / FIREBALL_TILE as f32 * 2.0 - 1.0;
                        let r = (du * du + dv * dv).sqrt();
                        if r > 0.5 && p[3] > 128 {
                            rim_gr += rgb[1] / rgb[0].max(1e-4);
                            rim_n += 1.0;
                        }
                        let k = ((dv.atan2(du) + std::f32::consts::PI) / std::f32::consts::TAU * 16.0)
                            as usize
                            % 16;
                        if p[3] > 128 {
                            radii[k] = radii[k].max(r);
                        }
                    }
                }
            }
            let rim_gr = rim_gr / rim_n.max(1.0);
            assert!(
                core_gr > rim_gr + 0.15,
                "frame {frame}: core g/r {core_gr:.2} rim {rim_gr:.2}"
            );
            let mean = radii.iter().sum::<f32>() / 16.0;
            let spread = radii.iter().fold(0.0f32, |m, r| m.max((r - mean).abs()));
            assert!(
                spread > 0.06,
                "frame {frame} silhouette is a disc (radii {radii:?})"
            );
            let mut bytes = Vec::new();
            for y in (0..FIREBALL_TILE).step_by(4) {
                for x in (0..FIREBALL_TILE).step_by(4) {
                    bytes.push(sheet_rgba(&a, tile, x, y)[3]);
                }
            }
            if !prev.is_empty() {
                assert_ne!(bytes, prev, "fireball frame {frame} matches the previous frame");
            }
            prev = bytes;
        }
    }

    /// The 256 px fireball frames carry detail a 64 px tile cannot: the mean difference
    /// between level 0 and level 2 (64 px tiles) brought back up is well above zero.
    #[test]
    fn fireball_tiles_hold_detail_finer_than_64_px() {
        let mips = fireball_sheet_mips();
        let (full, coarse) = (&mips[0], &mips[2]);
        let coarse_w = FIREBALL_SHEET_WIDTH >> 2;
        let mut diff = 0.0f32;
        let mut n = 0.0f32;
        for fy in (0..FIREBALL_SHEET_HEIGHT).step_by(2) {
            for fx in (0..FIREBALL_SHEET_WIDTH).step_by(2) {
                let i = ((fy * FIREBALL_SHEET_WIDTH + fx) * 4) as usize;
                let j = (((fy >> 2) * coarse_w + (fx >> 2)) * 4) as usize;
                if full[i + 3] < 128 {
                    continue;
                }
                let l0 = 0.2126 * lin(full[i]) + 0.7152 * lin(full[i + 1]) + 0.0722 * lin(full[i + 2]);
                let l2 = 0.2126 * lin(coarse[j]) + 0.7152 * lin(coarse[j + 1]) + 0.0722 * lin(coarse[j + 2]);
                diff += (l0 - l2).abs();
                n += 1.0;
            }
        }
        let mean = diff / n.max(1.0);
        assert!(
            mean > 0.02,
            "the 256 px fireball frames hold no finer detail ({mean:.4})"
        );
    }

    #[test]
    fn every_tile_has_content() {
        let a = generate_atlas();
        for tile in 0..7u32 {
            let mut sum = 0u32;
            for y in (8..TILE).step_by(8) {
                for x in (8..TILE).step_by(8) {
                    sum += px(&a, tile, x, y) as u32;
                }
            }
            assert!(sum > 0, "tile {tile} is empty");
        }
    }

    fn px(atlas: &[u8], tile: u32, x: u32, y: u32) -> u8 {
        let tx = (tile % TILES_X) * TILE + x.min(TILE - 1);
        let ty = (tile / TILES_X) * TILE + y.min(TILE - 1);
        atlas[((ty * ATLAS_SIZE + tx) * 4 + 3) as usize]
    }
}
