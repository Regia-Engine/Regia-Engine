// SPDX-License-Identifier: MPL-2.0
//! Scene-wide effect significance (ADR-3275): which live instances spawn, and at what rate, under
//! a quality tier's caps.
//!
//! Every system has its own `system.budget` and distance `lod`; this module ranks the instances
//! of the whole scene. [`significance_score`] is a pure function of one instance (distance,
//! projected size, frustum visibility, priority and age). Each [`crate::scene::VfxScene::update`]
//! sorts the active instances by score and gives each a [`SignificanceState`] by rank under
//! [`SignificanceCaps`]: the first `full_instances` play at full rate, the rest up to
//! `max_instances` at `reduced_rate`, the rest are culled (no spawns, their particles finish).
//! With `pause_offscreen`, an instance out of view is paused (no spawns) and may catch up a
//! bounded warm-up when it comes back. Hysteresis (an incumbent bonus and a dwell time) keeps an
//! instance at the cut from flickering between states.
//!
//! [`SignificanceCaps::default`] sets no cap. The scene then schedules exactly as it did before
//! this module existed: same order, same headroom, same calls (every determinism pin holds).
//! Tier presets are [`crate::quality::significance_caps`].
//!
//! Determinism: fixed inputs only. The ranking sorts by score with the instance id as the tie
//! break (a total order), uses no hash map and no clock, and reuses its buffers frame to frame.

use regia_core::math::{Aabb, Frustum, Sphere, Vec3};

use crate::bounds::CullingVolume;

use crate::format::{ShapeDef, VelocityMode, VfxSystemDef};
use crate::gpu::VfxCamera;
use crate::scene::VfxLibrary;
use crate::sim::VfxInstance;

/// Score multiplier of an instance outside the view (when off-screen instances are not paused).
pub const OFFSCREEN_WEIGHT: f32 = 0.25;
/// Extra score of an instance that just started, fading linearly over [`YOUTH_WINDOW_S`].
pub const YOUTH_BOOST: f32 = 0.5;
/// Seconds over which the [`YOUTH_BOOST`] fades to nothing.
pub const YOUTH_WINDOW_S: f32 = 1.0;
/// Largest priority a system or instance can be given.
pub const MAX_PRIORITY: f32 = 1.0e6;
/// Smallest radius a system's bound is given, metres.
pub const MIN_RADIUS: f32 = 0.25;
/// Extent the bound gives a mesh-surface or skinned-mesh spawn shape (v7), metres: the mesh is an
/// asset the definition only names, so its size is not known here; a human-sized bound.
pub const MESH_SHAPE_RADIUS: f32 = 1.0;
/// Nearest distance a score uses, metres (an effect at the eye is not infinitely large).
const MIN_DISTANCE: f32 = 0.01;
/// Largest projected size a score uses.
const MAX_SIZE: f32 = 1.0e6;
/// `tan(fov_y / 2)` without a view (a 60° camera).
const DEFAULT_TAN_HALF_FOV: f32 = 0.577_350_3;

/// What an active instance does this frame, by its rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignificanceState {
    /// Spawns at its authored rate.
    Full,
    /// Rate emitters spawn at [`SignificanceCaps::reduced_rate`]; bursts and per-distance
    /// emitters keep their counts.
    Reduced,
    /// Out of view with [`SignificanceCaps::pause_offscreen`]: spawns nothing; a burst due now is
    /// spent without particles. Holds no rank slot.
    Paused,
    /// Ranked past [`SignificanceCaps::max_instances`]: spawns nothing, its live particles finish.
    Culled,
}

impl SignificanceState {
    /// Every state, in rank order.
    pub const ALL: [Self; 4] = [Self::Full, Self::Reduced, Self::Paused, Self::Culled];

    /// Lower-case name (`full`, `reduced`, `paused`, `culled`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Reduced => "reduced",
            Self::Paused => "paused",
            Self::Culled => "culled",
        }
    }

    /// Whether the state spawns particles.
    #[must_use]
    pub fn spawns(self) -> bool {
        matches!(self, Self::Full | Self::Reduced)
    }
}

/// What a spawn does when its system is at its [`SystemCap::max_instances`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StealPolicy {
    /// The new spawn is refused.
    #[default]
    Reject,
    /// The oldest active instance of the system is stopped (its particles finish).
    Oldest,
    /// The least significant active instance is stopped, unless the new one scores lower: then
    /// the new spawn is refused.
    LeastSignificant,
}

impl StealPolicy {
    /// Every policy.
    pub const ALL: [Self; 3] = [Self::Reject, Self::Oldest, Self::LeastSignificant];

    /// Lower-case name (`reject`, `oldest`, `least_significant`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Oldest => "oldest",
            Self::LeastSignificant => "least_significant",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// A per-system instance cap ([`SignificanceCaps::per_system`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SystemCap {
    /// The system name (the requested name or the medium variant played).
    pub system: String,
    /// Active instances of the system at most.
    pub max_instances: u32,
    /// What a spawn past the cap does.
    pub steal: StealPolicy,
}

/// Scene-wide caps of the significance ranking. The default sets none (the ranking is off).
#[derive(Debug, Clone, PartialEq)]
pub struct SignificanceCaps {
    /// Active instances that spawn (full plus reduced) at most.
    pub max_instances: Option<u32>,
    /// Of those, how many spawn at the full rate (`None`: all of them).
    pub full_instances: Option<u32>,
    /// Rate scale of a reduced instance, `0..=1`.
    pub reduced_rate: f32,
    /// Particles the whole scene spawns in one update at most, granted in rank order.
    pub max_spawns_per_frame: Option<u32>,
    /// Live particles at most (the pool capacity is the smaller of this and the profile's).
    pub max_live: Option<u32>,
    /// Instances out of view spawn nothing ([`SignificanceState::Paused`]).
    pub pause_offscreen: bool,
    /// Seconds of its rate a paused instance may catch up on the frame it comes back (0: none).
    pub warmup_s: f32,
    /// Particles that catch-up frame may spawn at most.
    pub warmup_max_particles: u32,
    /// Score bonus of an incumbent full or reduced instance (`0.25`: it must be outscored by 25 %).
    pub hysteresis: f32,
    /// Frames an instance keeps its ranked state after a change. Not held: instances coming
    /// back into view, demotions a cap needs, and a held instance that a new instance outranks
    /// (with [`Self::hysteresis`] counted for the held one): the newcomer takes its slot on its
    /// first ranking (ADR-7431). A newcomer that does not outrank a held instance waits for it.
    pub min_dwell_frames: u32,
    /// Metres outside the frustum an instance's bound must be before it counts as out of view
    /// (it is back in view once its bound touches the frustum).
    pub offscreen_margin: f32,
    /// Per-system instance caps, sorted by system name.
    pub per_system: Vec<SystemCap>,
}

impl Default for SignificanceCaps {
    fn default() -> Self {
        Self {
            max_instances: None,
            full_instances: None,
            reduced_rate: 0.5,
            max_spawns_per_frame: None,
            max_live: None,
            pause_offscreen: false,
            warmup_s: 0.0,
            warmup_max_particles: 32,
            hysteresis: 0.25,
            min_dwell_frames: 30,
            offscreen_margin: 2.0,
            per_system: Vec::new(),
        }
    }
}

impl SignificanceCaps {
    /// Whether any cap is set. Without one the scene schedules exactly as before (ADR-3275).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.max_instances.is_some()
            || self.full_instances.is_some()
            || self.max_spawns_per_frame.is_some()
            || self.max_live.is_some()
            || self.pause_offscreen
            || !self.per_system.is_empty()
    }

    /// The cap of `system`, if one is set.
    #[must_use]
    pub fn system_cap(&self, system: &str) -> Option<&SystemCap> {
        self.per_system
            .binary_search_by(|c| c.system.as_str().cmp(system))
            .ok()
            .map(|i| &self.per_system[i])
    }

    /// Sets (or replaces) the cap of `cap.system`.
    pub fn set_system_cap(&mut self, cap: SystemCap) {
        match self
            .per_system
            .binary_search_by(|c| c.system.as_str().cmp(&cap.system))
        {
            Ok(i) => self.per_system[i] = cap,
            Err(i) => self.per_system.insert(i, cap),
        }
    }

    /// Removes the cap of `system`; `false` when it had none.
    pub fn remove_system_cap(&mut self, system: &str) -> bool {
        match self
            .per_system
            .binary_search_by(|c| c.system.as_str().cmp(system))
        {
            Ok(i) => {
                self.per_system.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    /// The caps with every number in range: rates in `0..=1`, non-negative finite seconds and
    /// metres, the per-system list sorted by name with one entry per system (the last wins).
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let unit = |v: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                fallback
            }
        };
        let non_negative = |v: f32, fallback: f32| if v.is_finite() { v.max(0.0) } else { fallback };
        self.reduced_rate = unit(self.reduced_rate, 0.5);
        self.warmup_s = non_negative(self.warmup_s, 0.0).min(10.0);
        self.hysteresis = non_negative(self.hysteresis, 0.25).min(10.0);
        self.offscreen_margin = non_negative(self.offscreen_margin, 2.0);
        let mut list = std::mem::take(&mut self.per_system);
        list.reverse();
        list.sort_by(|a, b| a.system.cmp(&b.system));
        list.dedup_by(|later, earlier| later.system == earlier.system);
        self.per_system = list;
        self
    }
}

/// The inputs of [`significance_score`] for one instance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreInput {
    /// Distance from the eye, metres.
    pub distance: f32,
    /// Radius of the instance's bound, metres ([`system_radius`] times the instance scale).
    pub radius: f32,
    /// `tan(fov_y / 2)` of the view.
    pub tan_half_fov: f32,
    /// Whether the bound touches the view frustum.
    pub visible: bool,
    /// Priority, `0..=MAX_PRIORITY` (1 by default).
    pub priority: f32,
    /// Instance age, seconds.
    pub age: f32,
}

/// How much one instance matters this frame: `priority × size × visibility × youth`.
///
/// `size` is the projected size `radius / (distance × tan(fov_y / 2))` (the fraction of half the
/// screen height the bound covers), `visibility` 1 in view and [`OFFSCREEN_WEIGHT`] out of it,
/// `youth` `1 + YOUTH_BOOST × max(0, 1 − age / YOUTH_WINDOW_S)`. Always finite and `>= 0`; a
/// non-finite input scores as far as possible (distance), 0 (priority) or old (age).
#[must_use]
pub fn significance_score(input: &ScoreInput) -> f32 {
    let priority = if input.priority.is_finite() {
        input.priority.clamp(0.0, MAX_PRIORITY)
    } else {
        0.0
    };
    if !input.distance.is_finite() || !input.radius.is_finite() {
        return 0.0;
    }
    let distance = input.distance.max(MIN_DISTANCE);
    let tan = if input.tan_half_fov.is_finite() && input.tan_half_fov > 1e-4 {
        input.tan_half_fov
    } else {
        DEFAULT_TAN_HALF_FOV
    };
    let size = (input.radius.max(0.0) / (distance * tan)).min(MAX_SIZE);
    let visibility = if input.visible { 1.0 } else { OFFSCREEN_WEIGHT };
    let age = if input.age.is_finite() {
        input.age.max(0.0)
    } else {
        YOUTH_WINDOW_S
    };
    let youth = 1.0 + YOUTH_BOOST * (1.0 - age / YOUTH_WINDOW_S).max(0.0);
    priority * size * visibility * youth
}

/// A bound of where a system's particles go, metres: the largest over its emitters of the spawn
/// shape's extent, plus the fastest launch speed times half the longest lifetime (gravity and drag
/// ignored: a bound of the bulk), plus the largest particle size. At least [`MIN_RADIUS`].
#[must_use]
pub fn system_radius(def: &VfxSystemDef) -> f32 {
    let radius = def
        .emitters
        .iter()
        .map(|e| {
            let shape = match &e.init.shape {
                ShapeDef::Point => 0.0,
                ShapeDef::Sphere { radius, .. } => radius.max.abs().max(radius.min.abs()),
                ShapeDef::Box { half_extents, .. } => {
                    Vec3::new(half_extents.0, half_extents.1, half_extents.2).length()
                }
                ShapeDef::Cone { angle, length } => {
                    let l = length.max.abs().max(length.min.abs());
                    l / angle.abs().min(80.0).to_radians().cos()
                }
                ShapeDef::MeshSurface { .. } | ShapeDef::SkinnedMesh { .. } => MESH_SHAPE_RADIUS,
            };
            let speed = match &e.init.velocity.mode {
                VelocityMode::Radial { speed }
                | VelocityMode::Directional { speed, .. }
                | VelocityMode::Normal { speed } => speed.max.abs().max(speed.min.abs()),
                VelocityMode::Planar { speed, lift } => {
                    let s = speed.max.abs().max(speed.min.abs());
                    let l = lift.max.abs().max(lift.min.abs());
                    (s * s + l * l).sqrt()
                }
            };
            let life = e.spawn.lifetime.max.max(e.spawn.lifetime.min).max(0.0);
            let size = e.init.size.max.abs().max(e.init.size.min.abs());
            shape + 0.5 * speed * life + size
        })
        .fold(MIN_RADIUS, f32::max);
    if radius.is_finite() { radius } else { MIN_RADIUS }
}

/// The view the ranking measures against: the eye, the frustum and the field of view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignificanceView {
    eye: Vec3,
    frustum: Frustum,
    tan_half_fov: f32,
}

impl SignificanceView {
    /// The view of `camera` on a `width × height` viewport (the plugin copies `VfxView`).
    #[must_use]
    pub fn from_camera(camera: &VfxCamera, width: u32, height: u32) -> Self {
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        let (_, _, view_proj) = camera.matrices(aspect);
        let half = 0.5 * camera.fov_y;
        let tan_half_fov = if half.is_finite() && half > 0.0 {
            half.min(1.55).tan()
        } else {
            DEFAULT_TAN_HALF_FOV
        };
        Self {
            eye: camera.eye,
            frustum: Frustum::from_view_projection(&view_proj),
            tan_half_fov,
        }
    }

    /// The eye position.
    #[must_use]
    pub fn eye(&self) -> Vec3 {
        self.eye
    }

    /// `tan(fov_y / 2)`.
    #[must_use]
    pub fn tan_half_fov(&self) -> f32 {
        self.tan_half_fov
    }

    /// Whether a sphere at `center` with `radius` touches the frustum (conservative near corners).
    #[must_use]
    pub fn sees(&self, center: Vec3, radius: f32) -> bool {
        self.frustum.intersects_sphere(&Sphere {
            center,
            radius: radius.max(0.0),
        })
    }

    /// Whether `volume` grown by `margin` metres touches the frustum: the computed sphere, or the
    /// authored fixed box ([`crate::bounds::culling_volume`]).
    #[must_use]
    pub fn sees_volume(&self, volume: &CullingVolume, margin: f32) -> bool {
        let margin = margin.max(0.0);
        match volume {
            CullingVolume::Computed { center, radius } => self.sees(*center, radius + margin),
            CullingVolume::Fixed(b) => self.frustum.intersects_aabb(&Aabb::new(
                b.min - Vec3::splat(margin),
                b.max + Vec3::splat(margin),
            )),
        }
    }
}

/// Counts of the last ranking ([`VfxSignificance::stats`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SignificanceStats {
    /// Caps were active in the last update.
    pub enabled: bool,
    /// Instances at full rate.
    pub full: u32,
    /// Instances at the reduced rate.
    pub reduced: u32,
    /// Instances paused out of view.
    pub paused: u32,
    /// Instances culled by rank.
    pub culled: u32,
    /// Stopped or spent instances whose particles are finishing (not ranked).
    pub finishing: u32,
    /// Spawns refused by a per-system cap since the update before the last.
    pub rejected_spawns: u32,
    /// Spawns refused since the scene started.
    pub rejected_total: u64,
    /// Instances stopped to make room for a new spawn since the scene started.
    pub stolen_total: u64,
    /// State changes in the last update (new instances not counted).
    pub transitions: u32,
    /// State changes since the scene started.
    pub transitions_total: u64,
    /// Particles spawned by warm-ups in the last update.
    pub warmup_particles: u32,
    /// Particles the last update spawned (all instances).
    pub spawned: u32,
    /// Whether the last update stopped at [`SignificanceCaps::max_spawns_per_frame`].
    pub spawn_cap_reached: bool,
}

/// One live instance as the last ranking saw it ([`VfxSignificance::entries`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignificanceEntry {
    /// Instance id.
    pub id: u64,
    /// Its state (`None` while it is finishing: stopped or spent, not ranked).
    pub state: Option<SignificanceState>,
    /// Its score.
    pub score: f32,
    /// Distance from the eye, metres.
    pub distance: f32,
    /// Whether it was in view.
    pub visible: bool,
    /// Frames since its state last changed.
    pub frames_in_state: u32,
}

/// How an instance spawns this frame (scene-internal).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SpawnPlan {
    /// The authored schedule, exactly as without caps.
    Normal,
    /// The schedule over `(dt + warm) × rate`; a warm-up (`warm > 0`) is bounded in particles.
    Scaled {
        /// Rate scale.
        rate: f32,
        /// Catch-up seconds.
        warm: f32,
    },
    /// No particles; due bursts are spent.
    Hold,
}

/// The ranking state of one scene slot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SlotRank {
    id: u64,
    live: bool,
    /// Not ranked yet (just spawned, or the slot was reused).
    fresh: bool,
    state: SignificanceState,
    held: u32,
    visible: bool,
    finishing: bool,
    /// In `order` this frame.
    ranked: bool,
    /// A cap needs this frame to demote it despite its dwell.
    forced: bool,
    paused_s: f32,
    warm: f32,
    score: f32,
    distance: f32,
}

impl SlotRank {
    const EMPTY: Self = Self {
        id: 0,
        live: false,
        fresh: true,
        state: SignificanceState::Full,
        held: 0,
        visible: true,
        finishing: false,
        ranked: false,
        forced: false,
        paused_s: 0.0,
        warm: 0.0,
        score: 0.0,
        distance: 0.0,
    };

    fn fresh(id: u64) -> Self {
        Self {
            id,
            live: true,
            ..Self::EMPTY
        }
    }

    fn incumbent(&self) -> bool {
        !self.fresh && self.state.spawns()
    }
}

/// The scene's significance ranking: caps, view, priority overrides and per-slot state
/// ([`crate::scene::VfxScene::significance`]).
#[derive(Debug, Clone, Default)]
pub struct VfxSignificance {
    caps: SignificanceCaps,
    view: Option<SignificanceView>,
    /// The eye of the last update (the view's, or the LOD camera's without a view).
    eye: Vec3,
    /// Priority per system name, sorted by name.
    system_priority: Vec<(String, f32)>,
    /// Priority per instance id, sorted by id.
    instance_priority: Vec<(u64, f32)>,
    slots: Vec<SlotRank>,
    /// Ranked slots, most significant first (scratch reused every frame).
    order: Vec<u32>,
    /// Bound radius per library definition (scratch reused every frame).
    radii: Vec<f32>,
    stats: SignificanceStats,
    rejected_since: u32,
}

/// A clamped priority; `None` for a value that is not a finite number `>= 0`.
fn checked_priority(value: f32) -> Option<f32> {
    (value.is_finite() && value >= 0.0).then_some(value.min(MAX_PRIORITY))
}

impl VfxSignificance {
    /// The caps.
    #[must_use]
    pub fn caps(&self) -> &SignificanceCaps {
        &self.caps
    }

    /// Replaces the caps ([`SignificanceCaps::sanitized`] first). Default caps turn the ranking
    /// off: the next update schedules exactly as without this module.
    pub fn set_caps(&mut self, caps: SignificanceCaps) {
        self.caps = caps.sanitized();
        if !self.caps.is_active() {
            self.stats.enabled = false;
        }
    }

    /// The view the ranking measures against (`None`: distance from the LOD camera, every instance
    /// in view).
    #[must_use]
    pub fn view(&self) -> Option<&SignificanceView> {
        self.view.as_ref()
    }

    /// Sets the view (the plugin copies `VfxView` here every frame while caps are active).
    pub fn set_view(&mut self, view: Option<SignificanceView>) {
        self.view = view;
    }

    /// The priority of `system` (1 unless overridden).
    #[must_use]
    pub fn system_priority(&self, system: &str) -> f32 {
        self.system_priority
            .binary_search_by(|(name, _)| name.as_str().cmp(system))
            .map_or(1.0, |i| self.system_priority[i].1)
    }

    /// Overrides the priority of every instance of `system` (`None` removes the override).
    /// Returns `false` and changes nothing for a value that is not a finite number `>= 0`.
    pub fn set_system_priority(&mut self, system: &str, priority: Option<f32>) -> bool {
        let at = self
            .system_priority
            .binary_search_by(|(name, _)| name.as_str().cmp(system));
        match (priority, at) {
            (None, Ok(i)) => {
                self.system_priority.remove(i);
            }
            (None, Err(_)) => {}
            (Some(value), at) => {
                let Some(value) = checked_priority(value) else {
                    return false;
                };
                match at {
                    Ok(i) => self.system_priority[i].1 = value,
                    Err(i) => self.system_priority.insert(i, (system.to_string(), value)),
                }
            }
        }
        true
    }

    /// The instance override of `id`, if one is set.
    #[must_use]
    pub fn instance_priority(&self, id: u64) -> Option<f32> {
        self.instance_priority
            .binary_search_by_key(&id, |&(i, _)| i)
            .ok()
            .map(|i| self.instance_priority[i].1)
    }

    /// Overrides the priority of instance `id` (`None` removes it); it wins over the system's.
    /// Returns `false` and changes nothing for a value that is not a finite number `>= 0`.
    pub fn set_instance_priority(&mut self, id: u64, priority: Option<f32>) -> bool {
        let at = self.instance_priority.binary_search_by_key(&id, |&(i, _)| i);
        match (priority, at) {
            (None, Ok(i)) => {
                self.instance_priority.remove(i);
            }
            (None, Err(_)) => {}
            (Some(value), at) => {
                let Some(value) = checked_priority(value) else {
                    return false;
                };
                match at {
                    Ok(i) => self.instance_priority[i].1 = value,
                    Err(i) => self.instance_priority.insert(i, (id, value)),
                }
            }
        }
        true
    }

    /// Every system priority override, sorted by name.
    #[must_use]
    pub fn system_priorities(&self) -> &[(String, f32)] {
        &self.system_priority
    }

    /// Every instance priority override, sorted by id.
    #[must_use]
    pub fn instance_priorities(&self) -> &[(u64, f32)] {
        &self.instance_priority
    }

    /// The counts of the last update.
    #[must_use]
    pub fn stats(&self) -> &SignificanceStats {
        &self.stats
    }

    /// The live instances as the last ranking saw them, in slot order. Empty while the caps are
    /// off. An instance that retired in that update may still be listed until the next one.
    pub fn entries(&self) -> impl Iterator<Item = SignificanceEntry> + '_ {
        let enabled = self.stats.enabled;
        self.slots
            .iter()
            .filter(move |s| enabled && s.live && !s.fresh)
            .map(|s| SignificanceEntry {
                id: s.id,
                state: (!s.finishing).then_some(s.state),
                score: s.score,
                distance: s.distance,
                visible: s.visible,
                frames_in_state: s.held,
            })
    }

    /// The state of instance `id` in the last ranking (`None`: not ranked, finishing, or the caps
    /// are off).
    #[must_use]
    pub fn state_of(&self, id: u64) -> Option<SignificanceState> {
        self.entries().find(|e| e.id == id).and_then(|e| e.state)
    }

    /// The priority instance `instance` of `system` ranks with: its override, else the system's.
    #[must_use]
    pub fn priority_of(&self, id: u64, system: &str) -> f32 {
        self.instance_priority(id)
            .unwrap_or_else(|| self.system_priority(system))
    }

    /// The score `instance` would have now (with its def's bound, the current view and priority).
    #[must_use]
    pub fn score_now(&self, instance: &VfxInstance, def: &VfxSystemDef) -> (f32, f32, bool) {
        let eye = self.view.map_or(self.eye, |v| v.eye());
        let volume = crate::bounds::culling_volume(def, instance.origin(), instance.axes(), instance.scale());
        let distance = (volume.center() - eye).length();
        let visible = self.view.is_none_or(|v| v.sees_volume(&volume, 0.0));
        let score = significance_score(&ScoreInput {
            distance,
            radius: volume.radius(),
            tan_half_fov: self.view.map_or(DEFAULT_TAN_HALF_FOV, |v| v.tan_half_fov()),
            visible,
            priority: self.priority_of(instance.id(), &def.name),
            age: instance.age(),
        });
        (score, distance, visible)
    }

    /// Ranks every live instance and gives each its state for this update (ADR-3275 (2), (3)).
    pub(crate) fn rank(
        &mut self,
        instances: &[Option<VfxInstance>],
        library: &VfxLibrary,
        lod_eye: Vec3,
        dt: f32,
    ) {
        let _span = tracing::info_span!("vfx_significance").entered();
        let caps = &self.caps;
        self.stats.enabled = true;
        self.stats.rejected_spawns = self.rejected_since;
        self.rejected_since = 0;
        self.radii.clear();
        self.radii.extend(library.defs().iter().map(system_radius));
        if self.slots.len() < instances.len() {
            self.slots.resize(instances.len(), SlotRank::EMPTY);
        }
        self.eye = self.view.map_or(lod_eye, |v| v.eye());
        let tan_half_fov = self.view.map_or(DEFAULT_TAN_HALF_FOV, |v| v.tan_half_fov());
        let mut transitions = 0u32;
        self.order.clear();
        for (slot, entry) in instances.iter().enumerate() {
            let rank = &mut self.slots[slot];
            let Some(instance) = entry else {
                rank.live = false;
                rank.ranked = false;
                continue;
            };
            if !rank.live || rank.id != instance.id() {
                *rank = SlotRank::fresh(instance.id());
            }
            rank.ranked = false;
            rank.forced = false;
            let def = library.def(instance.def_index());
            let origin = instance.origin();
            // Authored fixed bounds replace the computed sphere (`.vfx.ron` v8, P-225).
            let volume = match &def.system.bounds {
                Some(b) if b.is_valid() => CullingVolume::Fixed(crate::bounds::fixed_world_aabb(
                    b,
                    origin,
                    instance.axes(),
                    instance.scale(),
                )),
                _ => CullingVolume::Computed {
                    center: origin,
                    radius: self
                        .radii
                        .get(instance.def_index() as usize)
                        .copied()
                        .unwrap_or(MIN_RADIUS)
                        * instance.scale(),
                },
            };
            let radius = volume.radius();
            rank.distance = (volume.center() - self.eye).length();
            rank.visible = match &self.view {
                None => true,
                Some(view) => {
                    let margin = if rank.fresh || !rank.visible {
                        0.0
                    } else {
                        caps.offscreen_margin
                    };
                    view.sees_volume(&volume, margin)
                }
            };
            let priority = match self
                .instance_priority
                .binary_search_by_key(&instance.id(), |&(i, _)| i)
            {
                Ok(i) => self.instance_priority[i].1,
                Err(_) => match self
                    .system_priority
                    .binary_search_by(|(name, _)| name.as_str().cmp(&def.name))
                {
                    Ok(i) => self.system_priority[i].1,
                    Err(_) => 1.0,
                },
            };
            rank.score = significance_score(&ScoreInput {
                distance: rank.distance,
                radius,
                tan_half_fov,
                visible: rank.visible,
                priority,
                age: instance.age(),
            });
            rank.finishing = !instance.is_active();
            if rank.finishing {
                continue;
            }
            if caps.pause_offscreen && !rank.visible {
                transitions += u32::from(settle(rank, SignificanceState::Paused, caps.warmup_s));
                rank.paused_s += dt.max(0.0);
                continue;
            }
            self.order.push(slot as u32);
        }

        // Most significant first; an incumbent ranks with its bonus. Ids break ties, so the
        // order is total and the same on every run.
        let bonus = 1.0 + caps.hysteresis;
        let slots = &self.slots;
        let effective = |s: &SlotRank| if s.incumbent() { s.score * bonus } else { s.score };
        self.order.sort_unstable_by(|&a, &b| {
            let (a, b) = (&slots[a as usize], &slots[b as usize]);
            effective(b).total_cmp(&effective(a)).then(a.id.cmp(&b.id))
        });

        let dwell = caps.min_dwell_frames;
        let locked = |s: &SlotRank| !s.fresh && s.state != SignificanceState::Paused && s.held < dwell;
        let max_kept = caps.max_instances.unwrap_or(u32::MAX);
        let max_full = caps.full_instances.unwrap_or(u32::MAX).min(max_kept);
        let (mut locked_full, mut locked_reduced) = (0u32, 0u32);
        for &slot in &self.order {
            let s = &self.slots[slot as usize];
            if locked(s) {
                match s.state {
                    SignificanceState::Full => locked_full += 1,
                    SignificanceState::Reduced => locked_reduced += 1,
                    _ => {}
                }
            }
        }
        // A cap lowered under the held instances: release the least significant of them.
        for &slot in self.order.iter().rev() {
            let s = &mut self.slots[slot as usize];
            if !locked(s) {
                continue;
            }
            let over_kept = locked_full + locked_reduced > max_kept;
            match s.state {
                SignificanceState::Full if locked_full > max_full || over_kept => {
                    s.forced = true;
                    locked_full -= 1;
                }
                SignificanceState::Reduced if over_kept => {
                    s.forced = true;
                    locked_reduced -= 1;
                }
                _ => {}
            }
        }
        let mut free_full = max_full.saturating_sub(locked_full);
        let mut free_kept = max_kept.saturating_sub(locked_full + locked_reduced);
        for at in 0..self.order.len() {
            let slot = self.order[at] as usize;
            // A newcomer that outranks (bonus included) an instance its dwell holds takes that
            // instance's slot (ADR-7431): the dwell keeps a ranked instance from flickering, it
            // does not make a better newcomer wait. The least significant held instance below it
            // gives way.
            if self.slots[slot].fresh {
                if free_full == 0 {
                    let below = &self.order[at + 1..];
                    if let Some(victim) = held_below(&self.slots, below, &locked, |state| {
                        state == SignificanceState::Full
                    }) {
                        self.slots[victim].forced = true;
                        free_full += 1;
                        free_kept += 1;
                    }
                }
                if free_kept == 0 {
                    let below = &self.order[at + 1..];
                    if let Some(victim) = held_below(&self.slots, below, &locked, SignificanceState::spawns) {
                        self.slots[victim].forced = true;
                        if self.slots[victim].state == SignificanceState::Full {
                            free_full += 1;
                        }
                        free_kept += 1;
                    }
                }
            }
            let s = &mut self.slots[slot];
            s.ranked = true;
            let next = if locked(s) && !s.forced {
                s.state
            } else if free_full > 0 && free_kept > 0 {
                free_full -= 1;
                free_kept -= 1;
                SignificanceState::Full
            } else if free_kept > 0 {
                free_kept -= 1;
                SignificanceState::Reduced
            } else {
                SignificanceState::Culled
            };
            transitions += u32::from(settle(s, next, caps.warmup_s));
        }

        let stats = &mut self.stats;
        stats.full = 0;
        stats.reduced = 0;
        stats.paused = 0;
        stats.culled = 0;
        stats.finishing = 0;
        for s in self.slots.iter().take(instances.len()).filter(|s| s.live) {
            if s.finishing {
                stats.finishing += 1;
                continue;
            }
            match s.state {
                SignificanceState::Full => stats.full += 1,
                SignificanceState::Reduced => stats.reduced += 1,
                SignificanceState::Paused => stats.paused += 1,
                SignificanceState::Culled => stats.culled += 1,
            }
        }
        stats.transitions = transitions;
        stats.transitions_total += u64::from(transitions);
        stats.rejected_total += u64::from(stats.rejected_spawns);
        stats.warmup_particles = 0;
        stats.spawned = 0;
        stats.spawn_cap_reached = false;
    }

    /// Ranked slots this frame, most significant first.
    pub(crate) fn order(&self) -> &[u32] {
        &self.order
    }

    /// Whether `slot` is in [`Self::order`] this frame.
    pub(crate) fn is_ranked(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.live && s.ranked)
    }

    /// How `slot` spawns this frame; takes its pending warm-up.
    pub(crate) fn plan(&mut self, slot: usize) -> SpawnPlan {
        let reduced_rate = self.caps.reduced_rate;
        let Some(s) = self.slots.get_mut(slot).filter(|s| s.live && !s.fresh) else {
            return SpawnPlan::Normal;
        };
        if s.finishing {
            return SpawnPlan::Normal;
        }
        let warm = std::mem::take(&mut s.warm);
        match s.state {
            SignificanceState::Full if warm <= 0.0 => SpawnPlan::Normal,
            SignificanceState::Full => SpawnPlan::Scaled { rate: 1.0, warm },
            SignificanceState::Reduced => SpawnPlan::Scaled {
                rate: reduced_rate,
                warm,
            },
            SignificanceState::Paused | SignificanceState::Culled => SpawnPlan::Hold,
        }
    }

    /// Whether `slot` holds instance `id` in a non-spawning state with nothing left to draw
    /// (its GPU emitter run can go back, ADR-3275 (7)).
    pub(crate) fn idle(&self, slot: usize, id: u64) -> bool {
        self.caps.is_active()
            && self
                .slots
                .get(slot)
                .is_some_and(|s| s.live && s.id == id && !s.fresh && !s.finishing && !s.state.spawns())
    }

    /// Counts what one update spawned.
    pub(crate) fn note_spawned(&mut self, spawned: u32, warm: bool, cap_reached: bool) {
        self.stats.spawned = self.stats.spawned.saturating_add(spawned);
        if warm {
            self.stats.warmup_particles = self.stats.warmup_particles.saturating_add(spawned);
        }
        self.stats.spawn_cap_reached |= cap_reached;
    }

    /// Counts a spawn refused by a per-system cap.
    pub(crate) fn note_rejected(&mut self) {
        self.rejected_since = self.rejected_since.saturating_add(1);
    }

    /// Counts an instance stopped to make room.
    pub(crate) fn note_stolen(&mut self) {
        self.stats.stolen_total += 1;
    }
}

/// The least significant slot of `below` (a tail of the rank order) that its dwell still holds
/// in a state `wanted` accepts, and that no cap or newcomer has released yet.
fn held_below(
    slots: &[SlotRank],
    below: &[u32],
    locked: &impl Fn(&SlotRank) -> bool,
    wanted: impl Fn(SignificanceState) -> bool,
) -> Option<usize> {
    below.iter().rev().map(|&slot| slot as usize).find(|&slot| {
        let s = &slots[slot];
        locked(s) && !s.forced && wanted(s.state)
    })
}

/// Moves `rank` to `next`; returns whether that is a counted transition (a new instance's first
/// state is not). Leaving `Paused` books the warm-up.
fn settle(rank: &mut SlotRank, next: SignificanceState, warmup_s: f32) -> bool {
    if rank.fresh {
        rank.fresh = false;
        rank.state = next;
        rank.held = 0;
        return false;
    }
    if rank.state == next {
        rank.held = rank.held.saturating_add(1);
        return false;
    }
    if rank.state == SignificanceState::Paused {
        rank.warm = rank.paused_s.min(warmup_s);
        rank.paused_s = 0.0;
    }
    rank.state = next;
    rank.held = 0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(distance: f32) -> ScoreInput {
        ScoreInput {
            distance,
            radius: 1.0,
            tan_half_fov: DEFAULT_TAN_HALF_FOV,
            visible: true,
            priority: 1.0,
            age: 5.0,
        }
    }

    /// Nearer, bigger, visible, higher-priority and younger each score higher; bad input is
    /// finite and never negative.
    #[test]
    fn the_score_orders_by_each_input() {
        let base = significance_score(&input(10.0));
        assert!(significance_score(&input(5.0)) > base);
        assert!(
            significance_score(&ScoreInput {
                radius: 2.0,
                ..input(10.0)
            }) > base
        );
        assert!(
            significance_score(&ScoreInput {
                visible: false,
                ..input(10.0)
            }) < base
        );
        assert!(
            significance_score(&ScoreInput {
                priority: 4.0,
                ..input(10.0)
            }) > base
        );
        assert!(
            significance_score(&ScoreInput {
                age: 0.0,
                ..input(10.0)
            }) > base
        );
        assert_eq!(
            significance_score(&ScoreInput {
                priority: 0.0,
                ..input(1.0)
            }),
            0.0
        );
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            for i in [
                ScoreInput {
                    distance: bad,
                    ..input(1.0)
                },
                ScoreInput {
                    priority: bad,
                    ..input(1.0)
                },
                ScoreInput {
                    age: bad,
                    ..input(1.0)
                },
                ScoreInput {
                    tan_half_fov: bad,
                    ..input(1.0)
                },
            ] {
                let s = significance_score(&i);
                assert!(s.is_finite() && s >= 0.0, "{i:?} -> {s}");
            }
        }
        // Projected size: a 1 m bound at 10 m under a 60° camera covers about 17 % of half the
        // screen height.
        assert!((base - 1.0 / (10.0 * DEFAULT_TAN_HALF_FOV)).abs() < 1e-6);
    }

    #[test]
    fn default_caps_are_off_and_every_cap_turns_them_on() {
        assert!(!SignificanceCaps::default().is_active());
        let on = [
            SignificanceCaps {
                max_instances: Some(1),
                ..Default::default()
            },
            SignificanceCaps {
                full_instances: Some(1),
                ..Default::default()
            },
            SignificanceCaps {
                max_spawns_per_frame: Some(1),
                ..Default::default()
            },
            SignificanceCaps {
                max_live: Some(1),
                ..Default::default()
            },
            SignificanceCaps {
                pause_offscreen: true,
                ..Default::default()
            },
        ];
        assert!(on.iter().all(SignificanceCaps::is_active));
        let mut caps = SignificanceCaps::default();
        caps.set_system_cap(SystemCap {
            system: "smoke".into(),
            max_instances: 2,
            steal: StealPolicy::Oldest,
        });
        assert!(caps.is_active());
        assert!(caps.remove_system_cap("smoke"));
        assert!(!caps.is_active());
    }

    #[test]
    fn sanitized_caps_clamp_and_dedupe() {
        let caps = SignificanceCaps {
            reduced_rate: 3.0,
            warmup_s: f32::NAN,
            hysteresis: -1.0,
            per_system: vec![
                SystemCap {
                    system: "b".into(),
                    max_instances: 1,
                    steal: StealPolicy::Reject,
                },
                SystemCap {
                    system: "a".into(),
                    max_instances: 1,
                    steal: StealPolicy::Reject,
                },
                SystemCap {
                    system: "b".into(),
                    max_instances: 7,
                    steal: StealPolicy::Oldest,
                },
            ],
            ..Default::default()
        }
        .sanitized();
        assert_eq!(caps.reduced_rate, 1.0);
        assert_eq!(caps.warmup_s, 0.0);
        assert_eq!(caps.hysteresis, 0.0);
        let names: Vec<_> = caps.per_system.iter().map(|c| c.system.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(
            caps.system_cap("b").map(|c| c.max_instances),
            Some(7),
            "the last entry wins"
        );
    }

    #[test]
    fn policies_and_states_round_trip_their_names() {
        for p in StealPolicy::ALL {
            assert_eq!(StealPolicy::from_name(p.name()), Some(p));
        }
        assert_eq!(StealPolicy::from_name("newest"), None);
        let names: Vec<_> = SignificanceState::ALL.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["full", "reduced", "paused", "culled"]);
    }

    #[test]
    fn priority_overrides_set_replace_and_clear() {
        let mut sig = VfxSignificance::default();
        assert_eq!(sig.system_priority("smoke"), 1.0);
        assert!(sig.set_system_priority("smoke", Some(5.0)));
        assert!(sig.set_system_priority("smoke", Some(7.0)));
        assert_eq!(sig.system_priority("smoke"), 7.0);
        assert!(!sig.set_system_priority("smoke", Some(f32::NAN)));
        assert!(!sig.set_system_priority("smoke", Some(-1.0)));
        assert!(sig.set_instance_priority(4, Some(2e9)));
        assert_eq!(sig.instance_priority(4), Some(MAX_PRIORITY));
        assert_eq!(sig.priority_of(4, "smoke"), MAX_PRIORITY);
        assert_eq!(sig.priority_of(5, "smoke"), 7.0);
        assert!(sig.set_system_priority("smoke", None));
        assert!(sig.set_instance_priority(4, None));
        assert!(sig.system_priorities().is_empty() && sig.instance_priorities().is_empty());
    }

    #[test]
    fn a_view_sees_what_is_in_front_and_not_behind() {
        let camera = VfxCamera::looking_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -10.0));
        let view = SignificanceView::from_camera(&camera, 1280, 720);
        assert!(view.sees(Vec3::new(0.0, 0.0, -20.0), 1.0));
        assert!(!view.sees(Vec3::new(0.0, 0.0, 20.0), 1.0));
        assert!((view.tan_half_fov() - DEFAULT_TAN_HALF_FOV).abs() < 1e-5);
    }

    #[test]
    fn the_system_radius_bounds_shape_travel_and_size() {
        let def = crate::showcase::sparks();
        let r = system_radius(&def);
        assert!(r.is_finite() && r >= MIN_RADIUS);
        let empty = VfxSystemDef {
            emitters: Vec::new(),
            ..def
        };
        assert_eq!(system_radius(&empty), MIN_RADIUS);
    }

    /// The v7 mesh shapes and the along-normal velocity (ADR-2800) are bounded like the others: the
    /// mesh gets [`MESH_SHAPE_RADIUS`], the normal speed travels like a radial one.
    #[test]
    fn mesh_shapes_and_normal_velocity_are_bounded() {
        let mut def = crate::showcase::sparks();
        def.emitters.truncate(1);
        let base = {
            let mut point = def.clone();
            point.emitters[0].init.shape = ShapeDef::Point;
            system_radius(&point)
        };
        assert!(base > MIN_RADIUS, "the sparks emitter travels: {base}");
        for shape in [
            ShapeDef::MeshSurface { mesh: "hero".into() },
            ShapeDef::SkinnedMesh {
                mesh: "hero".into(),
                max_per_frame: 64,
            },
        ] {
            let mut mesh = def.clone();
            mesh.emitters[0].init.shape = shape;
            let r = system_radius(&mesh);
            assert!((r - (base + MESH_SHAPE_RADIUS)).abs() < 1e-4, "{r} vs {base}");
        }
        let speed = match &def.emitters[0].init.velocity.mode {
            VelocityMode::Directional { speed, .. } => *speed,
            other => panic!("the sparks emitter launches directionally: {other:?}"),
        };
        let mut radial = def.clone();
        radial.emitters[0].init.velocity.mode = VelocityMode::Radial { speed };
        let mut normal = def;
        normal.emitters[0].init.velocity.mode = VelocityMode::Normal { speed };
        assert_eq!(system_radius(&normal), system_radius(&radial));
    }
}
