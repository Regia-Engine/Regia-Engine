// SPDX-License-Identifier: MPL-2.0
//! ADR-7200: `render.visible_entities` end to end, through the agent protocol's real dispatch path
//! (permission check, rate limit, handler, reply). A mesh ahead of the camera is listed, one behind
//! it is not, a hidden one drops out the next frame, and `camera` narrows the test to one camera.
//! No window, no GPU, no socket, no wall clock: a channel stands in for the WebSocket inbox.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_render::{
    Camera, CpuMaterial, CpuMesh, ExtractedFrame, LabVertex, MeshRenderer, OcclusionFeedback,
    ViewVisibilityStats, extract_frame, update_view_visibility,
};
use regia_scene::Transform;
use regia_scene::components::Visibility;
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    project: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let project = unique_temp_dir("agent-visible-entities");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin))
            .insert_resource(AgentSettings {
                project_dir: Some(project.clone()),
            })
            .add_plugins(AgentPlugin)
            .init_resource::<ExtractedFrame>()
            .init_resource::<ViewVisibilityStats>()
            .add_systems(Render, (extract_frame, update_view_visibility).chain())
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.finish_plugins();
        let (inbox, inbox_rx) = unbounded();
        app.insert_resource(AgentInbox(inbox_rx));
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: u32::MAX,
                ..AgentConfig::default()
            },
            source: None,
        });
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            replies,
            reply,
            project,
        }
    }

    /// Sends one request, runs two frames, and returns the one reply's result or error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        self.inbox
            .send(Inbound {
                request,
                reply: self.reply.clone(),
            })
            .expect("inbox open");
        for _ in 0..2 {
            self.app.update();
        }
        let response = self.replies.try_recv().expect("one reply per request");
        match (response.result, response.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(serde_json::to_value(error).expect("error serializes")),
            other => panic!("malformed response {other:?}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

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

fn listed(reply: &Value) -> Vec<u64> {
    reply["entities"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

#[test]
fn visible_entities_lists_what_the_frame_drew_on_screen() {
    let mut h = Harness::new();
    let world = h.app.world_mut();
    let camera = world.spawn((Transform::default(), Camera::default())).id();
    let mesh = |world: &mut World, z: f32| {
        world
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, z)),
                MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
            ))
            .id()
    };
    let ahead = mesh(world, -5.0);
    let behind = mesh(world, 5.0);
    // The agent answers before `Render` in a frame: one frame extracts and culls the new meshes.
    h.app.update();

    let got = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(got["scope"], "view_visibility");
    assert_eq!(got["visible"], 1, "{got}");
    assert_eq!(listed(&got), vec![ahead.to_bits()]);
    assert!(!listed(&got).contains(&behind.to_bits()));

    // `limit` bounds the list, not the count.
    let none = h
        .call("render.visible_entities", json!({ "limit": 0 }))
        .expect("reply");
    assert_eq!(
        (none["visible"].as_u64(), none["listed"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(none["truncated"], true);

    // One camera's view.
    let one = h
        .call("render.visible_entities", json!({ "camera": camera.to_bits() }))
        .expect("reply");
    assert_eq!(one["scope"], "camera");
    assert_eq!(listed(&one), vec![ahead.to_bits()]);

    // Hidden: off the list from the next frame.
    h.app.world_mut().entity_mut(ahead).insert(Visibility::Hidden);
    h.app.update();
    let hidden = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(hidden["visible"], 0, "{hidden}");

    // Errors: a bad limit, and a camera that is not one.
    let bad = h
        .call("render.visible_entities", json!({ "limit": 5000 }))
        .expect_err("limit too large");
    assert_eq!(bad["data"]["code"], "F0002");
    let not_camera = h
        .call("render.visible_entities", json!({ "camera": behind.to_bits() }))
        .expect_err("not a camera");
    assert_eq!(not_camera["data"]["code"], "F0003");
}

/// ADR-7255: the occlusion result is folded in and counted. A host-published result (what the
/// GPU readback writes) hides a mesh in the frustum from the next frame: `visible` drops, the
/// `frame.occluded` count and the `occlusion` counters say why, and clearing it brings it back.
#[test]
fn visible_entities_reports_the_occlusion_culled_count() {
    let mut h = Harness::new();
    h.app.init_resource::<OcclusionFeedback>();
    let world = h.app.world_mut();
    world.spawn((Transform::default(), Camera::default()));
    let mesh = |world: &mut World, x: f32| {
        world
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, -5.0)),
                MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
            ))
            .id()
    };
    let seen = mesh(world, -0.5);
    let occluded = mesh(world, 0.5);
    h.app.update();
    let before = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(before["visible"], 2, "{before}");
    assert_eq!(before["frame"]["occluded"], 0, "{before}");

    h.app
        .world()
        .resource::<OcclusionFeedback>()
        .publish(&[occluded.to_bits()]);
    h.app.update();
    let after = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(after["visible"], 1, "{after}");
    assert_eq!(listed(&after), vec![seen.to_bits()]);
    assert_eq!(after["frame"]["occluded"], 1, "{after}");
    assert_eq!(after["occlusion"]["published"], 1, "{after}");
    assert_eq!(after["occlusion"]["occluded"], 1, "{after}");

    h.app.world().resource::<OcclusionFeedback>().publish(&[]);
    h.app.update();
    let back = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(back["visible"], 2, "{back}");
    assert_eq!(back["frame"]["occluded"], 0, "{back}");
}

/// ADR-7286: with two views (split screen) each view's occlusion result speaks for that view only.
/// A mesh the left view's result hides but the right view sees stays visible; one both results
/// hide is not. `views` reports each view's in-view and occluded counts and its own readback
/// counters, and `camera` uses that camera's view's result.
#[test]
fn visible_entities_reports_occlusion_per_view() {
    use regia_render::ViewVisibilityPerView;
    let mut h = Harness::new();
    h.app.init_resource::<OcclusionFeedback>();
    h.app.init_resource::<ViewVisibilityPerView>();
    let world = h.app.world_mut();
    let half = |world: &mut World, x: f32| {
        world
            .spawn((
                Transform::default(),
                Camera {
                    viewport: [x, 0.0, 0.5, 1.0],
                    ..Camera::default()
                },
            ))
            .id()
    };
    let _left = half(world, 0.0);
    let right = half(world, 0.5);
    let mesh = |world: &mut World, x: f32| {
        world
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, -5.0)),
                MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
            ))
            .id()
    };
    let one_side = mesh(world, -0.5);
    let both = mesh(world, 0.5);
    h.app.update();
    let before = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(before["visible"], 2, "{before}");
    assert_eq!(before["views"][1]["in_view"], 2, "{before}");
    assert_eq!(
        before["views"][0]["occlusion"],
        Value::Null,
        "no ring yet: {before}"
    );

    let feedback = h.app.world().resource::<OcclusionFeedback>().clone();
    feedback.publish(&[one_side.to_bits(), both.to_bits()]);
    feedback.publish_view(1, &[both.to_bits()]);
    h.app.update();
    let after = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(listed(&after), vec![one_side.to_bits()], "{after}");
    assert_eq!(after["frame"]["occluded"], 1, "{after}");
    let views = &after["views"];
    assert_eq!(views.as_array().map(Vec::len), Some(2), "{after}");
    for (index, (in_view, occluded)) in [(2, 2), (2, 1)].into_iter().enumerate() {
        let line = &views[index];
        assert_eq!(line["view"], index, "{after}");
        assert_eq!(line["target"], "window", "{after}");
        assert_eq!(line["occlusion_folded"], true, "{after}");
        assert_eq!(
            (line["in_view"].as_u64(), line["occluded"].as_u64()),
            (Some(in_view), Some(occluded)),
            "view {index}: {after}"
        );
        assert_eq!(line["occlusion"]["published"], 1, "view {index}: {after}");
    }
    assert_eq!(
        after["occlusion"]["occluded"], 3,
        "summed over the views: {after}"
    );

    // One camera: the right view's result only.
    let one = h
        .call("render.visible_entities", json!({ "camera": right.to_bits() }))
        .expect("reply");
    assert_eq!(listed(&one), vec![one_side.to_bits()], "{one}");
}

/// ADR-7329: each view reports the Hi-Z pyramid the renderer keeps for its occlusion history:
/// `views[i].pyramid_bytes` and the same in the view's `occlusion` block, summed in the
/// top-level `occlusion.pyramid_bytes`; a view that goes away takes its report with it.
#[test]
fn visible_entities_reports_each_views_pyramid_memory() {
    use regia_render::ViewVisibilityPerView;
    let mut h = Harness::new();
    h.app.init_resource::<OcclusionFeedback>();
    h.app.init_resource::<ViewVisibilityPerView>();
    let world = h.app.world_mut();
    let half = |world: &mut World, x: f32| {
        world
            .spawn((
                Transform::default(),
                Camera {
                    viewport: [x, 0.0, 0.5, 1.0],
                    ..Camera::default()
                },
            ))
            .id()
    };
    let _left = half(world, 0.0);
    let right = half(world, 0.5);
    let feedback = h.app.world().resource::<OcclusionFeedback>().clone();
    feedback.publish_view(0, &[]);
    feedback.publish_view(1, &[]);
    feedback.note_view_pyramid(0, 87_380);
    feedback.note_view_pyramid(1, 21_844);
    h.app.update();
    let reply = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(reply["occlusion"]["pyramid_bytes"], 87_380 + 21_844, "{reply}");
    for (index, bytes) in [87_380, 21_844].into_iter().enumerate() {
        let line = &reply["views"][index];
        assert_eq!(line["pyramid_bytes"], bytes, "view {index}: {reply}");
        assert_eq!(line["occlusion"]["pyramid_bytes"], bytes, "view {index}: {reply}");
    }

    // The right view goes away: its pyramid report goes with it.
    h.app.world_mut().despawn(right);
    h.app.update();
    let after = h.call("render.visible_entities", json!({})).expect("reply");
    assert_eq!(after["views"].as_array().map(Vec::len), Some(1), "{after}");
    assert_eq!(after["occlusion"]["pyramid_bytes"], 87_380, "{after}");
    assert_eq!(feedback.view_pyramid_bytes(1), 0);
}
