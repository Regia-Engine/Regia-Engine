// SPDX-License-Identifier: MPL-2.0
//! Projected decals in the tracer and its roughness AOV (ADR-2951).
//!
//! A known plane with a decal must give the decal's analytic normal (the authored tangent-space
//! normal whiteout-blended over the receiver, in the decal's frame), its roughness and its
//! albedo in the tracer's AOVs, and the plane's own values outside it; a scene without decals
//! must trace the base kernels (no variant compiled) and the image of a scene whose only decal
//! touches nothing must equal the decal-free one.

use std::sync::{Mutex, OnceLock, PoisonError};

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_pt::decal::{CHANNEL_ALBEDO, CHANNEL_NORMAL, CHANNEL_ROUGHNESS};
use regia_pt::{
    PathTracer, PtCamera, PtDecal, PtImage, PtMaterial, PtScene, PtSettings, PtSun, PtTexture, scenes,
};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::shared_headless().expect("headless GPU (lavapipe?)");
        PathTracer::new(&device).expect("compile the tracer")
    })
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static G: Mutex<()> = Mutex::new(());
    G.lock().unwrap_or_else(PoisonError::into_inner)
}

const W: u32 = 64;
const H: u32 = 48;
const PLANE_ROUGHNESS: f32 = 0.8;
const DECAL_ROUGHNESS: f32 = 0.25;
const NORMAL_X: f32 = 0.3;

/// A 6 m grey plane at y = 0 (roughness 0.8) under a sun, seen from above at an angle.
fn plane() -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 3.0, 2.0], [0.0, 0.0, 0.0], 0.8, W, H));
    let floor = scene.add_mesh(scenes::quad([
        [-3.0, 0.0, -3.0],
        [-3.0, 0.0, 3.0],
        [3.0, 0.0, 3.0],
        [3.0, 0.0, -3.0],
    ]));
    let grey = scene.add_material(PtMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: PLANE_ROUGHNESS,
        ..PtMaterial::default()
    });
    scene.add_instance(floor, grey, Mat4::IDENTITY);
    scene.sun = Some(PtSun {
        toward: Vec3::new(1.0, 1.0, 0.3).normalize().to_array(),
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    scene
}

/// The decal: a 1.6 m square at the origin projecting down the plane's normal, replacing the
/// albedo with red, roughness 0.25, the tangent-space normal (0.3, 0) at full strength, hard edge.
fn decal() -> PtDecal {
    // Local +Z along the plane's normal (+Y): right stays +X, up becomes −Z.
    let rotation = Quat::from_rotation_arc(Vec3::Z, Vec3::Y);
    let mut d = PtDecal::from_box(
        Vec3::ZERO,
        rotation,
        Vec3::new(0.8, 0.8, 0.3),
        [0.8, 0.1, 0.1, 1.0],
    );
    d.roughness = DECAL_ROUGHNESS;
    d.normal_xy = [NORMAL_X, 0.0];
    d.normal_strength = 1.0;
    d.angle_fade = [0.0, 0.0];
    d.edge_softness = 0.0;
    d.channels = CHANNEL_ALBEDO | CHANNEL_NORMAL | CHANNEL_ROUGHNESS;
    d.blend = 1.0;
    d
}

/// The decal's normal on the plane, analytically: whiteout of (0.3, 0, √(1 − 0.09)) over (0, 0, 1)
/// is that normal; the frame is t = the decal's right axis (+X), b = n × t, n = +Y.
fn analytic_normal() -> [f32; 3] {
    let nz = (1.0 - NORMAL_X * NORMAL_X).sqrt();
    let n = Vec3::X * NORMAL_X + Vec3::Y * nz;
    n.normalize().to_array()
}

fn trace(scene: &PtScene) -> PtImage {
    tracer()
        .render(
            scene,
            &PtSettings {
                spp: 4,
                max_bounces: 1,
                seed: 7,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace")
}

fn angle_deg(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] * b[0] + a[1] * b[1] + a[2] * b[2])
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

/// Pixels whose 5×5 neighbourhood all satisfy `inside` (away from the decal's edge).
fn eroded(img: &PtImage, inside: impl Fn(usize) -> bool) -> Vec<usize> {
    let (w, h) = (img.width as i32, img.height as i32);
    (0..w * h)
        .filter(|&i| {
            let (x, y) = (i % w, i / w);
            (-2..=2).all(|dy| {
                (-2..=2).all(|dx| {
                    let (xx, yy) = (x + dx, y + dy);
                    xx >= 0 && yy >= 0 && xx < w && yy < h && inside((yy * w + xx) as usize)
                })
            })
        })
        .map(|i| i as usize)
        .collect()
}

#[test]
fn a_decal_on_a_plane_gives_the_analytic_normal_roughness_and_albedo() {
    let _g = serial();
    let mut scene = plane();
    scene.decals.push(decal());
    let img = trace(&scene);
    let red = |i: usize| img.albedo[i][0] > 0.6 && img.albedo[i][1] < 0.2;
    let grey = |i: usize| (img.albedo[i][0] - 0.5).abs() < 0.02 && (img.albedo[i][1] - 0.5).abs() < 0.02;
    let inside = eroded(&img, red);
    let outside = eroded(&img, grey);
    assert!(
        inside.len() > 100 && outside.len() > 100,
        "{} in, {} out",
        inside.len(),
        outside.len()
    );
    let want = analytic_normal();
    for &i in &inside {
        let a = angle_deg(img.normal[i], want);
        assert!(
            a < 0.1,
            "pixel {i}: normal {:?} is {a}° off the decal's {want:?}",
            img.normal[i]
        );
        assert!(
            (img.roughness[i] - DECAL_ROUGHNESS).abs() < 1e-4,
            "pixel {i}: {}",
            img.roughness[i]
        );
        assert!(
            (img.albedo[i][0] - 0.8).abs() < 1e-3,
            "pixel {i}: {:?}",
            img.albedo[i]
        );
    }
    for &i in &outside {
        let a = angle_deg(img.normal[i], [0.0, 1.0, 0.0]);
        assert!(a < 0.1, "pixel {i}: plane normal {:?}", img.normal[i]);
        assert!(
            (img.roughness[i] - PLANE_ROUGHNESS).abs() < 1e-4,
            "pixel {i}: {}",
            img.roughness[i]
        );
    }
    // The decal's normal is lit: toward the sun (+X side) the decal is brighter than the plane
    // at the same albedo would be. Compare the direct light per unit albedo.
    let per_albedo = |i: usize| img.direct[i][0] / img.albedo[i][0].max(1e-6);
    let mean = |px: &[usize]| px.iter().map(|&i| per_albedo(i)).sum::<f32>() / px.len() as f32;
    assert!(
        mean(&inside) > 1.05 * mean(&outside),
        "{} vs {}",
        mean(&inside),
        mean(&outside)
    );
}

/// Modulate, opacity and an atlas cell: half opacity mixes halfway; the atlas alpha is the
/// coverage and its colour multiplies the albedo; the bump comes from the atlas luma.
#[test]
fn opacity_modulation_and_atlas_coverage_weight_the_decal() {
    let _g = serial();
    let mut scene = plane();
    let mut d = decal();
    d.channels = CHANNEL_ALBEDO | CHANNEL_ROUGHNESS;
    d.blend = 0.0;
    d.opacity = 0.5;
    d.albedo = [0.5, 1.0, 1.0];
    scene.decals.push(d);
    let img = trace(&scene);
    // Modulated by mix(1, albedo, 0.5): red 0.5 × 0.75, green 0.5 × 1.
    let inside = eroded(&img, |i| img.albedo[i][0] < 0.45 && img.albedo[i][0] > 0.3);
    assert!(inside.len() > 100, "{}", inside.len());
    for &i in &inside {
        assert!((img.albedo[i][0] - 0.375).abs() < 2e-3, "{:?}", img.albedo[i]);
        assert!((img.albedo[i][1] - 0.5).abs() < 2e-3, "{:?}", img.albedo[i]);
        let r = 0.5 * PLANE_ROUGHNESS + 0.5 * DECAL_ROUGHNESS;
        assert!((img.roughness[i] - r).abs() < 1e-4, "{}", img.roughness[i]);
    }

    // A 2×2 atlas cell: opaque white everywhere: tint 1, coverage 1, flat luma (no bump).
    let mut scene = plane();
    let atlas = scene.textures.len();
    scene.textures.push(PtTexture {
        width: 2,
        height: 2,
        rgba: std::sync::Arc::from(vec![255; 16]),
        srgb: true,
    });
    let mut d = decal();
    d.atlas = Some(atlas);
    d.normal_xy = [0.0, 0.0];
    scene.decals.push(d);
    let img = trace(&scene);
    let inside = eroded(&img, |i| img.albedo[i][0] > 0.6 && img.albedo[i][1] < 0.2);
    assert!(inside.len() > 100, "{}", inside.len());
    for &i in &inside {
        // A flat atlas bumps nothing: the receiver's normal.
        assert!(
            angle_deg(img.normal[i], [0.0, 1.0, 0.0]) < 0.1,
            "{:?}",
            img.normal[i]
        );
    }
    // Transparent atlas: coverage 0, the decal vanishes.
    scene.textures[atlas].rgba = std::sync::Arc::from([255, 255, 255, 0].repeat(4));
    let img = trace(&scene);
    assert!(
        img.albedo.iter().all(|a| a[0] < 0.6),
        "a transparent cell painted"
    );
}

#[test]
fn a_decal_free_scene_traces_the_base_kernels_and_a_decal_that_touches_nothing_changes_nothing() {
    let _g = serial();
    let before = tracer().graph_kernel_compiles();
    let bare = trace(&plane());
    assert_eq!(
        tracer().graph_kernel_compiles(),
        before,
        "a decal-free scene compiled a variant"
    );
    // A decal box floating 5 m above the plane: the variant runs, nothing is inside.
    let mut far = plane();
    let mut d = decal();
    d.center = [0.0, 5.0, 0.0];
    far.decals.push(d);
    let img = trace(&far);
    // The variant is compiled once per process and cached (this binary composes no graphs, so
    // every compile counted is a decal set).
    assert!(
        tracer().graph_kernel_compiles() >= 1,
        "a decal scene traced the base kernels"
    );
    assert_eq!(img.albedo, bare.albedo);
    assert_eq!(img.normal, bare.normal);
    assert_eq!(img.roughness, bare.roughness);
    for (a, b) in img.beauty.iter().zip(&bare.beauty) {
        for k in 0..3 {
            assert!(
                (a[k] - b[k]).abs() <= 1e-6 * b[k].abs().max(1.0),
                "{a:?} vs {b:?}"
            );
        }
    }
    // The roughness AOV of the bare plane is the material's.
    assert!(
        bare.roughness
            .iter()
            .all(|r| (r - PLANE_ROUGHNESS).abs() < 1e-4 || *r == 0.0)
    );
}
