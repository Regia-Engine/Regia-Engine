// SPDX-License-Identifier: MPL-2.0
//! Minimum-spec terrain frame: the shared terrain draw plus a lit mesh, rendered on a device
//! clamped to a Pascal or mid-range mobile profile. Proves the terrain's group-4 bindings (the
//! virtual-texture colour atlas and the cascade-shadow view-projection) build and draw when the
//! device has the engine's 5 bind groups, and that the fallbacks the tier lacks are reported in
//! `render.problems`.

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabMesh, LabScene, MinspecProfile, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SharedPassDraw, uv_sphere,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainSharedDraw, VirtualTextureSettings,
};

const W: u32 = 160;
const H: u32 = 90;
const EYE: [f32; 3] = [8.0, 10.0, -32.0];

fn device(profile: MinspecProfile) -> RenderDevice {
    RenderDevice::headless_with_profile(profile)
        .unwrap_or_else(|err| panic!("the {profile:?} profile must create a device: {err}"))
}

/// 256 m, 1 m samples: gentle rolling hills.
fn map() -> TerrainMap {
    let n = 257u32;
    let world = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            heights.push(6.0 * (x / 21.0).sin() * (z / 26.0).cos() + 2.5 * (x / 9.0 + z / 13.0).sin());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 30.0, -10.0).expect("map")
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.55, 0.52, 0.48],
            roughness: 1.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    }
}

fn material() -> TerrainMaterial {
    TerrainMaterial {
        layers: vec![Some(LayerTextures {
            albedo: Rgba8Image::solid(64, 64, [140, 133, 122, 255]),
            normal: Rgba8Image::neutral_normal(64),
            height: Rgba8Image::neutral_height(64),
            orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
            use_orm: false,
        })],
        macro_set: None,
        params: MaterialParams::default(),
    }
}

fn vt_settings() -> VirtualTextureSettings {
    VirtualTextureSettings {
        lod0_texel_m: 0.1,
        page_texels: 30,
        atlas_slots: 24,
        near_m: 0.0,
        bakes_per_frame: 400,
        rebakes_per_frame: 0,
        stochastic_lod: false,
        ..VirtualTextureSettings::default()
    }
}

/// The shared terrain draw plus one lit sphere mesh in front of the camera.
fn draw(device: &RenderDevice, vt: bool) -> TerrainSharedDraw {
    let mut draw = TerrainSharedDraw::new(device, map(), &layers(), CdlodConfig::default(), 3)
        .expect("terrain builds on the profile");
    draw.set_material(&material(), &[]).expect("material");
    if vt {
        draw.set_virtual_texture(Some(vt_settings()))
            .expect("virtual texture");
    }
    draw
}

fn lit_mesh_scene() -> LabScene {
    let (sphere_verts, sphere_indices) = uv_sphere(1.0, 20, 14);
    let mut sphere = LabMesh::unskinned();
    sphere.material = 0;
    sphere.vertices = sphere_verts;
    sphere.indices = sphere_indices;
    sphere.model = regia_core::glam::Mat4::from_translation([3.0, 1.0, -12.0].into()).to_cols_array_2d();

    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, -8.0],
        fov_y: 0.9,
        near: 0.5,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.85, 0.85, 0.85, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            ..LabMaterial::default()
        }],
        meshes: vec![sphere],
    }
}

fn render(
    device: &RenderDevice,
    draw: TerrainSharedDraw,
    scene: &LabScene,
    preset: RenderProfilePreset,
) -> (FrameRenderer, Vec<u8>) {
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.taau = false;
    let frame: FrameScene<'_> = scene.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let pixels = renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("terrain frame on a min-spec profile: {err}"))
        .pixels;
    (renderer, pixels)
}

fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0)
        .collect()
}

fn variance(xs: &[f32]) -> f32 {
    let n = xs.len() as f32;
    let mean = xs.iter().sum::<f32>() / n;
    xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n
}

fn assert_visibly_lit(pixels: &[u8]) {
    let lum = luma(pixels);
    let var = variance(&lum);
    assert!(var > 0.0005, "the terrain frame is flat (variance {var:.5})");
    let mean = lum.iter().sum::<f32>() / lum.len() as f32;
    assert!(
        (0.02..=0.9).contains(&mean),
        "mean luminance {mean:.3} is black or blown out"
    );
    let mut sorted = lum.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let lo = sorted[sorted.len() / 10];
    let hi = sorted[sorted.len() * 9 / 10];
    assert!(
        hi - lo > 0.03,
        "no lit-vs-shadowed contrast (p10 {lo:.3}, p90 {hi:.3})"
    );
}

fn codes(problems: &[regia_render::host_checklist::FrameProblem]) -> Vec<&str> {
    problems.iter().map(|p| p.code).collect()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_pascal_profile_renders_terrain_and_a_lit_mesh() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device(MinspecProfile::Pascal);
    let scene = lit_mesh_scene();
    let (renderer, pixels) = render(
        &device,
        draw(&device, false),
        &scene,
        RenderProfilePreset::DesktopHigh,
    );
    assert_visibly_lit(&pixels);
    // Pascal needs no fallback the adapter itself does not have.
    let creation_notes = device.capabilities.fallback_notes();
    let at_creation = codes(&creation_notes);
    for problem in renderer.frame_problems() {
        assert!(
            at_creation.contains(&problem.code),
            "the frame invented fallback `{}`",
            problem.code
        );
    }
}

#[test]
fn the_mobile_profile_renders_terrain_and_reports_its_fallbacks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device(MinspecProfile::Mobile);
    let scene = lit_mesh_scene();
    let (renderer, pixels) = render(
        &device,
        draw(&device, false),
        &scene,
        RenderProfilePreset::MobileHigh,
    );
    assert_visibly_lit(&pixels);
    let reported = codes(renderer.frame_problems());
    for code in [
        "fallback_no_timestamps",
        "fallback_no_float32_filter",
        "fallback_no_block_compression",
    ] {
        assert!(reported.contains(&code), "{code} missing from {reported:?}");
    }
}

/// The mobile profile (5 bind groups) still builds the terrain's group-4 bindings: the
/// virtual-texture colour atlas and the cascade-shadow view-projection.
#[test]
fn the_mobile_profile_builds_the_terrain_vt_and_shadow_layouts() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device(MinspecProfile::Mobile);
    let scene = lit_mesh_scene();
    let draw = draw(&device, true); // VT on: its colour atlas binds at @group(4).
    let handle = draw.vt_handle();
    // Render several frames (the VT bakes its pages over the first few): shadows exercise the
    // cascade at @group(4), the colour pass exercises the VT atlas at @group(4).
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh));
    quality.taau = false;
    let frame: FrameScene<'_> = scene.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut pixels = Vec::new();
    for _ in 0..8 {
        pixels = renderer
            .render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("terrain frame on the mobile profile: {err}"))
            .pixels;
    }
    assert_visibly_lit(&pixels);
    // VT pages became resident, so the VT layout + pipeline built and ran at group 4.
    // The virtual texture needs forced early depth tests; where the adapter lacks them (Metal) it
    // stays off by design (same guard as tests/terrain_vt.rs), but the frames above still render.
    if !device
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
    {
        eprintln!("SKIP: the device lacks SHADER_EARLY_DEPTH_TEST (the virtual texture stays off)");
        return;
    }
    let pages = handle.resident_pages();
    assert!(
        !pages.is_empty(),
        "the virtual texture stayed off on the mobile profile (group 4 unusable)"
    );
}

/// The terrain takes the engine's point and spot lights on both min-spec tiers: the light
/// bindings fit the fragment stage's 8 storage buffers (Mobile with the virtual texture uses
/// all eight). Sun off and no meshes, so any change a lamp 2 m above the ground makes is the
/// terrain's.
#[test]
fn the_minspec_profiles_light_terrain_with_local_lights() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (profile, preset, vt) in [
        (MinspecProfile::Pascal, RenderProfilePreset::DesktopHigh, false),
        (MinspecProfile::Mobile, RenderProfilePreset::MobileHigh, true),
    ] {
        let device = device(profile);
        let mut scene = lit_mesh_scene();
        scene.meshes.clear();
        scene.directional = DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 0.0);
        scene.exposure_ev = 8.0;
        if let Some(post) = scene.post.as_mut() {
            post.exposure_ev = 8.0;
        }
        let frame = |scene: &LabScene| {
            let draw = draw(&device, vt);
            assert!(
                draw.gpu().local_lights_supported(),
                "{profile:?}: the light bindings must fit the min-spec tier"
            );
            let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            quality.taau = false;
            let frame: FrameScene<'_> = scene.as_frame_scene_with_quality(quality);
            let mut renderer = FrameRenderer::new();
            renderer.set_outdoor(OutdoorParams {
                planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
                ..OutdoorParams::default()
            });
            renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
            let mut pixels = Vec::new();
            for _ in 0..3 {
                pixels = renderer
                    .render_offscreen(&device, &frame)
                    .unwrap_or_else(|err| panic!("{profile:?}: terrain frame with a lamp: {err}"))
                    .pixels;
            }
            luma(&pixels)
        };
        let dark = frame(&scene);
        scene.local_lights = vec![regia_render::LabLocalLight::Point(regia_render::LabPointLight {
            position: [0.0, 0.6, -8.0],
            color: [1.0, 0.8, 0.6],
            intensity: 400_000.0,
            radius: 25.0,
            cast_shadows: false,
            source_radius: None,
        })];
        let lit = frame(&scene);
        let gain = lit.iter().zip(&dark).map(|(l, d)| l - d).fold(0.0f32, f32::max);
        let changed = lit.iter().zip(&dark).filter(|(l, d)| *l - *d > 0.05).count();
        eprintln!("{profile:?}: lamp gain max {gain:.3}, {changed} px brighter by > 0.05");
        assert!(
            gain > 0.2 && changed > 100,
            "{profile:?}: a lamp 2 m above the ground does not light it (max +{gain:.3}, {changed} px)"
        );
    }
}
