// SPDX-License-Identifier: MPL-2.0
//! What a lightmap receiver does when its mesh moved since the bake (ADR-3006): keep drawing the baked
//! light (the default) or fall back to the frame's other light, here a baked volume, through
//! `apply_lightmap_receivers` and a frame on the forward paths. A fallback frame is byte for byte
//! the frame the same floor makes with no lightmap at all, a kept frame is the lightmap's, and a
//! receiver that did not move keeps its lightmap under either policy. No golden image: every check
//! compares frames rendered here (lavapipe, deterministic).

use std::sync::{Arc, OnceLock};

use regia_app::prelude::*;
use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer,
    VolumeLayout,
};
use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
    lightmap_source_hash,
};
use regia_render::lightmap::{
    AppliedLightmapReceiver, InstalledLightmap, LIGHTMAP_RECEIVER_STALE, LightmapReceiver,
    LightmapReceiverReport, LightmapSettings, ReceiverFreshness, StaleLightmapFallback, StaleReceiverPolicy,
    apply_lightmap_receivers,
};
use regia_render::{
    CpuMesh, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabMesh, LabScene, LabVertex, MeshRenderer, MeshUv1, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, RuntimeRenderer, SsaoMode,
};

const W: u32 = 96;
const H: u32 = 64;
/// The lightmap's irradiance (lux), well away from what the volume holds.
const BAKED_LUX: f32 = 12_000.0;
/// What the volume holds of every component.
const VOLUME_LEVEL: f32 = 0.05;
/// Pipelines compile once per renderer: one test at a time keeps lavapipe's threads useful.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

/// A 16 m floor at y = 0 facing up.
fn floor() -> Arc<CpuMesh> {
    let n = [0.0, 1.0, 0.0];
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-8.0, 0.0, 8.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([8.0, 0.0, 8.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([8.0, 0.0, -8.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-8.0, 0.0, -8.0], n, [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

/// The floor after the bake: the same triangles, a corner a little off the floor. Only the hash can
/// tell, so the receiver still builds.
fn moved_floor() -> Arc<CpuMesh> {
    let mesh = floor();
    let mut vertices = mesh.vertices.clone();
    vertices[0].position[1] = 0.02;
    Arc::new(CpuMesh {
        vertices,
        indices: mesh.indices.clone(),
    })
}

/// A lightmap of one entry `floor`, its hash taken from [`floor`].
fn lightmap() -> Lightmap {
    let mesh = floor();
    let map = Lightmap::from_irradiance(
        LightmapLayout {
            width: 4,
            height: 4,
            density: 0.5,
            padding: 0,
            entries: vec![LightmapEntry {
                name: "floor".into(),
                source_vertices: 4,
                rect: [0, 0, 4, 4],
                page: 0,
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 256.0,
                texels_per_metre: 0.25,
                key: None,
                seams: None,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &[[BAKED_LUX; 3]; 16],
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    );
    let mut map = match map {
        Ok(map) => map,
        Err(err) => panic!("valid lightmap: {err:?}"),
    };
    map.source_hashes = vec![lightmap_source_hash(
        mesh.vertices.iter().map(|v| v.position),
        &mesh.indices,
    )];
    map
}

fn volume() -> Arc<BakedGiVolume> {
    let layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-16.0, -16.0],
            spacing: [8.0, 8.0],
            counts: [5, 5],
            levels: vec![0.5],
            ground: vec![0.0; 25],
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| ProbeData {
            sh: [[[VOLUME_LEVEL, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
            moments: vec![[16.0, 256.0]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let lights = BakeLights {
        sky_scale: 1.0,
        ..BakeLights::default()
    };
    match BakedGiVolume::from_probes(&layout, lights, BakeInfo::default(), &probes) {
        Ok(volume) => Arc::new(volume),
        Err(err) => panic!("valid volume: {err:?}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Path {
    Cpu,
    GpuDriven,
}

fn quality(path: Path) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.ssao = SsaoMode::Off;
    q.baked_gi = true;
    q.gpu_driven = path == Path::GpuDriven;
    q.visibility_buffer = false;
    q.paged_sun_shadows = false;
    q
}

fn lab() -> LabScene {
    let mesh = floor();
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 4.0, 6.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: mesh.vertices.clone(),
            indices: mesh.indices.clone(),
            ..LabMesh::unskinned()
        }],
    }
}

/// An app whose one entity is a receiver of the installed lightmap, drawing `mesh`; a baked volume
/// lights the frame in the same renderer. Only `apply_lightmap_receivers` runs: the frame below reads
/// what it left on the entity and in the renderer.
fn app(mesh: Arc<CpuMesh>, policy: StaleReceiverPolicy) -> (App, Entity) {
    let mut app = App::new();
    app.init_resource::<RuntimeRenderer>();
    app.init_resource::<LightmapReceiverReport>();
    app.insert_resource(LightmapSettings {
        enabled: true,
        stale_receivers: policy,
        ..LightmapSettings::default()
    });
    let mut installed = InstalledLightmap::default();
    installed.install("test", lightmap());
    app.insert_resource(installed);
    app.world_mut()
        .resource_mut::<RuntimeRenderer>()
        .0
        .set_baked_gi(Some(volume()));
    let material = Arc::new(LabMaterial::default());
    let entity = app
        .world_mut()
        .spawn((
            MeshRenderer::new(mesh, material),
            LightmapReceiver {
                entry: "floor".into(),
            },
        ))
        .id();
    app.add_systems(Render, apply_lightmap_receivers);
    (app, entity)
}

/// Runs the receivers, then two frames of what the entity draws now (`mesh_key` names the mesh).
fn draw(app: &mut App, entity: Entity, path: Path) -> Vec<u8> {
    app.update();
    let world = app.world_mut();
    let mesh = world
        .get::<MeshRenderer>(entity)
        .map(|m| m.mesh.clone())
        .expect("the entity has a mesh");
    let uv2 = world.get::<MeshUv1>(entity).map(|u| u.0.clone());
    let uv: &[[f32; 2]] = uv2.as_deref().map_or(&[], |u| u.as_slice());
    let mut runtime = world.resource_mut::<RuntimeRenderer>();
    frames(&mut runtime.0, &mesh, uv, entity.to_bits(), path)
}

/// Two frames of `mesh` in `renderer` (the second has every history settled), the last one's pixels.
fn frames(
    renderer: &mut FrameRenderer,
    mesh: &CpuMesh,
    uv1: &[[f32; 2]],
    object_key: u64,
    path: Path,
) -> Vec<u8> {
    let lab = lab();
    let base = lab.as_frame_scene_with_quality(quality(path));
    // A receiver mesh (it has a second UV set) and its source are two meshes.
    let mesh_key = if uv1.is_empty() { 0x0A1 << 3 } else { 0x0A2 << 3 };
    let scene = FrameScene {
        meshes: vec![FrameMesh {
            mesh_key,
            object_key,
            vertices: &mesh.vertices,
            indices: &mesh.indices,
            uv1,
            ..base.meshes[0]
        }],
        ..base.clone()
    };
    let mut last = Vec::new();
    for _ in 0..2 {
        last = match renderer.render_offscreen(device(), &scene) {
            Ok(frame) => frame.pixels,
            Err(err) => panic!("frame: {err:?}"),
        };
    }
    assert_eq!(last.len(), (W * H * 4) as usize);
    last
}

/// The frame with no lightmap at all: the moved floor in a renderer that holds only the volume, its
/// object key that of the entity it is compared with.
fn volume_only(object_key: u64, path: Path) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume()));
    frames(&mut renderer, &moved_floor(), &[], object_key, path)
}

fn mean_luma(img: &[u8]) -> f32 {
    let (x0, x1, y0, y1) = (W / 3, 2 * W / 3, H / 2, 3 * H / 4);
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum +=
                0.2126 * f32::from(img[i]) + 0.7152 * f32::from(img[i + 1]) + 0.0722 * f32::from(img[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn report(app: &App) -> LightmapReceiverReport {
    app.world().resource::<LightmapReceiverReport>().clone()
}

fn stats(app: &App) -> regia_render::lightmap::LightmapForwardStats {
    app.world()
        .resource::<RuntimeRenderer>()
        .0
        .lightmap_forward_stats()
}

/// The default keeps drawing a moved receiver with its baked light and says so only through its
/// freshness: today's behavior, unchanged.
#[test]
fn a_moved_receiver_keeps_its_baked_light_by_default() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        LightmapSettings::default().stale_receivers,
        StaleReceiverPolicy::KeepDrawing
    );
    for path in [Path::Cpu, Path::GpuDriven] {
        let (mut app, entity) = app(moved_floor(), StaleReceiverPolicy::KeepDrawing);
        let img = draw(&mut app, entity, path);
        let r = report(&app);
        assert_eq!((r.applied, r.stale_fallback), (1, 0), "{path:?}: {r:?}");
        assert!(r.errors.is_empty(), "{path:?}: {r:?}");
        let applied = app
            .world()
            .get::<AppliedLightmapReceiver>(entity)
            .expect("applied");
        assert_eq!(applied.freshness(), ReceiverFreshness::Stale);
        let s = stats(&app);
        assert_eq!((s.drawn, s.fallback), (1, 0), "{path:?}: {s:?}");

        let plain = volume_only(entity.to_bits(), path);
        assert!(
            mean_luma(&img) > mean_luma(&plain) + 20.0,
            "{path:?}: the lightmap lights the floor: {} vs {}",
            mean_luma(&img),
            mean_luma(&plain)
        );
    }
}

/// Under the fallback policy the moved receiver draws its own mesh with the frame's other light:
/// byte for byte the frame with no lightmap installed; it is reported, and no later frame hashes
/// the mesh again.
#[test]
fn a_moved_receiver_falls_back_to_the_volume_under_the_policy() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for path in [Path::Cpu, Path::GpuDriven] {
        let moved = moved_floor();
        let (mut app, entity) = app(moved.clone(), StaleReceiverPolicy::FallBack);
        let img = draw(&mut app, entity, path);
        let r = report(&app);
        assert_eq!(
            (r.receivers, r.applied, r.stale_fallback),
            (1, 0, 1),
            "{path:?}: {r:?}"
        );
        assert_eq!(r.errors.len(), 1, "{path:?}: {r:?}");
        assert_eq!(r.errors[0].code, LIGHTMAP_RECEIVER_STALE);
        assert_eq!(r.errors[0].entry, "floor");
        let world = app.world();
        assert!(world.get::<AppliedLightmapReceiver>(entity).is_none());
        assert!(world.get::<StaleLightmapFallback>(entity).is_some());
        assert!(world.get::<MeshUv1>(entity).is_none());
        let drawn = &world.get::<MeshRenderer>(entity).expect("mesh").mesh;
        assert!(Arc::ptr_eq(drawn, &moved), "the entity draws its own mesh");
        assert_eq!(stats(&app).drawn, 0, "{path:?}: no lightmap draw");
        let status = world.resource::<RuntimeRenderer>().0.baked_gi_status();
        assert!(status.active, "{path:?}: the volume lights the frame: {status:?}");

        let plain = volume_only(entity.to_bits(), path);
        assert_eq!(img, plain, "{path:?}: the frame the volume alone makes");

        // Frames after it: the same answer from the marker.
        let again = draw(&mut app, entity, path);
        assert_eq!(report(&app).stale_fallback, 1);
        assert_eq!(again, plain, "{path:?}: a later frame is the same");
    }
}

/// A receiver that did not move keeps its lightmap under the fallback policy.
#[test]
fn a_fresh_receiver_keeps_its_lightmap_under_the_fallback_policy() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for path in [Path::Cpu, Path::GpuDriven] {
        let (mut kept, entity) = app(floor(), StaleReceiverPolicy::KeepDrawing);
        let reference = draw(&mut kept, entity, path);
        let (mut app, entity) = app(floor(), StaleReceiverPolicy::FallBack);
        let img = draw(&mut app, entity, path);
        let r = report(&app);
        assert_eq!((r.applied, r.stale_fallback), (1, 0), "{path:?}: {r:?}");
        assert!(r.errors.is_empty(), "{path:?}: {r:?}");
        assert_eq!(stats(&app).drawn, 1, "{path:?}");
        assert_eq!(img, reference, "{path:?}: the policy touches only moved meshes");
    }
}

/// The policy changing at runtime moves an applied receiver off its lightmap and back.
#[test]
fn the_policy_switches_a_running_receiver_both_ways() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = Path::Cpu;
    let (mut app, entity) = app(moved_floor(), StaleReceiverPolicy::KeepDrawing);
    let kept = draw(&mut app, entity, path);
    assert_eq!(report(&app).applied, 1);

    app.world_mut().resource_mut::<LightmapSettings>().stale_receivers = StaleReceiverPolicy::FallBack;
    // The commands of the switching update take effect before its frame is drawn.
    let fallen = draw(&mut app, entity, path);
    let r = report(&app);
    assert_eq!((r.applied, r.stale_fallback), (0, 1), "{r:?}");
    assert!(app.world().get::<MeshUv1>(entity).is_none());
    assert_ne!(fallen, kept, "the frame moved off the lightmap");
    assert_eq!(stats(&app).drawn, 0);

    app.world_mut().resource_mut::<LightmapSettings>().stale_receivers = StaleReceiverPolicy::KeepDrawing;
    let back = draw(&mut app, entity, path);
    let r = report(&app);
    assert_eq!((r.applied, r.stale_fallback), (1, 0), "{r:?}");
    assert!(app.world().get::<StaleLightmapFallback>(entity).is_none());
    assert_eq!(stats(&app).drawn, 1);
    // Forward frames settle with their histories: the same light, not necessarily the same bytes.
    assert!(
        (mean_luma(&back) - mean_luma(&kept)).abs() < 2.0,
        "back on the lightmap: {} vs {}",
        mean_luma(&back),
        mean_luma(&kept)
    );
}

/// A file with no mesh hash cannot tell a moved mesh: it stays unverified and keeps drawing.
#[test]
fn an_unverified_receiver_keeps_drawing_under_the_fallback_policy() {
    let (mut app, entity) = app(moved_floor(), StaleReceiverPolicy::FallBack);
    let mut map = lightmap();
    map.source_hashes.clear();
    app.world_mut()
        .resource_mut::<InstalledLightmap>()
        .install("old", map);
    app.update();
    let r = report(&app);
    assert_eq!((r.applied, r.stale_fallback), (1, 0), "{r:?}");
    let applied = app
        .world()
        .get::<AppliedLightmapReceiver>(entity)
        .expect("applied");
    assert_eq!(applied.freshness(), ReceiverFreshness::Unverified);
}

/// The names `render.set_lightmaps` takes.
#[test]
fn the_policy_names_are_stable() {
    assert_eq!(StaleReceiverPolicy::KeepDrawing.name(), "keep_drawing");
    assert_eq!(StaleReceiverPolicy::FallBack.name(), "fall_back");
    for policy in StaleReceiverPolicy::ALL {
        assert_eq!(StaleReceiverPolicy::from_name(policy.name()), Some(policy));
    }
    assert_eq!(StaleReceiverPolicy::from_name("sometimes"), None);
}
