// SPDX-License-Identifier: MPL-2.0
//! ADR-7585 (DO27 contract § 7.4 stage 2): a runtime height patch redraws only the terrain sun
//! cascades it can reach. Uses only the stage-1 API (`height_patches_handle`, `HeightPatch`) and
//! the renderer's own counter, so it runs unchanged against the code before the narrowing,
//! where every applied band re-keyed all four cascades.
//!
//! Lavapipe, synthetic map, held camera; counters only.

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{CdlodConfig, HeightPatch, LayerDesc, TerrainLayers, TerrainMap, TerrainSharedDraw};

/// 2 km, 4 m samples.
const N: u32 = 513;
const WORLD_M: f32 = 2048.0;
/// Near the −X −Z corner, looking at the centre; the far plane (1 km) bounds the last cascade.
const EYE: [f32; 3] = [-700.0, 30.0, -700.0];

fn sample_of(world: f32) -> u32 {
    (((world + WORLD_M * 0.5) / WORLD_M * (N - 1) as f32).round() as u32).min(N - 1)
}

fn map() -> TerrainMap {
    let mut heights = Vec::with_capacity((N * N) as usize);
    for row in 0..N {
        for col in 0..N {
            let x = col as f32 / (N - 1) as f32 * WORLD_M - WORLD_M * 0.5;
            let z = row as f32 / (N - 1) as f32 * WORLD_M - WORLD_M * 0.5;
            heights.push(6.0 * (x / 97.0).sin() * (z / 131.0).cos() + 2.0 * (x / 23.0 + z / 37.0).sin());
        }
    }
    let shaped = TerrainMap::from_heights(N, N, heights, WORLD_M, 64.0, -32.0).expect("map");
    TerrainMap::new(&shaped.to_heightfield(), WORLD_M, 64.0, -32.0).expect("16-bit map")
}

/// A square patch of `side` samples centred on world `(x, z)`, `depth` metres below `map`.
fn dig(map: &TerrainMap, x: f32, z: f32, side: u32, depth: f32) -> HeightPatch {
    let (c0, r0) = (sample_of(x) - side / 2, sample_of(z) - side / 2);
    let heights = (r0..r0 + side)
        .flat_map(|row| (c0..c0 + side).map(move |col| (row, col)))
        .map(|(row, col)| map.heights()[(row * N + col) as usize] - depth)
        .collect();
    HeightPatch {
        col: c0,
        row: r0,
        width: side,
        height: side,
        heights,
    }
}

#[test]
fn a_far_one_sample_patch_redraws_no_terrain_cascade() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU device: {err}"));
    let map = map();
    let layers = TerrainLayers {
        layers: vec![LayerDesc::ground()],
        ..Default::default()
    };
    let draw =
        TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 4).expect("terrain");
    let patches = draw.height_patches_handle();
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: 96,
        height: 54,
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
        renderer.render_offscreen(&device, &frame).expect("frame");
    };
    for _ in 0..4 {
        render(&mut renderer);
    }
    let held = renderer.shared_shadow_draws();
    render(&mut renderer);
    assert_eq!(
        renderer.shared_shadow_draws(),
        held,
        "a held camera redraws no cascade"
    );

    // One sample 1 m down, 2.2 km from the camera: past the far plane and every cascade.
    patches
        .request(dig(&map, 900.0, 880.0, 1, 1.0))
        .expect("far patch");
    assert_eq!(patches.take_frame().len(), 1);
    let before = renderer.shared_shadow_draws();
    render(&mut renderer);
    let far = renderer.shared_shadow_draws() - before;
    assert_eq!(far, 0, "a far one-sample patch redrew {far} terrain cascades");

    // A crater at the camera's feet still redraws the cascades it is in.
    patches
        .request(dig(&map, EYE[0], EYE[2] + 20.0, 13, 3.0))
        .expect("near patch");
    assert_eq!(patches.take_frame().len(), 1);
    let before = renderer.shared_shadow_draws();
    render(&mut renderer);
    let near = renderer.shared_shadow_draws() - before;
    assert!(near >= 1, "a crater at the camera's feet redrew no cascade");
    eprintln!("terrain_edit_cascade_scope: far patch {far} cascade redraws, near crater {near}");
}
