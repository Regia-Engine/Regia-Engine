// SPDX-License-Identifier: MPL-2.0
//! The bird spec (`bird.json`, version 1): where it perches, what size it is, and how it
//! reacts to the camera.

use serde::{Deserialize, Serialize};

/// Format version of a bird spec.
pub const BIRD_SPEC_VERSION: u32 = 1;

/// A size class with typical dimensions and wingbeat. Approximate values for common species
/// from public field-guide measurements; wingbeat frequencies follow the measured cruising
/// values summarized by Pennycuick (1996, "Wingbeat frequency of birds in steady cruising
/// flight", J. Exp. Biol. 199).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BirdSpecies {
    /// Sparrow-sized: 0.23 m span, 0.16 m long, about 15 beats per second.
    Sparrow,
    /// Pigeon-sized: 0.67 m span, 0.33 m long, about 6 Hz.
    Pigeon,
    /// Crow-sized: 0.95 m span, 0.47 m long, about 4 Hz.
    #[default]
    Crow,
    /// Gull-sized: 1.3 m span, 0.6 m long, about 3 Hz.
    Gull,
    /// Buzzard-sized raptor: 1.2 m span, 0.53 m long, about 3 Hz.
    Raptor,
}

impl BirdSpecies {
    /// `(wingspan m, body length m, wingbeat Hz, body colour, wing colour)`.
    #[must_use]
    pub fn preset(self) -> (f32, f32, f32, [f32; 3], [f32; 3]) {
        match self {
            Self::Sparrow => (0.23, 0.16, 15.0, [0.36, 0.27, 0.19], [0.30, 0.22, 0.15]),
            Self::Pigeon => (0.67, 0.33, 6.0, [0.42, 0.44, 0.48], [0.36, 0.38, 0.42]),
            Self::Crow => (0.95, 0.47, 4.0, [0.035, 0.035, 0.04], [0.03, 0.03, 0.035]),
            Self::Gull => (1.3, 0.6, 3.0, [0.82, 0.83, 0.84], [0.55, 0.57, 0.6]),
            Self::Raptor => (1.2, 0.53, 3.0, [0.32, 0.22, 0.14], [0.26, 0.18, 0.12]),
        }
    }

    /// The species' snake-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sparrow => "sparrow",
            Self::Pigeon => "pigeon",
            Self::Crow => "crow",
            Self::Gull => "gull",
            Self::Raptor => "raptor",
        }
    }
}

/// What the bird stands on. A bird on a branch or post drops a little as it launches; one on
/// the ground jumps straight up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Perch {
    /// Standing on the ground.
    #[default]
    Ground,
    /// On a branch.
    Branch,
    /// On a post or rock.
    Post,
}

/// When the bird startles.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StartleSpec {
    /// Startles when the camera's centre comes this close, metres.
    pub distance_m: f32,
    /// Startles when the camera closes at this speed or faster (m/s) while it is within
    /// `alert_distance_m`.
    pub closing_speed_m_s: f32,
    /// Range within which a fast approach is seen, metres.
    pub alert_distance_m: f32,
    /// Startles when the wake's front reaches it with this peak gust or more, m/s. A body above
    /// the speed of sound cannot be heard coming; its boom arrives with the front.
    pub gust_m_s: f32,
    /// Delay from the cue to the first movement, seconds (a startle reflex: tens of ms).
    pub reaction_s: f32,
    /// Length of the flinch (snap pose, wings flared, eyes wide) before the take-off, seconds
    /// (`0.1..=0.2`).
    pub flinch_s: f32,
}

impl Default for StartleSpec {
    fn default() -> Self {
        Self {
            distance_m: 30.0,
            closing_speed_m_s: 12.0,
            alert_distance_m: 120.0,
            gust_m_s: 2.0,
            reaction_s: 0.06,
            flinch_s: 0.15,
        }
    }
}

/// How it flees.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FleeSpec {
    /// Flight speed it flees at, m/s.
    pub speed_m_s: f32,
    /// Climb angle of the take-off, degrees.
    pub takeoff_climb_deg: f32,
    /// Seconds of panicked take-off (fast, deep wingbeats) before the erratic flight.
    pub takeoff_s: f32,
    /// How erratic the flight is (`0..=1`): the size of its random jinks.
    pub erratic: f32,
    /// Seconds between jinks.
    pub jink_s: f32,
    /// Seconds after the take-off when it stops flapping and glides away.
    pub glide_after_s: f32,
    /// Glide speed, m/s.
    pub glide_speed_m_s: f32,
    /// Glide angle below the horizon, degrees.
    pub glide_deg: f32,
    /// How strongly the camera's wake throws it (1 = drag-limited response in 0.2 s).
    pub wake_gain: f32,
}

impl Default for FleeSpec {
    fn default() -> Self {
        Self {
            speed_m_s: 13.0,
            takeoff_climb_deg: 35.0,
            takeoff_s: 0.9,
            erratic: 0.6,
            jink_s: 0.45,
            glide_after_s: 3.5,
            glide_speed_m_s: 11.0,
            glide_deg: 6.0,
            wake_gain: 1.0,
        }
    }
}

/// One bird.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BirdSpec {
    /// Format version ([`BIRD_SPEC_VERSION`]).
    pub version: u32,
    /// Unique name (agents refer to the bird by it).
    pub name: String,
    /// Seed of its idle fidgets and its flight's jinks.
    pub seed: u64,
    /// Where its feet are, metres.
    pub position: [f32; 3],
    /// Which way it faces while perched, degrees about +Y (0 = facing −Z).
    pub facing_deg: f32,
    /// What it stands on.
    pub perch: Perch,
    /// Size class.
    pub species: BirdSpecies,
    /// Multiplies the species' size (`0.25..=4`).
    pub size_scale: f32,
    /// Wingbeat frequency override, Hz.
    pub flap_hz: Option<f32>,
    /// Body colour override, linear RGB.
    pub color: Option<[f32; 3]>,
    /// Wing colour override, linear RGB.
    pub wing_color: Option<[f32; 3]>,
    /// When it startles.
    pub startle: StartleSpec,
    /// How it flees.
    pub flee: FleeSpec,
}

impl Default for BirdSpec {
    fn default() -> Self {
        Self {
            version: BIRD_SPEC_VERSION,
            name: "bird".into(),
            seed: 1,
            position: [0.0; 3],
            facing_deg: 0.0,
            perch: Perch::Ground,
            species: BirdSpecies::Crow,
            size_scale: 1.0,
            flap_hz: None,
            color: None,
            wing_color: None,
            startle: StartleSpec::default(),
            flee: FleeSpec::default(),
        }
    }
}

/// Why a bird spec was refused.
#[derive(Debug, thiserror::Error)]
pub enum BirdSpecError {
    /// Not valid JSON for the spec.
    #[error("bird spec: {0}")]
    Parse(String),
    /// Newer than this build reads.
    #[error("bird spec version {found} is newer than {BIRD_SPEC_VERSION}")]
    Version {
        /// The version in the document.
        found: u32,
    },
    /// The name is empty.
    #[error("bird spec: the name is empty")]
    NoName,
    /// The position is not finite.
    #[error("bird spec: the position is not finite")]
    BadPosition,
}

/// The resolved size and look of a bird.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BirdBody {
    /// Wingspan, metres.
    pub wingspan_m: f32,
    /// Beak to tail tip, metres.
    pub length_m: f32,
    /// Wingbeat, Hz.
    pub flap_hz: f32,
    /// Body colour.
    pub color: [f32; 3],
    /// Wing colour.
    pub wing_color: [f32; 3],
}

impl BirdSpec {
    /// Parses a JSON document (unknown fields refused), checks it and clamps it.
    ///
    /// # Errors
    ///
    /// [`BirdSpecError`] for bad JSON, a newer version, an empty name or a non-finite position.
    pub fn from_json(text: &str) -> Result<Self, BirdSpecError> {
        let spec: Self = serde_json::from_str(text).map_err(|e| BirdSpecError::Parse(e.to_string()))?;
        spec.checked()
    }

    /// Checks and clamps a spec built in code.
    ///
    /// # Errors
    ///
    /// As [`Self::from_json`] (except parsing).
    pub fn checked(self) -> Result<Self, BirdSpecError> {
        if self.version > BIRD_SPEC_VERSION {
            return Err(BirdSpecError::Version { found: self.version });
        }
        if self.name.trim().is_empty() {
            return Err(BirdSpecError::NoName);
        }
        if !self.position.iter().all(|v| v.is_finite()) {
            return Err(BirdSpecError::BadPosition);
        }
        Ok(self.clamped())
    }

    /// Every value clamped into its documented range.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        let s = StartleSpec::default();
        let f = FleeSpec::default();
        self.version = BIRD_SPEC_VERSION;
        self.facing_deg = fix(self.facing_deg, -3600.0, 3600.0, 0.0);
        self.size_scale = fix(self.size_scale, 0.25, 4.0, 1.0);
        self.flap_hz = self.flap_hz.map(|h| fix(h, 0.5, 40.0, 4.0));
        self.startle = StartleSpec {
            distance_m: fix(self.startle.distance_m, 0.0, 10_000.0, s.distance_m),
            closing_speed_m_s: fix(self.startle.closing_speed_m_s, 0.0, 10_000.0, s.closing_speed_m_s),
            alert_distance_m: fix(self.startle.alert_distance_m, 0.0, 10_000.0, s.alert_distance_m),
            gust_m_s: fix(self.startle.gust_m_s, 0.0, 1_000.0, s.gust_m_s),
            reaction_s: fix(self.startle.reaction_s, 0.0, 1.0, s.reaction_s),
            flinch_s: fix(self.startle.flinch_s, 0.1, 0.2, s.flinch_s),
        };
        self.flee = FleeSpec {
            speed_m_s: fix(self.flee.speed_m_s, 1.0, 60.0, f.speed_m_s),
            takeoff_climb_deg: fix(self.flee.takeoff_climb_deg, 0.0, 80.0, f.takeoff_climb_deg),
            takeoff_s: fix(self.flee.takeoff_s, 0.1, 5.0, f.takeoff_s),
            erratic: fix(self.flee.erratic, 0.0, 1.0, f.erratic),
            jink_s: fix(self.flee.jink_s, 0.1, 5.0, f.jink_s),
            glide_after_s: fix(self.flee.glide_after_s, 0.5, 60.0, f.glide_after_s),
            glide_speed_m_s: fix(self.flee.glide_speed_m_s, 1.0, 60.0, f.glide_speed_m_s),
            glide_deg: fix(self.flee.glide_deg, 0.0, 45.0, f.glide_deg),
            wake_gain: fix(self.flee.wake_gain, 0.0, 10.0, f.wake_gain),
        };
        self
    }

    /// The resolved size, wingbeat and colours.
    #[must_use]
    pub fn body(&self) -> BirdBody {
        let (span, length, hz, color, wing) = self.species.preset();
        let k = self.size_scale;
        BirdBody {
            wingspan_m: span * k,
            length_m: length * k,
            // Larger birds of one shape beat slower: frequency scales roughly as length^-1.
            flap_hz: self.flap_hz.unwrap_or(hz / k),
            color: self.color.unwrap_or(color),
            wing_color: self.wing_color.unwrap_or(wing),
        }
    }
}
