// SPDX-License-Identifier: MPL-2.0
//! Plan row W-17 (spec 26 § 2a.6 "Pick", § 2a.8; ADR-8537): the mesh layers join the terrain's
//! other consumers.
//!
//! - `a_pick_ray_into_the_mouth_hits_the_layer`: rays from an approach view over every mouth of the
//!   cooked Switchbacks fixture. A ray that meets the ground inside the lattice cut hits the band
//!   (reported as terrain, layer 0) or the tube (its own pick layer), never the cut ground; on the
//!   tube's layer alone it reaches the tube at the triangle oracle's distance. A ray that meets
//!   ground the lattice keeps hits the terrain there, even inside the coverage shape. The agent's
//!   `pick_terrain` (`terrain.raycast`) agrees with the world pick on the terrain layer.
//! - `a_terrain_without_layers_picks_as_before`: without `TerrainMeshLayers` the world pick's
//!   terrain answer is `raycast_with_holes` on the mask, bit for bit.
//! - `collider_triangles_are_the_canonical_set`: the collider pieces hold exactly
//!   `WorldGeom::triangles` (non-blocker), by bits, each piece's triangles centred in its chunk.
//!
//! Fixture: DO27's Switchbacks layout (`regia_world_geom`'s `fixtures` feature) on flat synthetic
//! ground at -256 m, 1025 samples over 1280 m; no DO27 map data, no goldens, no timing.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use regia_core::glam::{Affine3A, Vec3};
use regia_core::math::Ray;
use regia_ecs::prelude::*;
use regia_pick::{MapLayer, PickFilter, PickSurface, world_pick};
use regia_render::{CpuMaterial, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_terrain::{
    BandMesh, BandVertex, CdlodConfig, HoleShape, HolesMask, LatticeHoleCut, LayerChunkGrid, LayerMouth,
    MinMaxPyramid, NamedHole, Terrain, TerrainLayers, TerrainMap, TerrainMeshLayers, TerrainMeshLayersCooked,
    collar_triangles, pick_terrain, raycast_with_holes, root_level_for,
};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook};
use regia_world_geom::cooked::TriKind;
use regia_world_geom::geom::WorldGeom;
use regia_world_geom::ground::vertex_m;
use regia_world_geom::passages::PassagesDocument;

const N: u32 = 1025;
const WIDTH_M: f32 = 1280.0;
/// The cooked ground: counts 8192 at 1/32 m from -512 m.
const GROUND_Y: f32 = -256.0;
/// The tube's pick layer.
const TUBE_LAYER: u8 = 1;

fn cook_switchbacks() -> CookOutput {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), WIDTH_M).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: N,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; (N * N) as usize],
    };
    let layout = for_map("switchbacks").unwrap();
    let mouths = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&terrain)));
    let fills: Vec<String> = mouths.iter().map(|m| format!("{:?}", m.id)).collect();
    let doc = format!(
        "(version: 1, ground_tile_cells: 64, layers: [(id: \"tubes\", source: Passages(path: \"p\"), fills: [{}])])",
        fills.join(", ")
    );
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let read = |_: &str| Ok(passages.clone().into_bytes());
    let run = |doc: &str| {
        cook(&CookInputs {
            document: doc.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // The Switchbacks ramp facets over the slope limit are a known Stage B red (W-11): the line's
    // test cooks waive exactly those findings and nothing else (`waive_ramp_facets`).
    match run(&doc) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&doc, &report).unwrap()).unwrap()
        }
        other => other.unwrap(),
    }
}

/// A synthetic band: each ring edge drops 1.5 m while it moves a fifth of the way to the ring's
/// centroid (a throat lip). LOD-0 positions equal the drawn ones.
fn band_for(ring: &[[f32; 3]]) -> BandMesh {
    let n = ring.len() as f32;
    let c = ring
        .iter()
        .fold([0.0f32; 2], |a, p| [a[0] + p[0] / n, a[1] + p[2] / n]);
    let mut band = BandMesh::default();
    for p in ring {
        let inner = [p[0] + (c[0] - p[0]) * 0.2, p[1] - 1.5, p[2] + (c[1] - p[2]) * 0.2];
        for q in [*p, inner] {
            band.vertices.push(BandVertex {
                position: q,
                lod0: q,
                ring: q == *p,
            });
        }
    }
    let m = ring.len() as u32;
    for i in 0..m {
        let j = (i + 1) % m;
        let (a, b, ai, bi) = (2 * i, 2 * j, 2 * i + 1, 2 * j + 1);
        band.triangles.push([a, b, bi]);
        band.triangles.push([a, bi, ai]);
    }
    band
}

struct Fixture {
    geom: Arc<WorldGeom>,
    map: TerrainMap,
    mask: HolesMask,
    layers: TerrainMeshLayers,
    /// Mouth id, ring XZ box `[min, max]`.
    boxes: Vec<(String, [f32; 2], [f32; 2])>,
}

fn fixture() -> Fixture {
    let out = cook_switchbacks();
    let canonical = regia_world_geom::cooked::encode_canonical(&out.file.canonical).unwrap();
    let geom = Arc::new(WorldGeom::from_canonical_bytes(&canonical).unwrap());
    let l = out.file.canonical.lattice;
    let map = TerrainMap::from_heights(N, N, vec![GROUND_Y; (N * N) as usize], WIDTH_M, 1.0, 0.0).unwrap();
    let mut mask = HolesMask::default();
    let mut layers = TerrainMeshLayers::with_geom(geom.clone());
    let mut boxes = Vec::new();
    assert!(!out.report.ground.mouths.is_empty());
    for mouth in &out.report.ground.mouths {
        let loops: Vec<Vec<[f32; 2]>> = mouth
            .rings
            .iter()
            .map(|ring| {
                ring.iter()
                    .map(|&(i, j)| [vertex_m(&l, i), vertex_m(&l, j)])
                    .collect()
            })
            .collect();
        let shape = HoleShape::lattice_rings(&loops).unwrap();
        mask.set_named(NamedHole {
            id: mouth.id.clone(),
            open: true,
            shape: shape.clone(),
        })
        .unwrap();
        let rings: Vec<Vec<[f32; 3]>> = loops
            .iter()
            .map(|r| r.iter().map(|p| [p[0], GROUND_Y, p[1]]).collect())
            .collect();
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for p in loops.iter().flatten() {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        boxes.push((mouth.id.clone(), lo, hi));
        layers.push_mouth(LayerMouth {
            hole: mouth.id.clone(),
            band: band_for(&rings[0]),
            rings,
            collar: collar_triangles(&map, &shape),
            throat: Vec::new(),
            body: Vec::new(),
            open: true,
        });
    }
    Fixture {
        geom,
        map,
        mask,
        layers,
        boxes,
    }
}

/// The terrain entity (with `layers` when given) and the tube's canonical triangles as one mesh
/// entity on [`TUBE_LAYER`], as a chunk entity of the layer would be.
fn world(f: &Fixture, with_layers: bool) -> (World, Entity) {
    let mut world = World::new();
    let cdlod = CdlodConfig::default();
    let root_level = root_level_for(N, N, &cdlod);
    let terrain = Terrain {
        minmax: MinMaxPyramid::build(&f.map, root_level).unwrap(),
        map: f.map.clone(),
        cdlod,
        root_level,
        layers: TerrainLayers::default(),
        holes: Some(f.mask.clone()),
        water: None,
    };
    let e = world.spawn(terrain).id();
    if with_layers {
        world.entity_mut(e).insert(f.layers.clone());
    }
    let mut vertices = Vec::new();
    for t in f.geom.triangles() {
        if t.kind == TriKind::Blocker {
            continue;
        }
        for p in t.v {
            vertices.push(LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]));
        }
    }
    let indices = (0..vertices.len() as u32).collect();
    world.spawn((
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial::default()),
        ),
        GlobalTransform(Affine3A::IDENTITY),
        MapLayer(TUBE_LAYER),
    ));
    (world, e)
}

/// Two-sided ray/triangle distance, `d` unit: the oracle. It is `regia_pick::ray_triangle`
/// (Möller and Trumbore 1997) brute-forced over every triangle, so it follows the pick's documented
/// edge rule (edges count as inside, a barycentric slack of `1e-6`): a ground point on a lattice
/// line (x = 20 m is 16 cells of 1.25 m) lies on the edge of a ground-level canonical triangle, and
/// a strict test there disagrees with the pick by float rounding alone (ADR-8872).
fn tri_hit(o: Vec3, d: Vec3, t: &[[f32; 3]; 3]) -> Option<f32> {
    let (a, b, c) = (Vec3::from(t[0]), Vec3::from(t[1]), Vec3::from(t[2]));
    regia_pick::ray_triangle(o, d, a, b, c, regia_pick::FaceCull::None).map(|h| h.t)
}

fn tube_hit(geom: &WorldGeom, o: Vec3, d: Vec3) -> Option<f32> {
    geom.triangles()
        .iter()
        .filter(|t| t.kind != TriKind::Blocker)
        .filter_map(|t| tri_hit(o, d, &t.v))
        .min_by(f32::total_cmp)
}

/// Every ground point of a mouth's box grown by 3 m, on a 0.45 m grid.
fn ground_points(f: &Fixture) -> Vec<Vec3> {
    let mut out = Vec::new();
    for (_, lo, hi) in &f.boxes {
        let mut x = lo[0] - 3.0;
        while x <= hi[0] + 3.0 {
            let mut z = lo[1] - 3.0;
            while z <= hi[1] + 3.0 {
                out.push(Vec3::new(x, GROUND_Y, z));
                z += 0.45;
            }
            x += 0.45;
        }
    }
    out
}

/// Plan W-17 item 1 (spec 26 § 2a.6 "Pick"): rays into the mouth hit the band or the tube, never
/// the cut ground; rays at kept ground hit the terrain.
#[test]
fn a_pick_ray_into_the_mouth_hits_the_layer() {
    let f = fixture();
    let lattice: LatticeHoleCut = f
        .layers
        .lattice_hole_cut(&f.map)
        .expect("the mouths cut the lattice");
    let (world, terrain) = world(&f, true);
    // Two approaches: 8.5 m back along +x+z (or -x-z) and 7 m up from each ground point.
    let (mut into, mut through, mut band_hits, mut kept, mut kept_in_shape, mut agent_checked) =
        (0, 0, 0, 0, 0, 0);
    let near = [[0.3f32, 0.0], [-0.3, 0.0], [0.0, 0.3], [0.0, -0.3], [0.0, 0.0]];
    let points = ground_points(&f);
    let rays = [Vec3::new(6.0, 7.0, 6.0), Vec3::new(-6.0, 7.0, -6.0)]
        .into_iter()
        .flat_map(|back| points.iter().map(move |g| (*g, back)));
    for (g, back) in rays {
        let o = g + back;
        let d = (g - o).normalize();
        let ray = Ray::new(o, d);
        let to_ground = (g - o).length();
        let band = f.layers.raycast_bands(o.to_array(), d.to_array(), f32::INFINITY);
        let tube = tube_hit(&f.geom, o, d);
        let pick = world_pick(&world, ray, &PickFilter::default());
        let deep_in = near.iter().all(|e| lattice.cuts(g.x + e[0], g.z + e[1]));
        let clear_out = near
            .iter()
            .all(|e| !f.mask.contains_drawn(g.x + e[0], g.z + e[1], Some(&lattice)));
        // The layer's own triangles at or before the ground point (band, tube, or a canonical
        // triangle lying on the ground at the ring) decide the answer: not a kept-ground ray.
        let occluded = band.as_ref().is_some_and(|b| b.distance < to_ground + 0.05)
            || tube.is_some_and(|t| t < to_ground + 0.05);
        // Into the cut the answer is exactly the band/tube oracle's, including none: a ray that
        // enters near the lip heading away from the tube passes under the kept ground, which is a
        // surface with nothing below it.
        let nearest = band
            .as_ref()
            .map(|b| b.distance)
            .into_iter()
            .chain(tube)
            .min_by(f32::total_cmp);
        if deep_in && nearest.is_none() {
            assert!(
                pick.is_none(),
                "into the mouth at {g} (from {back}): the oracle sees nothing, the pick hit {pick:?}"
            );
            through += 1;
        } else if deep_in {
            let pick = pick.unwrap_or_else(|| panic!("a ray into the mouth at {g} hit nothing"));
            let nearest = nearest.unwrap_or(f32::NAN);
            let on_cut_ground = pick.surface == PickSurface::Terrain
                && (pick.point.y - GROUND_Y).abs() < 1e-3
                && lattice.cuts(pick.point.x, pick.point.z);
            assert!(
                !on_cut_ground,
                "into the mouth at {g}: the cut ground was hit: {pick:?}"
            );
            assert!(
                (pick.distance - nearest).abs() < 0.01,
                "into the mouth at {g}: pick {:.3} m, band/tube oracle {nearest:.3} m ({pick:?})",
                pick.distance
            );
            if band.as_ref().is_some_and(|b| b.distance <= nearest) {
                assert_eq!(pick.surface, PickSurface::Terrain, "the band is terrain at {g}");
                assert_eq!(pick.layer, MapLayer(0));
                assert_eq!(pick.entity, terrain);
                band_hits += 1;
            } else {
                assert_eq!(pick.surface, PickSurface::Mesh, "the tube at {g}: {pick:?}");
                assert_eq!(pick.layer, MapLayer(TUBE_LAYER));
            }
            // On the tube's layer alone the ray falls through the cut to the tube.
            if let Some(t) = tube {
                let only = world_pick(&world, ray, &PickFilter::on_layers(&[TUBE_LAYER]))
                    .unwrap_or_else(|| panic!("the tube layer at {g} hit nothing"));
                assert_eq!(only.surface, PickSurface::Mesh);
                assert!(
                    (only.distance - t).abs() < 0.01,
                    "tube {:.3} vs {t:.3}",
                    only.distance
                );
            }
            into += 1;
        } else if clear_out && !occluded {
            let pick = pick.unwrap_or_else(|| panic!("a ray at kept ground {g} hit nothing"));
            assert_eq!(pick.surface, PickSurface::Terrain, "kept ground at {g}: {pick:?}");
            assert_eq!(pick.layer, MapLayer(0));
            assert!(
                (pick.distance - to_ground).abs() < 0.01,
                "kept ground at {g}: {:.3} m, expected {to_ground:.3} m",
                pick.distance
            );
            kept += 1;
            if f.mask.contains(g.x, g.z) {
                kept_in_shape += 1;
            }
        }
        // The agent's `terrain.raycast` path agrees with the world pick on the terrain layer.
        if (deep_in || clear_out) && agent_checked < 400 {
            let a = pick_terrain(&world, o, d, 1.0e4).map(|h| h.distance);
            let w = world_pick(&world, ray, &PickFilter::on_layers(&[0])).map(|p| p.distance);
            match (a, w) {
                (Some(a), Some(w)) => assert!((a - w).abs() < 0.01, "pick_terrain {a} vs world {w} at {g}"),
                (a, w) => assert_eq!(
                    a.is_some(),
                    w.is_some(),
                    "pick_terrain {a:?} vs world {w:?} at {g}"
                ),
            }
            agent_checked += 1;
        }
    }
    println!(
        "{} mouths: {into} rays into the cut hit ({band_hits} on the band, {through} with no band or tube on the ray), {kept} on kept ground ({kept_in_shape} inside the coverage shape)",
        f.boxes.len()
    );
    assert!(
        into > 50 && band_hits > 0 && kept > 50,
        "the rays straddle the mouths"
    );
}

/// Plan W-17 (spec 26 § 2a "defaults stay"): a terrain without `TerrainMeshLayers` picks as on the
/// base: its world-pick terrain hit is `raycast_with_holes` on the mask, bit for bit.
#[test]
fn a_terrain_without_layers_picks_as_before() {
    let f = fixture();
    let (world, _) = world(&f, false);
    let back = Vec3::new(6.0, 7.0, 6.0);
    let mut hits = 0;
    for g in ground_points(&f).into_iter().step_by(7) {
        let o = g + back;
        let d = (g - o).normalize();
        let base =
            raycast_with_holes(&f.map, Some(&f.mask), o, d, 1.0e4).map(|p| p.to_array().map(f32::to_bits));
        let agent = pick_terrain(&world, o, d, 1.0e4).map(|h| h.position.to_array().map(f32::to_bits));
        assert_eq!(agent, base, "pick_terrain at {g}");
        if world_pick(&world, Ray::new(o, d), &PickFilter::on_layers(&[0])).is_some() {
            hits += 1;
        }
    }
    assert!(hits > 0);
}

/// Plan W-17 item 5, amendment A1 (spec 26 § 2a.8): the collider pieces are the canonical
/// triangles (`WorldGeom::triangles`, i32 mm read through `mm_to_m`), counts and positions equal,
/// per chunk; never the render LODs or the band.
#[test]
fn collider_triangles_are_the_canonical_set() {
    let f = fixture();
    let canon: Vec<[[u32; 3]; 3]> = f
        .geom
        .triangles()
        .iter()
        .filter(|t| t.kind != TriKind::Blocker)
        .map(|t| t.v.map(|p| p.map(f32::to_bits)))
        .collect();
    assert!(!canon.is_empty());
    // Without a presentation section: one piece per layer, the whole canonical set in order.
    let whole = f.layers.collider_pieces();
    assert!(whole.iter().all(|p| p.chunk.is_none() && p.rect.is_none()));
    let flat: Vec<[[u32; 3]; 3]> = whole
        .iter()
        .flat_map(|p| p.triangles.iter().map(|t| t.map(|v| v.map(f32::to_bits))))
        .collect();
    assert_eq!(
        flat, canon,
        "one piece per layer holds the canonical set in order"
    );
    // With chunks: each piece is the canonical triangles whose XZ centroid is in its chunk.
    let grid = LayerChunkGrid::new(
        WIDTH_M,
        root_level_for(N, N, &CdlodConfig::default()),
        16,
        4.0,
        [0.0, 0.0],
        64.0,
    );
    let layers = f.layers.clone().with_presentation(
        TerrainMeshLayersCooked {
            version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
            grid,
            layers: Vec::new(),
            chunk_importer: 0,
        },
        None,
    );
    let pieces = layers.collider_pieces();
    assert!(
        pieces.len() > 1,
        "the tubes span several chunks ({})",
        pieces.len()
    );
    let mut total = 0usize;
    for piece in &pieces {
        let (x, z) = piece.chunk.expect("a chunk");
        let rect = piece.rect.expect("a rect");
        let (mx, mz, size) = grid.chunk_rect(x, z);
        assert_eq!(rect, [mx, mz, size]);
        let expect: Vec<[[u32; 3]; 3]> = f
            .geom
            .triangles()
            .iter()
            .filter(|t| t.kind != TriKind::Blocker && t.tri.layer == piece.layer)
            .filter(|t| {
                let c = [
                    (t.v[0][0] + t.v[1][0] + t.v[2][0]) / 3.0,
                    (t.v[0][2] + t.v[1][2] + t.v[2][2]) / 3.0,
                ];
                grid.chunk_at(c) == Some((x, z))
            })
            .map(|t| t.v.map(|p| p.map(f32::to_bits)))
            .collect();
        let got: Vec<[[u32; 3]; 3]> = piece
            .triangles
            .iter()
            .map(|t| t.map(|v| v.map(f32::to_bits)))
            .collect();
        assert_eq!(got, expect, "chunk ({x}, {z}) of layer {}", piece.layer);
        total += got.len();
    }
    assert_eq!(
        total,
        canon.len(),
        "every canonical triangle is in exactly one piece"
    );
    // The band and the collar never reach the colliders.
    let band: Vec<[[u32; 3]; 3]> = f
        .layers
        .mouths()
        .iter()
        .flat_map(|m| m.band_lod0())
        .map(|t| t.map(|v| v.map(f32::to_bits)))
        .collect();
    assert!(!band.is_empty());
    assert!(band.iter().all(|t| !canon.contains(t)));
}

fn empty_export() -> regia_render::FrameExport {
    regia_render::FrameExport {
        frame_index: 0,
        eye: [0.0; 3],
        target: [0.0, 0.0, -1.0],
        fov_y: 1.0,
        width: 8,
        height: 8,
        sun: bytemuck::Zeroable::zeroed(),
        fill_light: None,
        extra_directional: Vec::new(),
        ambient_lux: 0.0,
        ibl: false,
        outdoor: regia_render::OutdoorParams::default(),
        local_lights: Vec::new(),
        instances: Vec::new(),
        meshes: std::collections::HashMap::new(),
        materials: std::collections::HashMap::new(),
        meshes_copied: 0,
    }
}

/// Plan W-17 item 2 (spec 26 § 2a.6 "Path tracer"): the reference export of a map with one layer
/// holds every open mouth's band (LOD 0) and collar exactly once, as mesh instances in the
/// export's local frame; a ray-cast oracle through the exported pieces meets the band where the
/// band pick does. A closed mouth adds nothing, and so does a terrain without layers.
#[test]
fn reference_export_holds_layers() {
    let f = fixture();
    let mut export = empty_export();
    let offset = [10.0, 0.0, -20.0];
    let added = f.layers.add_reference_pieces(&mut export, offset);
    let pieces: usize = f
        .layers
        .mouths()
        .iter()
        .map(|m| usize::from(!m.band_lod0().is_empty()) + usize::from(!m.collar.is_empty()))
        .sum();
    assert!(f.layers.mouths().iter().all(|m| !m.band_lod0().is_empty()));
    assert!(
        f.layers.mouths().iter().any(|m| !m.collar.is_empty()),
        "a mouth has a collar"
    );
    assert_eq!(
        added, pieces,
        "a band and (when it has one) a collar per open mouth"
    );
    assert_eq!(export.instances.len(), added);
    let keys: std::collections::BTreeSet<u64> = export.instances.iter().map(|i| i.mesh_key).collect();
    assert_eq!(keys.len(), added, "each piece exactly once");
    for (i, mouth) in f.layers.mouths().iter().enumerate() {
        for (collar, want) in [(false, mouth.band_lod0()), (true, mouth.collar.clone())] {
            if want.is_empty() {
                continue;
            }
            let key = regia_terrain::REFERENCE_PIECE_TAG | ((i as u64) << 1) | u64::from(collar);
            let mesh = export.meshes.get(&key).expect("the piece's mesh");
            let got: Vec<[[u32; 3]; 3]> = mesh
                .vertices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| t.each_ref().map(|v| v.position.map(f32::to_bits)))
                .collect();
            let want: Vec<[[u32; 3]; 3]> = want.iter().map(|t| t.map(|v| v.map(f32::to_bits))).collect();
            assert_eq!(got, want, "mouth {} collar {collar}", mouth.hole);
        }
    }
    let inst = &export.instances[0];
    assert_eq!(
        inst.model[3],
        [-10.0, 0.0, 20.0, 1.0],
        "local frame, like the heightfield"
    );
    // Oracle: a ray straight down onto a band triangle's centroid meets the exported band where
    // the band pick does.
    let band = f.layers.mouths()[0].band_lod0();
    let c = band[0].iter().fold(Vec3::ZERO, |a, p| a + Vec3::from(*p) / 3.0);
    let o = c + Vec3::new(0.0, 5.0, 0.0);
    let d = Vec3::NEG_Y;
    let pick = f
        .layers
        .raycast_bands(o.to_array(), d.to_array(), 100.0)
        .expect("band pick");
    let key = regia_terrain::REFERENCE_PIECE_TAG;
    let exported = export.meshes.get(&key).expect("band mesh");
    let tris: Vec<[[f32; 3]; 3]> = exported
        .vertices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| t.each_ref().map(|v| v.position))
        .collect();
    let t = tris
        .iter()
        .filter_map(|t| tri_hit(o, d, t))
        .min_by(f32::total_cmp)
        .expect("the exported band");
    assert!(
        (t - pick.distance).abs() < 1e-4,
        "export {t} vs pick {}",
        pick.distance
    );
    // Closing every mouth adds nothing; no layers adds nothing.
    let mut closed = f.layers.clone();
    for m in f.layers.mouths() {
        closed.set_open(&m.hole, false);
    }
    let mut export = empty_export();
    assert_eq!(closed.add_reference_pieces(&mut export, [0.0; 3]), 0);
    assert!(export.instances.is_empty() && export.meshes.is_empty() && export.materials.is_empty());
    assert_eq!(
        TerrainMeshLayers::new().add_reference_pieces(&mut export, [0.0; 3]),
        0
    );
}

/// A presentation section on `grid` with one layer per document layer of the fixture, `collider`
/// as given (no chunks: the pieces follow the grid, not the chunk list).
fn presentation_with(grid: LayerChunkGrid, colliders: &[bool]) -> TerrainMeshLayersCooked {
    TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        grid,
        layers: colliders
            .iter()
            .enumerate()
            .map(|(i, &collider)| regia_terrain::CookedLayer {
                id: format!("layer{i}"),
                map_layer: TUBE_LAYER,
                cast_shadows: true,
                collider,
                material: String::new(),
                material_id: None,
                chunks: Vec::new(),
            })
            .collect(),
        chunk_importer: 0,
    }
}

/// ADR-8872 (ADR-8537 deferred item 5b, spec 26 § 2a.8 "`collider: true` builds pieces"): the
/// cooked `collider` flag selects the layers whose canonical pieces a host builds:
/// `cooked_collider_pieces` equals `collider_pieces` when the fixture's one layer sets it, is empty
/// when it does not, and is empty without a presentation section (the flag is not known there).
#[test]
fn the_cooked_collider_flag_selects_the_layers() {
    let f = fixture();
    let grid = LayerChunkGrid::new(
        WIDTH_M,
        root_level_for(N, N, &CdlodConfig::default()),
        16,
        4.0,
        [0.0, 0.0],
        64.0,
    );
    assert!(
        f.layers.cooked_collider_pieces().is_empty(),
        "no presentation: no flag"
    );
    let on = f
        .layers
        .clone()
        .with_presentation(presentation_with(grid, &[true]), None);
    let all = on.collider_pieces();
    assert!(!all.is_empty());
    assert_eq!(on.cooked_collider_pieces(), all, "the flagged layer's pieces");
    let off = f
        .layers
        .clone()
        .with_presentation(presentation_with(grid, &[false]), None);
    assert!(off.cooked_collider_pieces().is_empty(), "collider false: none");
    assert_eq!(off.collider_pieces(), all, "collider_pieces ignores the flag");
}

/// ADR-8872 (format batch 2, `terrain-mesh-layers-cooked` v1 -> v2): a v1 file (the v0.6.0 cook:
/// header version 1, chunk section version 2 without `collider`) upgrades to the current file
/// version with a version-3 chunk section whose layers say `collider: false` and are otherwise
/// equal; the canonical section takes its version 2 layout (ADR-8816) with the same lockstep
/// `content_hash`; the presentation section stays byte for byte; a second upgrade changes nothing;
/// and the run time loads the v1 file as it is.
#[test]
fn a_v1_cooked_file_upgrades_keeping_the_content_hash() {
    #[derive(serde::Serialize)]
    struct LayerV2 {
        id: String,
        map_layer: u8,
        cast_shadows: bool,
        material: String,
        material_id: Option<regia_asset::AssetId>,
        chunks: Vec<regia_terrain::CookedLayerChunk>,
    }
    #[derive(serde::Serialize)]
    struct SectionV2 {
        version: u32,
        grid: LayerChunkGrid,
        layers: Vec<LayerV2>,
        chunk_importer: u32,
    }
    let grid = LayerChunkGrid::new(
        WIDTH_M,
        root_level_for(N, N, &CdlodConfig::default()),
        16,
        4.0,
        [0.0, 0.0],
        64.0,
    );
    let chunk = regia_terrain::CookedLayerChunk {
        x: 3,
        z: 5,
        bounds: [[-1.0; 3], [1.0; 3]],
        lods: vec![regia_terrain::CookedChunkLod {
            mesh: regia_asset::AssetId::sub_asset(
                regia_asset::AssetId::default(),
                "meshlayers/tubes/3_5/lod0",
            ),
            triangles: 2,
            bytes: 64,
            error_m: 0.0,
        }],
    };
    let v2 = SectionV2 {
        version: 2,
        grid,
        layers: vec![LayerV2 {
            id: "tubes".into(),
            map_layer: TUBE_LAYER,
            cast_shadows: false,
            material: String::new(),
            material_id: None,
            chunks: vec![chunk.clone()],
        }],
        chunk_importer: 9,
    };
    // The v0.6.0 file as stored: header version 1, canonical section version 1 (one postcard
    // value, ADR-8816's legacy layout), the presentation section, the version-2 chunk section.
    let file = cook_switchbacks().file;
    let mut header = file.header.clone();
    header.version = 1;
    let mut canonical_v1 = file.canonical.clone();
    canonical_v1.canonical_version = 1;
    let mut v1 = regia_world_geom::cooked::COOKED_MAGIC.to_vec();
    v1.extend_from_slice(&postcard::to_allocvec(&header).unwrap());
    v1.extend_from_slice(&postcard::to_allocvec(&postcard::to_allocvec(&canonical_v1).unwrap()).unwrap());
    v1.extend_from_slice(&postcard::to_allocvec(&file.presentation).unwrap());
    v1.extend_from_slice(&postcard::to_allocvec(&postcard::to_allocvec(&v2).unwrap()).unwrap());
    assert_eq!(regia_world_geom::cooked::cooked_file_version(&v1), Ok(Some(1)));

    let up = regia_terrain::upgrade_cooked_mesh_layers(&v1).unwrap();
    let parts = regia_world_geom::cooked::split_file(&up).unwrap();
    let old = regia_world_geom::cooked::split_file(&v1).unwrap();
    assert_eq!(
        parts.header.input_hash, [0; 32],
        "migration requires a source recook"
    );
    assert_ne!(parts.header.input_hash, old.header.input_hash);
    assert_eq!(
        parts.header.version,
        regia_world_geom::cooked::COOKED_FORMAT_VERSION
    );
    // The canonical section moves to its version 2 layout (ADR-8816) with the same value, so the
    // lockstep `content_hash` stays.
    assert_eq!(
        parts.canonical,
        regia_world_geom::cooked::migrate_canonical(&old.canonical).unwrap(),
        "the canonical section is the migrated v1 section"
    );
    assert_eq!(
        regia_world_geom::WorldGeom::from_canonical_bytes(&parts.canonical)
            .unwrap()
            .content_hash(),
        regia_world_geom::WorldGeom::from_canonical_bytes(&old.canonical)
            .unwrap()
            .content_hash(),
        "the canonical content_hash stays"
    );
    assert_eq!(
        parts.presentation, old.presentation,
        "the presentation section stays"
    );
    assert_eq!(
        parts.chunks.first(),
        Some(&(regia_terrain::mesh_layer::PRESENTATION_VERSION as u8)),
        "the chunk section is written at the current version"
    );
    let chunks = TerrainMeshLayersCooked::decode(&parts.chunks).unwrap().unwrap();
    assert_eq!(
        chunks,
        TerrainMeshLayersCooked {
            version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
            grid,
            layers: vec![regia_terrain::CookedLayer {
                id: "tubes".into(),
                map_layer: TUBE_LAYER,
                cast_shadows: false,
                collider: false,
                material: String::new(),
                material_id: None,
                chunks: vec![chunk],
            }],
            chunk_importer: 9,
        }
    );
    assert_eq!(
        regia_terrain::upgrade_cooked_mesh_layers(&up).unwrap(),
        up,
        "a second upgrade changes nothing"
    );
    // The run time reads the v1 file without the upgrade: its chunk section migrates on load.
    let loaded = TerrainMeshLayersCooked::decode(&old.chunks).unwrap().unwrap();
    assert_eq!(loaded, chunks);
}
