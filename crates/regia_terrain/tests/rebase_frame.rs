// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 12.2, the terrain row: a floating-origin rebase moves the camera, the meshes and the
//! lights by whole cells and the terrain stays in its own world-anchored space, so the frame before
//! and the frame after print the same picture. Each lighting mode reaches a different local lookup:
//! the sun the engine's cascades, the lamp the froxel lights, the probes the irradiance field, the
//! clouds the cloud shadow map.
//! The control frame moves the scene without telling the renderer: the ground must then be wrong.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, CloudPattern, CloudSettings, DirectionalLightUniform,
    FrameQuality, FrameRenderer, LabLocalLight, LabPointLight, LabScene, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, TerrainLayers, TerrainMap, TerrainRelief, TerrainSharedDraw, WaterBody,
    WaterLook, WaterRenderer,
};

const W: u32 = 192;
const H: u32 = 108;
const SAMPLES: u32 = 257;
const WORLD: f32 = 2048.0;
/// A rebase by (1, 0, 1) cells of 1024 m: exact in `f32`.
const SHIFT: [f32; 3] = [1024.0, 0.0, 1024.0];
/// Where the camera looks, in the terrain's space.
const SITE: [f32; 2] = [700.0, 500.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Rolling ground, different everywhere, so a misplaced terrain cannot look right by accident.
fn height(x: f32, z: f32) -> f32 {
    18.0 * (x / 60.0).sin() * (z / 47.0).cos() + 6.0 * (x / 13.0 + z / 17.0).sin()
}

fn ground() -> TerrainSharedDraw {
    let spacing = WORLD / (SAMPLES - 1) as f32;
    let half = WORLD * 0.5;
    let heights: Vec<f32> = (0..SAMPLES)
        .flat_map(|j| (0..SAMPLES).map(move |i| (i, j)))
        .map(|(i, j)| height(i as f32 * spacing - half, j as f32 * spacing - half))
        .collect();
    let map = TerrainMap::from_heights(SAMPLES, SAMPLES, heights, WORLD, 120.0, -60.0).expect("map");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.4, 0.4, 0.4],
            roughness: 1.0,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 4).expect("terrain");
    draw.set_relief(TerrainRelief::off());
    draw
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Light {
    Sun,
    Lamp,
    Probes,
    Clouds,
}

/// One frame of the scene with every position moved by `-offset`.
fn frame(renderer: &mut FrameRenderer, light: Light, offset: [f32; 3]) -> Vec<u8> {
    let at = |dx: f32, up: f32, dz: f32| {
        let (x, z) = (SITE[0] + dx, SITE[1] + dz);
        [x - offset[0], height(x, z) + up - offset[1], z - offset[2]]
    };
    let ev = if light == Light::Lamp { 8.0 } else { 14.5 };
    let post = PostSettings {
        exposure_ev: ev,
        ..PostSettings::golden_deterministic()
    };
    let (verts, indices) = regia_render::box_mesh([1.0, 1.0, 1.0]);
    let lights = if light == Light::Lamp {
        vec![LabLocalLight::Point(LabPointLight {
            position: at(-3.0, 4.0, -2.0),
            color: [1.0, 1.0, 1.0],
            intensity: 400_000.0,
            radius: 80.0,
            cast_shadows: false,
            source_radius: None,
        })]
    } else {
        Vec::new()
    };
    let sun_lux = if light == Light::Lamp { 0.0 } else { 100_000.0 };
    let scene = LabScene {
        width: W,
        height: H,
        eye: at(0.0, 14.0, -24.0),
        target: at(0.0, 0.0, 0.0),
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: light != Light::Lamp,
        ibl: light != Light::Lamp,
        exposure_ev: ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], sun_lux),
        local_lights: lights,
        materials: vec![regia_render::LabMaterial {
            base_color: [0.5, 0.2, 0.2, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes: vec![regia_render::LabMesh {
            vertices: verts,
            indices,
            material: 0,
            model: regia_core::glam::Mat4::from_translation(at(3.0, 1.0, 0.0).into()).to_cols_array_2d(),
            ..regia_render::LabMesh::unskinned()
        }],
    };
    let preset = RenderProfilePreset::DesktopHigh;
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.taau = false;
    quality.gi_probes = light == Light::Probes;
    if light == Light::Clouds {
        quality.volumetric_clouds = true;
        quality.physical_atmosphere = true;
        quality.msaa_samples = 1;
    }
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    renderer.render_offscreen(device(), &frame).expect("frame").pixels
}

/// The sky of `light`; `drift` is what the rebase hook keeps the clouds over the same ground with.
fn outdoor(light: Light, drift: [f32; 2]) -> OutdoorParams {
    let mut outdoor = OutdoorParams::default();
    if matches!(light, Light::Lamp | Light::Probes) {
        let mut planet = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
        planet.ground_albedo = [0.4, 0.4, 0.4];
        outdoor.planet = planet;
    }
    if light == Light::Clouds {
        // Half the sky cloudy, with the edge of the shadow through the middle of the view: the sun
        // ray from the site crosses the layer's base at about x = 1030, and the weather's edge is at
        // `x - drift = 0`.
        outdoor.clouds = CloudSettings {
            coverage: 0.5,
            pattern: CloudPattern::SplitHemisphere,
            map_resolution: 1024,
            wind_m_per_s: [0.0, 0.0],
            altitude_bottom: 400.0,
            altitude_top: 900.0,
            shadow_map: true,
            shadow_map_extent_m: 1500.0,
            drift_offset_m: [1030.0 + drift[0], drift[1]],
            ..CloudSettings::default()
        };
    }
    outdoor
}

fn renderer(light: Light) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(light == Light::Probes));
    renderer.set_outdoor(outdoor(light, [0.0; 2]));
    renderer.set_shared_draw(Some(Box::new(ground()) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer
}

/// Mean absolute difference per channel (8-bit levels) and the share of pixels off by more than 8.
fn difference(a: &[u8], b: &[u8]) -> (f64, f64) {
    assert_eq!(a.len(), b.len());
    let mut sum = 0u64;
    let mut off = 0u64;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let worst = (0..3).map(|c| pa[c].abs_diff(pb[c])).max().unwrap_or(0);
        sum += (0..3).map(|c| u64::from(pa[c].abs_diff(pb[c]))).sum::<u64>();
        off += u64::from(worst > 8);
    }
    let pixels = (a.len() / 4) as f64;
    (sum as f64 / (pixels * 3.0), off as f64 / pixels)
}

fn mean_level(a: &[u8]) -> f64 {
    let sum: u64 = a
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| (0..3).map(|c| u64::from(p[c])).sum::<u64>())
        .sum();
    sum as f64 / ((a.len() / 4) as f64 * 3.0)
}

/// `FRAMES` frames with no rebase against the same frames with one in the middle.
fn same_picture(light: Light) {
    const FRAMES: usize = 6;
    let mut still = renderer(light);
    let mut unmoved = Vec::new();
    for _ in 0..FRAMES {
        unmoved = frame(&mut still, light, [0.0; 3]);
    }
    assert!(
        mean_level(&unmoved) > 5.0,
        "{light:?}: the frame must show something"
    );

    let mut moved = renderer(light);
    let mut rebased = Vec::new();
    for i in 0..FRAMES {
        let rebase_now = i == FRAMES / 2;
        if rebase_now {
            moved.shift_history(SHIFT);
            moved.set_outdoor(outdoor(light, [-SHIFT[0], -SHIFT[2]]));
        }
        let offset = if i >= FRAMES / 2 { SHIFT } else { [0.0; 3] };
        rebased = frame(&mut moved, light, offset);
    }
    assert_eq!(moved.origin_offset(), SHIFT);

    // Control: the scene moves and nobody tells the renderer, so the terrain stays behind.
    let mut control = renderer(light);
    control.set_camera_cut_detection(None);
    let mut wrong = Vec::new();
    for i in 0..FRAMES {
        let offset = if i >= FRAMES / 2 { SHIFT } else { [0.0; 3] };
        wrong = frame(&mut control, light, offset);
    }

    let (mean, share) = difference(&unmoved, &rebased);
    let (wrong_mean, wrong_share) = difference(&unmoved, &wrong);
    eprintln!(
        "{light:?}: rebased mean {mean:.3} levels, {:.2} % of pixels off; control mean {wrong_mean:.3}, {:.2} %",
        share * 100.0,
        wrong_share * 100.0
    );
    assert!(
        wrong_mean > 4.0,
        "{light:?}: the control must see the terrain left behind: {wrong_mean}"
    );
    assert!(mean < 1.0, "{light:?}: mean difference {mean} levels");
    assert!(share < 0.02, "{light:?}: {share} of the pixels moved");
}

/// What the probes' rays saw, whatever slot each probe sits in: `(mean hit distance, hits, back
/// faces, misses)`. Back faces are rays that started under the ground.
fn ray_summary(renderer: &FrameRenderer) -> (f64, usize, usize, usize) {
    let readback = renderer
        .ddgi_read_back(device())
        .expect("read back")
        .expect("the probes ran");
    let (mut sum, mut hits, mut back, mut miss) = (0.0f64, 0usize, 0usize, 0usize);
    for ray in &readback.rays {
        if ray[3] >= 1.0e8 {
            miss += 1;
        } else if ray[3] < 0.0 {
            back += 1;
        } else {
            sum += f64::from(ray[3]);
            hits += 1;
        }
    }
    (sum / hits.max(1) as f64, hits, back, miss)
}

/// The probes trace the heightfield where it is drawn: their rays see the same ground from the same
/// frames after a rebase, against a control that keeps the terrain behind (`hf_trace`'s entry).
#[test]
fn a_rebase_keeps_the_probes_tracing_the_ground() {
    let probes = || {
        let mut renderer = renderer(Light::Probes);
        // One 1 m cascade: a whole number of probes per 1024 m cell, so the grid lands the same.
        renderer.set_ddgi_settings(regia_render::DdgiSettings {
            cascades: 1,
            dims: [12, 8, 12],
            spacing: 1.0,
            update_divisor: 1,
            bake_on_caller: true,
            ..regia_render::DdgiSettings::default()
        });
        renderer
    };
    const FRAMES: usize = 8;
    let mut still = probes();
    for _ in 0..FRAMES {
        frame(&mut still, Light::Probes, [0.0; 3]);
    }
    let mut moved = probes();
    moved.shift_history(SHIFT);
    for _ in 0..FRAMES {
        frame(&mut moved, Light::Probes, SHIFT);
    }
    let mut control = probes();
    for _ in 0..FRAMES {
        frame(&mut control, Light::Probes, SHIFT);
    }
    let (a, b, c) = (ray_summary(&still), ray_summary(&moved), ray_summary(&control));
    eprintln!("probe rays (mean hit, hits, back, miss): still {a:?}, rebased {b:?}, control {c:?}");
    assert!(a.1 > 100, "the probes must see the ground: {a:?}");
    let near = |x: usize, y: usize| x.abs_diff(y) * 50 <= x.max(y).max(50);
    assert!(near(a.1, b.1) && near(a.2, b.2) && near(a.3, b.3), "{a:?} {b:?}");
    assert!((a.0 - b.0).abs() <= a.0 * 0.02, "{a:?} {b:?}");
    assert!(
        !near(a.1, c.1) || !near(a.2, c.2) || (a.0 - c.0).abs() > a.0 * 0.1,
        "control {a:?} {c:?}"
    );
}

/// A lake stays on the terrain across a rebase. Its control follows the rebase everywhere but in the
/// water renderer, so what it loses is the water alone.
#[test]
fn a_rebase_keeps_the_water_where_it_is() {
    let body = WaterBody::plane(
        [SITE[0], height(SITE[0], SITE[1]) + 2.0, SITE[1]],
        [30.0, 30.0],
        WaterLook::lake(),
    );
    let with_water = || {
        let mut renderer = renderer(Light::Sun);
        let water = WaterRenderer::new(device());
        water.set_bodies(&[&body]);
        renderer.set_translucent_layers(vec![water.layer()]);
        (renderer, water)
    };
    const FRAMES: usize = 4;
    let (mut still, _still_water) = with_water();
    let mut unmoved = Vec::new();
    for _ in 0..FRAMES {
        unmoved = frame(&mut still, Light::Sun, [0.0; 3]);
    }
    let (mut dry, _dry_water) = (renderer(Light::Sun), ());
    let mut no_water = Vec::new();
    for _ in 0..FRAMES {
        no_water = frame(&mut dry, Light::Sun, [0.0; 3]);
    }
    let (water_shows, _) = difference(&unmoved, &no_water);
    assert!(water_shows > 1.0, "the lake must be in the frame: {water_shows}");

    let run = |tell_water: bool| {
        let (mut renderer, water) = with_water();
        frame(&mut renderer, Light::Sun, [0.0; 3]);
        renderer.shift_history(SHIFT);
        if tell_water {
            water.set_origin_offset(SHIFT);
        }
        let mut last = Vec::new();
        for _ in 1..FRAMES {
            last = frame(&mut renderer, Light::Sun, SHIFT);
        }
        last
    };
    let (rebased, wrong) = (run(true), run(false));
    let (mean, share) = difference(&unmoved, &rebased);
    let (wrong_mean, _) = difference(&unmoved, &wrong);
    eprintln!(
        "water: rebased mean {mean:.3}, {:.2} % off; control mean {wrong_mean:.3}; water shows {water_shows:.3}",
        share * 100.0
    );
    assert!(
        wrong_mean > 0.5 * water_shows,
        "the control must lose the lake: {wrong_mean}"
    );
    assert!(mean < 1.0, "{mean} levels");
    assert!(share < 0.02, "{share}");
}

#[test]
fn a_rebase_keeps_the_cloud_shadows_on_the_ground() {
    // The clouds must print, or this test says nothing about them.
    let (mut clear, mut cloudy) = (renderer(Light::Sun), renderer(Light::Clouds));
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        a = frame(&mut clear, Light::Sun, [0.0; 3]);
        b = frame(&mut cloudy, Light::Clouds, [0.0; 3]);
    }
    let (shows, _) = difference(&a, &b);
    eprintln!("clouds against a clear sky: {shows:.3} levels");
    assert!(shows > 1.0, "the cloud shadow must be in the frame: {shows}");
    same_picture(Light::Clouds);
}

#[test]
fn a_rebase_keeps_the_sunlit_frame() {
    same_picture(Light::Sun);
}

#[test]
fn a_rebase_keeps_the_lamp_on_the_ground() {
    same_picture(Light::Lamp);
}

#[test]
fn a_rebase_keeps_the_probe_light() {
    same_picture(Light::Probes);
}
