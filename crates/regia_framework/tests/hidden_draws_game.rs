// SPDX-License-Identifier: MPL-2.0
//! Hidden things stop drawing in a game App (spec 14 § A1, ADR-7200): the sequencer's `visible`
//! track and a net-interpolated proxy kept hidden until its spawn tick is shown both leave the
//! mesh out of the extracted frame. Fixed steps, no wall clock, no GPU.

use std::sync::Arc;
use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::sequencer::{
    Binding, EndPolicy, Sequence, SequenceBindings, SequencePlayer, SequencerPlugin,
};
use regia_render::{Camera, CpuMaterial, CpuMesh, ExtractedFrame, LabVertex, MeshRenderer, extract_frame};
use regia_scene::Transform;
use regia_scene::components::Visibility;

const STEP: Duration = Duration::from_millis(100);

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.5, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.5, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

/// A game App: the scene plugin (transform and visibility propagation), the sequencer, the
/// runtime extraction in `Render`, a camera, and one mesh 5 m ahead of it.
fn game() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, SequencerPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(STEP))
        .init_resource::<ExtractedFrame>()
        .add_systems(Render, extract_frame);
    app.world_mut().spawn((Transform::default(), Camera::default()));
    let mesh = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, -5.0)),
            MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
        ))
        .id();
    app.update();
    (app, mesh)
}

fn drawn(app: &App, entity: Entity) -> bool {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .any(|m| m.entity == entity)
}

#[test]
fn the_sequencer_visible_track_hides_a_mesh_in_a_game() {
    let (mut app, mesh) = game();
    assert!(drawn(&app, mesh), "drawn before the sequence");
    // Shown for the first second, hidden from 1 s, shown again from 2 s.
    let sequence = Sequence::from_ron(
        r#"(version: 5, name: "blink", tracks: [
            (name: "blink", target: "crate", property: "visible",
                keys: Bool([(time: 0.0, value: true), (time: 1.0, value: false), (time: 2.0, value: true)])),
        ])"#,
    )
    .expect("valid sequence");
    let mut player = SequencePlayer::new(sequence);
    player.end = EndPolicy::Hold;
    player.play();
    app.world_mut().spawn((
        player,
        SequenceBindings::default().with("crate", Binding::Entity(mesh)),
    ));
    let mut hidden_frames = 0;
    let mut shown_after = false;
    for _ in 0..30 {
        app.update();
        let time = {
            let world = app.world_mut();
            let mut players = world.query::<&SequencePlayer>();
            players.iter(world).next().map_or(0.0, SequencePlayer::time)
        };
        let visibility = app.world().get::<Visibility>(mesh).copied();
        if visibility == Some(Visibility::Hidden) {
            assert!(
                !drawn(&app, mesh),
                "t = {time}: the track hid it, yet it was extracted"
            );
            hidden_frames += 1;
        } else if hidden_frames > 0 && visibility == Some(Visibility::Visible) {
            assert!(drawn(&app, mesh), "t = {time}: shown again, yet not extracted");
            shown_after = true;
        }
    }
    assert!(hidden_frames > 0, "the track hid the mesh for some frames");
    assert!(shown_after, "and showed it again");
}

/// The state a net client keeps a newly spawned interpolated proxy in until its spawn tick is shown
/// (`regia_net::interp`'s spawn hook inserts `Visibility::Hidden` beside `NetHiddenUntilShown`, and
/// the reveal puts the previous visibility back; regia_net's `spawn_and_despawn_follow_the_shown_time`
/// covers that hook end to end). The renderer honors it: hidden, the proxy draws nothing; revealed, it
/// draws the next frame.
#[test]
fn a_net_interpolated_hidden_proxy_is_not_drawn_until_revealed() {
    let (mut app, proxy) = game();
    let previous = app.world().get::<Visibility>(proxy).copied();
    app.world_mut().entity_mut(proxy).insert(Visibility::Hidden);
    app.update();
    assert!(!drawn(&app, proxy), "a hidden proxy is not extracted");
    app.world_mut()
        .entity_mut(proxy)
        .insert(previous.unwrap_or(Visibility::Inherited));
    app.update();
    assert!(drawn(&app, proxy), "revealed, it draws the next frame");
}
