// SPDX-License-Identifier: MPL-2.0
//! The studio: a fixed environment cube map with roughness mips and three key lights.
//!
//! The studio is procedural (soft boxes on a dim sweep, a few small bright kickers that make gems
//! sparkle, a darker patch where the camera stands so stones keep their contrast). It is built once
//! and shared by every product; the viewer may instead load a baked one from its studio package.
//! Mips are prefiltered on the CPU with a fixed spiral of taps in a cone that widens per level, so
//! the result is deterministic and the GPU only does `textureSampleLevel`.

use glam::Vec3;

/// One of the three fixed studio key lights.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyLight {
    /// Direction toward the light (world, unit).
    pub dir: Vec3,
    /// Linear RGB radiance scale.
    pub color: Vec3,
}

/// A rectangular soft box seen from the studio center.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SoftBox {
    center: Vec3,
    half_w: f32,
    half_h: f32,
    radiance: Vec3,
}

/// The studio description (data).
#[derive(Debug, Clone, PartialEq)]
pub struct Studio {
    /// The three key lights (also drawn into the map as soft boxes).
    pub keys: [KeyLight; 3],
    /// Base face size of the cube map in texels.
    pub base_size: u32,
    /// Number of mips (the last is the diffuse-like level).
    pub mips: u32,
}

impl Default for Studio {
    fn default() -> Self {
        Studio {
            keys: [
                KeyLight {
                    dir: Vec3::new(-0.55, 0.62, 0.56).normalize(),
                    color: Vec3::new(5.0, 4.9, 4.7),
                },
                KeyLight {
                    dir: Vec3::new(0.70, 0.40, 0.59).normalize(),
                    color: Vec3::new(2.2, 2.25, 2.4),
                },
                KeyLight {
                    dir: Vec3::new(0.10, 0.85, -0.52).normalize(),
                    color: Vec3::new(3.0, 3.0, 3.0),
                },
            ],
            base_size: 128,
            mips: 6,
        }
    }
}

/// Cube face directions in wgpu's face order (+X, -X, +Y, -Y, +Z, -Z) for face coordinates
/// `u, v` in `[-1, 1]` (u to the right, v down the image).
pub fn face_dir(face: u32, u: f32, v: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    }
    .normalize()
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Studio {
    fn boxes(&self) -> Vec<SoftBox> {
        let [k0, k1, k2] = self.keys;
        vec![
            SoftBox {
                center: k0.dir,
                half_w: 0.42,
                half_h: 0.55,
                radiance: k0.color,
            },
            SoftBox {
                center: k1.dir,
                half_w: 0.35,
                half_h: 0.45,
                radiance: k1.color,
            },
            SoftBox {
                center: k2.dir,
                half_w: 0.9,
                half_h: 0.16,
                radiance: k2.color,
            },
        ]
    }

    /// Radiance of the studio along a world direction (linear RGB).
    pub fn radiance(&self, d: Vec3) -> Vec3 {
        let d = d.normalize_or_zero();
        // Sweep: a dim warm-gray upper wall, a darker floor.
        let up = d.y;
        let wall = Vec3::new(0.46, 0.455, 0.45) * (0.55 + 0.45 * smooth(-0.1, 0.8, up));
        let floor = Vec3::new(0.045, 0.045, 0.05);
        let mut c = floor.lerp(wall, smooth(-0.25, 0.05, up));
        for b in self.boxes() {
            let cd = d.dot(b.center);
            if cd <= 0.05 {
                continue;
            }
            let right = Vec3::Y.cross(b.center).normalize_or(Vec3::X);
            let upv = b.center.cross(right);
            let x = d.dot(right) / cd;
            let y = d.dot(upv) / cd;
            let m = (1.0 - smooth(b.half_w * 0.85, b.half_w, x.abs()))
                * (1.0 - smooth(b.half_h * 0.85, b.half_h, y.abs()));
            c += b.radiance * m;
        }
        // Small bright kickers (studio spots) for gem sparkle.
        let kickers = [
            (Vec3::new(-0.25, 0.9, 0.35), 30.0),
            (Vec3::new(0.45, 0.75, -0.2), 26.0),
            (Vec3::new(-0.8, 0.3, -0.3), 22.0),
            (Vec3::new(0.85, 0.15, 0.35), 18.0),
            (Vec3::new(0.0, 0.55, 0.83), 14.0),
            (Vec3::new(-0.6, 0.6, 0.5), 20.0),
            (Vec3::new(0.3, 0.4, 0.86), 16.0),
            (Vec3::new(-0.35, 0.2, 0.9), 16.0),
            (Vec3::new(0.65, 0.65, 0.2), 22.0),
            (Vec3::new(-0.1, 0.98, -0.1), 26.0),
            (Vec3::new(0.2, 0.15, -0.95), 18.0),
            (Vec3::new(-0.9, 0.05, 0.4), 14.0),
            (Vec3::new(0.9, 0.35, -0.25), 14.0),
        ];
        for (dir, power) in kickers {
            let cd = d.dot(dir.normalize());
            c += Vec3::splat(power * smooth(0.9985, 0.9996, cd));
        }
        // The camera stands at +z: a darker patch keeps contrast in stones and polished faces.
        let cam = d.dot(Vec3::new(0.0, 0.05, 1.0).normalize());
        c * (1.0 - 0.75 * smooth(0.93, 0.985, cam))
    }

    /// Face size of mip `level`.
    pub fn mip_size(&self, level: u32) -> u32 {
        (self.base_size >> level).max(1)
    }

    /// Builds every mip of every face as linear RGBA f32, `[mip][face]` row-major texels, for the
    /// look shaders' linear roughness-to-mip mapping ([`Studio::level_spread`]).
    pub fn build(&self) -> Vec<[Vec<[f32; 4]>; 6]> {
        self.build_with(|level| self.level_spread(level))
    }

    /// Builds every mip with a caller's blur: `spread(level)` is the cone half-angle in radians
    /// (0 is the supersampled sharp level).
    pub fn build_with(&self, spread: impl Fn(u32) -> f32) -> Vec<[Vec<[f32; 4]>; 6]> {
        let _span =
            tracing::debug_span!("studio_env_build", size = self.base_size, mips = self.mips).entered();
        (0..self.mips)
            .map(|level| self.build_level(level, spread(level)))
            .collect()
    }

    /// The cone half-angle (radians) a mip blurs over: 0 at the base, about 1 radian at the last.
    pub fn level_spread(&self, level: u32) -> f32 {
        if level == 0 || self.mips <= 1 {
            return 0.0;
        }
        let r = level as f32 / (self.mips - 1) as f32;
        // Roughness r maps to the mip linearly in the shader; the lobe width grows ~ r^2.
        (1.1 * r * r).max(0.02)
    }

    fn build_level(&self, level: u32, spread: f32) -> [Vec<[f32; 4]>; 6] {
        let size = self.mip_size(level);
        // Supersample the base level 2x2; blur higher levels over a cone with a Fibonacci spiral.
        let taps: Vec<(Vec3, f32)> = if spread <= 1e-4 {
            Vec::new()
        } else {
            let n = 96;
            let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
            (0..n)
                .map(|i| {
                    let t = (i as f32 + 0.5) / n as f32;
                    let theta = spread * t.sqrt() * 1.6;
                    let phi = i as f32 * golden;
                    let w = (-(theta / spread).powi(2)).exp();
                    (
                        Vec3::new(theta.sin() * phi.cos(), theta.sin() * phi.sin(), theta.cos()),
                        w,
                    )
                })
                .collect()
        };
        std::array::from_fn(|face| {
            let mut out = Vec::with_capacity((size * size) as usize);
            for y in 0..size {
                for x in 0..size {
                    let c = if taps.is_empty() {
                        let mut acc = Vec3::ZERO;
                        for (ox, oy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                            let u = (x as f32 + ox) / size as f32 * 2.0 - 1.0;
                            let v = (y as f32 + oy) / size as f32 * 2.0 - 1.0;
                            acc += self.radiance(face_dir(face as u32, u, v));
                        }
                        acc * 0.25
                    } else {
                        let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                        let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                        let n = face_dir(face as u32, u, v);
                        let t = n.any_orthonormal_vector();
                        let b = n.cross(t);
                        let mut acc = Vec3::ZERO;
                        let mut wsum = 0.0;
                        for (l, w) in &taps {
                            let d = t * l.x + b * l.y + n * l.z;
                            acc += self.radiance(d) * *w;
                            wsum += *w;
                        }
                        acc / wsum.max(1e-6)
                    };
                    out.push([c.x, c.y, c.z, 1.0]);
                }
            }
            out
        })
    }
}

/// IEEE half-precision bits of an `f32` (round to nearest even; clamps to the largest finite half).
pub fn f32_to_f16_bits(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | if mant == 0 { 0x7c00 } else { 0x7e00 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7bff;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let mid = 1 << (shift - 1);
        let round = u32::from(rem > mid || (rem == mid && (half & 1) == 1));
        return sign | (half + round) as u16;
    }
    let half = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    let round = u32::from(rem > 0x1000 || (rem == 0x1000 && (half & 1) == 1));
    let out = half + round;
    if out >= 0x7c00 {
        sign | 0x7bff
    } else {
        sign | out as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_dirs_point_at_their_face() {
        let axes = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
        for (f, a) in axes.iter().enumerate() {
            assert!(face_dir(f as u32, 0.0, 0.0).dot(*a) > 0.999);
        }
        // Image-right on +Z is world +x, image-down is world -y.
        assert!(face_dir(4, 1.0, 0.0).x > 0.5);
        assert!(face_dir(4, 0.0, 1.0).y < -0.5);
    }

    #[test]
    fn key_lights_are_the_brightest_soft_boxes() {
        let s = Studio::default();
        let key = s.radiance(s.keys[0].dir);
        let floor = s.radiance(Vec3::NEG_Y);
        assert!(key.x > 4.0 && floor.x < 0.1, "{key} {floor}");
    }

    #[test]
    fn mips_blur_and_conserve_roughly() {
        let s = Studio {
            base_size: 16,
            mips: 4,
            ..Studio::default()
        };
        let levels = s.build();
        assert_eq!(levels.len(), 4);
        assert_eq!(levels[3][0].len(), 4);
        let mean = |l: &[Vec<[f32; 4]>; 6]| {
            let n: usize = l.iter().map(Vec::len).sum();
            l.iter().flatten().map(|t| t[1]).sum::<f32>() / n as f32
        };
        let (m0, m3) = (mean(&levels[0]), mean(&levels[3]));
        assert!((m3 - m0).abs() < 0.6 * m0, "{m0} {m3}");
        let max = |l: &[Vec<[f32; 4]>; 6]| l.iter().flatten().map(|t| t[1]).fold(0.0f32, f32::max);
        assert!(max(&levels[3]) < max(&levels[0]), "the last mip is blurred");
    }

    #[test]
    fn half_floats() {
        assert_eq!(f32_to_f16_bits(0.0), 0);
        assert_eq!(f32_to_f16_bits(1.0), 0x3c00);
        assert_eq!(f32_to_f16_bits(-2.0), 0xc000);
        assert_eq!(f32_to_f16_bits(65504.0), 0x7bff);
        assert_eq!(f32_to_f16_bits(1e9), 0x7bff);
        assert_eq!(f32_to_f16_bits(0.5), 0x3800);
        assert_eq!(f32_to_f16_bits(5.960_464_5e-8), 0x0001);
    }
}
