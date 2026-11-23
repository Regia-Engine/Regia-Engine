// SPDX-License-Identifier: MPL-2.0
//! Per-frame measurements that mean something without a real GPU.

use regia_core::glam::{Mat4, Vec3, Vec4};

use crate::scene::{CamFrame, H, HALF_WIDTH, RAMP_START_Z, TUBE_START_Z, W, view_proj};

/// Mean BT.709 luma of the frame, 0–255.
pub fn mean_luma(px: &[u8]) -> f32 {
    let mut sum = 0.0f64;
    for p in px.as_chunks::<4>().0 {
        sum += f64::from(luma(p));
    }
    (sum / (px.len() / 4).max(1) as f64) as f32
}

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

/// Fraction of pixels whose three channels are all below 3 (black on any display).
pub fn black_fraction(px: &[u8]) -> f32 {
    let black = px
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] < 3 && p[1] < 3 && p[2] < 3)
        .count();
    black as f32 / (px.len() / 4).max(1) as f32
}

fn rgb_at(px: &[u8], x: i32, y: i32) -> [f32; 3] {
    let x = x.clamp(0, W as i32 - 1) as u32;
    let y = y.clamp(0, H as i32 - 1) as u32;
    let i = ((y * W + x) * 4) as usize;
    [f32::from(px[i]), f32::from(px[i + 1]), f32::from(px[i + 2])]
}

/// Pixel coordinates (x right, y down) of a world point, `None` behind the camera.
pub fn project(vp: Mat4, p: Vec3) -> Option<[f32; 2]> {
    let c = vp * Vec4::new(p.x, p.y, p.z, 1.0);
    if c.w <= 1e-4 {
        return None;
    }
    Some([
        (c.x / c.w * 0.5 + 0.5) * W as f32,
        (0.5 - c.y / c.w * 0.5) * H as f32,
    ])
}

/// Where the ray through pixel (x, y) crosses the plane y = 0, if it goes down.
fn ground_hit(inv: Mat4, eye: Vec3, x: f32, y: f32) -> Option<Vec3> {
    let ndc = [x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0];
    let a = inv * Vec4::new(ndc[0], ndc[1], 0.0, 1.0);
    let b = inv * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
    let (a, b) = (a.truncate() / a.w, b.truncate() / b.w);
    let dir = (b - a).normalize();
    (dir.y < -1e-4).then(|| eye + dir * (-eye.y / dir.y))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Footprint,
    Ground,
    Other,
}

/// Terrain pixels inside the cut: `(judged, terrain_like)`.
///
/// A pixel is judged when its ray crosses the ground plane inside the cut (shrunk by 0.3 m,
/// and eroded by two pixels), with the camera above the ground. It is terrain-like when its
/// colour is within 14 levels of the ground just outside the cut. Frames with the eye
/// under the ground judge nothing.
pub fn terrain_in_cut(px: &[u8], cam: &CamFrame) -> (u32, u32) {
    if cam.eye[1] < 0.5 {
        return (0, 0);
    }
    let eye = Vec3::from_array(cam.eye);
    let inv = view_proj(cam).inverse();
    let (z0, z1) = (RAMP_START_Z, TUBE_START_Z);
    let cell = |x: u32, y: u32| match ground_hit(inv, eye, x as f32 + 0.5, y as f32 + 0.5) {
        Some(g) if g.x.abs() < HALF_WIDTH - 0.3 && g.z > z0 + 0.3 && g.z < z1 - 0.3 => Cell::Footprint,
        Some(g) if g.x.abs() > HALF_WIDTH + 0.5 || g.z < z0 - 0.5 || g.z > z1 + 0.5 => {
            if (g.x * g.x + g.z * g.z).sqrt() < 120.0 {
                Cell::Ground
            } else {
                Cell::Other
            }
        }
        _ => Cell::Other,
    };
    let grid: Vec<Cell> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| cell(x, y))
        .collect();
    let at = |x: i32, y: i32| {
        grid[(y.clamp(0, H as i32 - 1) as u32 * W + x.clamp(0, W as i32 - 1) as u32) as usize]
    };
    // The ground just outside the cut: a median colour of ground pixels within 10 px of it.
    let mut refs: [Vec<f32>; 3] = Default::default();
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            if at(x, y) != Cell::Ground {
                continue;
            }
            let near = (-10..=10).step_by(5).any(|dy| {
                (-10..=10)
                    .step_by(5)
                    .any(|dx| at(x + dx, y + dy) == Cell::Footprint)
            });
            if near {
                let c = rgb_at(px, x, y);
                for k in 0..3 {
                    refs[k].push(c[k]);
                }
            }
        }
    }
    if refs[0].is_empty() {
        return (0, 0);
    }
    let reference: [f32; 3] = std::array::from_fn(|k| {
        refs[k].sort_by(f32::total_cmp);
        refs[k][refs[k].len() / 2]
    });
    let (mut judged, mut like) = (0, 0);
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            if at(x, y) != Cell::Footprint {
                continue;
            }
            if !(-2..=2).all(|dy| (-2..=2).all(|dx| at(x + dx, y + dy) == Cell::Footprint)) {
                continue;
            }
            judged += 1;
            let c = rgb_at(px, x, y);
            if (0..3).all(|k| (c[k] - reference[k]).abs() <= 14.0) {
                like += 1;
            }
        }
    }
    (judged, like)
}

/// Distance from `c` to the segment `a`–`b` in RGB (0–255 levels).
fn dist_to_segment(c: [f32; 3], a: [f32; 3], b: [f32; 3]) -> f32 {
    let ab = Vec3::from_array(b) - Vec3::from_array(a);
    let ac = Vec3::from_array(c) - Vec3::from_array(a);
    let t = if ab.length_squared() < 1e-6 {
        0.0
    } else {
        (ac.dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
    };
    (ac - ab * t).length()
}

/// Crack detector along the cut's edge: `(samples, cracked)`.
///
/// Walks the cut's edge on the ground every 0.25 m. Across each sample (in screen space,
/// perpendicular to the seam) the ground reads on one side and the lip on the other; a sample
/// is cracked when a pixel within 1.5 px of the seam is further than 30 levels (RGB) from every
/// ground–lip colour segment, i.e. it is neither ground, lip nor a blend of the two: something
/// else shows through (the trench, the sky). Only edge stretches within 18 m whose lip is at
/// least 3.5 px wide on screen are judged, and only from a camera above the ground (eye at
/// least 1 m up).
///
/// The references are anchored in the world, not in pixels: the lip's colour is read on its
/// centre line (`LIP_WIDTH / 2` inside the edge), the ground's 0.5 m outside the cut (past the
/// lip's under-ground overlap and the drawn edge's raster error). A fixed 2.5 px offset read the
/// trench for a lip about 3.5 px wide (frame 9 on lavapipe: the "lip" was 82,67,60 and the
/// seam pixels, lip-coloured, failed). The ground segment end is any ground colour read along
/// the same edge in the same frame, scaled by 0.75 to 1.5 (lighting scales the albedo): the
/// ground is lit unevenly (macro variation, cloud shadows: 128 to 180 levels in patches on
/// lavapipe; a 1 to 2 px brighter band right along the seam at close range, 163 against 127),
/// and one reference pixel failed plain ground pixels (29 samples, every seam pixel ground or
/// lip). Samples within `LIP_WIDTH` of an edge's ends are not judged: there the across-seam
/// profile runs along the perpendicular edge, where the terrain covers half the lip, so the
/// "lip" reference read ground (two corner samples at camera z -26). Trench and sky stay far from
/// every such segment: the trench is brown (about 80,67,60 in shade), the ground grey (128 to
/// 180), the lip green (`scene::materials`), the sky blue.
pub fn edge_cracks(px: &[u8], cam: &CamFrame) -> (u32, u32) {
    let samples = edge_samples(px, cam);
    let cracked = samples.iter().filter(|s| s.cracked).count();
    (samples.len() as u32, cracked as u32)
}

/// One judged sample of [`edge_cracks`], for the dump's overlay.
pub struct EdgeSample {
    /// Screen position of the seam, pixels.
    pub at: [f32; 2],
    /// Ground reference colour (0.5 m outside the cut).
    pub ground: [f32; 3],
    /// Lip reference colour (on the lip's centre line).
    pub lip: [f32; 3],
    /// The seam pixels, -1.5 px (lip side) to +1.5 px (ground side) in 0.5 px steps.
    pub seam: [[f32; 3]; 7],
    /// Largest distance (levels) of a seam pixel from the nearest ground–lip segment.
    pub worst: f32,
    /// `worst` exceeds the bound.
    pub cracked: bool,
}

/// Brightness scales of a ground reference colour [`edge_cracks`] accepts (lighting scales the
/// albedo: AO, macro variation, cloud shadows). The shaded trench (about 82,67,60) stays 65
/// levels from the darkest scaled grey, the lit trench and the sky about 60 from any.
const GROUND_SCALES: [f32; 8] = [0.75, 0.85, 0.95, 1.05, 1.15, 1.25, 1.375, 1.5];

/// The judged samples of [`edge_cracks`].
pub fn edge_samples(px: &[u8], cam: &CamFrame) -> Vec<EdgeSample> {
    let mut out_samples = Vec::new();
    if cam.eye[1] < 1.0 {
        return out_samples;
    }
    let vp = view_proj(cam);
    let eye = Vec3::from_array(cam.eye);
    let (z0, z1, x) = (RAMP_START_Z, TUBE_START_Z, HALF_WIDTH);
    let lip_width = crate::scene::LIP_WIDTH;
    // (start, end, outward normal) of the four edges on the ground plane.
    let edges = [
        (Vec3::new(-x, 0.0, z0), Vec3::new(-x, 0.0, z1), Vec3::NEG_X),
        (Vec3::new(x, 0.0, z0), Vec3::new(x, 0.0, z1), Vec3::X),
        (Vec3::new(-x, 0.0, z0), Vec3::new(x, 0.0, z0), Vec3::NEG_Z),
        (Vec3::new(-x, 0.0, z1), Vec3::new(x, 0.0, z1), Vec3::Z),
    ];
    for (a, b, out) in edges {
        let len = (b - a).length();
        let n = (len / 0.25) as usize;
        // (seam position, ground reference, lip reference, seam pixels) of this edge.
        let mut edge = Vec::new();
        for i in 0..=n {
            let p = a + (b - a) * (i as f32 / n.max(1) as f32);
            let from_end = (p - a).length().min((b - p).length());
            if (p - eye).length() > 18.0 || from_end < lip_width {
                continue;
            }
            let (Some(s), Some(s_out), Some(s_in), Some(s_ground), Some(s_lip)) = (
                project(vp, p),
                project(vp, p + out),
                project(vp, p - out * lip_width),
                project(vp, p + out * 0.5),
                project(vp, p - out * (lip_width * 0.5)),
            ) else {
                continue;
            };
            let to_out = Vec3::new(s_out[0] - s[0], s_out[1] - s[1], 0.0);
            let to_in = Vec3::new(s[0] - s_in[0], s[1] - s_in[1], 0.0);
            if to_out.length() < 3.5 || to_in.length() < 3.5 {
                continue;
            }
            let u = to_out.normalize();
            let at = |t: f32| {
                rgb_at(
                    px,
                    (s[0] + u.x * t).floor() as i32,
                    (s[1] + u.y * t).floor() as i32,
                )
            };
            let read = |q: [f32; 2]| rgb_at(px, q[0].floor() as i32, q[1].floor() as i32);
            let seam = [-1.5f32, -1.0, -0.5, 0.0, 0.5, 1.0, 1.5].map(at);
            edge.push((s, read(s_ground), read(s_lip), seam));
        }
        // Every edge ground colour, lit from 0.75 to 1.5 times as bright.
        let grounds: Vec<[f32; 3]> = edge
            .iter()
            .flat_map(|e| GROUND_SCALES.map(|k| e.1.map(|c| (c * k).min(255.0))))
            .collect();
        for (s, ground, lip, seam) in edge {
            let worst = seam
                .iter()
                .map(|&c| {
                    grounds
                        .iter()
                        .map(|&g| dist_to_segment(c, g, lip))
                        .fold(f32::MAX, f32::min)
                })
                .fold(0.0, f32::max);
            out_samples.push(EdgeSample {
                at: s,
                ground,
                lip,
                seam,
                worst,
                cracked: worst > 30.0,
            });
        }
    }
    out_samples
}
