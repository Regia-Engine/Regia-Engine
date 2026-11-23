// SPDX-License-Identifier: MPL-2.0
//! A camera far above a height-fog layer looking down (audit 5, 2026-10-21): the layer's
//! analytic column `ρ(start)·(1 − e^(−x))/x` was `0 · inf` once the ray dropped more than about
//! `88 / k` metres, so the ground was replaced by a black medium hit. The column of a thin
//! exponential haze is `σ/k` metres of extinction (a few percent here), so the ground must
//! stay lit and finite.

use std::sync::OnceLock;

use regia_pt::scenes;
use regia_pt::{PathTracer, PtMaterial, PtMediumLayer, PtPhase, PtScene, PtSettings};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn mean_luminance(scene: &PtScene) -> f64 {
    let settings = PtSettings {
        spp: 16,
        max_bounces: 2,
        seed: 3,
        rr_start: 3,
        jitter: true,
        samples_per_submit: 16,
    };
    let img = tracer().render(scene, &settings).expect("render");
    assert!(
        img.beauty.iter().all(|c| c.iter().all(|v| v.is_finite())),
        "non-finite pixel"
    );
    img.beauty
        .iter()
        .map(|c| 0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2]))
        .sum::<f64>()
        / img.beauty.len() as f64
}

fn scene(fog: bool) -> PtScene {
    let mut s = scenes::sun_plane(PtMaterial::lambert([0.5; 3]), 1.0, 100_000.0, 0.0047, 8);
    // The plane is 100 m square: from 5 km up a 0.01 rad view (50 m across) stays on it. Every
    // pixel's ray drops 5 km through the layer, far past the `88 / k` m where the column broke.
    s.camera.eye = [0.0, 5000.0, 0.0];
    s.camera.target = [10.0, 0.0, 0.0];
    s.camera.fov_y = 0.01;
    if fog {
        s.media.layers.push(PtMediumLayer {
            scattering: [1e-3; 3],
            absorption: [0.0; 3],
            reference_height: 0.0,
            falloff: 0.04,
            min_height: -100.0,
            max_height: 20_000.0,
            phase: PtPhase::HenyeyGreenstein([0.0; 3]),
            aerial: false,
        });
    }
    s
}

#[test]
fn the_ground_stays_lit_through_a_long_descent_into_height_fog() {
    let clear = mean_luminance(&scene(false));
    let hazy = mean_luminance(&scene(true));
    assert!(clear > 1.0, "the clear ground is lit: {clear}");
    let ratio = hazy / clear;
    assert!(
        (0.8..1.2).contains(&ratio),
        "thin haze barely changes the ground: clear {clear}, hazy {hazy}"
    );
}
