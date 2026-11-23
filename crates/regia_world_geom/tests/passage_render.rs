// SPDX-License-Identifier: MPL-2.0
//! Plan row W-07 (spec 121 § 9.2 item 3, ADR-8175): the `Passages` render half of the cook. The render mesh shares
//! the canonical section planes, subdivides every face, and no canonical (collision) triangle reaches the draw list;
//! the canonical bytes do not move.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use regia_detsim::float::det_max_f64;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{
    CookError, CookInputs, CookOutput, GroundInput, WorldTriangleMm, cook, quantize_mesh, trim, weld,
};
use regia_world_geom::cooked::{TriKind, WorldGeomCooked, content_hash_of, split_file};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::render::{LayerRender, PresentationCooked, RenderPart};
use regia_world_geom::tunnel_bake::point_triangle_dist2;
use regia_world_geom::{TriRef, WorldGeom};

const MAPS: [&str; 3] = ["switchbacks", "atacama_rail", "cinder_steps"];

fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    }
}

const ROLLING_SIDE: usize = 65;

fn rolling_counts() -> Vec<u16> {
    (0..ROLLING_SIDE * ROLLING_SIDE)
        .map(|i| {
            let (x, z) = (i % ROLLING_SIDE, i / ROLLING_SIDE);
            (20_000 + (x * 37 + z * 91) % 640) as u16
        })
        .collect()
}

fn rolling_terrain() -> TerrainSampler {
    TerrainSampler::new(
        HeightGrid::square(ROLLING_SIDE, rolling_counts()).unwrap(),
        1280.0,
    )
}

fn rolling_ground() -> GroundInput {
    let counts = rolling_counts();
    let centre = counts[(ROLLING_SIDE / 2) * ROLLING_SIDE + ROLLING_SIDE / 2];
    GroundInput {
        lattice_n: ROLLING_SIDE as u32,
        width_m: 1280.0,
        offset_m: -(centre as f32) / 32.0,
        step_m: 1.0 / 32.0,
        counts,
    }
}

fn document(look: &str) -> String {
    format!(
        "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), {look})])"
    )
}

fn cook_map(map: &str, look: &str, ground: &GroundInput, terrain: &TerrainSampler) -> CookOutput {
    let passages = PassagesDocument::from_layout(for_map(map).unwrap())
        .to_ron()
        .unwrap();
    cook_text(&passages, look, ground, terrain)
}

fn cook_text(passages: &str, look: &str, ground: &GroundInput, terrain: &TerrainSampler) -> CookOutput {
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let doc = document(look);
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground,
            terrain: Some(terrain),
            read_file: &read,
        })
    };
    match run(&doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            let waived = waive_ramp_facets(&doc, &report).expect("waive ramp facets");
            run(&waived).unwrap_or_else(|e| panic!("{e} ({})", e.code()))
        }
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

/// The canonical triangles of layer 0 in world millimetres, canonical index order.
fn canonical_layer0(c: &WorldGeomCooked) -> Vec<WorldTriangleMm> {
    c.triangles_sorted()
        .into_iter()
        .filter(|(_, t)| t.layer == 0)
        .map(|(o, t)| WorldTriangleMm {
            v: t.v.map(|p| [p[0] + o[0], p[1], p[2] + o[1]]),
            kind: t.kind,
            flags: t.flags,
        })
        .collect()
}

/// The presentation section of a cooked file, read back from the file bytes.
fn presentation(out: &CookOutput) -> PresentationCooked {
    let parts = split_file(&out.file.encode().unwrap()).unwrap();
    PresentationCooked::decode(&parts.presentation).unwrap()
}

fn sorted(mut t: [[i32; 3]; 3]) -> [[i32; 3]; 3] {
    t.sort_unstable();
    t
}

fn m(p: [i32; 3]) -> [f64; 3] {
    p.map(|c| f64::from(c) / 1000.0)
}

/// Plan § 4.3 (DO27's "flat collision-tube material" known issue): the Switchbacks bore cooks to a render mesh whose
/// draw list holds no canonical collision triangle. Every draw triangle names (by `TriRef`) the canonical face it
/// subdivides, that face exists in the loaded `WorldGeom`, and the draw triangle is never that face nor any other
/// canonical triangle; every non-degenerate canonical face is drawn, as `subdivision²` pieces.
#[test]
fn collision_triangles_never_reach_the_draw_list() {
    for map in MAPS {
        let out = cook_map(map, "", &canyon_ground(), &canyon_terrain());
        let geom = WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap();
        let refs: BTreeSet<TriRef> = geom.triangles().iter().map(|t| t.tri).collect();
        let canonical = canonical_layer0(&out.file.canonical);
        let canonical_set: BTreeSet<[[i32; 3]; 3]> = canonical.iter().map(|t| sorted(t.v)).collect();
        let pres = presentation(&out);
        assert_eq!(pres.layers.len(), 1, "{map}");
        let layer: &LayerRender = &pres.layers[0];
        assert_eq!(layer.subdivision, 4, "{map}: the default subdivision");
        assert!(!layer.triangles.is_empty(), "{map}: no render mesh");
        let mut pieces: BTreeMap<u32, u32> = BTreeMap::new();
        for (t, corners) in layer.triangles.iter().zip(layer.triangles_mm()) {
            let parent = TriRef {
                layer: layer.layer,
                triangle: t.source.face,
            };
            assert!(
                refs.contains(&parent),
                "{map}: {parent:?} is not a canonical triangle"
            );
            let face = &canonical[parent.triangle as usize];
            assert_ne!(
                sorted(corners),
                sorted(face.v),
                "{map}: {parent:?} drawn as it is"
            );
            assert!(
                !canonical_set.contains(&sorted(corners)),
                "{map}: a canonical triangle is in the draw list ({parent:?} piece {})",
                t.source.piece
            );
            let part = match face.kind {
                TriKind::Floor => RenderPart::Floor,
                TriKind::Roof => RenderPart::Roof,
                TriKind::Wall | TriKind::Blocker => RenderPart::Wall,
            };
            assert_eq!(t.part, part, "{map}");
            *pieces.entry(t.source.face).or_default() += 1;
        }
        let drawn_faces = canonical
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let [a, b, c] = t.v.map(|p| p.map(i64::from));
                let (u, w) = (
                    [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
                    [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
                );
                [
                    u[1] * w[2] - u[2] * w[1],
                    u[2] * w[0] - u[0] * w[2],
                    u[0] * w[1] - u[1] * w[0],
                ] != [0, 0, 0]
            })
            .count();
        assert_eq!(pieces.len(), drawn_faces, "{map}: every canonical face is drawn");
        assert!(
            pieces.values().all(|n| (14..=16).contains(n)),
            "{map}: each face is split into 16 pieces (a sliver may lose a zero-area one)"
        );
        // The report lists the render triangles per section (passage).
        let report = &out.report.layers[0];
        assert_eq!(report.render_triangles as usize, layer.triangles.len(), "{map}");
        assert_eq!(
            report.render_sections.iter().map(|s| s.triangles).sum::<u32>(),
            report.render_triangles,
            "{map}"
        );
        assert_eq!(
            report
                .render_sections
                .iter()
                .map(|s| s.passage.as_str())
                .collect::<Vec<_>>(),
            for_map(map)
                .unwrap()
                .passages
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            "{map}"
        );
        assert!(
            report.render_sections.iter().all(|s| s.triangles > 0),
            "{map}: a passage draws nothing"
        );
        // One mouth cut quad per opening.
        assert_eq!(layer.mouths.len(), for_map(map).unwrap().openings.len(), "{map}");
    }
}

/// Collision displacement 0: every render vertex lies on the canonical face it subdivides (so on its section plane)
/// within 1 mm, on flat and on rolling ground (the weld moves canonical vertices; the render follows). The straight
/// sections of the Switchbacks tube are checked by name, so the test cannot pass on an empty set.
#[test]
fn render_shares_the_collision_section_planes() {
    for (terrain, ground) in [
        (canyon_terrain(), canyon_ground()),
        (rolling_terrain(), rolling_ground()),
    ] {
        for map in MAPS {
            let out = cook_map(map, "", &ground, &terrain);
            let canonical = canonical_layer0(&out.file.canonical);
            let pres = presentation(&out);
            let layer = &pres.layers[0];
            let mut worst = 0.0f64;
            for (t, corners) in layer.triangles.iter().zip(layer.triangles_mm()) {
                let face = canonical[t.source.face as usize].v.map(m);
                for p in corners {
                    worst = det_max_f64(
                        worst,
                        regia_detsim::math::sqrt_f64(point_triangle_dist2(m(p), &face)),
                    );
                }
            }
            assert!(worst <= 0.001, "{map}: a render vertex is {worst} m off its face");
        }
    }
    // A straight tube section (two centreline points): floors flat at the tube floor, walls vertical, a flat lid.
    let straight = r#"(version: 1, layout: (map_id: "straight", version: 2, sample_step_m: 5.0, passages: [(id: "bore", kind: tube, width_m: 22.0, ceiling_m: 12.0, points: [(-100.0, -286.0, 10.0), (100.0, -286.0, 10.0)])], openings: [], junctions: []))"#;
    let out = cook_text(straight, "", &canyon_ground(), &canyon_terrain());
    let canonical = canonical_layer0(&out.file.canonical);
    let pres = presentation(&out);
    let layer = &pres.layers[0];
    let mut planes: BTreeMap<RenderPart, usize> = BTreeMap::new();
    for (t, corners) in layer.triangles.iter().zip(layer.triangles_mm()) {
        let face = canonical[t.source.face as usize].v;
        let plane_ok = match t.part {
            RenderPart::Floor => corners.iter().all(|p| p[1] == -286_000),
            RenderPart::Roof => corners.iter().all(|p| p[1] == -274_000),
            // The side walls stand at z = 10 m +- 11 m; the end caps at x = +-100 m.
            RenderPart::Wall => {
                let same = |k: usize| face.iter().all(|p| p[k] == face[0][k]);
                (same(2) && corners.iter().all(|p| p[2] == face[0][2]))
                    || (same(0) && corners.iter().all(|p| p[0] == face[0][0]))
            }
        };
        assert!(
            plane_ok,
            "{:?} {corners:?} is off its face's plane {face:?}",
            t.part
        );
        *planes.entry(t.part).or_default() += 1;
    }
    assert!(
        planes.len() == 3 && planes.values().all(|n| *n >= 16),
        "{planes:?}"
    );
}

/// The render output is byte-stable: two cooks give the same file, presentation section included.
#[test]
fn cook_twice_gives_identical_bytes() {
    for (terrain, ground) in [
        (canyon_terrain(), canyon_ground()),
        (rolling_terrain(), rolling_ground()),
    ] {
        for map in MAPS {
            let a = cook_map(map, "", &ground, &terrain);
            let b = cook_map(map, "", &ground, &terrain);
            assert!(!a.file.presentation.is_empty(), "{map}: no presentation section");
            assert_eq!(a.file.presentation, b.file.presentation, "{map}");
            assert_eq!(a.file.encode().unwrap(), b.file.encode().unwrap(), "{map}");
            assert_eq!(a.report, b.report, "{map}");
        }
    }
}

/// The render cook never changes the canonical (collision) bytes: the canonical triangles are still the pinned
/// mesher's (on the layout with its skylight shafts opened, W-08), quantized, welded and trimmed (W-06's rule; W-01's `mesh_bits` hashes pin the mesher itself), and a
/// render-only change (subdivision, material, UV tile) leaves `content_hash` where it was while the presentation
/// section moves.
#[test]
fn passages_collision_bytes_unchanged_by_the_render_cook() {
    for (terrain, ground) in [
        (canyon_terrain(), canyon_ground()),
        (rolling_terrain(), rolling_ground()),
    ] {
        for map in MAPS {
            let out = cook_map(map, "", &ground, &terrain);
            // Since lane W-08 the cook meshes the layout with every skylight shaft opened (K8, ADR-8210).
            let layout = for_map(map).unwrap();
            let mut sampled = layout.sample(Some(&terrain));
            regia_world_geom::sections::open_skylight_shafts(&mut sampled, &layout.openings);
            let mesh = sampled.mesh(&layout.openings, Some(&terrain));
            let lattice = ground.lattice(16).unwrap();
            let mut expected = quantize_mesh(&mesh.triangles).unwrap();
            weld(&mut expected, Some(&ground), &lattice);
            trim(&mut expected, Some(&ground), &lattice);
            assert_eq!(canonical_layer0(&out.file.canonical), expected, "{map}");
            let hash = content_hash_of(&out.file.canonical).unwrap();
            for look in [
                "look: (subdivision: 2)",
                "look: (subdivision: 16)",
                "look: (uv_tile_m: 2.0)",
                "material: \"rock/basalt\"",
                "look: (detail: Some((normal_displace: 0.0, noise_amplitude: 0.0, noise_scale: 0.9, seed: 7)))",
            ] {
                let other = cook_map(map, look, &ground, &terrain);
                assert_eq!(
                    content_hash_of(&other.file.canonical).unwrap(),
                    hash,
                    "{map}: {look}"
                );
                if !look.contains("detail") {
                    assert_ne!(other.file.presentation, out.file.presentation, "{map}: {look}");
                }
            }
        }
    }
}

/// Displaced render detail is not cooked yet (lane W-16) and a subdivision of 1 would draw the canonical triangles:
/// both are refused by name, never cooked silently.
#[test]
fn displaced_detail_and_subdivision_one_are_refused() {
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let read = |_: &str| Ok(passages.as_bytes().to_vec());
    let code = |look: &str| {
        let doc = document(look);
        cook(&CookInputs {
            document: doc.as_bytes(),
            ground: &canyon_ground(),
            terrain: Some(&canyon_terrain()),
            read_file: &read,
        })
        .err()
        .map(|e| e.code())
    };
    assert_eq!(
        code(
            "look: (detail: Some((normal_displace: 0.05, noise_amplitude: 0.0, noise_scale: 0.9, seed: 7)))"
        ),
        Some("cook_not_built")
    );
    assert_eq!(
        code(
            "look: (detail: Some((normal_displace: 0.0, noise_amplitude: 0.12, noise_scale: 0.9, seed: 7)))"
        ),
        Some("render_off_collision")
    );
    assert_eq!(code("look: (subdivision: 1)"), Some("meshlayers_invalid"));
    assert_eq!(code("look: (subdivision: 17)"), Some("meshlayers_invalid"));
    let subdivision_two = code("look: (subdivision: 2)");
    assert!(
        subdivision_two.is_none() || subdivision_two == Some("ramp_facet_slope_over_limit"),
        "{subdivision_two:?}"
    );
}

/// The render mesh is as closed as the canonical one: a shared canonical edge splits into the same points from both
/// faces, so the render boundary edges are exactly the canonical boundary edges split `subdivision` times.
#[test]
fn render_boundary_is_the_canonical_boundary_split() {
    let out = cook_map("switchbacks", "", &canyon_ground(), &canyon_terrain());
    let canonical = canonical_layer0(&out.file.canonical);
    let pres = presentation(&out);
    let layer = &pres.layers[0];
    let boundary = |tris: &mut dyn Iterator<Item = [[i32; 3]; 3]>| -> usize {
        let mut edges: BTreeMap<([i32; 3], [i32; 3]), u32> = BTreeMap::new();
        for t in tris {
            for (p, q) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                *edges.entry(if p <= q { (p, q) } else { (q, p) }).or_default() += 1;
            }
        }
        edges.values().filter(|n| **n == 1).count()
    };
    let drawn: BTreeSet<u32> = layer.triangles.iter().map(|t| t.source.face).collect();
    let canonical_boundary = boundary(
        &mut canonical
            .iter()
            .enumerate()
            .filter(|(i, _)| drawn.contains(&(*i as u32)))
            .map(|(_, t)| t.v),
    );
    let render_boundary = boundary(&mut layer.triangles_mm().into_iter());
    assert_eq!(
        render_boundary,
        canonical_boundary * usize::from(layer.subdivision)
    );
}

/// The baker's `collision_inside_render` rule, exactly: every corner of every drawn canonical face is a render vertex
/// (the subdivision grid's corners are the face's own corners), on flat and rolling ground, for every fixture map.
#[test]
fn every_canonical_corner_is_a_render_vertex() {
    for (terrain, ground) in [
        (canyon_terrain(), canyon_ground()),
        (rolling_terrain(), rolling_ground()),
    ] {
        for map in MAPS {
            let out = cook_map(map, "", &ground, &terrain);
            let canonical = canonical_layer0(&out.file.canonical);
            let pres = presentation(&out);
            let layer = &pres.layers[0];
            let vertices: BTreeSet<[i32; 3]> = layer.vertices.iter().copied().collect();
            let drawn: BTreeSet<u32> = layer.triangles.iter().map(|t| t.source.face).collect();
            let mut corners = 0usize;
            for face in &drawn {
                for p in canonical[*face as usize].v {
                    assert!(
                        vertices.contains(&p),
                        "{map}: canonical corner {p:?} is not drawn"
                    );
                    corners += 1;
                }
            }
            assert!(corners > 1000, "{map}: {corners} corners");
        }
    }
}

/// Cross-host pin of the render bytes: the presentation section of each fixture map cooked on flat and rolling
/// ground hashes (BLAKE3) to the value recorded on x86-64 Linux and aarch64 macOS. The render cook is integer
/// arithmetic on the canonical millimetres, so any host gives these bytes; a mismatch names the map that moved.
/// Re-pinned by lane W-08 (ADR-8210): the two Switchbacks values moved because its skylight shafts (`west`,
/// `mid_east`) are now cooked as open cuts (K8); the new values are identical on x86-64 Linux and aarch64 macOS.
#[test]
fn render_section_bytes_are_pinned_across_hosts() {
    const PINS: [(&str, &str, &str); 6] = [
        (
            "canyon",
            "switchbacks",
            "01e5e48235165f8ea1536f92da4ad6a897e136f87ac911d3690baf9195bfb528",
        ),
        (
            "canyon",
            "atacama_rail",
            "fac9823ef06145bdd1e82cf4516729137242bdeed4efa286c91b31b07fb63ee0",
        ),
        (
            "canyon",
            "cinder_steps",
            "60bdc9a1e790dcb9f28e36500d246541f748a7dec2c9d8d6a486870eadb3fd42",
        ),
        (
            "rolling",
            "switchbacks",
            "9ce7cdd64e6cb146b770aa2c80574b8bdb6ba1b5484cd91472b3eaa33f3637f6",
        ),
        (
            "rolling",
            "atacama_rail",
            "a11771a09317eda5bda006f6cdb93ec40a009c97dd6c612226c9c8a434ff934f",
        ),
        (
            "rolling",
            "cinder_steps",
            "d05717ffce77a2f325bfcbb42d760ec83272add3d378d2945ceacdd01d58de66",
        ),
    ];
    let mut got = Vec::new();
    for (name, terrain, ground) in [
        ("canyon", canyon_terrain(), canyon_ground()),
        ("rolling", rolling_terrain(), rolling_ground()),
    ] {
        for map in MAPS {
            let out = cook_map(map, "", &ground, &terrain);
            got.push((
                name,
                map,
                blake3::hash(&out.file.presentation).to_hex().to_string(),
            ));
        }
    }
    for ((terrain, map, want), (_, _, have)) in PINS.iter().zip(&got) {
        assert_eq!(
            have, want,
            "{terrain}/{map}: the render bytes moved (all: {got:?})"
        );
    }
}
