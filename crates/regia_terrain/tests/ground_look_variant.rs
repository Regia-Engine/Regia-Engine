// SPDX-License-Identifier: MPL-2.0
//! The ground look compiles out of the shared colour pass while it is off (ADR-1452).
//!
//! v0.2.0 drew DO27's switchbacks 9-10 % slower than v0.1.29 on the RTX 5090: the ground look's
//! code (ADR-1408), compiled into the terrain forward fragment shader but never taken (every
//! row zero), cost the pass 0.3-0.45 ms, most of it from one `max` with its local wetness.
//! `terrain.wgsl` marks every line of the look (`// @ground-look-begin` / `-end`, with
//! `// @ground-look-off` lines for the statements it changes), and the shared pipelines compile
//! `without_ground_look` of the source unless some ground-look row is non-zero.
//!
//! The checks: the stripped source has none of the look; every marked branch tests a row that
//! is zero when the look is off, so the stripped code is the code the full shader runs at zero
//! rows; the stripper leaves unmarked or unbalanced sources alone; the rows are zero exactly
//! when the look is off; and on a GPU the switch on changes the picture, while switching back
//! off and on again after the first switch compiles nothing and draws the same pictures.

use std::sync::OnceLock;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    SsaoMode,
};
use regia_terrain::gpu::{shared_source, without_ground_look};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainGroundLook, TerrainLayers,
    TerrainMap, TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const TERRAIN_WGSL: &str = include_str!("../src/gpu/terrain.wgsl");
const BEGIN: &str = "// @ground-look-begin";
const END: &str = "// @ground-look-end";

/// Serialises device use across this binary's test threads (ADR-1438), and keeps the
/// process-wide pipeline counter to one test at a time.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The look's names: none may survive `without_ground_look`.
const LOOK_NAMES: [&str; 9] = [
    "material.ground",
    "material.winter",
    "ground_remix",
    "ground_look(",
    "hoarfrost(",
    "ground_wet",
    "GroundLook",
    "shade_facing",
    "winter2",
];

/// The code lines (comments and blank lines dropped) of each marked region of `source`.
fn regions(source: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in source.lines() {
        let t = line.trim();
        if t.starts_with(BEGIN) {
            assert!(current.is_none(), "nested {BEGIN}");
            current = Some(Vec::new());
        } else if t.starts_with(END) {
            out.push(current.take().expect("an end without a begin"));
        } else if let Some(region) = current.as_mut()
            && !t.is_empty()
            && !t.starts_with("//")
        {
            region.push(t.to_owned());
        }
    }
    assert!(current.is_none(), "a begin without an end");
    out
}

/// A test that is false while every ground-look row is zero: `material.<row>.<lane> > 0.0`,
/// or a parenthesised `||` of such tests.
fn dead_at_zero_rows(term: &str) -> bool {
    let term = term.trim();
    let inner = term
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .unwrap_or(term);
    inner.split("||").all(|t| {
        let t = t.trim();
        let Some(rest) = t.strip_prefix("material.") else {
            return false;
        };
        let Some((field, cmp)) = rest.split_once(' ') else {
            return false;
        };
        let row_lane = field.split_once('.').is_some_and(|(row, lane)| {
            matches!(row, "ground" | "winter" | "winter2") && matches!(lane, "x" | "y" | "z" | "w")
        });
        row_lane && cmp.trim() == "> 0.0"
    })
}

/// The top-level `&&` terms of a condition (outside parentheses).
fn conjuncts(cond: &str) -> Vec<&str> {
    let mut depth = 0i32;
    let mut terms = Vec::new();
    let mut start = 0;
    let bytes = cond.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'&' if depth == 0 && bytes.get(i + 1) == Some(&b'&') => {
                terms.push(&cond[start..i]);
                start = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    terms.push(&cond[start..]);
    terms
}

/// The stripped shader has none of the look, the full one all of it, for `terrain.wgsl` and
/// for the shared colour pass's composed sources (probes, lights, hardware height).
#[test]
fn the_stripped_source_has_none_of_the_ground_look() {
    let sources = [
        ("terrain.wgsl", TERRAIN_WGSL.to_owned()),
        ("shared", shared_source(false, false)),
        ("shared, lights", shared_source(false, true)),
        ("shared, hw height, lights", shared_source(true, true)),
    ];
    for (name, full) in &sources {
        let off = without_ground_look(full);
        for look in LOOK_NAMES {
            assert!(full.contains(look), "{name}: the full source lost `{look}`");
            assert!(
                !off.contains(look),
                "{name}: `{look}` survives without_ground_look"
            );
        }
        assert!(
            !off.lines().any(|l| l.trim_start().starts_with("// @ground-look")),
            "{name}: a marker line survives"
        );
        assert!(off.len() < full.len());
    }
    // The off variants of the two statements the look changes are the old statements.
    let off = without_ground_look(TERRAIN_WGSL);
    assert!(off.contains("    let wetness = clamp(frame.sun_dir_wetness.w, 0.0, 1.0);\n"));
    assert!(off.contains(
        "            a_rgb = a_rgb * tri_sample(layer_albedo, st, slot.params.x, i, lod, g_top).rgb;\n"
    ));
}

/// Every marked region is module-scope look code, a local the look writes, or a call site
/// whose branch tests a row that is zero while the look is off; the statements with an
/// `@ground-look-off` variant read only the look's own locals besides the old operands. So with
/// every row zero the full shader runs exactly the stripped code.
#[test]
fn every_marked_branch_is_dead_at_zero_rows() {
    let regions = regions(TERRAIN_WGSL);
    assert_eq!(
        regions.len(),
        7,
        "struct rows, functions and five call-site regions: {regions:?}"
    );
    let mut branches = 0;
    for region in &regions {
        for line in region {
            if let Some(rest) = line.strip_prefix("if (") {
                // Branches inside the look's own functions are not call sites.
                if region.iter().any(|l| l.starts_with("fn ")) {
                    continue;
                }
                let cond = rest.strip_suffix(") {").expect("one-line branch");
                assert!(
                    conjuncts(cond).iter().any(|t| dead_at_zero_rows(t)),
                    "marked branch `if ({cond})` has no term that is false at zero rows"
                );
                branches += 1;
            }
        }
    }
    assert_eq!(branches, 3, "remix, ground_look() and hoarfrost call sites");
    // The look's calls sit only inside those branches (and its own functions).
    let stripped = without_ground_look(TERRAIN_WGSL);
    for call in ["ground_remix(", "ground_look(", "hoarfrost("] {
        assert!(!stripped.contains(call));
    }
}

/// `without_ground_look` leaves a source without markers alone, and a source with unbalanced
/// markers too (the full source draws the same pixels), and makes `@ground-look-off` code.
#[test]
fn the_stripper_handles_plain_and_broken_sources() {
    let plain = "fn a() -> f32 {\n    return 1.0;\n}\n";
    assert_eq!(without_ground_look(plain), plain);
    let marked =
        "a\n  // @ground-look-begin\n  look\n  // @ground-look-end\n  // @ground-look-off off();\nb\n";
    assert_eq!(without_ground_look(marked), "a\n  off();\nb\n");
    for broken in [
        "a\n// @ground-look-begin\nlook\n",
        "a\n// @ground-look-end\nb\n",
        "// @ground-look-begin\n// @ground-look-begin\n// @ground-look-end\n",
    ] {
        assert_eq!(without_ground_look(broken), broken);
    }
}

/// The GPU picks the off variant from the rows: they are all zero exactly when the look is
/// off, for the presets and for single terms (including out-of-range and non-finite input).
#[test]
fn rows_are_zero_exactly_when_the_look_is_off() {
    let off = TerrainGroundLook::OFF;
    let mut looks = vec![off, TerrainGroundLook::VARIED, TerrainGroundLook::LATE_WINTER];
    for v in [0.0, 1e-6, 0.5, -1.0, f32::NAN, f32::INFINITY] {
        looks.push(TerrainGroundLook { variation: v, ..off });
        looks.push(TerrainGroundLook { remix: v, ..off });
        looks.push(TerrainGroundLook { snow: v, ..off });
        looks.push(TerrainGroundLook {
            mud_wetness: v,
            ..off
        });
        looks.push(TerrainGroundLook { hoarfrost: v, ..off });
        looks.push(TerrainGroundLook {
            variation_scale_m: v,
            snow_line_m: v,
            ..off
        });
    }
    for look in looks {
        let zero = look.uniforms().iter().flatten().all(|v| *v == 0.0);
        assert_eq!(zero, !look.is_on(), "{look:?}");
    }
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

/// Flat 0.3-grey photoreal ground, 128 m across.
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

/// One frame of the shared draw in `renderer` (Desktop High without history passes, sun
/// from behind the eye).
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
    // No pass that carries history between frames (TAA, SSAO, screen-space GI and
    // reflections, probes, clouds, fog): the same inputs then draw the same pixels.
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

/// The switch on, mid-play through the draw's handle (the agent's `terrain.set_ground_look`
/// path): the ground-look colour pipeline is built once and the picture changes (the variation
/// moves the ground's brightness). Off again and on again: no compile, the same pictures.
#[test]
fn the_switch_changes_the_picture_and_switching_back_compiles_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let draw = ground();
    let look = draw.ground_look_handle();
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    // Settle the off frames (temporal passes), then keep the last off picture.
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let off = frame(&mut renderer);

    let before = pipelines_created();
    look.set(TerrainGroundLook::VARIED);
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let on = frame(&mut renderer);
    let first_on = pipelines_created() - before;
    assert!(
        first_on >= 1,
        "the first switch on builds the ground-look colour pipeline"
    );

    let before = pipelines_created();
    look.set(TerrainGroundLook::OFF);
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let off_again = frame(&mut renderer);
    look.set(TerrainGroundLook::VARIED);
    for _ in 0..3 {
        frame(&mut renderer);
    }
    let on_again = frame(&mut renderer);
    let switches = pipelines_created() - before;
    println!(
        "ground look on: {first_on} creates, {} px changed; off and on again: {switches} creates, {} / {} px changed",
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
        "the ground look shows once on: {} of {} px changed",
        differing(&off, &on),
        W * H
    );
    assert_eq!(differing(&off, &off_again), 0, "off again draws the off picture");
    assert_eq!(differing(&on, &on_again), 0, "on again draws the on picture");
}
