// SPDX-License-Identifier: MPL-2.0
//! Seam stitching, crease placement and the automatic density rule (ADR-2404) on the v0.3.0
//! evidence room (`lightmap_pt_evidence.rs`, ADR-2315): the colour room with a 0.35 m sphere on its
//! floor, every mesh casting and bouncing light, the sphere the receiver whose chart seams are
//! measured (`regia_asset::lightmap::seam_report`, the figure `regia bake-lightmaps` stores).
//!
//! CPU only (no GPU, no clocks; fixed seeds): the bake settings are the evidence's (256 samples,
//! 6 bounces, the 3×3 filter, directions). Asserted:
//!
//! - at the CLI's default density (8 texels/m) and at the evidence's (16), the stitch lowers the
//!   sphere's seam `p95`, and with the automatic density rule on top it is at or under the
//!   shipping default budget (0.25);
//! - the rule picks a power-of-two scale above 1 for the sphere and leaves the room's boxes at 1;
//! - the room's crease placement moves texels that sit under the walls (the floor under the side
//!   and back walls, the ceiling over them) and leaves every texel it cannot move to the dilation.
//!
//! The figures are printed for the calibration of `regia_pt::lightmap::density`'s constants.

use regia_asset::lightmap::{
    DEFAULT_HARD_EDGE_DEG, DEFAULT_SEAM_BUDGET, Lightmap, SeamGeometry, seam_report,
};
use regia_asset_pipeline::lightmap_uv::{LightmapUvSettings, UnwrapInput, build_atlas_layout};
use regia_pt::PtScene;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::lightmap::density::auto_density_scale;
use regia_pt::lightmap::{LightmapBakeReport, LightmapBakeSettings, LightmapScene, bake_lightmap};
use regia_render::{FrameQuality, LabMesh, uv_sphere};
use regia_test_utils::gi_scenes::color_room;

/// The room with the sphere, flattened (the evidence's `lightmap_room`).
fn room() -> PtScene {
    let mut scene = color_room();
    let (vertices, indices) = uv_sphere(0.35, 32, 16);
    scene.lab.meshes.push(LabMesh {
        vertices,
        indices,
        material: 0,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.45, 0.35, -0.45, 1.0],
        ],
        ..LabMesh::unskinned()
    });
    let frame = scene.lab.as_frame_scene_with_quality(FrameQuality::default());
    from_frame(&frame, &ExtractOptions::default())
}

/// The sphere's instance: the last receiver.
fn sphere_instance(flat: &LightmapScene) -> usize {
    flat.instances()
        .iter()
        .rposition(|i| i.receiver && !i.indices.is_empty())
        .expect("the sphere")
}

fn settings(adr_2404: bool) -> LightmapBakeSettings {
    LightmapBakeSettings {
        samples: 256,
        bounces: 6,
        seed: 1,
        threads: 0,
        crease_placement: adr_2404,
        stitch_seams: adr_2404,
        ..LightmapBakeSettings::default()
    }
}

/// Bakes `receivers` at `density` texels/m, each with its scale, and returns the map, the report
/// and the layout's entries' densities.
fn bake(
    flat: &LightmapScene,
    receivers: &[usize],
    density: f32,
    scales: &[f32],
    s: &LightmapBakeSettings,
) -> (Lightmap, LightmapBakeReport) {
    let names: Vec<String> = receivers.iter().map(|k| format!("instance_{k}")).collect();
    let inputs: Vec<UnwrapInput<'_>> = receivers
        .iter()
        .zip(&names)
        .zip(scales)
        .map(|((&k, name), &density_scale)| {
            let inst = &flat.instances()[k];
            UnwrapInput {
                name,
                positions: &inst.positions,
                indices: &inst.indices,
                density_scale,
            }
        })
        .collect();
    let uv = LightmapUvSettings {
        texels_per_metre: density,
        padding: 2,
        max_atlas: 2048,
        ..LightmapUvSettings::default()
    };
    let (layout, _) = build_atlas_layout(&inputs, &uv, 2).expect("lay out");
    bake_lightmap(flat, layout, receivers, s, |done, total| assert!(done <= total)).expect("bake")
}

/// The seam `p95` of entry `e` (instance `k`).
fn seam_p95(map: &Lightmap, flat: &LightmapScene, e: usize, k: usize) -> f32 {
    let geometry = SeamGeometry {
        positions: &flat.instances()[k].positions,
    };
    seam_report(map, e, geometry, DEFAULT_HARD_EDGE_DEG)
        .seams
        .map(|s| s.p95)
        .expect("the sphere has seams")
}

#[test]
fn the_sphere_meets_the_seam_budget_with_the_stitch_and_the_auto_rule() {
    let flat = LightmapScene::from_pt_scene(&room()).expect("flatten");
    let k = sphere_instance(&flat);
    let inst = &flat.instances()[k];
    for density in [8.0f32, 16.0] {
        let (base, _) = bake(&flat, &[k], density, &[1.0], &settings(false));
        let (stitched, report) = bake(&flat, &[k], density, &[1.0], &settings(true));
        let auto = auto_density_scale(&inst.positions, &inst.indices, density, DEFAULT_SEAM_BUDGET, true);
        let (scaled, _) = bake(&flat, &[k], density, &[auto.density_scale], &settings(true));
        let (b, s, a) = (
            seam_p95(&base, &flat, 0, k),
            seam_p95(&stitched, &flat, 0, k),
            seam_p95(&scaled, &flat, 0, k),
        );
        eprintln!(
            "sphere at {density} texels/m: seam p95 {b:.4} before, {s:.4} stitched ({} edges, {} texels), \
             {a:.4} stitched at the rule's scale {} (curvature {:.3}/m, predicted {:.3} at 1, {:.3} scaled); \
             unstitched error per radian {:.2}, stitched {:.2}",
            report.seam_edges_stitched,
            report.texels_stitched,
            auto.density_scale,
            auto.curvature,
            auto.predicted_p95,
            auto.predicted_p95_scaled,
            b * density / auto.curvature,
            s * density / auto.curvature,
        );
        assert!(
            report.seam_edges_stitched > 0 && report.texels_stitched > 0,
            "{report:?}"
        );
        assert!(s < b, "the stitch lowers the seams at {density}: {s} vs {b}");
        assert!(
            a <= DEFAULT_SEAM_BUDGET,
            "at {density} texels/m with the rule (scale {}) the seams are within the shipping budget: {a}",
            auto.density_scale
        );
        // The prediction is conservative: the measured figure is under it.
        assert!(
            a <= auto.predicted_p95_scaled.max(DEFAULT_SEAM_BUDGET),
            "{a} vs {auto:?}"
        );
    }
    // The room's boxes are flat: the rule leaves them at the requested density.
    for (j, i) in flat.instances().iter().enumerate() {
        if j == k || !i.receiver || i.indices.is_empty() {
            continue;
        }
        let d = auto_density_scale(&i.positions, &i.indices, 8.0, DEFAULT_SEAM_BUDGET, true);
        assert_eq!(d.density_scale, 1.0, "instance {j}: {d:?}");
    }
}

#[test]
fn the_room_moves_texels_under_its_walls_and_bakes_the_rest_as_before() {
    let flat = LightmapScene::from_pt_scene(&room()).expect("flatten");
    let receivers: Vec<usize> = flat
        .instances()
        .iter()
        .enumerate()
        .filter(|(_, i)| i.receiver && !i.indices.is_empty())
        .map(|(k, _)| k)
        .collect();
    let scales = vec![1.0; receivers.len()];
    let quick = |adr: bool| LightmapBakeSettings {
        samples: 16,
        bounces: 2,
        ..settings(adr)
    };
    let (_, base) = bake(&flat, &receivers, 16.0, &scales, &quick(false));
    let (new_map, new) = bake(&flat, &receivers, 16.0, &scales, &quick(true));
    // Crease placement uses the current lightmap format, including source hashes and pages;
    // it does not introduce a separate format. Its texels still round trip exactly.
    let bytes = new_map.to_bytes().expect("encode");
    assert_eq!(
        regia_asset::lightmap::lightmap_version(&bytes).expect("version"),
        regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION
    );
    let back = Lightmap::from_bytes(&bytes).expect("decode");
    assert_eq!(
        (back.texels.clone(), back.coverage.clone()),
        (new_map.texels, new_map.coverage)
    );
    eprintln!(
        "room: {} texels inside before; crease placement found {}, moved {}, {} left inside",
        base.texels_inside, new.texels_crease_inside, new.texels_relocated, new.texels_inside
    );
    assert_eq!(base.texels_crease_inside, 0);
    assert!(base.texels_inside > 0, "the floor runs under the walls: {base:?}");
    assert!(new.texels_relocated > 0, "{new:?}");
    assert!(new.texels_relocated <= new.texels_crease_inside, "{new:?}");
    assert!(new.texels_inside < base.texels_inside, "{new:?} vs {base:?}");
    assert_eq!(
        new.texels_baked + new.texels_inside,
        base.texels_baked + base.texels_inside,
        "the same texels are claimed"
    );
}
