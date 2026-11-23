// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/present.rs, commit c9513623f), ADR-8012.

//! The sim bridge's presentation settings and per-kind blend policy
//! (`docs/specs/25-sim-bridge-and-do27.md` § 3, ADR-8456).
//!
//! A sim that publishes its last two frames as one pair (a
//! [`ThreadedSimHost`](crate::sim_host::ThreadedSimHost), or any
//! [`ExternalSim`](crate::sim_bridge::ExternalSim) that implements
//! [`read_pair`](crate::sim_bridge::ExternalSim::read_pair)) is drawn between them: the bridge reads
//! the pair once per frame, with the blend weight [`PublishedFrames::alpha`] gives for a frame
//! drawn [`SimBridgeSettings::presentation_delay`] behind the newest publish, and poses every
//! entity by its kind's [`BlendPolicy`]:
//!
//! - [`BlendPolicy::Lerp`] (the default): translation lerps and rotation slerps from `prev` to
//!   `curr`. When one tick's jump is longer than [`SimBridgeSettings::teleport_distance`] the
//!   entity snaps to `curr` instead (a respawn or a teleport never streaks across the map).
//! - [`BlendPolicy::Snap`]: always `curr`, the newest pose, without the presentation delay.
//! - [`BlendPolicy::Hold`]: `prev` until the blend reaches `curr`: no in-between poses, but the
//!   same delay as the lerped kinds, so a stepped prop stays in time with its lerped neighbours.
//!
//! An entity new in `curr` (no `prev` pose) is drawn at `curr`. A still pair (a held host, the
//! boot pair) gives a weight of 1, so every policy draws `curr`. A sim that publishes no pair is
//! drawn as [`read_snapshot`](crate::sim_bridge::ExternalSim::read_snapshot) returns it, exactly
//! as before.
//!
//! [`SimBridgeSettings::handedness`] converts the sim's frame once at the boundary, before the
//! blend (`crate::sim_coords`).
//!
//! The blend is adapted from Direct Order 27's `shell-regia/game/src/present.rs` (`blend_frames`,
//! `lerp`, `lerp_angle`, SHA c9513623f, regia-absorb-4): its single policy became the per-kind
//! [`BlendPolicy`], and the teleport snap is new (named issue: a lerp across a respawn drew the
//! entity sliding through the world for one tick).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};

use crate::sim_bridge::SnapshotTick;
use crate::sim_coords::SimHandedness;
#[cfg(doc)]
use crate::sim_host::PublishedFrames;

/// A blend weight at or above this draws `curr` exactly (bit for bit): DO27's threshold.
pub const BLEND_CURR_AT: f32 = 0.999;

/// A blend weight at or below this draws `prev` exactly: DO27's threshold.
pub const BLEND_PREV_AT: f32 = 0.001;

/// The default presentation delay: one 60 Hz tick (DO27 draws one tick behind the newest publish).
pub const DEFAULT_PRESENTATION_DELAY: Duration = crate::sim_host::DEFAULT_TICK;

/// How the bridge poses one kind between the two published frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum BlendPolicy {
    /// Lerp the translation and slerp the rotation from `prev` to `curr` (snaps past the
    /// teleport distance).
    #[default]
    Lerp,
    /// Always the newest pose (`curr`).
    Snap,
    /// `prev` until the blend reaches `curr`: a step, with the lerped kinds' delay.
    Hold,
}

impl BlendPolicy {
    /// Every policy, in protocol order.
    pub const ALL: [Self; 3] = [Self::Lerp, Self::Snap, Self::Hold];

    /// The protocol name (`lerp`, `snap`, `hold`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lerp => "lerp",
            Self::Snap => "snap",
            Self::Hold => "hold",
        }
    }

    /// Parses a protocol name ([`as_str`](Self::as_str)).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

/// The bridge's presentation settings. Insert as a resource (before or after the plugin), or
/// build the plugin with [`SimBridgePlugin::settings`](crate::sim_bridge::SimBridgePlugin::settings).
/// [`Default`] changes nothing for a right-handed sim that publishes no pair.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct SimBridgeSettings {
    /// The sim's handedness; applied once to every snapshot pose and placed event.
    pub handedness: SimHandedness,
    /// A [`BlendPolicy::Lerp`] entity whose position moves more than this (metres) between the two
    /// published frames snaps to `curr`. `None` (the default, DO27's behavior) never snaps.
    pub teleport_distance: Option<f32>,
    /// How far behind the newest publish the presentation draws (one tick by default).
    pub presentation_delay: Duration,
}

impl Default for SimBridgeSettings {
    fn default() -> Self {
        Self {
            handedness: SimHandedness::RightHanded,
            teleport_distance: None,
            presentation_delay: DEFAULT_PRESENTATION_DELAY,
        }
    }
}

/// What a pair read returned ([`ExternalSim::read_pair`](crate::sim_bridge::ExternalSim::read_pair)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapshotPair {
    /// The sim tick of `prev`.
    pub prev_tick: SnapshotTick,
    /// The sim tick of `curr`.
    pub curr_tick: SnapshotTick,
    /// The blend weight from `prev` (0) to `curr` (1), clamped to `0..=1` by the bridge.
    pub alpha: f32,
}

/// One entity pose: translation and rotation, in engine coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimPose {
    /// World-space position (metres).
    pub translation: Vec3,
    /// World-space rotation.
    pub rotation: Quat,
}

/// Which pose a blend drew ([`blend_pose`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendOutcome {
    /// `curr` because the entity has no `prev` pose, or the weight reached `curr`.
    Curr,
    /// `prev`: the weight is at `prev`, or a [`BlendPolicy::Hold`] has not reached `curr`.
    Prev,
    /// An in-between pose ([`BlendPolicy::Lerp`]).
    Lerped,
    /// `curr` because the policy is [`BlendPolicy::Snap`].
    Snapped,
    /// `curr` because one tick's jump passed the teleport distance.
    Teleported,
}

/// Pose one entity between two published frames.
///
/// `prev` is the entity's pose in the older frame (`None` for an entity new in `curr`). `alpha` is
/// clamped to `0..=1`; at or above [`BLEND_CURR_AT`] the result is `curr` bit for bit, at or below
/// [`BLEND_PREV_AT`] it is `prev` (except for [`BlendPolicy::Snap`], always `curr`). A non-finite
/// `alpha` draws `curr`.
pub fn blend_pose(
    policy: BlendPolicy,
    prev: Option<SimPose>,
    curr: SimPose,
    alpha: f32,
    teleport_distance: Option<f32>,
) -> (SimPose, BlendOutcome) {
    if policy == BlendPolicy::Snap {
        return (curr, BlendOutcome::Snapped);
    }
    let Some(prev) = prev else {
        return (curr, BlendOutcome::Curr);
    };
    let alpha = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        1.0
    };
    if alpha >= BLEND_CURR_AT {
        return (curr, BlendOutcome::Curr);
    }
    if policy == BlendPolicy::Hold {
        return (prev, BlendOutcome::Prev);
    }
    if let Some(limit) = teleport_distance
        && prev.translation.distance(curr.translation) > limit
    {
        return (curr, BlendOutcome::Teleported);
    }
    if alpha <= BLEND_PREV_AT {
        return (prev, BlendOutcome::Prev);
    }
    (
        SimPose {
            translation: prev.translation.lerp(curr.translation, alpha),
            rotation: prev.rotation.slerp(curr.rotation, alpha),
        },
        BlendOutcome::Lerped,
    )
}

/// Linear interpolation, `a + (b − a)·t`.
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Interpolate an angle (radians) along the shorter way round.
#[inline]
pub fn lerp_angle(a: f32, b: f32, t: f32) -> f32 {
    let mut d = (b - a) % std::f32::consts::TAU;
    if d > std::f32::consts::PI {
        d -= std::f32::consts::TAU;
    }
    if d < -std::f32::consts::PI {
        d += std::f32::consts::TAU;
    }
    a + d * t
}

/// How the bridge posed entities since start (the `sim.bridge_settings` agent method).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SimBlendStats {
    /// Frames the sim answered with a published pair.
    pub pair_reads: u64,
    /// Frames the sim answered with a single snapshot (no pair: drawn as read).
    pub single_reads: u64,
    /// Entity poses drawn in between ([`BlendOutcome::Lerped`]).
    pub lerped: u64,
    /// Entity poses drawn at `curr` by [`BlendPolicy::Snap`].
    pub snapped: u64,
    /// Entity poses drawn at `prev` ([`BlendOutcome::Prev`], including holds).
    pub held: u64,
    /// Entity poses drawn at `curr` (no `prev`, or the weight reached `curr`).
    pub at_curr: u64,
    /// Lerped entities snapped past the teleport distance.
    pub teleports: u64,
}

impl SimBlendStats {
    /// Count one blend outcome.
    pub fn note(&mut self, outcome: BlendOutcome) {
        let slot = match outcome {
            BlendOutcome::Curr => &mut self.at_curr,
            BlendOutcome::Prev => &mut self.held,
            BlendOutcome::Lerped => &mut self.lerped,
            BlendOutcome::Snapped => &mut self.snapped,
            BlendOutcome::Teleported => &mut self.teleports,
        };
        *slot = slot.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(x: f32, yaw: f32) -> SimPose {
        SimPose {
            translation: Vec3::new(x, 1.0, -2.0),
            rotation: Quat::from_rotation_y(yaw),
        }
    }

    #[test]
    fn lerp_blends_between_the_two_frames() {
        let (out, how) = blend_pose(
            BlendPolicy::Lerp,
            Some(pose(0.0, 0.0)),
            pose(10.0, 1.0),
            0.25,
            None,
        );
        assert_eq!(how, BlendOutcome::Lerped);
        assert!((out.translation.x - 2.5).abs() < 1.0e-5);
        assert!((out.rotation.angle_between(Quat::from_rotation_y(0.25))).abs() < 1.0e-5);
    }

    #[test]
    fn a_teleport_snaps_instead_of_lerping() {
        let prev = pose(0.0, 0.0);
        let curr = pose(50.0, 0.5);
        // Under the threshold: lerped.
        let (_, how) = blend_pose(BlendPolicy::Lerp, Some(prev), curr, 0.5, Some(60.0));
        assert_eq!(how, BlendOutcome::Lerped);
        // Past it: exactly `curr`, at every weight.
        for alpha in [0.0, 0.3, 0.5, 0.998] {
            let (out, how) = blend_pose(BlendPolicy::Lerp, Some(prev), curr, alpha, Some(20.0));
            assert_eq!(how, BlendOutcome::Teleported, "alpha {alpha}");
            assert_eq!(out, curr);
        }
        // No threshold (the default): never snaps.
        let (out, how) = blend_pose(BlendPolicy::Lerp, Some(prev), curr, 0.5, None);
        assert_eq!(how, BlendOutcome::Lerped);
        assert!((out.translation.x - 25.0).abs() < 1.0e-4);
    }

    #[test]
    fn hold_keeps_prev() {
        let prev = pose(0.0, 0.0);
        let curr = pose(4.0, 1.2);
        for alpha in [0.0, 0.25, 0.5, 0.75, 0.998] {
            let (out, how) = blend_pose(BlendPolicy::Hold, Some(prev), curr, alpha, Some(1.0));
            assert_eq!(how, BlendOutcome::Prev, "alpha {alpha}");
            assert_eq!(out, prev);
        }
        // The step lands when the blend reaches `curr`.
        let (out, how) = blend_pose(BlendPolicy::Hold, Some(prev), curr, 1.0, None);
        assert_eq!((out, how), (curr, BlendOutcome::Curr));
        // A new entity has nothing to hold.
        let (out, how) = blend_pose(BlendPolicy::Hold, None, curr, 0.5, None);
        assert_eq!((out, how), (curr, BlendOutcome::Curr));
    }

    #[test]
    fn snap_always_draws_curr() {
        for alpha in [0.0, 0.5, 1.0] {
            let (out, how) = blend_pose(
                BlendPolicy::Snap,
                Some(pose(0.0, 0.0)),
                pose(3.0, 0.2),
                alpha,
                None,
            );
            assert_eq!((out, how), (pose(3.0, 0.2), BlendOutcome::Snapped));
        }
    }

    #[test]
    fn the_ends_of_the_blend_are_bit_exact() {
        let prev = pose(0.1, 0.3);
        let curr = pose(0.7, -0.9);
        let (out, _) = blend_pose(BlendPolicy::Lerp, Some(prev), curr, 1.0, None);
        assert_eq!(out, curr);
        let (out, _) = blend_pose(BlendPolicy::Lerp, Some(prev), curr, 0.0, None);
        assert_eq!(out, prev);
        // Out of range and non-finite weights clamp.
        assert_eq!(blend_pose(BlendPolicy::Lerp, Some(prev), curr, 7.0, None).0, curr);
        assert_eq!(
            blend_pose(BlendPolicy::Lerp, Some(prev), curr, -3.0, None).0,
            prev
        );
        assert_eq!(
            blend_pose(BlendPolicy::Lerp, Some(prev), curr, f32::NAN, None).0,
            curr
        );
    }

    #[test]
    fn lerp_angle_takes_the_short_way() {
        let pi = std::f32::consts::PI;
        assert!((lerp_angle(0.1, 0.5, 0.5) - 0.3).abs() < 1.0e-6);
        // From just under +π to just over −π: through π, not through 0.
        let mid = lerp_angle(pi - 0.1, -pi + 0.1, 0.5);
        assert!((mid.abs() - pi).abs() < 1.0e-5, "{mid}");
        assert_eq!(lerp(2.0, 6.0, 0.25), 3.0);
    }

    #[test]
    fn policy_names_round_trip() {
        for p in BlendPolicy::ALL {
            assert_eq!(BlendPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(BlendPolicy::parse("slerp"), None);
        assert_eq!(BlendPolicy::default(), BlendPolicy::Lerp);
        let settings = SimBridgeSettings::default();
        assert_eq!(settings.handedness, SimHandedness::RightHanded);
        assert_eq!(settings.teleport_distance, None);
        assert_eq!(settings.presentation_delay, DEFAULT_PRESENTATION_DELAY);
    }
}

/// The bridge end to end: settings, per-kind policies, handedness, and the default against the base.
#[cfg(test)]
mod bridge_tests {
    use std::time::Duration;

    use regia_core::math::{Quat, Vec3};
    use regia_ecs::ecs::message::MessageCursor;
    use regia_scene::Transform;

    use super::*;
    use crate::sim_bridge::{
        BridgeInput, EventPlace, EventRouter, ExternalSim, KindRegistry, RoutedEvent, SimBridgePlugin,
        SimEntityMap, SimEntityView, SimSnapshot, SnapshotTick,
    };
    use crate::sim_coords;

    #[derive(Default, Clone)]
    struct Snap {
        entries: Vec<(u64, &'static str, Vec3, Quat)>,
    }

    impl SimSnapshot for Snap {
        fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
            for &(id, kind, translation, rotation) in &self.entries {
                f(SimEntityView {
                    id,
                    kind,
                    team: 0,
                    translation,
                    rotation,
                });
            }
        }
    }

    /// A sim that answers with a fixed pair (`pair: Some`) or a single snapshot (`pair: None`).
    #[derive(Resource)]
    struct PairSim {
        prev: Snap,
        curr: Snap,
        alpha: Option<f32>,
        tick: u64,
        /// The delay the bridge asked for on the last pair read.
        asked_delay: Option<Duration>,
        place: Option<Vec3>,
        events: Vec<&'static str>,
    }

    impl PairSim {
        fn single(curr: Snap) -> Self {
            Self {
                prev: Snap::default(),
                curr,
                alpha: None,
                tick: 0,
                asked_delay: None,
                place: None,
                events: Vec::new(),
            }
        }

        fn pair(prev: Snap, curr: Snap, alpha: f32) -> Self {
            Self {
                prev,
                alpha: Some(alpha),
                ..Self::single(curr)
            }
        }
    }

    struct Ev(Option<Vec3>);

    impl ExternalSim for PairSim {
        type Snapshot = Snap;
        type Event = Ev;

        fn submit_input(&mut self, _input: &BridgeInput) {}

        fn read_snapshot(&mut self, _frame_time: Duration, out: &mut Snap) -> SnapshotTick {
            self.tick += 1;
            out.clone_from(&self.curr);
            SnapshotTick(self.tick)
        }

        fn read_pair(
            &mut self,
            _frame_time: Duration,
            delay: Duration,
            prev: &mut Snap,
            curr: &mut Snap,
        ) -> Option<SnapshotPair> {
            let alpha = self.alpha?;
            self.asked_delay = Some(delay);
            self.tick += 1;
            prev.clone_from(&self.prev);
            curr.clone_from(&self.curr);
            Some(SnapshotPair {
                prev_tick: SnapshotTick(self.tick - 1),
                curr_tick: SnapshotTick(self.tick),
                alpha,
            })
        }

        fn drain_events(&mut self, out: &mut Vec<Ev>) {
            out.extend(self.events.drain(..).map(|_| Ev(self.place)));
        }

        fn event_kind(_event: &Ev) -> String {
            "boom".into()
        }

        fn event_place(event: &Ev) -> EventPlace {
            event.0.map_or(EventPlace::Unplaced, EventPlace::At)
        }
    }

    fn kinds() -> KindRegistry {
        KindRegistry::from_ron(
            r#"(kinds: {
                "lerper": (prefab: None, presentation_scripts: []),
                "snapper": (prefab: None, presentation_scripts: [], blend: Snap),
                "holder": (prefab: None, presentation_scripts: [], blend: Hold),
                "jumper": (prefab: None, presentation_scripts: [], blend: Lerp),
            })"#,
        )
        .expect("valid kinds")
    }

    fn app_with(sim: PairSim, settings: Option<SimBridgeSettings>) -> App {
        let mut plugin = SimBridgePlugin::new(sim).kinds(kinds());
        if let Some(settings) = settings {
            plugin = plugin.settings(settings);
        }
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, plugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    fn transform_of(app: &mut App, id: u64) -> Transform {
        let entity = app
            .world()
            .resource::<SimEntityMap>()
            .entity(id)
            .unwrap_or_else(|| panic!("sim id {id} has no entity"));
        *app.world().get::<Transform>(entity).expect("transform")
    }

    fn bits(t: Vec3, r: Quat) -> ([u32; 3], [u32; 4]) {
        (t.to_array().map(f32::to_bits), r.to_array().map(f32::to_bits))
    }

    /// Awkward values: a negative zero, a subnormal, a large coordinate and a non-unit quaternion,
    /// so any arithmetic on the default path shows up in the bits.
    fn awkward() -> Snap {
        Snap {
            entries: vec![
                (
                    1,
                    "lerper",
                    Vec3::new(-0.0, f32::from_bits(7), 3.0e7),
                    Quat::from_xyzw(0.1, -0.0, 0.3, 0.9),
                ),
                (
                    2,
                    "unknown",
                    Vec3::new(0.1, 0.2, 0.3),
                    Quat::from_xyzw(0.0, 0.6, 0.0, 0.8),
                ),
                (3, "holder", Vec3::new(-5.5, 2.25, -1.0e-3), Quat::IDENTITY),
            ],
        }
    }

    #[test]
    fn right_handed_default_changes_nothing() {
        // A single-snapshot sim (every sim before ADR-8456), without and with explicit default
        // settings: every transform is the snapshot's pose, bit for bit (what the base wrote).
        for settings in [None, Some(SimBridgeSettings::default())] {
            let mut app = app_with(PairSim::single(awkward()), settings);
            for _ in 0..3 {
                app.update();
            }
            for (id, _, t, r) in awkward().entries {
                let got = transform_of(&mut app, id);
                assert_eq!(bits(got.translation, got.rotation), bits(t, r), "sim id {id}");
                assert_eq!(got.scale, Vec3::ONE);
            }
            let stats = *app.world().resource::<SimBlendStats>();
            assert_eq!(stats.single_reads, 3);
            assert_eq!(stats.pair_reads, 0);
            assert_eq!(stats.lerped + stats.held + stats.snapped + stats.teleports, 0);
        }
        // A still pair (weight 1) also draws `curr` bit for bit under every policy.
        let mut app = app_with(PairSim::pair(Snap::default(), awkward(), 1.0), None);
        app.update();
        for (id, _, t, r) in awkward().entries {
            let got = transform_of(&mut app, id);
            assert_eq!(bits(got.translation, got.rotation), bits(t, r), "sim id {id}");
        }
    }

    #[test]
    fn the_bridge_poses_each_kind_by_its_policy() {
        let q0 = Quat::from_rotation_y(0.0);
        let q1 = Quat::from_rotation_y(1.0);
        let prev = Snap {
            entries: vec![
                (1, "lerper", Vec3::new(0.0, 0.0, 0.0), q0),
                (2, "snapper", Vec3::new(0.0, 0.0, 0.0), q0),
                (3, "holder", Vec3::new(0.0, 0.0, 0.0), q0),
                (4, "jumper", Vec3::new(0.0, 0.0, 0.0), q0),
                (5, "unknown", Vec3::new(0.0, 0.0, 0.0), q0),
            ],
        };
        let mut curr = Snap {
            entries: vec![
                (1, "lerper", Vec3::new(4.0, 0.0, 0.0), q1),
                (2, "snapper", Vec3::new(4.0, 0.0, 0.0), q1),
                (3, "holder", Vec3::new(4.0, 0.0, 0.0), q1),
                (4, "jumper", Vec3::new(400.0, 0.0, 0.0), q1),
                (5, "unknown", Vec3::new(4.0, 0.0, 0.0), q1),
            ],
        };
        // New in `curr`: drawn at `curr`.
        curr.entries.push((6, "lerper", Vec3::new(9.0, 9.0, 9.0), q1));
        let settings = SimBridgeSettings {
            teleport_distance: Some(10.0),
            presentation_delay: Duration::from_millis(25),
            ..SimBridgeSettings::default()
        };
        let mut app = app_with(PairSim::pair(prev, curr, 0.25), Some(settings));
        app.update();

        assert_eq!(
            app.world().resource::<PairSim>().asked_delay,
            Some(Duration::from_millis(25)),
            "the presentation delay setting reaches the pair read"
        );
        let lerped = transform_of(&mut app, 1);
        assert!(
            (lerped.translation.x - 1.0).abs() < 1.0e-5,
            "{:?}",
            lerped.translation
        );
        assert!(lerped.rotation.angle_between(Quat::from_rotation_y(0.25)) < 1.0e-5);
        assert_eq!(transform_of(&mut app, 2).translation, Vec3::new(4.0, 0.0, 0.0));
        assert_eq!(transform_of(&mut app, 3).translation, Vec3::ZERO);
        assert_eq!(transform_of(&mut app, 3).rotation, q0);
        assert_eq!(transform_of(&mut app, 4).translation, Vec3::new(400.0, 0.0, 0.0));
        // A kind the registry does not know lerps (the default policy).
        assert!((transform_of(&mut app, 5).translation.x - 1.0).abs() < 1.0e-5);
        assert_eq!(transform_of(&mut app, 6).translation, Vec3::new(9.0, 9.0, 9.0));

        let stats = *app.world().resource::<SimBlendStats>();
        assert_eq!(stats.pair_reads, 1);
        assert_eq!(stats.single_reads, 0);
        assert_eq!(stats.lerped, 2);
        assert_eq!(stats.snapped, 1);
        assert_eq!(stats.held, 1);
        assert_eq!(stats.teleports, 1);
        assert_eq!(stats.at_curr, 1);
    }

    #[test]
    fn left_handed_reflects_poses_and_event_places_once() {
        let yaw = Quat::from_rotation_y(0.4);
        let prev = Snap {
            entries: vec![(1, "lerper", Vec3::new(2.0, 1.0, 5.0), Quat::IDENTITY)],
        };
        let curr = Snap {
            entries: vec![(1, "lerper", Vec3::new(6.0, 1.0, 5.0), yaw)],
        };
        let settings = SimBridgeSettings {
            handedness: sim_coords::SimHandedness::LeftHandedReflectX,
            ..SimBridgeSettings::default()
        };
        let mut sim = PairSim::pair(prev, curr, 0.5);
        sim.place = Some(Vec3::new(7.0, 0.0, -3.0));
        sim.events.push("boom");
        let router = EventRouter::from_ron(r#"(routes: { "boom": [ Toast(text: "x") ] })"#).expect("routes");
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            SimBridgePlugin::new(sim)
                .kinds(kinds())
                .events(router)
                .settings(settings),
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.update();

        // Halfway between the reflected frames: x = −4, and the yaw is negated (half of −0.4).
        let got = transform_of(&mut app, 1);
        assert!(
            (got.translation - Vec3::new(-4.0, 1.0, 5.0)).length() < 1.0e-5,
            "{:?}",
            got.translation
        );
        assert!(got.rotation.angle_between(Quat::from_rotation_y(-0.2)) < 1.0e-5);

        let mut cursor = MessageCursor::default();
        let routed: Vec<RoutedEvent> = cursor
            .read(app.world().resource::<Messages<RoutedEvent>>())
            .cloned()
            .collect();
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].position, Some(Vec3::new(-7.0, 0.0, -3.0)));
    }

    #[test]
    fn a_kinds_file_without_blend_keeps_parsing() {
        let old = KindRegistry::from_ron(r#"(kinds: { "disc": (prefab: None, presentation_scripts: []) })"#)
            .expect("pre-ADR-8456 kinds file");
        assert_eq!(old.entry("disc").map(|e| e.blend), Some(BlendPolicy::Lerp));
        assert_eq!(old.blend_of("disc"), BlendPolicy::Lerp);
        assert_eq!(kinds().blend_of("holder"), BlendPolicy::Hold);
        assert_eq!(kinds().blend_of("nobody"), BlendPolicy::Lerp);
    }
}
