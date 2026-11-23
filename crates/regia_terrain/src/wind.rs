// SPDX-License-Identifier: MPL-2.0
//! Wind as data (`docs/specs/82-terrain-editing.md` § 4, ADR-1853): one [`TerrainWind`] that every
//! swaying thing reads. The grass draws it today (through [`crate::TerrainGrassHandle::set_wind`]
//! into the blade shader's `wind` uniform); [`TerrainWind::sway_offset`] is the CPU mirror of the
//! blade sway for foliage and gameplay later.
//!
//! The default reproduces the grass's old fixed wind exactly: direction `(1, 0.3)` (normalised on
//! the GPU), unit strength and frequency, and a 0.35-weight second harmonic. Wind is visual only: a
//! stateless function of position and clock, never read by the simulation.

use regia_app::prelude::*;
use regia_render::wind_natural::{NaturalBlade, WindStyle, natural_wave};
use serde::{Deserialize, Serialize};

/// `2π`, the blade shader's `GRASS_TAU`.
const TAU: f32 = std::f32::consts::TAU;
/// Shortest direction vector kept; a shorter one has no meaningful heading.
const MIN_DIRECTION_LEN: f32 = 1.0e-6;
/// Longest direction vector kept: its square stays finite in the shader's `normalize`.
const MAX_DIRECTION_LEN: f32 = 1.0e6;

/// The wind every swaying thing reads (a world resource; `terrain.wind` / `terrain.set_wind`).
#[derive(Resource, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerrainWind {
    /// Direction the wind blows toward, world XZ. Need not be unit length; its length must be
    /// in `1e-6..=1e6` ([`TerrainWind::usable_direction`]).
    pub direction: [f32; 2],
    /// Multiplies every sway amplitude, `0..=4` (0 = still air).
    pub strength: f32,
    /// Multiplies every sway frequency, `0.1..=4`.
    pub frequency_scale: f32,
    /// Weight of the second, faster harmonic (gustiness), `0..=1`.
    pub gust: f32,
    /// How the blades move ([`WindStyle`]); `Classic` (the default) is the original two-harmonic
    /// plane wave, `Natural` adds calm spells and decorrelated phase (ADR-2446).
    pub style: WindStyle,
}

impl Default for TerrainWind {
    fn default() -> Self {
        Self {
            direction: [1.0, 0.3],
            strength: 1.0,
            frequency_scale: 1.0,
            gust: 0.35,
            style: WindStyle::Classic,
        }
    }
}

impl TerrainWind {
    /// `true` when `direction` has a heading the shader can normalize: finite, not zero, and
    /// short enough that its square does not overflow.
    #[must_use]
    pub fn usable_direction(direction: [f32; 2]) -> bool {
        (MIN_DIRECTION_LEN..=MAX_DIRECTION_LEN).contains(&direction[0].hypot(direction[1]))
    }

    /// Every field in its documented range; non-finite values and an unusable direction
    /// ([`TerrainWind::usable_direction`]) take the default.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        let d = Self::default();
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        if !Self::usable_direction(self.direction) {
            self.direction = d.direction;
        }
        self.strength = fix(self.strength, 0.0, 4.0, d.strength);
        self.frequency_scale = fix(self.frequency_scale, 0.1, 4.0, d.frequency_scale);
        self.gust = fix(self.gust, 0.0, 1.0, d.gust);
        self
    }

    /// The direction normalised (the default's for a zero or non-finite direction).
    #[must_use]
    pub fn unit_direction(&self) -> [f32; 2] {
        let [x, z] = self.clamped().direction;
        let len = (x * x + z * z).sqrt();
        [x / len, z / len]
    }

    /// The sway wave: `sin(w + phase) + gust · sin(2.3 w + 1.7 phase)` with
    /// `w = 2π · hz · frequency_scale · time_s`; in `-(1 + gust)..=(1 + gust)`.
    #[must_use]
    pub fn gust_wave(&self, phase: f32, hz: f32, time_s: f32) -> f32 {
        // The values the grass shader reads (`gpu_words`), not the raw public fields.
        let wind = self.clamped();
        let w = TAU * hz * wind.frequency_scale * time_s;
        (w + phase).sin() + wind.gust * (2.3 * w + 1.7 * phase).sin()
    }

    /// The horizontal offset of a blade point, the grass shader's sway on the CPU: a blade
    /// rooted at `pos_xz` with per-clump `jitter` (`0..1`), the class's `[amplitude_m, hz]`, the
    /// request's `sway_scale`, at height fraction `s` (`0` root, `1` tip).
    /// The offset grows with `s²` and lies along [`TerrainWind::unit_direction`].
    #[must_use]
    pub fn sway_offset(
        &self,
        pos_xz: [f32; 2],
        jitter: f32,
        [amplitude_m, hz]: [f32; 2],
        sway_scale: f32,
        s: f32,
        time_s: f32,
    ) -> [f32; 2] {
        let wind = self.clamped();
        let [dx, dz] = wind.unit_direction();
        let phase = (pos_xz[0] * dx + pos_xz[1] * dz) * 0.4 + jitter * 1.5;
        let k = amplitude_m * sway_scale * wind.strength * s * s * wind.gust_wave(phase, hz, time_s);
        [dx * k, dz * k]
    }

    /// The horizontal offset of a blade point under [`WindStyle::Natural`], the grass shader's
    /// natural sway on the CPU. `s_height_seed` is `(s, height_m, species_seed)`: the height
    /// fraction, the blade height and the class stream seed; the other arguments are those of
    /// [`TerrainWind::sway_offset`].
    #[must_use]
    pub fn natural_offset(
        &self,
        pos_xz: [f32; 2],
        jitter: f32,
        [amplitude_m, hz]: [f32; 2],
        sway_scale: f32,
        (s, height_m, species_seed): (f32, f32, u32),
        time_s: f32,
    ) -> [f32; 2] {
        let wind = self.clamped();
        let dir = wind.unit_direction();
        let wave = natural_wave(
            &NaturalBlade {
                pos_xz,
                dir,
                jitter,
                hz: hz * wind.frequency_scale,
                height_m,
                flutter_weight: wind.gust,
                species_seed,
            },
            time_s,
        );
        let k = amplitude_m * sway_scale * wind.strength * s * s * wave;
        [dir[0] * k, dir[1] * k]
    }

    /// The `wind` vec4 of the grass uniform: direction (as given; the shader normalises),
    /// frequency scale, gust weight.
    #[must_use]
    pub fn gpu_words(&self) -> [f32; 4] {
        let w = self.clamped();
        [w.direction[0], w.direction[1], w.frequency_scale, w.gust]
    }
}

/// The scene wind speed (m/s, 10 m up) at which [`couple_scene_wind`] gives unit
/// [`TerrainWind::strength`], the sway the grass was tuned for (spec 88 § 2.4 uses the same
/// 5 m/s reference for `sway × speed / 5`).
pub const SCENE_WIND_REFERENCE_M_PER_S: f32 = 5.0;

/// Steps [`couple_scene_wind`] rounds to, so a slowly changing weather writes the resource (and
/// redraws the grass's cached shadows) only when the sway visibly changes: strength in 1/32,
/// heading in whole degrees.
const STRENGTH_STEPS: f32 = 32.0;
/// Mean winds slower than this (m/s) have no usable heading: the direction is kept.
const CALM_M_PER_S: f32 = 0.05;

impl TerrainWind {
    /// `base` steered by a scene wind's gust-free `mean_m_per_s` (spec 99 § 3.3): the direction
    /// is the mean's heading rounded to whole degrees, the strength is its speed over
    /// [`SCENE_WIND_REFERENCE_M_PER_S`] rounded to 1/32 (clamped to `0..=4`). The frequency scale
    /// and the gust weight stay `base`'s (the grass draws the gusting with its own harmonic).
    /// A calm or non-finite mean keeps `base`'s direction with strength 0 (non-finite: `base`).
    #[must_use]
    pub fn steered_by(base: Self, mean_m_per_s: [f32; 2]) -> Self {
        let [x, z] = mean_m_per_s;
        if !(x.is_finite() && z.is_finite()) {
            return base;
        }
        let speed = x.hypot(z);
        let strength = ((speed / SCENE_WIND_REFERENCE_M_PER_S).clamp(0.0, 4.0) * STRENGTH_STEPS).round()
            / STRENGTH_STEPS;
        if speed < CALM_M_PER_S {
            return Self {
                strength: 0.0,
                ..base
            };
        }
        let heading = z.atan2(x).to_degrees().round().to_radians();
        let (sin, cos) = heading.sin_cos();
        Self {
            direction: [cos, sin],
            strength,
            ..base
        }
    }
}

/// Lets the weather's [`regia_scene::weather::SceneWind`] steer the [`TerrainWind`] (spec 99
/// § 3.3, the shared wind of ADR-1853), in [`crate::TerrainPlugin`]'s `Update` before
/// [`sync_terrain_wind`]. Nothing happens without a `SceneWind` (the project's
/// `effects.scene_wind` off, the default) or while it is `Off`.
///
/// It reads the gust-free mean ([`TerrainWind::steered_by`]) and writes only when that result
/// changes, so the grass's shadow cache does not redraw for every gust, and a direct
/// `terrain.set_wind` (or a host write) holds until the weather's mean wind changes a step.
/// A missing [`TerrainWind`] is inserted the first time.
pub fn couple_scene_wind(
    mut commands: Commands,
    scene: Option<Res<regia_scene::weather::SceneWind>>,
    wind: Option<ResMut<TerrainWind>>,
    mut last: Local<Option<TerrainWind>>,
) {
    let Some(scene) = scene else {
        *last = None;
        return;
    };
    if scene.mode == regia_scene::weather::SceneWindMode::Off {
        *last = None;
        return;
    }
    let base = wind.as_deref().copied().unwrap_or_default();
    let steered = TerrainWind::steered_by(base, scene.mean_m_per_s);
    let key = TerrainWind::steered_by(TerrainWind::default(), scene.mean_m_per_s);
    if *last == Some(key) {
        return;
    }
    *last = Some(key);
    match wind {
        Some(mut wind) => {
            if *wind != steered {
                *wind = steered;
            }
        }
        None => commands.insert_resource(steered),
    }
}

/// Copies a changed [`TerrainWind`] resource into the world's [`crate::TerrainGrassHandle`]
/// ([`crate::TerrainPlugin`], `Update`). Nothing happens while the wind is unchanged.
pub fn sync_terrain_wind(wind: Option<Res<TerrainWind>>, grass: Option<Res<crate::TerrainGrassHandle>>) {
    let (Some(wind), Some(grass)) = (wind, grass) else {
        return;
    };
    if wind.is_changed() || grass.is_added() {
        grass.set_wind(*wind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_mirror_reads_the_clamped_values_the_shader_gets() {
        let wild = TerrainWind {
            strength: 10.0,
            frequency_scale: f32::NAN,
            gust: 5.0,
            ..TerrainWind::default()
        };
        let clamped = wild.clamped();
        let args = ([3.0, 4.0], 0.5, [0.3, 1.2], 1.0, 0.8, 2.7);
        let a = wild.sway_offset(args.0, args.1, args.2, args.3, args.4, args.5);
        let b = clamped.sway_offset(args.0, args.1, args.2, args.3, args.4, args.5);
        assert_eq!(a, b);
        assert!(a.iter().all(|v| v.is_finite()));
        assert_eq!(wild.gust_wave(0.4, 1.0, 2.7), clamped.gust_wave(0.4, 1.0, 2.7));
    }

    /// The shader's old fixed sway, written out as it was before the wind block.
    fn legacy_offset(pos: [f32; 2], u8_: f32, amp: f32, hz: f32, scale: f32, s: f32, time: f32) -> [f32; 2] {
        let len = (1.0f32 + 0.09).sqrt();
        let wind = [1.0 / len, 0.3 / len];
        let phase = (pos[0] * wind[0] + pos[1] * wind[1]) * 0.4 + u8_ * 1.5;
        let w = TAU * hz * time;
        let gust = (w + phase).sin() + 0.35 * (2.3 * w + 1.7 * phase).sin();
        let k = amp * scale * s * s * gust;
        [wind[0] * k, wind[1] * k]
    }

    #[test]
    fn the_default_wind_is_the_old_fixed_sway() {
        let wind = TerrainWind::default();
        for (i, t) in [0.0f32, 0.37, 1.5, 12.25, 300.0].into_iter().enumerate() {
            let pos = [i as f32 * 13.7 - 40.0, 9.0 - i as f32 * 3.1];
            let a = wind.sway_offset(pos, 0.3, [0.08, 0.6], 1.0, 0.8, t);
            let b = legacy_offset(pos, 0.3, 0.08, 0.6, 1.0, 0.8, t);
            assert!(
                (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6,
                "{a:?} vs {b:?} at {t}"
            );
        }
        assert_eq!(wind.gpu_words(), [1.0, 0.3, 1.0, 0.35]);
    }

    #[test]
    fn sway_follows_direction_strength_and_root() {
        let wind = TerrainWind {
            direction: [0.0, -2.0],
            strength: 2.0,
            ..TerrainWind::default()
        };
        assert_eq!(wind.unit_direction(), [0.0, -1.0]);
        let at = |s: f32| wind.sway_offset([5.0, 5.0], 0.5, [0.1, 0.7], 1.0, s, 0.9);
        assert_eq!(at(0.0), [0.0, 0.0]);
        let tip = at(1.0);
        assert_eq!(tip[0], 0.0);
        let still = TerrainWind {
            strength: 0.0,
            ..wind
        };
        assert_eq!(
            still.sway_offset([5.0, 5.0], 0.5, [0.1, 0.7], 1.0, 1.0, 0.9),
            [0.0, 0.0]
        );
        let half = TerrainWind {
            strength: 1.0,
            ..wind
        }
        .sway_offset([5.0, 5.0], 0.5, [0.1, 0.7], 1.0, 1.0, 0.9);
        assert!((tip[1] - 2.0 * half[1]).abs() < 1e-6);
    }

    #[test]
    fn frequency_scale_speeds_the_clock() {
        let base = TerrainWind::default();
        let fast = TerrainWind {
            frequency_scale: 2.0,
            ..base
        };
        assert!((fast.gust_wave(0.4, 0.5, 1.0) - base.gust_wave(0.4, 0.5, 2.0)).abs() < 1e-5);
    }

    #[test]
    fn natural_offset_is_still_in_calm_air_and_follows_the_wind() {
        let wind = TerrainWind {
            style: WindStyle::Natural,
            ..TerrainWind::default()
        };
        let [dx, dz] = wind.unit_direction();
        let mut moved = 0;
        let mut still = 0;
        for i in 0..4000 {
            let t = i as f32 * 0.5;
            let o = wind.natural_offset([8.0, -3.0], 0.4, [0.1, 0.7], 1.0, (1.0, 0.5, 9), t);
            assert!(o.iter().all(|v| v.is_finite()));
            // Always along the wind's line.
            assert!((o[0] * dz - o[1] * dx).abs() < 1e-6, "{o:?}");
            if o == [0.0, 0.0] {
                still += 1;
            } else {
                moved += 1;
            }
        }
        assert!(still > 300 && moved > 300, "calm {still}, moving {moved}");
        let root = wind.natural_offset([8.0, -3.0], 0.4, [0.1, 0.7], 1.0, (0.0, 0.5, 9), 12.0);
        assert_eq!(root, [0.0, 0.0]);
    }

    #[test]
    fn clamped_repairs_bad_input() {
        let w = TerrainWind {
            direction: [0.0, 0.0],
            strength: f32::NAN,
            frequency_scale: 99.0,
            gust: -1.0,
            ..TerrainWind::default()
        }
        .clamped();
        assert_eq!(w.direction, [1.0, 0.3]);
        assert_eq!((w.strength, w.frequency_scale, w.gust), (1.0, 4.0, 0.0));
        let inf = TerrainWind {
            direction: [f32::INFINITY, 1.0],
            ..TerrainWind::default()
        };
        assert_eq!(inf.unit_direction(), TerrainWind::default().unit_direction());
        // Finite but so long that x² overflows: the shader's normalize would give nothing.
        let huge = TerrainWind {
            direction: [1.0e30, 0.0],
            ..TerrainWind::default()
        };
        assert_eq!(huge.clamped().direction, [1.0, 0.3]);
        assert_eq!(huge.gpu_words()[..2], [1.0, 0.3]);
        let long = TerrainWind {
            direction: [3.0e5, 4.0e5],
            ..TerrainWind::default()
        };
        assert_eq!(long.clamped().direction, [3.0e5, 4.0e5]);
        assert!((long.unit_direction()[0] - 0.6).abs() < 1e-6);
        assert!(!TerrainWind::usable_direction([f32::NAN, 1.0]));
        assert!(!TerrainWind::usable_direction([1.0e-7, 0.0]));
    }

    #[test]
    fn steered_by_takes_heading_and_speed_and_keeps_the_rest() {
        let base = TerrainWind {
            frequency_scale: 1.5,
            gust: 0.6,
            ..TerrainWind::default()
        };
        let w = TerrainWind::steered_by(base, [0.0, -10.0]);
        assert!(
            w.direction[0].abs() < 1e-6 && (w.direction[1] + 1.0).abs() < 1e-6,
            "{w:?}"
        );
        assert_eq!((w.strength, w.frequency_scale, w.gust), (2.0, 1.5, 0.6));
        // The reference speed is unit strength; a gale clamps to 4.
        assert_eq!(TerrainWind::steered_by(base, [5.0, 0.0]).strength, 1.0);
        assert_eq!(TerrainWind::steered_by(base, [400.0, 0.0]).strength, 4.0);
        // Calm keeps the heading and stills the grass; garbage changes nothing.
        let calm = TerrainWind::steered_by(base, [0.01, 0.0]);
        assert_eq!((calm.direction, calm.strength), (base.direction, 0.0));
        assert_eq!(TerrainWind::steered_by(base, [f32::NAN, 1.0]), base);
        // Rounded: a 0.2 degree and 0.01 m/s change is the same wind.
        assert_eq!(
            TerrainWind::steered_by(base, [7.0, 3.0]),
            TerrainWind::steered_by(base, [7.0 * 1.0005, 3.0 * 1.001])
        );
    }

    fn coupled_app() -> App {
        let mut app = App::new();
        app.add_systems(Update, (couple_scene_wind, sync_terrain_wind).chain());
        app
    }

    #[test]
    fn without_a_scene_wind_nothing_changes() {
        let mut app = coupled_app();
        app.update();
        assert!(
            app.world().get_resource::<TerrainWind>().is_none(),
            "nothing inserted"
        );
        let own = TerrainWind {
            direction: [0.0, 1.0],
            strength: 0.25,
            ..TerrainWind::default()
        };
        app.insert_resource(own);
        app.update();
        assert_eq!(*app.world().resource::<TerrainWind>(), own);
        app.insert_resource(regia_scene::weather::SceneWind {
            mode: regia_scene::weather::SceneWindMode::Off,
            ..regia_scene::weather::SceneWind::manual([9.0, 0.0])
        });
        app.update();
        assert_eq!(
            *app.world().resource::<TerrainWind>(),
            own,
            "Off is no scene wind"
        );
    }

    #[test]
    fn the_weather_steers_the_grass_wind_by_its_mean() {
        use regia_scene::weather::{SceneWind, SceneWindMode};
        let mut app = coupled_app();
        let handle = crate::TerrainGrassHandle::new_linked();
        app.insert_resource(handle.clone());
        app.insert_resource(SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [11.0, 0.0],
            mean_m_per_s: [10.0, 0.0],
        });
        app.update();
        app.update();
        let wind = *app.world().resource::<TerrainWind>();
        assert_eq!((wind.direction, wind.strength), ([1.0, 0.0], 2.0));
        assert_eq!(handle.wind(), wind, "reached the grass");
        // Gusts move the velocity, not the mean: the resource is not written again.
        let tick = app.world().resource_ref::<TerrainWind>().last_changed();
        for gust in [6.0, 14.0, 9.5] {
            app.world_mut().resource_mut::<SceneWind>().velocity_m_per_s = [gust, 0.5];
            app.update();
        }
        assert_eq!(app.world().resource_ref::<TerrainWind>().last_changed(), tick);
        // A direct write holds while the mean holds, and the next step of the mean wins.
        let own = TerrainWind {
            direction: [0.0, 1.0],
            strength: 0.5,
            ..wind
        };
        app.insert_resource(own);
        app.update();
        assert_eq!(*app.world().resource::<TerrainWind>(), own);
        app.world_mut().resource_mut::<SceneWind>().mean_m_per_s = [0.0, 15.0];
        app.update();
        let wind = *app.world().resource::<TerrainWind>();
        assert!(wind.direction[0].abs() < 1e-6 && (wind.direction[1] - 1.0).abs() < 1e-6);
        assert_eq!(wind.strength, 3.0);
        // A manual scene wind steers it as well.
        app.insert_resource(SceneWind::manual([-5.0, 0.0]));
        app.update();
        let wind = *app.world().resource::<TerrainWind>();
        assert!((wind.direction[0] + 1.0).abs() < 1e-6 && wind.direction[1].abs() < 1e-6);
        assert_eq!(wind.strength, 1.0);
    }

    #[test]
    fn sync_pushes_the_resource_into_the_grass_handle() {
        let mut app = App::new();
        let handle = crate::TerrainGrassHandle::new_linked();
        app.insert_resource(handle.clone());
        app.insert_resource(TerrainWind {
            direction: [0.0, 1.0],
            strength: 0.5,
            ..TerrainWind::default()
        });
        app.add_systems(Update, sync_terrain_wind);
        app.update();
        assert_eq!(handle.wind().direction, [0.0, 1.0]);
        assert_eq!(handle.wind().strength, 0.5);
        // An untouched resource is not pushed again: a direct handle write survives a frame.
        handle.set_wind(TerrainWind::default());
        app.update();
        assert_eq!(handle.wind(), TerrainWind::default());
        app.world_mut().resource_mut::<TerrainWind>().gust = 0.9;
        app.update();
        assert_eq!(handle.wind().gust, 0.9);
    }
}
