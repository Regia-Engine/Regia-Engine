// SPDX-License-Identifier: MPL-2.0
//! The CPU lightmap baker against an independent reference (lightmaps phase 2, ADR-1101): the
//! floor of the empty Cornell room baked on its own (`bake_mesh_lightmap`, direct and multi-bounce
//! indirect light) matches classical radiosity (`regia_pt::radiosity`, form factors by Lambert's
//! contour integral and a final gather), which shares no code with the baker. The room is convex,
//! so the reference has no visibility to approximate; its only error is the piecewise-constant
//! radiance of 5 cm patches.
//!
//! CPU only: no GPU is needed.

use regia_pt::lightmap::{LightmapBakeSettings, LightmapScene, MeshLightmapRequest, bake_mesh_lightmap};
use regia_pt::radiosity::Radiosity;
use regia_pt::scenes::{CORNELL_WHITE, cornell_box, cornell_room_quads};

#[test]
fn the_cornell_floor_matches_radiosity() {
    let pt = cornell_box(4, 4, false);
    // `add_room` adds the room's quads in order: the floor is instance 0, with UVs (0, 0) at
    // corner 0, (1, 0) at corner 1 and (0, 1) at corner 3.
    let floor = 0;
    let uv = pt.meshes[pt.instances[floor].mesh].uvs.clone();
    let quads = cornell_room_quads();
    let corners = quads[0].corners.map(|p| p.map(f64::from));
    let scene = LightmapScene::from_pt_scene(&pt).expect("flatten the Cornell room");
    let (size, pad) = (20u32, 2u32);
    let request = MeshLightmapRequest {
        instance: floor,
        name: "floor",
        uv: &uv,
        size: [size, size],
        padding: pad,
    };
    let settings = LightmapBakeSettings {
        samples: 512,
        bounces: 12,
        seed: 3,
        direct_sun: false,
        filter: false,
        threads: 0,
        ..LightmapBakeSettings::default()
    };
    let (map, report) = bake_mesh_lightmap(&scene, &request, &settings, |_, _| {}).expect("bake the floor");
    assert_eq!(report.contents, ["emissive"]);
    assert_eq!(
        report.texels_inside, 0,
        "nothing in the empty room is inside geometry"
    );

    let reference = Radiosity::solve(&quads, 0.05);
    let interior = f64::from(size - 2 * pad);
    let mut ratios = Vec::new();
    for ty in [6u32, 10, 14] {
        for tx in [6u32, 10, 14] {
            let u = (f64::from(tx) + 0.5 - f64::from(pad)) / interior;
            let v = (f64::from(ty) + 0.5 - f64::from(pad)) / interior;
            let x: [f64; 3] = std::array::from_fn(|k| {
                corners[0][k] + u * (corners[1][k] - corners[0][k]) + v * (corners[3][k] - corners[0][k])
            });
            // The floor does not emit: E = π L_out / ρ per channel.
            let l = reference.radiance_at(0, x);
            let got = map.irradiance_at(tx, ty);
            for ((g, l), rho) in got.iter().zip(l).zip(CORNELL_WHITE) {
                let want = std::f64::consts::PI * l / f64::from(rho);
                let r = f64::from(*g) / want;
                assert!(
                    (r - 1.0).abs() < 0.05,
                    "texel ({tx}, {ty}) at {x:?}: baked {got:?}, radiosity E {want} (ratio {r})"
                );
                ratios.push(r);
            }
        }
    }
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    assert!((mean - 1.0).abs() < 0.025, "mean ratio to radiosity {mean}");
}
