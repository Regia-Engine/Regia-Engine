// SPDX-License-Identifier: MPL-2.0
//! The ambience bed: looping layers (calm air, wind, gusts, leaf rustle, rain, drips) whose
//! levels follow the scene's wind and weather (ADR-2442).
//!
//! The bed reads the same [`SceneWind`] the renderer bends the foliage with
//! (`regia_render::FoliageSway`, spec 88) and the [`SurfaceWeather`] the surfaces shade with, so
//! what is heard matches what is drawn: the wind layers are silent while the scene wind is under
//! [`AmbienceBed::calm_m_per_s`] (the foliage is still or barely moving), the rustle layer follows
//! the leaf-flutter factor `speed / (speed + 5)` the sway stage uses, and the gust layer follows
//! the gust part of the wind (`|velocity| − |mean|`), which is what makes the foliage bend further.
//! Rain follows the falling rain rate; drips follow the wetness once the rain has stopped.
//!
//! Levels are pure functions of [`AmbienceInputs`] ([`BedLayer::target_db`]); the runtime then
//! slews every layer's amplitude with an attack and a release time constant
//! ([`AmbienceBed::step`]) so a gust swells and dies away instead of stepping.

use regia_ecs::prelude::*;
use regia_scene::weather::{SceneWind, SurfaceWeather};
use serde::{Deserialize, Serialize};

use crate::bus::MUTE_DB;
use crate::spatial::{db_to_gain, gain_to_db, polyline};
use crate::voices::SoundInstanceId;

/// What the bed hears of the scene: the inputs every layer's level is a function of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AmbienceInputs {
    /// The wind now, gusts included (m/s); 0 when the scene wind is off or absent.
    pub wind_m_per_s: f32,
    /// The wind without gusts (m/s).
    pub mean_wind_m_per_s: f32,
    /// Rain falling now (mm/h).
    pub rain_mm_per_h: f32,
    /// Snow falling now as water equivalent (mm/h).
    pub snow_mm_per_h: f32,
    /// The water film on exposed surfaces, `0..1`.
    pub wetness: f32,
}

fn finite_non_negative(x: f32) -> f32 {
    if x.is_finite() { x.max(0.0) } else { 0.0 }
}

impl AmbienceInputs {
    /// The inputs from the scene's weather resources. No [`SceneWind`] (or one in `Off` mode)
    /// is no wind; no [`SurfaceWeather`] is dry. NaN and negative values read as 0.
    #[must_use]
    pub fn from_weather(wind: Option<&SceneWind>, surface: Option<&SurfaceWeather>) -> Self {
        let (now, mean) = wind
            .and_then(|w| w.active().map(|_| (w.speed(), w.mean_speed())))
            .unwrap_or((0.0, 0.0));
        let surface = surface.copied().unwrap_or_default();
        Self {
            wind_m_per_s: finite_non_negative(now),
            mean_wind_m_per_s: finite_non_negative(mean),
            rain_mm_per_h: finite_non_negative(surface.rain_mm_per_h),
            snow_mm_per_h: finite_non_negative(surface.snow_we_mm_per_h),
            wetness: finite_non_negative(surface.wetness).min(1.0),
        }
    }

    /// The gust part of the wind: how much faster the wind is now than its mean (m/s, ≥ 0).
    #[must_use]
    pub fn gust_m_per_s(&self) -> f32 {
        (self.wind_m_per_s - self.mean_wind_m_per_s).max(0.0)
    }
}

/// The leaf-flutter factor of the foliage sway stage at `speed` m/s: `speed / (speed + 5)`
/// (0 at rest, 0.5 at 5 m/s, toward 1 in a storm). The rustle layer follows it, so leaves are
/// heard as much as they are seen to move.
#[must_use]
pub fn foliage_flutter(speed_m_per_s: f32) -> f32 {
    let s = finite_non_negative(speed_m_per_s);
    s / (s + 5.0)
}

/// What a bed layer's level follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BedDrive {
    /// A fixed level (the calm air tone of a forest): the curve is read at 1.
    #[default]
    Constant,
    /// The mean wind speed, m/s.
    Wind,
    /// The gust part of the wind, m/s ([`AmbienceInputs::gust_m_per_s`]).
    Gust,
    /// The leaf-flutter factor of the wind now, `0..1` ([`foliage_flutter`]).
    Rustle,
    /// The rain rate, mm/h.
    Rain,
    /// The wetness, `0..1`, only while no rain falls (drips after a shower).
    Drips,
    /// The snowfall rate, mm/h water equivalent.
    Snow,
}

impl BedDrive {
    /// Every drive, in declaration order.
    pub const ALL: [Self; 7] = [
        Self::Constant,
        Self::Wind,
        Self::Gust,
        Self::Rustle,
        Self::Rain,
        Self::Drips,
        Self::Snow,
    ];

    /// The stable lowercase name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Constant => "constant",
            Self::Wind => "wind",
            Self::Gust => "gust",
            Self::Rustle => "rustle",
            Self::Rain => "rain",
            Self::Drips => "drips",
            Self::Snow => "snow",
        }
    }

    /// The drive named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.name() == name)
    }

    /// `true` for the drives that follow the wind (silenced below the calm threshold).
    #[must_use]
    pub fn is_wind(self) -> bool {
        matches!(self, Self::Wind | Self::Gust | Self::Rustle)
    }

    /// The drive's value for `inputs`. The wind drives read 0 while the wind now is under
    /// `calm_m_per_s`.
    #[must_use]
    pub fn value(self, inputs: &AmbienceInputs, calm_m_per_s: f32) -> f32 {
        if self.is_wind() && inputs.wind_m_per_s < calm_m_per_s.max(0.0) {
            return 0.0;
        }
        match self {
            Self::Constant => 1.0,
            Self::Wind => inputs.mean_wind_m_per_s,
            Self::Gust => inputs.gust_m_per_s(),
            Self::Rustle => foliage_flutter(inputs.wind_m_per_s),
            Self::Rain => inputs.rain_mm_per_h,
            Self::Drips if inputs.rain_mm_per_h <= 0.0 => inputs.wetness,
            Self::Drips => 0.0,
            Self::Snow => inputs.snow_mm_per_h,
        }
    }
}

/// One looping layer of the bed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BedLayer {
    /// Layer name (unique in the bed).
    pub name: String,
    /// The looping sound, by its [`crate::SoundAssets`] name. `None` (or a name not registered
    /// yet) computes the level without playing anything.
    #[serde(default)]
    pub sound: Option<String>,
    /// What the level follows.
    pub drive: BedDrive,
    /// `(drive value, level dB)` points, linearly interpolated and clamped at both ends. A
    /// non-constant drive at or below 0 is silent whatever the curve says.
    pub curve: Vec<(f32, f32)>,
}

impl BedLayer {
    /// A layer with no sound yet.
    #[must_use]
    pub fn new(name: impl Into<String>, drive: BedDrive, curve: Vec<(f32, f32)>) -> Self {
        Self {
            name: name.into(),
            sound: None,
            drive,
            curve,
        }
    }

    /// The level this layer heads for, dB ([`MUTE_DB`] when silent).
    #[must_use]
    pub fn target_db(&self, inputs: &AmbienceInputs, calm_m_per_s: f32) -> f32 {
        let value = self.drive.value(inputs, calm_m_per_s);
        if self.drive != BedDrive::Constant && value <= 0.0 {
            return MUTE_DB;
        }
        if self.curve.is_empty() || !value.is_finite() {
            return MUTE_DB;
        }
        let db = polyline(&self.curve, value);
        if db.is_finite() {
            db.clamp(MUTE_DB, 12.0)
        } else {
            MUTE_DB
        }
    }
}

/// The live state of one layer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BedLayerState {
    /// The level the layer heads for, dB.
    pub target_db: f32,
    /// The slewed amplitude now (linear).
    pub gain: f32,
    /// The drive's value on the last step.
    pub drive_value: f32,
    /// The layer's looping voice, once its sound is registered.
    pub voice: Option<SoundInstanceId>,
    /// The sound name the voice plays (a changed name restarts the voice).
    pub voice_sound: Option<String>,
}

impl BedLayerState {
    /// The slewed level now, dB ([`MUTE_DB`] for silence).
    #[must_use]
    pub fn level_db(&self) -> f32 {
        if self.gain <= db_to_gain(MUTE_DB) {
            MUTE_DB
        } else {
            gain_to_db(self.gain).max(MUTE_DB)
        }
    }
}

/// The ambience bed (opt-in): insert it, register its layers' sounds in
/// [`crate::SoundAssets`], and `AudioPlugin`'s ambience system plays and levels them every frame.
///
/// # Example
///
/// ```
/// use regia_audio::{AmbienceBed, AmbienceInputs};
///
/// let mut bed = AmbienceBed::forest();
/// // A still day: the wind layers are silent.
/// bed.step(&AmbienceInputs::default(), 1.0);
/// assert!(bed.level_db("wind").expect("a forest layer") <= -99.0);
/// ```
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct AmbienceBed {
    /// The layers.
    pub layers: Vec<BedLayer>,
    /// Below this wind (m/s, gusts included) the wind, gust and rustle layers are silent.
    pub calm_m_per_s: f32,
    /// Time constant of a rising level, seconds.
    pub attack_secs: f32,
    /// Time constant of a falling level, seconds.
    pub release_secs: f32,
    /// The bus the layers play on.
    pub bus: String,
    /// The bed's master offset, dB (a mix trim; the bus volume stays the player's).
    pub volume_db: f32,
    /// Live state per layer (same order as [`Self::layers`]).
    pub state: Vec<BedLayerState>,
    /// The inputs of the last step.
    pub inputs: AmbienceInputs,
    /// The wind the bed hears while no [`SceneWind`] resource exists, m/s on world X and Z (no
    /// gusts). For a host that sets the renderer's foliage wind directly: set the same vector here
    /// so what is heard matches what is drawn. A `SceneWind`, when one exists, wins (it wins in the
    /// renderer too, spec 99 § 3.3). `None`: no `SceneWind` is calm.
    pub wind_override: Option<[f32; 2]>,
}

impl Default for AmbienceBed {
    fn default() -> Self {
        Self::forest()
    }
}

impl AmbienceBed {
    /// A temperate forest: a quiet air tone, wind and gusts, leaf rustle, rain and drips, each
    /// without a sound (register one per layer with [`Self::set_sound`]).
    ///
    /// The curves keep a calm day quiet: under 0.3 m/s nothing but the air tone (−42 dB); a
    /// light breeze (2 m/s) brings the wind to −36 dB and the rustle to −33 dB; a fresh breeze
    /// (8 m/s) −20 and −19 dB; a gale (17 m/s) −10 and −14 dB.
    #[must_use]
    pub fn forest() -> Self {
        Self::with_layers(vec![
            BedLayer::new("air", BedDrive::Constant, vec![(1.0, -42.0)]),
            BedLayer::new(
                "wind",
                BedDrive::Wind,
                vec![
                    (0.3, -54.0),
                    (2.0, -36.0),
                    (8.0, -20.0),
                    (17.0, -10.0),
                    (30.0, -5.0),
                ],
            ),
            BedLayer::new(
                "gust",
                BedDrive::Gust,
                vec![(0.2, -48.0), (2.0, -28.0), (6.0, -16.0)],
            ),
            BedLayer::new(
                "rustle",
                BedDrive::Rustle,
                vec![(0.05, -54.0), (0.3, -32.0), (0.6, -19.0), (0.8, -13.0)],
            ),
            BedLayer::new(
                "rain",
                BedDrive::Rain,
                vec![(0.1, -46.0), (2.0, -28.0), (10.0, -16.0), (50.0, -8.0)],
            ),
            BedLayer::new("drips", BedDrive::Drips, vec![(0.05, -50.0), (1.0, -30.0)]),
        ])
    }

    /// A bed with these layers and the default tuning (calm 0.3 m/s, attack 0.4 s, release 1.5 s,
    /// the `Ambience` bus).
    #[must_use]
    pub fn with_layers(layers: Vec<BedLayer>) -> Self {
        let state = vec![BedLayerState::default(); layers.len()];
        Self {
            layers,
            calm_m_per_s: 0.3,
            attack_secs: 0.4,
            release_secs: 1.5,
            bus: "Ambience".into(),
            volume_db: 0.0,
            state,
            inputs: AmbienceInputs::default(),
            wind_override: None,
        }
    }

    /// The inputs this bed hears from the scene's weather: [`AmbienceInputs::from_weather`]; without
    /// a [`SceneWind`], [`Self::wind_override`] (when set) as a steady wind.
    #[must_use]
    pub fn hear(&self, wind: Option<&SceneWind>, surface: Option<&SurfaceWeather>) -> AmbienceInputs {
        match (wind, self.wind_override) {
            (None, Some(held)) => {
                let speed = finite_non_negative(held[0].hypot(held[1]));
                AmbienceInputs {
                    wind_m_per_s: speed,
                    mean_wind_m_per_s: speed,
                    ..AmbienceInputs::from_weather(None, surface)
                }
            }
            _ => AmbienceInputs::from_weather(wind, surface),
        }
    }

    /// The index of the layer named `name`.
    #[must_use]
    pub fn layer_index(&self, name: &str) -> Option<usize> {
        self.layers.iter().position(|l| l.name == name)
    }

    /// Sets the sound of the layer named `name`; `false` when there is no such layer.
    pub fn set_sound(&mut self, name: &str, sound: Option<String>) -> bool {
        match self.layer_index(name) {
            Some(i) => {
                self.layers[i].sound = sound;
                true
            }
            None => false,
        }
    }

    /// The slewed level of the layer named `name`, dB.
    #[must_use]
    pub fn level_db(&self, name: &str) -> Option<f32> {
        let i = self.layer_index(name)?;
        Some(self.state.get(i).map_or(MUTE_DB, BedLayerState::level_db))
    }

    /// The combined level of the wind layers (wind, gust, rustle), dB: their amplitudes summed
    /// as uncorrelated noise (power sum).
    #[must_use]
    pub fn wind_level_db(&self) -> f32 {
        let power: f32 = self
            .layers
            .iter()
            .zip(&self.state)
            .filter(|(l, _)| l.drive.is_wind())
            .map(|(_, s)| s.gain * s.gain)
            .sum();
        if power <= db_to_gain(MUTE_DB).powi(2) {
            MUTE_DB
        } else {
            (10.0 * power.log10()).max(MUTE_DB)
        }
    }

    /// One step: sets every layer's target from `inputs` and slews its amplitude over `dt`
    /// seconds (attack when rising, release when falling). A `dt` of 0 only retargets.
    pub fn step(&mut self, inputs: &AmbienceInputs, dt: f32) {
        self.inputs = *inputs;
        self.state.resize(self.layers.len(), BedLayerState::default());
        let dt = finite_non_negative(dt);
        for (layer, state) in self.layers.iter().zip(self.state.iter_mut()) {
            state.drive_value = layer.drive.value(inputs, self.calm_m_per_s);
            state.target_db = layer.target_db(inputs, self.calm_m_per_s);
            let target = if state.target_db <= MUTE_DB {
                0.0
            } else {
                db_to_gain(state.target_db)
            };
            let tau = if target > state.gain {
                self.attack_secs
            } else {
                self.release_secs
            };
            state.gain = slew(state.gain, target, dt, tau);
        }
    }

    /// The dB a layer's voice is set to: its slewed level plus the bed's offset.
    #[must_use]
    pub fn voice_db(&self, index: usize) -> f32 {
        let level = self.state.get(index).map_or(MUTE_DB, BedLayerState::level_db);
        if level <= MUTE_DB {
            MUTE_DB
        } else {
            (level + finite_or_zero(self.volume_db)).clamp(MUTE_DB, 12.0)
        }
    }
}

/// The `SoundAssets` name prefix of the generated placeholder loops ([`test_tone_loopgens`]).
pub const TEST_TONE_PREFIX: &str = "ambience_test/";

/// Generated placeholder loops for the forest bed's layers, as `(layer, .loopgen.ron text)`:
/// shaped noise (brown air, low-passed pink wind and gust, high-passed white rustle, low-passed
/// white rain, a band-passed tick for drips). They are **test tones**, not recordings: they make
/// the levels audible before real assets exist. Register each with `SoundAssets::add_loopgen`
/// under [`TEST_TONE_PREFIX`] plus the layer name.
#[must_use]
pub fn test_tone_loopgens() -> Vec<(&'static str, String)> {
    let make = |name: &str, seed: u64, nodes: &str, output: &str| {
        format!(
            "(version: 1, name: \"{name}\", sample_rate: 48000, length_secs: 4.0, channels: 1, seed: {seed}, \
             crossfade_secs: 0.25, output_gain_db: -6.0, nodes: [{nodes}], output: \"{output}\")"
        )
    };
    vec![
        (
            "air",
            make(
                "ambience_test_air",
                1,
                "(id: \"n\", node: Noise(color: Brown, gain: 0.6)), \
                 (id: \"lp\", node: Filter(input: \"n\", mode: LowPass, cutoff_hz: 250.0, q: 0.707))",
                "lp",
            ),
        ),
        (
            "wind",
            make(
                "ambience_test_wind",
                2,
                "(id: \"n\", node: Noise(color: Pink, gain: 0.8)), \
                 (id: \"lp\", node: Filter(input: \"n\", mode: LowPass, cutoff_hz: 600.0, q: 0.707)), \
                 (id: \"swell\", node: Am(input: \"lp\", rate_hz: 0.25, depth: 0.3))",
                "swell",
            ),
        ),
        (
            "gust",
            make(
                "ambience_test_gust",
                3,
                "(id: \"n\", node: Noise(color: Pink, gain: 0.8)), \
                 (id: \"bp\", node: Filter(input: \"n\", mode: BandPass, cutoff_hz: 900.0, q: 0.8))",
                "bp",
            ),
        ),
        (
            "rustle",
            make(
                "ambience_test_rustle",
                4,
                "(id: \"n\", node: Noise(color: White, gain: 0.5)), \
                 (id: \"hp\", node: Filter(input: \"n\", mode: HighPass, cutoff_hz: 3000.0, q: 0.707)), \
                 (id: \"flutter\", node: Am(input: \"hp\", rate_hz: 6.0, depth: 0.5))",
                "flutter",
            ),
        ),
        (
            "rain",
            make(
                "ambience_test_rain",
                5,
                "(id: \"n\", node: Noise(color: White, gain: 0.6)), \
                 (id: \"lp\", node: Filter(input: \"n\", mode: LowPass, cutoff_hz: 6000.0, q: 0.707))",
                "lp",
            ),
        ),
        (
            "drips",
            make(
                "ambience_test_drips",
                6,
                "(id: \"n\", node: Noise(color: White, gain: 0.6)), \
                 (id: \"bp\", node: Filter(input: \"n\", mode: BandPass, cutoff_hz: 2500.0, q: 4.0)), \
                 (id: \"tick\", node: Am(input: \"bp\", rate_hz: 3.0, depth: 0.9))",
                "tick",
            ),
        ),
    ]
}

fn finite_or_zero(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// One-pole step of `current` toward `target` with time constant `tau` seconds. Lands on the
/// target within 0.1 %, and on silence below −80 dB (inaudible under any bed), so a silenced
/// layer reaches exactly 0.
fn slew(current: f32, target: f32, dt: f32, tau: f32) -> f32 {
    let current = if current.is_finite() { current } else { target };
    if !tau.is_finite() || tau <= 1e-4 {
        return target;
    }
    let k = 1.0 - (-dt / tau).exp();
    let next = current + (target - current) * k;
    if target <= 0.0 && next < 1e-4 {
        return 0.0;
    }
    if (next - target).abs() <= 1e-3 * target.max(1e-3) {
        target
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::weather::SceneWindMode;

    fn wind(now: f32, mean: f32) -> AmbienceInputs {
        AmbienceInputs {
            wind_m_per_s: now,
            mean_wind_m_per_s: mean,
            ..AmbienceInputs::default()
        }
    }

    /// Twenty seconds of frames: a −16 dB layer released over 1.5 s is below −80 dB (silence)
    /// after about 11 s.
    fn settle(bed: &mut AmbienceBed, inputs: &AmbienceInputs) {
        for _ in 0..1200 {
            bed.step(inputs, 1.0 / 60.0);
        }
    }

    #[test]
    fn calm_air_is_quiet_and_wind_layers_are_silent() {
        let mut bed = AmbienceBed::forest();
        settle(&mut bed, &AmbienceInputs::default());
        for name in ["wind", "gust", "rustle", "rain", "drips"] {
            assert_eq!(bed.level_db(name), Some(MUTE_DB), "{name}");
        }
        assert!((bed.level_db("air").expect("present") + 42.0).abs() < 0.01);
        assert_eq!(bed.wind_level_db(), MUTE_DB);
        // Just under the calm threshold: still silent.
        settle(&mut bed, &wind(0.29, 0.29));
        assert_eq!(bed.wind_level_db(), MUTE_DB);
    }

    #[test]
    fn wind_level_rises_monotonically_with_the_wind() {
        let mut last = MUTE_DB;
        for speed in [0.5, 1.0, 2.0, 4.0, 8.0, 12.0, 17.0, 25.0] {
            let mut bed = AmbienceBed::forest();
            settle(&mut bed, &wind(speed, speed));
            let level = bed.wind_level_db();
            assert!(level > last, "{speed} m/s: {level} after {last}");
            last = level;
        }
        assert!(last > -12.0, "a storm is loud: {last}");
    }

    #[test]
    fn a_gust_swells_the_gust_layer_and_dies_away() {
        let mut bed = AmbienceBed::forest();
        settle(&mut bed, &wind(5.0, 5.0));
        assert_eq!(bed.level_db("gust"), Some(MUTE_DB), "no gust at the mean");
        // A 4 m/s gust for half a second: the layer rises but does not jump.
        bed.step(&wind(9.0, 5.0), 1.0 / 60.0);
        let first = bed.level_db("gust").expect("present");
        for _ in 0..30 {
            bed.step(&wind(9.0, 5.0), 1.0 / 60.0);
        }
        let swelled = bed.level_db("gust").expect("present");
        assert!(first < swelled - 6.0, "attack is slewed: {first} then {swelled}");
        assert!(swelled > -30.0, "{swelled}");
        // The gust ends: the release takes longer than the attack.
        for _ in 0..30 {
            bed.step(&wind(5.0, 5.0), 1.0 / 60.0);
        }
        let falling = bed.level_db("gust").expect("present");
        assert!(falling > MUTE_DB && falling < swelled, "{falling}");
        settle(&mut bed, &wind(5.0, 5.0));
        assert_eq!(bed.level_db("gust"), Some(MUTE_DB));
    }

    #[test]
    fn rain_then_drips() {
        let mut bed = AmbienceBed::forest();
        let raining = AmbienceInputs {
            rain_mm_per_h: 8.0,
            wetness: 1.0,
            ..AmbienceInputs::default()
        };
        settle(&mut bed, &raining);
        assert!(bed.level_db("rain").expect("present") > -25.0);
        assert_eq!(
            bed.level_db("drips"),
            Some(MUTE_DB),
            "drips wait for the rain to stop"
        );
        let after = AmbienceInputs {
            rain_mm_per_h: 0.0,
            ..raining
        };
        settle(&mut bed, &after);
        assert_eq!(bed.level_db("rain"), Some(MUTE_DB));
        assert!(bed.level_db("drips").expect("present") > -35.0);
    }

    #[test]
    fn inputs_follow_the_scene_resources_and_ignore_bad_values() {
        assert_eq!(
            AmbienceInputs::from_weather(None, None),
            AmbienceInputs::default()
        );
        let gusty = SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [6.0, 8.0],
            mean_m_per_s: [3.0, 4.0],
        };
        let inputs = AmbienceInputs::from_weather(Some(&gusty), None);
        assert_eq!(inputs.wind_m_per_s, 10.0);
        assert_eq!(inputs.mean_wind_m_per_s, 5.0);
        assert_eq!(inputs.gust_m_per_s(), 5.0);
        let off = SceneWind {
            mode: SceneWindMode::Off,
            ..gusty
        };
        assert_eq!(AmbienceInputs::from_weather(Some(&off), None).wind_m_per_s, 0.0);
        let nan = SceneWind::manual([f32::NAN, 0.0]);
        assert_eq!(AmbienceInputs::from_weather(Some(&nan), None).wind_m_per_s, 0.0);
        let wet = SurfaceWeather {
            rain_mm_per_h: -3.0,
            wetness: 7.0,
            ..SurfaceWeather::DRY
        };
        let inputs = AmbienceInputs::from_weather(None, Some(&wet));
        assert_eq!(inputs.rain_mm_per_h, 0.0);
        assert_eq!(inputs.wetness, 1.0);
    }

    #[test]
    fn rustle_follows_the_sway_flutter_factor() {
        assert_eq!(foliage_flutter(0.0), 0.0);
        assert_eq!(foliage_flutter(5.0), 0.5);
        assert_eq!(foliage_flutter(f32::NAN), 0.0);
        let layer = &AmbienceBed::forest().layers[3];
        assert_eq!(layer.drive, BedDrive::Rustle);
        let a = layer.target_db(&wind(2.0, 2.0), 0.3);
        let b = layer.target_db(&wind(10.0, 2.0), 0.3);
        assert!(b > a, "the rustle follows the wind now, gusts included: {a} {b}");
    }

    #[test]
    fn the_test_tones_render_and_cover_the_forest_layers() {
        let mut sounds = crate::SoundAssets::default();
        let forest = AmbienceBed::forest();
        for (layer, text) in test_tone_loopgens() {
            assert!(forest.layer_index(layer).is_some(), "{layer}");
            let name = format!("{TEST_TONE_PREFIX}{layer}");
            sounds
                .add_loopgen(name.clone(), &text)
                .unwrap_or_else(|e| panic!("{layer}: {e}"));
            let asset = sounds.by_name(&name).expect("registered");
            assert!((asset.duration_secs() - 4.0).abs() < 0.01, "{layer}");
        }
    }

    #[test]
    fn a_wind_override_stands_in_for_a_missing_scene_wind() {
        let mut bed = AmbienceBed::forest();
        assert_eq!(bed.hear(None, None).wind_m_per_s, 0.0);
        bed.wind_override = Some([3.0, 4.0]);
        let heard = bed.hear(None, None);
        assert_eq!((heard.wind_m_per_s, heard.gust_m_per_s()), (5.0, 0.0));
        // A SceneWind wins, as it does in the renderer: still air stays still.
        let scene = SceneWind::manual([0.0, 0.0]);
        assert_eq!(bed.hear(Some(&scene), None).wind_m_per_s, 0.0);
        bed.wind_override = Some([f32::NAN, 1.0]);
        assert_eq!(bed.hear(None, None).wind_m_per_s, 0.0);
    }

    #[test]
    fn drive_names_round_trip() {
        for drive in BedDrive::ALL {
            assert_eq!(BedDrive::from_name(drive.name()), Some(drive));
        }
        assert_eq!(BedDrive::from_name("Wind"), None);
    }

    #[test]
    fn a_nan_curve_or_step_never_reaches_the_mixer() {
        let mut bed =
            AmbienceBed::with_layers(vec![BedLayer::new("x", BedDrive::Wind, vec![(0.0, f32::NAN)])]);
        bed.step(&wind(5.0, 5.0), f32::NAN);
        assert_eq!(bed.voice_db(0), MUTE_DB);
        bed.volume_db = f32::INFINITY;
        assert!(bed.voice_db(0).is_finite());
    }
}
