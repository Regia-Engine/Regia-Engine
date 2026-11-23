// SPDX-License-Identifier: MPL-2.0
//! The terrain cost debug views compile out of the shared colour pass while none is on (ADR-1491).
//!
//! `terrain.wgsl`'s `cost_off` and `ab_off` read the frame's debug view in every pixel's layer
//! loop; on the RTX 5090 that test cost the terrain forward pass 0.026 ms (DO27 switchbacks
//! census) although it is false for every view but 78-92. Their bodies sit between
//! `// @cost-views-begin` / `-end` markers with a `// @cost-views-off return false;` line, and the
//! shared pipelines compile `without_cost_views` of the source unless
//! `cost_view_needs_full_shader` says the frame's view needs the full one.
//!
//! The checks: the stripped functions are `return false;` and the full ones are the two tests
//! this file mirrors; with the mirror, every call site in the shader is false for every view
//! outside the full-shader range, so the stripped shader draws what the full one draws there;
//! and on a GPU a cost view switches the pipelines (the picture changes), while switching back
//! and forth after the first switch compiles nothing and draws the same pictures.

use std::sync::OnceLock;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    SsaoMode,
};
use regia_terrain::gpu::{
    cost_view_needs_full_shader, shared_source, without_cost_views, without_ground_look,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const TERRAIN_WGSL: &str = include_str!("../src/gpu/terrain.wgsl");

/// Serialises device use across this binary's test threads (ADR-1438), and keeps the
/// process-wide pipeline counter to one test at a time.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The code lines (comments and blank lines dropped) of `fn name(` through its closing brace.
fn function_code(source: &str, name: &str) -> Vec<String> {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("`fn {name}` not found"));
    let mut depth = 0usize;
    let mut out = Vec::new();
    for line in source[start..].lines() {
        let code = line.trim();
        if !code.is_empty() && !code.starts_with("//") {
            out.push(code.to_owned());
        }
        depth += code.matches('{').count();
        depth -= code.matches('}').count();
        if depth == 0 && code.ends_with('}') {
            return out;
        }
    }
    panic!("`fn {name}` is not closed");
}

/// Whether some line of `source` starts with `marker` (after indentation).
fn marker_lines(source: &str, marker: &str) -> bool {
    source.lines().any(|line| line.trim_start().starts_with(marker))
}

/// `terrain.wgsl` `cost_off(view)` at debug view `v` (the full shader's body).
fn cost_off(v: f32, view: f32) -> bool {
    let cached = v > 77.5 && v < 78.5 && (view == 83.0 || view == 86.0 || view == 87.0 || view == 88.0);
    cached || (v > view - 0.5 && v < view + 0.5)
}

/// `terrain.wgsl` `ab_off(view)` at debug view `v`.
fn ab_off(v: f32, view: f32) -> bool {
    cost_off(v, view) || (v > 91.5 && v < 92.5)
}

/// The view arguments of every `name(<n>.0)` call in `source`.
fn call_views(source: &str, name: &str) -> Vec<f32> {
    let key = format!("{name}(");
    let mut out = Vec::new();
    for (at, _) in source.match_indices(&key) {
        // Skip the definition (`fn cost_off(view: f32)`).
        if source[..at].ends_with("fn ") {
            continue;
        }
        let rest = &source[at + key.len()..];
        let arg = &rest[..rest.find(')').expect("closing parenthesis")];
        // `ab_off` forwards its own argument (`cost_off(view)`): the mirror covers it.
        if arg == "view" {
            continue;
        }
        out.push(
            arg.parse::<f32>()
                .unwrap_or_else(|e| panic!("{name}({arg}): a literal view number expected ({e})")),
        );
    }
    out
}

#[test]
fn the_full_tests_are_the_mirrored_ones_and_the_stripped_ones_are_false() {
    assert_eq!(
        function_code(TERRAIN_WGSL, "cost_off"),
        [
            "fn cost_off(view: f32) -> bool {",
            "let v = frame.ambient_debug.a;",
            "let cached = v > 77.5 && v < 78.5 && (view == 83.0 || view == 86.0 || view == 87.0 || view == 88.0);",
            "return cached || (v > view - 0.5 && v < view + 0.5);",
            "}",
        ],
        "cost_off changed: update the mirror here and `cost_view_needs_full_shader`"
    );
    assert_eq!(
        function_code(TERRAIN_WGSL, "ab_off"),
        [
            "fn ab_off(view: f32) -> bool {",
            "let v = frame.ambient_debug.a;",
            "return cost_off(view) || (v > 91.5 && v < 92.5);",
            "}",
        ],
        "ab_off changed: update the mirror here and `cost_view_needs_full_shader`"
    );
    for source in [
        without_cost_views(TERRAIN_WGSL),
        without_cost_views(&without_ground_look(&shared_source(false, true))),
        without_cost_views(&without_ground_look(&shared_source(true, false))),
    ] {
        for name in ["cost_off", "ab_off"] {
            let code = function_code(&source, name);
            assert_eq!(code.len(), 3, "{name}: {code:?}");
            assert_eq!(code[1], "return false;", "{name}: {code:?}");
        }
        assert!(!marker_lines(&source, "// @cost-views"), "a marker survived");
    }
    // The ground look's markers are not the cost views' (and the reverse).
    let both = without_cost_views(&without_ground_look(TERRAIN_WGSL));
    assert_eq!(both, without_ground_look(&without_cost_views(TERRAIN_WGSL)));
    assert!(!marker_lines(&both, "// @ground-look"));
}

#[test]
fn every_call_site_is_false_outside_the_full_shader_range() {
    let cost = call_views(TERRAIN_WGSL, "cost_off");
    let ab = call_views(TERRAIN_WGSL, "ab_off");
    assert!(cost.len() >= 10 && !ab.is_empty(), "call sites: {cost:?} {ab:?}");
    for v in 0..=1000u32 {
        let needs = cost_view_needs_full_shader(v);
        let any = cost.iter().any(|&view| cost_off(v as f32, view))
            || ab.iter().any(|&view| ab_off(v as f32, view));
        if !needs {
            assert!(
                !any,
                "view {v} changes the shading but compiles the stripped shader"
            );
        }
    }
    // The range is not wider than it needs to be at its ends.
    for v in [78, 92] {
        let any = cost.iter().any(|&view| cost_off(v as f32, view))
            || ab.iter().any(|&view| ab_off(v as f32, view));
        assert!(any && cost_view_needs_full_shader(v), "view {v}");
    }
    assert!(
        !cost_view_needs_full_shader(0)
            && !cost_view_needs_full_shader(77)
            && !cost_view_needs_full_shader(93)
    );
}

#[test]
fn the_stripper_handles_unmarked_and_broken_sources() {
    let plain = "fn a() -> bool {\n    return true;\n}\n";
    assert_eq!(without_cost_views(plain), plain);
    let open = "fn a() -> bool {\n    // @cost-views-begin\n    return true;\n}\n";
    assert_eq!(without_cost_views(open), open, "unbalanced: unchanged");
    let marked = "fn a() -> bool {\n    // @cost-views-begin\n    return t();\n    // @cost-views-end\n    // @cost-views-off return false;\n}\n";
    assert_eq!(
        without_cost_views(marked),
        "fn a() -> bool {\n    return false;\n}\n"
    );
}

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

const W: u32 = 160;
const H: u32 = 90;

/// Flat 0.3-grey photoreal ground, 128 m across (as `ground_look_variant.rs`).
fn ground() -> TerrainSharedDraw {
    let map = TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 128.0, 80.0, -40.0).expect("plane");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.3);
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

/// One frame of the shared draw in `renderer` (Desktop High without history passes).
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

/// Pixels whose colour differs between `a` and `b`.
fn differing(a: &[u8], b: &[u8]) -> usize {
    let (a, b) = (a.as_chunks::<4>().0, b.as_chunks::<4>().0);
    a.iter().zip(b).filter(|(p, q)| p[..3] != q[..3]).count()
}

/// Cost view 80 (no layers) through the draw's handle (the agent's `terrain.set_debug_view`
/// path): the full shader's colour pipeline is built once and the picture changes. Off again and
/// on again: no compile, the same pictures.
#[test]
fn a_cost_view_switches_the_pipelines_and_switching_back_compiles_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let draw = ground();
    let view = draw.debug_view_handle();
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let off = frame(&mut renderer);

    let before = pipelines_created();
    view.set(80);
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let on = frame(&mut renderer);
    let first_on = pipelines_created() - before;
    assert!(
        first_on >= 1,
        "the first cost view builds the full shader's colour pipeline"
    );

    let before = pipelines_created();
    view.set(0);
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let off_again = frame(&mut renderer);
    view.set(80);
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let on_again = frame(&mut renderer);
    let switches = pipelines_created() - before;
    println!(
        "cost view 80: {first_on} creates, {} px changed; off and on again: {switches} creates, {} / {} px changed",
        differing(&off, &on),
        differing(&off, &off_again),
        differing(&on, &on_again)
    );
    assert_eq!(
        switches, 0,
        "switching back and forth after the first switch compiles nothing"
    );
    assert!(
        differing(&off, &on) * 20 > (W * H) as usize,
        "view 80 shows: {} of {} px changed",
        differing(&off, &on),
        W * H
    );
    assert_eq!(differing(&off, &off_again), 0, "off again draws the off picture");
    assert_eq!(differing(&on, &on_again), 0, "on again draws the on picture");
}
