// SPDX-License-Identifier: MPL-2.0
//! DO27 ask 4: the GPU irradiance probes see terrain holes (ADR-247).
//!
//! A 30 m mouth is cut through flat ground and the camera stands 1.5 m below the surface inside
//! it. Without the cut every probe under the surface is buried: its rays start under the
//! heightfield, so it is marked inactive. With the cut, the probes in the mouth below the
//! surface are open interior space (active), while the probes under solid ground beside the
//! mouth stay buried and the probes above the ground stay active.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DdgiReadback, DdgiSettings, DirectionalLightUniform,
    FrameQuality, FrameRenderer, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{CdlodConfig, HoleShape, HolesMask, TerrainLayers, TerrainMap, TerrainSharedDraw};

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// A 30 m × 26 m mouth around the origin.
fn mouth() -> HolesMask {
    HolesMask::from_shapes(vec![HoleShape::Polygon {
        points: vec![[-15.0, -13.0], [15.0, -12.0], [14.0, 13.0], [-14.0, 12.0]],
    }])
    .expect("mask")
}

/// 256 m of flat ground at y = 0, 1 m samples, cut by `mask`.
fn draw_with(mask: Option<&HolesMask>) -> TerrainSharedDraw {
    let n = 257u32;
    let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 256.0, 20.0, -10.0).expect("map");
    let mut draw = TerrainSharedDraw::new(
        device(),
        map,
        &TerrainLayers::default(),
        CdlodConfig::default(),
        3,
    )
    .expect("terrain");
    draw.set_holes(mask);
    draw
}

/// Two cascades of 12 × 8 × 12 probes at 1 m and 3 m, re-traced every frame.
fn settings() -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims: [12, 8, 12],
        spacing: 1.0,
        update_divisor: 1,
        bake_on_caller: true,
        ..DdgiSettings::default()
    }
}

fn probes(holes: bool) -> DdgiReadback {
    let mask = holes.then(mouth);
    probes_over(draw_with(mask.as_ref()), [0.0, -1.5, 0.0], [6.0, -1.0, 2.0])
}

/// The probes around a camera at `eye` over `draw`, after 12 frames.
fn probes_over(draw: TerrainSharedDraw, eye: [f32; 3], target: [f32; 3]) -> DdgiReadback {
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: 160,
        height: 96,
        eye,
        target,
        fov_y: 1.0,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.4, -0.85, -0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(settings());
    renderer.set_ddgi_enabled(Some(true));
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    for _ in 0..12 {
        renderer.render_offscreen(device(), &frame).expect("frame");
    }
    renderer
        .ddgi_read_back(device())
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes did not run"))
}

/// Per group: (probes, active probes).
#[derive(Debug, Default)]
struct Groups {
    /// In the mouth (2 m inside its edge), at least 0.5 m under the surface.
    mouth: (u32, u32),
    /// Under solid ground (2 m outside the mouth's edge), at least 0.5 m under the surface.
    buried: (u32, u32),
    /// At least 0.5 m over the ground, outside the mouth.
    above: (u32, u32),
}

fn groups(rb: &DdgiReadback) -> Groups {
    let mask = mouth();
    let mut out = Groups::default();
    for c in 0..rb.cascades {
        let s = settings().spacing * 3f32.powi(c as i32);
        let base = rb.bases[c as usize];
        for z in 0..rb.dims[2] as i32 {
            for y in 0..rb.dims[1] as i32 {
                for x in 0..rb.dims[0] as i32 {
                    let cell = [base[0] + x, base[1] + y, base[2] + z];
                    let (offset, tag) = rb.probe(rb.slot(c, cell));
                    if tag[..3] != cell || tag[3] < 4 {
                        continue;
                    }
                    let p = [
                        cell[0] as f32 * s + offset[0],
                        cell[1] as f32 * s + offset[1],
                        cell[2] as f32 * s + offset[2],
                    ];
                    let active = u32::from(offset[3] > 0.5);
                    let slot = if p[1] < -0.5 && mask.contains_deep(p[0], p[2], 2.0) {
                        &mut out.mouth
                    } else if p[1] < -0.5
                        && !(-1..=1).any(|j| {
                            (-1..=1).any(|i| mask.contains(p[0] + i as f32 * 2.0, p[2] + j as f32 * 2.0))
                        })
                    {
                        &mut out.buried
                    } else if p[1] > 0.5 && !mask.contains(p[0], p[2]) {
                        &mut out.above
                    } else {
                        continue;
                    };
                    slot.0 += 1;
                    slot.1 += active;
                }
            }
        }
    }
    out
}

#[test]
fn probes_in_a_mouth_are_open_space_and_probes_under_ground_stay_buried() {
    let cut = groups(&probes(true));
    let uncut = groups(&probes(false));
    eprintln!("cut: {cut:?}\nuncut: {uncut:?}");
    assert!(cut.mouth.0 > 100, "too few probes in the mouth: {cut:?}");
    assert!(
        cut.buried.0 > 20,
        "too few probes under the ground beside it: {cut:?}"
    );
    assert!(cut.above.0 > 20, "too few probes over the ground: {cut:?}");
    assert_eq!(
        cut.mouth.1, cut.mouth.0,
        "a probe in the mouth reads as buried: {cut:?}"
    );
    assert_eq!(
        cut.buried.1, 0,
        "a probe under solid ground reads as open: {cut:?}"
    );
    assert_eq!(
        cut.above.1, cut.above.0,
        "a probe over the ground reads as buried: {cut:?}"
    );
    // The control: uncut, the same probes are buried.
    assert!(uncut.mouth.0 > 100, "{uncut:?}");
    assert_eq!(
        uncut.mouth.1, 0,
        "uncut ground leaves probes under it open: {uncut:?}"
    );
}

/// 2 m square holes every 8 m over the whole map (one raster shape).
fn pockmarks() -> HolesMask {
    let n = 256u32;
    let coverage = (0..n * n)
        .map(|t| {
            let (i, j) = (t % n, t / n);
            if i % 8 < 2 && j % 8 < 2 { 255 } else { 0 }
        })
        .collect();
    HolesMask::from_shapes(vec![HoleShape::Raster(regia_terrain::HoleRaster {
        center: [0.0, 0.0],
        half_extents: [128.0, 128.0],
        rotation_radians: 0.0,
        width: n,
        height: n,
        coverage,
    })])
    .expect("mask")
}

/// Review holes #9: far probe rays meet the heightfield at a coarse pyramid level. A coarse
/// hit inside a hole used to skip the whole coarse cell, so a ray down a small hole came out
/// at the cell's far side (metres past the hole) instead of meeting the shaft wall at the
/// hole's edge. Over pockmarked flat ground, every far ray of the cut scene now ends within a
/// hole's width (plus a cell) of where the uncut scene's ray hit the ground.
#[test]
fn far_rays_down_small_holes_stop_at_the_hole_not_the_coarse_cell() {
    let eye = [0.0, 6.0, 0.0];
    let target = [8.0, 5.0, 3.0];
    let cut = probes_over(draw_with(Some(&pockmarks())), eye, target);
    let uncut = probes_over(draw_with(None), eye, target);
    assert_eq!(cut.rays.len(), uncut.rays.len());
    let rays_per_slot = 64usize;
    let (mut far, mut through, mut overshoot, mut worst) = (0u32, 0u32, 0u32, 0.0f32);
    for (slot, (a, b)) in cut
        .rays
        .chunks(rays_per_slot)
        .zip(uncut.rays.chunks(rays_per_slot))
        .enumerate()
    {
        let (offset, tag) = uncut.probe(slot as u32);
        if tag[3] < 4 || offset[3] <= 0.5 {
            continue;
        }
        for (c, u) in a.iter().zip(b) {
            // Ground hits far enough out to take a coarse level (cone 0.08: level 2 from 50 m).
            if !(50.0..1e8).contains(&u[3]) {
                continue;
            }
            far += 1;
            if (c[3] - u[3]).abs() > 1e-3 {
                through += 1;
            }
            let excess = if c[3] < 0.0 || c[3] >= 1e8 {
                f32::INFINITY
            } else {
                c[3] - u[3]
            };
            worst = worst.max(excess);
            // A 2 m hole plus a 1 m cell of wall test, along a ray at most ~15° below level.
            if excess > 4.0 {
                overshoot += 1;
            }
        }
    }
    eprintln!("far rays {far}, changed by the holes {through}, overshooting {overshoot}, worst {worst} m");
    assert!(far > 200, "too few far ground rays: {far}");
    assert!(through > 10, "too few far rays meet a hole: {through} of {far}");
    assert_eq!(
        overshoot, 0,
        "{overshoot} of {through} far rays through a hole ran past it (worst {worst} m)"
    );
}
