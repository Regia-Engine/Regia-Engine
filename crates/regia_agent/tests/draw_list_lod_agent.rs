// SPDX-License-Identifier: MPL-2.0
//! ADR-8457: the LOD and shadow rows of `render.draw_list`, end to end through the agent
//! protocol's real dispatch path (permission check, rate limit, handler, reply). Keyed draws with
//! a LOD chain are counted per colour and shadow LOD after the frame's selection, a dolly-out shows
//! a draw cross-fading, and local and shadowless casters are counted. No window, no GPU, no socket,
//! no wall clock: a channel stands in for the WebSocket inbox.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DrawKey, DrawList, ExtractedFrame, KeyedDraw, LabVertex, LodViewport,
    MeshLodChain, MeshLodLevel, MeshLodScales, ShadowCast, extract_frame, select_mesh_lods,
};
use regia_scene::Transform;
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

const FOV_Y: f32 = 0.9;

struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    project: std::path::PathBuf,
    camera: Entity,
}

impl Harness {
    fn new() -> Self {
        let project = unique_temp_dir("agent-draw-list-lod");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin))
            .insert_resource(AgentSettings {
                project_dir: Some(project.clone()),
            })
            .add_plugins(AgentPlugin)
            .init_resource::<ExtractedFrame>()
            .init_resource::<DrawList>()
            .insert_resource(LodViewport {
                width: 1920,
                height: 1080,
            })
            .add_systems(Render, (extract_frame, select_mesh_lods).chain())
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
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.0, 4.0)),
                Camera {
                    fov_y: FOV_Y,
                    near: 0.1,
                    far: 2000.0,
                    ..Camera::default()
                },
            ))
            .id();
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            replies,
            reply,
            project,
            camera,
        }
    }

    fn set_distance(&mut self, distance: f32) {
        self.app
            .world_mut()
            .entity_mut(self.camera)
            .insert(Transform::from_translation(Vec3::new(0.0, 0.0, distance)));
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

fn mesh() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3],
        indices: vec![0, 1, 2],
    })
}

/// Three levels: LOD 0 above 0.4 of the screen, LOD 1 above 0.1, LOD 2 below.
fn chain() -> Arc<MeshLodChain> {
    Arc::new(MeshLodChain {
        levels: [(3usize, 0.4f32), (2, 0.1), (1, 0.0)]
            .into_iter()
            .map(|(tris, screen_size)| MeshLodLevel {
                indices: Arc::new(vec![0; tris * 3]),
                screen_size,
            })
            .collect(),
        bounds_center: [0.0; 3],
        bounds_radius: 1.0,
    })
}

fn switch_distance(threshold: f32) -> f32 {
    MeshLodScales::default()
        .switch_distance(1.0, threshold, 1080, FOV_Y, 1.0, false)
        .expect("a positive threshold switches")
}

#[test]
fn draw_list_reports_lod_rows_and_local_casters() {
    let mut h = Harness::new();
    let (mesh, material, chain) = (mesh(), Arc::new(CpuMaterial::default()), chain());
    let local = ShadowCast::Local {
        reach: 2.0,
        max_cascades: 1,
        shadow_scale: 1.0,
        shadow_lift: 0.0,
        anchor: None,
        lod_switch_keeps_pages: false,
    };
    {
        let mut list = h.app.world_mut().resource_mut::<DrawList>();
        let item = |x: f32| {
            KeyedDraw::new(
                Arc::clone(&mesh),
                Arc::clone(&material),
                Mat4::from_translation(Vec3::new(x, 0.0, 0.0)),
            )
        };
        // Two chained draws (one a local caster), one plain draw that casts no shadow.
        list.insert(DrawKey(1), item(0.0).with_lod_chain(Arc::clone(&chain)));
        list.insert(
            DrawKey(2),
            item(0.0).with_lod_chain(Arc::clone(&chain)).with_shadow(local),
        );
        list.insert(DrawKey(3), item(0.0).with_shadow(ShadowCast::Off));
    }
    h.set_distance(0.5 * switch_distance(0.4));
    h.app.update();

    let near = h.call("render.draw_list", json!({})).expect("reply");
    assert_eq!(near["present"], true, "{near}");
    assert_eq!(near["items"], 3);
    assert_eq!(near["chained"], 2);
    assert_eq!(near["local_casters"], 1);
    assert_eq!(near["shadow_off"], 1);
    assert_eq!(near["lod"]["selected"], 2, "{near}");
    assert_eq!(
        near["lod"]["color_lods"],
        json!([2]),
        "both chained draws at LOD 0"
    );
    assert_eq!(near["lod"]["crossfading"], 0);
    assert!(near["lod"]["selections"].as_u64().is_some_and(|n| n >= 1));
    let draws = near["draws"].as_array().expect("draws");
    assert_eq!(
        draws.iter().map(|d| d["lod_levels"].clone()).collect::<Vec<_>>(),
        vec![json!(3), json!(3), json!(0)]
    );
    assert_eq!(draws[1]["shadow"], "local");

    // Dolly out past the LOD 0 → 1 switch: the next frames cross-fade both chained draws. The
    // agent answers before `Render` in a frame, so one frame selects at the new distance first.
    h.set_distance(1.5 * switch_distance(0.4));
    h.app.update();
    let fading = h.call("render.draw_list", json!({})).expect("reply");
    assert_eq!(fading["lod"]["color_lods"], json!([0, 2]), "{fading}");
    assert_eq!(fading["lod"]["crossfading"], 2, "{fading}");
    assert_eq!(fading["extracted"], 3);

    // A bad limit is the universal invalid-params error.
    let bad = h
        .call("render.draw_list", json!({ "limit": -1 }))
        .expect_err("bad limit");
    assert_eq!(bad["data"]["code"], "F0002", "{bad}");
}
