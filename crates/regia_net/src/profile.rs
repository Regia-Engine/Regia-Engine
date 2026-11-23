// SPDX-License-Identifier: MPL-2.0
//! `.netprofile.ron`: per-entity relevance, priority, rate, quantization and interpolation
//! settings (spec 72 § 8, spec 74 § 10).
//!
//! `format_version: 2` (v2 adds `extrapolation_gate_m`, ADR-8522; a v1 file loads with the gate off
//! and `regia upgrade-project` rewrites its version). Every field has a serde default so profiles
//! written without the newer fields load. Registered in `check-formats` and `regia upgrade-project`.
//! See `docs/formats/netprofile-ron.md`.
//!
//! An entity names its profile in `Replicated.profile` (or at runtime with [`NetProfileRef`],
//! which wins); without one it uses the project default
//! (`config/net.ron` `replication.default_profile`). Built-in profiles live under
//! `engine://net/profiles/` and are addressed by that path or by their short name.

use std::collections::{BTreeMap, BTreeSet};

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The `format_version` this build writes and the newest it reads. Version 2 adds the optional
/// `rate_tiers` (spec 120 § 8.3, ADR-8031) and `extrapolation_gate_m` (spec 120 § 8.2, ADR-8522;
/// one unreleased v2, ADR-8572); a version 1 file loads unchanged.
pub const NET_PROFILE_FORMAT_VERSION: u32 = 2;

/// The rotation half of the extrapolation gate (spec 120 § 8.2, ADR-8522): with
/// [`NetProfile::extrapolation_gate_m`] set, a rotation that moved more than this many degrees from
/// the record the client holds is sent even when the position is within the gate.
pub const EXTRAPOLATION_GATE_DEG: f32 = 0.5;

/// The largest `extrapolation_gate_m` a profile may set, in metres.
pub const MAX_EXTRAPOLATION_GATE_M: f32 = 10.0;

/// The path prefix of the built-in profiles.
pub const BUILTIN_PROFILE_PREFIX: &str = "engine://net/profiles/";

pub use crate::interest::{RateTier, RateTiers, RelevanceShape};

/// How a proxy is shown between received states (spec 74 § 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InterpolationMode {
    /// Linear position blend (the default).
    #[default]
    Linear,
    /// Cubic Hermite with replicated velocities (fast curved motion).
    Hermite,
    /// Apply the newest state as it arrives.
    None,
}

fn d_priority() -> f32 {
    1.0
}
fn d_relevance() -> RelevanceShape {
    RelevanceShape::Spatial {
        radius: 150.0,
        hysteresis: 0.15,
    }
}
fn d_one() -> u32 {
    1
}
fn d_linger() -> u32 {
    30
}
fn d_owner_boost() -> f32 {
    4.0
}
fn d_true() -> bool {
    true
}
fn d_position_step() -> Option<f32> {
    Some(0.001)
}
fn d_version() -> u32 {
    NET_PROFILE_FORMAT_VERSION
}
fn d_snap() -> f32 {
    5.0
}
fn d_extrapolate() -> f32 {
    100.0
}

/// One `.netprofile.ron` (spec 72 § 8 plus the spec 74 § 10 fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetProfile {
    /// Always [`NET_PROFILE_FORMAT_VERSION`] when written.
    #[serde(default = "d_version")]
    pub format_version: u32,
    /// Priority weight (> 0). Only ratios between profiles matter.
    #[serde(default = "d_priority")]
    pub priority: f32,
    /// Spatial or world relevance.
    #[serde(default = "d_relevance")]
    pub relevance: RelevanceShape,
    /// Relevant to every connection in game (game state, team scores).
    #[serde(default)]
    pub sent_to_all: bool,
    /// Relevant to its owner only (beats `sent_to_all`).
    #[serde(default)]
    pub owner_only: bool,
    /// Passes only if one of the entity's groups is allowed for the connection.
    #[serde(default)]
    pub group_required: bool,
    /// Relevant exactly when its parent or owner-link target is relevant.
    #[serde(default)]
    pub follow_parent: bool,
    /// A registered custom relevance filter by name.
    #[serde(default)]
    pub custom_filter: Option<String>,
    /// At most one send per this many ticks (>= 1).
    #[serde(default = "d_one")]
    pub min_interval_ticks: u32,
    /// An entity with changes is sent at least this often, regardless of priority.
    #[serde(default)]
    pub max_interval_ticks: Option<u32>,
    /// Ticks an entity that lost relevance stays on the client before it is despawned.
    #[serde(default = "d_linger")]
    pub linger_ticks: u32,
    /// Priority factor for entities owned by the connection (>= 1).
    #[serde(default = "d_owner_boost")]
    pub owner_boost: f32,
    /// Entities outside every viewpoint's forward cone gain half priority.
    #[serde(default = "d_true")]
    pub view_falloff: bool,
    /// Quantization step of replicated positions in metres (`None` = raw `f32`).
    #[serde(default = "d_position_step")]
    pub position_step_m: Option<f32>,
    /// How proxies are shown between states (spec 74).
    #[serde(default)]
    pub interpolation: InterpolationMode,
    /// A jump larger than this between two states snaps instead of blending (spec 74).
    #[serde(default = "d_snap")]
    pub snap_distance_m: f32,
    /// How long a proxy extrapolates past its newest state (spec 74; 0 disables).
    #[serde(default = "d_extrapolate")]
    pub max_extrapolate_ms: f32,
    /// Show a spawned proxy at once instead of when its spawn tick is shown (spec 74).
    #[serde(default)]
    pub appear_immediately: bool,
    /// Distance send-rate tiers (spec 120 § 8.3; `None`: every send, the version 1 behavior).
    #[serde(default)]
    pub rate_tiers: Option<RateTiers>,
    /// The extrapolation gate in metres (spec 120 § 8.2, ADR-8522; `None`, the default, is off).
    /// With a gate, the acknowledged state channel sends a transform-only change only when the
    /// client's extrapolation of the record it holds (or of any record on its way) along that
    /// record's velocity is farther than this from the server's position, or its rotation more
    /// than [`EXTRAPOLATION_GATE_DEG`] off; records carry the steady bit, so the client keeps
    /// extrapolating them past `max_extrapolate_ms`. The shown error is then up to the gate, not
    /// the precision. In `(0, 10]` metres.
    #[serde(default)]
    pub extrapolation_gate_m: Option<f32>,
}

impl Default for NetProfile {
    fn default() -> Self {
        Self {
            format_version: NET_PROFILE_FORMAT_VERSION,
            priority: d_priority(),
            relevance: d_relevance(),
            sent_to_all: false,
            owner_only: false,
            group_required: false,
            follow_parent: false,
            custom_filter: None,
            min_interval_ticks: 1,
            max_interval_ticks: None,
            linger_ticks: d_linger(),
            owner_boost: d_owner_boost(),
            view_falloff: true,
            position_step_m: d_position_step(),
            interpolation: InterpolationMode::Linear,
            snap_distance_m: d_snap(),
            max_extrapolate_ms: d_extrapolate(),
            appear_immediately: false,
            rate_tiers: None,
            extrapolation_gate_m: None,
        }
    }
}

/// Why a profile could not be loaded.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProfileError {
    /// Not valid RON for the profile shape.
    #[error("netprofile: {0}")]
    Parse(String),
    /// Written by a newer build.
    #[error("netprofile: format_version {found} is newer than {supported}")]
    TooNew {
        /// The file's version.
        found: u32,
        /// The newest this build reads.
        supported: u32,
    },
    /// A field is out of its range.
    #[error("netprofile: field `{field}`: {message}")]
    Invalid {
        /// The field name.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
}

impl InterpolationMode {
    /// The name used in profiles and agent replies.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Hermite => "hermite",
            Self::None => "none",
        }
    }
}

impl NetProfile {
    /// The spec 74 interpolation fields of this profile.
    pub fn interp_profile(&self) -> crate::interp::InterpProfile {
        crate::interp::InterpProfile {
            interpolation: self.interpolation,
            snap_distance_m: self.snap_distance_m,
            max_extrapolate_ms: self.max_extrapolate_ms,
            appear_immediately: self.appear_immediately,
        }
    }

    /// Reads `.netprofile.ron` text and validates it. An older `format_version` loads with its newer
    /// fields at their defaults and keeps its version ([`NetProfile::migrate_ron`] rewrites it).
    pub fn from_ron(text: &str) -> Result<Self, ProfileError> {
        let p: NetProfile = ron::from_str(text).map_err(|e| ProfileError::Parse(e.to_string()))?;
        if p.format_version > NET_PROFILE_FORMAT_VERSION {
            return Err(ProfileError::TooNew {
                found: p.format_version,
                supported: NET_PROFILE_FORMAT_VERSION,
            });
        }
        p.validate()?;
        Ok(p)
    }

    /// `regia upgrade-project`'s step for `.netprofile.ron` (ADR-8031): a version 1 file only needs
    /// its version number (v2 adds the optional `rate_tiers`), so the step rewrites that number and
    /// keeps every other byte; a file that names no version is written out at the current one. A
    /// current file comes back unchanged.
    pub fn migrate_ron(text: &str) -> Result<String, ProfileError> {
        let p = Self::from_ron(text)?;
        if p.format_version == NET_PROFILE_FORMAT_VERSION && text.contains("format_version") {
            return Ok(text.to_owned());
        }
        if let Some(out) =
            regia_core::version::rewrite_ron_version(text, "format_version", NET_PROFILE_FORMAT_VERSION)
        {
            return Ok(out);
        }
        Self {
            format_version: NET_PROFILE_FORMAT_VERSION,
            ..p
        }
        .to_ron()
    }

    /// Writes the profile as RON. `\n` on every platform: ron's default is `\r\n` on Windows,
    /// and a `.netprofile.ron` is a hashed gameplay kind, so its bytes must not depend on the host
    /// (ADR-4772, same fix as `KnownHosts::to_ron`, ADR-3150).
    pub fn to_ron(&self) -> Result<String, ProfileError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ProfileError::Parse(e.to_string()))
    }

    /// The load-time rules of spec 72 § 8 (and the spec 74 fields' ranges).
    pub fn validate(&self) -> Result<(), ProfileError> {
        let bad = |field: &'static str, message: String| Err(ProfileError::Invalid { field, message });
        if !(self.priority > 0.0 && self.priority.is_finite()) {
            return bad("priority", format!("must be > 0, is {}", self.priority));
        }
        if let RelevanceShape::Spatial { radius, hysteresis } = self.relevance {
            if !(radius > 0.0 && radius.is_finite()) {
                return bad("relevance.radius", format!("must be > 0, is {radius}"));
            }
            if !(0.0..=1.0).contains(&hysteresis) {
                return bad(
                    "relevance.hysteresis",
                    format!("must be in 0..=1, is {hysteresis}"),
                );
            }
        }
        if self.min_interval_ticks < 1 {
            return bad("min_interval_ticks", "must be >= 1".into());
        }
        if let Some(max) = self.max_interval_ticks
            && max < self.min_interval_ticks
        {
            return bad(
                "max_interval_ticks",
                format!(
                    "must be >= min_interval_ticks ({}), is {max}",
                    self.min_interval_ticks
                ),
            );
        }
        if !(self.owner_boost >= 1.0 && self.owner_boost.is_finite()) {
            return bad("owner_boost", format!("must be >= 1, is {}", self.owner_boost));
        }
        if let Some(step) = self.position_step_m
            && !(1e-5..=1.0).contains(&step)
        {
            return bad("position_step_m", format!("must be in [1e-5, 1.0], is {step}"));
        }
        if !(self.snap_distance_m >= 0.0 && self.snap_distance_m.is_finite()) {
            return bad(
                "snap_distance_m",
                format!("must be >= 0, is {}", self.snap_distance_m),
            );
        }
        if !(self.max_extrapolate_ms >= 0.0 && self.max_extrapolate_ms.is_finite()) {
            return bad(
                "max_extrapolate_ms",
                format!("must be >= 0, is {}", self.max_extrapolate_ms),
            );
        }
        if let Some(t) = &self.rate_tiers {
            t.check()
                .map_err(|(field, message)| ProfileError::Invalid { field, message })?;
        }
        if let Some(gate) = self.extrapolation_gate_m
            && !(gate > 0.0 && gate <= MAX_EXTRAPOLATION_GATE_M)
        {
            return bad(
                "extrapolation_gate_m",
                format!("must be in (0, {MAX_EXTRAPOLATION_GATE_M}], is {gate}"),
            );
        }
        Ok(())
    }

    /// The radius the interest grid visits for this profile (`None` = not spatial).
    pub fn spatial_radius(&self) -> Option<f32> {
        match self.relevance {
            RelevanceShape::Spatial { radius, hysteresis } => Some(radius * (1.0 + hysteresis)),
            RelevanceShape::World => None,
        }
    }

    /// `owner_only` together with `sent_to_all` is treated as owner-exclusive (spec 72 § 5.1).
    pub fn is_conflicting(&self) -> bool {
        self.owner_only && self.sent_to_all
    }

    /// This profile's position quantizer (`position_step_m`; `None` = raw `f32`).
    pub fn position_quantizer(&self) -> Option<PositionQuantizer> {
        self.position_step_m.and_then(PositionQuantizer::new)
    }

    /// `p` on this profile's position grid (unchanged without a step). See [`PositionQuantizer`].
    pub fn quantize_position(&self, p: glam::Vec3) -> glam::Vec3 {
        self.position_quantizer().map_or(p, |q| q.snap(p))
    }
}

/// The position grid of `position_step_m` (spec 72 § 8, ADR-2413): each axis is a whole number of
/// steps. A value is the nearest grid point (computed in `f64`, then rounded once to `f32`), so the
/// error per axis is at most half a step plus half an `f32` unit in the last place of the value.
/// Axes whose step count would pass `2^31` (beyond 2 147 km at 1 mm) are sent unchanged, as are
/// non-finite values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionQuantizer {
    step: f64,
}

impl PositionQuantizer {
    /// A quantizer for `step_m` metres; `None` for a step that is not finite and positive.
    pub fn new(step_m: f32) -> Option<Self> {
        (step_m.is_finite() && step_m > 0.0).then_some(Self {
            step: f64::from(step_m),
        })
    }

    /// The step in metres.
    pub fn step_m(&self) -> f32 {
        self.step as f32
    }

    /// The whole number of steps of one axis (`None`: out of range or not finite).
    pub fn steps(&self, v: f32) -> Option<i32> {
        if !v.is_finite() {
            return None;
        }
        let n = (f64::from(v) / self.step).round();
        (n.abs() <= f64::from(i32::MAX)).then_some(n as i32)
    }

    /// The position of a whole number of steps.
    pub fn value(&self, steps: i32) -> f32 {
        (f64::from(steps) * self.step) as f32
    }

    /// One axis on the grid.
    pub fn snap_axis(&self, v: f32) -> f32 {
        self.steps(v).map_or(v, |n| self.value(n))
    }

    /// A position on the grid.
    pub fn snap(&self, p: glam::Vec3) -> glam::Vec3 {
        glam::Vec3::new(self.snap_axis(p.x), self.snap_axis(p.y), self.snap_axis(p.z))
    }
}

/// The position grid of the profile entity `e` uses (ADR-2413 (4)), read-only: its
/// [`NetProfileRef`], else `Replicated.profile`, else the project default; a name that is not
/// loaded falls back to the project default, then to the built-in `default`, as
/// [`NetProfiles::resolve`] does (without its log line). `None`: no [`NetProfiles`] resource, or a
/// profile without `position_step_m`.
///
/// This is the hook the transform fast path calls for a root's wire translation (see
/// [`snap_root_translation`]).
pub fn entity_position_quantizer(world: &World, e: Entity) -> Option<PositionQuantizer> {
    entity_profile(world, e)?.position_quantizer()
}

/// The profile `e` replicates with: its [`NetProfileRef`], else `Replicated.profile`, else the
/// project default (`None` without [`NetProfiles`]).
fn entity_profile(world: &World, e: Entity) -> Option<&NetProfile> {
    let profiles = world.get_resource::<NetProfiles>()?;
    let ent = world.get_entity(e).ok()?;
    let named = ent.get::<NetProfileRef>().map(|r| r.0.as_str()).or_else(|| {
        ent.get::<crate::replication::Replicated>()
            .and_then(|r| r.profile.as_deref())
    });
    named
        .and_then(|n| profiles.get(n))
        .or_else(|| profiles.get(profiles.default_key()))
        .or_else(|| profiles.get("default"))
}

/// The extrapolation gate of `e`'s profile in metres (`None`: off, the default; spec 120 § 8.2,
/// ADR-8522). Resolved with the position grid, once per profile change.
pub fn entity_extrapolation_gate(world: &World, e: Entity) -> Option<f32> {
    entity_profile(world, e)?.extrapolation_gate_m
}

/// `translation` (a root's position in the shared wire frame) snapped to the position grid of
/// `e`'s profile; unchanged when that profile has no step. The server calls it once per encoded
/// root transform, so motion under half a step makes no new transform version.
pub fn snap_root_translation(world: &World, e: Entity, translation: glam::Vec3) -> glam::Vec3 {
    entity_position_quantizer(world, e).map_or(translation, |q| q.snap(translation))
}

/// The built-in profiles (spec 72 § 8), by short name.
pub fn builtin_profiles() -> Vec<(&'static str, NetProfile)> {
    let spatial = |radius: f32| RelevanceShape::Spatial {
        radius,
        hysteresis: 0.15,
    };
    vec![
        ("default", NetProfile::default()),
        (
            "player",
            NetProfile {
                priority: 3.0,
                relevance: spatial(400.0),
                ..NetProfile::default()
            },
        ),
        (
            "projectile",
            NetProfile {
                priority: 2.0,
                relevance: spatial(200.0),
                linger_ticks: 2,
                ..NetProfile::default()
            },
        ),
        (
            "prop",
            NetProfile {
                priority: 0.5,
                relevance: spatial(100.0),
                ..NetProfile::default()
            },
        ),
        (
            "held",
            NetProfile {
                follow_parent: true,
                ..NetProfile::default()
            },
        ),
        (
            "game_state",
            NetProfile {
                sent_to_all: true,
                relevance: RelevanceShape::World,
                min_interval_ticks: 6,
                ..NetProfile::default()
            },
        ),
    ]
}

/// Strips `engine://net/profiles/` and `.netprofile.ron` from a built-in profile path.
pub fn profile_key(name: &str) -> &str {
    let name = name.strip_prefix(BUILTIN_PROFILE_PREFIX).unwrap_or(name);
    name.strip_suffix(".netprofile.ron").unwrap_or(name)
}

/// Names a profile for a replicated entity at runtime (path or short name of a loaded profile);
/// overrides `Replicated.profile`. Not reflected or saved.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NetProfileRef(pub String);

/// The loaded profiles (server and clients; `net.profiles`).
#[derive(Resource, Debug, Clone)]
pub struct NetProfiles {
    profiles: BTreeMap<String, NetProfile>,
    /// The project default (`replication.default_profile`), as a key.
    default_key: String,
    /// Profiles that failed to load: key → error (entities naming them use the default).
    pub failed: BTreeMap<String, ProfileError>,
    warned: BTreeSet<String>,
    generation: u64,
}

impl Default for NetProfiles {
    fn default() -> Self {
        Self {
            profiles: builtin_profiles()
                .into_iter()
                .map(|(k, p)| (k.to_string(), p))
                .collect(),
            default_key: "default".into(),
            failed: BTreeMap::new(),
            warned: BTreeSet::new(),
            generation: 0,
        }
    }
}

#[cfg(test)]
thread_local! {
    static PROFILE_GETS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Sets this thread's [`NetProfiles::get`] count to zero.
#[cfg(test)]
pub(crate) fn reset_profile_gets() {
    PROFILE_GETS.with(|c| c.set(0));
}

/// [`NetProfiles::get`] calls on this thread since [`reset_profile_gets`].
#[cfg(test)]
pub(crate) fn profile_gets() -> u64 {
    PROFILE_GETS.with(|c| c.get())
}

impl NetProfiles {
    /// Adds or replaces a profile. Invalid profiles are refused (and remembered in `failed`).
    pub fn insert(&mut self, name: &str, profile: NetProfile) -> Result<(), ProfileError> {
        self.generation += 1;
        let key = profile_key(name).to_string();
        if let Err(e) = profile.validate() {
            tracing::error!(target: "net", profile = %key, error = %e, "net.profile_invalid");
            self.failed.insert(key, e.clone());
            return Err(e);
        }
        if profile.is_conflicting() {
            tracing::warn!(target: "net", profile = %key, "net.profile_conflict");
        }
        self.failed.remove(&key);
        self.profiles.insert(key, profile);
        Ok(())
    }

    /// Loads `.netprofile.ron` text under a name (path or short name).
    pub fn load_ron(&mut self, name: &str, text: &str) -> Result<(), ProfileError> {
        match NetProfile::from_ron(text) {
            Ok(p) => self.insert(name, p),
            Err(e) => {
                self.generation += 1;
                let key = profile_key(name).to_string();
                tracing::error!(target: "net", profile = %key, error = %e, "net.profile_invalid");
                self.failed.insert(key, e.clone());
                Err(e)
            }
        }
    }

    /// Sets the project default profile (`replication.default_profile`).
    pub fn set_default(&mut self, name: &str) {
        self.generation += 1;
        self.default_key = profile_key(name).to_string();
    }

    /// Bumped by every change (insert, load, failure, new default): users resolve their profile
    /// again when it moves.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The project default profile key.
    pub fn default_key(&self) -> &str {
        &self.default_key
    }

    /// A profile by name, if loaded.
    pub fn get(&self, name: &str) -> Option<&NetProfile> {
        #[cfg(test)]
        PROFILE_GETS.with(|c| c.set(c.get() + 1));
        self.profiles.get(profile_key(name))
    }

    /// The profile an entity uses: its named one, else the project default, else the built-in
    /// `default`. A name that is unknown or failed to load logs an error once.
    pub fn resolve(&mut self, name: Option<&str>) -> (String, NetProfile) {
        if let Some(n) = name {
            let key = profile_key(n);
            if let Some(p) = self.profiles.get(key) {
                return (key.to_string(), p.clone());
            }
            if self.warned.insert(key.to_string()) {
                tracing::error!(target: "net", profile = %key, "net.profile_fallback");
            }
        }
        let key = self.default_key.clone();
        match self.profiles.get(&key) {
            Some(p) => (key, p.clone()),
            None => ("default".into(), NetProfile::default()),
        }
    }

    /// Every loaded profile, by key.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &NetProfile)> {
        self.profiles.iter().map(|(k, p)| (k.as_str(), p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit fix-net3-determinism M3 (ADR-4772): `.netprofile.ron` is a hashed gameplay kind, so
    /// its bytes must not depend on the host. ron's default newline is `\r\n` on Windows, so this
    /// only fails there; the assertion guards the contract on every platform.
    #[test]
    fn to_ron_uses_lf_on_every_platform() {
        let text = NetProfile::default().to_ron().expect("ron");
        assert!(text.contains('\n'), "the text has newlines");
        assert!(!text.contains('\r'), "no CR bytes on any platform");
        assert_eq!(
            NetProfile::from_ron(&text).expect("re-parse"),
            NetProfile::default()
        );
    }

    /// The RON newline is pinned to `\n` on every host (ron's default is `\r\n` on Windows,
    /// ADR-3150); the asserts fail there until the pin. The round trip accepts both.
    #[test]
    fn profile_ron_uses_lf_on_every_host() {
        let text = NetProfile::default().to_ron().expect("write");
        assert!(text.contains('\n'), "expected LF newlines");
        assert!(
            !text.contains('\r'),
            "the newline must be pinned to LF on every host"
        );
        assert_eq!(
            NetProfile::from_ron(&text).expect("read back"),
            NetProfile::default()
        );
    }

    /// ADR-2413: the quantization error stays within the step (half a step, plus the `f32` rounding
    /// of the value), for every built-in step size and positions from millimetres to tens of
    /// kilometres, deterministic inputs.
    #[test]
    fn quantization_error_stays_within_the_step() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for step in [1e-5_f32, 1e-4, 0.001, 0.01, 0.1, 1.0] {
            let profile = NetProfile {
                position_step_m: Some(step),
                ..NetProfile::default()
            };
            let q = profile.position_quantizer().expect("quantizer");
            for i in 0..4000 {
                let scale = [0.01_f32, 1.0, 100.0, 10_000.0, 50_000.0][i % 5];
                let unit = (next() >> 11) as f64 / (1u64 << 53) as f64;
                let v = ((unit * 2.0 - 1.0) * f64::from(scale)) as f32;
                let p = glam::Vec3::new(v, -v * 0.5, v * 0.25);
                let snapped = profile.quantize_position(p);
                for (a, b) in [(p.x, snapped.x), (p.y, snapped.y), (p.z, snapped.z)] {
                    let bound =
                        f64::from(step) / 2.0 + f64::from(a.abs().max(b.abs())) * f64::from(f32::EPSILON);
                    let err = (f64::from(a) - f64::from(b)).abs();
                    assert!(
                        err <= bound,
                        "step {step}: {a} -> {b} (error {err}, bound {bound})"
                    );
                    // On the grid: snapping again is a no-op.
                    assert_eq!(q.snap_axis(b), b, "step {step}: {b} is a grid point");
                }
            }
        }
        // Raw f32 without a step; non-finite and out-of-range values pass through.
        let raw = NetProfile {
            position_step_m: None,
            ..NetProfile::default()
        };
        let p = glam::Vec3::new(1.234_567, -2.0, 3.5);
        assert_eq!(raw.quantize_position(p), p);
        let q = PositionQuantizer::new(1e-5).expect("q");
        assert!(q.snap_axis(f32::NAN).is_nan());
        assert_eq!(q.snap_axis(1.0e9), 1.0e9, "past 2^31 steps: unchanged");
        assert!(PositionQuantizer::new(0.0).is_none());
        assert!(PositionQuantizer::new(-1.0).is_none());
    }

    /// ADR-2413 (4): an entity's grid comes from its `NetProfileRef`, else `Replicated.profile`,
    /// else the project default; an unknown name falls back to the default; no resource or no step
    /// leaves the translation raw.
    #[test]
    fn entity_grid_follows_the_profile_resolution() {
        use crate::replication::Replicated;
        let mut world = World::new();
        let p = glam::Vec3::new(0.123_456, 7.777_7, -3.271_9);
        let plain = world.spawn(Replicated::default()).id();
        assert_eq!(snap_root_translation(&world, plain, p), p, "no NetProfiles: raw");

        let mut profiles = NetProfiles::default();
        for (name, step) in [("coarse", Some(0.5_f32)), ("raw", None), ("fine", Some(0.01))] {
            let profile = NetProfile {
                position_step_m: step,
                ..NetProfile::default()
            };
            profiles.insert(name, profile).expect("valid profile");
        }
        world.insert_resource(profiles);
        let named = |name: &str| Replicated {
            profile: Some(name.into()),
            ..Replicated::default()
        };
        let coarse = world.spawn(named("coarse")).id();
        let raw = world.spawn(named("raw")).id();
        let unknown = world.spawn(named("no_such_profile")).id();
        let overridden = world.spawn((named("coarse"), NetProfileRef("fine".into()))).id();

        let default_step = NetProfile::default().position_step_m.expect("default step");
        let grid = |step: f32| PositionQuantizer::new(step).expect("q").snap(p);
        assert_eq!(snap_root_translation(&world, plain, p), grid(default_step));
        assert_eq!(snap_root_translation(&world, coarse, p), grid(0.5));
        assert_eq!(
            snap_root_translation(&world, coarse, p),
            glam::Vec3::new(0.0, 8.0, -3.5)
        );
        assert_eq!(snap_root_translation(&world, raw, p), p, "no step: raw f32");
        assert_eq!(snap_root_translation(&world, unknown, p), grid(default_step));
        assert_eq!(
            snap_root_translation(&world, overridden, p),
            grid(0.01),
            "NetProfileRef wins"
        );

        world.resource_mut::<NetProfiles>().set_default("coarse");
        assert_eq!(
            snap_root_translation(&world, plain, p),
            grid(0.5),
            "project default"
        );
        assert_eq!(snap_root_translation(&world, unknown, p), grid(0.5));
    }

    #[test]
    fn spec_example_loads_and_round_trips() {
        // The example of docs/formats/netprofile-ron.md (v2 since ADR-8031).
        let text = r#"NetProfile(
            format_version: 2,
            priority: 1.0,
            relevance: Spatial(radius: 150.0, hysteresis: 0.15),
            sent_to_all: false,
            owner_only: false,
            group_required: false,
            follow_parent: false,
            custom_filter: None,
            min_interval_ticks: 1,
            max_interval_ticks: None,
            linger_ticks: 30,
            owner_boost: 4.0,
            view_falloff: true,
            position_step_m: Some(0.001),
        )"#;
        let p = NetProfile::from_ron(text).expect("spec example");
        // The v2 example loads with every v2 field at its default (no rate tiers, the gate off,
        // ADR-8522).
        assert_eq!(p, NetProfile::default());
        // The same profile written as v1 still loads, with its version.
        let v1 = NetProfile::from_ron(&text.replace("format_version: 2", "format_version: 1")).expect("v1");
        assert_eq!(
            v1,
            NetProfile {
                format_version: 1,
                ..NetProfile::default()
            }
        );
        assert_eq!(p.extrapolation_gate_m, None);
        let back = NetProfile::from_ron(&p.to_ron().expect("write")).expect("read back");
        assert_eq!(back, p);
        // Missing fields default (spec 74 fields included).
        let p = NetProfile::from_ron("(priority: 2.0, relevance: World)").expect("partial");
        assert_eq!(p.priority, 2.0);
        assert_eq!(p.relevance, RelevanceShape::World);
        assert_eq!(p.interpolation, InterpolationMode::Linear);
        assert_eq!(p.snap_distance_m, 5.0);
    }

    #[test]
    fn validation_names_the_field() {
        let cases = [
            ("(priority: 0.0)", "priority"),
            (
                "(relevance: Spatial(radius: 0.0, hysteresis: 0.1))",
                "relevance.radius",
            ),
            (
                "(relevance: Spatial(radius: 5.0, hysteresis: 1.5))",
                "relevance.hysteresis",
            ),
            ("(min_interval_ticks: 0)", "min_interval_ticks"),
            (
                "(min_interval_ticks: 4, max_interval_ticks: Some(3))",
                "max_interval_ticks",
            ),
            ("(owner_boost: 0.5)", "owner_boost"),
            ("(position_step_m: Some(2.0))", "position_step_m"),
            ("(position_step_m: Some(0.000001))", "position_step_m"),
            ("(extrapolation_gate_m: Some(0.0))", "extrapolation_gate_m"),
            ("(extrapolation_gate_m: Some(-0.02))", "extrapolation_gate_m"),
            ("(extrapolation_gate_m: Some(11.0))", "extrapolation_gate_m"),
        ];
        for (text, field) in cases {
            match NetProfile::from_ron(text) {
                Err(ProfileError::Invalid { field: f, .. }) => assert_eq!(f, field, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        // Version 2 (ADR-8031) is current: the first unknown version is 3.
        assert!(matches!(
            NetProfile::from_ron("(format_version: 3)"),
            Err(ProfileError::TooNew { found: 3, .. })
        ));
        let gated = NetProfile::from_ron("(format_version: 2, extrapolation_gate_m: Some(0.02))")
            .expect("a 2 cm gate");
        assert_eq!(gated.extrapolation_gate_m, Some(0.02));
        let not_a_number = NetProfile {
            extrapolation_gate_m: Some(f32::NAN),
            ..NetProfile::default()
        };
        assert!(matches!(
            not_a_number.validate(),
            Err(ProfileError::Invalid {
                field: "extrapolation_gate_m",
                ..
            })
        ));
        assert!(matches!(
            NetProfile::from_ron("(priority: \"x\")"),
            Err(ProfileError::Parse(_))
        ));
        assert!(NetProfile::from_ron("(position_step_m: None)").is_ok());
    }

    #[test]
    fn builtins_and_fallback() {
        let mut ps = NetProfiles::default();
        for (name, p) in builtin_profiles() {
            p.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                ps.get(&format!("{BUILTIN_PROFILE_PREFIX}{name}.netprofile.ron"))
                    .is_some()
            );
        }
        assert_eq!(ps.get("player").map(|p| p.priority), Some(3.0));
        assert_eq!(ps.get("projectile").map(|p| p.linger_ticks), Some(2));
        assert!(ps.get("held").is_some_and(|p| p.follow_parent));
        assert!(
            ps.get("game_state")
                .is_some_and(|p| p.sent_to_all && p.min_interval_ticks == 6)
        );
        // An invalid profile is refused; entities naming it use the default.
        assert!(ps.load_ron("bad", "(priority: -1.0)").is_err());
        assert!(ps.failed.contains_key("bad"));
        let (key, p) = ps.resolve(Some("bad"));
        assert_eq!(key, "default");
        assert_eq!(p, NetProfile::default());
        ps.set_default("engine://net/profiles/prop.netprofile.ron");
        assert_eq!(ps.resolve(None).0, "prop");
        assert_eq!(ps.resolve(Some("player")).0, "player");
    }
}
