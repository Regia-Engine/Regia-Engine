// SPDX-License-Identifier: MPL-2.0
//! `FillCanyonScope` on the shared terrain path (ADR-8412, DO27 request look-fill-canyon-scoped).
//!
//! Airless flat ground lit by the sky body's fill only (the sun at 0 lux, no sky light, SSAO and
//! the irradiance probes off), with a trench 40 m away and one hull box sent as a mesh occluder
//! (`FrameRenderer::set_mesh_occluders`; no mesh is drawn, so only the proxy terms move). The
//! layers' ORM occlusion is 0.5, so the full indirect stack halves the fill and the
//! canyon-scale term (`min(macro AO, SSAO)` = 1 here) does not.
//!
//! - `NearOccluders` brightens the ground in the hull's sun shade (beside it, on the side away
//!   from the sun and the fill reaches) and leaves the trench floor and an open control patch
//!   byte for byte as `Off` prints them.
//! - `MapWide` lifts all three (the old `set_fill_canyon_visibility(true)`).
//! - `NearOccluders` with no occluder prints the `Off` frame byte for byte.
//!
//! Each frame is the third of its own never-switched renderer, so every comparison is at the
//! same frame index. No golden: the checks are differences between runs on one device.

use std::f32::consts::FRAC_1_SQRT_2;
use std::sync::OnceLock;

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    MeshOccluder, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SharedPassDraw, SkyBody, SsaoMode, exposure_from_ev100, inverse_tonemap_agx,
};
use regia_terrain::{
    CdlodConfig, FillCanyonScope, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers,
    TerrainMap, TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 192;
const H: u32 = 108;
const EV: f32 = 9.0;
const FOV_Y: f32 = 0.9;
const EYE: [f32; 3] = [10.0, 45.0, -30.0];
const TARGET: [f32; 3] = [10.0, 0.0, 0.0];
/// The hull: a 4 × 2 × 2 m box standing on the ground at x = −10.
const HULL: [f32; 3] = [-10.0, 1.0, 0.0];
/// Ground beside the hull on its shadow side (the sun is toward +X): inside its sun-shadow
/// polygon (x −14..−8), and the fill (from −X, +Z) reaches it past the box.
const SHADE: [f32; 3] = [-13.0, 0.0, 0.0];
/// The trench floor (8 m deep, its 16 m floor centred on x = 30), 40 m from the hull.
const TRENCH: [f32; 3] = [30.0, -6.0, 0.0];
/// Open flat ground 18 m from the hull.
const CONTROL: [f32; 3] = [8.0, 0.0, 6.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Trench depth (m) at world x: 6 m deep, floor x 22..38, 2 m walls.
fn trench_depth(x: f32) -> f32 {
    6.0 * (1.0 - ((x - 30.0).abs() - 8.0).clamp(0.0, 2.0) / 2.0)
}

struct Frame {
    pixels: Vec<u8>,
    occluders_in_use: usize,
}

/// The third frame of a fresh renderer drawing the scene with `scope`, with or without the hull.
fn render(scope: FillCanyonScope, with_hull: bool) -> Frame {
    let n = 257u32;
    let width = 128.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push(-trench_depth(x));
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, width, 16.0, -10.0).expect("ground and trench");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ice".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.6);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                // ORM occlusion 0.5 (128 / 255): the full stack halves the fill.
                orm: Rgba8Image::solid(64, 64, [128, 255, 0, 255]),
                use_orm: true,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief {
        cavity: 1.0,
        ..TerrainRelief::off()
    });
    draw.set_fill_canyon_scope(scope);
    assert_eq!(draw.fill_canyon_scope(), scope);
    let post = PostSettings {
        exposure_ev: EV,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        // The sun 45° up toward +X, at 0 lux: it only places the hull's shadow polygon.
        directional: DirectionalLightUniform::new(
            [-FRAC_1_SQRT_2, -FRAC_1_SQRT_2, 0.0],
            [1.0, 1.0, 1.0],
            0.0,
        ),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.ssao = SsaoMode::Off;
    quality.screen_space_gi = false;
    quality.ssr = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let mut planet = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
    planet.ground_albedo = [0.6, 0.6, 0.6];
    renderer.set_outdoor(OutdoorParams {
        planet,
        // Jupiter-shine from −X and +Z, 45° up, grey.
        sky_body: Some(SkyBody {
            direction: [-0.5, FRAC_1_SQRT_2, 0.5],
            tint: [0.55, 0.55, 0.55],
            fill_lux: Some(1600.0),
            ..SkyBody::jupiter_from_europa()
        }),
        ..OutdoorParams::default()
    });
    if with_hull {
        renderer.set_mesh_occluders(vec![MeshOccluder {
            center: HULL,
            axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            half_extents: [2.0, 1.0, 1.0],
            albedo: [0.3, 0.3, 0.3],
            ground_y: 0.0,
            far_band: false,
        }]);
    }
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    }
    if let Some(dir) = regia_core::brand::env_var_os("FILL_CANYON_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("{}_hull{with_hull}.png", scope.name()));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        }
    }
    Frame {
        pixels,
        occluders_in_use: renderer.mesh_occluders_in_use(),
    }
}

/// Pixel of a world point.
fn project(p: [f32; 3]) -> (u32, u32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::from_array(TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.1, 2000.0);
    let clip: Vec4 = (proj * view as Mat4) * Vec3::from_array(p).extend(1.0);
    let ndc = clip.truncate() / clip.w;
    let (x, y) = ((ndc.x * 0.5 + 0.5) * W as f32, (0.5 - ndc.y * 0.5) * H as f32);
    assert!(
        x >= 2.0 && y >= 2.0 && x < (W - 2) as f32 && y < (H - 2) as f32,
        "{p:?} is off screen at ({x}, {y})"
    );
    (x as u32, y as u32)
}

/// The 3 × 3 patch around a pixel, RGB bytes.
fn patch(pixels: &[u8], at: (u32, u32)) -> Vec<u8> {
    let mut out = Vec::with_capacity(27);
    for y in at.1 - 1..=at.1 + 1 {
        for x in at.0 - 1..=at.0 + 1 {
            let i = ((y * W + x) * 4) as usize;
            out.extend_from_slice(&pixels[i..i + 3]);
        }
    }
    out
}

/// Scene luminance (cd/m²) of a 3 × 3 patch: the display value through the inverse AgX and
/// the exposure.
fn luminance(pixels: &[u8], at: (u32, u32)) -> f32 {
    let p = patch(pixels, at);
    let mut sum = [0.0f32; 3];
    for px in p.as_chunks::<3>().0 {
        for (s, &v) in sum.iter_mut().zip(px) {
            *s += f32::from(v) / 255.0;
        }
    }
    let rgb = inverse_tonemap_agx(sum.map(|v| v / 9.0));
    (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]) / exposure_from_ev100(EV)
}

#[test]
fn near_occluders_lifts_the_hull_shade_and_leaves_the_trench() {
    let off_bare = render(FillCanyonScope::Off, false);
    let off = render(FillCanyonScope::Off, true);
    let near = render(FillCanyonScope::NearOccluders, true);
    let wide = render(FillCanyonScope::MapWide, true);
    assert_eq!(off.occluders_in_use, 1, "the hull proxy did not reach the frame");
    assert_eq!(off_bare.occluders_in_use, 0);

    let shade = project(SHADE);
    let trench = project(TRENCH);
    let control = project(CONTROL);
    for (name, at) in [("shade", shade), ("trench", trench), ("control", control)] {
        eprintln!(
            "{name} {at:?}: off without hull {:.2}, off {:.2}, near_occluders {:.2}, map_wide {:.2} cd/m²",
            luminance(&off_bare.pixels, at),
            luminance(&off.pixels, at),
            luminance(&near.pixels, at),
            luminance(&wide.pixels, at),
        );
    }
    // The control patch is lit (the fill reaches open ground) and the hull does not touch it or
    // the trench: what moves there is the scope alone.
    assert!(
        luminance(&off.pixels, control) > 1.0,
        "the control patch is black: no fill?"
    );
    assert!(
        luminance(&off.pixels, trench) > 1.0,
        "the trench floor is black: no fill?"
    );
    assert_eq!(patch(&off.pixels, control), patch(&off_bare.pixels, control));
    assert_eq!(patch(&off.pixels, trench), patch(&off_bare.pixels, trench));

    // Near the hull: brighter than Off by most of the ORM occlusion's halving.
    let (l_off, l_near) = (luminance(&off.pixels, shade), luminance(&near.pixels, shade));
    assert!(l_off > 1.0, "the hull's shade gets no fill: {l_off}");
    assert!(
        l_near > 1.5 * l_off,
        "near_occluders does not lift the fill in the hull's shade: {l_near:.2} vs off {l_off:.2} cd/m²"
    );
    // Away from it: Off's bytes.
    assert_eq!(
        patch(&near.pixels, trench),
        patch(&off.pixels, trench),
        "near_occluders moved the trench floor 40 m from the hull"
    );
    assert_eq!(
        patch(&near.pixels, control),
        patch(&off.pixels, control),
        "near_occluders moved open ground 18 m from the hull"
    );
    // MapWide lifts all three.
    for (name, at) in [("shade", shade), ("trench", trench), ("control", control)] {
        let (a, b) = (luminance(&off.pixels, at), luminance(&wide.pixels, at));
        assert!(
            b > 1.5 * a,
            "map_wide does not lift the {name}: {b:.2} vs off {a:.2} cd/m²"
        );
    }
}

#[test]
fn near_occluders_without_occluders_prints_the_off_frame() {
    let off = render(FillCanyonScope::Off, false);
    let near = render(FillCanyonScope::NearOccluders, false);
    assert!(
        off.pixels == near.pixels,
        "near_occluders with no mesh occluder is not the off frame"
    );
}
