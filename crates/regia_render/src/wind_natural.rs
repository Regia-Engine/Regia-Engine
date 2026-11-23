// SPDX-License-Identifier: MPL-2.0
//! The `natural` wind style (ADR-2446): a gust field with calm spells, sway slower for taller
//! blades, a spatially varying phase and a per-species flutter band. The CPU reference of the
//! terrain grass shader's `grass_natural_wave` and of the `natural` foliage stage (keep them in
//! step); [`WindStyle::Classic`] stays the default and is untouched. Shared by `regia_terrain`.
//!
//! ## Provenance
//!
//! Regia's formulation. The gust field is a value-noise field ("lattice noise", Perlin 1985
//! `An Image Synthesizer`, SIGGRAPH; the value variant is described in Ebert et al., *Texturing
//! and Modeling*) carried along the wind at a gust speed, the frozen-turbulence idea of Taylor
//! (1938, "The spectrum of turbulence", Proc. R. Soc. A 164); the calm spells come from a
//! smoothstep threshold of that field. Natural sway frequency falling with plant height follows
//! the cantilever-beam result `f ∝ 1/√L` for a bending stem under its own weight (Euler-Bernoulli
//! beam theory; see Niklas, *Plant Biomechanics*, 1992). The integer hash is the PCG variant of
//! Jarzynski & Olano, "Hash Functions for GPU Rendering", JCGT 9(3), 2020.

const TAU: f32 = std::f32::consts::TAU;

/// How the wind moves the blades. Both are stateless functions of position and the sway clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindStyle {
    /// Two sine harmonics on a plane wave (the grass's original sway; the default).
    #[default]
    Classic,
    /// A travelling gust field with calm spells, height-scaled frequency, decorrelated phase and a
    /// per-species flutter band ([`natural_wave`]).
    Natural,
}

impl WindStyle {
    /// The name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Natural => "natural",
        }
    }

    /// Parses [`WindStyle::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "classic" => Some(Self::Classic),
            "natural" => Some(Self::Natural),
            _ => None,
        }
    }

    /// The word the grass uniform carries (`counts2.y`).
    #[must_use]
    pub fn gpu_word(self) -> u32 {
        match self {
            Self::Classic => 0,
            Self::Natural => 1,
        }
    }
}

/// Metres per second the gust pattern travels along the wind (large octave).
const GUST_SPEED_BIG: f32 = 3.2;
/// Metres per second of the small octave (a different speed decorrelates the two).
const GUST_SPEED_SMALL: f32 = 2.1;
/// Noise value below which the air is calm, and above which gusts are at full strength.
const CALM_LO: f32 = 0.40;
const CALM_HI: f32 = 0.74;

/// PCG hash (`grass_pcg` in the shaders).
#[must_use]
pub fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / 16_777_216.0
}

fn lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    unit(pcg(
        (ix as u32).wrapping_mul(0x9E37_79B1) ^ pcg((iy as u32) ^ seed)
    ))
}

/// Value noise in `0..1` at `(x, y)`.
#[must_use]
pub fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = (x - fx, y - fy);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let (ix, iy) = (fx as i32, fy as i32);
    let a = lattice(ix, iy, seed);
    let b = lattice(ix + 1, iy, seed);
    let c = lattice(ix, iy + 1, seed);
    let d = lattice(ix + 1, iy + 1, seed);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sy
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The gust strength `0..=1` at world `pos_xz` and `time_s` for a unit wind `dir`: two travelling
/// noise octaves through a threshold, so it goes to exactly 0 in calm spells.
#[must_use]
pub fn gust_intensity(pos_xz: [f32; 2], dir: [f32; 2], time_s: f32) -> f32 {
    let u = pos_xz[0] * dir[0] + pos_xz[1] * dir[1];
    let v = -pos_xz[0] * dir[1] + pos_xz[1] * dir[0];
    let big = value_noise((u - GUST_SPEED_BIG * time_s) / 46.0, v / 30.0, 0x51ed_270b);
    let small = value_noise(
        (u - GUST_SPEED_SMALL * time_s) / 14.0 + 17.3,
        (v + 0.8 * time_s) / 11.0 + 5.1,
        0x9c3a_7d15,
    );
    smoothstep(CALM_LO, CALM_HI, 0.65 * big + 0.35 * small)
}

/// The spatially varying phase (radians): two decorrelated noise octaves of position plus the
/// per-blade draw `jitter` in `0..1`. No dominant direction, so no regular wave pattern.
#[must_use]
pub fn natural_phase(pos_xz: [f32; 2], jitter: f32) -> f32 {
    let a = value_noise(pos_xz[0] / 6.0 + 31.0, pos_xz[1] / 6.0, 0x2f6b_1a93);
    let b = value_noise(pos_xz[0] / 2.3, pos_xz[1] / 2.3 + 9.0, 0x7be1_44c7);
    TAU * 2.0 * (0.6 * a + 0.4 * b) + 1.5 * jitter
}

/// The sway frequency factor for a blade `height_m` tall: `1/√height` about 0.4 m, bounded.
#[must_use]
pub fn height_frequency_factor(height_m: f32) -> f32 {
    (0.4 / height_m.max(0.02)).sqrt().clamp(0.6, 1.25)
}

/// The flutter frequency multiplier of a species (class stream seed): `2.6..4.6` times the sway.
#[must_use]
pub fn flutter_band(species_seed: u32) -> f32 {
    2.6 + 2.0 * unit(pcg(species_seed ^ 0xf10a_77e5))
}

/// What [`natural_wave`] needs to know about one blade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NaturalBlade {
    /// The clump position, world XZ.
    pub pos_xz: [f32; 2],
    /// The unit wind direction.
    pub dir: [f32; 2],
    /// The per-blade draw in `0..1`.
    pub jitter: f32,
    /// The class sway frequency (Hz) times the wind's frequency scale.
    pub hz: f32,
    /// The blade height (m).
    pub height_m: f32,
    /// The wind's `gust` field, the weight of the flutter band.
    pub flutter_weight: f32,
    /// The class stream seed (picks the species' flutter band).
    pub species_seed: u32,
}

/// The signed sway wave of a blade, the `natural` counterpart of `TerrainWind::gust_wave`:
/// `0` in calm air; mostly downwind, `|wave| <= 1 + 0.35 * flutter_weight`.
#[must_use]
pub fn natural_wave(blade: &NaturalBlade, time_s: f32) -> f32 {
    let intensity = gust_intensity(blade.pos_xz, blade.dir, time_s);
    let phase = natural_phase(blade.pos_xz, blade.jitter);
    let w = TAU * blade.hz * 0.55 * height_frequency_factor(blade.height_m) * time_s;
    let sway = 0.55 + 0.45 * (w + phase).sin();
    let wf = TAU * blade.hz * flutter_band(blade.species_seed) * time_s;
    let flutter = blade.flutter_weight * 0.35 * intensity * (wf + 1.9 * phase + 2.1).sin();
    intensity * (sway + flutter)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: [f32; 2] = [0.958, 0.287];

    fn blade(pos_xz: [f32; 2], jitter: f32, hz: f32, height_m: f32, species_seed: u32) -> NaturalBlade {
        NaturalBlade {
            pos_xz,
            dir: DIR,
            jitter,
            hz,
            height_m,
            flutter_weight: 0.35,
            species_seed,
        }
    }

    /// The grass's classic sway wave (`regia_terrain::TerrainWind::gust_wave`, default wind).
    fn classic_wave(phase: f32, hz: f32, t: f32) -> f32 {
        let w = TAU * hz * t;
        (w + phase).sin() + 0.35 * (2.3 * w + 1.7 * phase).sin()
    }

    fn series(pos: [f32; 2], n: usize, dt: f32) -> Vec<f32> {
        (0..n).map(|i| gust_intensity(pos, DIR, i as f32 * dt)).collect()
    }

    #[test]
    fn calm_spells_really_reach_zero() {
        // Many places, 20 minutes at 0.5 s.
        let mut calm = 0usize;
        let mut total = 0usize;
        let mut max = 0.0f32;
        for p in 0..16 {
            let pos = [p as f32 * 37.0 - 200.0, p as f32 * -23.0 + 50.0];
            for g in series(pos, 2400, 0.5) {
                total += 1;
                calm += usize::from(g <= 0.0);
                max = max.max(g);
            }
        }
        let fraction = calm as f32 / total as f32;
        assert!((0.10..0.60).contains(&fraction), "calm fraction {fraction}");
        assert!(max > 0.95, "gusts reach full strength: {max}");
    }

    #[test]
    fn intensity_is_smooth_and_decorrelates_over_tens_of_seconds() {
        let s = series([10.0, -4.0], 4000, 0.25);
        let jump = s.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(jump < 0.2, "no jumps between quarter-second samples: {jump}");
        let ac = |lag: usize| autocorr(&s, lag);
        assert!(ac(2) > 0.9, "smooth at 0.5 s: {}", ac(2));
        assert!(ac(160) < 0.5, "decorrelated at 40 s: {}", ac(160));
    }

    fn autocorr(s: &[f32], lag: usize) -> f32 {
        let n = s.len() - lag;
        let mean = s.iter().sum::<f32>() / s.len() as f32;
        let var: f32 = s.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / s.len() as f32;
        let cov: f32 = (0..n).map(|i| (s[i] - mean) * (s[i + lag] - mean)).sum::<f32>() / n as f32;
        cov / var
    }

    /// Largest normalized autocorrelation after the first lag where it drops below zero: a
    /// periodic motion (the classic sines) comes back to near 1 at its period; the gust field,
    /// once decorrelated, does not.
    fn periodicity_peak(s: &[f32]) -> f32 {
        let max_lag = s.len() / 3;
        let first_zero = (1..max_lag)
            .find(|l| autocorr(s, *l) < 0.0)
            .expect("autocorrelation reaches zero");
        (first_zero..max_lag)
            .map(|l| autocorr(s, l))
            .fold(f32::MIN, f32::max)
    }

    #[test]
    fn the_wave_has_no_periodicity_peak_where_the_classic_one_has() {
        let dt = 0.1;
        let n = 6000;
        let pos = [12.0, 7.0];
        let natural: Vec<f32> = (0..n)
            .map(|i| natural_wave(&blade(pos, 0.4, 0.6, 0.5, 77), i as f32 * dt))
            .collect();
        let classic: Vec<f32> = (0..n).map(|i| classic_wave(1.3, 0.6, i as f32 * dt)).collect();
        let classic_peak = periodicity_peak(&classic);
        let natural_peak = periodicity_peak(&natural);
        assert!(classic_peak > 0.9, "the classic wave repeats: {classic_peak}");
        // Wave = intensity * oscillation: the oscillation is periodic but is gated by the
        // aperiodic gust field, so no lag brings it back.
        assert!(natural_peak < 0.5, "natural wave must not repeat: {natural_peak}");
    }

    #[test]
    fn calm_air_means_a_still_blade() {
        // Find a calm moment and check the wave is exactly zero there.
        let pos = [3.0, 3.0];
        let t = (0..20_000)
            .map(|i| i as f32 * 0.05)
            .find(|t| gust_intensity(pos, DIR, *t) == 0.0)
            .expect("a calm moment exists");
        assert_eq!(natural_wave(&blade(pos, 0.3, 0.7, 0.4, 5), t), 0.0);
    }

    #[test]
    fn taller_blades_sway_slower() {
        assert!(height_frequency_factor(1.6) < height_frequency_factor(0.4));
        assert!(height_frequency_factor(0.4) < height_frequency_factor(0.05));
        assert!((0.6..=1.25).contains(&height_frequency_factor(100.0)));
        assert!((0.6..=1.25).contains(&height_frequency_factor(0.0)));
    }

    #[test]
    fn flutter_band_differs_by_species_and_stays_in_range() {
        let bands: Vec<f32> = (0..32).map(flutter_band).collect();
        assert!(bands.iter().all(|b| (2.6..4.6).contains(b)));
        let min = bands.iter().copied().fold(f32::MAX, f32::min);
        let max = bands.iter().copied().fold(f32::MIN, f32::max);
        assert!(max - min > 1.0, "species spread {min}..{max}");
    }

    #[test]
    fn phase_has_no_dominant_direction() {
        // A plane wave has phase differences that are a linear function of position: along a
        // line the second difference is zero. The noise phase is not linear.
        let line = |dx: f32, dz: f32| -> f32 {
            let p = |i: f32| natural_phase([i * dx * 3.0, i * dz * 3.0], 0.0);
            (0..40)
                .map(|i| (p(i as f32 + 2.0) - 2.0 * p(i as f32 + 1.0) + p(i as f32)).abs())
                .sum::<f32>()
                / 40.0
        };
        for (dx, dz) in [(1.0, 0.0), (0.0, 1.0), (0.7, 0.7)] {
            assert!(line(dx, dz) > 0.2, "curved along ({dx},{dz}): {}", line(dx, dz));
        }
        // And neighbouring clumps are not in lock step: the phase differs by blade draw.
        assert_ne!(natural_phase([1.0, 1.0], 0.1), natural_phase([1.0, 1.0], 0.9));
    }

    #[test]
    fn wave_stays_bounded_and_finite() {
        let mut worst = 0.0f32;
        for i in 0..5000 {
            let t = i as f32 * 0.37;
            let w = natural_wave(
                &NaturalBlade {
                    flutter_weight: 1.0,
                    ..blade(
                        [i as f32 * 1.7, -(i as f32) * 0.9],
                        (i % 7) as f32 / 7.0,
                        1.3,
                        0.2,
                        i,
                    )
                },
                t,
            );
            assert!(w.is_finite());
            worst = worst.max(w.abs());
        }
        assert!(worst <= 1.35 + 1e-4, "{worst}");
    }

    #[test]
    fn style_names_round_trip() {
        for s in [WindStyle::Classic, WindStyle::Natural] {
            assert_eq!(WindStyle::from_name(s.name()), Some(s));
        }
        assert_eq!(WindStyle::from_name("wild"), None);
        assert_eq!(WindStyle::default(), WindStyle::Classic);
    }
}
