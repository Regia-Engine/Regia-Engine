// SPDX-License-Identifier: MPL-2.0
//! A terrain layer's ORM blue channel as its wet-weather porosity (`.terrain.layers.ron` v5
//! `porosity_from_orm`, ADR-3367).
//!
//! Spec 99 gives every terrain layer a porosity: how much a wet layer darkens. Before v5 it came
//! from the layer's roughness or a single number in its weather block, and the ORM map's blue
//! channel (glTF metallic) was never read. A layer that opts in now reads its porosity per texel
//! from that channel, with the colour pass's own ORM tap. Existing maps are not reinterpreted:
//! without the flag the blue channel stays unread, whatever it holds.
//!
//! Two terraces, the high one with an ORM map (R 255, G 255 = roughness 1, so the derived
//! porosity is 1), rained on. Checks: without the flag the high terrace darkens by the derived
//! porosity whatever its blue; with the flag, blue 0 darkens it less, blue 255 as much as the
//! derived porosity, and blue 128 in between; a named porosity still wins; the low terrace never
//! moves; and the dry frame is the same bytes with the flag on or off. Lavapipe, no goldens.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    SsaoMode, SurfaceWeatherFrame,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, LayerWeather, MaterialParams, Rgba8Image, TerrainLayers,
    TerrainMap, TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

/// Serialises device use across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

const W: u32 = 160;
const H: u32 = 90;
const EDGE: usize = 129;
const WORLD_M: f32 = 128.0;
const HIGH: (u32, u32) = (10, 60);
const LOW: (u32, u32) = (110, 150);
const ROWS: (u32, u32) = (45, 85);

/// Two terraces: `low` without an ORM map, `high` with one whose blue channel is `blue`, and
/// the high layer's weather block `weather`.
fn terraces(blue: u8, weather: Option<LayerWeather>) -> TerrainSharedDraw {
    let layer = |name: &str, lo: f32, hi: f32| LayerDesc {
        name: name.into(),
        albedo: [1.0, 1.0, 1.0],
        roughness: 1.0,
        slope_min: 0.0,
        slope_max: 1.6,
        altitude_min: lo,
        altitude_max: hi,
        triplanar: false,
        ..LayerDesc::ground()
    };
    let stack = TerrainLayers {
        layers: vec![
            layer("low", -100.0, 2.0),
            LayerDesc {
                orm_texture: Some("high_orm.png".into()),
                weather,
                ..layer("high", 2.0, 100.0)
            },
        ],
        ..Default::default()
    };
    let mut samples = Vec::with_capacity(EDGE * EDGE);
    for _j in 0..EDGE {
        for i in 0..EDGE {
            let x = i as f32 - 64.0;
            samples.push((2.5 * x).clamp(0.0, 5.0));
        }
    }
    let map = TerrainMap::from_heights(EDGE as u32, EDGE as u32, samples, WORLD_M, 80.0, -40.0).expect("map");
    let v = regia_terrain::linear_to_srgb8(0.3);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    let set = |orm: Option<[u8; 4]>| LayerTextures {
        albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
        normal: Rgba8Image::neutral_normal(64),
        height: Rgba8Image::neutral_height(64),
        orm: Rgba8Image::solid(64, 64, orm.unwrap_or([255, 255, 0, 255])),
        use_orm: orm.is_some(),
    };
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(set(None)), Some(set(Some([255, 255, blue, 255])))],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    draw
}

fn renderer(draw: TerrainSharedDraw) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer
}

/// One frame of the shared draw (Desktop High without history passes).
fn frame(renderer: &mut FrameRenderer) -> Vec<u8> {
    let post = PostSettings {
        exposure_ev: 11.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 12.0, -40.0],
        target: [0.0, 0.0, 10.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 11.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.2, -0.7, 0.6], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.ssao = SsaoMode::Off;
    quality.gi = GiMode::Ambient;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.cloud_reprojection = false;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    renderer.render_offscreen(device(), &scene).expect("frame").pixels
}

fn settled(renderer: &mut FrameRenderer) -> Vec<u8> {
    for _ in 0..3 {
        frame(renderer);
    }
    frame(renderer)
}

/// Mean brightness of the pixels of columns `x0..x1`, rows `y0..y1`.
fn mean(pixels: &[u8], (x0, x1): (u32, u32), (y0, y1): (u32, u32)) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let at = ((y * W + x) * 4) as usize;
            sum += f32::from(pixels[at]) + f32::from(pixels[at + 1]) + f32::from(pixels[at + 2]);
            n += 3.0;
        }
    }
    sum / n
}

/// The dry frame, and the soaked/dry brightness ratio of the high and low terraces.
fn rain(blue: u8, weather: Option<LayerWeather>) -> (Vec<u8>, Vec<u8>, f32, f32) {
    let mut renderer = renderer(terraces(blue, weather));
    let dry = settled(&mut renderer);
    renderer.set_surface_weather(SurfaceWeatherFrame {
        wetness: 1.0,
        ..SurfaceWeatherFrame::DRY
    });
    let soaked = settled(&mut renderer);
    let ratio = |x| mean(&soaked, x, ROWS) / mean(&dry, x, ROWS);
    let (high, low) = (ratio(HIGH), ratio(LOW));
    (dry, soaked, high, low)
}

fn from_orm() -> Option<LayerWeather> {
    Some(LayerWeather {
        porosity_from_orm: true,
        ..LayerWeather::default()
    })
}

#[test]
fn an_orm_blue_layer_wets_by_its_porosity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Without the flag the blue channel is not read: blue 0 and 255 rain alike, by the derived
    // porosity (roughness 1 → porosity 1).
    let (dry_off, soaked_0, default_high, default_low) = rain(0, None);
    let (_, soaked_255, _, _) = rain(255, None);
    assert!(
        soaked_0 == soaked_255,
        "an unflagged ORM blue changed the wet frame"
    );
    assert!(
        default_high < 0.9 && default_low < 0.9,
        "default: both darken ({default_high}, {default_low})"
    );

    // With it, blue 0 (sheds a film) darkens less, blue 255 (soaks) as much as porosity 1, and
    // blue 128 in between.
    let (dry_on, _, shed, shed_low) = rain(0, from_orm());
    let (_, _, soak, soak_low) = rain(255, from_orm());
    let (_, _, half, _) = rain(128, from_orm());
    assert!(
        shed > default_high + 0.05,
        "ORM blue 0 darkens less: {shed} vs {default_high}"
    );
    assert!(
        (soak - default_high).abs() < 0.02,
        "ORM blue 255 is porosity 1: {soak} vs {default_high}"
    );
    assert!(
        shed > half + 0.01 && half > soak + 0.01,
        "blue 128 lies between: {shed} > {half} > {soak}"
    );
    for low in [shed_low, soak_low] {
        assert!(
            (low - default_low).abs() < 0.02,
            "the low terrace is unchanged: {low} vs {default_low}"
        );
    }

    // A named porosity wins over the ORM map.
    let (_, _, named, _) = rain(
        0,
        Some(LayerWeather {
            porosity: Some(1.0),
            porosity_from_orm: true,
            ..LayerWeather::default()
        }),
    );
    assert!(
        (named - default_high).abs() < 0.02,
        "porosity 1 wins over blue 0: {named}"
    );

    // The dry frame does not read the weather block: the same bytes with the flag on or off.
    assert!(dry_on == dry_off, "the flag moved the dry frame");
}
