// SPDX-License-Identifier: MIT-0
//! The default-look shots' sun shadows against the path tracer (lane csm-far).
//!
//! Each shot renders with the sun as the only light (no IBL, no ambient, no GI), once with
//! sun shadows and once without; their ratio is the raster's sun visibility. The tracer
//! traces the same frame (direct light, the raster's 1° sun disc) and reports its own. A
//! pixel disagrees when one says lit (> 0.5) and the other shadowed: `leak` is raster lit
//! where the tracer is shadowed, `excess` the opposite. Pixels the raster's PCSS penumbra can
//! reach (within 3 px of a tracer shadow edge) and pixels the sun barely lights are skipped.
//!
//! `CSM_FAR_OUT=<dir>` writes per-shot masks (white agree-shadow, red excess, cyan leak).

use lookdev::default_look::{LookShot, build_shot};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{FrameRenderer, PostSettings, RenderDevice, RenderSettings};

const W: u32 = 640;
const H: u32 = 360;
const EDGE_PX: usize = 3;

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

#[test]
fn default_look_sun_shadows_match_the_tracer() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile the path tracer");
    let settings = RenderSettings::default();
    let mut quality = settings.quality();
    quality.screen_space_gi = false;
    // No GI probes either (the module doc: the sun is the only light). Since gi-q-1's probe
    // specular (ADR-1103) the probes' light in the specular lobe lifted sun-shadowed faces of the
    // floating vehicle above the judging floor in both raster passes, where the shadow toggle
    // cannot dim it, and they counted as raster-lit against the direct-only tracer (ADR-1410):
    // vehicle_in_air 0.43 % at v0.1.28, 1.42 % from the gi-bake merge, 0.41 % with the probes
    // off (the build server lavapipe). The cockpit, lit mostly by its probes, then judges about half as
    // many pixels (28530 of 53731).
    quality.gi_probes = false;
    quality.gi = regia_render::GiMode::Ambient;
    quality.ssao = regia_render::SsaoMode::Off;
    quality.ssr = false;
    quality.taau = false;
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let mut report = Vec::new();
    for shot in LookShot::ALL {
        let scene = build_shot(shot, W, H).expect("build shot");
        let mut lab = scene.lab;
        lab.ibl = false;
        lab.ambient_lux = 0.0;
        lab.post = Some(post.clone());
        lab.exposure_ev = 14.5;
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.fog = regia_render::FogSettings::default();
        let raster = |shadows: bool| {
            let mut f = frame.clone();
            f.directional_shadows = shadows;
            let mut renderer = FrameRenderer::new();
            renderer.set_gi_field(None);
            renderer.request_hdr_capture();
            renderer.render_offscreen(&device, &f).expect("frame");
            renderer
                .take_hdr_capture(&device)
                .expect("hdr readback")
                .expect("post ran")
                .rgb
        };
        let lit = raster(false);
        let shadowed = raster(true);
        let pt_scene = from_frame(
            &frame,
            &ExtractOptions {
                sky: Some(PtSky::None),
                sun_angular_radius: 0.0175,
                ..ExtractOptions::default()
            },
        );
        let pt = tracer
            .render(
                &pt_scene,
                &PtSettings {
                    spp: 4,
                    max_bounces: 1,
                    jitter: false,
                    ..PtSettings::default()
                },
            )
            .expect("trace");
        let (w, h) = (W as usize, H as usize);
        let peak = lit.iter().map(|c| luma(*c)).fold(0.0f32, f32::max);
        let pt_shadow: Vec<bool> = pt.shadow.iter().map(|&v| v < 0.5).collect();
        let (mut judged, mut leak, mut excess) = (0usize, 0usize, 0usize);
        let mut img = vec![0u8; w * h * 3];
        for y in EDGE_PX..h - EDGE_PX {
            for x in EDGE_PX..w - EDGE_PX {
                let i = y * w + x;
                let l = luma(lit[i]);
                if pt.depth[i] <= 0.0 || l < 0.05 * peak {
                    continue;
                }
                let near_edge = (y - EDGE_PX..=y + EDGE_PX)
                    .any(|yy| (x - EDGE_PX..=x + EDGE_PX).any(|xx| pt_shadow[yy * w + xx] != pt_shadow[i]));
                if near_edge {
                    continue;
                }
                judged += 1;
                let r_shadow = luma(shadowed[i]) / l < 0.5;
                let px = match (r_shadow, pt_shadow[i]) {
                    (true, false) => {
                        excess += 1;
                        [255, 40, 40]
                    }
                    (false, true) => {
                        leak += 1;
                        [40, 220, 255]
                    }
                    (true, true) => [255, 255, 255],
                    (false, false) => [90, 90, 90],
                };
                img[i * 3..i * 3 + 3].copy_from_slice(&px);
            }
        }
        let line = format!(
            "{}: judged {judged}, leak {leak} ({:.2} %), excess {excess} ({:.2} %)",
            shot.name(),
            100.0 * leak as f32 / judged.max(1) as f32,
            100.0 * excess as f32 / judged.max(1) as f32
        );
        eprintln!("{line}");
        report.push((line, leak, excess, judged));
        if let Some(dir) = std::env::var_os("CSM_FAR_OUT") {
            let dir = std::path::Path::new(&dir);
            let _ = std::fs::create_dir_all(dir);
            if let Ok(file) = std::fs::File::create(dir.join(format!("sun_vs_pt_{}.png", shot.name()))) {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
                enc.set_color(png::ColorType::Rgb);
                enc.set_depth(png::BitDepth::Eight);
                if let Ok(mut writer) = enc.write_header() {
                    let _ = writer.write_image_data(&img);
                }
            }
        }
    }
    // Ceilings (percent of the judged pixels). Measured on lavapipe at csm-far's final
    // shader, main before it in brackets: props 0.34 [0.32], vehicle 0.44 [0.49], cockpit
    // 4.54 [12.94], vista 0.48 [0.29]. The cockpit's rest is the far edge of the canopy
    // rail's grazing shadow on the tub wall; the vista's rise is 300–960 m moving into the
    // last cascade (16 / 69 / 300 m ladder): its 4 m texels lose the thin far ends of 16°
    // hill shadows.
    let ceilings = [
        ("props_on_snow", 0.6),
        ("vehicle_in_air", 0.7),
        ("cockpit", 6.0),
        ("wide_vista", 0.8),
    ];
    for (line, leak, excess, judged) in &report {
        let pct = 100.0 * (leak + excess) as f32 / (*judged).max(1) as f32;
        let ceiling = ceilings
            .iter()
            .find(|(name, _)| line.starts_with(name))
            .map_or(1.0, |(_, c)| *c);
        assert!(
            pct <= ceiling,
            "{line}: {pct:.2} % disagree with the tracer (ceiling {ceiling} %)"
        );
    }
}
