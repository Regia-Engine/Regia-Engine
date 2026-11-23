// SPDX-License-Identifier: MPL-2.0
//! Atlas generator tests (ADR-1483). Every check is geometric: no golden images, no wall-clock
//! budgets.

use std::f32::consts::PI;

use super::*;

/// A unit cube. `split`: 24 vertices (one quad per face, as exporters write it); else 8 shared.
fn cube(split: bool) -> (Vec<[f32; 3]>, Vec<u32>) {
    // Outward faces as (corner order counter-clockwise seen from outside).
    let faces: [[[f32; 3]; 4]; 6] = [
        [[1., 0., 0.], [1., 1., 0.], [1., 1., 1.], [1., 0., 1.]],
        [[0., 0., 0.], [0., 0., 1.], [0., 1., 1.], [0., 1., 0.]],
        [[0., 1., 0.], [0., 1., 1.], [1., 1., 1.], [1., 1., 0.]],
        [[0., 0., 0.], [1., 0., 0.], [1., 0., 1.], [0., 0., 1.]],
        [[0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.]],
        [[0., 0., 0.], [0., 1., 0.], [1., 1., 0.], [1., 0., 0.]],
    ];
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices = Vec::new();
    let index_of = |p: [f32; 3], positions: &mut Vec<[f32; 3]>| -> u32 {
        if !split && let Some(i) = positions.iter().position(|q| *q == p) {
            return i as u32;
        }
        positions.push(p);
        (positions.len() - 1) as u32
    };
    for quad in faces {
        let ids: Vec<u32> = quad.iter().map(|p| index_of(*p, &mut positions)).collect();
        indices.extend_from_slice(&[ids[0], ids[1], ids[2], ids[0], ids[2], ids[3]]);
    }
    (positions, indices)
}

/// A latitude/longitude sphere with a duplicated seam column and one pole vertex per segment (as
/// exporters write it), optionally with radial bumps.
fn sphere(segments: u32, rings: u32, bump: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let mut positions = Vec::new();
    for r in 0..=rings {
        let theta = PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = 2.0 * PI * s as f32 / segments as f32;
            let radius = 1.0 + bump * (5.0 * theta).sin() * (7.0 * phi).cos();
            let (st, ct) = theta.sin_cos();
            let (sp, cp) = phi.sin_cos();
            positions.push([radius * st * cp, radius * ct, radius * st * sp]);
        }
    }
    let row = segments + 1;
    let mut indices = Vec::new();
    for r in 0..rings {
        for s in 0..segments {
            let a = r * row + s;
            let b = a + row;
            if r != 0 {
                indices.extend_from_slice(&[a, a + 1, b]);
            }
            if r != rings - 1 {
                indices.extend_from_slice(&[a + 1, b + 1, b]);
            }
        }
    }
    (positions, indices)
}

/// Half of a cylinder (radius 1, height 2), open, `segments` around.
fn half_cylinder(segments: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let mut positions = Vec::new();
    for s in 0..=segments {
        let phi = PI * s as f32 / segments as f32;
        positions.push([phi.cos(), 0.0, phi.sin()]);
        positions.push([phi.cos(), 2.0, phi.sin()]);
    }
    let mut indices = Vec::new();
    for s in 0..segments {
        let (a, b, c, d) = (2 * s, 2 * s + 1, 2 * s + 2, 2 * s + 3);
        indices.extend_from_slice(&[a, b, d, a, d, c]);
    }
    (positions, indices)
}

fn texel(result: &AtlasResult, v: u32) -> [f64; 2] {
    let uv = result.vertices[v as usize].uv;
    [
        f64::from(uv[0]) * f64::from(result.width),
        f64::from(uv[1]) * f64::from(result.height),
    ]
}

fn chart_triangles(result: &AtlasResult) -> Vec<(u32, [[f64; 2]; 3])> {
    let mut out = Vec::new();
    for (c, chart) in result.charts.iter().enumerate() {
        for &face in &chart.faces {
            let tri = [0, 1, 2].map(|k| texel(result, result.indices[face as usize * 3 + k]));
            out.push((c as u32, tri));
        }
    }
    out
}

fn signed_area(t: &[[f64; 2]; 3]) -> f64 {
    0.5 * ((t[1][0] - t[0][0]) * (t[2][1] - t[0][1]) - (t[2][0] - t[0][0]) * (t[1][1] - t[0][1]))
}

/// Strict overlap of two triangles shrunk by `shrink` texels toward their centroids (separating
/// axes over both triangles' edge normals).
fn triangles_overlap(a: &[[f64; 2]; 3], b: &[[f64; 2]; 3], shrink: f64) -> bool {
    let shrunk = |t: &[[f64; 2]; 3]| {
        let c = [
            (t[0][0] + t[1][0] + t[2][0]) / 3.0,
            (t[0][1] + t[1][1] + t[2][1]) / 3.0,
        ];
        t.map(|p| {
            let d = [p[0] - c[0], p[1] - c[1]];
            let len = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-12);
            let k = ((len - shrink) / len).max(0.0);
            [c[0] + d[0] * k, c[1] + d[1] * k]
        })
    };
    let (a, b) = (shrunk(a), shrunk(b));
    for tri in [&a, &b] {
        for i in 0..3 {
            let p = tri[i];
            let q = tri[(i + 1) % 3];
            let n = [-(q[1] - p[1]), q[0] - p[0]];
            let project = |t: &[[f64; 2]; 3]| {
                let d: Vec<f64> = t.iter().map(|v| v[0] * n[0] + v[1] * n[1]).collect();
                (d[0].min(d[1]).min(d[2]), d[0].max(d[1]).max(d[2]))
            };
            let (amin, amax) = project(&a);
            let (bmin, bmax) = project(&b);
            if amax <= bmin || bmax <= amin {
                return false;
            }
        }
    }
    true
}

/// The shared validity checks: every chart triangle counter-clockwise with positive area, inside
/// the atlas, and no two triangles overlapping (exact pairwise test).
fn assert_valid(result: &AtlasResult, face_count: usize) {
    assert_eq!(result.indices.len(), face_count * 3);
    for v in &result.vertices {
        assert!(
            v.uv.iter().all(|c| (0.0..=1.0).contains(c)),
            "uv {:?} outside [0,1]",
            v.uv
        );
    }
    let tris = chart_triangles(result);
    for (c, t) in &tris {
        assert!(
            signed_area(t) > 0.0,
            "chart {c} has a flipped or collapsed triangle {t:?}"
        );
        for p in t {
            assert!(p[0] > 0.0 && p[1] > 0.0, "chart {c} touches the atlas edge");
            assert!(p[0] < f64::from(result.width) && p[1] < f64::from(result.height));
        }
    }
    // Sweep over x-sorted bounding boxes.
    let mut order: Vec<usize> = (0..tris.len()).collect();
    let min_x = |t: &[[f64; 2]; 3]| t[0][0].min(t[1][0]).min(t[2][0]);
    let max_x = |t: &[[f64; 2]; 3]| t[0][0].max(t[1][0]).max(t[2][0]);
    order.sort_by(|&a, &b| min_x(&tris[a].1).total_cmp(&min_x(&tris[b].1)));
    for (k, &i) in order.iter().enumerate() {
        let a = &tris[i].1;
        let end = max_x(a);
        for &j in &order[k + 1..] {
            let b = &tris[j].1;
            if min_x(b) >= end {
                break;
            }
            assert!(
                !triangles_overlap(a, b, 1e-4),
                "triangles of charts {} and {} overlap: {a:?} {b:?}",
                tris[i].0,
                tris[j].0
            );
        }
    }
}

/// Footprint texels of every chart (texels whose interior a chart triangle covers), as a label
/// grid; asserts two charts never share a texel.
fn footprints(result: &AtlasResult) -> Vec<u32> {
    let (w, h) = (result.width as usize, result.height as usize);
    let mut label = vec![u32::MAX; w * h];
    let mut mask = vec![false; w * h];
    for (c, chart) in result.charts.iter().enumerate() {
        mask.fill(false);
        let mut uvs = Vec::new();
        let mut faces = Vec::new();
        for &face in &chart.faces {
            let base = uvs.len() as u32;
            for k in 0..3 {
                let t = texel(result, result.indices[face as usize * 3 + k]);
                uvs.push(regia_core::glam::DVec2::new(t[0], t[1]));
            }
            faces.push([base, base + 1, base + 2]);
        }
        // Slightly smaller than a texel so float noise in the stored UVs cannot add a texel.
        pack::rasterize(&mut mask, result.width, result.height, &uvs, &faces, 0.5 - 1e-3);
        for (i, &set) in mask.iter().enumerate() {
            if set {
                assert!(
                    label[i] == u32::MAX || label[i] == c as u32,
                    "charts {} and {c} share texel {i}",
                    label[i]
                );
                label[i] = c as u32;
            }
        }
    }
    label
}

#[test]
fn a_cube_gets_at_most_six_planar_charts_without_overlap() {
    for split in [true, false] {
        let (positions, indices) = cube(split);
        let result =
            generate(&AtlasMesh::new(&positions, &indices), &AtlasOptions::default()).expect("cube atlas");
        assert!(result.charts.len() <= 6, "{} charts", result.charts.len());
        assert_eq!(result.chart_count(ChartKind::Planar), result.charts.len());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_valid(&result, indices.len() / 3);
        footprints(&result);
        // Each source vertex is split once per chart it touches: 6 faces x 4 corners.
        assert_eq!(result.vertices.len(), 24);
        assert!(result.vertices.iter().all(|v| v.chart.is_some()));
    }
}

#[test]
fn a_sphere_packs_above_sixty_percent_at_1024() {
    let (positions, indices) = sphere(48, 24, 0.0);
    let options = AtlasOptions {
        resolution: 1024,
        ..AtlasOptions::default()
    };
    let result = generate(&AtlasMesh::new(&positions, &indices), &options).expect("sphere atlas");
    eprintln!(
        "sphere: {} charts ({} planar, {} ortho, {} lscm, {} piecewise), utilization {:.3}, \
         texels_per_unit {:.2}, {} passes",
        result.charts.len(),
        result.chart_count(ChartKind::Planar),
        result.chart_count(ChartKind::Ortho),
        result.chart_count(ChartKind::Lscm),
        result.chart_count(ChartKind::Piecewise),
        result.utilization,
        result.texels_per_unit,
        result.pack_passes
    );
    assert!(result.utilization > 0.6, "utilization {}", result.utilization);
    assert_valid(&result, indices.len() / 3);
    footprints(&result);
    // The seam column and the poles weld: one surface, so charts span the seam.
    assert!(
        result.charts.len() < indices.len() / 3 / 8,
        "{} charts",
        result.charts.len()
    );
}

#[test]
fn padding_separates_chart_footprints() {
    let (positions, indices) = sphere(32, 16, 0.15);
    for padding in [0u32, 1, 3] {
        let options = AtlasOptions {
            resolution: 512,
            padding,
            ..AtlasOptions::default()
        };
        let result = generate(&AtlasMesh::new(&positions, &indices), &options).expect("atlas");
        assert_valid(&result, indices.len() / 3);
        let label = footprints(&result);
        let (w, h) = (result.width as i64, result.height as i64);
        // Footprints of different charts are more than 2 * padding texels apart (8-neighbour).
        let reach = 2 * i64::from(padding);
        let at = |x: i64, y: i64| {
            if x < 0 || y < 0 || x >= w || y >= h {
                u32::MAX
            } else {
                label[(y * w + x) as usize]
            }
        };
        for y in 0..h {
            for x in 0..w {
                let c = label[(y * w + x) as usize];
                if c == u32::MAX {
                    continue;
                }
                // And `padding` texels from the atlas border.
                assert!(x >= i64::from(padding) && y >= i64::from(padding));
                assert!(x < w - i64::from(padding) && y < h - i64::from(padding));
                // The closest texels of two charts are edge texels: only those scan around.
                let edge = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                    .iter()
                    .any(|&(dx, dy)| at(x + dx, y + dy) != c);
                if !edge {
                    continue;
                }
                for dy in -reach..=reach {
                    for dx in -reach..=reach {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx < 0 || ny < 0 || nx >= w || ny >= h {
                            continue;
                        }
                        let other = label[(ny * w + nx) as usize];
                        assert!(
                            other == u32::MAX || other == c,
                            "padding {padding}: charts {c} and {other} at ({x},{y}) and ({nx},{ny})"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_result_is_identical_for_one_and_eight_threads() {
    let (positions, indices) = sphere(32, 16, 0.2);
    for brute_force in [false, true] {
        let run = |threads| {
            let options = AtlasOptions {
                resolution: 256,
                threads,
                brute_force,
                ..AtlasOptions::default()
            };
            generate(&AtlasMesh::new(&positions, &indices), &options).expect("atlas")
        };
        let one = run(1);
        let eight = run(8);
        assert_eq!(one, eight, "brute_force {brute_force}");
        assert_eq!(one, run(3));
    }
}

#[test]
fn degenerate_triangles_are_skipped_with_a_typed_warning() {
    let positions = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 0.0],
        [2.0, 0.0, 0.0],
        // Same position as vertex 0 (welds onto it).
        [0.0, 0.0, 0.0],
    ];
    let indices = [
        0, 1, 2, 0, 2, 3, // a quad
        0, 1, 4, // collinear: zero area
        2, 2, 3, // repeated index
        0, 5, 1, // co-located corners
    ];
    let result = generate(&AtlasMesh::new(&positions, &indices), &AtlasOptions::default()).expect("atlas");
    assert_eq!(
        result.warnings,
        vec![
            AtlasWarning::DegenerateTriangle { face: 2 },
            AtlasWarning::DegenerateTriangle { face: 3 },
            AtlasWarning::DegenerateTriangle { face: 4 },
        ]
    );
    assert_eq!(result.warnings[0].code(), "atlas_degenerate_triangle");
    assert_eq!(result.charts.len(), 1);
    assert_eq!(result.charts[0].faces, vec![0, 1]);
    assert_valid(&result, 5);
    for face in 2..5 {
        for k in 0..3 {
            let v = result.vertices[result.indices[face * 3 + k] as usize];
            assert_eq!(v.chart, None);
            assert_eq!(v.uv, [0.0, 0.0]);
            assert_eq!(v.xref, indices[face * 3 + k]);
        }
    }
}

#[test]
fn curved_charts_use_the_conformal_map() {
    let (positions, indices) = half_cylinder(48);
    let result = generate(&AtlasMesh::new(&positions, &indices), &AtlasOptions::default()).expect("atlas");
    assert!(
        result.chart_count(ChartKind::Lscm) >= 1,
        "kinds: {:?}",
        result.charts.iter().map(|c| c.kind).collect::<Vec<_>>()
    );
    assert_eq!(result.chart_count(ChartKind::Piecewise), 0);
    assert_valid(&result, indices.len() / 3);
    footprints(&result);
}

#[test]
fn malformed_input_and_options_are_typed_errors() {
    let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let o = AtlasOptions::default();
    let err = |positions: &[[f32; 3]], indices: &[u32], o: &AtlasOptions| {
        generate(&AtlasMesh::new(positions, indices), o).expect_err("must fail")
    };
    let e = err(&positions, &[0, 1], &o);
    assert_eq!(e, AtlasError::IndexCount { count: 2 });
    assert_eq!(e.code(), "atlas_index_count");
    let e = err(&positions, &[0, 1, 3], &o);
    assert_eq!(e.code(), "atlas_index_out_of_range");
    let e = err(
        &[[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        &[0, 1, 2],
        &o,
    );
    assert_eq!(e, AtlasError::NonFinitePosition { vertex: 0 });
    assert_eq!(e.code(), "atlas_non_finite");
    let e = err(&positions, &[0, 0, 1], &o);
    assert_eq!(e, AtlasError::NoTriangles);
    assert_eq!(e.code(), "atlas_no_triangles");
    let e = err(&positions, &[], &o);
    assert_eq!(e.code(), "atlas_no_triangles");
    let normals = [[0.0, 0.0, 1.0]];
    let e = generate(
        &AtlasMesh {
            normals: Some(&normals),
            ..AtlasMesh::new(&positions, &[0, 1, 2])
        },
        &o,
    )
    .expect_err("normals length");
    assert_eq!(e.code(), "atlas_attribute_length");
    for bad in [
        AtlasOptions { resolution: 4, ..o },
        AtlasOptions {
            resolution: 20_000,
            ..o
        },
        AtlasOptions { padding: 100, ..o },
        AtlasOptions {
            texels_per_unit: -1.0,
            ..o
        },
        AtlasOptions { max_cost: 0.0, ..o },
        AtlasOptions {
            normal_deviation_weight: f32::NAN,
            ..o
        },
    ] {
        let e = err(&positions, &[0, 1, 2], &bad);
        assert_eq!(e.code(), "atlas_invalid_option", "{bad:?}");
    }
}

#[test]
fn a_fixed_density_that_does_not_fit_is_reduced_with_a_warning() {
    let (positions, indices) = cube(true);
    let options = AtlasOptions {
        resolution: 64,
        texels_per_unit: 1000.0,
        ..AtlasOptions::default()
    };
    let result = generate(&AtlasMesh::new(&positions, &indices), &options).expect("atlas");
    assert!(result.texels_per_unit < 1000.0);
    assert!(matches!(
        result.warnings.as_slice(),
        [AtlasWarning::DensityReduced { requested, .. }] if *requested == 1000.0
    ));
    assert_eq!(result.warnings[0].code(), "atlas_density_reduced");
    assert_valid(&result, indices.len() / 3);

    // A density that fits is used as is.
    let options = AtlasOptions {
        resolution: 256,
        texels_per_unit: 32.0,
        ..AtlasOptions::default()
    };
    let result = generate(&AtlasMesh::new(&positions, &indices), &options).expect("atlas");
    assert_eq!(result.texels_per_unit, 32.0);
    assert!(result.warnings.is_empty());
    assert_eq!(result.pack_passes, 1);
}

#[test]
fn input_normals_and_uvs_are_hints_only() {
    let (positions, indices) = sphere(24, 12, 0.0);
    let normals: Vec<[f32; 3]> = positions.clone();
    let uvs: Vec<[f32; 2]> = (0..positions.len()).map(|i| [i as f32 * 0.01, 0.0]).collect();
    let mesh = AtlasMesh {
        normals: Some(&normals),
        uvs: Some(&uvs),
        ..AtlasMesh::new(&positions, &indices)
    };
    let result = generate(&mesh, &AtlasOptions::default()).expect("atlas");
    assert_valid(&result, indices.len() / 3);
    footprints(&result);
}

#[test]
fn rotations_and_options_off_still_pack_validly() {
    let (positions, indices) = sphere(24, 12, 0.3);
    let options = AtlasOptions {
        resolution: 256,
        rotate_charts: false,
        rotate_charts_to_axis: false,
        bilinear: false,
        max_iterations: 0,
        ..AtlasOptions::default()
    };
    let result = generate(&AtlasMesh::new(&positions, &indices), &options).expect("atlas");
    assert!(result.charts.iter().all(|c| !c.rotated));
    assert_valid(&result, indices.len() / 3);
    footprints(&result);
}

/// About 100k triangles. Prints the time (no budget is asserted); run with
/// `cargo test -p regia_asset_pipeline --release atlas::tests::a_100k_triangle_mesh -- --ignored --nocapture`.
#[test]
#[ignore = "large input; run in release (see the doc comment)"]
fn a_100k_triangle_mesh_finishes() {
    let (positions, indices) = sphere(224, 224, 0.05);
    let faces = indices.len() / 3;
    assert!(faces >= 99_000, "{faces} faces");
    let start = std::time::Instant::now();
    let result = generate(
        &AtlasMesh::new(&positions, &indices),
        &AtlasOptions {
            resolution: 2048,
            ..AtlasOptions::default()
        },
    )
    .expect("atlas");
    eprintln!(
        "100k: {faces} faces, {} charts ({} ortho, {} lscm, {} piecewise), utilization {:.3}, {} passes, {:.2?}",
        result.charts.len(),
        result.chart_count(ChartKind::Ortho),
        result.chart_count(ChartKind::Lscm),
        result.chart_count(ChartKind::Piecewise),
        result.utilization,
        result.pack_passes,
        start.elapsed()
    );
    assert_eq!(result.indices.len(), faces * 3);
    footprints(&result);
}

/// A named test mesh: positions and indices.
type Case = (&'static str, Vec<[f32; 3]>, Vec<u32>);

#[test]
fn awkward_meshes_still_produce_valid_atlases() {
    let mut cases: Vec<Case> = Vec::new();
    // A strip of quads with every other quad's winding flipped (no consistent orientation).
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for x in 0..=8 {
        positions.push([x as f32, 0.0, 0.0]);
        positions.push([x as f32, 0.0, 1.0]);
    }
    for x in 0..8u32 {
        let (a, b, c, d) = (2 * x, 2 * x + 1, 2 * x + 2, 2 * x + 3);
        if x % 2 == 0 {
            indices.extend_from_slice(&[a, b, d, a, d, c]);
        } else {
            indices.extend_from_slice(&[a, d, b, a, c, d]);
        }
    }
    cases.push(("flipped winding", positions, indices));
    // Three fins sharing one edge (non-manifold) plus a bent fourth.
    let positions = vec![
        [0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [1.0, 0.5, 0.0],
        [-1.0, 0.5, 0.0],
        [0.0, 0.5, 1.0],
        [0.3, 0.5, -1.0],
    ];
    let indices = vec![0, 1, 2, 1, 0, 3, 0, 1, 4, 1, 0, 5];
    cases.push(("non-manifold fins", positions, indices));
    // Two cubes 10 km apart (f32 precision at large offsets).
    let (mut positions, mut indices) = cube(true);
    let (far, far_indices) = cube(true);
    let base = positions.len() as u32;
    positions.extend(far.iter().map(|p| [p[0] + 10_000.0, p[1], p[2] - 10_000.0]));
    indices.extend(far_indices.iter().map(|i| i + base));
    cases.push(("far apart", positions, indices));
    // A long thin strip: 200 quads of 1 x 0.01, gently curved.
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for x in 0..=200 {
        let angle = x as f32 * 0.01;
        let (s, c) = angle.sin_cos();
        positions.push([10.0 * s, 10.0 * (1.0 - c), 0.0]);
        positions.push([10.0 * s, 10.0 * (1.0 - c), 0.01]);
    }
    for x in 0..200u32 {
        let (a, b, c, d) = (2 * x, 2 * x + 1, 2 * x + 2, 2 * x + 3);
        indices.extend_from_slice(&[a, b, d, a, d, c]);
    }
    cases.push(("thin strip", positions, indices));
    for (name, positions, indices) in cases {
        let result = generate(
            &AtlasMesh::new(&positions, &indices),
            &AtlasOptions {
                resolution: 256,
                ..AtlasOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(result.warnings.is_empty(), "{name}: {:?}", result.warnings);
        assert_valid(&result, indices.len() / 3);
        footprints(&result);
    }
}
