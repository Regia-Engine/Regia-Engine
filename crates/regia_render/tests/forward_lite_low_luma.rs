// SPDX-License-Identifier: MPL-2.0
//! Where the Mobile Low lite frame's luma gap came from (ADR-2635, diagnosing ADR-2403's finding: the
//! colour room's Mobile Low lite frame is about 10 % darker than the full shader's on the same
//! profile, Mobile High's matches).
//!
//! The lite frame is not the odd one out. Mobile Low's one 1024² cascade spans the full shader's whole
//! view (400 m here), too coarse for a 2.4 m room, so that frame's sun leaks past the walls: mean luma
//! 118.8 against 105.7 for the full shader on Mobile High. The Low tier reaches 40 m and reads 106.5.
//! Any sharper shadow map brings the full shader to the Low tier's level, and lavapipe measured
//! (full, Low): as is 118.8, 106.5; one 4096² cascade 106.3, 104.8; two 1024² cascades 106.1, 104.7;
//! four 2048² cascades 105.0, 104.7; the view's far plane at 40 m 105.2, 106.5.
//!
//! The rule below holds the Low tier to the full shader's frame given the same sun reach. The two
//! ignored tests print the whole table (`--ignored --nocapture`): measurements, not rules. With
//! `REGIA_LOW_LUMA_DIR` set, the shadow-map rows' frames are written there as binary PPM, to read by eye.

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, LabScene, RenderDevice, RenderProfile,
    RenderProfilePreset,
};
use regia_test_utils::gi_scenes::color_room;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// `forward_lite_cascades.rs`'s quality: `preset` on the CPU colour loop with the flat ambient.
fn quality(preset: RenderProfilePreset) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    q.gpu_driven = false;
    q.visibility_buffer = false;
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

fn render(device: &RenderDevice, scene: &LabScene, quality: FrameQuality, lite: ForwardLite) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(lite));
    let frame = scene.as_frame_scene_with_quality(quality);
    match renderer.render_offscreen(device, &frame) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("offscreen frame: {err:?}"),
    }
}

fn luma(pixels: &[u8]) -> f64 {
    let (rgba, _) = pixels.as_chunks::<4>();
    rgba.iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum::<f64>()
        / rgba.len().max(1) as f64
}

/// Writes `pixels` (RGBA8, `width` wide) as `dir/name.ppm` when a directory was given.
fn dump(dir: Option<&std::path::Path>, name: &str, pixels: &[u8], width: u32) {
    let Some(dir) = dir else { return };
    let (rgba, _) = pixels.as_chunks::<4>();
    let height = rgba.len() / width.max(1) as usize;
    let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
    for p in rgba {
        bytes.extend_from_slice(&p[..3]);
    }
    if let Err(err) = std::fs::write(dir.join(format!("{name}.ppm")), bytes) {
        eprintln!("{name}: {err}");
    }
}

/// The same room and light on Mobile Low with a sharper sun shadow map: whichever of the two frames
/// moves was reading a shadow map too coarse for the room.
/// How far the Low tier's mean luma may sit from the full shader's when both read a sun shadow map
/// of the same reach (ADR-2635: 1.3 % measured on lavapipe).
const SAME_REACH_LUMA_SHARE: f64 = 0.03;

/// The Low tier as it ships (40 m sun reach, on a 400 m view) against the full shader whose whole-view
/// cascade spans the same 40 m: the sun's shadows and every other term agree.
#[test]
fn the_low_tier_matches_the_full_shader_at_the_same_sun_reach() {
    let device = device();
    let room = color_room().lab;
    let q = quality(RenderProfilePreset::MobileLow);
    let same_reach = LabScene {
        far: regia_render::forward_lite::LOW_SUN_RANGE_M,
        ..room.clone()
    };
    let full = luma(&render(&device, &same_reach, q, ForwardLite::Off));
    let low = luma(&render(&device, &room, q, ForwardLite::Low));
    assert!(
        (low / full.max(1e-9) - 1.0).abs() <= SAME_REACH_LUMA_SHARE,
        "Low {low:.1} against the full shader's {full:.1} at the same sun reach"
    );
}

#[test]
#[ignore = "hand-run diagnosis (ADR-2635)"]
fn mobile_low_lite_luma_by_shadow_map() {
    let device = device();
    let room = color_room().lab;
    let dir = std::env::var_os("REGIA_LOW_LUMA_DIR").map(std::path::PathBuf::from);
    // The last row gives the full shader's whole-view cascade the Low tier's 40 m reach.
    let rows: [(&str, u32, u32, f32); 5] = [
        ("as is", 1, 1024, room.far),
        ("1 cascade 4096", 1, 4096, room.far),
        ("2 cascades 1024", 2, 1024, room.far),
        ("4 cascades 2048", 4, 2048, room.far),
        ("far 40 m", 1, 1024, regia_render::forward_lite::LOW_SUN_RANGE_M),
    ];
    for (name, count, resolution, far) in rows {
        let scene = LabScene { far, ..room.clone() };
        let mut q = quality(RenderProfilePreset::MobileLow);
        q.csm_cascade_count = count;
        q.csm_resolution = resolution;
        let full = render(&device, &scene, q, ForwardLite::Off);
        let lite = render(&device, &scene, q, ForwardLite::Low);
        let slug = name.replace(' ', "_");
        dump(dir.as_deref(), &format!("full_{slug}"), &full, scene.width);
        dump(dir.as_deref(), &format!("low_{slug}"), &lite, scene.width);
        let (full, lite) = (luma(&full), luma(&lite));
        eprintln!(
            "MobileLow {name:>15}: full {full:6.1}, Low {lite:6.1} ({:+.1} %)",
            (lite / full.max(1e-9) - 1.0) * 100.0
        );
    }
}

#[test]
#[ignore = "hand-run diagnosis (ADR-2635)"]
fn mobile_low_lite_luma_by_light_term() {
    let device = device();
    let base = color_room().lab;
    let sun = base.directional;
    let rows: Vec<(&str, LabScene)> = vec![
        ("as is", base.clone()),
        (
            "no sun shadows",
            LabScene {
                directional_shadows: false,
                ..base.clone()
            },
        ),
        (
            "no sun",
            LabScene {
                directional: DirectionalLightUniform::new([-0.35, -0.55, -0.76], [1.0, 0.97, 0.92], 0.0),
                ..base.clone()
            },
        ),
        (
            "no ambient",
            LabScene {
                ambient_lux: 0.0,
                directional: sun,
                ..base.clone()
            },
        ),
    ];
    for preset in [RenderProfilePreset::MobileLow, RenderProfilePreset::MobileHigh] {
        let tier = if preset == RenderProfilePreset::MobileLow {
            ForwardLite::Low
        } else {
            ForwardLite::High
        };
        let q = quality(preset);
        for (name, scene) in &rows {
            let full = luma(&render(&device, scene, q, ForwardLite::Off));
            let lite = luma(&render(&device, scene, q, tier));
            eprintln!(
                "{preset:?} {name:>15}: full {full:6.1}, {tier:?} {lite:6.1} ({:+.1} %)",
                (lite / full.max(1e-9) - 1.0) * 100.0
            );
        }
    }
}
