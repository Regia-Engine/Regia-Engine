// SPDX-License-Identifier: MPL-2.0
//! What the cluster cull costs on the GPU by itself, phase by phase (ADR-2294's check that the
//! deterministic order costs no more than the atomic slots did). Ignored: it prints, it asserts
//! nothing about time.
//!
//! ```sh
//! REGIA_CULL_COST_SCENES=4096x2048,4096x8192,16384x2048 cargo test -p regia_render --lib \
//!     cull_cost -- --ignored --nocapture
//! ```

use super::*;

/// `<instances>x<triangles per instance>` pairs.
fn scenes() -> Vec<(usize, u32)> {
    regia_core::brand::env_var("REGIA_CULL_COST_SCENES")
        .unwrap_or_else(|_| "1024x2048,4096x2048".to_string())
        .split(',')
        .filter_map(|pair| {
            let (a, b) = pair.trim().split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .collect()
}

#[test]
#[ignore = "timing printout; run on the bench GPU"]
fn cull_cost() {
    let device = crate::test_device();
    let info = device.adapter.get_info();
    eprintln!("adapter: {} ({:?})", info.name, info.backend);
    let frames: u32 = regia_core::brand::env_var("REGIA_CULL_COST_FRAMES")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(64);
    let settings = post();
    let rock = LabMaterial::default();
    for (instances, triangles) in scenes() {
        let segments = ((f64::from(triangles)).sqrt() * 1.4).max(4.0) as u32;
        let sphere = crate::uv_sphere(0.45, segments, (segments / 2).max(3));
        let side = (instances as f64).sqrt().ceil() as usize;
        let base = fixture(1);
        let mut s = scene(&base, &settings, true, false);
        s.width = 1920;
        s.height = 1080;
        s.far = 2000.0;
        s.eye = [0.0, side as f32 * 0.3 + 3.0, 6.0];
        s.target = [0.0, 0.0, -(side as f32) * 0.5];
        s.meshes = (0..instances)
            .map(|i| FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1001,
                object_key: 100 + i as u64,
                vertices: &sphere.0,
                indices: &sphere.1,
                material: &rock,
                model: translate(
                    (i % side) as f32 - side as f32 * 0.5,
                    0.5,
                    -((i / side) as f32),
                    1.0,
                ),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            })
            .collect();
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        if !renderer.visbuffer_pass_mut().enable_cull_timer(&device) {
            eprintln!("no timestamps inside encoders on this adapter");
            return;
        }
        for _ in 0..8 {
            render(&mut renderer, &device, &s);
        }
        let (mut one, mut two) = (0.0f64, 0.0f64);
        for _ in 0..frames {
            render(&mut renderer, &device, &s);
            let (a, b) = renderer
                .visbuffer_pass_mut()
                .read_cull_ms(&device)
                .expect("readback")
                .expect("timer on");
            one += a;
            two += b;
        }
        let n = f64::from(frames);
        let counters = renderer.visbuffer_counters(&device).expect("readback");
        eprintln!(
            "{instances}x{triangles}: cull phase 1 {:.4} ms, phase 2 {:.4} ms, total {:.4} ms (mean of {frames}); \
             candidates {}; {counters:?}",
            one / n,
            two / n,
            (one + two) / n,
            renderer.visbuffer_stats().candidate_clusters,
        );
    }
}
