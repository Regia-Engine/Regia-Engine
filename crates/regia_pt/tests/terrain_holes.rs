// SPDX-License-Identifier: MPL-2.0
//! DO27 ask 4: the path tracer and the GI bake see terrain holes (ADR-247). A NaN height is a
//! cut sample (`PtHeightfield::cut_holes`): camera rays pass through the mouth, a probe in the
//! mouth below the rim is a valid probe (not under the terrain), and a baked terrain-layer
//! column over the mouth starts at the rim, not at 0 m or NaN. Runs on lavapipe.

use std::sync::OnceLock;

use regia_asset::baked_gi::{ProbeFlags, VolumeLayout};
use regia_core::glam::Vec3;
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, detail_brick, terrain_layer};
use regia_pt::{PathTracer, PtCamera, PtHeightfield, PtMaterial, PtScene, PtSettings, PtSky};
use regia_render::RenderDevice;
use regia_terrain::{HoleShape, HolesMask};

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

/// A 30 m mouth (a square with bevelled corners) around the origin.
fn mouth() -> HolesMask {
    HolesMask::from_shapes(vec![HoleShape::Polygon {
        points: vec![
            [-15.0, -11.0],
            [-11.0, -15.0],
            [11.0, -15.0],
            [15.0, -11.0],
            [15.0, 11.0],
            [11.0, 15.0],
            [-11.0, 15.0],
            [-15.0, 11.0],
        ],
    }])
    .expect("mask")
}

/// 80 m of flat ground at `height`, 1 m samples, centred on the origin.
fn ground(scene: &mut PtScene, height: f32, holes: bool) {
    let n = 81u32;
    let material = scene.add_material(PtMaterial::lambert([0.3; 3]));
    let mut hf = PtHeightfield {
        width: n,
        depth: n,
        origin: [-40.0, -40.0],
        spacing: [1.0, 1.0],
        heights: vec![height; (n * n) as usize],
        material,
    };
    if holes {
        assert!(hf.cut_holes(&mouth()) > 800, "the mouth cuts its samples");
    }
    scene.terrain = Some(hf);
}

#[test]
fn camera_rays_pass_through_the_mouth() {
    let cam = PtCamera::pinhole([0.0, 60.0, 0.1], [0.0, 0.0, 0.0], 1.0, 48, 48);
    let mask = mouth();
    let image = |holes: bool| {
        let mut scene = PtScene::new(cam);
        scene.sky = PtSky::Uniform([1.0; 3]);
        ground(&mut scene, 0.0, holes);
        let settings = PtSettings {
            spp: 1,
            max_bounces: 1,
            seed: 3,
            rr_start: 3,
            jitter: false,
            samples_per_submit: 16,
        };
        tracer().render(&scene, &settings).expect("render")
    };
    let (cut, uncut) = (image(true), image(false));
    let (fwd, right, up) = cam.basis();
    let th = (0.5 * cam.fov_y).tan();
    let (mut inside, mut outside) = (0, 0);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let sx = ((x as f32 + 0.5) / cam.width as f32) * 2.0 - 1.0;
            let sy = 1.0 - ((y as f32 + 0.5) / cam.height as f32) * 2.0;
            let d = (fwd + right * (sx * th) + up * (sy * th)).normalize();
            let p = Vec3::from_array(cam.eye) + d * (-cam.eye[1] / d.y);
            let i = (y * cam.width + x) as usize;
            assert!(uncut.depth[i] > 0.0, "uncut ground is hit at ({x}, {y})");
            // Away from the edge by the cell the cut may widen to (and a half for the pixel).
            if mask.contains_deep(p.x, p.z, 1.5) {
                inside += 1;
                assert!(
                    cut.depth[i] <= 0.0,
                    "a ray through the mouth hit at ({x}, {y}): {p}"
                );
            } else if !(-1..=1)
                .any(|j| (-1..=1).any(|k| mask.contains(p.x + k as f32 * 2.5, p.z + j as f32 * 2.5)))
            {
                outside += 1;
                assert!(
                    cut.depth[i] > 0.0,
                    "the ground beside the mouth is not hit at ({x}, {y})"
                );
            }
        }
    }
    assert!(
        inside > 300 && outside > 300,
        "inside {inside}, outside {outside}"
    );
}

#[test]
fn a_probe_in_the_mouth_is_valid_and_its_layer_column_starts_at_the_rim() {
    let camera = PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.5, 4, 4);
    let bake = |holes: bool| {
        let mut scene = PtScene::new(camera);
        scene.sky = PtSky::Uniform([1.0; 3]);
        ground(&mut scene, 2.0, holes);
        // One probe 0.5 m under the rim in the mouth's middle, one 1 m over the ground beside it.
        let mut below = detail_brick([0.0, 1.5, 0.0], [0.0, 1.5, 0.0], 1.0);
        below.counts = [1, 1, 1];
        let mut beside = detail_brick([30.0, 3.0, 30.0], [30.0, 3.0, 30.0], 1.0);
        beside.counts = [1, 1, 1];
        let layout = VolumeLayout {
            terrain: None,
            bricks: vec![below, beside],
            moment_resolution: 4,
            max_distance: 8.0,
        };
        let settings = GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: 32,
                samples: 2,
                max_bounces: 1,
                seed: 5,
                samples_per_submit: 16,
                ..ProbeTraceSettings::default()
            },
            ..GiBakeSettings::default()
        };
        let result = bake_gi_volume(tracer(), &scene, &layout, &settings, |_, _, _| true).expect("bake");
        let layer =
            terrain_layer(scene.terrain.as_ref().expect("terrain"), [4.0, 4.0], &[1.0, 3.0]).expect("layer");
        (result, layer)
    };
    let (cut, layer) = bake(true);
    let m = cut.volume.probe_meta(0);
    assert!(m.is_valid() && m.flags & ProbeFlags::UNDER_TERRAIN == 0, "{m:?}");
    assert!(cut.volume.probe_meta(1).is_valid());
    // Every column, over the mouth too, stands on the 2 m ground (the rim).
    assert!(
        layer.ground.iter().all(|g| (g - 2.0).abs() < 1e-4),
        "{:?}",
        layer.ground
    );
    let (uncut, _) = bake(false);
    let m = uncut.volume.probe_meta(0);
    assert!(!m.is_valid() && m.flags & ProbeFlags::UNDER_TERRAIN != 0, "{m:?}");
}
