// SPDX-License-Identifier: MPL-2.0
//! Manual study of the Mobile Low golden's miss and of what the lite tier gives up on the golden
//! view (ADR-5945). Not acceptance: `the_lite_low_frame_agrees_with_the_full_frame` is. It stages
//! process-wide shader overrides, so it runs alone:
//!
//! ```text
//! REGIA_LIGHTMAP_LITE_STUDY=<dir> cargo test -p regia_pt --test lightmap_forward_quad_box -- \
//!     --ignored lite_low_golden_study --nocapture --test-threads=1
//! ```
//!
//! It prints one row per pair of frames (the harness's SSIM, the flat-window brightness step, the
//! largest channel difference and the pixels that differ) and, with the variable set, writes the
//! frames, the table and the owner's before/after plates there.

use super::*;

use regia_render::ShadowFilter;
use regia_render::lightmap::LightmapTier;
use regia_test_utils::golden::{pixel_delta, read_png_rgba8};

/// Where the study writes its frames, table and plates when set.
const STUDY_ENV: &str = "REGIA_LIGHTMAP_LITE_STUDY";
/// The plates' size: the golden view six times over, large enough to judge by eye.
const PLATE: (u32, u32) = (W * 6, H * 6);
/// The gain of a difference plate: one code value shows as this many.
const DIFF_GAIN: u8 = 32;

/// The lite shader before ADR-4930's energy correction (commit ab50abf39) on today's interface: its
/// own BRDF fit for the lights and the analytic split-sum fit for the lightmap's energy partition.
/// The same control `lightmap_pt_quality_study` stages for the room.
fn legacy_mobile_shader(current: &str) -> String {
    let mut source = current.to_owned();
    let begin = source
        .find("fn lite_brdf(")
        .expect("the lite BRDF (update this control with the shader)");
    let end = begin
        + source[begin..]
            .find("\n}\n")
            .expect("the lite BRDF's end (update this control with the shader)")
        + 3;
    let legacy = r"fn lite_brdf(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32, dfg: vec4<f32>) -> vec3<f32> {
    let a2 = perceptual * perceptual * perceptual * perceptual;
    let h = normalize(l + v);
    let no_h = max(dot(n, h), 0.0);
    let lo_h = max(dot(l, h), 0.0);
    let d = no_h * no_h * (a2 - 1.0) + 1.0;
    let vis = 1.0 / (4.0 * max(lo_h * lo_h, 0.01) * (perceptual + 0.5));
    let x = 1.0 - lo_h;
    let x2 = x * x;
    let f = f0 + (vec3<f32>(1.0) - f0) * (x2 * x2 * x);
    return (diffuse_color + f * min(a2 / max(d * d, 1e-6) * vis, 256.0)) * (1.0 / regia::pbr::lighting::PI);
}
";
    source.replace_range(begin..end, legacy);
    let patched = source.replace(
        "let lm_ab = dfg.rg;",
        "let lm_ab = env_brdf_approx(perceptual, no_v);",
    );
    assert_ne!(
        patched, source,
        "the lightmap's energy partition (update this control with the shader)"
    );
    patched
}

/// The current lite shader with Low's one bilinear lightmap fetch replaced by the full shader's
/// bicubic lookup (four fetches): what the filter alone would buy on this view.
fn bicubic_mobile_shader(current: &str) -> String {
    let patched = current.replace(
        "lightmap_atlas_sample(lightmap_atlas_uv(in.lightmap_uv, in.lightmap_scale_offset), lightmap_page(in.lightmap_scale_offset)).rgb",
        "lightmap_sample_bicubic(lightmap_atlas_uv(in.lightmap_uv, in.lightmap_scale_offset), lightmap_page(in.lightmap_scale_offset))",
    );
    assert_ne!(
        patched, current,
        "Low's lightmap fetch (update this control with the shader)"
    );
    patched
}

/// `frame` drawn with `shader` staged as `forward_mobile.wgsl`, the override dropped afterwards.
fn render_staged(
    device: &RenderDevice,
    map: &Arc<regia_asset::lightmap::Lightmap>,
    frame: &TierFrame,
    shader: &str,
) -> Vec<u8> {
    regia_render::shader_reload::stage_shader_edit("forward_mobile.wgsl", shader)
        .expect("stage the control shader");
    let pixels = render_frame(device, map, frame);
    regia_render::shader_reload::clear_shader_overrides();
    pixels
}

fn write_sized_png(path: &std::path::Path, pixels: &[u8], (width, height): (u32, u32)) {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut w| w.write_image_data(pixels))
            .expect("encode png");
    }
    regia_test_utils::write_atomic(path, &bytes).expect("write png");
}

/// |a − b| per channel times [`DIFF_GAIN`], opaque.
fn difference(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for (&x, &y) in pa[..3].iter().zip(&pb[..3]) {
            out.push(x.abs_diff(y).saturating_mul(DIFF_GAIN));
        }
        out.push(255);
    }
    out
}

/// `frames` side by side (each `size`).
fn strip(frames: &[&[u8]], (width, height): (u32, u32)) -> (Vec<u8>, (u32, u32)) {
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(frames.len() * row * height as usize);
    for y in 0..height as usize {
        for frame in frames {
            out.extend_from_slice(&frame[y * row..(y + 1) * row]);
        }
    }
    (out, (width * frames.len() as u32, height))
}

/// The lite shader before ADR-5945: no multi-scatter share in a receiver's baked reflection and
/// the occlusion map as it is on the baked diffuse. The control of
/// `the_lite_tiers_draw_the_full_shaders_baked_light_on_every_material`.
fn before_adr_5945_mobile_shader(current: &str) -> String {
    let single = current.replace(
        "if (lightmap_specular_on(in.lightmap_scale_offset) && material.response.y > 0.5) {",
        "if (false) {",
    );
    assert_ne!(
        single, current,
        "the receiver's multi-scatter share (update this control with the shader)"
    );
    let patched = single.replace("if (lm_ao < 1.0) {", "if (false) {");
    assert_ne!(
        patched, single,
        "the baked diffuse's multi-bounce occlusion (update this control with the shader)"
    );
    patched
}

/// The golden test's bake of the floor at `size` x `size` texels (36 there).
fn bake_sized(size: u32) -> regia_asset::lightmap::Lightmap {
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten the quad+box scene");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [size, size],
        padding: 2,
    };
    let settings = LightmapBakeSettings {
        samples: 64,
        bounces: 3,
        seed: 7,
        direct_sun: false,
        threads: 4,
        ..LightmapBakeSettings::default()
    };
    bake_mesh_lightmap(&scene, &request, &settings, |_, _| {})
        .expect("bake the floor")
        .0
}

/// One row of the table: `test` against `reference`.
fn row(table: &mut String, name: &str, test: &[u8], reference: &[u8]) -> TierAgreement {
    let agreement = tier_agreement(test, reference, W, H);
    let (max_channel, differing) = pixel_delta(test, reference, 0);
    let line = format!(
        "| {name} | {:.4} | {:+.3} | {:.3} | {}/{} | {max_channel} | {differing} |",
        agreement.ssim, agreement.mean_step, agreement.max_step, agreement.flat, agreement.windows
    );
    eprintln!("LITE_STUDY {line}");
    table.push_str(&line);
    table.push('\n');
    agreement
}

/// The table's header for rows of `what`.
fn header(table: &mut String, what: &str) {
    table.push_str(&format!(
        "\n| {what} | SSIM | mean flat step | max flat step | flat windows | max channel | pixels differing |\n|---|---|---|---|---|---|---|\n"
    ));
}

#[test]
#[ignore = "manual study: stages a process-wide shader override, run alone (ADR-5945)"]
fn lite_low_golden_study() {
    let device = device();
    let info = device.adapter.get_info();
    let map = Arc::new(bake().0);
    let current = include_str!("../../../regia_render/shaders/forward_mobile.wgsl");
    let low = Some(ForwardLite::Low);
    let golden_frame = TierFrame::golden;
    let plate_frame = |lite| TierFrame {
        size: PLATE,
        ..TierFrame::golden(lite)
    };

    // The two tiers as the golden test draws them.
    let full = render_frame(&device, &map, &golden_frame(None));
    let lite = render_frame(&device, &map, &golden_frame(low));
    // The full shader with one Mobile Low setting at a time, then with all of them: what the
    // profile gives up, apart from the shader.
    let full_bilinear = render_frame(
        &device,
        &map,
        &TierFrame {
            settings: LightmapSettings {
                bicubic: false,
                ..golden_frame(None).settings
            },
            ..golden_frame(None)
        },
    );
    let full_flat = render_frame(
        &device,
        &map,
        &TierFrame {
            quality: FrameQuality {
                lightmap_tier: LightmapTier::Bilinear,
                ..FrameQuality::default()
            },
            ..golden_frame(None)
        },
    );
    let full_tap = render_frame(
        &device,
        &map,
        &TierFrame {
            quality: FrameQuality {
                shadow_filter: ShadowFilter::Hardware2x2,
                ..FrameQuality::default()
            },
            ..golden_frame(None)
        },
    );
    let full_low = render_frame(
        &device,
        &map,
        &TierFrame {
            quality: FrameQuality {
                shadow_filter: ShadowFilter::Hardware2x2,
                lightmap_tier: LightmapTier::Bilinear,
                ..FrameQuality::default()
            },
            ..golden_frame(None)
        },
    );
    // The lite shader's controls.
    let legacy_shader = legacy_mobile_shader(current);
    let legacy = render_staged(&device, &map, &golden_frame(low), &legacy_shader);
    let lite_bicubic = render_staged(&device, &map, &golden_frame(low), &bicubic_mobile_shader(current));
    assert!(
        lite == render_frame(&device, &map, &golden_frame(low)),
        "the overrides are dropped: the lite frame is the first one again"
    );

    let golden = |name: &str| {
        read_png_rgba8(&golden_path(name))
            .filter(|(_, w, h)| (*w, *h) == (W, H))
            .map(|(rgba, _, _)| rgba)
    };
    let golden_lite = golden("lightmap_forward_quad_box_lite_low");
    let golden_full = golden("lightmap_forward_quad_box");

    let mut table = format!(
        "Adapter: {} ({:?}, {})\n",
        info.name, info.device_type, info.driver_info
    );
    header(&mut table, "golden view: test against reference");
    if let (Some(golden_lite), Some(golden_full)) = (&golden_lite, &golden_full) {
        row(
            &mut table,
            "lite now / blessed lite golden (the failing check)",
            &lite,
            golden_lite,
        );
        row(
            &mut table,
            "lite before ADR-4930 / blessed lite golden",
            &legacy,
            golden_lite,
        );
        row(&mut table, "full now / blessed full golden", &full, golden_full);
        row(
            &mut table,
            "blessed lite golden / blessed full golden",
            golden_lite,
            golden_full,
        );
    }
    let before = row(&mut table, "lite before ADR-4930 / full", &legacy, &full);
    let after = row(&mut table, "lite now / full", &lite, &full);
    row(
        &mut table,
        "lite now / full, bilinear lightmap",
        &lite,
        &full_bilinear,
    );
    row(
        &mut table,
        "lite now / full, bilinear without directions",
        &lite,
        &full_flat,
    );
    row(
        &mut table,
        "lite now / full, one hardware shadow tap",
        &lite,
        &full_tap,
    );
    row(
        &mut table,
        "lite now / full at Mobile Low's shadow and lightmap settings",
        &lite,
        &full_low,
    );
    row(
        &mut table,
        "lite now with a bicubic lightmap / full",
        &lite_bicubic,
        &full,
    );
    row(
        &mut table,
        "lite now with a bicubic lightmap / lite now",
        &lite_bicubic,
        &lite,
    );

    // Every receiver material: each lite tier against the full shader, today's shader and the one
    // before ADR-5945 (the control of the receiver-material test).
    let before_shader = before_adr_5945_mobile_shader(current);
    let tiers = [("Low", ForwardLite::Low), ("High", ForwardLite::High)];
    header(
        &mut table,
        "receiver material and lite tier, against the full shader",
    );
    for floor in floors() {
        let full = render_frame(&device, &map, &TierFrame::with_floor(None, &floor));
        for (tier, lite) in tiers {
            let frame = TierFrame::with_floor(Some(lite), &floor);
            let now = render_frame(&device, &map, &frame);
            let was = render_staged(&device, &map, &frame, &before_shader);
            let name = format!("{}, Mobile {tier}", floor.name);
            let before = row(&mut table, &format!("{name}, before ADR-5945"), &was, &full);
            let after = row(&mut table, &name, &now, &full);
            row(&mut table, &format!("{name}, after against before"), &now, &was);
            if floor.material.hard_surface.multi_scatter < 0.5 && !floor.is_occluded() {
                assert!(
                    now == was,
                    "{name}: a material without multi-scatter and occlusion draws the same frame"
                );
            }
            if floor.changed_by_adr_5945() && (lite == ForwardLite::High || floor.low_reads_the_full_field())
            {
                assert!(
                    before.max_step > TIER_MATERIAL_STEP && after.max_step <= TIER_MATERIAL_STEP,
                    "{name}: ADR-5945 is this receiver's missing light: {before:?} then {after:?}"
                );
            }
        }
    }

    // Not held by any test, measured for the report: what Low's one bilinear fetch costs as the map
    // gets coarser (a tier at half and at a quarter of the bake's density), against the full shader
    // on a map twice as fine as the golden's.
    header(
        &mut table,
        "floor map size and lookup, against the full shader on a 72 x 72 map",
    );
    let bicubic_shader = bicubic_mobile_shader(current);
    let reference = render_frame(&device, &Arc::new(bake_sized(72)), &golden_frame(None));
    let mut coarse_frames = Vec::new();
    for size in [36u32, 18, 9] {
        let coarse = Arc::new(bake_sized(size));
        let bilinear = render_frame(&device, &coarse, &golden_frame(low));
        let bicubic = render_staged(&device, &coarse, &golden_frame(low), &bicubic_shader);
        let full = render_frame(&device, &coarse, &golden_frame(None));
        let name = format!("{size} x {size} map");
        row(
            &mut table,
            &format!("{name}, Mobile Low (one bilinear fetch)"),
            &bilinear,
            &reference,
        );
        row(
            &mut table,
            &format!("{name}, Mobile Low with a bicubic lookup"),
            &bicubic,
            &reference,
        );
        row(&mut table, &format!("{name}, full shader"), &full, &reference);
        coarse_frames.push((size, coarse));
    }

    // The control of `the_lite_low_frame_agrees_with_the_full_frame`: the shader before the
    // correction fails both of its assertions, today's passes them.
    let bar = regia_test_utils::golden::adapter_ssim_threshold(&info, DEFAULT_SSIM_THRESHOLD);
    assert!(
        before.max_step > TIER_FLAT_STEP,
        "the lite shader before ADR-4930 drew its own brightness: {before:?}"
    );
    assert!(
        after.max_step <= TIER_FLAT_STEP && after.ssim >= bar,
        "today's lite shader agrees with the full shader: {after:?}"
    );
    if bar >= DEFAULT_SSIM_THRESHOLD {
        assert!(
            before.ssim < bar,
            "the lite shader before ADR-4930 misses the golden bar against the full shader: {before:?}"
        );
    }

    let Some(dir) = regia_core::brand::env_var_os(STUDY_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create the study directory");
    regia_test_utils::write_atomic(&dir.join("lite_low_study.md"), table.as_bytes())
        .expect("write the table");
    for (name, frame) in [
        ("golden_size_full", &full),
        ("golden_size_lite_before_adr4930", &legacy),
        ("golden_size_lite_now", &lite),
        ("golden_size_lite_bicubic", &lite_bicubic),
        ("golden_size_full_mobile_low_settings", &full_low),
    ] {
        write_sized_png(&dir.join(format!("{name}.png")), frame, (W, H));
    }
    let (pixels, size) = strip(
        &[
            &legacy,
            &lite,
            &full,
            &difference(&lite, &legacy),
            &difference(&lite, &full),
        ],
        (W, H),
    );
    write_sized_png(
        &dir.join("golden_size_before_after_full_diffs.png"),
        &pixels,
        size,
    );

    // The owner's plates: the same view and frame at six times the golden's size.
    let plate_full = render_frame(&device, &map, &plate_frame(None));
    let plate_after = render_frame(&device, &map, &plate_frame(low));
    let plate_before = render_staged(&device, &map, &plate_frame(low), &legacy_shader);
    let plate_bicubic = render_staged(&device, &map, &plate_frame(low), &bicubic_mobile_shader(current));
    for (name, frame) in [
        ("plate_mobile_low_before", &plate_before),
        ("plate_mobile_low_after", &plate_after),
        ("plate_full_shader", &plate_full),
        ("plate_mobile_low_bicubic", &plate_bicubic),
        (
            "plate_diff_after_minus_before_x32",
            &difference(&plate_after, &plate_before),
        ),
        (
            "plate_diff_after_minus_full_x32",
            &difference(&plate_after, &plate_full),
        ),
        (
            "plate_diff_bicubic_minus_after_x32",
            &difference(&plate_bicubic, &plate_after),
        ),
    ] {
        write_sized_png(&dir.join(format!("{name}.png")), frame, PLATE);
    }
    let (pixels, size) = strip(&[&plate_before, &plate_after, &plate_full], PLATE);
    write_sized_png(&dir.join("plate_before_after_full.png"), &pixels, size);

    // The coarse maps: Low's bilinear fetch, a bicubic lookup in its place, the full shader.
    for (size, coarse) in &coarse_frames {
        let bilinear = render_frame(&device, coarse, &plate_frame(low));
        let bicubic = render_staged(&device, coarse, &plate_frame(low), &bicubic_shader);
        let full = render_frame(&device, coarse, &plate_frame(None));
        let (pixels, strip_size) = strip(&[&bilinear, &bicubic, &full], PLATE);
        write_sized_png(
            &dir.join(format!("map_{size}_mobile_low_bilinear_bicubic_full.png")),
            &pixels,
            strip_size,
        );
    }

    // ADR-5945's plates: the receivers it changed before and after, beside the full shader's frame.
    for floor in floors().iter().filter(|floor| floor.changed_by_adr_5945()) {
        let slug: String = floor
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let sized = |lite| TierFrame {
            size: PLATE,
            ..TierFrame::with_floor(lite, floor)
        };
        let full = render_frame(&device, &map, &sized(None));
        for (tier, lite) in tiers {
            let after = render_frame(&device, &map, &sized(Some(lite)));
            let before = render_staged(&device, &map, &sized(Some(lite)), &before_shader);
            let (pixels, size) = strip(&[&before, &after, &full], PLATE);
            write_sized_png(
                &dir.join(format!("adr5945_{slug}_mobile_{tier}_before_after_full.png")),
                &pixels,
                size,
            );
        }
    }
}
