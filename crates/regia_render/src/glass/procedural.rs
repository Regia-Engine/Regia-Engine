// SPDX-License-Identifier: MPL-2.0
//! Built-in crack sets drawn procedurally: `laminate` (unit canopies) and `pressure_pane`
//! (building windows). They follow the same format as authored sets
//! (`docs/formats/glass-crack.md`), so games and tests have damage glass before DO27's art
//! lands, and DO27's sets replace them by name.
//!
//! The patterns follow the fracture morphology of impacted glazing described in the public
//! literature on glass fractography (radial cracks from the impact, concentric cracks joining
//! them, a crushed cone at the point of impact; laminated glass crazes and delaminates around the
//! hit instead of releasing fragments): see `docs/provenance.md` ("glass crack stamps").

use regia_asset::glasscrack::{
    ChunkBurst, CookedCrackSet, CrackShading, CrackStamp, DamageSpace, Escalation, GLASSCRACK_FORMAT_VERSION,
    GlassCrackSet, GlassType, LaminateRules, PressurePaneRules, StampPixels,
};

use crate::soft_math;

/// Small deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in 0..1.
    fn f(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f()
    }
}

/// Float canvas for one stamp (R cracks, G crazing, B holes; A and normals derived).
struct Canvas {
    n: usize,
    r: Vec<f32>,
    g: Vec<f32>,
    b: Vec<f32>,
}

impl Canvas {
    fn new(n: usize) -> Self {
        Self {
            n,
            r: vec![0.0; n * n],
            g: vec![0.0; n * n],
            b: vec![0.0; n * n],
        }
    }

    /// Anti-aliased segment into R (or G), half-width `w` texels, max-combined.
    fn segment(&mut self, a: [f32; 2], b: [f32; 2], w: f32, value: f32, crazing: bool) {
        let n = self.n as i32;
        let pad = w + 1.5;
        let x0 = (a[0].min(b[0]) - pad).floor().max(0.0) as i32;
        let y0 = (a[1].min(b[1]) - pad).floor().max(0.0) as i32;
        let x1 = ((a[0].max(b[0]) + pad).ceil() as i32).min(n - 1);
        let y1 = ((a[1].max(b[1]) + pad).ceil() as i32).min(n - 1);
        let d = [b[0] - a[0], b[1] - a[1]];
        let len2 = (d[0] * d[0] + d[1] * d[1]).max(1e-6);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let p = [x as f32 + 0.5 - a[0], y as f32 + 0.5 - a[1]];
                let t = ((p[0] * d[0] + p[1] * d[1]) / len2).clamp(0.0, 1.0);
                let q = [p[0] - d[0] * t, p[1] - d[1] * t];
                let dist = (q[0] * q[0] + q[1] * q[1]).sqrt();
                let cover = (1.0 - (dist - w + 0.5).max(0.0)).clamp(0.0, 1.0);
                if cover <= 0.0 {
                    continue;
                }
                let i = y as usize * self.n + x as usize;
                let target = if crazing { &mut self.g } else { &mut self.r };
                target[i] = target[i].max(value * cover);
            }
        }
    }

    /// A jagged crack from `from` heading `angle`, `length` texels, tapering width, with
    /// occasional side branches. Returns the end point.
    #[allow(clippy::too_many_arguments)] // one crack's whole shape
    fn crack(
        &mut self,
        rng: &mut Rng,
        from: [f32; 2],
        angle: f32,
        length: f32,
        width: f32,
        value: f32,
        branch: f32,
    ) -> [f32; 2] {
        let steps = ((length / 6.0).ceil() as usize).clamp(3, 64);
        let step = length / steps as f32;
        let mut p = from;
        let mut heading = angle;
        for i in 0..steps {
            heading += rng.range(-0.22, 0.22);
            // Cracks keep their general direction: pull back toward the initial heading.
            heading += (angle - heading) * 0.35;
            let q = [
                p[0] + soft_math::cos(heading) * step,
                p[1] + soft_math::sin(heading) * step,
            ];
            let taper = 1.0 - 0.6 * i as f32 / steps as f32;
            self.segment(
                p,
                q,
                (width * taper).max(0.55),
                value * (0.75 + 0.25 * taper),
                false,
            );
            if branch > 0.0 && rng.f() < branch && i > 1 {
                let side = if rng.f() < 0.5 { -1.0 } else { 1.0 };
                let sub = length * rng.range(0.12, 0.3);
                let turn = side * rng.range(0.35, 0.8);
                self.crack(rng, q, heading + turn, sub, width * 0.6, value * 0.8, 0.0);
            }
            p = q;
        }
        p
    }

    /// Short random micro-cracks into G with a density falling off from `c` over `radius`.
    fn crazing(&mut self, rng: &mut Rng, c: [f32; 2], radius: f32, count: usize, strength: f32) {
        for _ in 0..count {
            let r = radius * rng.f().sqrt();
            let a = rng.range(0.0, std::f32::consts::TAU);
            let p = [c[0] + soft_math::cos(a) * r, c[1] + soft_math::sin(a) * r];
            let len = rng.range(2.0, 7.0);
            let dir = rng.range(0.0, std::f32::consts::TAU);
            let q = [p[0] + soft_math::cos(dir) * len, p[1] + soft_math::sin(dir) * len];
            let fall = 1.0 - (r / radius).powi(2);
            self.segment(p, q, 0.6, strength * fall * rng.range(0.5, 1.0), true);
        }
    }

    /// Soft disc into G (milky crush/delamination).
    fn haze(&mut self, c: [f32; 2], radius: f32, value: f32) {
        for y in 0..self.n {
            for x in 0..self.n {
                let d = ((x as f32 + 0.5 - c[0]).powi(2) + (y as f32 + 0.5 - c[1]).powi(2)).sqrt();
                if d < radius {
                    let t = 1.0 - d / radius;
                    let i = y * self.n + x;
                    self.g[i] = self.g[i].max(value * t * t);
                }
            }
        }
    }

    /// Jagged hole of mean radius `radius` texels into B, with cracked rim into R.
    fn hole(&mut self, rng: &mut Rng, c: [f32; 2], radius: f32, jag: f32) {
        let lobes: Vec<(f32, f32, f32)> = (0..7)
            .map(|k| {
                (
                    k as f32 + 2.0,
                    rng.range(0.0, std::f32::consts::TAU),
                    rng.range(0.3, 1.0) / (k as f32 + 1.0),
                )
            })
            .collect();
        // Angular jag: a few sines plus a sawtooth at high frequency (angular facets).
        let edge = |a: f32| -> f32 {
            // soft_math: the rim is quantized into u8 texels, so the platform libm is
            // not allowed here (audit G, render5 determinism).
            let s: f32 = lobes
                .iter()
                .map(|(f, ph, amp)| amp * soft_math::sin(a * f + ph))
                .sum();
            let facet = ((a * 23.0).fract() - 0.5).abs() * 0.5;
            radius * (1.0 + jag * (0.45 * s + facet))
        };
        for y in 0..self.n {
            for x in 0..self.n {
                let (dx, dy) = (x as f32 + 0.5 - c[0], y as f32 + 0.5 - c[1]);
                let d = (dx * dx + dy * dy).sqrt();
                let e = edge(soft_math::atan2(dy, dx));
                let i = y * self.n + x;
                if d < e {
                    self.b[i] = 1.0;
                } else if d < e + 2.5 {
                    self.r[i] = self.r[i].max(1.0 - (d - e) / 2.5);
                }
            }
        }
    }

    /// Cooked pixels: A = bend strength (cracks full, crazing half), normals from the gradient
    /// of the blurred crack field (a crack reads as a ridge/valley edge).
    fn finish(&self) -> StampPixels {
        let n = self.n;
        let blur = |x: usize, y: usize| -> f32 {
            let mut s = 0.0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let xx = (x as i32 + dx).clamp(0, n as i32 - 1) as usize;
                    let yy = (y as i32 + dy).clamp(0, n as i32 - 1) as usize;
                    s += self.r[yy * n + xx] + 0.4 * self.g[yy * n + xx];
                }
            }
            s / 9.0
        };
        let h: Vec<f32> = (0..n * n).map(|i| blur(i % n, i / n)).collect();
        let mut mask = vec![0u8; n * n * 4];
        let mut normal = vec![128u8; n * n * 2];
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        for y in 0..n {
            for x in 0..n {
                let i = y * n + x;
                let at = |xx: usize, yy: usize| h[yy.min(n - 1) * n + xx.min(n - 1)];
                let gx = at(x + 1, y) - at(x.saturating_sub(1), y);
                let gy = at(x, y + 1) - at(x, y.saturating_sub(1));
                // Image Y down → tangent Y up (glTF): flip gy.
                let (nx, ny) = ((-gx * 1.6).clamp(-1.0, 1.0), (gy * 1.6).clamp(-1.0, 1.0));
                normal[i * 2] = q(nx * 0.5 + 0.5);
                normal[i * 2 + 1] = q(ny * 0.5 + 0.5);
                let a = self.r[i].max(self.g[i] * 0.5);
                mask[i * 4..i * 4 + 4].copy_from_slice(&[q(self.r[i]), q(self.g[i]), q(self.b[i]), q(a)]);
            }
        }
        StampPixels {
            size: n as u32,
            mask,
            normal,
        }
    }
}

/// Radial + concentric fracture: `radials` cracks from the centre, rings at `ring_radii`
/// (fractions of the half size) joining neighbouring radials.
fn web(
    c: &mut Canvas,
    rng: &mut Rng,
    radials: usize,
    reach: f32,
    rings: &[f32],
    width: f32,
    ring_value: f32,
) {
    let centre = [c.n as f32 * 0.5, c.n as f32 * 0.5];
    let mut angles: Vec<f32> = (0..radials)
        .map(|k| (k as f32 + rng.range(-0.3, 0.3)) / radials as f32 * std::f32::consts::TAU)
        .collect();
    angles.sort_by(f32::total_cmp);
    let half = c.n as f32 * 0.5;
    let lengths: Vec<f32> = angles
        .iter()
        .map(|_| half * reach * rng.range(0.65, 1.0))
        .collect();
    for (a, l) in angles.iter().zip(&lengths) {
        c.crack(rng, centre, *a, *l, width, 1.0, 0.18);
    }
    for ring in rings {
        for k in 0..radials {
            let (a0, a1) = (
                angles[k],
                angles[(k + 1) % radials]
                    + if k + 1 == radials {
                        std::f32::consts::TAU
                    } else {
                        0.0
                    },
            );
            let r = half * ring * rng.range(0.85, 1.15);
            if r > lengths[k].min(lengths[(k + 1) % radials]) || rng.f() < 0.2 {
                continue;
            }
            // Concentric cracks bow slightly toward the centre between two radials.
            let mid = (a0 + a1) * 0.5;
            let p0 = [
                centre[0] + soft_math::cos(a0) * r,
                centre[1] + soft_math::sin(a0) * r,
            ];
            let pm = [
                centre[0] + soft_math::cos(mid) * r * 0.93,
                centre[1] + soft_math::sin(mid) * r * 0.93,
            ];
            let p1 = [
                centre[0] + soft_math::cos(a1) * r,
                centre[1] + soft_math::sin(a1) * r,
            ];
            c.segment(p0, pm, width * 0.6, ring_value, false);
            c.segment(pm, p1, width * 0.6, ring_value, false);
        }
    }
}

fn laminate_d1(seed: u64) -> StampPixels {
    let mut rng = Rng(seed);
    let mut c = Canvas::new(256);
    let centre = [128.0, 128.0];
    // Bullseye: crushed core, a short radial star and one or two tight rings.
    c.haze(centre, 16.0, 0.95);
    let radials = 8 + (rng.next() % 5) as usize;
    web(&mut c, &mut rng, radials, 0.55, &[0.12, 0.24], 1.1, 0.8);
    c.crazing(&mut rng, centre, 40.0, 260, 0.7);
    // Spall chips: small flakes knocked off the surface near the hit.
    for _ in 0..4 {
        let a = rng.range(0.0, std::f32::consts::TAU);
        let r = rng.range(14.0, 40.0);
        // soft_math, not the platform libm: the chip centre feeds `finish`'s u8
        // texels, so a glibc/UCRT/bionic ulp difference would flip replayed D1 page
        // bits per OS (audit G finding 4, render5 determinism).
        let p = [
            centre[0] + soft_math::cos(a) * r,
            centre[1] + soft_math::sin(a) * r,
        ];
        c.haze(p, rng.range(3.0, 6.0), 0.8);
        let len = rng.range(4.0, 9.0);
        c.crack(&mut rng, p, a + 1.4, len, 0.7, 0.7, 0.0);
    }
    c.finish()
}

fn laminate_d2(seed: u64) -> StampPixels {
    let mut rng = Rng(seed);
    let mut c = Canvas::new(512);
    let centre = [256.0, 256.0];
    c.haze(centre, 34.0, 0.95);
    let radials = 14 + (rng.next() % 6) as usize;
    web(
        &mut c,
        &mut rng,
        radials,
        0.95,
        &[0.1, 0.2, 0.33, 0.5, 0.7],
        1.2,
        0.75,
    );
    c.crazing(&mut rng, centre, 150.0, 2400, 0.75);
    c.finish()
}

fn laminate_d3(seed: u64, hole: bool) -> StampPixels {
    let mut rng = Rng(seed);
    let mut c = Canvas::new(512);
    let centre = [256.0, 256.0];
    c.haze(centre, 90.0, 0.95);
    let radials = 20 + (rng.next() % 8) as usize;
    web(
        &mut c,
        &mut rng,
        radials,
        0.98,
        &[0.08, 0.15, 0.24, 0.34, 0.46, 0.6, 0.78],
        1.3,
        0.8,
    );
    c.crazing(&mut rng, centre, 240.0, 6000, 0.85);
    if hole {
        // 0.6 m footprint over 512 texels: 0.025 m mean radius ≈ 21 texels; the jag stays
        // inside the 0.035 m laminate limit (validated in tests).
        c.hole(&mut rng, centre, 19.0, 0.35);
    }
    c.finish()
}

fn pressure_d1(seed: u64) -> StampPixels {
    let mut rng = Rng(seed);
    let mut c = Canvas::new(256);
    let centre = [128.0, 128.0];
    c.haze(centre, 9.0, 0.95);
    // Star: long radials from a small crushed core, no rings.
    let radials = 6 + (rng.next() % 4) as usize;
    for k in 0..radials {
        let a = (k as f32 + rng.range(-0.35, 0.35)) / radials as f32 * std::f32::consts::TAU;
        let len = rng.range(80.0, 124.0);
        c.crack(&mut rng, centre, a, len, 1.8, 1.0, 0.12);
    }
    c.finish()
}

fn pressure_d2(seed: u64) -> StampPixels {
    let mut rng = Rng(seed);
    let mut c = Canvas::new(512);
    let centre = [256.0, 256.0];
    // Spiderweb: a crushed core, many radials and tight-to-wide rings. Lines are wide enough to
    // survive the stamp's downsample into the page (a 1 m stamp spans ~200 page texels).
    c.haze(centre, 22.0, 0.95);
    let radials = 14 + (rng.next() % 6) as usize;
    web(
        &mut c,
        &mut rng,
        radials,
        0.98,
        &[0.05, 0.1, 0.16, 0.23, 0.31, 0.4, 0.52, 0.66, 0.82],
        1.6,
        0.85,
    );
    c.crazing(&mut rng, centre, 70.0, 1200, 0.6);
    c.finish()
}

fn pressure_d3(seed: u64) -> StampPixels {
    let mut rng = Rng(seed);
    let mut c = Canvas::new(512);
    let centre = [256.0, 256.0];
    web(&mut c, &mut rng, 16, 1.0, &[0.5, 0.65, 0.82], 1.4, 0.9);
    // The pane is gone over most of the footprint; a jagged rim of chunks stays in the frame.
    c.hole(&mut rng, centre, 190.0, 0.28);
    c.finish()
}

/// An 8-bit sRGB colour as the manifest stores it (exact through the `#rrggbb` round trip).
fn srgb8(c: [u8; 3]) -> [f32; 3] {
    c.map(|v| f32::from(v) / 255.0)
}

fn stamp(name: &str, size_m: f32) -> CrackStamp {
    CrackStamp {
        name: name.to_string(),
        mask: format!("builtin:{name}"),
        normal: None,
        size_m: (size_m, size_m),
        pivot: (0.5, 0.5),
        weight: 1.0,
        random_rotation: true,
    }
}

/// The built-in `laminate` set (DO27 unit canopies).
#[must_use]
pub fn laminate_set() -> CookedCrackSet {
    let manifest = GlassCrackSet {
        format_version: GLASSCRACK_FORMAT_VERSION,
        name: "laminate".to_string(),
        glass_type: GlassType::Laminate,
        page_size: 512,
        damage_space: DamageSpace::Planar,
        shading: CrackShading {
            crack_scatter: srgb8([219, 230, 237]),
            crack_transmission: 0.12,
            crack_roughness: 0.6,
            crazing_haze: 0.6,
            crazing_roughness: 0.5,
            normal_bend: 1.0,
            hole_cutoff: 0.5,
            edge_jitter: 0.35,
        },
        escalation: Escalation::default(),
        d1: vec![
            stamp("lam_d1_a", 0.12),
            stamp("lam_d1_b", 0.1),
            stamp("lam_d1_c", 0.11),
        ],
        d2: vec![stamp("lam_d2_a", 0.45), stamp("lam_d2_b", 0.4)],
        d3: vec![stamp("lam_d3_punch", 0.6), stamp("lam_d3_crazed", 0.55)],
        laminate: Some(LaminateRules::default()),
        pressure_pane: None,
    };
    let stamps = vec![
        laminate_d1(11),
        laminate_d1(12),
        laminate_d1(13),
        laminate_d2(21),
        laminate_d2(22),
        laminate_d3(31, true),
        laminate_d3(32, false),
    ];
    CookedCrackSet { manifest, stamps }
}

/// The built-in `pressure_pane` set (DO27 building windows).
#[must_use]
pub fn pressure_pane_set() -> CookedCrackSet {
    let mut d3 = stamp("pp_d3_fail", 2.0);
    d3.random_rotation = false;
    let manifest = GlassCrackSet {
        format_version: GLASSCRACK_FORMAT_VERSION,
        name: "pressure_pane".to_string(),
        glass_type: GlassType::PressurePane,
        page_size: 512,
        damage_space: DamageSpace::Planar,
        shading: CrackShading {
            crack_scatter: srgb8([214, 224, 230]),
            crack_transmission: 0.15,
            crack_roughness: 0.55,
            crazing_haze: 0.35,
            crazing_roughness: 0.4,
            normal_bend: 1.4,
            hole_cutoff: 0.5,
            edge_jitter: 0.5,
        },
        escalation: Escalation {
            d1: 1.0,
            d2: 5.0,
            d3: 14.0,
        },
        d1: vec![stamp("pp_d1_a", 0.45), stamp("pp_d1_b", 0.4)],
        d2: vec![stamp("pp_d2_a", 1.2), stamp("pp_d2_b", 1.0)],
        d3: vec![d3],
        laminate: None,
        pressure_pane: Some(PressurePaneRules {
            pane_gap_m: 0.012,
            inner_tint: srgb8([242, 247, 249]),
            chunks: ChunkBurst::default(),
            breach: true,
        }),
    };
    let stamps = vec![
        pressure_d1(41),
        pressure_d1(42),
        pressure_d2(51),
        pressure_d2(52),
        pressure_d3(61),
    ];
    CookedCrackSet { manifest, stamps }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a over the bytes (no dependency; the pinned hashes below come from it).
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    #[test]
    fn built_in_sets_pass_the_cook_rules() {
        for set in [laminate_set(), pressure_pane_set()] {
            set.manifest.validate().expect("manifest");
            set.validate_pixels().expect("pixels");
            let bytes = set.to_bytes().expect("bytes");
            assert_eq!(CookedCrackSet::from_bytes(&bytes).expect("decode"), set);
        }
    }

    #[test]
    fn stamps_have_cracks_and_d3_laminate_holes_stay_small() {
        let set = laminate_set();
        for px in &set.stamps {
            let cracked = px.mask.as_chunks::<4>().0.iter().filter(|t| t[0] > 128).count();
            assert!(cracked > 50, "stamp with {cracked} crack texels");
        }
        let punch = &set.stamps[set.level_range(3).start];
        let decl = &set.manifest.d3[0];
        let extent = punch.hole_extent(decl.pivot, decl.size_m, 128);
        assert!(extent > 0.01 && extent <= 0.035, "laminate hole extent {extent}");
        let pp = pressure_pane_set();
        let fail = &pp.stamps[pp.level_range(3).start];
        let holes = fail.mask.as_chunks::<4>().0.iter().filter(|t| t[2] > 128).count();
        assert!(
            holes > 512 * 512 / 3,
            "a failed pressure pane is mostly gone ({holes})"
        );
    }

    /// Audit G finding 4 (render5 determinism): the built-in stamps regenerate in
    /// process from a fixed SplitMix64 seed, and the saved state is the impact list,
    /// so a replay must rebuild the same page on every OS. The pinned hashes are the
    /// software-libm result: with the platform libm the mask hash comes out
    /// `0x58e4_7eec_05ef_2f88` on the reference host (measured by routing soft_math to
    /// the builtins), so these pins fail on a revert there.
    #[test]
    fn laminate_d3_hole_stamp_is_pinned() {
        let stamp = laminate_d3(1, true);
        assert_eq!(fnv1a(&stamp.mask), 0x8c3d_5f49_7297_a8d3);
        assert_eq!(fnv1a(&stamp.normal), 0x91ab_a53a_fb99_532b);
    }

    /// The stamped page at a non-zero rotation is pinned the same way (the rotation
    /// trig runs at stamp time, `glass/mask.rs`). The footprint is 1:1 so stamp-texel
    /// differences reach the page bytes instead of averaging away in minification.
    #[test]
    fn stamped_page_at_a_rotation_is_pinned() {
        let stamp = laminate_d3(1, true);
        let mut page = crate::glass::mask::GlassMask::new(128);
        page.stamp(
            &stamp,
            &crate::glass::mask::StampPlacement {
                center: [64.0, 64.0],
                footprint: [512.0, 512.0],
                pivot: [0.5, 0.5],
                rotation: 0.61,
                hole_radius: f32::INFINITY,
                hole_scale: 1.0,
                halo_radius: 0.0,
            },
        );
        assert_eq!(fnv1a(&page.rgba), 0x99d0_1a7d_784a_1763);
        assert_eq!(fnv1a(&page.normal), 0x8524_74a1_38c8_1361);
    }

    /// fixrev-19-k (audit G finding 4, render5 determinism): the first pass pinned only
    /// `laminate_d3` and one stamped page, and missed that `laminate_d1`'s spall chips
    /// still called the platform libm — the exact per-OS divergence the finding asked to
    /// remove. Every stamp in both built-in sets is pinned here: mask and normal, in the
    /// order the sets build them. The values are the software-libm result; on a platform
    /// libm the D1 rows come out differently (that is what the pin guards).
    #[test]
    fn every_built_in_stamp_is_pinned() {
        let names = [
            "lam_d1_a",
            "lam_d1_b",
            "lam_d1_c",
            "lam_d2_a",
            "lam_d2_b",
            "lam_d3_punch",
            "lam_d3_crazed",
            "pp_d1_a",
            "pp_d1_b",
            "pp_d2_a",
            "pp_d2_b",
            "pp_d3_fail",
        ];
        let laminate = laminate_set();
        let pane = pressure_pane_set();
        let actual: Vec<(u64, u64)> = laminate
            .stamps
            .iter()
            .chain(&pane.stamps)
            .map(|px| (fnv1a(&px.mask), fnv1a(&px.normal)))
            .collect();
        for (name, (mask, normal)) in names.iter().zip(&actual) {
            println!("{name}: mask {mask:#018x} normal {normal:#018x}");
        }
        let expected: [(u64, u64); 12] = [
            (0x544b_69f6_7afc_d12f, 0xbb5c_7b6d_582a_784f),
            (0xd580_8264_87c3_e922, 0x7ea6_4cf2_6fca_46e3),
            (0x3e02_52a4_d322_644b, 0xb230_a738_3731_7861),
            (0xd236_79d2_016c_70e5, 0x9c89_fc0a_1925_c2e9),
            (0xe210_a8a8_bfeb_1a48, 0x3938_3c00_c560_4fb0),
            (0x7016_5128_4118_c55a, 0x0caa_1741_d655_13db),
            (0xae28_ea94_ae6b_4a73, 0x64f9_d434_7a48_2266),
            (0xb4c9_acc2_6bd4_5afe, 0x7866_2d0a_5307_4d8e),
            (0xd889_b5c4_43c7_28aa, 0x73a9_d6c3_ec6c_68fe),
            (0xbcd8_55ec_f9c9_b147, 0x601f_2033_7ebe_155f),
            (0xa843_f4ea_2ddd_b63a, 0xd8c5_5d34_30ca_dcf8),
            (0xd7e1_3e0c_491e_67ef, 0x68ef_6ef7_f23f_f6e2),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((name, (mask, normal)), (want_mask, want_normal)) in names.iter().zip(&actual).zip(expected) {
            assert_eq!(*mask, want_mask, "{name} mask");
            assert_eq!(*normal, want_normal, "{name} normal");
        }
    }
}
