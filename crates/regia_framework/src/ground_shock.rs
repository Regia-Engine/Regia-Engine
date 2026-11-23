// SPDX-License-Identifier: MPL-2.0
//! Ground-motion and airblast shake of a distant explosion, as a camera on a tripod feels it.
//!
//! [`GroundShock`] turns a yield and a distance into two arrivals: the seismic wave (P wave,
//! several km/s) and, later, the airblast (sound speed). Each becomes a [`MountShock`]: a damped
//! oscillation of the camera's pitch, yaw and roll (and a tiny translation) that
//! [`CameraDirector`](crate::camera::CameraDirector) plays back as a [`ShakePose`].
//!
//! The chain, all original code over public relations (`docs/provenance.md`):
//!
//! 1. Peak particle velocity of the ground from the scaled-distance law of blast vibration,
//!    `PPV = K · SD^-β` with `SD = R / sqrt(W)` (USBM RI 8507, Siskind et al. 1980; the
//!    `K = 160 in/s`, `β = 1.6` fit with `SD` in ft/lb^½). `W` is the charge weight that couples
//!    into the ground: a surface or impact burst couples a few percent of an equal buried charge
//!    ([`GroundShock::coupling`]).
//! 2. Ground displacement at the mount's resonance `u = PPV / (2π f)`.
//! 3. The tripod flexes: the head sways `Q · u` (resonant gain `Q`), which tilts the camera by
//!    `Q · u / L` for an effective lever `L` of about a metre. This is why a motion of tenths of a
//!    millimetre shows on a long lens: the angle, not the translation, is what the picture sees.
//! 4. Airblast peak overpressure from the Kinney-Graham fit (Kinney and Graham, *Explosive Shocks
//!    in Air*, 1985) at the Hopkinson-Cranz scaled distance, with the surface burst's reflection
//!    factor on the charge. The camera body and lens catch it as a force `p · A`, and the head
//!    deflects `F / k` (with a dynamic factor), tilting the camera by that over `L`.
//!
//! The strength is a named, physically motivated parameter, not a fudge factor: the mount's
//! resonant gain [`GroundShock::mount_q`] (default 0.8, a damped fluid head under a heavy lens;
//! the earlier 10 is a lightly damped tripod at resonance and is [`GroundShock::legacy`]).
//! [`ShakeVisibility`] then bounds the result by what a viewer can see: a floor below which a
//! tilt is not worth scheduling and a ceiling that keeps a near blast from throwing the picture.
//! The seismic wave rings at the mount's modes ([`ShakeShape::Oscillation`]); the airblast is
//! one short push that decays ([`ShakeShape::Impulse`]), because its positive phase lasts a
//! fraction of a second and the mount does not ring on it for seconds.
//!
//! Every number is a field of [`GroundShock`] so a host can tune it. The oscillation frequencies
//! come from a seed (a 4 to 9 Hz mode, the other two axes within 15 percent of it), so the same
//! blast and the same seed play back identically.

use regia_core::math::{EulerRot, Quat, Rng, Vec3};

/// Speed of sound used for the airblast (m/s): the crash effects' blast-front speed, one constant
/// for the shake, the visible front and the vegetation gust (ADR-1497). Defined in
/// `regia_core::SPEED_OF_SOUND_M_S` so a server build (without the `client` feature and its
/// `regia_vfx`) sees it too; `regia_vfx::crash::SHOCK_FRONT_SPEED` re-exports the same constant.
pub const AIR_SPEED_M_S: f32 = regia_core::SPEED_OF_SOUND_M_S;
/// Speed of the P wave through crust near the surface (m/s); a mid value of 5 to 6 km/s.
pub const P_WAVE_SPEED_M_S: f32 = 5500.0;

/// Kilograms in one kiloton of TNT.
const KG_PER_KT: f64 = 1.0e6;
/// Ambient pressure (Pa).
const AMBIENT_PA: f64 = 101_325.0;
/// Feet per metre over the square root of pounds per kilogram: `SD` in ft/lb^½ per m/kg^½.
const FT_LB_HALF_PER_M_KG_HALF: f64 = 3.280_84 / 1.484_80;
/// The USBM fit constant, inches per second at `SD = 1 ft/lb^½`.
const USBM_K_IN_S: f64 = 160.0;
/// The USBM fit exponent.
const USBM_BETA: f64 = 1.6;
/// Metres per inch.
const M_PER_IN: f64 = 0.0254;
/// Smallest scaled distance the fit is evaluated at (m/kg^½): closer is held, not extrapolated.
const MIN_SCALED_DISTANCE: f64 = 1.5;
/// Smallest Hopkinson-Cranz distance the airblast fit is evaluated at (m/kg^⅓).
const MIN_Z: f64 = 0.2;
/// Largest rotation any shock produces (radians, about 20 degrees): the mount saturates.
const MAX_ANGLE_RAD: f64 = 0.35;
/// A shock below this peak angle is not worth scheduling (radians, about 0.0002 px at 4K).
pub const MIN_ANGLE_RAD: f32 = 1.0e-7;

/// The explosion and the camera mount: the law of [`MountShock`]s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundShock {
    /// Yield in kilotons of TNT (the pod impact is 0.2 to 0.5).
    pub yield_kt: f32,
    /// Share of the charge that couples into the ground as seismic energy (a surface burst:
    /// about 0.05; `1` is a fully buried charge).
    pub coupling: f32,
    /// Seismic P-wave speed (m/s).
    pub p_speed_m_s: f32,
    /// Airblast speed (m/s).
    pub air_speed_m_s: f32,
    /// Effective lever of the tripod, from the ground to the camera's optical centre (metres).
    pub lever_m: f32,
    /// Resonant gain of the mount (the quality factor `Q`).
    pub mount_q: f32,
    /// Frontal area the airblast loads: body, lens and head (square metres).
    pub area_m2: f32,
    /// Lateral stiffness of the tripod at the head (newtons per metre).
    pub stiffness_n_m: f32,
    /// Dynamic load factor of the blast's positive phase against the mount's period.
    pub dynamic_factor: f32,
    /// Factor on the charge for the airblast of a surface burst (hemispherical reflection).
    pub surface_factor: f32,
    /// Decay time constant of the seismic oscillation (seconds).
    pub seismic_decay_s: f32,
    /// Decay time constant of the airblast motion (seconds).
    pub air_decay_s: f32,
    /// How the seismic wave moves the mount.
    pub seismic_shape: ShakeShape,
    /// How the airblast moves the mount.
    pub air_shape: ShakeShape,
    /// The perceptual floor and ceiling the camera angles are held between.
    pub visibility: ShakeVisibility,
}

/// How one wave moves a camera mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShakeShape {
    /// A damped oscillation at the mount's modes (rise, then exponential decay under a sine).
    Oscillation,
    /// One push in a fixed direction per axis that rises quickly and decays, with no sign change.
    Impulse,
}

/// The perceptual bounds of a shake, as camera rotations (radians). One pixel at 4K (2160 rows)
/// on a 20 degree vertical field of view is `1.6e-4` rad, so the defaults are 0.25 px and 215 px
/// there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShakeVisibility {
    /// A wave whose peak tilt is under this is not scheduled: nobody would see it.
    pub floor_rad: f32,
    /// No wave tilts the camera more than this (a blast next door must not roll the picture).
    /// Held to at most 0.35 rad (about 20 degrees) whatever the setting.
    pub ceiling_rad: f32,
}

impl ShakeVisibility {
    /// The default bounds: a floor of 0.25 px at 4K on a 20 degree lens, a ceiling of 2 degrees.
    pub const DEFAULT: Self = Self {
        floor_rad: 4.0e-5,
        ceiling_rad: 0.0349,
    };

    /// No bounds beyond the mount's own 20 degree saturation (the behavior before ADR-1473).
    pub const UNBOUNDED: Self = Self {
        floor_rad: MIN_ANGLE_RAD,
        ceiling_rad: MAX_ANGLE_RAD as f32,
    };

    fn ceiling(&self) -> f64 {
        if self.ceiling_rad.is_finite() && self.ceiling_rad > 0.0 {
            f64::from(self.ceiling_rad).min(MAX_ANGLE_RAD)
        } else {
            MAX_ANGLE_RAD
        }
    }

    fn floor(&self) -> f32 {
        if self.floor_rad.is_finite() {
            self.floor_rad.max(MIN_ANGLE_RAD)
        } else {
            MIN_ANGLE_RAD
        }
    }
}

impl Default for ShakeVisibility {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Default for GroundShock {
    fn default() -> Self {
        Self {
            yield_kt: 0.35,
            coupling: 0.05,
            p_speed_m_s: P_WAVE_SPEED_M_S,
            air_speed_m_s: AIR_SPEED_M_S,
            lever_m: 1.0,
            mount_q: 0.8,
            area_m2: 0.01,
            stiffness_n_m: 57_600.0,
            dynamic_factor: 1.5,
            surface_factor: 1.8,
            seismic_decay_s: 1.4,
            air_decay_s: 0.4,
            seismic_shape: ShakeShape::Oscillation,
            air_shape: ShakeShape::Impulse,
            visibility: ShakeVisibility::DEFAULT,
        }
    }
}

impl GroundShock {
    /// The strengths of ADR-1457: a lightly damped mount (`Q` 10), the airblast ringing for
    /// seconds at the mount's modes, and no perceptual bounds. At 3.3 km, 0.35 kt this tilts the
    /// camera 0.093 degrees for the seismic wave (10 px at 4K on a 20 degree lens).
    pub fn legacy() -> Self {
        Self {
            mount_q: 10.0,
            stiffness_n_m: 12_000.0,
            air_decay_s: 0.9,
            air_shape: ShakeShape::Oscillation,
            visibility: ShakeVisibility::UNBOUNDED,
            ..Self::default()
        }
    }
}

/// Which wave a [`MountShock`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveKind {
    /// The ground wave, first to arrive.
    Seismic,
    /// The pressure front, at the speed of sound.
    Airblast,
}

/// What a blast does to a camera at one distance: both waves, as the agent reports them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundPrediction {
    /// Peak particle velocity of the ground (mm/s).
    pub ppv_mm_s: f32,
    /// Peak ground displacement at the mount's mid frequency (mm).
    pub ground_displacement_mm: f32,
    /// Peak airblast overpressure (Pa).
    pub overpressure_pa: f32,
    /// Seismic arrival (seconds after the blast).
    pub seismic_delay_s: f32,
    /// Airblast arrival (seconds after the blast).
    pub air_delay_s: f32,
    /// Peak camera rotation of the seismic wave (radians).
    pub seismic_angle_rad: f32,
    /// Peak camera rotation of the airblast (radians).
    pub air_angle_rad: f32,
}

/// Mid frequency of the mount's modes, for the displacement from a velocity (hertz).
const MID_FREQUENCY_HZ: f64 = 6.5;

impl GroundShock {
    fn finite_or(value: f32, fallback: f32) -> f64 {
        if value.is_finite() && value > 0.0 {
            f64::from(value)
        } else {
            f64::from(fallback)
        }
    }

    /// Peak particle velocity of the ground at `distance_m`, metres per second. `0` for a NaN
    /// or infinite distance; inside the fit's useful range the scaled distance is held.
    pub fn ppv_m_s(&self, distance_m: f32) -> f32 {
        if !distance_m.is_finite() {
            return 0.0;
        }
        let d = f64::from(distance_m.max(0.0));
        let coupled_kg = f64::from(self.yield_kt.max(0.0)) * KG_PER_KT * Self::finite_or(self.coupling, 0.05);
        if coupled_kg <= 0.0 {
            return 0.0;
        }
        let sd = (d / coupled_kg.sqrt()).max(MIN_SCALED_DISTANCE);
        let in_s = USBM_K_IN_S * (sd * FT_LB_HALF_PER_M_KG_HALF).powf(-USBM_BETA);
        (in_s * M_PER_IN) as f32
    }

    /// Peak displacement of the ground at the mount's mid frequency, metres.
    pub fn ground_displacement_m(&self, distance_m: f32) -> f32 {
        (f64::from(self.ppv_m_s(distance_m)) / (std::f64::consts::TAU * MID_FREQUENCY_HZ)) as f32
    }

    /// Peak camera rotation from the seismic wave at `distance_m`, radians (`Q · u / L`, held to
    /// the [`ShakeVisibility`] ceiling).
    pub fn seismic_angle_rad(&self, distance_m: f32) -> f32 {
        let sway = f64::from(self.ground_displacement_m(distance_m)) * Self::finite_or(self.mount_q, 0.8);
        (sway / Self::finite_or(self.lever_m, 1.0)).min(self.visibility.ceiling()) as f32
    }

    /// Peak airblast overpressure at `distance_m`, pascals (Kinney-Graham fit, surface burst).
    pub fn overpressure_pa(&self, distance_m: f32) -> f32 {
        if !distance_m.is_finite() {
            return 0.0;
        }
        let w_kg = f64::from(self.yield_kt.max(0.0)) * KG_PER_KT * Self::finite_or(self.surface_factor, 1.8);
        if w_kg <= 0.0 {
            return 0.0;
        }
        let z = (f64::from(distance_m.max(0.0)) / w_kg.cbrt()).max(MIN_Z);
        let num = 808.0 * (1.0 + (z / 4.5).powi(2));
        let den =
            ((1.0 + (z / 0.048).powi(2)) * (1.0 + (z / 0.32).powi(2)) * (1.0 + (z / 1.35).powi(2))).sqrt();
        (AMBIENT_PA * num / den) as f32
    }

    /// Peak camera rotation from the airblast at `distance_m`, radians (`dynamic · p · A / k / L`,
    /// held to the [`ShakeVisibility`] ceiling).
    pub fn air_angle_rad(&self, distance_m: f32) -> f32 {
        let force = f64::from(self.overpressure_pa(distance_m)) * Self::finite_or(self.area_m2, 0.01);
        let deflection =
            Self::finite_or(self.dynamic_factor, 1.5) * force / Self::finite_or(self.stiffness_n_m, 57_600.0);
        (deflection / Self::finite_or(self.lever_m, 1.0)).min(self.visibility.ceiling()) as f32
    }

    /// Seconds from the blast to the seismic wave reaching `distance_m`.
    pub fn seismic_delay_s(&self, distance_m: f32) -> f32 {
        self.seismic_delay_f64(distance_m) as f32
    }

    /// [`seismic_delay_s`](Self::seismic_delay_s) in double precision, for exact step counts.
    pub fn seismic_delay_f64(&self, distance_m: f32) -> f64 {
        delay(distance_m, self.p_speed_m_s, P_WAVE_SPEED_M_S)
    }

    /// Seconds from the blast to the airblast reaching `distance_m`.
    pub fn air_delay_s(&self, distance_m: f32) -> f32 {
        self.air_delay_f64(distance_m) as f32
    }

    /// [`air_delay_s`](Self::air_delay_s) in double precision, for exact step counts.
    pub fn air_delay_f64(&self, distance_m: f32) -> f64 {
        delay(distance_m, self.air_speed_m_s, AIR_SPEED_M_S)
    }

    /// Everything the blast does at `distance_m`.
    pub fn predict(&self, distance_m: f32) -> GroundPrediction {
        GroundPrediction {
            ppv_mm_s: self.ppv_m_s(distance_m) * 1000.0,
            ground_displacement_mm: self.ground_displacement_m(distance_m) * 1000.0,
            overpressure_pa: self.overpressure_pa(distance_m),
            seismic_delay_s: self.seismic_delay_s(distance_m),
            air_delay_s: self.air_delay_s(distance_m),
            seismic_angle_rad: self.seismic_angle_rad(distance_m),
            air_angle_rad: self.air_angle_rad(distance_m),
        }
    }

    /// The oscillation of one wave at `distance_m`, played from the wave's arrival, or `None` when
    /// it is negligible (a NaN distance, or a peak angle under the [`ShakeVisibility`] floor).
    pub fn mount_shock(&self, kind: WaveKind, distance_m: f32, seed: u64) -> Option<MountShock> {
        if distance_m.is_nan() {
            return None;
        }
        let (angle, decay, translation, shape) = match kind {
            WaveKind::Seismic => (
                self.seismic_angle_rad(distance_m),
                self.seismic_decay_s,
                self.ground_displacement_m(distance_m),
                self.seismic_shape,
            ),
            WaveKind::Airblast => (
                self.air_angle_rad(distance_m),
                self.air_decay_s,
                0.0,
                self.air_shape,
            ),
        };
        if angle.is_nan() || angle < self.visibility.floor() {
            return None;
        }
        let decay = Self::finite_or(decay, 1.0) as f32;
        Some(match shape {
            ShakeShape::Oscillation => MountShock::new(angle, translation, decay, seed),
            ShakeShape::Impulse => MountShock::impulse(angle, translation, decay, seed),
        })
    }
}

fn delay(distance_m: f32, speed: f32, fallback: f32) -> f64 {
    if distance_m.is_nan() || distance_m <= 0.0 {
        return 0.0;
    }
    let speed = if speed.is_finite() && speed > 0.0 {
        speed
    } else {
        fallback
    };
    f64::from(distance_m) / f64::from(speed)
}

/// The camera's pose offset a shake adds this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShakePose {
    /// Translation (metres).
    pub offset: Vec3,
    /// Rotation about the camera's right axis (radians).
    pub pitch: f32,
    /// Rotation about the camera's up axis (radians).
    pub yaw: f32,
    /// Rotation about the camera's view axis (radians).
    pub roll: f32,
}

impl ShakePose {
    /// Whether the pose is exactly the unshaken one.
    pub fn is_idle(&self) -> bool {
        *self == Self::default()
    }

    /// The rotation as a quaternion to multiply onto the camera's rotation (yaw, then pitch, then
    /// roll).
    pub fn rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, self.roll)
    }
}

/// A motion of a camera mount: three angles and a small translation under one envelope (a quick
/// rise, then an exponential decay), each a sine at its own seeded frequency ([`ShakeShape::Oscillation`])
/// or a fixed push ([`ShakeShape::Impulse`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MountShock {
    /// Peak pitch (radians); yaw is 0.8 and roll 0.3 of it.
    pub angle_rad: f32,
    /// Peak translation (metres) on each axis.
    pub translation_m: f32,
    /// Decay time constant (seconds).
    pub decay_s: f32,
    /// Rise time constant (seconds).
    pub rise_s: f32,
    /// Seed of the frequencies and phases.
    pub seed: u64,
    /// Whether the motion rings or is one decaying push.
    pub shape: ShakeShape,
    freq_hz: [f32; 3],
    phase: [f32; 6],
    peak_scale: f32,
}

/// Stream of the generator that picks a shock's frequencies and phases.
const MOUNT_STREAM: u64 = 0x6d6f_756e_7473_6b31;
/// Share of the pitch amplitude each axis gets: pitch, yaw, roll.
const AXIS_WEIGHT: [f32; 3] = [1.0, 0.8, 0.3];

impl MountShock {
    /// A shock of peak pitch `angle_rad` with frequencies and phases drawn from `seed`.
    pub fn new(angle_rad: f32, translation_m: f32, decay_s: f32, seed: u64) -> Self {
        Self::build(ShakeShape::Oscillation, angle_rad, translation_m, decay_s, seed)
    }

    /// A single push: each axis moves in one direction (drawn from `seed`), rising in about 30 ms
    /// and decaying with `decay_s`, and never changes sign. Peak pitch is `angle_rad`.
    pub fn impulse(angle_rad: f32, translation_m: f32, decay_s: f32, seed: u64) -> Self {
        Self::build(ShakeShape::Impulse, angle_rad, translation_m, decay_s, seed)
    }

    fn build(shape: ShakeShape, angle_rad: f32, translation_m: f32, decay_s: f32, seed: u64) -> Self {
        let mut rng = Rng::with_stream(seed, MOUNT_STREAM);
        let base = 4.0 + 5.0 * rng.f32();
        let freq_hz = [
            base,
            base * (0.9 + 0.25 * rng.f32()),
            base * (0.87 + 0.26 * rng.f32()),
        ];
        let mut phase = [0.0; 6];
        for p in &mut phase {
            *p = rng.f32() * std::f32::consts::TAU;
        }
        let rise_s = match shape {
            ShakeShape::Oscillation => 0.12,
            ShakeShape::Impulse => 0.03,
        };
        let decay_s = if decay_s.is_finite() && decay_s > 0.0 {
            decay_s
        } else {
            1.0
        };
        // Normalise the envelope so its largest value is 1.
        let a = 1.0 / decay_s;
        let b = a + 1.0 / rise_s;
        let t_peak = (b / a).ln() / (b - a);
        let peak = (-a * t_peak).exp() - (-b * t_peak).exp();
        Self {
            angle_rad,
            translation_m,
            decay_s,
            rise_s,
            seed,
            shape,
            freq_hz,
            phase,
            peak_scale: 1.0 / peak,
        }
    }

    /// How long the shock lasts (seconds): seven decay constants, when the envelope is below a
    /// thousandth.
    pub fn duration_s(&self) -> f32 {
        7.0 * self.decay_s
    }

    /// The three mode frequencies (pitch, yaw, roll), hertz.
    pub fn frequencies_hz(&self) -> [f32; 3] {
        self.freq_hz
    }

    pub(crate) fn envelope(&self, t: f32) -> f32 {
        let a = 1.0 / self.decay_s;
        let b = a + 1.0 / self.rise_s;
        ((-a * t).exp() - (-b * t).exp()) * self.peak_scale
    }

    /// The pose offset `t` seconds after the wave arrived; the idle pose before it and after
    /// [`duration_s`](Self::duration_s).
    pub fn pose(&self, t: f32) -> ShakePose {
        if t.is_nan() || t < 0.0 || t >= self.duration_s() {
            return ShakePose::default();
        }
        let env = self.envelope(t);
        let wave = |axis: usize, phase: usize| match self.shape {
            ShakeShape::Oscillation => {
                (std::f32::consts::TAU * self.freq_hz[axis] * t + self.phase[phase]).sin()
            }
            // One direction per axis, fixed for the whole push.
            ShakeShape::Impulse => {
                if self.phase[phase] < std::f32::consts::PI {
                    1.0
                } else {
                    -1.0
                }
            }
        };
        let a = self.angle_rad * env;
        let u = self.translation_m * env;
        ShakePose {
            offset: Vec3::new(u * wave(1, 3), u * wave(0, 4), u * wave(2, 5)),
            pitch: a * AXIS_WEIGHT[0] * wave(0, 0),
            yaw: a * AXIS_WEIGHT[1] * wave(1, 1),
            roll: a * AXIS_WEIGHT[2] * wave(2, 2),
        }
    }
}

/// Image-plane shift in pixels of a rotation of `angle_rad` for a camera with vertical field of
/// view `vfov_rad` rendering `height_px` rows (the pixel offset at the image centre).
pub fn angle_to_pixels(angle_rad: f32, vfov_rad: f32, height_px: f32) -> f32 {
    let half = (vfov_rad * 0.5).tan();
    if half <= 0.0 || !half.is_finite() {
        return 0.0;
    }
    angle_rad.tan() / half * height_px * 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    const VFOV: f32 = 60.0 * std::f32::consts::PI / 180.0;

    /// ADR-1497: the shake's airblast, the visible blast front and the vegetation gust share one
    /// speed. The server build has no `regia_vfx`, so the value lives in `regia_core` and this
    /// pins the crash effects' constant to it wherever both are compiled (ADR-7386, ADR-7490).
    #[cfg(feature = "client")]
    #[test]
    fn the_airblast_speed_is_the_crash_effects_front_speed() {
        assert_eq!(
            AIR_SPEED_M_S.to_bits(),
            regia_vfx::crash::SHOCK_FRONT_SPEED.to_bits()
        );
    }

    fn peak(shock: &MountShock) -> (f32, f32) {
        let mut pitch = 0.0f32;
        let mut offset = 0.0f32;
        let mut t = 0.0;
        while t < shock.duration_s() {
            let p = shock.pose(t);
            pitch = pitch.max(p.pitch.abs().max(p.yaw.abs()).max(p.roll.abs()));
            offset = offset.max(p.offset.length());
            t += 1.0 / 240.0;
        }
        (pitch, offset)
    }

    #[test]
    fn the_scaling_law_holds() {
        let g = GroundShock::default();
        // Siskind's fit with a surface coupling of 5 percent: tenths of m/s inside 300 m,
        // millimetres per second at 3.3 km, falling as distance^-1.6.
        let ppv_300 = g.ppv_m_s(300.0);
        let ppv_3300 = g.ppv_m_s(3300.0);
        assert!((0.1..0.6).contains(&ppv_300), "{ppv_300}");
        assert!((0.003..0.012).contains(&ppv_3300), "{ppv_3300}");
        let ratio = g.ppv_m_s(2000.0) / g.ppv_m_s(4000.0);
        assert!((ratio - 2.0f32.powf(1.6)).abs() < 0.01, "ratio {ratio}");
        // Cube-root-in-yield style scaling: PPV at equal scaled distance is equal.
        let big = GroundShock {
            yield_kt: g.yield_kt * 8.0,
            ..g
        };
        let a = g.ppv_m_s(1000.0);
        let b = big.ppv_m_s(1000.0 * 8.0f32.sqrt());
        assert!((a - b).abs() / a < 1.0e-4, "{a} vs {b}");
    }

    #[test]
    fn amplitude_never_grows_with_distance_or_shrinks_with_yield() {
        let g = GroundShock::default();
        let mut last = (f32::MAX, f32::MAX, f32::MAX);
        for d in [0.0, 50.0, 300.0, 1000.0, 3300.0, 10_000.0, 100_000.0] {
            let now = (g.seismic_angle_rad(d), g.air_angle_rad(d), g.overpressure_pa(d));
            assert!(
                now.0 <= last.0 && now.1 <= last.1 && now.2 <= last.2,
                "{d}: {now:?} vs {last:?}"
            );
            last = now;
        }
        // The ceiling would flatten a near blast; compare the yields without it.
        let g = GroundShock {
            visibility: ShakeVisibility::UNBOUNDED,
            ..g
        };
        let small = GroundShock { yield_kt: 0.2, ..g };
        let large = GroundShock { yield_kt: 0.5, ..g };
        for d in [300.0, 3300.0] {
            assert!(small.seismic_angle_rad(d) < large.seismic_angle_rad(d), "{d}");
            assert!(small.air_angle_rad(d) < large.air_angle_rad(d), "{d}");
        }
    }

    #[test]
    fn the_overpressure_matches_a_surface_burst() {
        let g = GroundShock::default();
        // 0.35 kt surface burst: a few kPa at 3.3 km, tens of kPa at 300 m.
        let far = g.overpressure_pa(3300.0);
        let near = g.overpressure_pa(300.0);
        assert!((1500.0..3500.0).contains(&far), "{far}");
        assert!((30_000.0..90_000.0).contains(&near), "{near}");
    }

    #[test]
    fn the_seismic_wave_arrives_before_the_airblast() {
        let g = GroundShock::default();
        let d = 3300.0;
        assert!(
            (g.seismic_delay_s(d) - 0.6).abs() < 0.01,
            "{}",
            g.seismic_delay_s(d)
        );
        assert!((g.air_delay_s(d) - 9.62).abs() < 0.01);
        assert!(g.seismic_delay_s(d) < g.air_delay_s(d));
        assert_eq!(g.seismic_delay_s(f32::NAN), 0.0);
        let bad = GroundShock {
            p_speed_m_s: 0.0,
            ..g
        };
        assert!(
            (bad.seismic_delay_s(5500.0) - 1.0).abs() < 1e-6,
            "falls back to 5.5 km/s"
        );
    }

    #[test]
    fn legacy_at_3_3_km_the_shake_is_a_few_pixels_at_4k_and_at_300_m_it_is_hard() {
        // The strengths of ADR-1457, kept reachable as `GroundShock::legacy()`.
        let g = GroundShock::legacy();
        let seismic_px = angle_to_pixels(g.seismic_angle_rad(3300.0), VFOV, 2160.0);
        let air_px = angle_to_pixels(g.air_angle_rad(3300.0), VFOV, 2160.0);
        assert!((2.0..8.0).contains(&seismic_px), "seismic {seismic_px} px");
        assert!((2.0..12.0).contains(&air_px), "airblast {air_px} px");
        let near_px = angle_to_pixels(g.seismic_angle_rad(300.0), VFOV, 2160.0);
        assert!(
            near_px > 60.0 && near_px > 10.0 * seismic_px,
            "300 m: {near_px} px"
        );
        // Translation alone stays sub-pixel at 3.3 km (the finding the angular term answers).
        assert!(g.ground_displacement_m(3300.0) < 0.001);
    }

    /// Image-plane shift at 4K on the showcase's shot-4 lens (20 degrees vertical).
    fn px_20(angle: f32) -> f32 {
        angle_to_pixels(angle, 20.0f32.to_radians(), 2160.0)
    }

    #[test]
    fn legacy_reproduces_the_look_round_three_numbers() {
        // 0.163 mm of ground, 0.093 degrees, 10 px (the derivation in the lane report).
        let g = GroundShock::legacy();
        let mm = g.ground_displacement_m(3300.0) * 1000.0;
        assert!((mm - 0.163).abs() < 0.005, "{mm} mm");
        let deg = g.seismic_angle_rad(3300.0).to_degrees();
        assert!((deg - 0.0934).abs() < 0.002, "{deg} deg");
        let px = px_20(g.seismic_angle_rad(3300.0));
        assert!((9.0..11.0).contains(&px), "{px} px");
        let air = g.mount_shock(WaveKind::Airblast, 3300.0, 1).expect("air");
        assert_eq!(air.shape, ShakeShape::Oscillation);
        assert!((air.duration_s() - 6.3).abs() < 1.0e-4);
    }

    #[test]
    fn by_default_the_seismic_wave_is_near_invisible_at_3_3_km() {
        let g = GroundShock::default();
        let px = px_20(g.seismic_angle_rad(3300.0));
        assert!((0.3..=1.0).contains(&px), "seismic {px} px at 4K, 20 degree lens");
        // Still scheduled (above the floor), and a closer camera feels more of it.
        assert!(g.mount_shock(WaveKind::Seismic, 3300.0, 1).is_some());
        assert!(g.seismic_angle_rad(1000.0) > 3.0 * g.seismic_angle_rad(3300.0));
        // The old strength is the same law with Q 10.
        let ratio = GroundShock::legacy().seismic_angle_rad(3300.0) / g.seismic_angle_rad(3300.0);
        assert!((ratio - 10.0 / 0.8).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn by_default_the_airblast_is_one_short_decaying_jolt() {
        let g = GroundShock::default();
        let shock = g.mount_shock(WaveKind::Airblast, 3300.0, 5).expect("air");
        assert_eq!(shock.shape, ShakeShape::Impulse);
        assert!(shock.duration_s() <= 3.0, "{} s", shock.duration_s());
        // The decay constant is 0.3 to 0.5 s: after it the push is at 1/e of its peak.
        assert!((0.3..=0.5).contains(&shock.decay_s), "{} s", shock.decay_s);
        // Every axis keeps one sign for the whole motion and its magnitude peaks once.
        let steps = (shock.duration_s() * 240.0) as usize;
        for axis in 0..3 {
            let samples: Vec<f32> = (0..steps)
                .map(|i| {
                    let p = shock.pose(i as f32 / 240.0);
                    [p.pitch, p.yaw, p.roll][axis]
                })
                .collect();
            let sign = samples
                .iter()
                .copied()
                .find(|v| *v != 0.0)
                .expect("moves")
                .signum();
            assert!(
                samples.iter().all(|v| *v == 0.0 || v.signum() == sign),
                "axis {axis} changes sign"
            );
            let mags: Vec<f32> = samples.iter().map(|v| v.abs()).collect();
            let top = mags
                .iter()
                .position(|m| *m == mags.iter().copied().fold(0.0, f32::max))
                .expect("peak");
            assert!(
                mags[..top].windows(2).all(|w| w[0] <= w[1] + 1.0e-9),
                "rises to one peak"
            );
            assert!(
                mags[top..].windows(2).all(|w| w[0] + 1.0e-9 >= w[1]),
                "then only decays"
            );
        }
        // Pitch peaks at the tilt, and the push is nearly gone after 1.5 s.
        let mut pitch = 0.0f32;
        for i in 0..steps {
            pitch = pitch.max(shock.pose(i as f32 / 240.0).pitch.abs());
        }
        assert!(
            (pitch - shock.angle_rad).abs() < 0.03 * shock.angle_rad,
            "{pitch}"
        );
        assert!(shock.pose(1.5).pitch.abs() < 0.05 * shock.angle_rad);
        // One jolt of 3 to 4 px at 4K on the 20 degree lens.
        let px = px_20(pitch);
        assert!((3.0..=4.0).contains(&px), "{px} px");
        assert!(shock.pose(shock.duration_s()).is_idle());
        // The 2.2 kPa airblast, met by a stiff fluid head: 0.033 degrees. The legacy mount
        // (12 kN/m) tilts 0.158 degrees for the same pressure.
        assert!((g.overpressure_pa(3300.0) - 2200.0).abs() < 500.0);
        assert!((0.028..0.04).contains(&shock.angle_rad.to_degrees()));
        let old = GroundShock::legacy().air_angle_rad(3300.0).to_degrees();
        assert!((0.14..0.18).contains(&old), "{old}");
    }

    #[test]
    fn the_visibility_bounds_hold_a_near_blast_and_drop_an_unseen_one() {
        let g = GroundShock::default();
        // 100 m: far above the ceiling, held to it (2 degrees).
        assert_eq!(g.air_angle_rad(100.0), g.visibility.ceiling_rad);
        assert!(g.seismic_angle_rad(50.0) <= g.visibility.ceiling_rad);
        // A floor above the 3.3 km seismic tilt (1.3e-4) and under the airblast (5.7e-4) drops that wave but not the airblast.
        let picky = GroundShock {
            visibility: ShakeVisibility {
                floor_rad: 3.0e-4,
                ..g.visibility
            },
            ..g
        };
        assert!(picky.mount_shock(WaveKind::Seismic, 3300.0, 1).is_none());
        assert!(picky.mount_shock(WaveKind::Airblast, 3300.0, 1).is_some());
        // Bad bounds fall back to the saturation of the mount.
        let bad = GroundShock {
            visibility: ShakeVisibility {
                floor_rad: f32::NAN,
                ceiling_rad: f32::NAN,
            },
            ..g
        };
        assert_eq!(bad.air_angle_rad(0.0), 0.35);
        assert!(bad.mount_shock(WaveKind::Seismic, 3300.0, 1).is_some());
        // The legacy profile has no ceiling below the saturation.
        assert!(GroundShock::legacy().air_angle_rad(100.0) > g.air_angle_rad(100.0));
    }

    #[test]
    fn the_oscillation_is_damped_within_the_band_and_deterministic() {
        let shock = GroundShock::default()
            .mount_shock(WaveKind::Seismic, 3300.0, 7)
            .expect("shock");
        for f in shock.frequencies_hz() {
            assert!((2.0..=12.0).contains(&f), "{f} Hz");
        }
        let (pitch_peak, offset_peak) = peak(&shock);
        assert!(
            pitch_peak > 0.5 * shock.angle_rad && pitch_peak <= shock.angle_rad * 1.001,
            "{pitch_peak}"
        );
        assert!(offset_peak <= shock.translation_m * 1.74);
        // Decays: the last quarter is far under the peak, and it is idle after the duration.
        let late = shock.pose(shock.duration_s() * 0.75);
        assert!(late.pitch.abs() < 0.05 * pitch_peak, "late {}", late.pitch);
        assert!(shock.pose(shock.duration_s()).is_idle());
        assert!(shock.pose(-1.0).is_idle());
        assert!(shock.pose(f32::NAN).is_idle());
        // Starts at rest-ish: the envelope is zero at the arrival.
        assert!(shock.pose(0.0).pitch.abs() < 1.0e-6);
        // Same seed, same motion; another seed, other motion.
        let again = GroundShock::default()
            .mount_shock(WaveKind::Seismic, 3300.0, 7)
            .expect("shock");
        assert_eq!(shock, again);
        assert_eq!(shock.pose(0.9), again.pose(0.9));
        let other = GroundShock::default()
            .mount_shock(WaveKind::Seismic, 3300.0, 8)
            .expect("shock");
        assert_ne!(shock.pose(0.9), other.pose(0.9));
    }

    #[test]
    fn negligible_and_bad_inputs_make_no_shock() {
        let g = GroundShock::default();
        assert!(g.mount_shock(WaveKind::Seismic, f32::NAN, 1).is_none());
        assert!(g.mount_shock(WaveKind::Seismic, 1.0e9, 1).is_none());
        assert!(g.mount_shock(WaveKind::Airblast, 1.0e9, 1).is_none());
        let none = GroundShock { yield_kt: 0.0, ..g };
        assert!(none.mount_shock(WaveKind::Airblast, 100.0, 1).is_none());
        assert_eq!(g.ppv_m_s(f32::INFINITY), 0.0);
        for d in [-5.0, 0.0] {
            assert!(
                g.seismic_angle_rad(d).is_finite() && g.seismic_angle_rad(d) <= 0.35,
                "{d}"
            );
            assert!(
                g.air_angle_rad(d).is_finite() && g.air_angle_rad(d) <= 0.35,
                "{d}"
            );
        }
    }

    #[test]
    fn pixels_follow_the_lens() {
        // 1 degree on a 60 degree lens at 2160 rows is 32.7 px (tan(1 deg) / tan(30 deg) * 1080).
        let px = angle_to_pixels(1.0f32.to_radians(), VFOV, 2160.0);
        assert!((px - 32.65).abs() < 0.2, "{px}");
        assert_eq!(angle_to_pixels(0.1, 0.0, 2160.0), 0.0);
    }

    /// One front speed: the shake's airblast constant is exactly the crash effects'
    /// `SHOCK_FRONT_SPEED` (ADR-1497), both now re-exported from `regia_core::SPEED_OF_SOUND_M_S`
    /// (the server build has no `regia_vfx`, so the link is asserted here, under `client`).
    #[cfg(feature = "client")]
    #[test]
    fn the_airblast_speed_is_the_crash_front_speed() {
        assert_eq!(AIR_SPEED_M_S, regia_vfx::crash::SHOCK_FRONT_SPEED);
        assert_eq!(AIR_SPEED_M_S, regia_core::SPEED_OF_SOUND_M_S);
    }
}
