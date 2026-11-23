// SPDX-License-Identifier: MIT-0
//! The montage-in-a-graph case (spec 34 M9, ADR-1870; ADR-2131 adds it to the lab).
//!
//! An upper-body animation graph (`assets/upper_body.animgraph.ron`, format v3) holds a montage
//! `Slot` node under a `Grip` layer. A two-section wave montage (`assets/wave.montage.ron`) plays
//! on the slot: the arm waves while the layer above the slot keeps the hand on its grip, which the
//! post-graph montage stage cannot do. The same montage then plays backward and time-stretched,
//! and a child montage (`assets/wave_short.montagevar.ron`) swaps its clip and keeps its timing.
//!
//! Needs no downloaded content: the rig and clips are built here, the graph and montages are the
//! sample's text assets, read through their format loaders. Fixed 1/32 s frames, no wall clock.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::graph::{AnimGraph, AnimNode, ClipNode, JointMaskDef, LayerEntry, LayerMode, LayerNode};
use regia_animation::montage_variant::{MontageLibrary, MontageVariant};
use regia_animation::pose::translated;
use regia_animation::{
    AnimationClip, AnimationPlugin, Animator, ClipLibrary, Montage, MontageId, MontagePlayer, PlayOptions,
    Skeleton, SlotDef,
};
use regia_app::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_scene::Transform;

/// The sample graph: `Layer(base: Slot("upper", Idle), layers: [Grip on the hand])`.
pub(crate) const GRAPH_RON: &str = include_str!("../assets/upper_body.animgraph.ron");
/// The parent montage: `Wave` on the `upper` slot, sections `raise` and `lower`.
pub(crate) const MONTAGE_RON: &str = include_str!("../assets/wave.montage.ron");
/// The child montage: `WaveShort` in place of `Wave`, rescaled to the parent's timing.
pub(crate) const VARIANT_RON: &str = include_str!("../assets/wave_short.montagevar.ron");

/// Fixed frame time.
const DT: f32 = 1.0 / 32.0;
/// Joint indices of the lab rig.
const ARM: u32 = 2;
const HAND: u32 = 3;
/// The arm's rest height (its bind translation) and the hand's grip height.
const ARM_REST: f32 = 0.5;
const GRIP: f32 = 0.25;
/// Frames before giving up on a montage that never ends.
const MAX_FRAMES: u32 = 200;

/// What the case measured.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SlotGraphReport {
    /// Highest arm height while the wave played forward through the Slot node.
    pub forward_peak: f32,
    /// Hand height at the wave's peak with the Slot node (the Grip layer above it wins).
    pub hand_with_slot: f32,
    /// Hand height at the wave's peak with the montage applied after the graph (no Slot node).
    pub hand_without_slot: f32,
    /// Arm height after the wave ended (back to the graph's own pose).
    pub arm_after: f32,
    /// Montage events of the forward play, in order.
    pub forward_events: Vec<String>,
    /// Frames the reverse, time-stretched play took (asked for 0.5 s at 1/32 s frames).
    pub reverse_frames: u32,
    /// Highest arm height during the reverse play.
    pub reverse_peak: f32,
    /// Montage events of the reverse play, in order.
    pub reverse_events: Vec<String>,
    /// The child montage's length and its parent's (rescale keeps the timing).
    pub child_length: f32,
    /// The parent's length.
    pub parent_length: f32,
    /// Highest arm height while the child montage played.
    pub child_peak: f32,
}

impl std::fmt::Display for SlotGraphReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "slot_graph: peak={:.3} hand_slot={:.3} hand_post={:.3} after={:.3} events=[{}] reverse_frames={} reverse_peak={:.3} reverse_events=[{}] len={:.3} child_len={:.3} child_peak={:.3}",
            self.forward_peak,
            self.hand_with_slot,
            self.hand_without_slot,
            self.arm_after,
            self.forward_events.join(" "),
            self.reverse_frames,
            self.reverse_peak,
            self.reverse_events.join(" "),
            self.parent_length,
            self.child_length,
            self.child_peak,
        )
    }
}

/// Run the case. Errors name what did not behave.
pub(crate) fn run() -> Result<SlotGraphReport, String> {
    let graph = AnimGraph::from_ron_str(GRAPH_RON).map_err(|e| format!("upper_body.animgraph.ron: {e}"))?;
    let slots = slot_defs();
    if !graph.has_slot_nodes() {
        return Err("the sample graph has no Slot node".into());
    }
    let issues = graph.check_slot_defs(&slots);
    if !issues.is_empty() {
        return Err(format!("the sample graph names slots the rig lacks: {issues:?}"));
    }
    let clips = clips();
    let span = |name: &str| clips.get(name).map(|c| c.duration);
    let mut library = MontageLibrary::new();
    let wave = Montage::from_ron_str(MONTAGE_RON).map_err(|e| format!("wave.montage.ron: {e}"))?;
    wave.validate().map_err(|e| format!("wave.montage.ron: {e}"))?;
    let errors = library.add_montage(wave, &span);
    if !errors.is_empty() {
        return Err(format!("wave: {errors:?}"));
    }
    let variant =
        MontageVariant::from_ron_str(VARIANT_RON).map_err(|e| format!("wave_short.montagevar.ron: {e}"))?;
    let child = library
        .add_variant(variant, &span)
        .map_err(|e| format!("wave_short.montagevar.ron: {e}"))?;
    let wave = library.get("wave").cloned().ok_or("the library lost wave")?;

    // Forward, through the Slot node, and the same play with the montage after the graph.
    let mut slotted = Lab::new(graph.clone(), &clips)?;
    let mut post = Lab::new(no_slot_graph(), &clips)?;
    let id = slotted.play(wave.clone(), PlayOptions::default())?;
    let post_id = post.play(wave.clone(), PlayOptions::default())?;
    let mut forward_peak = f32::MIN;
    let mut hand_with_slot = f32::NAN;
    let mut hand_without_slot = f32::NAN;
    let mut forward_events = Vec::new();
    for _ in 0..MAX_FRAMES {
        slotted.app.update();
        post.app.update();
        let arm = slotted.joint_y(ARM)?;
        if arm > forward_peak {
            forward_peak = arm;
            hand_with_slot = slotted.joint_y(HAND)?;
            hand_without_slot = post.joint_y(HAND)?;
        }
        if slotted.finished(id)? && post.finished(post_id)? {
            forward_events = slotted.events()?;
            break;
        }
    }
    if forward_events.is_empty() {
        return Err(format!("the wave did not end within {MAX_FRAMES} frames"));
    }
    slotted.app.update();
    let arm_after = slotted.joint_y(ARM)?;

    // Backward and time-stretched: the 1 s wave plays from its end to 0 in 0.5 s.
    let mut lab = Lab::new(graph.clone(), &clips)?;
    let id = lab.play(
        wave.clone(),
        PlayOptions {
            reverse: true,
            duration: Some(0.5),
            ..PlayOptions::default()
        },
    )?;
    let mut reverse_frames = 0;
    let mut reverse_peak = f32::MIN;
    let mut reverse_events = Vec::new();
    for frame in 1..=MAX_FRAMES {
        lab.app.update();
        reverse_peak = reverse_peak.max(lab.joint_y(ARM)?);
        if lab.finished(id)? {
            reverse_frames = frame;
            reverse_events = lab.events()?;
            break;
        }
    }
    if reverse_frames == 0 {
        return Err(format!("the reverse wave did not end within {MAX_FRAMES} frames"));
    }

    // The child montage: same timing, the shorter clip.
    let mut lab = Lab::new(graph, &clips)?;
    let id = lab.play(child.clone(), PlayOptions::default())?;
    let mut child_peak = f32::MIN;
    let mut ended = false;
    for _ in 0..MAX_FRAMES {
        lab.app.update();
        child_peak = child_peak.max(lab.joint_y(ARM)?);
        if lab.finished(id)? {
            ended = true;
            break;
        }
    }
    if !ended {
        return Err(format!(
            "the child montage did not end within {MAX_FRAMES} frames"
        ));
    }
    Ok(SlotGraphReport {
        forward_peak,
        hand_with_slot,
        hand_without_slot,
        arm_after,
        forward_events,
        reverse_frames,
        reverse_peak,
        reverse_events,
        child_length: montage_length(&child, &clips)?,
        parent_length: montage_length(&wave, &clips)?,
        child_peak,
    })
}

/// The rig's slots: a full-body slot and the upper body (the spine and its children).
pub(crate) fn slot_defs() -> Vec<SlotDef> {
    vec![
        SlotDef::full_body("full_body", "body"),
        SlotDef::masked("upper", "upper", &["spine"]),
    ]
}

/// A montage's laid-out length (seconds of montage time).
fn montage_length(montage: &Montage, clips: &ClipLibrary) -> Result<f32, String> {
    montage
        .layout(|name| clips.get(name).cloned())
        .map(|layout| layout.length)
        .map_err(|e| format!("{}: {e}", montage.name))
}

/// The comparison graph: the same Grip layer over the plain `Idle` clip, no Slot node, so the
/// montage applies after the graph and covers the hand too.
fn no_slot_graph() -> AnimGraph {
    AnimGraph {
        parameters: vec![regia_animation::params::ParamDecl(
            "Grip".into(),
            regia_animation::params::ParamValue::Float(1.0),
        )],
        root: AnimNode::Layer(LayerNode {
            base: Box::new(clip_node("Idle")),
            layers: vec![LayerEntry {
                graph: Box::new(clip_node("Grip")),
                mask: JointMaskDef {
                    joints: vec!["hand".into()],
                    include_children: true,
                },
                mode: LayerMode::Override,
                weight_param: "Grip".into(),
                weight_inertialize: 0.0,
            }],
        }),
    }
}

fn clip_node(clip: &str) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: clip.into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    })
}

/// root → spine → arm → hand, each a step up.
fn skeleton() -> Result<Skeleton, String> {
    Skeleton::from_local_bind(
        vec!["root".into(), "spine".into(), "arm".into(), "hand".into()],
        vec![None, Some(0), Some(1), Some(2)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::Y),
            translated(Vec3::new(0.0, ARM_REST, 0.0)),
            translated(Vec3::new(0.0, 0.5, 0.0)),
        ],
    )
    .map_err(|e| format!("lab rig: {e:?}"))
}

/// A track that moves `joint` through `heights` at `times`.
fn rise(joint: u32, times: &[f32], heights: &[f32]) -> JointTrack {
    JointTrack {
        joint,
        translations: TrackCurve::Animated {
            times: times.to_vec(),
            values: heights.iter().map(|&y| Vec3::new(0.0, y, 0.0)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    }
}

/// `Idle` (the bind pose), `Grip` (hand held low), `Wave` (1 s: arm up to 1.5 and back, the
/// hand lifting with it) and `WaveShort` (0.5 s: arm up to 1.0 and back).
pub(crate) fn clips() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("Idle", 1.0));
    let mut grip = AnimationClip::empty("Grip", 1.0);
    grip.tracks.push(JointTrack {
        joint: HAND,
        translations: TrackCurve::Static(Vec3::new(0.0, GRIP, 0.0)),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clips.insert(grip);
    let mut wave = AnimationClip::empty("Wave", 1.0);
    wave.tracks
        .push(rise(ARM, &[0.0, 0.5, 1.0], &[ARM_REST, 1.5, ARM_REST]));
    wave.tracks.push(rise(HAND, &[0.0, 0.5, 1.0], &[0.5, 1.0, 0.5]));
    clips.insert(wave);
    let mut short = AnimationClip::empty("WaveShort", 0.5);
    short
        .tracks
        .push(rise(ARM, &[0.0, 0.25, 0.5], &[ARM_REST, 1.0, ARM_REST]));
    clips.insert(short);
    clips
}

/// One rig in a headless app with the animation plugin.
struct Lab {
    app: App,
    rig: Entity,
}

impl Lab {
    fn new(graph: AnimGraph, clips: &ClipLibrary) -> Result<Self, String> {
        graph.validate().map_err(|e| format!("graph: {e}"))?;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AnimationPlugin));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
        let skeleton = Arc::new(skeleton()?);
        let rig = app.world_mut().spawn(Transform::default()).id();
        let binding = spawn_skeleton(app.world_mut(), skeleton);
        // `Animator::graph` starts with no parameters: the host applies the declared defaults.
        let parameters = graph.default_parameters();
        let mut animator = Animator::graph(graph, clips.clone());
        animator.parameters = parameters;
        let player = MontagePlayer::new(slot_defs(), clips.clone());
        app.world_mut()
            .entity_mut(rig)
            .insert((binding, animator, player));
        Ok(Lab { app, rig })
    }

    fn play(&mut self, montage: Arc<Montage>, options: PlayOptions) -> Result<MontageId, String> {
        let mut entity = self.app.world_mut().entity_mut(self.rig);
        let mut player = entity
            .get_mut::<MontagePlayer>()
            .ok_or("the montage player vanished")?;
        player
            .try_play(montage, options)
            .map_err(|e| format!("play: {e}"))
    }

    fn player(&self) -> Result<&MontagePlayer, String> {
        self.app
            .world()
            .get::<MontagePlayer>(self.rig)
            .ok_or_else(|| "the montage player vanished".to_owned())
    }

    fn finished(&self, id: MontageId) -> Result<bool, String> {
        Ok(self.player()?.status(id).is_none())
    }

    fn events(&self) -> Result<Vec<String>, String> {
        Ok(self
            .player()?
            .history()
            .map(|e| e.kind.as_str().to_owned())
            .collect())
    }

    /// A joint's local height in the evaluated pose.
    fn joint_y(&self, joint: u32) -> Result<f32, String> {
        let animator = self
            .app
            .world()
            .get::<Animator>(self.rig)
            .ok_or("the animator vanished")?;
        animator
            .evaluated_pose()
            .translations
            .get(usize::try_from(joint).unwrap_or(usize::MAX))
            .map(|t| t.y)
            .ok_or_else(|| format!("no joint {joint} in the pose"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::graph::ANIMGRAPH_FORMAT_VERSION;

    #[test]
    fn the_sample_assets_load_at_their_current_versions() {
        let graph = AnimGraph::from_ron_str(GRAPH_RON).expect("graph");
        assert!(
            GRAPH_RON.contains(&format!("format_version: {ANIMGRAPH_FORMAT_VERSION}")),
            "the sample graph is written at the current version"
        );
        assert_eq!(graph.slot_names(), ["upper"]);
        assert!(graph.check_slot_defs(&slot_defs()).is_empty());
        let wave = Montage::from_ron_str(MONTAGE_RON).expect("montage");
        wave.validate().expect("valid montage");
        assert_eq!(wave.tracks[0].slot, "upper");
        let variant = MontageVariant::from_ron_str(VARIANT_RON).expect("variant");
        assert_eq!(variant.parent, "wave");
    }

    #[test]
    fn a_montage_plays_through_the_graph_slot_node_under_the_grip_layer() {
        let report = run().expect("case");
        // The wave reaches its 1.5 peak through the Slot node (the 0.5 s key lies on a frame).
        assert!((report.forward_peak - 1.5).abs() < 1e-4, "{report:?}");
        // The Grip layer sits above the slot, so it still holds the hand ...
        assert!((report.hand_with_slot - GRIP).abs() < 1e-5, "{report:?}");
        // ... which the montage after the graph overrides (its slot mask covers the hand).
        assert!((report.hand_without_slot - 1.0).abs() < 1e-4, "{report:?}");
        // The graph's own pose comes back when the wave ends.
        assert!((report.arm_after - ARM_REST).abs() < 1e-5, "{report:?}");
        for kind in ["started", "notify", "section", "ended"] {
            assert!(
                report.forward_events.iter().any(|k| k == kind),
                "{kind}: {report:?}"
            );
        }
    }

    #[test]
    fn reverse_and_time_stretch_play_the_wave_back_in_half_the_time() {
        let report = run().expect("case");
        // 0.5 s at 1/32 s frames is 16 frames (1/32 is exact in binary, so it lands on frame 16).
        assert_eq!(report.reverse_frames, 16, "{report:?}");
        assert!((report.reverse_peak - 1.5).abs() < 1e-4, "{report:?}");
        // Backward it crosses `lower` into `raise` and passes the peak notify.
        for kind in ["started", "section", "notify", "ended"] {
            assert!(
                report.reverse_events.iter().any(|k| k == kind),
                "{kind}: {report:?}"
            );
        }
    }

    #[test]
    fn the_child_montage_keeps_its_parents_timing_with_its_own_clip() {
        let report = run().expect("case");
        assert!(
            (report.child_length - report.parent_length).abs() < 1e-5,
            "{report:?}"
        );
        assert!((report.parent_length - 1.0).abs() < 1e-5, "{report:?}");
        assert!((report.child_peak - 1.0).abs() < 1e-4, "{report:?}");
    }

    #[test]
    fn the_case_is_deterministic() {
        let first = run().expect("first");
        // The measured line (what `cargo run -p anim_lab` prints), for the test log.
        println!("{first}");
        assert_eq!(first, run().expect("second"));
    }
}
