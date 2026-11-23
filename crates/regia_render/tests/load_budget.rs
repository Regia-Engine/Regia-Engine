// SPDX-License-Identifier: MPL-2.0
//! The per-frame load budget (ADR-7884, DO27 tester item first-frame-stall part 2): with
//! `LateDraw::Defer`, a burst of late meshes is uploaded over several frames (at most the byte cap
//! plus one mesh per frame) and a burst of new material-graph pipeline keys is built off the frame
//! thread, the draws waiting meanwhile; the default budget changes nothing.
//!
//! Counters only (lavapipe; no wall clock): mesh bytes per frame (`upload_split`, process-wide, so
//! every test here holds `SERIAL_GPU`), frame-thread pipeline creations and blocking polls
//! (`frame_path_counters`, this thread), draws and cache entries.

use std::sync::Arc;

use regia_render::{
    CacheResidency, DirectionalLightUniform, FrameLoadBudget, FrameMesh, FrameRenderer, FrameScene,
    GraphCode, LabMaterial, LabMesh, LabScene, LabVertex, LateDraw, RenderDevice, SurfaceGraph,
    frame_path_counters, upload_split,
};

/// One device user at a time: `upload_split` is process-wide, and concurrent device churn can
/// live-lock the NVIDIA driver (ADR-1438). Declared first in each test so the device drops while held.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The late burst: 150 distinct meshes of about 2 MiB each (300 MiB).
const BURST_MESHES: usize = 150;
/// Vertices per burst mesh: 21,845 x 96 bytes + 24 index bytes is just under 2 MiB.
const BURST_VERTICES: usize = 21_845;
/// The budget the brief names: 64 MiB per frame.
const CAP: u64 = 64 << 20;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A one-quad scene; its first mesh is the template every test mesh copies.
fn lab() -> LabScene {
    let quad = LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.1, -0.1, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.1, -0.1, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.1, 0.1, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-0.1, 0.1, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    };
    LabScene {
        width: 32,
        height: 32,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![quad],
    }
}

/// The quad's corners repeated to `BURST_VERTICES` (the extra vertices are unreferenced and keep
/// the bound): one slice every burst mesh shares under its own key.
fn burst_vertices(quad: &[LabVertex]) -> Vec<LabVertex> {
    (0..BURST_VERTICES).map(|i| quad[i % quad.len()]).collect()
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// `base` plus the burst: mesh `i` at a distance growing with `i` (a 10 x 15 grid receding from
/// the eye), keys and objects of its own, all sharing `vertices`.
fn burst<'a>(base: &FrameScene<'a>, vertices: &'a [LabVertex]) -> FrameScene<'a> {
    let template = base.meshes[0];
    let mut meshes = vec![template];
    meshes.extend((0..BURST_MESHES).map(|i| FrameMesh {
        mesh_key: 0x0B0D_6E70_0000 + ((i as u64 + 1) << 3),
        object_key: 0x0B1E_0000 + i as u64 + 1,
        vertices,
        model: translate(
            (i % 10) as f32 * 0.25 - 1.1,
            ((i / 10) % 3) as f32 * 0.25 - 0.25,
            -((i / 30) as f32) * 2.0,
        ),
        ..template
    }));
    FrameScene {
        meshes,
        ..base.clone()
    }
}

fn mesh_bytes(mesh: &FrameMesh<'_>) -> u64 {
    (std::mem::size_of_val(mesh.vertices) + std::mem::size_of_val(mesh.indices)) as u64
}

/// A renderer whose cache holds the whole burst (no eviction in the test) and that drew `base`
/// three times, so its first-frame uploads and pipelines are behind it.
fn warmed(device: &RenderDevice, base: &FrameScene<'_>, budget: FrameLoadBudget) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_cache_residency(CacheResidency {
        budget_bytes: 4 << 30,
        ..CacheResidency::default()
    });
    renderer.set_load_budget(budget);
    for _ in 0..3 {
        renderer.render_offscreen(device, base).expect("warm-up frame");
    }
    renderer
}

/// One frame drawn like the window path (`begin_offscreen_live`): `(mesh bytes uploaded,
/// frame-thread pipelines, blocking polls)` of the recording and submit. The readback that follows
/// (a capture's blocking wait, not the frame's) is outside the counted window.
fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> (u64, u64, u64) {
    let (mesh_before, counters_before) = (upload_split().mesh_bytes, frame_path_counters());
    let target = renderer
        .begin_offscreen_live(device, scene, wgpu::TextureFormat::Rgba8Unorm)
        .expect("frame");
    let counters = frame_path_counters().since(counters_before);
    let bytes = upload_split().mesh_bytes - mesh_before;
    renderer.read_offscreen(device, target).expect("readback");
    (bytes, counters.pipelines, counters.blocking_polls)
}

#[test]
fn a_300_mib_burst_under_a_64_mib_defer_budget_uploads_over_frames_and_loses_no_draw() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();
    let vertices = burst_vertices(base.meshes[0].vertices);
    let scene = burst(&base, &vertices);
    let one_mesh = mesh_bytes(&scene.meshes[1]);
    let total: u64 = scene.meshes[1..].iter().map(mesh_bytes).sum();
    assert!(total >= 299 << 20, "the burst is about 300 MiB: {total}");

    // Block (the default): the first burst frame uploads all of it.
    let mut block = warmed(&device, &base, FrameLoadBudget::default());
    let (block_bytes, _, _) = frame(&mut block, &device, &scene);
    assert_eq!(
        block_bytes, total,
        "Block uploads the whole burst on its first frame"
    );
    frame(&mut block, &device, &scene);
    let block_drawn = block.draw_stats().drawn;
    assert!(
        block_drawn > BURST_MESHES / 2,
        "most of the burst is in view: {block_drawn}"
    );
    drop(block);

    // Defer: at most the cap plus one mesh per frame, nearest first, all resident after
    // ceil(300 / 64) + 1 frames, then the same draws as Block.
    let budget = FrameLoadBudget {
        upload_bytes_per_frame: Some(CAP),
        new_pipelines_per_frame: None,
        late: LateDraw::Defer,
    };
    let mut defer = warmed(&device, &base, budget);
    let frames = total.div_ceil(CAP) + 1;
    let mut uploaded = 0u64;
    let mut resident_at = None;
    for n in 1..=frames {
        let (bytes, _, _) = frame(&mut defer, &device, &scene);
        let report = defer.load_budget_report();
        eprintln!(
            "frame {n}: {bytes} B uploaded, {report:?}, drawn {}",
            defer.draw_stats().drawn
        );
        assert!(
            bytes <= CAP + one_mesh,
            "frame {n} uploaded {bytes} B, over the {CAP} B cap plus one {one_mesh} B mesh"
        );
        assert_eq!(
            report.admitted_bytes, bytes,
            "the plan's bytes are the frame's mesh uploads"
        );
        uploaded += bytes;
        if resident_at.is_none() && report.deferred_meshes == 0 {
            resident_at = Some(n);
        }
        if n == 1 {
            assert!(
                report.deferred_meshes > 0 && report.deferred_draws > 0,
                "{report:?}"
            );
            assert_eq!(report.bursts, 0, "a burst is counted when the next frame starts");
        }
    }
    assert_eq!(uploaded, total, "every burst mesh uploaded exactly once");
    assert!(
        resident_at.is_some_and(|n| n <= frames),
        "resident within {frames} frames: {resident_at:?}"
    );
    assert_eq!(defer.cache_stats().meshes, BURST_MESHES + 1);
    for _ in 0..2 {
        let (bytes, _, _) = frame(&mut defer, &device, &scene);
        assert_eq!(bytes, 0, "nothing left to upload");
        assert_eq!(
            defer.draw_stats().drawn,
            block_drawn,
            "no draw lost after the burst"
        );
        assert_eq!(defer.load_budget_report().deferred_draws, 0);
    }
    assert_eq!(defer.load_budget_report().bursts, 1, "one burst, logged once");
}

#[test]
fn the_nearest_meshes_of_a_burst_are_uploaded_first() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();
    let vertices = burst_vertices(base.meshes[0].vertices);
    let mut scene = burst(&base, &vertices);
    // Submission order farthest first: the plan still takes the nearest.
    scene.meshes[1..].reverse();
    let mut renderer = warmed(
        &device,
        &base,
        FrameLoadBudget {
            upload_bytes_per_frame: Some(CAP),
            new_pipelines_per_frame: None,
            late: LateDraw::Defer,
        },
    );
    frame(&mut renderer, &device, &scene);
    let admitted = renderer.load_budget_report().admitted_meshes as usize;
    assert!(admitted > 0 && admitted < BURST_MESHES);
    // The burst's nearest rows are z = 0 (i < 30) and z = -2 (i < 60): with 32 meshes a frame the
    // first frame takes the 30 at z = 0 and 2 at z = -2, never one farther back.
    let resident = renderer.cache_stats().meshes - 1;
    assert_eq!(resident, admitted);
    // A second plan with the same budget continues with the next-nearest.
    frame(&mut renderer, &device, &scene);
    assert_eq!(
        renderer.cache_stats().meshes - 1,
        admitted * 2,
        "{:?}",
        renderer.load_budget_report()
    );
    let near: Vec<FrameMesh<'_>> = scene.meshes[1..]
        .iter()
        .copied()
        .filter(|mesh| mesh.model[3][2] > -1.0)
        .collect();
    let near_scene = FrameScene {
        meshes: near,
        ..base.clone()
    };
    let (bytes, _, _) = frame(&mut renderer, &device, &near_scene);
    assert_eq!(bytes, 0, "the nearest row went first: already resident");
}

/// Graph `i`'s code: a flat surface whose albedo differs per `i`, so each is its own key.
fn graph_code(i: usize) -> String {
    format!(
        "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {{\n    \
         return regia::pbr::surface::Surface(vec3<f32>({:.4}, 0.8, 0.8), 1.0, 0.0, 1.0, in.world_normal, 1.0, \
         vec3<f32>(0.0), 0.0);\n}}\n",
        0.1 + i as f32 / 100.0
    )
}

#[test]
fn forty_new_graph_keys_under_an_8_pipeline_defer_budget_build_off_the_frame_thread() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();
    let materials: Vec<LabMaterial> = (0..40)
        .map(|i| LabMaterial {
            surface_graph: Some(Arc::new(SurfaceGraph::from_code(&GraphCode {
                surface: graph_code(i).into(),
                vertex: None,
                uv1: false,
            }))),
            ..LabMaterial::default()
        })
        .collect();
    let template = base.meshes[0];
    let mut meshes = vec![template];
    meshes.extend(materials.iter().enumerate().map(|(i, material)| FrameMesh {
        material,
        material_key: 0x6A7_0000 + i as u64 + 1,
        object_key: 0x0B1E_1000 + i as u64 + 1,
        model: translate((i % 8) as f32 * 0.25 - 0.9, (i / 8) as f32 * 0.25 - 0.5, 0.0),
        ..template
    }));
    let scene = FrameScene {
        meshes,
        ..base.clone()
    };
    let mut renderer = warmed(
        &device,
        &base,
        FrameLoadBudget {
            upload_bytes_per_frame: None,
            new_pipelines_per_frame: Some(8),
            late: LateDraw::Defer,
        },
    );
    let (_, pipelines, polls) = frame(&mut renderer, &device, &scene);
    let report = renderer.load_budget_report();
    eprintln!("frame 1: {pipelines} pipelines, {polls} polls, {report:?}");
    assert!(
        pipelines <= 8,
        "frame 1 created {pipelines} pipelines on the frame thread (cap 8)"
    );
    assert_eq!(polls, 0, "no blocking poll on the frame thread");
    assert_eq!(
        report.async_graph_sets, 40,
        "every set went to the workers: {report:?}"
    );
    assert_eq!(report.deferred_draws, 40, "the graph draws wait: {report:?}");
    assert_eq!(renderer.draw_stats().drawn, 1, "the plain quad still draws");
    // The workers finish (the test waits off the frame path); the next frames draw everything.
    renderer.settle_material_graphs();
    for n in 2..=3 {
        let (_, pipelines, polls) = frame(&mut renderer, &device, &scene);
        let stats = renderer.draw_stats();
        let report = renderer.load_budget_report();
        eprintln!("frame {n}: {pipelines} pipelines, {polls} polls, {stats:?}, {report:?}");
        assert!(pipelines <= 8, "frame {n}: {pipelines} pipelines (cap 8)");
        assert_eq!(polls, 0, "frame {n}: no blocking poll");
        assert_eq!(stats.drawn, 41, "frame {n}: every draw is back");
        assert_eq!(
            stats.graph_instanced + stats.graph_per_object,
            40,
            "drawn with their graphs"
        );
        assert_eq!(report.deferred_draws, 0);
    }
    assert_eq!(renderer.material_graph_stats().built_on_frame_path, 0);
}

#[test]
fn the_default_budget_and_block_with_caps_change_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();
    let vertices = burst_vertices(base.meshes[0].vertices);
    let scene = burst(&base, &vertices);
    let total: u64 = scene.meshes[1..].iter().map(mesh_bytes).sum();
    let mut runs = Vec::new();
    for budget in [
        FrameLoadBudget::default(),
        FrameLoadBudget {
            late: LateDraw::Block,
            ..FrameLoadBudget::RECOMMENDED_60FPS
        },
    ] {
        let mut renderer = warmed(&device, &base, budget);
        let mut per_frame = Vec::new();
        for _ in 0..3 {
            // Blocking polls are left out: two default renderers on the same 300 MiB frame differ
            // there (1 vs 2 on lavapipe at the base, 59ff4b61d), so they are no behavior signal.
            let (bytes, _, _) = frame(&mut renderer, &device, &scene);
            let stats = renderer.draw_stats();
            per_frame.push((bytes, stats.drawn, stats.culled, renderer.cache_stats().meshes));
            assert_eq!(renderer.load_budget_report().deferred_draws, 0);
            assert_eq!(renderer.load_budget_report().deferred_meshes, 0);
        }
        assert_eq!(per_frame[0].0, total, "everything on the first frame, as before");
        runs.push(per_frame);
    }
    assert_eq!(runs[0], runs[1], "Block with caps is the default");
}
