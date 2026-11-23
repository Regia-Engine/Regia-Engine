// SPDX-License-Identifier: MPL-2.0
//! GPU checks of the look renderer on the WebGL2 limits (lavapipe on CI hosts). No goldens: each
//! test compares the renderer with itself. Without an adapter the tests print why and pass, so the
//! lane report must show they ran (`REGIA_LOOKS_REQUIRE_GPU=1` turns the skip into a failure).
#![cfg(feature = "gpu")]

use glam::{Mat4, Vec3};
use regia_looks::gpu::{LookCamera, LookItem, LookRenderer, LookScene, headless_device};
use regia_looks::{CutType, GemCut, Library, LookMesh, StoneSize, Studio};

fn renderer() -> Option<LookRenderer> {
    match headless_device() {
        Ok((device, queue, info)) => {
            eprintln!("adapter: {} ({:?})", info.name, info.backend);
            Some(LookRenderer::new(
                device,
                queue,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                4,
                Studio::default(),
            ))
        }
        Err(e) => {
            assert!(
                std::env::var_os("REGIA_LOOKS_REQUIRE_GPU").is_none(),
                "no GPU: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn gem_scene(tilt_deg: f32, stone: &str) -> LookScene {
    let lib = Library::builtin().expect("library");
    let cut = GemCut::new(CutType::RoundBrilliant, [6.0, 6.0]);
    LookScene {
        camera: LookCamera {
            eye: Vec3::new(0.0, 0.0, 40.0),
            target: Vec3::ZERO,
            fov_y_deg: 11.0,
        },
        items: vec![LookItem {
            mesh: cut.mesh(),
            material: lib.look(stone, None).expect("stone"),
            model: Mat4::from_rotation_x(-tilt_deg.to_radians()),
            stone: cut.stone_size(),
        }],
        exposure: 1.0,
    }
}

fn luma(rgba: &[u8]) -> Vec<f32> {
    let (px, _) = rgba.as_chunks::<4>();
    px.iter()
        .map(|p| (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0)
        .collect()
}

#[test]
fn a_still_view_is_bit_stable() {
    let Some(mut r) = renderer() else { return };
    let scene = gem_scene(15.0, "diamond-white");
    let a = r.render_still(&scene, 128, 128).expect("render a");
    let b = r.render_still(&scene, 128, 128).expect("render b");
    assert_eq!(
        a, b,
        "the same view renders the same bytes (no noise, no history)"
    );
    // Something was drawn: the stone is brighter than the background in places.
    let l = luma(&a);
    assert!(l.iter().any(|v| *v > 0.8), "the stone has bright facets");
}

#[test]
fn material_swaps_create_no_pipeline() {
    let Some(mut r) = renderer() else { return };
    let created = r.pipelines_created();
    let lib = Library::builtin().expect("library");
    let ball = LookMesh::ball(4.0, 32, 16);
    for id in [
        "gold-yellow-18k",
        "titanium-anodized-blue",
        "niobium-anodized-green",
        "platinum",
        "titanium-anodized-gold",
    ] {
        let scene = LookScene {
            camera: LookCamera {
                eye: Vec3::new(0.0, 0.0, 30.0),
                target: Vec3::ZERO,
                fov_y_deg: 12.0,
            },
            items: vec![LookItem {
                mesh: ball.clone(),
                material: lib.look(id, Some("finish-polish")).expect("metal"),
                model: Mat4::IDENTITY,
                stone: StoneSize::default(),
            }],
            exposure: 1.0,
        };
        r.render_still(&scene, 64, 64).expect("render");
        assert_eq!(r.draws_last_frame(), 2, "background + one ball");
    }
    for id in ["diamond-white", "sapphire-blue", "opal-white", "cz-frosted"] {
        r.render_still(&gem_scene(10.0, id), 64, 64).expect("render");
    }
    assert_eq!(r.pipelines_created(), created, "swaps change uniforms only");
}

#[test]
fn anodize_colors_differ_on_the_same_ball() {
    let Some(mut r) = renderer() else { return };
    let lib = Library::builtin().expect("library");
    let mut means = Vec::new();
    for id in [
        "titanium-anodized-blue",
        "titanium-anodized-gold",
        "titanium-anodized-teal",
    ] {
        let scene = LookScene {
            camera: LookCamera {
                eye: Vec3::new(0.0, 0.0, 30.0),
                target: Vec3::ZERO,
                fov_y_deg: 9.0,
            },
            items: vec![LookItem {
                mesh: LookMesh::ball(4.0, 48, 24),
                material: lib.look(id, Some("finish-polish")).expect("anodized"),
                model: Mat4::IDENTITY,
                stone: StoneSize::default(),
            }],
            exposure: 1.0,
        };
        let img = r.render_still(&scene, 64, 64).expect("render");
        let center: Vec<&[u8; 4]> = img
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let (x, y) = (i % 64, i / 64);
                (x as i32 - 32).pow(2) + (y as i32 - 32).pow(2) < 100
            })
            .map(|(_, p)| p)
            .collect();
        let mean = |c: usize| center.iter().map(|p| f32::from(p[c])).sum::<f32>() / center.len() as f32;
        means.push([mean(0), mean(1), mean(2)]);
    }
    // Blue reads blue, gold reads warm, teal has more green than red.
    assert!(means[0][2] > means[0][0], "blue {:?}", means[0]);
    assert!(means[1][0] > means[1][2], "gold {:?}", means[1]);
    assert!(means[2][1] > means[2][0], "teal {:?}", means[2]);
}

/// A slow drag (0.25 degrees a frame, about 15 degrees a second at 60 fps): sparkles move with
/// the view instead of popping. A "single-frame sparkle" is a bright pixel that is dark in the
/// frame before and the frame after; ADR-2514's bar is at most 10 % of all sparkle pixels.
#[test]
fn sparkle_is_stable_during_a_slow_drag() {
    let Some(mut r) = renderer() else { return };
    let (maxima, single) = drag_sparkle(&mut r, 0.25);
    let ratio = single as f64 / maxima.max(1) as f64;
    eprintln!("0.25 deg/frame: sparkle pixels {maxima}, single-frame {single}, ratio {ratio:.4}");
    assert!(maxima > 100, "the stone sparkles at all: {maxima}");
    assert!(ratio <= 0.10, "single-frame sparkle ratio {ratio:.4} > 0.10");
    // A brisk drag (1 degree a frame, 60 degrees a second at 60 fps) is reported, not gated: the
    // owner judges it on a device.
    let (m, s1) = drag_sparkle(&mut r, 1.0);
    eprintln!(
        "1.00 deg/frame: sparkle pixels {m}, single-frame {s1}, ratio {:.4}",
        s1 as f64 / m.max(1) as f64
    );
}

/// Bright pixels over 12 frames of a tilt drag at `step_deg` a frame, and how many of them are
/// single-frame pops (dark in the frames before and after).
fn drag_sparkle(r: &mut LookRenderer, step_deg: f32) -> (usize, usize) {
    let size = 160u32;
    let frames: Vec<Vec<f32>> = (0..12)
        .map(|k| {
            luma(
                &r.render_still(&gem_scene(8.0 + k as f32 * step_deg, "diamond-white"), size, size)
                    .expect("frame"),
            )
        })
        .collect();
    let (bright, dark) = (0.85, 0.55);
    let mut maxima = 0usize;
    let mut single = 0usize;
    for k in 1..frames.len() - 1 {
        for (p, &v) in frames[k].iter().enumerate() {
            if v > bright {
                maxima += 1;
                if frames[k - 1][p] < dark && frames[k + 1][p] < dark {
                    single += 1;
                }
            }
        }
    }
    (maxima, single)
}
