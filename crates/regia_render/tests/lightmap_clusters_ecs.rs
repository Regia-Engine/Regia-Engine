// SPDX-License-Identifier: MPL-2.0
//! A lightmap receiver with baked clusters reaches the visibility buffer through the app's own
//! systems (ADR-2107): `apply_lightmap_receivers`, `extract_frame` and a frame, with no meshlet
//! builder installed, as in a shipped game. The unwrap has a seam, so the source mesh's clusters
//! could not fit it. Needs a headless device (lavapipe in CI).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_asset_pipeline::lightmap_uv::{SourceAttributes, meshlets_for_unwrap};
use regia_render::lightmap::{
    InstalledLightmap, LightmapReceiver, LightmapReceiverReport, LightmapSettings, apply_lightmap_receivers,
};
use regia_render::{
    Camera, CpuMesh, ExtractedFrame, LabMaterial, LabVertex, MeshRenderer, RenderDevice, RenderSettings,
    RuntimeRenderer, extract_frame,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 64;
const H: u32 = 64;
/// Quads per side of each chart.
const N: u32 = 12;
/// One test at a time: the frame-path trace counts the whole process's creates.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A flat strip of `2 N × N` quads facing +Z, and its unwrap into two charts of different light:
/// the column between them is split. Black texels between the charts catch a triangle across them.
fn strip_and_map() -> (Arc<CpuMesh>, Lightmap) {
    let columns = 2 * N + 1;
    let n = [0.0, 0.0, 1.0];
    let mut vertices = Vec::new();
    for j in 0..=N {
        for i in 0..columns {
            let (u, v) = (i as f32 / (2 * N) as f32, j as f32 / N as f32);
            vertices.push(LabVertex::from_pos_normal_uv(
                [u * 2.0 - 1.0, 2.0 * v - 1.0, 0.0],
                n,
                [u, v],
            ));
        }
    }
    let quad = |out: &mut Vec<u32>, at: &dyn Fn(u32, u32) -> u32, i: u32, j: u32| {
        out.extend([
            at(i, j),
            at(i + 1, j),
            at(i + 1, j + 1),
            at(i, j),
            at(i + 1, j + 1),
            at(i, j + 1),
        ]);
    };
    let source_at = |i: u32, j: u32| j * columns + i;
    let mut source_indices = Vec::new();
    for chart in 0..2 {
        for j in 0..N {
            for i in 0..N {
                quad(&mut source_indices, &source_at, i + chart * N, j);
            }
        }
    }
    let row = N + 1;
    let (mut xref, mut uv, mut indices) = (Vec::new(), Vec::new(), Vec::new());
    for chart in 0..2u32 {
        for j in 0..=N {
            for i in 0..=N {
                xref.push(j * columns + i + chart * N);
                let left = if chart == 0 { 0.0625 } else { 0.5625 };
                uv.push([
                    left + i as f32 / N as f32 * 0.375,
                    0.125 + j as f32 / N as f32 * 0.75,
                ]);
            }
        }
        let at = |i: u32, j: u32| chart * row * row + j * row + i;
        for j in 0..N {
            for i in 0..N {
                quad(&mut indices, &at, i, j);
            }
        }
    }
    let (width, height) = (32u32, 8u32);
    let irradiance: Vec<[f32; 3]> = (0..width * height)
        .map(|t| match t % width {
            15 | 16 => [0.0; 3],
            0..=14 => [2_000.0; 3],
            _ => [20_000.0; 3],
        })
        .collect();
    let map = Lightmap::from_irradiance(
        LightmapLayout {
            width,
            height,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "strip".into(),
                source_vertices: vertices.len() as u32,
                rect: [0, 0, width, height],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 2,
                area_m2: 4.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap { xref, uv, indices }],
        },
        &irradiance,
        vec![Coverage::BAKED; (width * height) as usize],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap");
    (
        Arc::new(CpuMesh {
            vertices,
            indices: source_indices,
        }),
        map,
    )
}

/// The clusters `regia bake-lightmaps --meshlets` stores for `map`'s entry over `mesh`.
fn with_clusters(mut map: Lightmap, mesh: &CpuMesh) -> Lightmap {
    let positions: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| v.uv).collect();
    let source = SourceAttributes {
        positions: &positions,
        normals: &normals,
        uvs: &uvs,
    };
    let clusters = meshlets_for_unwrap(source, &map.unwraps[0]).expect("clusters");
    map.meshlets = vec![clusters.to_bytes()];
    map
}

/// The frame the draw system read back.
#[derive(Resource, Default)]
struct Drawn(Vec<u8>);

fn draw(
    device: Res<RenderDevice>,
    extracted: Res<ExtractedFrame>,
    mut renderer: ResMut<RuntimeRenderer>,
    mut drawn: ResMut<Drawn>,
) {
    let scene = extracted.frame_scene(0, W, H);
    drawn.0 = renderer
        .0
        .render_offscreen(&device, &scene)
        .unwrap_or_else(|e| panic!("frame: {e}"))
        .pixels;
}

fn app(mesh: &Arc<CpuMesh>, map: Lightmap, visibility_buffer: bool) -> App {
    let mut app = App::new();
    app.insert_resource(device());
    app.init_resource::<RuntimeRenderer>();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<Drawn>();
    app.init_resource::<LightmapReceiverReport>();
    let mut settings = RenderSettings::default();
    settings.profile.visibility_buffer = visibility_buffer;
    app.insert_resource(settings);
    app.insert_resource(LightmapSettings {
        enabled: true,
        bicubic: false,
        ..LightmapSettings::default()
    });
    let mut installed = InstalledLightmap::default();
    installed.install("test", map);
    app.insert_resource(installed);
    let material = Arc::new(LabMaterial {
        metallic: 0.0,
        roughness: 1.0,
        ..LabMaterial::default()
    });
    let at = Transform::default();
    app.world_mut().spawn((
        at,
        GlobalTransform(at.compute_affine()),
        MeshRenderer::new(mesh.clone(), material),
        LightmapReceiver {
            entry: "strip".into(),
        },
    ));
    let eye = {
        use regia_core::math::{Quat, Vec3};
        Transform {
            translation: Vec3::new(0.0, 0.0, 3.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    };
    app.world_mut()
        .spawn((eye, GlobalTransform(eye.compute_affine()), Camera::default()));
    app.add_systems(Render, (apply_lightmap_receivers, extract_frame).chain());
    app
}

/// [`app`] with a frame drawn at the end of every update.
fn drawing_app(mesh: &Arc<CpuMesh>, map: Lightmap, visibility_buffer: bool) -> App {
    let mut app = app(mesh, map, visibility_buffer);
    app.add_systems(Render, draw.after(extract_frame));
    app
}

fn luma(img: &[u8], x: u32, y: u32) -> u32 {
    let i = ((y * W + x) * 4) as usize;
    u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2])
}

/// With no builder, the bake's clusters put a seamed receiver in the visibility buffer with the
/// light it has on the forward path. Without clusters it stays out of it and is counted, and its
/// light does not change either.
#[test]
fn a_receiver_with_baked_clusters_draws_in_the_visibility_buffer_with_no_builder() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mesh, plain) = strip_and_map();
    let clustered = with_clusters(plain.clone(), &mesh);

    let mut forward = drawing_app(&mesh, clustered.clone(), false);
    for _ in 0..3 {
        forward.update();
    }
    let reference = forward.world().resource::<Drawn>().0.clone();
    let report = forward.world().resource::<LightmapReceiverReport>();
    assert_eq!((report.applied, report.clusters, report.no_clusters), (1, 0, 0));
    let (left, right) = (
        luma(&reference, W / 2 - 6, H / 2),
        luma(&reference, W / 2 + 6, H / 2),
    );
    assert!(
        right > left + 100,
        "the frame must show both charts: left {left}, right {right}"
    );

    let mut vis = drawing_app(&mesh, clustered, true);
    for _ in 0..3 {
        vis.update();
    }
    let report = vis.world().resource::<LightmapReceiverReport>();
    assert_eq!((report.applied, report.clusters, report.no_clusters), (1, 1, 0));
    let renderer = &vis.world().resource::<RuntimeRenderer>().0;
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(
        (stats.visbuffer, stats.fallback, stats.unsupported),
        (1, 0, 0),
        "{stats:?}"
    );
    assert_eq!(renderer.visbuffer_stats().instances, 1);
    let img = &vis.world().resource::<Drawn>().0;
    let worst = img
        .iter()
        .zip(&reference)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(worst <= 6, "the visibility frame differs from forward by {worst}");

    let mut bare = drawing_app(&mesh, plain, true);
    for _ in 0..3 {
        bare.update();
    }
    let report = bare.world().resource::<LightmapReceiverReport>();
    assert_eq!((report.applied, report.clusters, report.no_clusters), (1, 0, 1));
    let renderer = &bare.world().resource::<RuntimeRenderer>().0;
    let stats = renderer.lightmap_forward_stats();
    assert_eq!((stats.drawn, stats.visbuffer), (1, 0), "{stats:?}");
    assert_eq!(renderer.visbuffer_stats().instances, 0);
    let img = &bare.world().resource::<Drawn>().0;
    let worst = img
        .iter()
        .zip(&reference)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(worst <= 6, "the batches differ from forward by {worst}");
}

/// The game's order within one tick: the receivers apply, the frame is extracted, and a warm of that
/// frame runs (`GameFrameWarm` runs inside the window render, after these). The frames after it draw
/// the receiver at once and create no pipeline and no lightmap table, on the forward and the
/// visibility paths.
#[test]
fn a_warm_of_an_extracted_frame_covers_its_receivers_on_both_paths() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mesh, plain) = strip_and_map();
    let clustered = with_clusters(plain, &mesh);
    for visibility_buffer in [false, true] {
        let mut app = app(&mesh, clustered.clone(), visibility_buffer);
        app.update();
        let mut renderer = app
            .world_mut()
            .remove_resource::<RuntimeRenderer>()
            .expect("renderer");
        {
            let world = app.world();
            renderer
                .0
                .warm_extracted(
                    world.resource::<RenderDevice>(),
                    world.resource::<ExtractedFrame>(),
                    wgpu::TextureFormat::Rgba8Unorm,
                    true,
                )
                .unwrap_or_else(|e| panic!("warm: {e}"));
        }
        app.world_mut().insert_resource(renderer);
        app.add_systems(Render, draw.after(extract_frame));
        regia_render::trace_frame_path(true);
        let _ = regia_render::take_frame_path_events();
        for frame in 0..3 {
            app.update();
            let stats = app
                .world()
                .resource::<RuntimeRenderer>()
                .0
                .lightmap_forward_stats();
            assert_eq!(
                (stats.drawn, stats.fallback, stats.visbuffer),
                (1, 0, usize::from(visibility_buffer)),
                "visibility {visibility_buffer}, frame {frame}: {stats:?}"
            );
        }
        let created: Vec<String> = regia_render::take_frame_path_events()
            .into_iter()
            .filter(|e| e.kind == regia_render::FramePathKind::Pipeline || e.label.contains("lightmap"))
            .map(|e| e.label)
            .collect();
        regia_render::trace_frame_path(false);
        assert!(
            created.is_empty(),
            "visibility {visibility_buffer}: frames after the warm created {created:?}"
        );
    }
}
