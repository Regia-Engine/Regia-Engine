// SPDX-License-Identifier: MPL-2.0
//! Sun cascades at a grazing sun (terrain-look round 9).
//!
//! On DO27's bore_twelve_close the slope beyond the crest printed in cascade shadow below a
//! ~30° sun while the heightfield and a CPU raycast say it is lit. Here a slope falls away from
//! a 25° sun at 17° (N·L = sin 8° ≈ 0.14: grazing, but lit) just beyond the crest the camera
//! stands on; the cascade shadow alone (terrain debug view 11) must call it lit.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 320;
const H: u32 = 180;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Flat for x < 0, falling at `slope_deg` toward +x beyond (a crest along z at x = 0), with
/// an optional 40 m ridge 40–45 m up-sun (x = −45..−40).
fn crest(slope_deg: f32, ridge: bool) -> TerrainMap {
    let n = 513u32;
    let width = 128.0f32;
    let k = slope_deg.to_radians().tan();
    let mut heights = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let wall = if ridge && (-45.0..=-40.0).contains(&x) {
                40.0
            } else {
                0.0
            };
            heights.push(-x.max(0.0) * k + wall);
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 64.0, -60.0).expect("crest")
}

/// The cascade shadow (debug view 11, grey) of the frame: 1 = lit.
fn cascade_shadow(sun_elevation_deg: f32, slope_deg: f32, relief: f32, ridge: bool) -> Vec<u8> {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
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
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw = TerrainSharedDraw::new(
        device(),
        crest(slope_deg, ridge),
        &stack,
        CdlodConfig::default(),
        3,
    )
    .expect("terrain");
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
    draw.set_relief(TerrainRelief {
        cliff_relief: relief,
        ..TerrainRelief::default()
    });
    draw.set_debug_view(11);
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let el = sun_elevation_deg.to_radians();
    // The sun stands toward −x: it travels toward +x and down.
    let travel = [el.cos(), -el.sin(), 0.0];
    let scene = LabScene {
        width: W,
        height: H,
        eye: [-2.0, 1.75, 0.0],
        target: [6.0, -1.5, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new(travel, [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    if let Some(dir) = regia_core::brand::env_var_os("GRAZING_SHADOW_DUMP") {
        let path = std::path::Path::new(&dir).join(format!(
            "crest_sun{sun_elevation_deg}_relief{relief}_ridge{ridge}.png"
        ));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        }
    }
    pixels
}

/// Fraction of the lower half of the frame (ground only: the horizon sits near 40 % down;
/// the crest and the slope beyond it) the cascades leave lit: grey at least half the
/// brightest pixel.
fn lit_fraction(px: &[u8]) -> f64 {
    let grey = |x: u32, y: u32| px[((y * W + x) * 4) as usize];
    let max = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| grey(x, y))
        .max()
        .unwrap_or(0);
    let mut lit = 0;
    let mut n = 0;
    for y in H / 2..H {
        for x in 0..W {
            n += 1;
            if u32::from(grey(x, y)) * 2 >= u32::from(max) {
                lit += 1;
            }
        }
    }
    f64::from(lit) / f64::from(n.max(1))
}

#[test]
fn a_slope_grazed_by_a_25_degree_sun_is_lit_by_the_cascades() {
    for relief in [0.0, 1.0] {
        let px = cascade_shadow(25.0, 17.0, relief, false);
        let lit = lit_fraction(&px);
        eprintln!(
            "25° sun, 17° slope, relief {relief}: cascades leave {:.1} % lit",
            lit * 100.0
        );
        assert!(
            lit > 0.97,
            "the cascades shadow a sunlit slope: {:.1} % lit (relief {relief})",
            lit * 100.0
        );
    }
}

#[test]
fn a_ridge_up_sun_still_shadows_the_slope() {
    // The other side: a 40 m ridge 40 m toward the 25° sun puts the whole view in shadow
    // (it reaches 86 m down-sun); the cascades must not bias it away.
    let px = cascade_shadow(25.0, 17.0, 1.0, true);
    let lit = lit_fraction(&px);
    eprintln!(
        "25° sun behind a 40 m ridge: cascades leave {:.1} % lit",
        lit * 100.0
    );
    assert!(
        lit < 0.05,
        "the ridge's shadow is missing: {:.1} % lit",
        lit * 100.0
    );
}
