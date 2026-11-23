// SPDX-License-Identifier: MPL-2.0
//! DO27 contract § 7.4 acceptance (spec 25 § 7.11 style; ADR-7585): 20 crater edits over 200
//! fixed steps, pushed through `TerrainHeightEdits` in integer millimetres, on a synthetic map
//! on lavapipe (no DO27 data).
//!
//! Checked per step, by counters and the CPU mirror of the vertex shader, never by wall clock:
//! - no step applies more samples than the patch budget;
//! - no crack at node (tile) or LOD edges: after every applied band, the selection around the
//!   eye passes `lod_seam_report` on a mirror map that took the same bands (the GPU vertex
//!   shader's rule, as in `runtime_patch`'s crater test);
//! - each edit box invalidates only the cascades it lands in: a one-sample edit 2 km from the
//!   camera re-keys no cascade and the renderer redraws no terrain cascade for it, a crater at
//!   the camera's feet re-keys the near cascades;
//! - the virtual texture tests one rectangle per changed band, each inside its edit box (grown
//!   by the texture's height reach and one sample), and never merges them.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::cdlod::{MinMaxPyramid, select_nodes};
use regia_terrain::gpu::vt::HEIGHT_REACH_M;
use regia_terrain::height_mips::HeightMips;
use regia_terrain::lod_seams::lod_seam_report;
use regia_terrain::{
    CdlodConfig, HeightEdit, LayerDesc, LayerTextures, MAX_EDIT_SIDE, MaterialParams, PatchTarget,
    Rgba8Image, TerrainHeightEdits, TerrainLayers, TerrainMap, TerrainMaterial, TerrainSharedDraw,
    VirtualTextureSettings,
};

const W: u32 = 128;
const H: u32 = 72;
/// 2 km, 4 m samples.
const N: u32 = 513;
const WORLD_M: f32 = 2048.0;
const STEPS: u32 = 200;
/// Samples written per frame: a 13 × 13 crater fits; the 25 × 25 ones spread over frames.
const BUDGET: u32 = 400;
/// The camera, near the −X −Z corner, looking toward the centre.
const EYE: [f32; 3] = [-700.0, 30.0, -700.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

fn world_of(sample: u32) -> f32 {
    sample as f32 / (N - 1) as f32 * WORLD_M - WORLD_M * 0.5
}

fn sample_of(world: f32) -> u32 {
    (((world + WORLD_M * 0.5) / WORLD_M * (N - 1) as f32).round() as u32).min(N - 1)
}

/// Rolling hills, on the 16-bit grid a game loads.
fn map() -> TerrainMap {
    let mut heights = Vec::with_capacity((N * N) as usize);
    for row in 0..N {
        for col in 0..N {
            let (x, z) = (world_of(col), world_of(row));
            heights.push(6.0 * (x / 97.0).sin() * (z / 131.0).cos() + 2.0 * (x / 23.0 + z / 37.0).sin());
        }
    }
    let shaped = TerrainMap::from_heights(N, N, heights, WORLD_M, 64.0, -32.0).expect("map");
    TerrainMap::new(&shaped.to_heightfield(), WORLD_M, 64.0, -32.0).expect("16-bit map")
}

/// Edit `i` of 20: where, how big (samples a side) and how deep (m). Every fourth is one
/// sample 1 m down near the far +X +Z corner (2.2 km from the camera, past its 1 km far plane,
/// so past every cascade); the others alternate a 13 × 13 and a 25 × 25 bowl walking toward
/// the camera, edit 18 at its feet.
fn edit_spec(i: u32) -> ([f32; 2], u32, f32) {
    if i % 4 == 3 {
        return ([900.0 - i as f32 * 4.0, 880.0], 1, 1.0);
    }
    let t = (i as f32 / 18.0).min(1.0);
    let centre = [300.0 - 1000.0 * t, 250.0 - 950.0 * t];
    let side = if i.is_multiple_of(2) { 13 } else { 25 };
    (centre, side, 3.0 + (i % 3) as f32)
}

/// Edit `i` against the current heights of `mirror`: absolute millimetres, a bowl dug into it.
fn edit(mirror: &TerrainMap, i: u32) -> HeightEdit {
    let ([cx, cz], side, depth) = edit_spec(i);
    let (c0, r0) = (sample_of(cx) - side / 2, sample_of(cz) - side / 2);
    let radius = side as f32 * 0.5;
    let mut heights_mm = Vec::with_capacity((side * side) as usize);
    for row in r0..r0 + side {
        for col in c0..c0 + side {
            let base = mirror.heights()[(row * N + col) as usize];
            let (dc, dr) = (
                col as f32 - (c0 as f32 + radius - 0.5),
                row as f32 - (r0 as f32 + radius - 0.5),
            );
            let d = ((dc * dc + dr * dr).sqrt() / radius).min(1.0);
            let dig = if side == 1 { depth } else { depth * (1.0 - d * d) };
            heights_mm.push(((base - dig) * 1000.0).round() as i32);
        }
    }
    HeightEdit {
        col: c0,
        row: r0,
        width: side,
        height: side,
        heights_mm,
    }
}

fn draw(map: TerrainMap) -> (TerrainSharedDraw, bool) {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.5, 0.45, 0.4],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 4).expect("terrain");
    // The virtual texture serves the photoreal material path only.
    let v = regia_terrain::linear_to_srgb8(0.4);
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    let vt = device()
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST);
    if vt {
        draw.set_virtual_texture(Some(VirtualTextureSettings {
            lod0_texel_m: 0.2,
            page_texels: 30,
            atlas_slots: 24,
            near_m: 0.0,
            bakes_per_frame: 64,
            rebakes_per_frame: 0,
            stochastic_lod: false,
            ..VirtualTextureSettings::default()
        }))
        .expect("virtual texture");
    } else {
        eprintln!("SKIP (virtual texture part): the device lacks SHADER_EARLY_DEPTH_TEST");
    }
    (draw, vt)
}

#[test]
fn twenty_crater_edits_stay_in_budget_crack_free_and_local() {
    let map = map();
    let (draw, vt) = draw(map.clone());
    let stats = draw.stats();
    let patches = draw.height_patches_handle();
    patches.set_budget(BUDGET);
    let edits = TerrainHeightEdits::new(patches.clone());
    let config = draw.config().clone();
    let root_level = draw.root_level();
    let target = PatchTarget::of(&map);
    let step_m = target.height_scale / 65535.0;
    // The CPU mirror: the same bands, the same pyramids, the vertex shader's seam rule.
    let mut mirror = map;
    let mut pyramid = MinMaxPyramid::build(&mirror, root_level).expect("pyramid");
    let mut mips = HeightMips::build(&mirror);

    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        // The last cascade reaches the far plane: 1 km keeps the far corner out of every one.
        far: 1000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.terrain_budget.anisotropy = 0;
    quality.gi_probes = false;
    quality.screen_space_gi = false;
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let render = |renderer: &mut FrameRenderer| {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        renderer.render_offscreen(device(), &frame).expect("frame");
    };
    // The cascades settle on the held camera first.
    for _ in 0..3 {
        render(&mut renderer);
    }

    let mut pushed = 0u32;
    let mut last_edit: Option<HeightEdit> = None;
    let mut far_edits = 0u32;
    let mut near_hits = 0u32;
    let mut feet_hits = [0u64; 4];
    let mut boxes = 0u64;
    let mut vt_rects = stats.virtual_texture_edits().rects;
    for step in 0..STEPS {
        let mut far = false;
        let feet = (180..190).contains(&step);
        if step.is_multiple_of(10) {
            let i = step / 10;
            let e = edit(&mirror, i);
            assert!(e.width <= MAX_EDIT_SIDE);
            last_edit = Some(e.clone());
            edits
                .push(e)
                .unwrap_or_else(|err| panic!("edit {i}: {} ({err})", err.code()));
            pushed += 1;
            far = edit_spec(i).1 == 1;
        }
        let bands = patches.take_frame();
        let applied = patches.state().last_frame.samples;
        assert!(
            applied <= BUDGET,
            "step {step} applied {applied} samples, over {BUDGET}"
        );
        if bands.is_empty() {
            continue;
        }
        let mut changed_boxes = Vec::new();
        for band in &bands {
            let before: Vec<f32> = mirror.heights().to_vec();
            band.apply(&mut mirror).expect("same grid");
            pyramid.update_region(&mirror, &band.rect);
            mips.update_region(&mirror, &band.rect);
            if before != mirror.heights() {
                changed_boxes.push(target.footprint(&band.rect));
            }
            // Every sample of the band sits on the 16-bit grid of the contract.
            for (r, row) in (band.rect.row_start..band.rect.row_end).enumerate() {
                let w = (band.rect.col_end - band.rect.col_start) as usize;
                for (c, col) in (band.rect.col_start..band.rect.col_end).enumerate() {
                    let h = mirror.heights()[(row * N + col) as usize];
                    assert_eq!(h, target.dequantize(band.samples[r * w + c]));
                }
            }
        }
        let draws_before = renderer.shared_shadow_draws();
        let hits_before = stats.cascade_edit_invalidations();
        render(&mut renderer);
        boxes += changed_boxes.len() as u64;
        assert_eq!(
            stats.height_edit_boxes(),
            boxes,
            "step {step}: one edit box per changed band"
        );
        let hits = stats.cascade_edit_invalidations();
        if far {
            far_edits += 1;
            assert_eq!(
                hits, hits_before,
                "step {step}: a far one-sample edit invalidated a cascade"
            );
            assert_eq!(
                renderer.shared_shadow_draws(),
                draws_before,
                "step {step}: a far one-sample edit redrew a terrain cascade"
            );
        } else if hits.iter().zip(&hits_before).any(|(a, b)| a > b) {
            near_hits += 1;
        }
        if feet {
            for (sum, (a, b)) in feet_hits.iter_mut().zip(hits.iter().zip(&hits_before)) {
                *sum += a - b;
            }
        }
        // No crack at node or LOD edges around the camera, after this frame's bands.
        let eye = [EYE[0], EYE[2]];
        let selection = select_nodes(&mirror, &pyramid, &config, root_level, eye, None);
        let report = lod_seam_report(&mirror, &mips, &config, root_level, &selection, eye);
        assert!(report.level_change_edges > 0, "{report:?}");
        assert!(report.is_crack_free(1e-4), "step {step}: {report:?}");
        if vt {
            let counters = stats.virtual_texture_edits();
            assert_eq!(counters.merges, 0, "step {step}: edit rectangles were merged");
            assert_eq!(
                counters.rects - vt_rects,
                changed_boxes.len() as u64,
                "step {step}: one VT rectangle per changed band"
            );
            vt_rects = counters.rects;
            if let (Some(r), Some(([x0, z0], [x1, z1]))) = (counters.last_rect, changed_boxes.last()) {
                let grow = HEIGHT_REACH_M + 2.0 * mirror.sample_spacing() + 1e-3;
                assert!(
                    r[0] >= x0 - grow && r[1] >= z0 - grow && r[2] <= x1 + grow && r[3] <= z1 + grow,
                    "step {step}: VT rectangle {r:?} leaves the edit box {:?}",
                    ([x0, z0], [x1, z1])
                );
            }
        }
    }
    assert_eq!(pushed, 20);
    assert_eq!(far_edits, 5, "five far one-sample edits");
    assert!(
        near_hits >= 3,
        "craters in view invalidated cascades only {near_hits} times"
    );
    assert!(
        feet_hits[0] > 0,
        "the crater at the camera's feet left cascade 0: {feet_hits:?}"
    );
    assert!(patches.is_idle(), "every edit applied within the 200 steps");
    assert_eq!(patches.state().completed, 20);
    // The contract's millimetres round-trip: every sample of the last edit (applied after all the
    // others) equals its millimetres to within half a 16-bit step (the quantizer rounds).
    let last = last_edit.expect("twenty edits pushed");
    assert!(step_m > 0.0);
    for r in 0..last.height {
        for c in 0..last.width {
            let h = mirror.heights()[((last.row + r) * N + last.col + c) as usize];
            let want = last.heights_mm[(r * last.width + c) as usize] as f32 * 1.0e-3;
            assert!(
                (h - want).abs() <= step_m * 0.5 + 1.0e-5,
                "sample ({}, {}): {h} m, the edit asked {want} m (step {step_m} m)",
                last.col + c,
                last.row + r
            );
        }
    }
    eprintln!(
        "terrain_height_edits: {boxes} edit boxes, cascade invalidations {:?}, VT {:?}, grass reselects {}",
        stats.cascade_edit_invalidations(),
        stats.virtual_texture_edits(),
        stats.grass_edit_reselects()
    );
}

/// The contract's limits are refused with their codes and change nothing.
#[test]
fn the_contract_limits_are_refused_with_their_codes() {
    let map = map();
    let patches = regia_terrain::TerrainHeightPatches::default();
    patches.set_target(PatchTarget::of(&map));
    let edits = TerrainHeightEdits::new(patches.clone());
    let flat = |col, row, side: u32, mm: i32| HeightEdit {
        col,
        row,
        width: side,
        height: side,
        heights_mm: vec![mm; (side * side) as usize],
    };
    let code = |e: HeightEdit| edits.push(e).expect_err("refused").code();
    assert_eq!(code(flat(0, 0, MAX_EDIT_SIDE + 1, 0)), "edit_too_large");
    assert_eq!(
        code(flat(0, 0, 4, 40_000)),
        "edit_out_of_range",
        "above offset + scale"
    );
    assert_eq!(
        code(flat(0, 0, 4, -33_000)),
        "edit_out_of_range",
        "below the offset"
    );
    assert_eq!(code(flat(N - 2, 0, 4, 0)), "patch_out_of_bounds");
    assert!(patches.is_idle());
    for i in 0..16 {
        edits
            .push(flat(i * 4, 0, 4, 1_000))
            .expect("within the frame limit");
    }
    assert_eq!(code(flat(100, 100, 4, 1_000)), "edit_frame_full");
    assert_eq!(patches.state().queued_patches, 16);
    patches.take_frame();
    edits.push(flat(100, 100, 4, 1_000)).expect("a new frame");
}

/// ADR-7659 (spec 25 § 7.4): with the probes on, one crater re-reduces only the texels of the
/// probes' heightfield max pyramid its changed samples reach, well under 1 % of a full rebuild's
/// (the whole-map paths are `regia_render`'s `regional_max_pyramid_is_bit_identical_to_a_full_rebuild_on_the_gpu`).
/// Counters only, no wall clock.
#[test]
fn one_crater_rebuilds_under_one_percent_of_the_probe_pyramid() {
    let map = map();
    let (draw, _) = draw(map.clone());
    let patches = draw.height_patches_handle();
    patches.set_budget(BUDGET);
    let edits = TerrainHeightEdits::new(patches.clone());
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        far: 1000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.terrain_budget.anisotropy = 0;
    quality.gi_probes = true;
    quality.screen_space_gi = false;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(true));
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let render = |renderer: &mut FrameRenderer| {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        renderer.render_offscreen(device(), &frame).expect("frame");
    };
    for _ in 0..2 {
        render(&mut renderer);
    }
    if !renderer.ddgi_active() {
        eprintln!("SKIP: the probes did not run on this device");
        return;
    }
    let before = renderer.dynamic_gi_status().pyramid;
    assert!(before.full_rebuilds >= 1, "{before:?}");
    assert!(before.full_texels > 0, "{before:?}");
    // One 13 × 13 crater (edit 0), applied in one frame.
    edits.push(edit(&map, 0)).expect("edit");
    let bands = patches.take_frame();
    assert!(!bands.is_empty());
    render(&mut renderer);
    let after = renderer.dynamic_gi_status().pyramid;
    assert_eq!(after.full_rebuilds, before.full_rebuilds, "{after:?}");
    assert_eq!(after.regional_rebuilds, before.regional_rebuilds + 1, "{after:?}");
    assert!(
        after.last_texels > 0 && after.last_texels * 100 < after.full_texels,
        "one crater re-reduced {} of {} pyramid texels",
        after.last_texels,
        after.full_texels
    );
    // No edit, no rebuild.
    render(&mut renderer);
    assert_eq!(renderer.dynamic_gi_status().pyramid, after);
    eprintln!(
        "terrain_height_edits: one crater re-reduced {} of {} probe pyramid texels ({:.3} %)",
        after.last_texels,
        after.full_texels,
        after.last_texels as f64 * 100.0 / after.full_texels as f64
    );
}

/// Spec 25 § 7.4's GPU cost (ADR-7659), on a card only: the GPU time of a frame that applies one
/// 128 × 128 crater against a frame that applies none, on a 4097-sample synthetic map (8 km at
/// 2 m), with the terrain draw and the probe update (timestamp queries; skipped without them).
/// Prints medians; asserts nothing about time. Uses only APIs the pre-ADR-7659 engine has, so
/// the same test measures the whole-map rebuild before the change.
///
/// `REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1 cargo test -p regia_terrain --release --test
/// terrain_height_edits -- --ignored --nocapture probe_pyramid_edit_frame_gpu_cost`
#[test]
#[ignore = "card timing (spec 25 § 7.4): run with --ignored on a GPU"]
fn probe_pyramid_edit_frame_gpu_cost() {
    const N4: u32 = 4097;
    const WORLD4: f32 = 8192.0;
    const SIDE: u32 = MAX_EDIT_SIDE;
    const PAIRS: u32 = 24;
    if !device()
        .device
        .features()
        .contains(wgpu::Features::TIMESTAMP_QUERY)
    {
        eprintln!("SKIP: the device has no timestamp queries");
        return;
    }
    let world = |s: u32| s as f32 / (N4 - 1) as f32 * WORLD4 - WORLD4 * 0.5;
    let mut heights = Vec::with_capacity((N4 * N4) as usize);
    for row in 0..N4 {
        for col in 0..N4 {
            let (x, z) = (world(col), world(row));
            heights.push(6.0 * (x / 97.0).sin() * (z / 131.0).cos() + 2.0 * (x / 23.0 + z / 37.0).sin());
        }
    }
    let shaped = TerrainMap::from_heights(N4, N4, heights, WORLD4, 64.0, -32.0).expect("map");
    let map = TerrainMap::new(&shaped.to_heightfield(), WORLD4, 64.0, -32.0).expect("16-bit map");
    let (draw, _) = draw(map.clone());
    let patches = draw.height_patches_handle();
    patches.set_budget(SIDE * SIDE);
    let edits = TerrainHeightEdits::new(patches.clone());
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: 1280,
        height: 720,
        eye: [-300.0, 40.0, -300.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.gi_probes = true;
    quality.screen_space_gi = false;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(true));
    renderer.set_gpu_timing(device(), true);
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let render = |renderer: &mut FrameRenderer| {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        renderer.render_offscreen(device(), &frame).expect("frame");
        let t = renderer.gpu_timings();
        (t.frame_ms, t.gi_ms)
    };
    for _ in 0..30 {
        render(&mut renderer);
    }
    assert!(renderer.ddgi_active(), "the probes must run for this measurement");
    let crater = |i: u32| {
        // Craters on a ring around the camera, each on ground no earlier one touched.
        let a = i as f32 * 0.7;
        let (cx, cz) = (-300.0 + 600.0 * a.cos(), -300.0 + 600.0 * a.sin());
        let to_sample = |w: f32| (((w + WORLD4 * 0.5) / WORLD4 * (N4 - 1) as f32).round() as u32).min(N4 - 1);
        let (c0, r0) = (to_sample(cx) - SIDE / 2, to_sample(cz) - SIDE / 2);
        let r = SIDE as f32 * 0.5;
        let mut heights_mm = Vec::with_capacity((SIDE * SIDE) as usize);
        for row in r0..r0 + SIDE {
            for col in c0..c0 + SIDE {
                let base = map.heights()[(row * N4 + col) as usize];
                let (dc, dr) = (
                    col as f32 - (c0 as f32 + r - 0.5),
                    row as f32 - (r0 as f32 + r - 0.5),
                );
                let d = ((dc * dc + dr * dr).sqrt() / r).min(1.0);
                heights_mm.push(((base - 4.0 * (1.0 - d * d)) * 1000.0).round() as i32);
            }
        }
        HeightEdit {
            col: c0,
            row: r0,
            width: SIDE,
            height: SIDE,
            heights_mm,
        }
    };
    let (mut idle, mut edit) = (Vec::new(), Vec::new());
    for i in 0..PAIRS {
        patches.take_frame();
        idle.push(render(&mut renderer));
        edits.push(crater(i)).expect("crater");
        assert!(!patches.take_frame().is_empty());
        edit.push(render(&mut renderer));
    }
    let median = |v: &[(f32, f32)], pick: fn(&(f32, f32)) -> f32| {
        let mut x: Vec<f32> = v.iter().map(pick).collect();
        x.sort_by(f32::total_cmp);
        x[x.len() / 2]
    };
    let (f_idle, f_edit) = (median(&idle, |t| t.0), median(&edit, |t| t.0));
    let (g_idle, g_edit) = (median(&idle, |t| t.1), median(&edit, |t| t.1));
    eprintln!(
        "probe_pyramid_edit_frame_gpu_cost on {}: frame median no edit {f_idle:.3} ms, one 128 x 128 edit {f_edit:.3} ms (+{:.3} ms); probe update (gi_ms) {g_idle:.3} ms vs {g_edit:.3} ms (+{:.3} ms); {PAIRS} pairs",
        device().adapter.get_info().name,
        f_edit - f_idle,
        g_edit - g_idle
    );
}

/// Brief item 5 (spec 25 § 7.4 known issue): a scatter instance re-sat on the sloped wall of a crater
/// should lean as an instance placed on that ground leans. `ScatterField::heights_changed` moves it
/// vertically only, so it keeps the flat ground's tilt and this test fails; it is ignored until the
/// re-sit re-tilts (`scatter/field.rs`, owned by another lane). Run with `--ignored` to see it.
#[test]
#[ignore = "known issue (spec 25 § 7.4): a re-sat scatter instance keeps the old ground's tilt"]
fn a_resat_scatter_instance_on_a_sloped_crater_takes_the_new_tilt() {
    use regia_terrain::{
        DirtyRect, RockStyle, ScatterClass, ScatterField, ScatterLibrary, ScatterMeshRef, ScatterSet,
        ScatterSource, procedural_rock,
    };
    use std::sync::Arc;
    const S: u32 = 65;
    let flat = vec![10.0f32; (S * S) as usize];
    let map = Arc::new(TerrainMap::from_heights(S, S, flat.clone(), 1024.0, 64.0, 0.0).expect("map"));
    let set = ScatterSet {
        classes: vec![ScatterClass {
            name: "boulders".into(),
            meshes: vec![ScatterMeshRef {
                mesh: "rock_a".into(),
                weight: 1.0,
                min_scale: 0.5,
                max_scale: 1.5,
                scale_bias: 1.0,
            }],
            cell_size: 4.0,
            density: 1.0,
            fade_start: 150.0,
            fade_end: 200.0,
            align_to_normal: 1.0,
            tilt_max: 1.0,
            tilt_jitter: 0.0,
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock_a".into(),
        procedural_rock(3, RockStyle::angular([0.5, 0.4, 0.35])),
    );
    let eye = [0.0, 12.0, 0.0];
    let mut field =
        ScatterField::new(&set, ScatterSource::new(Arc::clone(&map)), &library, 1).expect("field");
    field.update(eye);
    // The crater wall: a 14° ramp (4 m a 16 m sample) over columns 28..36 of rows 20..44.
    let rect = DirtyRect {
        col_start: 28,
        col_end: 36,
        row_start: 20,
        row_end: 44,
    };
    let mut heights = flat;
    for row in rect.row_start..rect.row_end {
        for col in rect.col_start..rect.col_end {
            heights[(row * S + col) as usize] = 10.0 + 4.0 * (col - rect.col_start + 1) as f32;
        }
    }
    let edited = TerrainMap::from_heights(S, S, heights, 1024.0, 64.0, 0.0).expect("edited");
    field.heights_changed(&edited, &rect);
    let mut fresh =
        ScatterField::new(&set, ScatterSource::new(Arc::new(edited)), &library, 1).expect("fresh");
    fresh.update(eye);
    let up = |m: &[[f32; 4]; 4]| {
        let l = (m[1][0] * m[1][0] + m[1][1] * m[1][1] + m[1][2] * m[1][2]).sqrt();
        [m[1][0] / l, m[1][1] / l, m[1][2] / l]
    };
    let fresh_at: Vec<([f32; 3], [[f32; 4]; 4])> = fresh
        .instance_grounds()
        .into_iter()
        .zip(fresh.instance_models())
        .collect();
    // Instances on the ramp's inner columns (x from sample 29 to 34: −48 m .. 32 m).
    let on_ramp = |g: &[f32; 3]| g[0] > -48.0 && g[0] < 32.0 && g[2] > -160.0 && g[2] < 160.0;
    let mut compared = 0;
    let mut worst = 0.0f32;
    for (ground, model) in field.instance_grounds().iter().zip(field.instance_models()) {
        if !on_ramp(ground) {
            continue;
        }
        let Some((_, placed)) = fresh_at
            .iter()
            .find(|(g, _)| (g[0] - ground[0]).abs() < 1e-3 && (g[2] - ground[2]).abs() < 1e-3)
        else {
            continue;
        };
        let (a, b) = (up(&model), up(placed));
        let cos = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0);
        worst = worst.max(cos.acos().to_degrees());
        compared += 1;
    }
    assert!(compared > 0, "no instance on the ramp in both fields");
    assert!(
        worst < 1.0,
        "a re-sat instance leans {worst:.1}° off an instance placed on the new slope ({compared} compared)"
    );
}
