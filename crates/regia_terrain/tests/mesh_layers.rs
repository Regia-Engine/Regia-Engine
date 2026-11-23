// SPDX-License-Identifier: MPL-2.0
//! Seam cook acceptance (ADR-8291, spec 26 § 2a). CPU only: the band is not drawn here.

use std::collections::{BTreeMap, BTreeSet};

use regia_terrain::{
    CdlodConfig, Crater, HeightMips, HoleEllipse, HoleShape, LayerMouth, LodSelection, MinMaxPyramid,
    SeamCookError, SurfaceOwner, Terrain, TerrainLayers, TerrainMap, TerrainMeshLayers, ZipperInput,
    ZipperParams, apply_crater, attach_ring_detail, cascade_min_level, collar_triangles,
    eval_node_vertex_filtered, heightfield_triangles, node_world_size, quadtree_span, seam_report_with_mips,
    select_nodes, select_nodes_min_level, surface_owner, vertex_morph_t, zipper,
};

fn flat_map(n: u32, width: f32, hill: bool) -> TerrainMap {
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let u = col as f32 / (n - 1) as f32;
            let v = row as f32 / (n - 1) as f32;
            let x = (u - 0.5) * width;
            let z = (v - 0.5) * width;
            let h = if hill {
                3.0 * (x * 0.05).sin() * (z * 0.07).cos() + 1.5 * ((x + z) * 0.04).sin()
            } else {
                0.0
            };
            heights.push(h);
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("map")
}

fn terrain_from(map: TerrainMap, root: u32) -> Terrain {
    let minmax = MinMaxPyramid::build(&map, root).expect("pyramid");
    Terrain {
        map,
        cdlod: CdlodConfig::default(),
        root_level: root,
        minmax,
        layers: TerrainLayers::default(),
        holes: None,
        water: None,
    }
}

fn bits(p: [f32; 3]) -> [u32; 3] {
    p.map(f32::to_bits)
}

fn point_in_ring(ring: &[[f32; 3]], x: f32, z: f32) -> bool {
    let mut inside = false;
    let n = ring.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, zi) = (ring[i][0], ring[i][2]);
        let (xj, zj) = (ring[j][0], ring[j][2]);
        if (zi > z) != (zj > z) && x < (xj - xi) * (z - zi) / (zj - zi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Boundary rings of the centroid cut, largest first.
fn rings_of(map: &TerrainMap, shape: &HoleShape) -> Vec<Vec<[f32; 3]>> {
    let (w, h) = (map.width(), map.height());
    let mut cut = BTreeSet::<(u32, u32, bool)>::new();
    for row in 0..h.saturating_sub(1) {
        for col in 0..w.saturating_sub(1) {
            for upper in [false, true] {
                if centroid_inside(map, shape, col, row, upper) {
                    cut.insert((col, row, upper));
                }
            }
        }
    }
    let verts = |col: u32, row: u32, upper: bool| -> [(u32, u32); 3] {
        if upper {
            [(col + 1, row), (col, row + 1), (col + 1, row + 1)]
        } else {
            [(col, row), (col, row + 1), (col + 1, row)]
        }
    };
    let mut next: BTreeMap<(u32, u32), (u32, u32)> = BTreeMap::new();
    for &(col, row, upper) in &cut {
        let v = verts(col, row, upper);
        for e in 0..3 {
            let a = v[e];
            let b = v[(e + 1) % 3];
            let reversed = cut.iter().any(|&(c, r, u)| {
                let w = verts(c, r, u);
                w.iter()
                    .zip(w.iter().cycle().skip(1))
                    .any(|(&p, &q)| p == b && q == a)
            });
            if !reversed {
                next.insert(a, b);
            }
        }
    }
    let mut rings = Vec::new();
    while let Some((&start, _)) = next.iter().next() {
        let mut ring = Vec::new();
        let mut at = start;
        while let Some(to) = next.remove(&at) {
            let [x, z] = map.sample_to_world(at.0, at.1);
            let y = map.height_at_sample(i64::from(at.0), i64::from(at.1));
            ring.push([x, y, z]);
            at = to;
            if at == start {
                break;
            }
        }
        if ring.len() >= 3 {
            rings.push(ring);
        }
    }
    rings.sort_by_key(|r| std::cmp::Reverse(r.len()));
    rings
}

fn centroid_inside(map: &TerrainMap, shape: &HoleShape, col: u32, row: u32, upper: bool) -> bool {
    let corner = |c: u32, r: u32| map.sample_to_world(c, r);
    let pts = if upper {
        [
            corner(col + 1, row),
            corner(col, row + 1),
            corner(col + 1, row + 1),
        ]
    } else {
        [corner(col, row), corner(col, row + 1), corner(col + 1, row)]
    };
    let x = (pts[0][0] + pts[1][0] + pts[2][0]) / 3.0;
    let z = (pts[0][1] + pts[1][1] + pts[2][1]) / 3.0;
    shape.contains(x, z)
}

fn inset(ring: &[[f32; 3]], dist: f32) -> Vec<[f32; 3]> {
    let n = ring.len() as f32;
    let (cx, cz) = ring
        .iter()
        .fold((0.0f32, 0.0f32), |(sx, sz), p| (sx + p[0], sz + p[2]));
    let (cx, cz) = (cx / n, cz / n);
    ring.iter()
        .map(|p| {
            let (dx, dz) = (cx - p[0], cz - p[2]);
            let len = (dx * dx + dz * dz).sqrt().max(1.0e-6);
            [p[0] + dx / len * dist, p[1], p[2] + dz / len * dist]
        })
        .collect()
}

/// Top loop `outer`, a farther inner loop, a throat triangle and a deep triangle.
fn tube_between(outer: &[[f32; 3]], inner: &[[f32; 3]]) -> (Vec<[f32; 3]>, Vec<u32>) {
    let n = outer.len();
    assert_eq!(inner.len(), n);
    let mut positions = Vec::with_capacity(n * 2 + 6);
    positions.extend_from_slice(outer);
    positions.extend_from_slice(inner);
    let throat = [[0.05f32, -1.0, 0.0], [-0.05, -1.0, 0.08], [0.0, -1.0, -0.08]];
    let deep = [[0.05f32, -3.0, 0.0], [-0.05, -3.0, 0.08], [0.0, -3.0, -0.08]];
    positions.extend_from_slice(&throat);
    positions.extend_from_slice(&deep);
    let mut indices = Vec::new();
    let n = n as u32;
    for i in 0..n {
        let t0 = i;
        let t1 = (i + 1) % n;
        let b0 = n + i;
        let b1 = n + (i + 1) % n;
        indices.extend_from_slice(&[t0, t1, b1, t0, b1, b0]);
    }
    let base = n * 2;
    indices.extend_from_slice(&[base, base + 1, base + 2, base + 3, base + 4, base + 5]);
    (positions, indices)
}

fn seat(map: &TerrainMap, loop_pts: &mut [[f32; 3]]) {
    for p in loop_pts {
        p[1] = map.sample_bilinear(p[0], p[2]);
    }
}

fn ngon(radius: f32, y: f32, n: usize) -> Vec<[f32; 3]> {
    (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            [a.cos() * radius, y, a.sin() * radius]
        })
        .collect()
}

fn run_zipper(
    hole: &str,
    ring: &[[f32; 3]],
    positions: &[[f32; 3]],
    indices: &[u32],
    params: ZipperParams,
) -> Result<regia_terrain::ZipperOutput, SeamCookError> {
    let ground = |_x: f32, _z: f32| 0.0;
    let inside = |x: f32, z: f32| point_in_ring(ring, x, z);
    zipper(&ZipperInput {
        hole,
        ring,
        positions,
        indices,
        params,
        ground_y: &ground,
        inside_cut: &inside,
        source_tris: &[],
    })
}

#[test]
fn zipper_never_folds_and_keeps_authored_vertices() {
    let n = 32usize;
    let radius = 8.0f32;
    let ring = ngon(radius, 0.0, n);
    for offset in [0.0f32, 0.4, 0.9] {
        let outer = ngon(radius - offset, 0.0, n);
        let inner = ngon((radius - offset - 2.5).max(0.5), -0.4, n);
        let (positions, indices) = tube_between(&outer, &inner);
        let before: Vec<[u32; 3]> = positions.iter().copied().map(bits).collect();
        let out =
            run_zipper("mouth", &ring, &positions, &indices, ZipperParams::default()).unwrap_or_else(|e| {
                panic!("offset {offset}: {}", e.code());
            });
        let after: Vec<[u32; 3]> = positions.iter().copied().map(bits).collect();
        assert_eq!(before, after, "offset {offset}: authored positions moved");
        for p in &outer {
            assert!(
                out.band.vertices.iter().any(|v| bits(v.position) == bits(*p)),
                "offset {offset}: an authored vertex is missing from the band"
            );
        }
        assert!(
            out.band.triangles.len() > n,
            "offset {offset}: the skirt should add triangles"
        );
    }
    let outer = ngon(radius - 1.2, 0.0, n);
    let inner = ngon(radius - 1.2 - 2.5, -0.4, n);
    let (positions, indices) = tube_between(&outer, &inner);
    let err = run_zipper("mouth", &ring, &positions, &indices, ZipperParams::default()).expect_err("1.2 m");
    assert_eq!(err.code(), "seam_loop_missing");

    let mut outer = ngon(radius - 0.4, 0.0, n);
    outer[0] = [radius + 0.3, 0.02, 0.0];
    let inner = ngon(radius - 0.4 - 2.5, -0.4, n);
    let (positions, indices) = tube_between(&outer, &inner);
    let err = run_zipper("mouth", &ring, &positions, &indices, ZipperParams::default()).expect_err("above");
    assert_eq!(err.code(), "seam_above_ground");
}

#[test]
fn zipper_keeps_the_pinned_mesher_bytes() {
    let grid = regia_world_geom::compat::sampler::HeightGrid::new(8, 8, vec![32_768u16; 64]).expect("grid");
    let sampler = regia_world_geom::compat::sampler::TerrainSampler::new(grid, 64.0);
    let mesh = regia_world_geom::compat::fixtures::switchbacks().mesh(Some(&sampler));
    let before: Vec<[u32; 3]> = mesh
        .triangles
        .iter()
        .flat_map(|t| [t.a, t.b, t.c])
        .map(bits)
        .collect();
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    let mut weld: BTreeMap<(u32, u32, u32), u32> = BTreeMap::new();
    for tri in &mesh.triangles {
        for p in [tri.a, tri.b, tri.c] {
            let key = (p[0].to_bits(), p[1].to_bits(), p[2].to_bits());
            let ix = *weld.entry(key).or_insert_with(|| {
                let i = positions.len() as u32;
                positions.push(p);
                i
            });
            indices.push(ix);
        }
    }
    let snap: Vec<[u32; 3]> = positions.iter().copied().map(bits).collect();
    let ring = ngon(4.0, 0.0, 16);
    let _ = run_zipper(
        "switchbacks",
        &ring,
        &positions,
        &indices,
        ZipperParams::default(),
    );
    assert_eq!(snap, positions.iter().copied().map(bits).collect::<Vec<_>>());
    let after: Vec<[u32; 3]> = mesh
        .triangles
        .iter()
        .flat_map(|t| [t.a, t.b, t.c])
        .map(bits)
        .collect();
    assert_eq!(before, after, "the pinned mesher mesh changed");
}

#[test]
fn closed_mouth_hides_band_and_throat() {
    let n = 24usize;
    let ring = ngon(6.0, 0.0, n);
    let outer = ngon(5.6, 0.0, n);
    let inner = ngon(3.0, -0.4, n);
    let (positions, indices) = tube_between(&outer, &inner);
    let out = run_zipper("mouth", &ring, &positions, &indices, ZipperParams::default()).expect("zipper");
    let throat_y = out
        .throat
        .iter()
        .map(|t| (t.v[0][1] + t.v[1][1] + t.v[2][1]) / 3.0)
        .fold(0.0f32, f32::min);
    assert!(throat_y < -0.5 && throat_y > -2.0, "throat centroid y {throat_y}");
    assert!(
        out.body
            .iter()
            .any(|t| (t.v[0][1] + t.v[1][1] + t.v[2][1]) / 3.0 < -2.5),
        "the deep triangle stays in the body"
    );
    let mut layers = TerrainMeshLayers::new();
    layers.push_mouth(LayerMouth::from_zipper("mouth", ring, out, Vec::new(), true));
    assert!(!layers.mouth("mouth").expect("mouth").band_draw().is_empty());
    layers.set_open("mouth", false);
    assert!(layers.mouth("mouth").expect("mouth").band_draw().is_empty());
}

#[test]
fn collar_closes_the_cell_cut() {
    let map = flat_map(65, 64.0, false);
    let shape = HoleShape::Ellipse(HoleEllipse {
        center: [0.0, 0.0],
        radius: [5.0, 4.0],
        rotation_radians: 0.0,
    });
    let rings = rings_of(&map, &shape);
    let ring = rings.first().expect("ring").clone();
    assert!(ring.len() >= 8, "ring {}", ring.len());
    let outer = inset(&ring, 0.3);
    let inner = inset(&ring, 2.8);
    let (positions, indices) = tube_between(&outer, &inner);
    let out = run_zipper("mouth", &ring, &positions, &indices, ZipperParams::default()).expect("zipper");
    let collar = collar_triangles(&map, &shape);
    let field = heightfield_triangles(&map, &shape);
    assert!(!collar.is_empty(), "the collar is empty");
    assert!(!out.band.triangles.is_empty());
    let band = out.band.triangle_positions();
    let mut seen_collar = 0u32;
    let mut seen_field = 0u32;
    let mut x = -10.0f32;
    while x <= 10.0 {
        let mut z = -10.0f32;
        while z <= 10.0 {
            match surface_owner(&map, &shape, x, z) {
                SurfaceOwner::Collar => {
                    seen_collar += 1;
                    assert!(
                        !strictly_inside_any(x, z, &band),
                        "band covers a collar point at {x},{z}"
                    );
                    assert!(
                        loosely_inside_any(x, z, &collar),
                        "collar point {x},{z} is not on a collar triangle"
                    );
                    assert!(
                        !strictly_inside_any(x, z, &field),
                        "heightfield covers a collar point at {x},{z}"
                    );
                }
                SurfaceOwner::Heightfield => {
                    seen_field += 1;
                    assert!(
                        !strictly_inside_any(x, z, &band),
                        "band covers a heightfield point at {x},{z}"
                    );
                    assert!(
                        loosely_inside_any(x, z, &field),
                        "heightfield point {x},{z} is not on a heightfield triangle"
                    );
                    assert!(
                        !strictly_inside_any(x, z, &collar),
                        "collar covers a heightfield point at {x},{z}"
                    );
                }
                SurfaceOwner::LatticeCut | SurfaceOwner::OutsideMap => {}
            }
            z += 0.05;
        }
        x += 0.05;
    }
    assert!(
        seen_collar > 0 && seen_field > 0,
        "collar {seen_collar} field {seen_field}"
    );
}

fn loosely_inside_any(x: f32, z: f32, tris: &[[[f32; 3]; 3]]) -> bool {
    tris.iter().any(|tri| {
        let b = bary_xz([x, z], tri);
        b.iter().all(|c| *c >= -1.0e-3)
    })
}

fn strictly_inside_any(x: f32, z: f32, tris: &[[[f32; 3]; 3]]) -> bool {
    tris.iter().any(|t| strictly_inside(x, z, t))
}

fn strictly_inside(x: f32, z: f32, tri: &[[f32; 3]; 3]) -> bool {
    let b = bary_xz([x, z], tri);
    b.iter().all(|c| *c > 1.0e-4)
}

fn bary_xz(p: [f32; 2], tri: &[[f32; 3]; 3]) -> [f32; 3] {
    let (x1, z1) = (tri[0][0], tri[0][2]);
    let (x2, z2) = (tri[1][0], tri[1][2]);
    let (x3, z3) = (tri[2][0], tri[2][2]);
    let den = (z2 - z3) * (x1 - x3) + (x3 - x2) * (z1 - z3);
    if den.abs() < 1.0e-12 {
        return [0.0, 0.0, 0.0];
    }
    let b0 = ((z2 - z3) * (p[0] - x3) + (x3 - x2) * (p[1] - z3)) / den;
    let b1 = ((z3 - z1) * (p[0] - x3) + (x1 - x3) * (p[1] - z3)) / den;
    [b0, b1, 1.0 - b0 - b1]
}

#[test]
fn ring_attach_matches_the_drawn_triangles() {
    let terrain = terrain_from(flat_map(65, 64.0, true), 2);
    let shape = HoleShape::Ellipse(HoleEllipse {
        center: [0.0, 0.0],
        radius: [5.0, 3.5],
        rotation_radians: 0.4,
    });
    let ring = rings_of(&terrain.map, &shape).into_iter().next().expect("ring");
    let mips = HeightMips::build(&terrain.map);
    let eye = [12.0f32, -4.0];
    let selection = select_nodes(
        &terrain.map,
        &terrain.minmax,
        &terrain.cdlod,
        terrain.root_level,
        eye,
        None,
    );
    let detail = attach_ring_detail(&terrain, &mips, &selection, eye, &ring);
    let grid = terrain.cdlod.grid_size;
    for (src, v) in ring.iter().zip(&detail) {
        assert!(v.covered, "ring vertex {src:?} is not on a selected node");
        let (min_x, min_z, size) = v.node.draw_rect(&terrain.map, terrain.root_level, grid);
        let step = size / grid as f32;
        let draw = selection.nodes.iter().find(|d| d.node == v.node).expect("node");
        let mut drawn = [[0.0f32; 3]; 3];
        for (i, slot) in drawn.iter_mut().enumerate() {
            let fx = min_x + v.corners_gx[i] as f32 * step;
            let fz = min_z + v.corners_gz[i] as f32 * step;
            let d = ((fx - eye[0]).powi(2) + (fz - eye[1]).powi(2)).sqrt();
            let t = vertex_morph_t(draw.morph_start, draw.morph_end, d);
            assert!(
                (t - v.morph_t[i]).abs() <= 1.0e-6,
                "morph {t} vs {}",
                v.morph_t[i]
            );
            *slot = eval_node_vertex_filtered(
                &terrain.map,
                &mips,
                &terrain.cdlod,
                v.node,
                terrain.root_level,
                v.corners_gx[i],
                v.corners_gz[i],
                v.morph_t[i],
            );
        }
        let mut acc = [0.0f32; 3];
        for (i, corner) in drawn.iter().enumerate() {
            for (k, acc_k) in acc.iter_mut().enumerate() {
                *acc_k += v.bary[i] * corner[k];
            }
        }
        for (k, (got, want)) in acc.iter().zip(v.position).enumerate() {
            assert!(
                (got - want).abs() <= 1.0e-6,
                "corner {k}: {acc:?} vs {:?}",
                v.position
            );
        }
        let b = bary_xz([v.position[0], v.position[2]], &drawn);
        assert!(
            b.iter().all(|c| *c >= -1.0e-4),
            "outside the drawn triangle: {b:?}"
        );
    }
}

#[test]
fn seam_has_no_gap_or_lip_at_any_lod_pair() {
    let width = 512.0f32;
    let n = 513u32;
    let root = 5u32;
    let terrain = terrain_from(flat_map(n, width, true), root);
    let span = quadtree_span(width, n, terrain.cdlod.grid_size, root);
    assert_eq!(span.to_bits(), width.to_bits(), "this map already nests");
    let mouths = [
        HoleShape::Ellipse(HoleEllipse {
            center: [0.0, 0.0],
            radius: [6.0, 4.5],
            rotation_radians: 0.0,
        }),
        HoleShape::Rect {
            center: [24.0, -18.0],
            half_extents: [5.0, 3.0],
            rotation_radians: 0.6,
        },
    ];
    let mut layers = TerrainMeshLayers::new();
    for (i, shape) in mouths.iter().enumerate() {
        let ring = rings_of(&terrain.map, shape).into_iter().next().expect("ring");
        // The seam is the lattice ring against the draw. The authored loop is that ring, seated
        // on the ground, so the stitch does not invent a chord across a stair.
        let mut outer = ring.clone();
        seat(&terrain.map, &mut outer);
        let inner = inset(&ring, 2.6);
        let (positions, indices) = tube_between(&outer, &inner);
        let ground = |x: f32, z: f32| terrain.map.sample_bilinear(x, z);
        let inside = |x: f32, z: f32| shape.contains(x, z);
        let out = zipper(&ZipperInput {
            hole: if i == 0 { "ellipse" } else { "rect" },
            ring: &ring,
            positions: &positions,
            indices: &indices,
            params: ZipperParams::default(),
            ground_y: &ground,
            inside_cut: &inside,
            source_tris: &[],
        })
        .expect("zipper");
        layers.push_mouth(LayerMouth::from_zipper(
            if i == 0 { "ellipse" } else { "rect" },
            ring,
            out,
            Vec::new(),
            true,
        ));
    }
    let mips = HeightMips::build(&terrain.map);
    let mut saw_level0 = false;
    let mut saw_coarser = false;
    let mut morph_lo = false;
    let mut morph_hi = false;
    let grid = terrain.cdlod.grid_size;
    let eyes = seam_eyes(span, root, grid);
    assert!(eyes.len() >= 64, "eyes {}", eyes.len());
    for eye in eyes {
        let passes = selections(&terrain, eye);
        for selection in passes {
            let report = seam_report_with_mips(&terrain, &mips, &layers, &selection, eye);
            assert!(
                report.max_gap_m <= 1.0e-4 && report.max_lip_m <= 1.0e-4,
                "eye {eye:?}: gap {} lip {} at {:?}",
                report.max_gap_m,
                report.max_lip_m,
                report.worst_at
            );
            if report.pass_levels.contains(&0) {
                saw_level0 = true;
            }
            if report.pass_levels.iter().any(|l| *l > 0) {
                saw_coarser = true;
            }
            let ring = &layers.mouths()[0].rings[0];
            for v in attach_ring_detail(&terrain, &mips, &selection, eye, ring) {
                let m = v.morph_t.into_iter().fold(0.0f32, f32::max);
                let lo = v.morph_t.into_iter().fold(1.0f32, f32::min);
                if lo < 0.05 {
                    morph_lo = true;
                }
                if m > 0.95 {
                    morph_hi = true;
                }
            }
        }
    }
    assert!(
        saw_level0 && saw_coarser,
        "levels level0={saw_level0} coarser={saw_coarser}"
    );
    assert!(morph_lo && morph_hi, "morph near 0 {morph_lo}, near 1 {morph_hi}");
}

fn seam_eyes(span: f32, root: u32, grid: u32) -> Vec<[f32; 2]> {
    let mut eyes = Vec::new();
    eyes.push([2.0, 0.5]);
    for level in 0..=root {
        let size = node_world_size(span, root, level);
        let range = 4.0 * size;
        for k in 0..8 {
            let d = (range * (0.55 + k as f32 * 0.2)).max(2.0);
            eyes.push([d, (k as f32 - 3.0) * 0.7]);
        }
    }
    // Level 0 morphs across [112, 128] m on the 512 m map. An eye at 133.5 m still splits the
    // parent (distance to the level-1 rect is under its range) and puts the far ring past 0.95.
    let parent_range = 4.0 * node_world_size(span, root, 1);
    eyes.push([parent_range + 5.5, 0.0]);
    eyes.push([span * 4.0 + 20.0, 1.0]);
    for i in 0..16 {
        eyes.push([8.0 + i as f32 * 12.0, 3.0 + (i % 3) as f32]);
    }
    let _ = grid;
    eyes
}

fn selections(terrain: &Terrain, eye: [f32; 2]) -> Vec<LodSelection> {
    let mut out = vec![select_nodes(
        &terrain.map,
        &terrain.minmax,
        &terrain.cdlod,
        terrain.root_level,
        eye,
        None,
    )];
    let grid = terrain.cdlod.grid_size;
    let world = terrain.map.world_width();
    for k in 0..=3 {
        if k > terrain.root_level {
            break;
        }
        let cell = node_world_size(world, terrain.root_level, k) / grid as f32;
        let texel = cell / 2.0;
        let level = cascade_min_level(world, terrain.root_level, grid, texel);
        out.push(select_nodes_min_level(
            &terrain.map,
            &terrain.minmax,
            &terrain.cdlod,
            terrain.root_level,
            eye,
            None,
            level,
        ));
    }
    out
}

#[test]
fn crater_under_a_mouth_stays_sealed() {
    let terrain_map = flat_map(65, 64.0, true);
    let mut terrain = terrain_from(terrain_map, 2);
    let shape = HoleShape::Ellipse(HoleEllipse {
        center: [0.0, 0.0],
        radius: [5.0, 4.0],
        rotation_radians: 0.2,
    });
    let ring = rings_of(&terrain.map, &shape).into_iter().next().expect("ring");
    let mut outer = inset(&ring, 0.25);
    seat(&terrain.map, &mut outer);
    let inner = inset(&ring, 2.6);
    let (positions, indices) = tube_between(&outer, &inner);
    let ground = |x: f32, z: f32| terrain.map.sample_bilinear(x, z);
    let inside = |x: f32, z: f32| shape.contains(x, z);
    let out = zipper(&ZipperInput {
        hole: "mouth",
        ring: &ring,
        positions: &positions,
        indices: &indices,
        params: ZipperParams::default(),
        ground_y: &ground,
        inside_cut: &inside,
        source_tris: &[],
    })
    .expect("zipper");
    let xz: Vec<[u32; 2]> = out
        .band
        .vertices
        .iter()
        .filter(|v| v.ring)
        .map(|v| [v.position[0].to_bits(), v.position[2].to_bits()])
        .collect();
    let mut layers = TerrainMeshLayers::new();
    layers.push_mouth(LayerMouth::from_zipper("mouth", ring, out, Vec::new(), true));
    let crater = Crater {
        center: [1.0, -0.5],
        radius: 7.0,
        depth: 2.0,
        rim_height: 0.4,
    };
    let dirty = apply_crater(&mut terrain.map, &crater).expect("crater");
    terrain.minmax.update_region(&terrain.map, &dirty);
    layers.note_heights_changed(&terrain.map, &dirty);
    let after: Vec<[u32; 2]> = layers
        .mouth("mouth")
        .expect("mouth")
        .band
        .vertices
        .iter()
        .filter(|v| v.ring)
        .map(|v| [v.position[0].to_bits(), v.position[2].to_bits()])
        .collect();
    assert_eq!(xz, after, "a ring lattice XZ moved");
    let mips = HeightMips::build(&terrain.map);
    let eye = [3.0f32, 1.0];
    let selection = select_nodes(
        &terrain.map,
        &terrain.minmax,
        &terrain.cdlod,
        terrain.root_level,
        eye,
        None,
    );
    let report = seam_report_with_mips(&terrain, &mips, &layers, &selection, eye);
    assert!(
        report.max_gap_m <= 1.0e-4 && report.max_lip_m <= 1.0e-4,
        "gap {} lip {}",
        report.max_gap_m,
        report.max_lip_m
    );
}
