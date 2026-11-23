// SPDX-License-Identifier: MPL-2.0
//! The graph `Slot` node check (`animgraph_unknown_slot`, spec 34 M9) runs again when the
//! animator's graph or the player's slots change, and still warns only once per combination
//! (deep-zai-animation finding 5).
//!
//! The capture is a process-wide tracing subscriber, because the systems may run on executor
//! threads a thread-local subscriber would not see. It is installed once (`captured`) and
//! shared, so another test in this binary must use slot names of its own: the warnings of every
//! test land in the same log.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::graph::{ClipNode, SlotNode};
use regia_animation::pose::translated;
use regia_animation::prelude::*;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

/// Every event message the process logged.
struct Capture(Arc<Mutex<Vec<String>>>);

struct MessageText(String);

impl tracing::field::Visit for MessageText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::Id {
        tracing::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::Id, _: &tracing::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut text = MessageText(String::new());
        event.record(&mut text);
        if let Ok(mut seen) = self.0.lock() {
            seen.push(text.0);
        }
    }
    fn enter(&self, _: &tracing::Id) {}
    fn exit(&self, _: &tracing::Id) {}
}

/// The process-wide event log, installing the capturing subscriber on first use. A second
/// `set_global_default` would fail, so every test in this binary goes through here.
fn captured() -> Arc<Mutex<Vec<String>>> {
    static SEEN: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
    Arc::clone(SEEN.get_or_init(|| {
        let seen = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Capture(Arc::clone(&seen)))
            .expect("no other global subscriber in this test binary");
        seen
    }))
}

fn slot_graph(slot: &str) -> AnimGraph {
    AnimGraph {
        parameters: Vec::new(),
        root: AnimNode::Slot(SlotNode {
            name: slot.into(),
            source: Box::new(AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            })),
            eval_idle_source: true,
        }),
    }
}

fn graph_clips() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    clips
}

fn unknown_slot_warnings(seen: &Mutex<Vec<String>>, slot: &str) -> usize {
    let needle = format!("{slot:?}");
    seen.lock()
        .map(|m| {
            m.iter()
                .filter(|m| m.contains("animgraph_unknown_slot") && m.contains(&needle))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn a_changed_graph_or_slot_list_is_checked_again_and_warned_once() {
    let seen = captured();

    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(1.0 / 32.0),
    ));
    let skeleton = Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let mut montage_clips = ClipLibrary::new();
    // Long enough that the montage stays active through the whole test.
    montage_clips.insert(AnimationClip::empty("strike", 60.0));
    let mut player = MontagePlayer::new(
        vec![
            SlotDef::full_body("full_body", "body"),
            SlotDef::masked("upper", "upper", &["arm"]),
        ],
        montage_clips,
    );
    // The montage plays on `upper`, so it keeps playing when `full_body` goes away below.
    player.play(
        Arc::new(Montage::single("strike", "upper", "strike")),
        PlayOptions::default(),
    );
    let rig = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::graph(slot_graph("full_body"), graph_clips()),
            player,
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(
        unknown_slot_warnings(&seen, "full_body"),
        0,
        "a known slot is fine"
    );

    // The host swaps in a revised graph whose Slot node names a slot the rig lacks.
    app.world_mut().get_mut::<Animator>(rig).expect("animator").source = AnimatorSource::Graph {
        graph: Arc::new(slot_graph("tail")),
        clips: Arc::new(graph_clips()),
    };
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(
        unknown_slot_warnings(&seen, "tail"),
        1,
        "the new graph is checked, once: {:?}",
        seen.lock().map(|m| m.clone()).unwrap_or_default()
    );

    // Back to the first graph, then the player loses its `full_body` slot: the unchanged graph
    // now names a missing slot.
    app.world_mut().get_mut::<Animator>(rig).expect("animator").source = AnimatorSource::Graph {
        graph: Arc::new(slot_graph("full_body")),
        clips: Arc::new(graph_clips()),
    };
    app.update();
    assert_eq!(unknown_slot_warnings(&seen, "full_body"), 0);
    app.world_mut()
        .get_mut::<MontagePlayer>(rig)
        .expect("player")
        .slots = vec![SlotDef::masked("upper", "upper", &["arm"])];
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(
        unknown_slot_warnings(&seen, "full_body"),
        1,
        "a changed slot list is checked again, once"
    );
}

/// A second test in this binary: it shares the one capture (`captured`) with the test above,
/// and uses slot names of its own. A graph whose `Slot` node names a slot the rig lacks is
/// warned about once when the montage first plays, not once per frame.
#[test]
fn an_unknown_slot_in_the_first_graph_is_warned_once() {
    let seen = captured();

    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(1.0 / 32.0),
    ));
    let skeleton = Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let mut montage_clips = ClipLibrary::new();
    montage_clips.insert(AnimationClip::empty("wave", 60.0));
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("chest", "body")], montage_clips);
    player.play(
        Arc::new(Montage::single("wave", "chest", "wave")),
        PlayOptions::default(),
    );
    app.world_mut().spawn((
        Transform::default(),
        binding,
        Animator::graph(slot_graph("wing"), graph_clips()),
        player,
    ));
    for _ in 0..6 {
        app.update();
    }
    assert_eq!(
        unknown_slot_warnings(&seen, "wing"),
        1,
        "warned once over six frames: {:?}",
        seen.lock().map(|m| m.clone()).unwrap_or_default()
    );
    assert_eq!(unknown_slot_warnings(&seen, "chest"), 0, "a known slot is fine");
}

/// rev-61 note 1: the check's key was the graph `Arc`'s address, so an in-place edit through
/// the animator's own `Arc` (same address, new content) was never checked again. The key is
/// the content the check reads — the graph's `Slot` node names — so an in-place rename warns.
#[test]
fn an_in_place_graph_edit_is_checked_again() {
    let seen = captured();

    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(1.0 / 32.0),
    ));
    let skeleton = Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let mut montage_clips = ClipLibrary::new();
    montage_clips.insert(AnimationClip::empty("hymn", 60.0));
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("roost", "body")], montage_clips);
    player.play(
        Arc::new(Montage::single("hymn", "roost", "hymn")),
        PlayOptions::default(),
    );
    let rig = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::graph(slot_graph("crest"), graph_clips()),
            player,
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(
        unknown_slot_warnings(&seen, "crest"),
        1,
        "the unknown crest slot warns once"
    );

    // The host edits the graph in place through the animator's own `Arc`: the allocation (and
    // so the address) is unchanged, the content is not.
    {
        let mut animator = app.world_mut().get_mut::<Animator>(rig).expect("animator");
        let AnimatorSource::Graph { graph, .. } = &mut animator.source else {
            panic!("the animator plays a graph");
        };
        let graph = Arc::get_mut(graph).expect("the animator holds the only Arc");
        assert_eq!(graph.rename_slot("crest", "plume"), 1);
    }
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(
        unknown_slot_warnings(&seen, "plume"),
        1,
        "the in-place edit is checked again, once: {:?}",
        seen.lock().map(|m| m.clone()).unwrap_or_default()
    );
    assert_eq!(
        unknown_slot_warnings(&seen, "crest"),
        1,
        "the old name is not re-warned"
    );
}
