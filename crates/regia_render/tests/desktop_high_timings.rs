// SPDX-License-Identifier: MPL-2.0
//! Desktop High frame cost on lavapipe (the 2233 regression baseline).
//!
//! Prints CPU phase times and GPU timestamp passes for warm frames. Frame 0 pays
//! pipeline compile and is not the steady cost. GPU timestamps need
//! `TIMESTAMP_QUERY` (lavapipe has it). A static camera must collapse SSAO and the
//! SSGI gather after the first frame, and the reused picture must still be the lit
//! frame. The held view keeps compositing the cached bounce onto each new jittered
//! frame until the temporal resolve has converged (motion-look lane, round 2: a held
//! frame used to freeze one jittered, aliased picture); from then on SSGI, SSR and
//! TAAU are not dispatched at all. Frame 41 moves the camera: it runs the half-res
//! SSAO and the SSGI gather again. Desktop High also skips full-resolution SSR.
//! Which passes ran is read from [`regia_render::ScreenPassReport`] (set where each
//! pass is encoded), not from single GPU stamps: on a loaded farm or a shared card a
//! held frame's composite stamp (1.13 ms) read above the moving gather (0.94 ms).
//! The stamps are still printed. Scene totals are not compared: lavapipe's cull and
//! forward stamps move by more than the skip. This file does not assert the RX 7900
//! 7 ms budget. The numbers are copied into `docs/evidence/terrain-runtime/`.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ScreenPassReport,
};

fn scene() -> LabScene {
    let vertices = [
        LabVertex::from_pos_normal_uv([-8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 1.6, 6.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.35, 0.42, 0.22, 1.0],
            roughness: 0.85,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: vertices.to_vec(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

#[test]
fn desktop_high_warm_frames_print_pass_times() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    if !device.capabilities.timestamp_query {
        // Metal lacks TIMESTAMP_QUERY_INSIDE_ENCODERS (ADR-275): the frame timer is off there.
        // Only the printed pass times need them; the asserts below run everywhere.
        eprintln!(
            "{} ({:?}) has no in-encoder timestamp queries: GPU pass times read 0",
            device.capabilities.adapter_name, device.capabilities.backend
        );
    }
    // The 0.67 performance budget a game can opt into; the Desktop High preset is native.
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.render_scale = 0.67;
    let quality = FrameQuality::from_profile(&profile);
    assert_eq!(
        quality.render_scale_permille, 670,
        "0.67 budget is TAAU at 670 permille"
    );
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let mut lab = scene();
    lab.post = Some(post.clone());
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    frame.ibl = true;
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    let mut held_prev: Option<Vec<u8>> = None;
    let mut reused: Option<Vec<u8>> = None;
    let mut reused_size = (0u32, 0u32);
    let mut held = ScreenPassReport::default();
    let mut converged = ScreenPassReport::default();
    let mut moving = ScreenPassReport::default();
    let mut moving_gpu = regia_render::GpuTimings::default();
    for i in 0..=MOVE_FRAME {
        // The last frame steps the camera so the static-screen cache misses. The
        // cost that matters is this moving frame, not the held skip.
        if i == MOVE_FRAME {
            frame.eye[2] += 0.8;
        }
        let image = renderer
            .render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("frame {i}: {err}"));
        let cpu = renderer.cpu_timings();
        let gpu = renderer.gpu_timings();
        let passes = renderer.screen_pass_report();
        let sky_i = ((4 * image.width + image.width / 2) * 4) as usize;
        let sky = [
            image.pixels[sky_i],
            image.pixels[sky_i + 1],
            image.pixels[sky_i + 2],
        ];
        eprintln!(
            "frame {i} scale {} sky {sky:?} cpu encode {:.2} submit {:.2} readback {:.2} total {:.2} gpu shadows {:.3} clusters {:.3} prepass {:.3} cull {:.3} ssao {:.3} forward {:.3} ssgi {:.3} ssr {:.3} taau {:.3} scene {:.3}",
            quality.render_scale(),
            cpu.encode_ms,
            cpu.submit_ms,
            cpu.readback_ms,
            cpu.total_ms,
            gpu.shadows_ms,
            gpu.clusters_ms,
            gpu.prepass_ms,
            gpu.cull_ms,
            gpu.ssao_ms,
            gpu.forward_ms,
            gpu.ssgi_ms,
            gpu.ssr_ms,
            gpu.taau_ms,
            gpu.scene_ms,
        );
        eprintln!("frame {i} passes {passes:?}");
        // The same stamps, named after the frame-graph nodes that record them (ADR-5386).
        let nodes: Vec<String> = gpu
            .node_passes()
            .iter()
            .map(|p| format!("{}[{}] {:.3}", p.node, p.pass, p.ms))
            .collect();
        eprintln!("frame {i} gpu nodes {}", nodes.join(", "));
        if i == HELD_FRAME {
            held_prev = Some(image.pixels.clone());
        }
        if i == REUSED_FRAME {
            reused = Some(image.pixels.clone());
            reused_size = (image.width, image.height);
            held = passes;
        }
        if i == CONVERGED_FRAME {
            converged = passes;
        }
        if i == MOVE_FRAME {
            moving = passes;
            moving_gpu = gpu;
        }
    }
    let held_prev = held_prev.expect("cache-filling frame");
    let reused = reused.expect("reused frame");
    let (width, height) = reused_size;
    let mae = mean_abs_rgb(&held_prev, &reused);
    let ground_i = (((height - 8) * width + width / 2) * 4) as usize;
    let ground = [reused[ground_i], reused[ground_i + 1], reused[ground_i + 2]];
    let ground_luma = (u32::from(ground[0]) + u32::from(ground[1]) + u32::from(ground[2])) / 3;
    // The sky is read from the reused frame, not from the moving one.
    let sky_i = ((4 * width + width / 2) * 4) as usize;
    let reused_sky = [reused[sky_i], reused[sky_i + 1], reused[sky_i + 2]];
    eprintln!("reused frame MAE {mae:.2} ground {ground:?} luma {ground_luma} sky {reused_sky:?}");
    assert!(
        mae < 12.0,
        "the static frame does not match the cached frame (MAE {mae:.2}). A wrong reused image is tens of levels off; black versus this lit frame is far above 12"
    );
    assert!(
        ground_luma > 25 && reused_sky[2] > reused_sky[0] + 15 && reused_sky[2] > 40,
        "the reused image is not the lit frame: sky {reused_sky:?} ground {ground:?}"
    );
    // The held view reuses last frame's AO; the moving frame runs the half-res pass.
    assert!(
        !held.ssao && moving.ssao,
        "SSAO dispatched on held frame {REUSED_FRAME}: {}, on the moving frame: {} (held must skip, moving must run)",
        held.ssao,
        moving.ssao
    );
    // The reused frame skips the gather and only composites the cached bounce onto the new
    // jittered frame. The moving frame gathers again (a skipped moving gather would
    // show the held bounce on a moved view).
    assert!(
        held.ssgi_composite && !held.ssgi_gather,
        "held frame {REUSED_FRAME} must composite the kept SSGI bounce without gathering: {held:?}"
    );
    assert!(
        moving.ssgi_gather && moving.ssgi_composite,
        "the moving frame did not run the half-res SSGI gather: {moving:?}"
    );
    // Past convergence the resolve shows its last output: no SSGI, SSR or AO at all.
    assert!(
        !converged.ssgi_gather
            && !converged.ssgi_composite
            && !converged.ssr
            && !converged.sssr
            && !converged.ssao,
        "converged held frame {CONVERGED_FRAME} still dispatched screen-space passes: {converged:?}"
    );
    // Full-resolution SSR was ~12 ms on this moving frame, above the running
    // half-resolution SSGI. Desktop High skips that pass; a restored full-res SSR
    // fails this.
    assert!(
        !moving.ssr && !held.ssr,
        "Desktop High at the 0.67 budget recorded full-resolution SSR (held {}, moving {})",
        held.ssr,
        moving.ssr
    );
    // TAAU is not dispatched once converged either, but its stamp range is not a
    // measure of that on lavapipe (draws are binned and rasterised at a later flush, so
    // the empty range reads 0.5–1.1 ms); `held_aa.rs` checks the frozen frame by its bytes.
    eprintln!(
        "moving half-res ssgi {:.3} ssao {:.3} ssr {:.3} taau {:.3} scene {:.3}",
        moving_gpu.ssgi_ms, moving_gpu.ssao_ms, moving_gpu.ssr_ms, moving_gpu.taau_ms, moving_gpu.scene_ms
    );
    // ADR-5386: with timestamps, the moving frame's pass times are named after graph nodes and
    // are not all zero. A time is a measurement, so only "some pass took time" is asserted.
    if device.capabilities.timestamp_query {
        let nodes = moving_gpu.node_passes();
        let builtins: Vec<&str> = regia_render::FRAME_STEPS
            .iter()
            .filter_map(|step| match step {
                regia_render::FrameStep::Builtin { name, .. } => Some(*name),
                regia_render::FrameStep::Slot(_) => None,
            })
            .collect();
        assert!(
            nodes.iter().all(|p| builtins.contains(&p.node)),
            "a GPU pass is not named after a graph node: {nodes:?}"
        );
        assert!(
            nodes.iter().any(|p| p.ms > 0.0),
            "the moving frame's GPU passes all read 0 ms: {nodes:?}"
        );
        for pass in &nodes {
            eprintln!("moving gpu node {} ({}) {:.3} ms", pass.node, pass.pass, pass.ms);
        }
    }
}

/// Frames the camera holds before it moves (the held view converges 32 frames after the
/// screen key holds, which the probes delay by 8).
const MOVE_FRAME: usize = 49;
/// The GPU irradiance probes (Desktop High's Depth look) change the indirect light while
/// they converge, so a held view reuses the screen only once they settle (8 frames,
/// `DDGI_REUSE_FRAMES`): frames 0..=8 settle them, 9 fills the static-screen cache, 10
/// reuses it.
const HELD_FRAME: usize = 9;
/// The first frame served from the static-screen cache.
const REUSED_FRAME: usize = 10;
/// A held frame past convergence.
const CONVERGED_FRAME: usize = 46;

fn mean_abs_rgb(a: &[u8], b: &[u8]) -> f32 {
    let n = (a.len().min(b.len()) / 4).max(1);
    let mut sum = 0u64;
    for i in 0..n {
        let o = i * 4;
        sum += (i32::from(a[o]) - i32::from(b[o])).unsigned_abs() as u64;
        sum += (i32::from(a[o + 1]) - i32::from(b[o + 1])).unsigned_abs() as u64;
        sum += (i32::from(a[o + 2]) - i32::from(b[o + 2])).unsigned_abs() as u64;
    }
    sum as f32 / (n as f32 * 3.0)
}
