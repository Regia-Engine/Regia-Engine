// SPDX-License-Identifier: MPL-2.0
//! The bake starting from a CPU probe placement (`regia_pt::probe::bake_gi_volume_placed`,
//! ADR-418): culled probes are never traced and stay invalid with no light, placement offsets
//! reach the volume, and a placement made for another layout is refused. Runs the real kernels on
//! whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::OnceLock;

use regia_asset::baked_gi::{DetailBrick, GiComponent, ProbeFlags, VolumeLayout};
use regia_core::glam::Mat4;
use regia_pt::placement::{PlacementGeometry, PlacementSettings, place_probes};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, bake_gi_volume_placed};
use regia_pt::scenes::uv_sphere;
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtScene, PtSky};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

/// A ball of radius 0.5 at the origin under a uniform sky.
fn ball_scene() -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.5, 4, 4));
    scene.sky = PtSky::Uniform([1.0; 3]);
    let ball = scene.add_mesh(uv_sphere(0.5, 32, 16, false));
    let grey = scene.add_material(PtMaterial::lambert([0.5; 3]));
    scene.add_instance(ball, grey, Mat4::IDENTITY);
    scene
}

/// Four probes 0.53 m apart along +X: at the ball's centre (0.5 m from any exit, past the 0.45
/// cell bound: culled), 3 cm outside its surface (pushed), and two in the open.
fn line_layout(probes: u32) -> VolumeLayout {
    VolumeLayout {
        terrain: None,
        bricks: vec![DetailBrick {
            origin: [0.0; 3],
            spacing: [0.53; 3],
            counts: [probes, 1, 1],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 4.0,
    }
}

fn settings() -> GiBakeSettings {
    GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 64,
            samples: 4,
            max_bounces: 2,
            seed: 5,
            samples_per_submit: 4,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    }
}

#[test]
fn culled_probes_are_never_traced_and_offsets_reach_the_volume() {
    let scene = ball_scene();
    let layout = line_layout(4);
    let geometry = PlacementGeometry::from_scene(&scene);
    let placement = place_probes(&layout, &geometry, &PlacementSettings::default());
    assert_eq!(placement.flags[0], ProbeFlags::INSIDE_GEOMETRY);
    assert_eq!(placement.flags[1], ProbeFlags::VALID | ProbeFlags::RELOCATED);
    assert!(placement.offsets[1][0] > 0.0, "{:?}", placement.offsets[1]);
    assert_eq!(placement.report.culled, 1);

    let placed = bake_gi_volume_placed(
        tracer(),
        &scene,
        &layout,
        &settings(),
        Some(&placement),
        |_, _, _| true,
    )
    .expect("placed bake");
    // The plain bake traces every probe: ADR-862's inside skip (on by default) would also leave
    // out the probe at the ball's centre, which is what this compares the placement against.
    let every_probe = GiBakeSettings {
        skip_inside_fraction: 2.0,
        ..settings()
    };
    let plain = bake_gi_volume(tracer(), &scene, &layout, &every_probe, |_, _, _| true).expect("plain bake");
    assert_eq!(placed.report.rejected_rays, 0);
    assert_eq!(plain.report.rejected_rays, 0);
    assert_eq!(plain.report.inside_skipped, 0);
    // Three probes traced instead of four: the culled one cost no rays.
    assert_eq!(placed.report.rays_traced * 4, plain.report.rays_traced * 3);
    // Without a placement, the default inside skip finds the same probe from its relocation
    // traces and saves the same rays.
    let skipped =
        bake_gi_volume(tracer(), &scene, &layout, &settings(), |_, _, _| true).expect("default bake");
    assert_eq!(skipped.report.inside_skipped, 1);
    assert_eq!(skipped.report.rays_traced, placed.report.rays_traced);
    let v = &placed.volume;
    let m0 = v.probe_meta(0);
    assert!(
        !m0.is_valid() && m0.flags & ProbeFlags::INSIDE_GEOMETRY != 0,
        "{m0:?}"
    );
    // Never traced: no light in any component.
    for c in GiComponent::ALL {
        let sh = v.probe_sh(0, c);
        assert!(sh.iter().all(|ch| ch[0] == 0.0), "{c:?}: {sh:?}");
    }
    assert_eq!(placed.report.inside_geometry, 1);
    assert_eq!(placed.report.valid, 3);
    // The pushed probe keeps at least the placement's offset (relocation may add to it).
    let m1 = v.probe_meta(1);
    assert!(m1.is_valid() && m1.flags & ProbeFlags::RELOCATED != 0, "{m1:?}");
    assert!(m1.offset[0] >= placement.offsets[1][0] - 1e-3, "{:?}", m1.offset);
    // The open probes see the sky.
    assert!(v.probe_sh(3, GiComponent::Sky)[0][0] > 0.0);
}

#[test]
fn a_placement_for_another_layout_is_refused() {
    let scene = ball_scene();
    let geometry = PlacementGeometry::from_scene(&scene);
    let placement = place_probes(&line_layout(3), &geometry, &PlacementSettings::default());
    let err = bake_gi_volume_placed(
        tracer(),
        &scene,
        &line_layout(4),
        &settings(),
        Some(&placement),
        |_, _, _| true,
    )
    .expect_err("placement for 3 probes, layout of 4");
    assert!(err.to_string().contains("probe placement"), "{err}");
}
