// SPDX-License-Identifier: MPL-2.0
//! Schedule and rebase regressions from the 11-03 concurrency audit (ADR-2396): world-space warp
//! targets follow an origin rebase before the frame's evaluation reads them, parked entities are
//! not stepped, and the `AnimationApply` writers run in a fixed order.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{PropertyTrack, TrackCurve};
use regia_animation::prelude::*;
use regia_animation::{CrowdAnimationPlugin, ScriptedWarps, WarpTarget};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::floating_origin::Rebase;
use regia_scene::layers::LayerParked;
use regia_scene::prelude::Visibility;
use regia_scene::streaming::OriginShifted;

/// Writes one 4096 m x rebase in `PreUpdate` of the next update.
#[derive(Resource, Default)]
struct RebaseOnce(bool);

fn send_rebase(mut once: ResMut<RebaseOnce>, mut out: MessageWriter<OriginShifted>) {
    if once.0 {
        once.0 = false;
        out.write(OriginShifted {
            rebase: Rebase {
                cell_delta: [1, 0, 0],
                shift: [4096.0, 0.0, 0.0],
            },
            origin_cell: [1, 0, 0],
        });
    }
}

/// What the montage player's and the scripted warps' targets read when evaluation ran.
#[derive(Resource, Default)]
struct SeenAtEvaluation(Vec<(Option<Vec3>, Option<Vec3>)>);

fn record_targets(players: Query<(&MontagePlayer, &ScriptedWarps)>, mut seen: ResMut<SeenAtEvaluation>) {
    for (player, warps) in &players {
        seen.0.push((
            player.warp_target("ledge").map(|t| t.position),
            warps.target("enemy").map(|t| t.position),
        ));
    }
}

fn rebase_app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.init_resource::<RebaseOnce>();
    app.init_resource::<SeenAtEvaluation>();
    app.add_systems(PreUpdate, send_rebase);
    app.add_systems(
        Update,
        record_targets.after(regia_animation::systems::evaluate_animators),
    );
    let mut player = MontagePlayer::default();
    player.set_warp_target("ledge", WarpTarget::at(Vec3::new(5000.0, 2.0, 0.0)));
    let mut warps = ScriptedWarps::new();
    warps.set_target("enemy", WarpTarget::at(Vec3::new(4100.0, 0.0, -3.0)));
    let rig = app.world_mut().spawn((player, warps)).id();
    app.update();
    (app, rig)
}

/// Audit H1: the rebase lands in an earlier phase of the frame (here `PreUpdate`, in the engine
/// `FixedUpdate`), so the montage warp targets must be re-based before `evaluate_animators`
/// reads them in `Update`, not one phase later in `PostUpdate`.
#[test]
fn montage_warp_targets_are_rebased_before_the_same_frame_evaluates() {
    let (mut app, _) = rebase_app();
    app.world_mut().resource_mut::<SeenAtEvaluation>().0.clear();
    app.world_mut().resource_mut::<RebaseOnce>().0 = true;
    app.update();
    let seen = &app.world().resource::<SeenAtEvaluation>().0;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].0,
        Some(Vec3::new(904.0, 2.0, 0.0)),
        "the montage warp target is re-based before evaluation in the rebase frame"
    );
}

/// Audit H2: the scripted warps' world-space targets move with the rebase too.
#[test]
fn scripted_warp_targets_are_rebased() {
    let (mut app, rig) = rebase_app();
    app.world_mut().resource_mut::<SeenAtEvaluation>().0.clear();
    app.world_mut().resource_mut::<RebaseOnce>().0 = true;
    app.update();
    app.update();
    let warps = app.world().get::<ScriptedWarps>(rig).expect("warps");
    assert_eq!(
        warps.target("enemy").map(|t| t.position),
        Some(Vec3::new(4.0, 0.0, -3.0)),
        "a scripted warp target is a world position and follows the rebase"
    );
    let seen = &app.world().resource::<SeenAtEvaluation>().0;
    assert_eq!(
        seen[0].1,
        Some(Vec3::new(4.0, 0.0, -3.0)),
        "re-based before evaluation"
    );
}

fn parked() -> LayerParked {
    LayerParked {
        restore: Visibility::Inherited,
        inherited: false,
    }
}

fn door_clip() -> Arc<AnimationClip> {
    let mut clip = AnimationClip::empty("door", 2.0);
    clip.property_tracks.push(PropertyTrack {
        path: "Transform.translation.y".into(),
        curve: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![0.0, 3.0],
            exact_rotation: false,
        },
    });
    Arc::new(clip)
}

/// Audit M1: a parked entity is not simulated; its property animator keeps its time and does not
/// write the property, and a node morph player keeps its time.
#[test]
fn parked_property_and_node_morph_players_do_not_advance() {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.register_type::<Transform>();
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.update();
    let door = app
        .world_mut()
        .spawn((
            Transform::default(),
            PropertyAnimator {
                clip: door_clip(),
                time: 1.0,
                speed: 1.0,
                looping: false,
            },
            parked(),
        ))
        .id();
    let face = app
        .world_mut()
        .spawn((NodeMorphPlayer::new(door_clip(), false), parked()))
        .id();
    for _ in 0..5 {
        app.update();
    }
    let animator = app.world().get::<PropertyAnimator>(door).expect("animator");
    assert_eq!(animator.time, 1.0, "a parked property animator does not advance");
    let transform = app.world().get::<Transform>(door).expect("transform");
    assert_eq!(
        transform.translation.y, 0.0,
        "a parked property animator does not write"
    );
    let player = app.world().get::<NodeMorphPlayer>(face).expect("player");
    assert_eq!(player.time, 0.0, "a parked node morph player does not advance");

    // Unparked, both run again.
    app.world_mut().entity_mut(door).remove::<LayerParked>();
    app.world_mut().entity_mut(face).remove::<LayerParked>();
    app.update();
    assert!(app.world().get::<PropertyAnimator>(door).expect("animator").time > 1.0);
    assert!(app.world().get::<NodeMorphPlayer>(face).expect("player").time > 0.0);
}

fn ambiguous(app: &mut App, a: &str, b: &str) -> bool {
    app.ambiguity_report()
        .ranked()
        .iter()
        .any(|(_, pair)| (pair.a == a && pair.b == b) || (pair.a == b && pair.b == a))
}

/// Audit M2/M3: the `AnimationApply` writers have a fixed order: property tracks before the pose
/// (so pose writes win on joints and gaze/springs/sockets read settled rig transforms), and the
/// crowd's joint globals before the sockets that read them.
#[test]
fn animation_apply_writers_are_ordered() {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin, CrowdAnimationPlugin));
    for (a, b) in [
        ("apply_property_animators", "apply_animation"),
        ("apply_property_animators", "resolve_sockets"),
        ("apply_crowd_animation", "resolve_sockets"),
    ] {
        assert!(!ambiguous(&mut app, a, b), "{a} and {b} have no order");
    }
}
