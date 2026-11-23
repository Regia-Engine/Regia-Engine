// SPDX-License-Identifier: MPL-2.0
//! The CPU reference against brute force on the synthetic 1,000-light scene (ADR-2470). The
//! measured numbers are printed (`--nocapture`) and recorded in the lane report.

use super::*;
use crate::restir_di::scene::SyntheticScene;

const W: u32 = 48;
const H: u32 = 27;
const LIGHTS: u32 = 1000;

fn scene() -> (SyntheticScene, ClusterLists) {
    let scene = SyntheticScene::new(W, H, LIGHTS, 7);
    let clusters = ClusterLists::build(&scene.camera, W as f32 / H as f32, &scene.lights, 256);
    (scene, clusters)
}

#[test]
fn the_cluster_lists_hold_every_light_that_reaches_a_pixel() {
    let (scene, clusters) = scene();
    assert_eq!(clusters.overflowed, 0, "no list may drop a light at 256 slots");
    let g = &scene.gbuffer;
    let surfaces = g.surfaces(&scene.camera);
    let mut checked = 0;
    for (i, s) in surfaces.iter().enumerate() {
        let Some(s) = s else { continue };
        let (x, y) = (i as u32 % W, i as u32 / W);
        let uv = [(x as f32 + 0.5) / W as f32, (y as f32 + 0.5) / H as f32];
        let cid = cluster_of(uv, s.depth, scene.camera.near, scene.camera.far) as usize;
        let list = &clusters.indices[cid * 256..cid * 256 + clusters.counts[cid] as usize];
        for (li, light) in scene.lights.iter().enumerate() {
            if crate::restir_di::supports(light, s) {
                assert!(
                    list.contains(&(li as u32)),
                    "pixel {i}: light {li} reaches it but is not listed"
                );
                checked += 1;
            }
        }
    }
    let (mean, max) = clusters.occupancy();
    println!("cluster lists: mean {mean:.1}, max {max}, {checked} reaching pairs checked");
    assert!(checked > 1000, "the scene must light its pixels ({checked})");
    assert!(max < 256 && mean > 1.0);
}

#[test]
fn resampling_without_reuse_is_unbiased() -> Result<(), RestirDiError> {
    let (scene, clusters) = scene();
    let settings = RestirDiSettings::candidates_only(8);
    let r = evaluate(
        &scene.gbuffer,
        &scene.camera,
        &scene.lights,
        Candidates::Clusters(&clusters),
        &settings,
        1,
        256,
    )?;
    println!("candidates only: {r:?}");
    assert!(r.bias_sigma < 4.0, "bias {} sigma", r.bias_sigma);
    assert!(r.relative_bias.abs() < 0.03, "relative bias {}", r.relative_bias);
    Ok(())
}

#[test]
fn spatiotemporal_reuse_is_unbiased_and_removes_variance() -> Result<(), RestirDiError> {
    let (scene, clusters) = scene();
    let settings = RestirDiSettings::default();
    let r = evaluate(
        &scene.gbuffer,
        &scene.camera,
        &scene.lights,
        Candidates::Clusters(&clusters),
        &settings,
        6,
        48,
    )?;
    println!("restir: {r:?}");
    assert_eq!(r.lights, LIGHTS);
    assert!(r.bias_sigma < 4.0, "bias {} sigma", r.bias_sigma);
    assert!(r.relative_bias.abs() < 0.03, "relative bias {}", r.relative_bias);
    assert!(
        r.rmse_restir < r.rmse_candidates && r.rmse_candidates < r.rmse_uniform,
        "rmse uniform {} > candidates {} > restir {}",
        r.rmse_uniform,
        r.rmse_candidates,
        r.rmse_restir
    );
    assert!(
        r.variance_reduction > 4.0,
        "variance reduction {}",
        r.variance_reduction
    );
    let first = r.convergence.first().copied().unwrap_or(0.0);
    let last = r.convergence.last().copied().unwrap_or(f64::MAX);
    assert!(
        last < 0.8 * first,
        "error must fall as the history builds: {:?}",
        r.convergence
    );
    Ok(())
}

#[test]
fn uniform_over_every_light_is_unbiased_but_noisier_than_the_lists() -> Result<(), RestirDiError> {
    let (scene, clusters) = scene();
    let settings = RestirDiSettings::candidates_only(8);
    let all = evaluate(
        &scene.gbuffer,
        &scene.camera,
        &scene.lights,
        Candidates::All,
        &settings,
        1,
        48,
    )?;
    let listed = evaluate(
        &scene.gbuffer,
        &scene.camera,
        &scene.lights,
        Candidates::Clusters(&clusters),
        &settings,
        1,
        48,
    )?;
    println!("all lights: {all:?}");
    assert!(all.bias_sigma < 4.0, "bias {} sigma", all.bias_sigma);
    assert!(
        listed.rmse_candidates < all.rmse_candidates,
        "cluster candidates {} must beat uniform over all lights {}",
        listed.rmse_candidates,
        all.rmse_candidates
    );
    Ok(())
}

#[test]
fn a_camera_step_reprojects_most_history_and_rejects_the_revealed_edge() -> Result<(), RestirDiError> {
    let a = SyntheticScene::new(W, H, LIGHTS, 7);
    let cam_b = SyntheticScene::camera_at(W, H, 3.0);
    let b = SyntheticScene::with_camera(W, H, LIGHTS, 7, cam_b, Some(&a.camera));
    let clusters_a = ClusterLists::build(&a.camera, W as f32 / H as f32, &a.lights, 256);
    let clusters_b = ClusterLists::build(&b.camera, W as f32 / H as f32, &b.lights, 256);
    let mut sampler = ReferenceSampler::new(RestirDiSettings::default(), 3)?;
    let first = sampler.frame(
        &a.gbuffer,
        &a.camera,
        &a.lights,
        Candidates::Clusters(&clusters_a),
    )?;
    assert_eq!(first.temporal_accepted, 0, "no history on the first frame");
    let second = sampler.frame(
        &b.gbuffer,
        &b.camera,
        &b.lights,
        Candidates::Clusters(&clusters_b),
    )?;
    let surface = b.gbuffer.depth.iter().filter(|d| **d > 0.0).count() as u32;
    println!(
        "camera step: {} of {surface} pixels kept history",
        second.temporal_accepted
    );
    assert!(
        second.temporal_accepted > surface / 2,
        "most surfaces stay in view"
    );
    assert!(
        second.temporal_accepted < surface,
        "the revealed edge must start over"
    );
    // The same step with the motion vectors zeroed reads the wrong history: fewer pixels pass the
    // world-position test.
    let mut stale = b.gbuffer.clone();
    stale.motion.iter_mut().for_each(|m| *m = [0.0; 2]);
    let mut sampler = ReferenceSampler::new(RestirDiSettings::default(), 3)?;
    sampler.frame(
        &a.gbuffer,
        &a.camera,
        &a.lights,
        Candidates::Clusters(&clusters_a),
    )?;
    let wrong = sampler.frame(&stale, &b.camera, &b.lights, Candidates::Clusters(&clusters_b))?;
    assert!(
        wrong.temporal_accepted < second.temporal_accepted,
        "{} vs {}",
        wrong.temporal_accepted,
        second.temporal_accepted
    );
    // After a reset the history is gone.
    sampler.reset();
    let cut = sampler.frame(
        &b.gbuffer,
        &b.camera,
        &b.lights,
        Candidates::Clusters(&clusters_b),
    )?;
    assert_eq!(cut.temporal_accepted, 0);
    Ok(())
}

#[test]
fn a_moving_camera_stays_unbiased() -> Result<(), RestirDiError> {
    // Four frames stepping right; each frame's estimate is scored against its own brute force.
    let steps = [0.0f32, 0.3, 0.6, 0.9];
    let frames: Vec<SyntheticScene> = steps
        .iter()
        .enumerate()
        .map(|(k, dx)| {
            let cam = SyntheticScene::camera_at(W, H, *dx);
            let prev = (k > 0).then(|| SyntheticScene::camera_at(W, H, steps[k - 1]));
            SyntheticScene::with_camera(W, H, LIGHTS, 7, cam, prev.as_ref())
        })
        .collect();
    let lists: Vec<ClusterLists> = frames
        .iter()
        .map(|f| ClusterLists::build(&f.camera, W as f32 / H as f32, &f.lights, 256))
        .collect();
    let last = frames.len() - 1;
    let truth = mean_luminance(&brute_force(
        &frames[last].gbuffer,
        &frames[last].camera,
        &frames[last].lights,
    ));
    let mut means = Vec::new();
    for seed in 0..40 {
        let mut sampler = ReferenceSampler::new(RestirDiSettings::default(), seed)?;
        let mut out = Vec::new();
        for (f, l) in frames.iter().zip(&lists) {
            out = sampler
                .frame(&f.gbuffer, &f.camera, &f.lights, Candidates::Clusters(l))?
                .radiance;
        }
        means.push(mean_luminance(&out));
    }
    let n = means.len() as f64;
    let mean = means.iter().sum::<f64>() / n;
    let se = (means.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / (n - 1.0) / n).sqrt();
    println!("moving camera: truth {truth}, estimate {mean} ± {se}");
    assert!(
        (mean - truth).abs() < 4.0 * se.max(1e-9),
        "bias {} sigma",
        (mean - truth).abs() / se
    );
    Ok(())
}

#[test]
fn the_same_seed_gives_the_same_frames() -> Result<(), RestirDiError> {
    let (scene, clusters) = scene();
    let run = |seed| -> Result<Vec<[f32; 3]>, RestirDiError> {
        let mut s = ReferenceSampler::new(RestirDiSettings::default(), seed)?;
        let mut out = Vec::new();
        for _ in 0..3 {
            out = s
                .frame(
                    &scene.gbuffer,
                    &scene.camera,
                    &scene.lights,
                    Candidates::Clusters(&clusters),
                )?
                .radiance;
        }
        Ok(out)
    };
    assert_eq!(run(5)?, run(5)?);
    assert_ne!(run(5)?, run(6)?);
    Ok(())
}

#[test]
fn settings_and_inputs_are_validated() {
    let bad = [
        RestirDiSettings {
            initial_candidates: 0,
            ..RestirDiSettings::default()
        },
        RestirDiSettings {
            spatial_taps: 9,
            ..RestirDiSettings::default()
        },
        RestirDiSettings {
            history_cap: 0.5,
            ..RestirDiSettings::default()
        },
        RestirDiSettings {
            depth_tolerance: f32::NAN,
            ..RestirDiSettings::default()
        },
    ];
    for s in bad {
        let err = ReferenceSampler::new(s, 0).expect_err("refused");
        assert_eq!(err.code(), "invalid_setting");
    }
    let (scene, clusters) = scene();
    let mut g = scene.gbuffer.clone();
    g.motion.pop();
    let mut s = ReferenceSampler::new(RestirDiSettings::default(), 0).expect("sampler");
    let err = s
        .frame(&g, &scene.camera, &scene.lights, Candidates::Clusters(&clusters))
        .expect_err("short plane");
    assert_eq!(err.code(), "bad_input");
}

/// Many seeds on the GPU test's scene size: separates a real bias from the heavy-tailed noise
/// of a few seeds (`cargo test -p regia_render --lib bias_probe -- --ignored --nocapture`).
#[test]
#[ignore = "slow probe: many seeds"]
fn bias_probe_many_seeds() -> Result<(), RestirDiError> {
    let scene = SyntheticScene::new(64, 36, LIGHTS, 7);
    let clusters = ClusterLists::build(&scene.camera, 64.0 / 36.0, &scene.lights, 256);
    for (settings, frames, seeds) in [
        (RestirDiSettings::candidates_only(16), 1, 2048),
        (RestirDiSettings::default(), 1, 1024),
        (RestirDiSettings::default(), 6, 512),
        (
            RestirDiSettings {
                spatial_taps: 0,
                ..RestirDiSettings::default()
            },
            6,
            512,
        ),
        (
            RestirDiSettings {
                temporal: false,
                ..RestirDiSettings::default()
            },
            6,
            512,
        ),
    ] {
        let r = evaluate(
            &scene.gbuffer,
            &scene.camera,
            &scene.lights,
            Candidates::Clusters(&clusters),
            &settings,
            frames,
            seeds,
        )?;
        println!(
            "taps {} temporal {} frames {frames} seeds {seeds}: bias {:+.4} ({:.2} sigma), rmse {:.3}",
            settings.spatial_taps, settings.temporal, r.relative_bias, r.bias_sigma, r.rmse_restir
        );
    }
    Ok(())
}
