// SPDX-License-Identifier: MPL-2.0
//! The froxel fog's front-to-back integral against its CPU mirror (`integrate_segment`) in a
//! homogeneous fog lit by the fallback sky ambient. A thin haze (Mars dust, 1e-5–3e-5 /m)
//! once kept only σ / 1e-4 of its in-scatter because the shader floored σ at 1e-4 /m inside
//! the integral (lane mars-haze, 2026-10-09). Headless device required (lavapipe in CI).

use regia_render::{
    RenderDevice, VolumetricFogSettings, fog_layer_optical_depth, integrate_segment,
    render_uniform_fog_inscatter,
};

/// The CPU mirror's in-scatter for the same view: the fallback ambient (60, 75, 100) cd/m²
/// dimmed by the fog column above (Elsasser's 1.66 diffusivity), scattered with the fog's
/// albedo, integrated from the near plane (0.5 m) to the far plane.
fn expected(density: f32, far_m: f32) -> [f32; 3] {
    let settings = VolumetricFogSettings {
        density,
        height_falloff: 0.0,
        albedo: [0.85, 0.88, 0.92],
        ..VolumetricFogSettings::default()
    };
    let sky_t = (-1.66 * fog_layer_optical_depth([0.0; 3], [0.0, 1.0, 0.0], &settings)).exp();
    let ambient = [60.0f32, 75.0, 100.0];
    let per_m = std::array::from_fn(|c| density * settings.albedo[c] * ambient[c] * sky_t);
    integrate_segment(per_m, density, far_m - 0.5).0
}

#[test]
fn froxel_fog_in_scatter_matches_the_cpu_integral_from_thin_haze_to_thick_fog() {
    let device = RenderDevice::headless().expect("headless device");
    let far = 10_000.0f32;
    // 1e-5 and 2e-5: the Mars haze range the floor (and a 1e-5 /m lighting cut) broke;
    // 1.5e-4: a fog the floor never touched (denser fogs dim the fallback ambient below f16
    // precision through the 20 km column above).
    for density in [1.0e-5f32, 2.0e-5, 1.5e-4] {
        let got = render_uniform_fog_inscatter(&device, density, far).expect("render");
        let want = expected(density, far);
        for c in 0..3 {
            let ratio = got[c] / want[c];
            eprintln!(
                "density {density:e}: channel {c} GPU {:.3} CPU {:.3} ratio {ratio:.4}",
                got[c], want[c]
            );
            assert!(
                (ratio - 1.0).abs() < 0.03,
                "density {density}: channel {c} GPU/CPU {ratio}"
            );
        }
    }
}
