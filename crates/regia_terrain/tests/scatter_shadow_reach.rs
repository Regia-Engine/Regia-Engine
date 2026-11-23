// SPDX-License-Identifier: MPL-2.0
//! A `ShadowCast::Local` caster's reach follows the sun elevation (ADR-047): a 1 m rock on
//! flat ground at a 10° sun throws a shadow `r / tan(10°) ≈ 5.67 m` long, and the
//! sun-following reach draws the caster into the cascade that holds the shadow's far end, so the
//! shadow is not cut where the fixed `3 r + 1 m` reach (a ~20° sun) would have clipped it.
//!
//! The test renders the real shadow path headlessly (like
//! `regia_render/tests/shadows.rs::shadows_darken_the_receiver`) from a low camera looking
//! down-sun, so the 1 m rock sits in the near cascade while its ~5.67 m shadow tip falls in the
//! next cascade. It measures the darkened run along the ground and asserts it reaches
//! `r / tan(10°) · 0.9` with the sun-following reach, and that it falls short of that bar at
//! reach 0 (the shadow is cut where it crosses into the next cascade). A luma floor is not a
//! shadow: the run is measured in metres on the ground, never asserted from the reach formula,
//! and the reach-0 frame proves the camera really straddles a cascade split.

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    RenderDevice, ShadowCast,
};
use regia_terrain::scatter::field::local_shadow_reach;

const W: u32 = 512;
const H: u32 = 512;
/// The sun sits 10° above the horizon.
const SUN_ELEVATION: f32 = 10.0_f32.to_radians();
/// The reach parameter of the "1 m rock" (its bounding radius, metres).
const ROCK_RADIUS: f32 = 1.0;
/// Low camera eye behind and above the rock, looking down-sun (+Z).
const EYE: [f32; 3] = [0.0, 3.0, -4.0];
/// Look-at point down-sun so the near-cascade split lands inside the shadow.
const TARGET: [f32; 3] = [0.0, 0.0, 3.5];
/// Vertical field of view.
const FOV_Y: f32 = 0.9;
/// Near plane (the camera sits a few metres from the rock).
const NEAR: f32 = 0.1;
/// Far plane: with two cascades over `[NEAR, FAR]` the first split falls at ~8.5 m from the
/// eye, between the rock (~4.7 m) and its shadow tip (~10.6 m), so the shadow crosses a split.
const FAR: f32 = 54.0;
/// Sun illuminance (lux).
const SUN_LUX: f32 = 100_000.0;
/// A pixel is "shadowed" when the shadow-casting frame darkens it by at least this much luma.
const SHADOW_DARKEN: f32 = 12.0;

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

/// Renders the flat-ground scene once with the rock's shadow policy, returning RGBA8 pixels.
///
/// The rock is always present; only its shadow policy differs between calls, so the pixel diff
/// between a shadow-casting frame and an [`ShadowCast::Off`] frame is exactly the shadow it cast
/// (plus its own self-shadow, which stays within the rock's metre).
fn render(rock_shadow: ShadowCast) -> Vec<u8> {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU: {err}"));
    // Flat ground: a 30 m square slab whose top face sits at y = 0 (the shadow receiver).
    let (ground_v, ground_i) = regia_render::box_mesh([15.0, 0.05, 15.0]);
    let ground_v = regia_render::translate_mesh(&ground_v, [0.0, -0.05, 0.0]);
    // A 1 m rock (cube) centred at the origin, resting on the ground: 1 m across, 1 m tall.
    let (rock_v, rock_i) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let rock_v = regia_render::translate_mesh(&rock_v, [0.0, 0.5, 0.0]);
    let ground_mat = LabMaterial {
        base_color: [0.75, 0.75, 0.78, 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    };
    let rock_mat = LabMaterial {
        base_color: [0.3, 0.3, 0.32, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    let scene = FrameScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        // Light travels toward +Z and down (the sun sits 10° above the horizon toward -Z), so
        // the rock's shadow falls along +Z.
        directional: DirectionalLightUniform::new(
            [0.0, -SUN_ELEVATION.sin(), SUN_ELEVATION.cos()],
            [1.0, 1.0, 1.0],
            SUN_LUX,
        ),
        local_lights: &[],
        quality: FrameQuality {
            csm_cascade_count: 2,
            csm_resolution: 512,
            ..FrameQuality::LAB
        },
        meshes: vec![
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &ground_v,
                indices: &ground_i,
                material: &ground_mat,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                // The ground receives the shadow; it casts none.
                shadow: ShadowCast::Off,
                uv1: &[],
                lod_dither: 0.0,
            },
            FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 2,
                object_key: 2,
                vertices: &rock_v,
                indices: &rock_i,
                material: &rock_mat,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: rock_shadow,
                uv1: &[],
                lod_dither: 0.0,
            },
        ],
    };
    let mut renderer = FrameRenderer::new();
    // Two warm frames, then the read (warm-up builds the shadow pipelines and cache).
    for _ in 0..2 {
        renderer.render_offscreen(&device, &scene).expect("warm frame");
    }
    renderer.render_offscreen(&device, &scene).expect("frame").pixels
}

/// Projects a world point onto the framebuffer with the exact pair the renderer uses: a
/// right-handed look-at and a DirectX (0..1 depth) perspective.
fn project(p: [f32; 3]) -> (i32, i32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        regia_core::glam::Vec3::from_array(EYE),
        regia_core::glam::Vec3::from_array(TARGET),
        regia_core::glam::Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    let clip = (proj * view) * regia_core::glam::Vec4::from((regia_core::glam::Vec3::from_array(p), 1.0));
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as i32,
        ((0.5 - ndc.y * 0.5) * H as f32) as i32,
    )
}

/// Mean luma over a small patch around a pixel, so a texel of PCF soften does not matter.
fn patch_luma(pixels: &[u8], x: i32, y: i32) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for dy in -2..=2 {
        for dx in -2..=2 {
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 || px >= W as i32 || py >= H as i32 {
                continue;
            }
            let i = ((py as u32 * W + px as u32) * 4) as usize;
            sum += luma(&pixels[i..i + 4]);
            n += 1.0;
        }
    }
    sum / n
}

/// How far (metres along the ground, +Z down-sun) the rock's shadow darkens past the rock's
/// centre, plus the number of darkened sample points. Samples the ground along the shadow's
/// centreline and reports the farthest point the shadow reaches.
fn shadow_run(rock_shadow: ShadowCast) -> (f32, usize) {
    let lit = render(ShadowCast::Off);
    let shadowed = render(rock_shadow);
    let mut run = 0.0f32;
    let mut darkened = 0usize;
    let mut z = 0.5f32;
    while z <= 7.5 {
        let (x, y) = project([0.0, 0.0, z]);
        let delta = patch_luma(&lit, x, y) - patch_luma(&shadowed, x, y);
        if delta > SHADOW_DARKEN {
            run = z;
            darkened += 1;
        }
        z += 0.05;
    }
    (run, darkened)
}

/// The full shadow length a 1 m rock throws on flat ground at 10°: `r / tan(10°)`.
fn full_shadow_m() -> f32 {
    ROCK_RADIUS / SUN_ELEVATION.tan()
}

#[test]
fn a_low_sun_rock_shadow_is_not_cut_on_flat_ground() {
    // The sun-following reach of a 1 m rock at 10° (ADR-047).
    let reach = local_shadow_reach(ROCK_RADIUS, SUN_ELEVATION);
    let full = full_shadow_m();
    // The camera straddles the near-cascade split: the rock is in the near cascade and its
    // ~5.67 m shadow tip is in the next, so the reach is what carries the shadow across the
    // split. At reach 0 the caster stays in its own cascade and the shadow is cut short.
    let (cut, cut_px) = shadow_run(ShadowCast::Local {
        reach: 0.0,
        max_cascades: 2,
        shadow_scale: 1.0,
        shadow_lift: 0.0,
        anchor: None,
        lod_switch_keeps_pages: false,
    });
    let (run, run_px) = shadow_run(ShadowCast::Local {
        reach,
        max_cascades: 2,
        shadow_scale: 1.0,
        shadow_lift: 0.0,
        anchor: None,
        lod_switch_keeps_pages: false,
    });
    eprintln!(
        "rock shadow at 10°: reach {reach:.2} m run {run:.2} m ({run_px} pts), reach 0 run {cut:.2} m ({cut_px} pts) (full {full:.2} m)"
    );
    // With the sun-following reach the shadow reaches the far cascade and is not cut.
    assert!(
        run >= full * 0.9,
        "the 10° shadow was cut at {run:.2} m, below {} m",
        full * 0.9
    );
    // At reach 0 the shadow stops at the cascade boundary it cannot cross. This guard fails if
    // the camera ever stops straddling the split (a one-cascade view would cast the full shadow
    // at reach 0 too), so the proof cannot regress to true-by-construction.
    assert!(
        cut < full * 0.9,
        "reach 0 still cast {cut:.2} m; the camera must place the rock and its shadow tip in different cascades"
    );
}
