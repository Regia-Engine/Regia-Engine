// SPDX-License-Identifier: MPL-2.0
//! Point and spot lights light the terrain (lane v020-terrain-local-lights; gap A of
//! v017-black-hull: muzzle flashes, explosions and hull lamps never lit the ground).
//!
//! The shared colour pass now reads the forward pass's froxel lists, lights and local-light
//! shadow atlas (`SharedPassDraw::set_engine_lights`) and shades them with the engine's own
//! light code (`regia_render::shared_local_lights_wgsl`). The checks: a lamp near the ground
//! lights it (it added exactly nothing before), a grey mesh plane and grey terrain under the
//! same lamp print the same, a caster between a shadowed lamp and the ground shadows the
//! ground, and a spot light lights only its cone. The sun is off on an airless world, so the
//! lamp is the only light.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    LabLocalLight, LabPointLight, LabScene, LabSpotLight, OutdoorParams, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 192;
const H: u32 = 108;
/// EV100 that prints 0.18 grey under ~1000 lux near middle grey.
const EV: f32 = 8.0;
const EYE: [f32; 3] = [0.0, 10.0, -14.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Flat 0.18-grey ground at y = 0, 64 m across (65² samples).
fn ground() -> TerrainSharedDraw {
    let world = 64.0;
    let map = TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], world, 80.0, -40.0).expect("plane");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
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
    let v = regia_terrain::linear_to_srgb8(0.18);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
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
    draw.set_relief(TerrainRelief::off());
    draw
}

/// A box mesh of half extents `half` centred at `at`, material 0.
fn box_at(half: [f32; 3], at: [f32; 3]) -> regia_render::LabMesh {
    let (verts, indices) = regia_render::box_mesh(half);
    regia_render::LabMesh {
        vertices: verts,
        indices,
        material: 0,
        model: regia_core::glam::Mat4::from_translation(at.into()).to_cols_array_2d(),
        ..regia_render::LabMesh::unskinned()
    }
}

/// A point light of `lumens` at `position`, 30 m reach.
fn point(position: [f32; 3], lumens: f32, cast_shadows: bool) -> LabLocalLight {
    LabLocalLight::Point(LabPointLight {
        position,
        color: [1.0, 1.0, 1.0],
        intensity: lumens,
        radius: 30.0,
        cast_shadows,
        source_radius: None,
    })
}

/// One frame of the ground with `lights` and `meshes` (0.18 grey, roughness 1), sun off.
fn frame(
    lights: Vec<LabLocalLight>,
    meshes: Vec<regia_render::LabMesh>,
    preset: RenderProfilePreset,
) -> Vec<u8> {
    frame_on(device(), ground(), lights, meshes, preset)
}

fn frame_on(
    device: &RenderDevice,
    draw: TerrainSharedDraw,
    lights: Vec<LabLocalLight>,
    meshes: Vec<regia_render::LabMesh>,
    preset: RenderProfilePreset,
) -> Vec<u8> {
    let post = PostSettings {
        exposure_ev: EV,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 0.0),
        local_lights: lights,
        materials: vec![regia_render::LabMaterial {
            base_color: [0.18, 0.18, 0.18, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes,
    };
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.taau = false;
    quality.gi_probes = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let px = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    if let Some(dir) = regia_core::brand::env_var_os("LOCAL_LIGHTS_DUMP") {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::path::Path::new(&dir).join(format!("local_lights_{n:02}.png"));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&px);
            }
        }
    }
    px
}

fn srgb_to_linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Display-linear luma of a pixel.
fn luma(px: &[u8], x: u32, y: u32) -> f64 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * srgb_to_linear(px[i]) + 0.7152 * srgb_to_linear(px[i + 1]) + 0.0722 * srgb_to_linear(px[i + 2])
}

/// Mean luma over the `(2r+1)²` pixels around `(x, y)`.
fn patch(px: &[u8], x: u32, y: u32, r: u32) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for yy in y - r..=y + r {
        for xx in x - r..=x + r {
            sum += luma(px, xx, yy);
            n += 1.0;
        }
    }
    sum / n
}

/// The screen pixel of world point `p` (the frame's camera).
fn project(p: [f32; 3]) -> (u32, u32) {
    use regia_core::glam::camera::rh::{proj::directx::perspective, view::look_at_mat4};
    use regia_core::glam::{Vec3, Vec4};
    let view = look_at_mat4(Vec3::from(EYE), Vec3::ZERO, Vec3::Y);
    let proj = perspective(0.9, W as f32 / H as f32, 0.1, 2000.0);
    let clip = proj * view * Vec4::new(p[0], p[1], p[2], 1.0);
    let ndc = clip / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

/// 1000 lux at `d` metres from a point light: lm = 4π · E · d².
fn lumens_for_lux(lux: f32, d: f32) -> f32 {
    4.0 * std::f32::consts::PI * lux * d * d
}

#[test]
fn a_point_light_near_the_ground_lights_the_ground() {
    let lamp = [0.0, 3.0, 0.0];
    let dark = frame(Vec::new(), Vec::new(), RenderProfilePreset::DesktopHigh);
    let lit = frame(
        vec![point(lamp, lumens_for_lux(1000.0, 3.0), false)],
        Vec::new(),
        RenderProfilePreset::DesktopHigh,
    );
    let (cx, cy) = project([0.0, 0.0, 0.0]);
    let below_dark = patch(&dark, cx, cy, 3);
    let below_lit = patch(&lit, cx, cy, 3);
    // 9 m out (grazing, inverse square) the lamp gives about 3 % of the light below it.
    let (fx, fy) = project([8.0, 0.0, 4.0]);
    let far_dark = patch(&dark, fx, fy, 2);
    let far_lit = patch(&lit, fx, fy, 2);
    eprintln!("under the lamp: {below_dark:.4} -> {below_lit:.4}; 9 m out: {far_dark:.4} -> {far_lit:.4}");
    assert!(
        below_lit > below_dark + 0.1 && below_lit > 5.0 * below_dark.max(1e-3),
        "a lamp 3 m above the ground does not light it: {below_dark:.4} -> {below_lit:.4}"
    );
    assert!(
        far_lit < below_lit * 0.2,
        "the lamp's light does not fall off: {far_lit:.4} 9 m out vs {below_lit:.4} below"
    );
}

/// A 0.18 slab 1 cm above the 0.18 ground, a shadowless lamp 6 m above it: the slab's top and
/// the ground under it (the same pixels without the slab) print the same.
#[test]
fn a_grey_mesh_and_grey_terrain_print_the_same_under_a_lamp() {
    let mut fails = Vec::new();
    for (name, lamp) in [("overhead", [0.0, 6.0, 0.0]), ("oblique", [5.0, 4.0, 2.0])] {
        let lights = || {
            vec![point(
                lamp,
                lumens_for_lux(
                    1000.0,
                    (lamp[0] * lamp[0] + lamp[1] * lamp[1] + lamp[2] * lamp[2]).sqrt(),
                ),
                false,
            )]
        };
        let slab = box_at([2.0, 0.05, 2.0], [0.0, -0.04, 0.0]);
        let with = frame(lights(), vec![slab], RenderProfilePreset::DesktopHigh);
        let without = frame(lights(), Vec::new(), RenderProfilePreset::DesktopHigh);
        let (cx, cy) = project([0.0, 0.0, 0.0]);
        let (ground, mesh) = (patch(&without, cx, cy, 4), patch(&with, cx, cy, 4));
        let ratio = mesh / ground.max(1e-6);
        eprintln!("{name}: ground {ground:.4} mesh {mesh:.4} mesh/ground {ratio:.3}");
        if ground < 0.05 || (ratio - 1.0).abs() > 0.05 {
            fails.push(format!("{name}: ground {ground:.4} mesh {mesh:.4} ({ratio:.3})"));
        }
    }
    assert!(
        fails.is_empty(),
        "a 0.18 mesh and 0.18 terrain under a lamp differ: {fails:?}"
    );
}

/// A shadow-casting lamp 6 m up with a 1.2 m box floating 3 m below it: the ground under the
/// box is in the lamp's shadow (the forward pass's shadow atlas), the same lamp shadowless
/// lights it.
#[test]
fn a_shadowed_lamp_casts_a_caster_shadow_on_the_ground() {
    let lamp = [0.0, 6.0, 0.0];
    let caster = || vec![box_at([0.6, 0.6, 0.6], [0.0, 3.0, 0.0])];
    let shadowed = frame(
        vec![point(lamp, lumens_for_lux(1000.0, 6.0), true)],
        caster(),
        RenderProfilePreset::DesktopHigh,
    );
    let open = frame(
        vec![point(lamp, lumens_for_lux(1000.0, 6.0), false)],
        caster(),
        RenderProfilePreset::DesktopHigh,
    );
    let (cx, cy) = project([0.0, 0.0, 0.0]);
    let (s, o) = (patch(&shadowed, cx, cy, 2), patch(&open, cx, cy, 2));
    // Outside the umbra (4 m out) both print the lamp.
    let (ox, oy) = project([4.0, 0.0, 0.0]);
    let (s_out, o_out) = (patch(&shadowed, ox, oy, 2), patch(&open, ox, oy, 2));
    eprintln!("under the box: shadowless {o:.4} shadowed {s:.4}; 4 m out: {o_out:.4} / {s_out:.4}");
    assert!(o > 0.05, "the lamp does not light the ground: {o:.4}");
    assert!(
        s < 0.3 * o,
        "the box casts no lamp shadow on the ground: {s:.4} vs {o:.4} unshadowed"
    );
    assert!(
        (s_out - o_out).abs() < 0.1 * o_out,
        "the lamp shadow reaches past the umbra: {s_out:.4} vs {o_out:.4}"
    );
}

/// A spot light 5 m up pointing straight down with a 20° cone lights the ground inside the cone
/// and not 5 m to the side.
#[test]
fn a_spot_light_lights_only_its_cone() {
    let spot = LabLocalLight::Spot(LabSpotLight {
        position: [0.0, 5.0, 0.0],
        direction: [0.0, -1.0, 0.0],
        color: [1.0, 1.0, 1.0],
        intensity: 1000.0 * 25.0,
        range: 30.0,
        inner_angle: 15f32.to_radians(),
        outer_angle: 20f32.to_radians(),
        cast_shadows: false,
        source_radius: None,
    });
    let dark = frame(Vec::new(), Vec::new(), RenderProfilePreset::DesktopHigh);
    let lit = frame(vec![spot], Vec::new(), RenderProfilePreset::DesktopHigh);
    let (cx, cy) = project([0.0, 0.0, 0.0]);
    let (sx, sy) = project([5.0, 0.0, 0.0]);
    let inside = patch(&lit, cx, cy, 2) - patch(&dark, cx, cy, 2);
    let outside = patch(&lit, sx, sy, 2) - patch(&dark, sx, sy, 2);
    eprintln!("spot: inside +{inside:.4}, 5 m to the side +{outside:.4}");
    assert!(
        inside > 0.1,
        "the spot does not light the ground in its cone: +{inside:.4}"
    );
    assert!(
        outside.abs() < 0.01,
        "the spot lights the ground outside its cone: +{outside:.4}"
    );
}

/// A full device binds the light bindings; the min-spec Mobile tier (8 storage buffers per
/// stage) fits them exactly with the virtual texture; one storage buffer fewer does not.
#[test]
fn the_shared_path_binds_the_lights_on_a_capable_device() {
    let draw = ground();
    assert!(
        draw.gpu().local_lights_supported(),
        "a desktop / lavapipe device has room for the light bindings"
    );
    let mut limits = regia_render::MinspecProfile::Mobile.clamp_limits(&wgpu::Limits {
        max_bind_groups: 8,
        ..wgpu::Limits::default()
    });
    let vt = wgpu::Features::SHADER_EARLY_DEPTH_TEST;
    assert!(
        regia_terrain::gpu::local_lights_fit(&limits, vt),
        "Mobile: 8 storage buffers fit"
    );
    limits.max_storage_buffers_per_shader_stage = 7;
    assert!(!regia_terrain::gpu::local_lights_fit(&limits, vt));
    assert!(regia_terrain::gpu::local_lights_fit(
        &limits,
        wgpu::Features::empty()
    ));
}

/// Lights off before the warm: the shared pipelines are built from the shader without the
/// light loop (the v0.1 code and cost), and a lamp then adds exactly nothing to the ground.
/// Turning them back on rebuilds the lit variant at the call, not on the frame.
#[test]
fn lights_off_builds_the_colour_pass_without_the_light_loop() {
    let format = wgpu::TextureFormat::Rgba16Float;
    let mut lit = ground();
    lit.prewarm(format);
    assert_eq!(
        lit.gpu().shared_pipelines_lit(),
        Some(true),
        "default: the lit variant"
    );

    let mut off = ground();
    off.set_local_lights_enabled(false);
    assert!(!off.gpu().local_lights_compiled());
    off.prewarm(format);
    assert_eq!(
        off.gpu().shared_pipelines_lit(),
        Some(false),
        "lights off at the warm: the variant without the loop"
    );
    let lamp = vec![point([0.0, 3.0, 0.0], lumens_for_lux(1000.0, 3.0), false)];
    let with_lamp = frame_on(device(), off, lamp, Vec::new(), RenderProfilePreset::DesktopHigh);
    let dark = frame(Vec::new(), Vec::new(), RenderProfilePreset::DesktopHigh);
    let (cx, cy) = project([0.0, 0.0, 0.0]);
    let (a, b) = (patch(&dark, cx, cy, 3), patch(&with_lamp, cx, cy, 3));
    eprintln!("lights off: under the lamp {a:.4} (no lamp) vs {b:.4} (lamp)");
    assert!(
        (a - b).abs() < 1e-6,
        "lights off, a lamp still lights the ground: {a:.4} -> {b:.4}"
    );

    let mut toggled = ground();
    toggled.set_local_lights_enabled(false);
    toggled.prewarm(format);
    toggled.set_local_lights_enabled(true);
    assert_eq!(
        toggled.gpu().shared_pipelines_lit(),
        Some(true),
        "back on after the warm: the lit variant, built at the call"
    );
    let relit = frame_on(
        device(),
        toggled,
        vec![point([0.0, 3.0, 0.0], lumens_for_lux(1000.0, 3.0), false)],
        Vec::new(),
        RenderProfilePreset::DesktopHigh,
    );
    let c = patch(&relit, cx, cy, 3);
    assert!(
        c > a + 0.1,
        "back on, the lamp does not light the ground: {a:.4} -> {c:.4}"
    );
}
