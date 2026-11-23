// SPDX-License-Identifier: MPL-2.0
//! Far rocks sit on the terrain the GPU draws (ADR-048's rendered surface), not on the true
//! heightfield: a coarse CDLOD level departs from the bilinear surface, and a rock placed by the
//! true height then floats above or sinks into the visible ground by that error.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, RockStyle, ScatterField, ScatterLibrary, ScatterSet, ScatterSource, ScatterView,
    TerrainLayers, TerrainMap, TerrainSharedDraw, procedural_rock,
};

const SET: &str = r#"ScatterSet(version: 2, seed: 9, classes: [
    ScatterClass(name: "boulders", meshes: [ScatterMeshRef(mesh: "rock", min_scale: 0.8, max_scale: 1.6)],
        cell_size: 6.0, density: 1.0, slope_max: 1.2, sink: 0.15, fade_start: 300.0, fade_end: 400.0),
])"#;

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn far_rocks_sit_on_the_drawn_surface() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    // Rough 1 km island, 2 m samples: coarse LODs at 150 m visibly cut its bumps.
    let n = 513;
    let heights = regia_terrain::synthetic_mountains(n, 1024.0, 70.0, 11);
    let map = TerrainMap::from_heights(n, n, heights, 1024.0, 1.0, 0.0).expect("map");
    let layers = TerrainLayers::default();
    let draw =
        TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 4).expect("terrain");
    let surface = draw.surface();
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));

    let eye_ground = map.sample_bilinear(0.0, 120.0);
    let eye = [0.0, eye_ground + 25.0, 120.0];
    let target = [0.0, map.sample_bilinear(0.0, -60.0), -60.0];
    let post = PostSettings {
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let lab = LabScene {
        width: 160,
        height: 90,
        eye,
        target,
        fov_y: 0.9,
        near: 0.1,
        far: 3000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.4, -0.6, -0.7], [1.0; 3], 90_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut scene = lab.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    // One frame selects the drawn nodes for this eye.
    renderer
        .render_offscreen(&device, &scene)
        .unwrap_or_else(|err| panic!("frame: {err}"));

    let set = ScatterSet::from_ron(SET, None).expect("set");
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock".into(),
        procedural_rock(3, RockStyle::angular([0.3, 0.2, 0.15])),
    );
    let mut field =
        ScatterField::new(&set, ScatterSource::new(Arc::new(map.clone())), &library, 7).expect("field");
    field.update(eye);
    let view = ScatterView::look_at(eye, target, 0.9, 16.0 / 9.0, 0.1, 3000.0);
    let mut plain: Vec<FrameMesh<'_>> = Vec::new();
    field.collect(&view, &mut plain);
    let plain: Vec<([f32; 4], u64)> = plain.iter().map(|m| (m.model[3], m.object_key)).collect();
    field.set_rendered_surface(Some(surface.clone()));
    let mut seated: Vec<FrameMesh<'_>> = Vec::new();
    let stats = field.collect(&view, &mut seated);
    assert_eq!(seated.len(), plain.len());

    let mut checked = 0;
    let mut worst_unseated = 0.0f32;
    for (mesh, (p, key)) in seated.iter().zip(&plain) {
        assert_eq!(mesh.object_key, *key);
        let d = ((p[0] - eye[0]).powi(2) + (p[2] - eye[2]).powi(2)).sqrt();
        if !(130.0..=170.0).contains(&d) {
            continue;
        }
        let x = p[0];
        let z = p[2];
        let truth = map.sample_bilinear(x, z);
        let drawn = surface.heights(&map, &[[x, z]]).expect("surface")[0];
        // The pivot moved by exactly (drawn − true) at the contact point (1 cm steps)...
        let moved = mesh.model[3][1] - p[1];
        worst_unseated = worst_unseated.max((drawn - truth).abs());
        // (the contact point is the instance's ground point; the pivot is sunk along its
        // tilted up axis, so x/z of the pivot are within a few cm of it)
        assert!(
            (moved - (drawn - truth)).abs() < 0.03,
            "rock at {d:.0} m moved {moved:.3} m, drawn − true = {:.3} m",
            drawn - truth
        );
        checked += 1;
    }
    eprintln!(
        "{checked} rocks at 130–170 m; largest drawn-vs-true gap {worst_unseated:.3} m; seated {} (max {} cm)",
        stats.seated, stats.seat_max_cm
    );
    assert!(checked > 20, "too few rocks at 150 m ({checked})");
    // Without seating these rocks would float or sink visibly.
    assert!(
        worst_unseated > 0.02,
        "the far LOD matches the true surface here ({worst_unseated} m)"
    );
}

/// Every fast path of `ScatterField` (background streaming, tile and block culling shortcuts,
/// the seat cache, parallel collect) emits exactly what the plain algorithm emits — same
/// meshes in the same order with bit-identical matrices — along a moving camera that crosses
/// LOD, morph-band and tile boundaries.
#[test]
fn fast_paths_emit_exactly_the_reference() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    // 0.5 m texels: the finest nodes are 16 m, so morph bands (112–128 m, 224–256 m) fall
    // inside the rocks' fade range.
    let n = 2049;
    // Rough ground (±0.6 m per sample): morphing between LODs moves the drawn surface by
    // decimetres, so a stale seat would show.
    let mut heights = regia_terrain::synthetic_mountains(n, 1024.0, 70.0, 11);
    for (k, h) in heights.iter_mut().enumerate() {
        let mut v = (k as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        v ^= v >> 29;
        *h += ((v % 1000) as f32 / 1000.0 - 0.5) * 1.2;
    }
    let map = TerrainMap::from_heights(n, n, heights, 1024.0, 1.0, 0.0).expect("map");
    let draw = TerrainSharedDraw::new(
        &device,
        map.clone(),
        &TerrainLayers::default(),
        CdlodConfig::default(),
        4,
    )
    .expect("terrain");
    let surface = draw.surface();
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    // Dense rocks, so the LOD morph bands hold many of them.
    let set = ScatterSet::from_ron(&SET.replace("cell_size: 6.0", "cell_size: 2.5"), None).expect("set");
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock".into(),
        procedural_rock(3, RockStyle::angular([0.3, 0.2, 0.15])),
    );
    let make =
        || ScatterField::new(&set, ScatterSource::new(Arc::new(map.clone())), &library, 7).expect("field");
    let mut fast = make();
    fast.set_rendered_surface(Some(surface.clone()));
    let mut reference = make();
    reference.set_reference_mode(true);
    reference.set_rendered_surface(Some(surface.clone()));
    let post = PostSettings {
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut compared = 0usize;
    let mut fast_seats = 0u32;
    for frame in 0..70 {
        // 3 m per frame along a diagonal, turning (tiles stream, nodes split, rocks cross
        // bands), then 0.25 m per frame (the chase's speed: the seat cache's slack path).
        let s = if frame < 30 {
            frame as f32 * 3.0
        } else {
            90.0 + (frame - 30) as f32 * 0.25
        };
        let (x, z) = (-60.0 + s * 0.8, 120.0 - s);
        let eye = [x, map.sample_bilinear(x, z) + 6.0 + (frame % 7) as f32, z];
        let yaw = 0.05 * frame as f32;
        let tx = x + 80.0 * yaw.sin();
        let tz = z - 80.0 * yaw.cos();
        let target = [tx, map.sample_bilinear(tx, tz), tz];
        let lab = LabScene {
            width: 96,
            height: 54,
            eye,
            target,
            fov_y: 0.9,
            near: 0.1,
            far: 3000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 8000.0,
            double_sided: false,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new([0.4, -0.6, -0.7], [1.0; 3], 90_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        // The frame publishes this eye's selection; both fields then seat against it.
        renderer
            .render_offscreen(&device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        fast.update(eye);
        reference.update(eye);
        let view = ScatterView::look_at(eye, target, 0.9, 16.0 / 9.0, 0.1, 3000.0);
        for parallel in [true, false] {
            fast.set_parallel_collect(parallel);
            let mut a: Vec<FrameMesh<'_>> = Vec::new();
            let mut b: Vec<FrameMesh<'_>> = Vec::new();
            let sa = fast.collect(&view, &mut a);
            fast_seats += sa.seat_fast;
            let sb = reference.collect(&view, &mut b);
            assert_eq!(sa.emitted, sb.emitted, "frame {frame}");
            assert_eq!(a.len(), b.len(), "frame {frame}");
            for (m, r) in a.iter().zip(&b) {
                assert_eq!(
                    (m.mesh_key, m.material_key, m.object_key, m.shadow),
                    (r.mesh_key, r.material_key, r.object_key, r.shadow),
                    "frame {frame}"
                );
                let bits = |x: &[[f32; 4]; 4]| x.map(|c| c.map(f32::to_bits));
                assert_eq!(
                    bits(&m.model),
                    bits(&r.model),
                    "frame {frame} key {:x}",
                    m.object_key
                );
            }
            compared += a.len();
        }
    }
    eprintln!("{compared} emitted instances compared ({fast_seats} seats from the slack path)");
    assert!(
        fast_seats > 10_000,
        "the slack path was not exercised ({fast_seats})"
    );
    assert!(compared > 20_000, "too little compared ({compared})");
}

/// Rocks are not drawn over a hole cut through the drawn surface (ADR-088: no rocks floating
/// over a tunnel mouth), and the surface answers NaN there.
#[test]
fn no_rocks_over_a_hole() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let n = 513;
    let heights = regia_terrain::synthetic_mountains(n, 1024.0, 70.0, 11);
    let map = TerrainMap::from_heights(n, n, heights, 1024.0, 1.0, 0.0).expect("map");
    let draw = TerrainSharedDraw::new(
        &device,
        map.clone(),
        &TerrainLayers::default(),
        CdlodConfig::default(),
        4,
    )
    .expect("terrain");
    let surface = draw.surface();
    let holes = draw.holes_handle();
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));

    let eye = [0.0, map.sample_bilinear(0.0, 60.0) + 30.0, 60.0];
    let target = [0.0, map.sample_bilinear(0.0, -40.0), -40.0];
    let post = PostSettings {
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let lab = LabScene {
        width: 160,
        height: 90,
        eye,
        target,
        fov_y: 0.9,
        near: 0.1,
        far: 3000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.4, -0.6, -0.7], [1.0; 3], 90_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut scene = lab.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    let frame = |renderer: &mut FrameRenderer| {
        renderer
            .render_offscreen(&device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    };
    frame(&mut renderer);

    let set = ScatterSet::from_ron(SET, None).expect("set");
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock".into(),
        procedural_rock(3, RockStyle::angular([0.3, 0.2, 0.15])),
    );
    let mut field =
        ScatterField::new(&set, ScatterSource::new(Arc::new(map.clone())), &library, 7).expect("field");
    field.update(eye);
    field.set_rendered_surface(Some(surface.clone()));
    let view = ScatterView::look_at(eye, target, 0.9, 16.0 / 9.0, 0.1, 3000.0);
    // A 25 m × 20 m mouth 40 m in front of the camera.
    let mouth = regia_terrain::HoleShape::segment([0.0, -12.5], [0.0, 12.5], 20.0);
    let inside = |m: &FrameMesh<'_>| mouth.contains(m.model[3][0], m.model[3][2]);
    let mut uncut: Vec<FrameMesh<'_>> = Vec::new();
    field.collect(&view, &mut uncut);
    let before = uncut.iter().filter(|m| inside(m)).count();
    drop(uncut);
    assert!(before > 3, "the test needs rocks in the mouth ({before})");

    holes.request(regia_terrain::HolesMask::from_shapes(vec![mouth.clone()]).expect("mask"));
    frame(&mut renderer);
    assert!(
        !holes.state().applied.is_empty(),
        "the draw applied the requested mask"
    );
    let mut cut: Vec<FrameMesh<'_>> = Vec::new();
    field.collect(&view, &mut cut);
    let after = cut.iter().filter(|m| inside(m)).count();
    eprintln!(
        "rocks in the mouth: {before} uncut, {after} cut; {} drawn",
        cut.len()
    );
    assert_eq!(after, 0, "rocks float over the hole");
    assert!(surface.heights(&map, &[[0.0, 0.0]]).expect("surface")[0].is_nan());
    assert!(surface.heights(&map, &[[30.0, 0.0]]).expect("surface")[0].is_finite());
}
