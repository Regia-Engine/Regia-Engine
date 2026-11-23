// SPDX-License-Identifier: MPL-2.0
//! Mesh LOD chains on the visibility buffer (ADR-1148).
//!
//! A row of rocks with a two-level `MeshLodChain` and cooked meshlets (the cooker's cluster
//! hierarchy, so far rocks draw coarse clusters) under LOD 0's key, drawn through
//! the runtime path (`extract_frame`, `select_mesh_lods`, `render_offscreen`) with the visibility
//! buffer on. Two runs:
//!
//! - **hand-off**: the renderer is the app's `RuntimeRenderer`, so the selector sees that the
//!   path drew and leaves every rock at LOD 0. From the second frame on, every rock is a
//!   visibility instance and none draws forward.
//! - **chain only**: the same renderer held outside the app (a host driving `FrameRenderer`
//!   itself). The selector switches the far rocks to LOD 1, whose key has no meshlets, so they
//!   draw forward; only the near rocks are visibility instances.
//!
//! Counts are asserted; GPU frame times are printed (meaningful on a real GPU only):
//!
//! ```text
//! VISBUF_LOD_N=4000 VISBUF_LOD_FRAMES=40 cargo test -p regia_render --release \
//!     --test visbuffer_lod_handoff -- --nocapture --test-threads=1
//! ```

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_core::math::Vec3;
use regia_render::visbuffer::MeshletMesh;
use regia_render::{
    AntiAliasing, Camera, ClusterLodHandoff, CpuMaterial, CpuMesh, DirectionalLight, ExtractedFrame,
    FrameRenderer, LabVertex, LodViewport, MeshLodChain, MeshLodLevel, RenderDevice, RenderSettings,
    RuntimeRenderer, SsaoMode, extract_frame, mesh_key, select_mesh_lods,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 160;
const H: u32 = 96;
const FOV: f32 = 0.9;
const SEGMENTS: u32 = 48;
const RINGS: u32 = 32;

fn env_usize(name: &str, default: usize) -> usize {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn place(pos: Vec3) -> (Transform, GlobalTransform) {
    let transform = Transform::from_translation(pos);
    (transform, GlobalTransform(transform.compute_affine()))
}

/// Triangles of a UV sphere's quads taken every `step` rings and segments (outward, CCW). The
/// quads touching a pole are one triangle (the other would be degenerate).
fn sphere_indices(step: u32) -> Vec<u32> {
    let row = SEGMENTS + 1;
    let mut indices = Vec::new();
    for r in (0..RINGS).step_by(step as usize) {
        for s in (0..SEGMENTS).step_by(step as usize) {
            let a = r * row + s;
            let b = (r + step) * row + s;
            let c = (r + step) * row + s + step;
            let d = r * row + s + step;
            if r + step < RINGS {
                indices.extend_from_slice(&[a, c, b]);
            }
            if r > 0 {
                indices.extend_from_slice(&[a, d, c]);
            }
        }
    }
    indices
}

/// A unit sphere whose LOD 1 is the same sphere on every other ring and segment (a subset of
/// LOD 0's vertices, as a cooked chain shares one vertex buffer), and LOD 0's meshlets.
fn rock() -> (Arc<CpuMesh>, MeshLodChain, Arc<MeshletMesh>) {
    let mut vertices = Vec::new();
    for r in 0..=RINGS {
        let theta = std::f32::consts::PI * r as f32 / RINGS as f32;
        for s in 0..=SEGMENTS {
            let phi = std::f32::consts::TAU * s as f32 / SEGMENTS as f32;
            let p = [theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                p,
                [s as f32 / SEGMENTS as f32, r as f32 / RINGS as f32],
            ));
        }
    }
    let fine = sphere_indices(1);
    let coarse = sphere_indices(2);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let attributes = MeshletAttributes {
        normals: &normals,
        uvs: &uvs,
    };
    let meshlets =
        build_meshlet_mesh_with_attributes(&positions, Some(attributes), &fine, &MeshletSettings::default())
            .unwrap_or_else(|err| panic!("meshlet cook: {err}"));
    assert!(
        meshlets.meshlets.iter().any(|m| m.level > 0),
        "the cooker built no coarser level: far rocks would draw full detail"
    );
    let meshlets = Arc::new(meshlets);
    let chain = MeshLodChain {
        levels: vec![
            MeshLodLevel {
                indices: Arc::new(fine.clone()),
                screen_size: 0.25,
            },
            MeshLodLevel {
                indices: Arc::new(coarse),
                screen_size: 0.0,
            },
        ],
        bounds_center: [0.0; 3],
        bounds_radius: 1.0,
    };
    (
        Arc::new(CpuMesh {
            vertices,
            indices: fine,
        }),
        chain,
        meshlets,
    )
}

/// Desktop High with the visibility buffer on, a depth prepass, the post stack and no MSAA (the
/// path's frame conditions), and the costly screen-space passes off.
fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopHigh);
    settings.profile.visibility_buffer = true;
    settings.profile.depth_prepass = true;
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.gi_probes = false;
    settings.profile.screen_space_gi = false;
    settings.profile.ssr = false;
    settings.profile.volumetric_fog = false;
    settings.profile.volumetric_clouds = false;
    settings.profile.physical_atmosphere = false;
    settings.profile.reflection_probes = false;
    settings.profile.bloom = false;
    settings.profile.auto_exposure = false;
    settings.profile.ssao = SsaoMode::Off;
    settings.profile.contact_shadows = false;
    settings.profile.motion_blur = false;
    settings.profile.depth_of_field = false;
    // The rocks' LOD 0 → 1 switch at screen size 0.25 is about 8 m with scale 1.
    settings.profile.mesh_lod_distance_scale = 1.0;
    settings.profile.mesh_lod_shadow_scale = 0.5;
    settings.post.bloom = false;
    settings.post.auto_exposure = false;
    settings.post.taa = false;
    settings.post.fxaa = false;
    settings.post.exposure_ev = 14.0;
    settings
}

/// `n` rocks in two columns from 4 m to about `4 + 3n` m down −Z; the camera at the origin.
fn app(n: usize, mesh: &Arc<CpuMesh>, chain: &MeshLodChain) -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ClusterLodHandoff>();
    app.insert_resource(settings());
    app.insert_resource(LodViewport {
        width: 1920,
        height: 1080,
    });
    app.add_systems(Render, (extract_frame, select_mesh_lods).chain());
    let material = Arc::new(CpuMaterial {
        base_color: [0.55, 0.5, 0.45, 1.0],
        roughness: 0.9,
        ..CpuMaterial::default()
    });
    for i in 0..n {
        let x = if i.is_multiple_of(2) { -1.5 } else { 1.5 };
        app.world_mut().spawn((
            place(Vec3::new(x, 0.0, -(4.0 + 3.0 * i as f32))),
            regia_render::MeshRenderer::new(Arc::clone(mesh), Arc::clone(&material)),
            chain.clone(),
        ));
    }
    app.world_mut().spawn((
        place(Vec3::ZERO),
        Camera {
            fov_y: FOV,
            exposure_ev: Some(14.0),
            near: 0.1,
            far: 4.0 + 3.0 * n as f32 + 10.0,
            ..Camera::default()
        },
    ));
    app.world_mut().spawn((
        place(Vec3::ZERO),
        DirectionalLight {
            direction: [0.3, -0.8, -0.5],
            color: [1.0, 0.98, 0.95],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    app
}

/// One frame's counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts {
    /// Extracted draws (after LOD selection).
    draws: usize,
    /// Colour draws at LOD 0 (the key the meshlets are under).
    lod0: usize,
    /// Visibility-buffer instances the renderer drew.
    vis_instances: u32,
    /// Meshes the forward colour pass drew.
    forward: usize,
    handoff: ClusterLodHandoff,
}

struct Run {
    counts: Vec<Counts>,
    gpu_ms: Vec<f32>,
}

/// Steps the app (extraction and LOD selection) and draws its frame with `held`, or with the
/// app's `RuntimeRenderer` when `held` is `None`.
fn frame(app: &mut App, held: Option<&mut FrameRenderer>, device: &RenderDevice, i: usize) -> (Counts, f32) {
    app.update();
    let extracted = app.world().resource::<ExtractedFrame>().clone();
    let handoff = *app.world().resource::<ClusterLodHandoff>();
    match held {
        Some(renderer) => draw(renderer, device, &extracted, handoff, i),
        None => draw(
            &mut app.world_mut().resource_mut::<RuntimeRenderer>().0,
            device,
            &extracted,
            handoff,
            i,
        ),
    }
}

fn draw(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    extracted: &ExtractedFrame,
    handoff: ClusterLodHandoff,
    i: usize,
) -> (Counts, f32) {
    let scene = extracted.frame_scene(0, W, H);
    renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("frame {i}: {err}"));
    let draws = renderer.draw_stats();
    let counts = Counts {
        draws: extracted.meshes.len(),
        lod0: extracted.meshes.iter().filter(|m| m.lod_key == 0).count(),
        vis_instances: renderer.visbuffer_stats().instances,
        forward: draws.instanced + draws.per_object,
        handoff,
    };
    (counts, renderer.gpu_timings().frame_ms)
}

fn run(device: &RenderDevice, n: usize, frames: usize, in_world: bool) -> Run {
    let (mesh, chain, meshlets) = rock();
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    renderer.insert_meshlets(mesh_key(&mesh), meshlets);
    let mut app = app(n, &mesh, &chain);
    let mut held = if in_world {
        app.insert_resource(RuntimeRenderer(renderer));
        None
    } else {
        Some(renderer)
    };
    let mut out = Run {
        counts: Vec::with_capacity(frames),
        gpu_ms: Vec::with_capacity(frames),
    };
    for i in 0..frames {
        let (counts, ms) = frame(&mut app, held.as_mut(), device, i);
        out.counts.push(counts);
        out.gpu_ms.push(ms);
    }
    out
}

fn median(values: &[f32]) -> f32 {
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

#[test]
fn chained_meshes_with_meshlets_stay_on_the_visibility_buffer_at_every_distance() {
    let device = RenderDevice::shared_headless().unwrap_or_else(|err| panic!("no headless GPU: {err}"));
    let n = env_usize("VISBUF_LOD_N", 24);
    let frames = env_usize("VISBUF_LOD_FRAMES", 4).max(3);
    let handoff = run(&device, n, frames, true);
    let chain = run(&device, n, frames, false);
    let warm = frames / 2;
    eprintln!(
        "visbuffer LOD hand-off, {n} chained rocks, {W}x{H}: GPU frame median (frames {warm}..) \
         hand-off {:.3} ms, chain only {:.3} ms; last frame hand-off {:?}, chain only {:?}",
        median(&handoff.gpu_ms[warm..]),
        median(&chain.gpu_ms[warm..]),
        handoff.counts.last(),
        chain.counts.last()
    );

    // Frame 0: the path has not drawn yet, so the chain picks (only the near rocks are at LOD 0).
    let first = handoff.counts[0];
    assert!(!first.handoff.active);
    assert_eq!(first.handoff.cluster_lod, 0);
    let near = first.lod0;
    assert!(
        (1..n).contains(&near),
        "the layout puts {near} of {n} rocks at LOD 0; it needs some on each side of the switch"
    );
    // From frame 1: every rock stays at LOD 0 and draws through the visibility buffer.
    for (i, c) in handoff.counts.iter().enumerate().skip(1) {
        assert!(
            c.handoff.active,
            "frame {i}: the path drew the frame before: {c:?}"
        );
        assert_eq!(c.handoff.chained as usize, n, "frame {i}: {c:?}");
        assert_eq!(c.handoff.cluster_lod as usize, n, "frame {i}: {c:?}");
        assert_eq!(
            c.draws, n,
            "frame {i}: one draw per rock, no fade or shadow LOD: {c:?}"
        );
        assert_eq!(c.lod0, n, "frame {i}: {c:?}");
        assert_eq!(c.vis_instances as usize, n, "frame {i}: {c:?}");
        assert_eq!(c.forward, 0, "frame {i}: no rock draws forward: {c:?}");
    }
    // Without the runtime renderer in the app, the far rocks keep their chain and draw forward.
    for (i, c) in chain.counts.iter().enumerate() {
        assert!(!c.handoff.active, "frame {i}: {c:?}");
        assert_eq!(c.handoff.cluster_lod, 0, "frame {i}: {c:?}");
        assert_eq!(c.lod0, near, "frame {i}: {c:?}");
        assert_eq!(
            c.vis_instances as usize, near,
            "frame {i}: only LOD 0 has meshlets: {c:?}"
        );
        assert_eq!(
            c.forward,
            n - near,
            "frame {i}: the LOD 1 rocks draw forward: {c:?}"
        );
    }
}

/// The dense selection and the LOD chains (ADR-2071): the chains keep picking while the scene is
/// sparse, hand over one frame after the path takes the scene, and pick again the frame after the
/// path gives it back.
#[test]
fn a_dense_selection_hands_the_chains_over_only_while_the_scene_is_dense() {
    let device = RenderDevice::shared_headless().unwrap_or_else(|err| panic!("no headless GPU: {err}"));
    let n = 12;
    let (mesh, chain, meshlets) = rock();
    let mut renderer = FrameRenderer::new();
    renderer.insert_meshlets(mesh_key(&mesh), meshlets);
    let mut app = app(n, &mesh, &chain);
    app.insert_resource(RuntimeRenderer(renderer));
    let select = |enter_instances: u32| regia_render::VisibilityPath {
        select: regia_render::VisibilitySelect::Dense(regia_render::DenseSceneSelect {
            enter_triangles: u64::MAX,
            enter_instances,
            enter_frames: 2,
            leave_frames: 2,
        }),
        single_pass_resolve: false,
    };
    let set = |app: &mut App, path| {
        app.world_mut()
            .resource_mut::<RenderSettings>()
            .profile
            .visibility_path = path;
    };

    // Sparse: nothing takes the path, the chains pick, the far rocks draw forward at coarse levels.
    set(&mut app, select(1000));
    let mut sparse = None;
    for i in 0..3 {
        let (counts, _) = frame(&mut app, None, &device, i);
        assert!(!counts.handoff.active, "frame {i}: {counts:?}");
        assert_eq!(counts.handoff.cluster_lod, 0, "frame {i}: {counts:?}");
        assert_eq!(counts.vis_instances, 0, "frame {i}: {counts:?}");
        sparse = Some(counts);
    }
    let near = sparse.map_or(0, |c| c.lod0);
    assert!((1..n).contains(&near), "{near} of {n} at LOD 0");

    // Dense: the second frame in a row takes the path, and the frame after it the clusters pick.
    set(&mut app, select(n as u32));
    let (first, _) = frame(&mut app, None, &device, 10);
    assert_eq!(
        first.vis_instances, 0,
        "one frame over is not dense yet: {first:?}"
    );
    let (second, _) = frame(&mut app, None, &device, 11);
    assert_eq!(
        second.handoff.cluster_lod, 0,
        "the chains have not heard yet: {second:?}"
    );
    let (third, _) = frame(&mut app, None, &device, 12);
    assert!(third.handoff.active, "{third:?}");
    assert_eq!(third.handoff.cluster_lod as usize, n, "{third:?}");
    assert_eq!(third.lod0, n, "{third:?}");
    assert_eq!(third.vis_instances as usize, n, "{third:?}");
    assert_eq!(third.forward, 0, "{third:?}");
    let report = app
        .world()
        .resource::<RuntimeRenderer>()
        .0
        .visbuffer_dense_report();
    assert_eq!(report.entered, 1, "{report:?}");
    assert_eq!(report.density.instances as usize, n, "{report:?}");

    // Dense no more: under half of the threshold for two frames, the chains pick again.
    set(&mut app, select(n as u32 * 4));
    for i in 20..22 {
        frame(&mut app, None, &device, i);
    }
    let (after, _) = frame(&mut app, None, &device, 22);
    assert!(!after.handoff.active, "{after:?}");
    assert_eq!(after.handoff.cluster_lod, 0, "{after:?}");
    assert_eq!(after.lod0, near, "{after:?}");
    let report = app
        .world()
        .resource::<RuntimeRenderer>()
        .0
        .visbuffer_dense_report();
    assert_eq!(report.left, 1, "{report:?}");
}
