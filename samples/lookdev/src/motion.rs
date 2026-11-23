// SPDX-License-Identifier: MIT-0
//! Image quality in motion (`do27_chase --motion … --seq-dir …`).
//!
//! Every look judgement before this lane was made on still frames. This module reads back
//! each frame of a chase run, writes the frames and strips of crops, and scores the
//! sequence with temporal metrics (8-bit sRGB-coded Rec. 709 luma, 0–255):
//!
//! - **flicker**: mean |ΔL| between consecutive frames. On a held camera and a static scene
//!   everything above zero is temporal instability (jitter shimmer, specular aliasing,
//!   SSAO/SSGI noise, shadow crawl).
//! - **clarity**: the last moving frame's gradient energy over that of the same pose held
//!   until the history converged (spec 27 § 2.3 "motion clarity", a gradient-energy stand-in
//!   for MTF50). Below 1 is blur under motion.
//! - **smear**: mean |L_moving − L_converged| on the last pose (ghost trails, smearing).
//! - **rotation flicker**: when two frames share the eye (a pan), the earlier one is warped
//!   onto the later one through the rotation (a homography: no parallax) and the mean
//!   |ΔL| is taken over the pixels both see. A stable image scores only the resampling
//!   difference; shimmer, crawling and sparkle add to it. Compare runs, not absolutes.
//! - **pops**: per 64 px tile, frames whose frame-to-frame difference spikes above three times
//!   the median of the nine frames around it (LOD switches, fade pops, cascade swaps).

use std::path::PathBuf;

use serde::Serialize;

use crate::LookdevError;
use crate::default_look::write_png;

/// How the camera moves during a chase run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    /// The chase camera 9 m behind the vehicle (the bench).
    Chase,
    /// A plate camera yawing about its eye (a slow pan over cliffs). No vehicle.
    Pan {
        /// The plate camera the pan starts from.
        plate: crate::look_r2::R2Shot,
        /// Yaw per frame, degrees (positive turns left).
        deg_per_frame: f32,
    },
    /// The chase camera orbiting the moving vehicle (a fast turn past the scatter).
    Turn {
        /// Orbit per frame, degrees.
        deg_per_frame: f32,
    },
    /// The start pose's camera moved straight along its heading at a fixed height and
    /// pitch: nothing bobs, so a spike in a tile is a LOD, fade or cascade transition.
    Dolly {
        /// Metres per frame.
        m_per_frame: f32,
    },
    /// A fight position: the vehicle parked at the start, the chase camera (9 m back, 3.5 m
    /// up) orbiting it.
    Orbit {
        /// Orbit per frame, degrees.
        deg_per_frame: f32,
    },
    /// A fly-over: the eye `height_m` above the route's ground, moving along the route at
    /// `m_per_frame`, heading toward the route 150 m on and pitched 15° down (the horizon,
    /// the map's far field and its scatter in view). The vehicle drives below at the chase speed.
    Flyover {
        /// Eye height above the ground under it, metres.
        height_m: f32,
        /// Metres per frame along the route.
        m_per_frame: f32,
    },
}

/// Per-frame capture of a run.
#[derive(Debug, Clone)]
pub struct SeqOptions {
    /// Output directory (frames, strips, `motion.json`).
    pub dir: PathBuf,
    /// Write every n-th measured frame as a PNG (0 = none).
    pub every: u32,
    /// Frames the last pose is held for the converged reference.
    pub converge: u32,
    /// Crops `[x, y, w, h]` in output pixels; one strip per crop (frames left to right).
    pub crops: Vec<[u32; 4]>,
    /// Frames per strip.
    pub strip_frames: u32,
}

/// Temporal metrics of one captured run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MotionReport {
    /// Frames measured.
    pub frames: u32,
    /// Mean |ΔL| between consecutive frames (whole frame), 8-bit levels.
    pub flicker_mean: f32,
    /// 99th percentile over frames of the per-frame mean |ΔL|.
    pub flicker_p99: f32,
    /// Fraction of pixels whose |ΔL| exceeds 8 levels, mean over frame pairs.
    pub flicker_px_over_8: f32,
    /// Mean |ΔL| after warping the previous frame through the camera rotation, over the
    /// frame pairs that share an eye (0 when none do).
    pub rotation_flicker: f32,
    /// Frame pairs scored for [`Self::rotation_flicker`].
    pub rotation_pairs: u32,
    /// Gradient energy of the last moving frame over the converged still.
    pub clarity: f32,
    /// Mean |L_moving − L_converged| on the last pose.
    pub smear: f32,
    /// Gradient energy of the converged still over the native un-jittered frame (no TAA)
    /// of the same pose: how soft the temporal resolve is at rest.
    pub still_vs_native: f32,
    /// Gradient energy of the last moving frame over the native frame (spec 27 § 2.3
    /// motion clarity, target ≥ 0.85).
    pub moving_vs_native: f32,
    /// Per-crop clarity and smear (same order as the crops).
    pub crops: Vec<CropReport>,
    /// Temporal outliers over all 64 px tiles.
    pub pops: u32,
    /// Tile-frames scored for pops.
    pub pop_samples: u32,
    /// The strongest pops (at most 24), strongest first.
    pub pop_events: Vec<PopEvent>,
}

/// One temporal outlier of a tile.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PopEvent {
    /// Measured frame whose difference to the frame before spiked.
    pub frame: u32,
    /// Tile rectangle `[x, y, w, h]`.
    pub tile: [u32; 4],
    /// Mean |ΔL| in the tile at that frame.
    pub diff: f32,
    /// That over the median of the frames around it.
    pub ratio: f32,
}

/// Clarity and smear inside one crop.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CropReport {
    /// `[x, y, w, h]`.
    pub rect: [u32; 4],
    /// Gradient energy ratio (moving / converged).
    pub clarity: f32,
    /// Mean |L_moving − L_converged|.
    pub smear: f32,
    /// Converged still over native (no TAA) inside the crop.
    pub still_vs_native: f32,
    /// Moving over native inside the crop.
    pub moving_vs_native: f32,
    /// Mean |ΔL| between consecutive frames inside the crop.
    pub flicker: f32,
}

/// One RGBA8 frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Tightly packed RGBA8 (sRGB-coded).
    pub pixels: Vec<u8>,
}

/// Rec. 709 luma of sRGB-coded RGBA8, 0–255.
#[must_use]
pub fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

fn rect_or_full(rect: Option<[u32; 4]>, w: u32, h: u32) -> [u32; 4] {
    let [x, y, rw, rh] = rect.unwrap_or([0, 0, w, h]);
    let x = x.min(w.saturating_sub(1));
    let y = y.min(h.saturating_sub(1));
    [x, y, rw.min(w - x), rh.min(h - y)]
}

/// Mean |a − b| over `rect` (the whole image when `None`).
#[must_use]
pub fn mean_abs_diff(a: &[f32], b: &[f32], w: u32, h: u32, rect: Option<[u32; 4]>) -> f32 {
    let [x0, y0, rw, rh] = rect_or_full(rect, w, h);
    let mut sum = 0.0f64;
    for y in y0..y0 + rh {
        let row = (y * w) as usize;
        for x in x0..x0 + rw {
            let i = row + x as usize;
            sum += f64::from((a[i] - b[i]).abs());
        }
    }
    (sum / f64::from((rw * rh).max(1))) as f32
}

/// Mean squared forward-difference gradient over `rect` (the whole image when `None`).
#[must_use]
pub fn gradient_energy(l: &[f32], w: u32, h: u32, rect: Option<[u32; 4]>) -> f32 {
    let [x0, y0, rw, rh] = rect_or_full(rect, w, h);
    let mut sum = 0.0f64;
    let mut n = 0u64;
    for y in y0..(y0 + rh).min(h - 1) {
        let row = (y * w) as usize;
        for x in x0..(x0 + rw).min(w - 1) {
            let i = row + x as usize;
            let dx = l[i + 1] - l[i];
            let dy = l[i + w as usize] - l[i];
            sum += f64::from(dx * dx + dy * dy);
            n += 1;
        }
    }
    (sum / n.max(1) as f64) as f32
}

/// Spikes in one tile's frame-to-frame difference series: `d[t]` above three times the
/// median of `d[t-4..=t+4]` (without `d[t]`) and above `floor` levels.
#[must_use]
pub fn count_pops(series: &[f32], floor: f32) -> u32 {
    pop_indices(series, floor).len() as u32
}

/// The spikes of [`count_pops`] as `(t, d[t] / median)`.
#[must_use]
pub fn pop_indices(series: &[f32], floor: f32) -> Vec<(usize, f32)> {
    let mut pops = Vec::new();
    for t in 0..series.len() {
        let lo = t.saturating_sub(4);
        let hi = (t + 5).min(series.len());
        let mut around: Vec<f32> = (lo..hi).filter(|&i| i != t).map(|i| series[i]).collect();
        if around.len() < 4 {
            continue;
        }
        around.sort_by(f32::total_cmp);
        let median = around[around.len() / 2];
        if series[t] > floor && series[t] > 3.0 * median {
            pops.push((t, series[t] / median.max(1e-3)));
        }
    }
    pops
}

const TILE: u32 = 64;

/// A frame's camera (world metres, vertical field of view in radians).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Eye.
    pub eye: [f32; 3],
    /// Look-at point.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
}

impl Camera {
    /// Right, up and forward unit vectors (world +Y up).
    fn basis(&self) -> [glam::Vec3; 3] {
        let f = (glam::Vec3::from_array(self.target) - glam::Vec3::from_array(self.eye)).normalize_or_zero();
        let r = f.cross(glam::Vec3::Y).normalize_or_zero();
        [r, r.cross(f), f]
    }
}

/// Warps `prev` (seen by `from`) into the view of `to` — the same eye, rotated — and
/// returns the mean |cur − warped| over the pixels both views see.
#[must_use]
pub fn rotation_flicker(cur: &[f32], prev: &[f32], w: u32, h: u32, to: Camera, from: Camera) -> Option<f32> {
    rotation_flicker_into(cur, prev, w, h, to, from, None)
}

/// [`rotation_flicker`], also adding each pixel's |cur − warped| into `map` (`w`×`h`).
#[must_use]
pub fn rotation_flicker_into(
    cur: &[f32],
    prev: &[f32],
    w: u32,
    h: u32,
    to: Camera,
    from: Camera,
    mut map: Option<&mut [f32]>,
) -> Option<f32> {
    let tan_y = (to.fov_y * 0.5).tan();
    let tan_x = tan_y * w as f32 / h.max(1) as f32;
    let [r1, u1, f1] = to.basis();
    let [r0, u0, f0] = from.basis();
    let mut sum = 0.0f64;
    let mut n = 0u64;
    for y in 0..h {
        for x in 0..w {
            let sx = (2.0 * (x as f32 + 0.5) / w as f32 - 1.0) * tan_x;
            let sy = (1.0 - 2.0 * (y as f32 + 0.5) / h as f32) * tan_y;
            let ray = r1 * sx + u1 * sy + f1;
            let c = ray.dot(f0);
            if c <= 1e-4 {
                continue;
            }
            let px = ((ray.dot(r0) / (c * tan_x)) + 1.0) * 0.5 * w as f32 - 0.5;
            let py = (1.0 - ray.dot(u0) / (c * tan_y)) * 0.5 * h as f32 - 0.5;
            // Two pixels in from the border so the bilinear fetch has its four taps.
            if px < 1.0 || py < 1.0 || px > w as f32 - 3.0 || py > h as f32 - 3.0 {
                continue;
            }
            let (x0, y0) = (px.floor() as usize, py.floor() as usize);
            let (tx, ty) = (px - px.floor(), py - py.floor());
            let at = |xx: usize, yy: usize| prev[yy * w as usize + xx];
            let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1, y0) * tx;
            let bottom = at(x0, y0 + 1) * (1.0 - tx) + at(x0 + 1, y0 + 1) * tx;
            let warped = top * (1.0 - ty) + bottom * ty;
            let d = (cur[(y * w + x) as usize] - warped).abs();
            if let Some(map) = map.as_deref_mut() {
                map[(y * w + x) as usize] += d;
            }
            sum += f64::from(d);
            n += 1;
        }
    }
    (n > 0).then(|| (sum / n as f64) as f32)
}

/// Collects a run's frames and scores them.
pub struct Recorder {
    options: SeqOptions,
    width: u32,
    height: u32,
    prev: Option<Vec<f32>>,
    prev_camera: Option<Camera>,
    rotation: Vec<f32>,
    rotation_map: Vec<f32>,
    frame_diffs: Vec<f32>,
    over_8: Vec<f32>,
    crop_flicker: Vec<f32>,
    tiles: Vec<Vec<f32>>,
    strips: Vec<Vec<u8>>,
    strip_count: u32,
    measured: u32,
    last: Option<Frame>,
}

impl Recorder {
    /// A recorder writing into `options.dir` (created).
    ///
    /// # Errors
    ///
    /// The directory cannot be created.
    pub fn new(options: SeqOptions, width: u32, height: u32) -> Result<Self, LookdevError> {
        std::fs::create_dir_all(&options.dir).map_err(|source| LookdevError::Io {
            path: options.dir.display().to_string(),
            source,
        })?;
        let tiles = (width.div_ceil(TILE) * height.div_ceil(TILE)) as usize;
        let crops = options.crops.len();
        Ok(Self {
            options,
            width,
            height,
            prev: None,
            prev_camera: None,
            rotation: Vec::new(),
            rotation_map: Vec::new(),
            frame_diffs: Vec::new(),
            over_8: Vec::new(),
            crop_flicker: vec![0.0; crops],
            tiles: vec![Vec::new(); tiles],
            strips: vec![Vec::new(); crops],
            strip_count: 0,
            measured: 0,
            last: None,
        })
    }

    /// Scores and (every n-th) writes measured frame `index`, seen by `camera`.
    ///
    /// # Errors
    ///
    /// A PNG cannot be written.
    pub fn push(&mut self, index: u32, frame: Frame, camera: Camera) -> Result<(), LookdevError> {
        let _span = tracing::info_span!("motion_record").entered();
        let (w, h) = (self.width, self.height);
        if frame.width != w || frame.height != h {
            return Err(LookdevError::Config(format!(
                "frame {index} is {}x{}, the run is {w}x{h}",
                frame.width, frame.height
            )));
        }
        let l = luma(&frame.pixels);
        if let (Some(prev), Some(from)) = (self.prev.as_ref(), self.prev_camera)
            && from.eye == camera.eye
            && from.fov_y == camera.fov_y
            && let Some(d) = {
                self.rotation_map.resize((w * h) as usize, 0.0);
                rotation_flicker_into(&l, prev, w, h, camera, from, Some(&mut self.rotation_map))
            }
        {
            self.rotation.push(d);
        }
        self.prev_camera = Some(camera);
        if let Some(prev) = self.prev.as_ref() {
            self.frame_diffs.push(mean_abs_diff(&l, prev, w, h, None));
            let over = l.iter().zip(prev).filter(|(a, b)| (*a - *b).abs() > 8.0).count();
            self.over_8.push(over as f32 / l.len().max(1) as f32);
            for (c, rect) in self.options.crops.iter().enumerate() {
                self.crop_flicker[c] += mean_abs_diff(&l, prev, w, h, Some(*rect));
            }
            let tx = w.div_ceil(TILE);
            for (t, series) in self.tiles.iter_mut().enumerate() {
                let (x, y) = ((t as u32 % tx) * TILE, (t as u32 / tx) * TILE);
                series.push(mean_abs_diff(&l, prev, w, h, Some([x, y, TILE, TILE])));
            }
        }
        if self.options.every > 0 && index.is_multiple_of(self.options.every) {
            let path = self.options.dir.join(format!("frame_{index:04}.png"));
            write_png(&path, w, h, &frame.pixels)?;
        }
        if self.strip_count < self.options.strip_frames {
            for (strip, rect) in self.strips.iter_mut().zip(&self.options.crops) {
                append_crop(strip, &frame, *rect);
            }
            self.strip_count += 1;
        }
        self.prev = Some(l);
        self.measured += 1;
        self.last = Some(frame);
        Ok(())
    }

    /// Writes the strips and `motion.json` and returns the scores against the converged
    /// still of the last pose and its native (no TAA) frame.
    ///
    /// # Errors
    ///
    /// A PNG or the JSON cannot be written.
    pub fn finish(self, converged: &Frame, native: &Frame) -> Result<MotionReport, LookdevError> {
        let (w, h) = (self.width, self.height);
        let dir = self.options.dir.clone();
        let mut report = MotionReport {
            frames: self.measured,
            ..MotionReport::default()
        };
        let pairs = self.frame_diffs.len().max(1) as f32;
        report.flicker_mean = self.frame_diffs.iter().sum::<f32>() / pairs;
        report.flicker_p99 = crate::chase::percentile(&self.frame_diffs, 0.99);
        report.flicker_px_over_8 = self.over_8.iter().sum::<f32>() / pairs;
        report.rotation_pairs = self.rotation.len() as u32;
        if !self.rotation.is_empty() {
            // Where the pan shimmers: mean |ΔL| per pixel, 8 levels = white.
            let pairs = self.rotation.len() as f32;
            let heat: Vec<u8> = self
                .rotation_map
                .iter()
                .flat_map(|d| {
                    let v = (d / pairs / 8.0 * 255.0).clamp(0.0, 255.0) as u8;
                    [v, v, v, 255]
                })
                .collect();
            write_png(&dir.join("rotation_flicker.png"), w, h, &heat)?;
        }
        report.rotation_flicker = self.rotation.iter().sum::<f32>() / self.rotation.len().max(1) as f32;
        let still = luma(&converged.pixels);
        let base = luma(&native.pixels);
        write_png(&dir.join("converged.png"), w, h, &converged.pixels)?;
        write_png(&dir.join("native.png"), w, h, &native.pixels)?;
        let ratio = |a: &[f32], b: &[f32], rect: Option<[u32; 4]>| {
            gradient_energy(a, w, h, rect) / gradient_energy(b, w, h, rect).max(1e-6)
        };
        report.still_vs_native = ratio(&still, &base, None);
        if let Some(last) = self.last.as_ref() {
            write_png(&dir.join("last_moving.png"), w, h, &last.pixels)?;
            let moving = luma(&last.pixels);
            report.clarity = ratio(&moving, &still, None);
            report.moving_vs_native = ratio(&moving, &base, None);
            report.smear = mean_abs_diff(&moving, &still, w, h, None);
            for (c, rect) in self.options.crops.iter().enumerate() {
                report.crops.push(CropReport {
                    rect: *rect,
                    clarity: ratio(&moving, &still, Some(*rect)),
                    smear: mean_abs_diff(&moving, &still, w, h, Some(*rect)),
                    still_vs_native: ratio(&still, &base, Some(*rect)),
                    moving_vs_native: ratio(&moving, &base, Some(*rect)),
                    flicker: self.crop_flicker[c] / pairs,
                });
            }
        }
        let tx = w.div_ceil(TILE);
        for (t, series) in self.tiles.iter().enumerate() {
            let pops = pop_indices(series, 2.0);
            report.pops += pops.len() as u32;
            report.pop_samples += series.len() as u32;
            let (x, y) = ((t as u32 % tx) * TILE, (t as u32 / tx) * TILE);
            for (i, ratio) in pops {
                report.pop_events.push(PopEvent {
                    // `series[i]` compares measured frame i + 1 with frame i.
                    frame: i as u32 + 1,
                    tile: [x, y, TILE.min(w - x), TILE.min(h - y)],
                    diff: series[i],
                    ratio,
                });
            }
        }
        report.pop_events.sort_by(|a, b| b.ratio.total_cmp(&a.ratio));
        report.pop_events.truncate(24);
        for (c, rect) in self.options.crops.iter().enumerate() {
            let strip_w = rect[2] * self.strip_count;
            let path = dir.join(format!("strip_{c}.png"));
            let pixels = strip_rows(&self.strips[c], rect[2], rect[3], self.strip_count);
            if !pixels.is_empty() {
                write_png(&path, strip_w, rect[3], &pixels)?;
            }
        }
        let json = serde_json::to_string_pretty(&report).map_err(|e| LookdevError::Json(e.to_string()))?;
        let path = dir.join("motion.json");
        std::fs::write(&path, json).map_err(|source| LookdevError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(report)
    }
}

/// Appends crop `rect` of `frame` (rows top to bottom) to `out`.
fn append_crop(out: &mut Vec<u8>, frame: &Frame, rect: [u32; 4]) {
    let [x, y, w, h] = rect;
    for row in 0..h {
        for col in 0..w {
            let (px, py) = (x + col, y + row);
            if px < frame.width && py < frame.height {
                let i = ((py * frame.width + px) * 4) as usize;
                out.extend_from_slice(&frame.pixels[i..i + 4]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
}

/// Lays `count` crops of `w`×`h` (stored one after another) side by side.
fn strip_rows(crops: &[u8], w: u32, h: u32, count: u32) -> Vec<u8> {
    let crop_len = (w * h * 4) as usize;
    if count == 0 || crops.len() < crop_len * count as usize {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(crops.len());
    for row in 0..h as usize {
        for c in 0..count as usize {
            let start = c * crop_len + row * w as usize * 4;
            out.extend_from_slice(&crops[start..start + w as usize * 4]);
        }
    }
    out
}

/// Parses `x,y,w,h;x,y,w,h` crop lists.
#[must_use]
pub fn parse_crops(text: &str) -> Vec<[u32; 4]> {
    text.split(';')
        .filter_map(|part| {
            let v: Vec<u32> = part.split(',').filter_map(|n| n.trim().parse().ok()).collect();
            (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spike_is_a_pop_and_a_ramp_is_not() {
        let ramp: Vec<f32> = (0..20).map(|i| 3.0 + i as f32 * 0.2).collect();
        assert_eq!(count_pops(&ramp, 2.0), 0);
        let mut spike = vec![3.0; 20];
        spike[10] = 15.0;
        assert_eq!(count_pops(&spike, 2.0), 1);
        let mut quiet = vec![0.1; 20];
        quiet[10] = 1.5;
        assert_eq!(count_pops(&quiet, 2.0), 0, "below the floor is noise, not a pop");
    }

    #[test]
    fn a_blurred_image_has_less_gradient_energy() {
        let (w, h) = (16u32, 16u32);
        let sharp: Vec<f32> = (0..w * h)
            .map(|i| if (i % w) % 2 == 0 { 0.0 } else { 255.0 })
            .collect();
        let blurred: Vec<f32> = vec![127.5; (w * h) as usize];
        assert!(gradient_energy(&sharp, w, h, None) > 1000.0);
        assert!(gradient_energy(&blurred, w, h, None) < 1e-3);
        assert!((mean_abs_diff(&sharp, &blurred, w, h, None) - 127.5).abs() < 1e-3);
    }

    #[test]
    fn a_rotated_view_warps_back_onto_itself() {
        // A smooth world-space pattern seen by two cameras a degree of yaw apart: the warp
        // must line the frames up (the residual is resampling only).
        let (w, h) = (96u32, 64u32);
        let render = |cam: Camera| -> Vec<f32> {
            let tan_y = (cam.fov_y * 0.5).tan();
            let tan_x = tan_y * w as f32 / h as f32;
            let [r, u, f] = cam.basis();
            (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    let sx = (2.0 * (x as f32 + 0.5) / w as f32 - 1.0) * tan_x;
                    let sy = (1.0 - 2.0 * (y as f32 + 0.5) / h as f32) * tan_y;
                    let d = (r * sx + u * sy + f).normalize();
                    128.0 + 100.0 * (d.x * 9.0).sin() * (d.y * 7.0).cos()
                })
                .collect()
        };
        let a = Camera {
            eye: [0.0; 3],
            target: [0.0, -0.2, -1.0],
            fov_y: 1.0,
        };
        let yaw = 1.0f32.to_radians();
        let b = Camera {
            target: [-yaw.sin(), -0.2, -yaw.cos()],
            ..a
        };
        let (fa, fb) = (render(a), render(b));
        let raw = mean_abs_diff(&fb, &fa, w, h, None);
        let warped = rotation_flicker(&fb, &fa, w, h, b, a).unwrap_or(f32::MAX);
        assert!(warped < raw * 0.1, "warp residual {warped} vs unwarped {raw}");
    }

    #[test]
    fn strips_lay_crops_side_by_side() {
        let frame = |v: u8| Frame {
            width: 2,
            height: 2,
            pixels: vec![v; 16],
        };
        let mut crops = Vec::new();
        append_crop(&mut crops, &frame(10), [0, 0, 2, 2]);
        append_crop(&mut crops, &frame(20), [0, 0, 2, 2]);
        let strip = strip_rows(&crops, 2, 2, 2);
        assert_eq!(strip.len(), 32);
        assert_eq!(&strip[0..8], &[10; 8]);
        assert_eq!(&strip[8..16], &[20; 8]);
        assert_eq!(parse_crops("1,2,3,4;5,6,7,8"), vec![[1, 2, 3, 4], [5, 6, 7, 8]]);
    }
}
