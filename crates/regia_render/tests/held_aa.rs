// SPDX-License-Identifier: MPL-2.0
//! A held camera converges to an antialiased frame, then holds it bit for bit
//! (motion-look lane, round 2).
//!
//! Desktop High reuses the SSGI/SSR composite while the view is static (the static-screen
//! cache). With TAA on, the cache froze the composite of the first held frame — one
//! jittered sample per pixel — and the resolve converged onto it: held frames were stable
//! but their edges were as aliased as a frame without TAA. Held frames now keep rendering
//! with new jitters while the resolve accumulates a running mean, and the cache serves the
//! view (and the resolve shows its last output) only once that has converged.
//!
//! The scene is a bright slab with slanted edges over a dark ground. Edges are scored
//! against a 4×4 supersampled frame of the same view (TAA off, box-filtered down).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 160;
const H: u32 = 90;
const SS: u32 = 4;
// 48 frames to converge and freeze, plus the 8 the GPU irradiance probes (Desktop High's
// Depth look) take to settle before a held view may reuse the screen (`DDGI_REUSE_FRAMES`).
const FRAMES: usize = 56;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3], material: usize) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn scene(width: u32, height: u32) -> LabScene {
    // A slab standing 6 m in front of the eye, rotated so its edges are slanted on screen
    // (about 1 in 7 for the top and bottom, steep for the sides).
    let (s, c) = (0.14f32.sin(), 0.14f32.cos());
    let rot = |x: f32, y: f32| [x * c - y * s, 1.5 + x * s + y * c, -6.0];
    LabScene {
        width,
        height,
        eye: [0.0, 1.5, 0.0],
        target: [0.0, 1.5, -6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.5, -0.8], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.03, 0.03, 0.03, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            // Dark backdrop.
            quad(
                [
                    [-40.0, -20.0, -12.0],
                    [40.0, -20.0, -12.0],
                    [40.0, 25.0, -12.0],
                    [-40.0, 25.0, -12.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
            // The bright slab.
            quad(
                [rot(-2.2, -1.0), rot(2.2, -1.0), rot(2.2, 1.0), rot(-2.2, 1.0)],
                [0.0, 0.0, 1.0],
                1,
            ),
        ],
    }
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

fn quality(taa: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = taa;
    q
}

/// `HELD_AA_DUMP=<dir>` writes `name.ppm` (RGB of `pixels`, `w`×`h`).
fn dump(name: &str, pixels: &[u8], w: u32, h: u32) {
    let Some(dir) = regia_core::brand::env_var_os("HELD_AA_DUMP").map(std::path::PathBuf::from) else {
        return;
    };
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in pixels.as_chunks::<4>().0 {
        ppm.extend_from_slice(&p[..3]);
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{name}.ppm")), ppm);
}

fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

/// Coverage of `l` between the backdrop (`lo`) and the slab (`hi`) luma: 0 backdrop, 1 slab.
fn coverage(l: f32, lo: f32, hi: f32) -> f32 {
    (l - lo) / (hi - lo)
}

/// Of the pixels the supersampled reference shows partly covered by the slab (coverage
/// 0.1..0.9), the fraction `a` also shows partly covered (0.04..0.96). An aliased frame
/// snaps every one of them to backdrop or slab (0); an antialiased frame keeps the ramp.
///
/// Values are not compared directly: the reference averages tonemapped bytes, the
/// resolve averages linear HDR before the tonemap, so every edge pixel of a correct
/// resolve is brighter than the reference's.
fn edge_ramp_kept(a: &[f32], reference: &[f32], lo: f32, hi: f32) -> (f32, usize) {
    let mut partial = 0usize;
    let mut kept = 0usize;
    for (v, r) in a.iter().zip(reference) {
        let t = coverage(*r, lo, hi);
        if (0.1..=0.9).contains(&t) {
            partial += 1;
            let u = coverage(*v, lo, hi);
            if (0.04..=0.96).contains(&u) {
                kept += 1;
            }
        }
    }
    (kept as f32 / partial.max(1) as f32, partial)
}

#[test]
fn held_frames_converge_to_antialiased_and_freeze() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    assert!(
        quality(true).reuse_static_screen(),
        "this test covers the static-screen cache (SSGI + paged sun shadows)"
    );
    let post = post();

    // 4×4 supersampled reference, box-filtered to the output size.
    let big = scene(W * SS, H * SS);
    let mut frame = big.as_frame_scene_with_quality(quality(false));
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    let _ = renderer
        .render_offscreen(&device, &frame)
        .expect("reference warm-up");
    // A second frame so the reference's own temporal passes (shadow pages) have settled.
    let big_image = renderer.render_offscreen(&device, &frame).expect("reference");
    dump("reference_4x", &big_image.pixels, W * SS, H * SS);
    let big_l = luma(&big_image.pixels);
    let mut reference = vec![0.0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let mut sum = 0.0;
            for sy in 0..SS {
                for sx in 0..SS {
                    sum += big_l[((y * SS + sy) * W * SS + x * SS + sx) as usize];
                }
            }
            reference[(y * W + x) as usize] = sum / (SS * SS) as f32;
        }
    }

    // Native: one frame, no TAA (what a frozen single jittered frame looks like).
    let small = scene(W, H);
    let mut frame = small.as_frame_scene_with_quality(quality(false));
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    let _ = renderer
        .render_offscreen(&device, &frame)
        .expect("native warm-up");
    let native_image = renderer.render_offscreen(&device, &frame).expect("native");
    dump("native", &native_image.pixels, W, H);
    let native = luma(&native_image.pixels);

    // Held TAA.
    let mut frame = small.as_frame_scene_with_quality(quality(true));
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    let mut held = Vec::with_capacity(FRAMES);
    for _ in 0..FRAMES {
        let image = renderer.render_offscreen(&device, &frame).expect("held frame");
        dump(&format!("held_{:02}", held.len()), &image.pixels, W, H);
        held.push(image.pixels);
    }
    // Backdrop (top-left) and slab (centre) luma of the reference.
    let lo = reference[(5 * W + 5) as usize];
    let hi = reference[(40 * W + 85) as usize];
    let (native_kept, partial) = edge_ramp_kept(&native, &reference, lo, hi);
    let (early_kept, _) = edge_ramp_kept(&luma(&held[2]), &reference, lo, hi);
    let (held_kept, _) = edge_ramp_kept(&luma(&held[FRAMES - 1]), &reference, lo, hi);
    let changed: Vec<usize> = (FRAMES - 8..FRAMES)
        .map(|i| held[i].iter().zip(&held[i - 1]).filter(|(a, b)| a != b).count())
        .collect();
    eprintln!(
        "held AA: {partial} partly covered edge pixels in the 4×4 SS reference (backdrop {lo:.0}, slab \
         {hi:.0}); share kept partly covered: native {native_kept:.2}, held frame 2 {early_kept:.2}, held \
         frame {} {held_kept:.2}; changed bytes over the last 8 frames {changed:?}",
        FRAMES - 1
    );
    assert!(
        partial > 60,
        "the slab's edges hold {partial} partly covered pixels (> 60)"
    );
    assert!(hi - lo > 80.0, "slab/backdrop contrast {:.0} (> 80)", hi - lo);
    assert!(
        held_kept >= 0.75,
        "a converged held frame keeps {held_kept:.2} of the reference's antialiased edge pixels (≥ 0.75; \
         native {native_kept:.2}): held frames are not antialiased"
    );
    assert!(
        changed.iter().all(|&c| c == 0),
        "a converged held frame changes {changed:?} bytes per frame (bit-stable: 0)"
    );
}
