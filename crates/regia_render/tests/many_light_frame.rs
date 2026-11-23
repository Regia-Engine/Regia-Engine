// SPDX-License-Identifier: MPL-2.0
//! Stochastic many-light sampling in the production frame (ADR-2470 § 3, ADR-2652): with
//! `FrameQuality::many_light_sampling` the opaque forward pass shades one resampled point or spot
//! light per pixel from the frame's own light buffer and cluster lists, in place of the loop over
//! the list. Lavapipe is the reference; no golden image is involved.
//!
//! - one light: the sampled frame equals the loop's (a light counted twice would double it);
//! - 500 lights: the 16-frame average matches the brute-force loop's mean within the pre-registered
//!   bound (ADR-2652: 10 % in display units);
//! - steady sampled frames create no pipeline, bind group, buffer or texture;
//! - turning it off gives the loop frame back bit for bit;
//! - a blended surface keeps the loop (it is not the depth surface the reservoirs were built for);
//! - a camera cut and a changed light list drop the history.

use regia_core::glam::Mat4;
use regia_render::restir_di::scene::scatter_lights;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabLocalLight, LabMaterial, LabMesh, LabPointLight,
    LabScene, LabSpotLight, ManyLightSkip, PostSettings, RenderDevice, box_mesh, frame_path_counters,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

const W: u32 = 96;
const H: u32 = 54;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> SerialGpu {
    match serial_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// A floor, a back wall and two boxes, no sun and no ambient: only the local lights light it.
fn lab(local_lights: Vec<LabLocalLight>) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 6.0, -14.0],
        target: [0.0, 0.5, 8.0],
        fov_y: 1.0,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 4.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.3], [1.0; 3], 0.0),
        local_lights,
        materials: vec![LabMaterial {
            base_color: [0.7, 0.7, 0.7, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        }],
        meshes: vec![
            block([0.0, -0.25, 12.0], [30.0, 0.25, 30.0], 0),
            block([0.0, 5.0, 30.25], [30.0, 5.0, 0.25], 0),
            block([-3.0, 1.0, 8.0], [1.0, 1.0, 1.0], 0),
            block([4.0, 1.5, 12.0], [1.5, 1.5, 1.5], 0),
        ],
    }
}

/// The synthetic light field of the sampler's own tests, as frame lights.
fn scattered(count: u32) -> Vec<LabLocalLight> {
    scatter_lights(count, 11)
        .into_iter()
        .map(|l| {
            let [x, y, z, kind] = l.position_kind;
            let [r, g, b, candela] = l.color_intensity;
            let radius = l.direction_radius[3];
            if kind > 1.5 {
                LabLocalLight::Spot(LabSpotLight {
                    position: [x, y, z],
                    direction: [
                        l.direction_radius[0],
                        l.direction_radius[1],
                        l.direction_radius[2],
                    ],
                    color: [r, g, b],
                    intensity: candela,
                    range: radius,
                    inner_angle: 0.3,
                    outer_angle: 0.6,
                    cast_shadows: false,
                    source_radius: None,
                })
            } else {
                LabLocalLight::Point(LabPointLight {
                    position: [x, y, z],
                    color: [r, g, b],
                    // Lumens: the frame packs candela as lm / 4π.
                    intensity: candela * 4.0 * std::f32::consts::PI,
                    radius,
                    cast_shadows: false,
                    source_radius: None,
                })
            }
        })
        .collect()
}

/// The lab baseline with the sampler on or off, every light uploaded (the lab caps the frame at
/// 64) and Desktop Ultra's 256 slots per cluster.
fn quality(on: bool) -> FrameQuality {
    FrameQuality {
        many_light_sampling: on,
        max_visible_lights: 512,
        max_lights_per_cluster: 256,
        ..FrameQuality::LAB
    }
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, on: bool) -> Vec<u8> {
    renderer
        .render_offscreen(device, &lab.as_frame_scene_with_quality(quality(on)))
        .expect("offscreen frame")
        .pixels
}

/// Mean of the RGB channels over the image, 0..255.
fn mean(pixels: &[u8]) -> f64 {
    let (texels, _) = pixels.as_chunks::<4>();
    texels
        .iter()
        .map(|p| (f64::from(p[0]) + f64::from(p[1]) + f64::from(p[2])) / 3.0)
        .sum::<f64>()
        / texels.len().max(1) as f64
}

fn lit(pixels: &[u8]) -> usize {
    let (texels, _) = pixels.as_chunks::<4>();
    texels.iter().filter(|p| p[0].max(p[1]).max(p[2]) > 8).count()
}

/// Prints where two frames differ by more than 8 levels; with `REGIA_MANY_LIGHT_DUMP` set to a
/// directory, writes both as PPM there (investigation aid; no assertion).
fn report_diff(name: &str, a: &[u8], b: &[u8]) {
    let (ta, _) = a.as_chunks::<4>();
    let (tb, _) = b.as_chunks::<4>();
    let (mut n, mut x0, mut y0, mut x1, mut y1, mut brighter) = (0, W, H, 0, 0, 0);
    for (i, (p, q)) in ta.iter().zip(tb).enumerate() {
        let d = (0..3).map(|c| p[c].abs_diff(q[c])).max().unwrap_or(0);
        if d > 8 {
            let (x, y) = (i as u32 % W, i as u32 / W);
            n += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            if q[0] > p[0] {
                brighter += 1;
            }
        }
    }
    println!(
        "{name}: {n} pixels differ by more than 8 ({brighter} brighter sampled), box x {x0}..{x1} y {y0}..{y1}"
    );
    if let Ok(dir) = std::env::var("REGIA_MANY_LIGHT_DUMP") {
        for (suffix, img) in [("loop", a), ("sampled", b)] {
            let mut out = format!("P6 {W} {H} 255\n").into_bytes();
            for p in img.as_chunks::<4>().0 {
                out.extend_from_slice(&p[..3]);
            }
            let path = std::path::Path::new(&dir).join(format!("{name}-{suffix}.ppm").replace(' ', "_"));
            let _ = std::fs::write(path, out);
        }
    }
}

fn max_channel_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

#[test]
fn one_light_is_shaded_once_and_matches_the_loop() {
    let device = device();
    let lab = lab(vec![LabLocalLight::Point(LabPointLight {
        position: [0.0, 4.0, 10.0],
        color: [1.0, 0.95, 0.9],
        intensity: 60000.0,
        radius: 30.0,
        cast_shadows: false,
        source_radius: None,
    })]);
    let mut renderer = FrameRenderer::new();
    let looped = frame(&mut renderer, &device, &lab, false);
    assert!(
        lit(&looped) > (W * H / 4) as usize,
        "the light reaches the view: {}",
        lit(&looped)
    );
    assert_eq!(renderer.many_light_report().skipped, ManyLightSkip::Off);
    let mut sampled = Vec::new();
    for _ in 0..4 {
        sampled = frame(&mut renderer, &device, &lab, true);
    }
    let report = renderer.many_light_report();
    assert!(report.active, "{report:?}");
    assert_eq!(report.frames, 4);
    // With one light in every list the reservoir's weight is 1 up to float rounding: the frame is
    // the loop's. A light counted by both the reservoir and the loop doubles (far beyond this).
    report_diff("one light", &looped, &sampled);
    let diff = max_channel_diff(&looped, &sampled);
    println!(
        "one light: loop mean {:.3}, sampled mean {:.3}, max channel diff {diff}",
        mean(&looped),
        mean(&sampled)
    );
    assert!(diff <= 1, "max channel difference {diff}");
    assert!((mean(&looped) - mean(&sampled)).abs() < 0.05);
}

#[test]
fn five_hundred_lights_converge_to_the_loop_and_steady_frames_create_nothing() {
    let device = device();
    let lab = lab(scattered(500));
    let mut renderer = FrameRenderer::new();
    let looped = frame(&mut renderer, &device, &lab, false);
    let reference = mean(&looped);
    assert!(lit(&looped) > (W * H / 4) as usize, "lit {}", lit(&looped));

    const FRAMES: usize = 16;
    let mut sum = vec![0.0f64; looped.len()];
    let mut frame_means = Vec::new();
    for _ in 0..FRAMES {
        let px = frame(&mut renderer, &device, &lab, true);
        frame_means.push(mean(&px));
        for (s, v) in sum.iter_mut().zip(&px) {
            *s += f64::from(*v) / FRAMES as f64;
        }
    }
    let report = renderer.many_light_report();
    assert!(report.active, "{report:?}");
    assert_eq!(report.lights, 500);
    let average_px: Vec<u8> = sum.iter().map(|v| v.round().clamp(0.0, 255.0) as u8).collect();
    report_diff("500 lights", &looped, &average_px);
    let (texels, _) = sum.as_chunks::<4>();
    let (loop_texels, _) = looped.as_chunks::<4>();
    let average = texels.iter().map(|p| (p[0] + p[1] + p[2]) / 3.0).sum::<f64>() / texels.len() as f64;
    let relative = (average - reference).abs() / reference.max(1e-6);
    // Per-pixel error of the average, over pixels the loop lights.
    let (mut err, mut n) = (0.0, 0usize);
    for (a, l) in texels.iter().zip(loop_texels) {
        let l = (f64::from(l[0]) + f64::from(l[1]) + f64::from(l[2])) / 3.0;
        if l > 16.0 {
            err += ((a[0] + a[1] + a[2]) / 3.0 - l).abs() / l;
            n += 1;
        }
    }
    println!(
        "500 lights: loop mean {reference:.2}, {FRAMES}-frame average {average:.2} (relative {:.2} %), \
         per-pixel mean relative error {:.2} % over {n} pixels, frame means {frame_means:.1?}",
        100.0 * relative,
        100.0 * err / n.max(1) as f64
    );
    // ADR-2652's pre-registered bound for the frame mean after 16 frames, in display units (the
    // tonemap is concave, so a noisy estimate averages slightly low).
    assert!(relative < 0.10, "relative error of the mean {relative}");

    // Steady sampled frames create nothing (the view group alternates two cached keys).
    let before = frame_path_counters();
    for _ in 0..4 {
        frame(&mut renderer, &device, &lab, true);
    }
    let after = frame_path_counters().since(before);
    assert_eq!(
        (after.pipelines, after.bind_groups, after.buffers, after.textures),
        (0, 0, 0, 0),
        "{after:?}"
    );

    // Off again: the loop frame, bit for bit.
    let back = frame(&mut renderer, &device, &lab, false);
    assert_eq!(back, looped, "the profile off gives the loop frame back");
    assert!(!renderer.many_light_report().active);
}

#[test]
fn a_camera_cut_and_a_new_light_list_drop_the_history() {
    let device = device();
    let mut lab = lab(scattered(64));
    let mut renderer = FrameRenderer::new();
    for _ in 0..3 {
        frame(&mut renderer, &device, &lab, true);
    }
    let resets = renderer.many_light_report().resets;
    for _ in 0..2 {
        frame(&mut renderer, &device, &lab, true);
    }
    assert_eq!(
        renderer.many_light_report().resets,
        resets,
        "steady frames keep it"
    );
    // A cut: the camera jumps across the room.
    lab.eye = [12.0, 3.0, 26.0];
    lab.target = [0.0, 0.5, 4.0];
    frame(&mut renderer, &device, &lab, true);
    let after_cut = renderer.many_light_report().resets;
    assert!(
        after_cut > resets,
        "a camera cut resets ({resets} -> {after_cut})"
    );
    frame(&mut renderer, &device, &lab, true);
    assert_eq!(renderer.many_light_report().resets, after_cut);
    // A light leaves the list: the reservoirs' light indices no longer mean the same lights.
    lab.local_lights.pop();
    frame(&mut renderer, &device, &lab, true);
    assert!(
        renderer.many_light_report().resets > after_cut,
        "a new light list resets"
    );
}

#[test]
fn a_blended_surface_keeps_the_light_loop() {
    let device = device();
    // A light and a half-transparent pane in front of the empty sky: no opaque surface there,
    // so the reservoirs hold nothing. A pane that read them would lose its point light.
    let mut lab = lab(vec![LabLocalLight::Point(LabPointLight {
        position: [0.0, 6.0, -8.0],
        color: [1.0; 3],
        intensity: 9000.0,
        radius: 12.0,
        cast_shadows: false,
        source_radius: None,
    })]);
    lab.meshes.clear();
    lab.materials.push(LabMaterial {
        base_color: [0.9, 0.9, 0.9, 0.5],
        roughness: 0.6,
        alpha_mode: regia_render::ALPHA_BLEND,
        ..LabMaterial::default()
    });
    // The floor on one side keeps an opaque surface in view (and the frame sampling).
    lab.meshes.push(block([-6.0, -0.25, 12.0], [4.0, 0.25, 30.0], 0));
    lab.meshes.push(block([2.0, 6.0, -4.0], [2.0, 2.0, 0.05], 1));
    let mut renderer = FrameRenderer::new();
    let looped = frame(&mut renderer, &device, &lab, false);
    let mut sampled = Vec::new();
    for _ in 0..3 {
        sampled = frame(&mut renderer, &device, &lab, true);
    }
    assert!(renderer.many_light_report().active);
    let diff = max_channel_diff(&looped, &sampled);
    assert!(
        diff <= 1,
        "the pane and the one-light floor match the loop: {diff}"
    );
}

#[test]
fn the_restir_di_warm_key_builds_what_the_first_sampled_frame_needs() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    // The warm's frame: the profile asks but nothing samples yet (no local light in view).
    let dark = lab(Vec::new());
    frame(&mut renderer, &device, &dark, true);
    assert_eq!(renderer.many_light_report().skipped, ManyLightSkip::NoLights);
    let asked = renderer.warm_pipeline_key_for_tests(&device, "restir_di");
    assert!(
        asked >= 3,
        "the sampler, its G-buffer and the forward twin: {asked}"
    );
    // The first sampled frame compiles nothing.
    let lit = lab(scattered(64));
    let before = frame_path_counters();
    frame(&mut renderer, &device, &lit, true);
    let after = frame_path_counters().since(before);
    assert!(renderer.many_light_report().active);
    assert_eq!(after.pipelines, 0, "{after:?}");
    // A profile that does not ask warms nothing.
    let mut plain = FrameRenderer::new();
    frame(&mut plain, &device, &dark, false);
    assert_eq!(plain.warm_pipeline_key_for_tests(&device, "restir_di"), 0);
}
