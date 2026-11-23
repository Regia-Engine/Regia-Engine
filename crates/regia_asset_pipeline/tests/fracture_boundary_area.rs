// SPDX-License-Identifier: MPL-2.0
//! Independent render-boundary regression oracle for existing L2 pre-fracture assets.
//!
//! Expected areas come from the dimensions below, not production clipping, mesh volume,
//! interior tags, joints, or pixels. See book/agents/fracture-boundary-area-oracle.md.

use regia_asset_pipeline::fracture::{
    FractureError, FractureSet, FractureSettings, FractureSites, fracture_mesh,
};

type Point = [f64; 3];
type Triangle = [Point; 3];

fn difference(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[i] - b[i])
}

fn dot(a: Point, b: Point) -> f64 {
    a.into_iter().zip(b).map(|(x, y)| x * y).sum()
}

fn length(v: Point) -> f64 {
    dot(v, v).sqrt()
}

fn scaled(v: Point, factor: f64) -> Point {
    v.map(|x| x * factor)
}

/// Oriented area, evaluated after promoting the stored positions to f64.
fn area_vector([a, b, c]: Triangle) -> Point {
    let [u, v] = [difference(b, a), difference(c, a)];
    [
        (u[1] * v[2] - u[2] * v[1]) / 2.0,
        (u[2] * v[0] - u[0] * v[2]) / 2.0,
        (u[0] * v[1] - u[1] * v[0]) / 2.0,
    ]
}

fn center(t: Triangle) -> Point {
    std::array::from_fn(|i| (t[0][i] + t[1][i] + t[2][i]) / 3.0)
}

/// A finite source face on an axis plane; the stepped end faces share their bounding box.
struct SourceFace {
    name: String,
    axis: usize,
    coordinate: f64,
    sign: f64,
    min: Point,
    max: Point,
    area: f64,
}

impl SourceFace {
    fn contains(&self, triangle: Triangle, distance: f64) -> bool {
        triangle.into_iter().all(|p| {
            (p[self.axis] - self.coordinate).abs() <= distance
                && (0..3).all(|i| p[i] >= self.min[i] - distance && p[i] <= self.max[i] + distance)
        })
    }
}

struct Fixture {
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
    faces: Vec<SourceFace>,
    scale: f64,
}

/// Original prisms: a 2x2x2 box and the six-corner step (area 4, depth 2).
/// Side quads use either diagonal or a center fan; end faces use two distinct fans,
/// with the third variant splitting each first-fan triangle around its center.
fn fixture(stepped: bool, scale: f64, triangulation: usize) -> Fixture {
    let profile: Vec<[f64; 2]> = if stepped {
        vec![[0., 0.], [3., 0.], [3., 1.], [1., 1.], [1., 2.], [0., 2.]]
    } else {
        vec![[-1., -1.], [1., -1.], [1., 1.], [-1., 1.]]
    };
    let mut result = Fixture {
        positions: Vec::new(),
        indices: Vec::new(),
        faces: Vec::new(),
        scale,
    };
    let end_area = 4.0 * scale * scale;
    let (min_xy, max_xy) = if stepped {
        ([0., 0.], [3., 2.])
    } else {
        ([-1., -1.], [1., 1.])
    };
    for (z, sign) in [(-1., -1.), (1., 1.)] {
        result.faces.push(SourceFace {
            name: format!("z={z}"),
            axis: 2,
            coordinate: z * scale,
            sign,
            min: [min_xy[0] * scale, min_xy[1] * scale, z * scale],
            max: [max_xy[0] * scale, max_xy[1] * scale, z * scale],
            area: end_area,
        });
        let root = if triangulation == 1 && stepped {
            3
        } else if triangulation == 1 {
            1
        } else {
            0
        };
        for k in 1..profile.len() - 1 {
            let mut triangle = [root, (root + k) % profile.len(), (root + k + 1) % profile.len()]
                .map(|i| [profile[i][0], profile[i][1], z]);
            if sign < 0.0 {
                triangle.swap(1, 2);
            }
            if triangulation == 2 {
                let middle = center(triangle);
                for i in 0..3 {
                    result.push([triangle[i], triangle[(i + 1) % 3], middle]);
                }
            } else {
                result.push(triangle);
            }
        }
    }
    for (i, a) in profile.iter().enumerate() {
        let b = profile[(i + 1) % profile.len()];
        let q = [
            [a[0], a[1], -1.],
            [b[0], b[1], -1.],
            [b[0], b[1], 1.],
            [a[0], a[1], 1.],
        ];
        let axis = usize::from(a[1] == b[1]); // horizontal edge -> y plane
        let coordinate = a[axis];
        let sign = if axis == 0 {
            (b[1] - a[1]).signum()
        } else {
            (a[0] - b[0]).signum()
        };
        result.faces.push(SourceFace {
            name: format!("side {i}: axis {axis}={coordinate}"),
            axis,
            coordinate: coordinate * scale,
            sign,
            min: std::array::from_fn(|j| q.iter().map(|p| p[j]).fold(f64::INFINITY, f64::min) * scale),
            max: std::array::from_fn(|j| q.iter().map(|p| p[j]).fold(f64::NEG_INFINITY, f64::max) * scale),
            area: 2.0 * ((b[0] - a[0]).abs() + (b[1] - a[1]).abs()) * scale * scale,
        });
        match triangulation {
            0 => {
                result.push([q[0], q[1], q[2]]);
                result.push([q[0], q[2], q[3]]);
            }
            1 => {
                result.push([q[0], q[1], q[3]]);
                result.push([q[1], q[2], q[3]]);
            }
            _ => {
                let middle = std::array::from_fn(|j| (q[0][j] + q[2][j]) / 2.0);
                for j in 0..4 {
                    result.push([q[j], q[(j + 1) % 4], middle]);
                }
            }
        }
    }
    // Calibrate the authored triangulations against the independently specified face areas.
    let mut source: Vec<_> = result.faces.iter().map(|_| Measure::default()).collect();
    for triangle in result.indices.as_chunks::<3>().0 {
        let points = triangle.map(|i| result.positions[i as usize].map(f64::from));
        let face = result
            .boundary_face(points)
            .expect("authored triangle is on a source face");
        source[face].add(points, [0.0; 3]);
    }
    assert_boundary(&result, &source, "authored fixture");
    result
}

impl Fixture {
    fn push(&mut self, triangle: Triangle) {
        for point in triangle {
            self.indices.push(self.positions.len() as u32);
            self.positions.push(point.map(|x| (x * self.scale) as f32));
        }
    }

    /// Distance follows f32 position precision, including the plane's offset from the origin.
    fn distance_tolerance(&self, offset: f64) -> f64 {
        2e-6 * self.scale.max(offset.abs())
    }

    fn area_tolerance(&self, area: f64) -> f64 {
        2e-5 * area + 1e-10 * self.scale * self.scale
    }

    fn boundary_face(&self, triangle: Triangle) -> Option<usize> {
        let matches: Vec<_> = self
            .faces
            .iter()
            .enumerate()
            .filter_map(|(i, face)| {
                face.contains(triangle, self.distance_tolerance(face.coordinate))
                    .then_some(i)
            })
            .collect();
        assert!(matches.len() <= 1, "ambiguous nonzero triangle {triangle:?}");
        matches.first().copied()
    }
}

#[derive(Default)]
struct Measure {
    area: f64,
    vector: Point,
    shaded: Point,
    moment: Point,
}

impl Measure {
    fn add(&mut self, triangle: Triangle, shaded: Point) {
        let vector = area_vector(triangle);
        let area = length(vector);
        self.area += area;
        for (i, c) in center(triangle).into_iter().enumerate() {
            self.vector[i] += vector[i];
            self.shaded[i] += area * shaded[i];
            self.moment[i] += area * c;
        }
    }
}

struct CutFace {
    chunk: usize,
    normal: Point,
    offset: f64,
    triangles: Vec<Triangle>,
    measure: Measure,
}

impl CutFace {
    fn contains(&self, triangle: Triangle, fixture: &Fixture) -> bool {
        triangle
            .into_iter()
            .all(|p| (dot(self.normal, p) - self.offset).abs() <= fixture.distance_tolerance(self.offset))
    }
}

/// Classifies every positive-area render triangle geometrically, never by `interior_start`.
/// Largest triangles establish stable cut planes before f32 slivers are accumulated.
fn measure(fixture: &Fixture, set: &FractureSet) -> (Vec<Measure>, Vec<CutFace>) {
    let mut exterior: Vec<_> = fixture.faces.iter().map(|_| Measure::default()).collect();
    let mut cuts: Vec<CutFace> = Vec::new();
    for (chunk, c) in set.chunks.iter().enumerate() {
        let (indices, remainder) = c.render.indices.as_chunks::<3>();
        assert!(remainder.is_empty());
        let mut triangles: Vec<_> = indices
            .iter()
            .map(|t| {
                let triangle = t.map(|i| c.render.positions[i as usize].map(f64::from));
                let shaded = std::array::from_fn(|axis| {
                    t.iter()
                        .map(|i| f64::from(c.render.normals[*i as usize][axis]))
                        .sum::<f64>()
                        / 3.0
                });
                (triangle, shaded)
            })
            .collect();
        triangles.sort_by(|a, b| length(area_vector(b.0)).total_cmp(&length(area_vector(a.0))));
        for (triangle, shaded) in triangles {
            let vector = area_vector(triangle);
            let area = length(vector);
            assert!(area.is_finite());
            if area == 0.0 {
                continue;
            }
            if let Some(face) = fixture.boundary_face(triangle) {
                exterior[face].add(triangle, shaded);
            } else {
                let existing = cuts
                    .iter()
                    .position(|f| f.chunk == chunk && f.contains(triangle, fixture));
                let index = existing.unwrap_or_else(|| {
                    let normal = scaled(vector, 1.0 / area);
                    cuts.push(CutFace {
                        chunk,
                        normal,
                        offset: dot(normal, triangle[0]),
                        triangles: Vec::new(),
                        measure: Measure::default(),
                    });
                    cuts.len() - 1
                });
                cuts[index].triangles.push(triangle);
                cuts[index].measure.add(triangle, shaded);
            }
        }
    }
    (exterior, cuts)
}

fn assert_boundary(fixture: &Fixture, exterior: &[Measure], context: &str) {
    for (face, actual) in fixture.faces.iter().zip(exterior) {
        let tolerance = fixture.area_tolerance(face.area);
        assert!(
            (actual.area - face.area).abs() <= tolerance,
            "{context}: {}: expected {}, got {}, tolerance {tolerance}",
            face.name,
            face.area,
            actual.area
        );
        let mut expected = [0.0; 3];
        expected[face.axis] = face.sign * face.area;
        assert!(
            length(difference(actual.vector, expected)) <= tolerance,
            "{context}: {}: exterior winding {:?} vs {expected:?}",
            face.name,
            actual.vector
        );
    }
}

/// Pair by plane and area centroid, then require every triangle on both planes, equal areas,
/// opposite geometric area normals and opposite area-weighted render normals.
fn assert_pairs(fixture: &Fixture, cuts: &[CutFace], context: &str) {
    assert!(!cuts.is_empty(), "{context}: must exercise interior cuts");
    let mut paired = vec![false; cuts.len()];
    for (i, a) in cuts.iter().enumerate() {
        if paired[i] {
            continue;
        }
        let ca = scaled(a.measure.moment, 1.0 / a.measure.area);
        let matches: Vec<_> = cuts
            .iter()
            .enumerate()
            .filter_map(|(j, b)| {
                let cb = scaled(b.measure.moment, 1.0 / b.measure.area);
                (i != j
                    && a.chunk != b.chunk
                    && dot(a.normal, b.normal).abs() > 1.0 - 2e-5
                    && length(difference(ca, cb)) <= 8.0 * fixture.distance_tolerance(a.offset)
                    && b.triangles.iter().all(|t| a.contains(*t, fixture))
                    && a.triangles.iter().all(|t| b.contains(*t, fixture)))
                .then_some(j)
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "{context}: chunk {} cut at {:?}, area {}, normal {:?}: mates {matches:?}",
            a.chunk,
            ca,
            a.measure.area,
            a.normal
        );
        let j = matches[0];
        assert!(!paired[j], "{context}: cut paired more than once");
        let b = &cuts[j];
        let tolerance = fixture.area_tolerance(a.measure.area.max(b.measure.area));
        assert!(
            (a.measure.area - b.measure.area).abs() <= tolerance,
            "{context}: chunks {} / {}: cut area {} / {}",
            a.chunk,
            b.chunk,
            a.measure.area,
            b.measure.area
        );
        assert!(
            length(difference(a.measure.vector, scaled(b.measure.vector, -1.0))) <= tolerance,
            "{context}: geometric area normals do not oppose"
        );
        for face in [a, b] {
            assert!(
                length(difference(face.measure.vector, face.measure.shaded)) <= tolerance,
                "{context}: chunk {}: shaded vs geometric area normal",
                face.chunk
            );
        }
        assert!(
            length(difference(a.measure.shaded, scaled(b.measure.shaded, -1.0))) <= tolerance,
            "{context}: render area normals do not oppose"
        );
        paired[i] = true;
        paired[j] = true;
    }
    assert!(paired.into_iter().all(|p| p));
}

fn run(
    fixture: &Fixture,
    mut settings: FractureSettings,
    count: usize,
    context: &str,
) -> Result<(), FractureError> {
    settings.min_chunk_volume_fraction = 0.0; // Conservation excludes deliberate chunk dropping.
    let set = fracture_mesh(&fixture.positions, &fixture.indices, &settings).inspect_err(|error| {
        eprintln!("{context}: fracture failed: {error}");
    })?;
    assert_eq!(set.chunks.len(), count, "{context}: no sites may be discarded");
    let (exterior, cuts) = measure(fixture, &set);
    assert_pairs(fixture, &cuts, context);
    assert_boundary(fixture, &exterior, context);
    Ok(())
}

#[test]
fn every_source_plane_survives_seeds_scales_and_retriangulation() -> Result<(), FractureError> {
    for stepped in [false, true] {
        for scale in [0.125, 32.0] {
            for triangulation in 0..3 {
                let fixture = fixture(stepped, scale, triangulation);
                for seed in [3, 11, 29] {
                    let context = format!(
                        "stepped={stepped}, scale={scale}, triangulation={triangulation}, seed={seed}"
                    );
                    run(&fixture, FractureSettings::random(7, seed), 7, &context)?;
                }
            }
        }
    }
    Ok(())
}

#[test]
fn cuts_through_source_vertices_and_edges_conserve_each_render_plane() -> Result<(), FractureError> {
    // Extends fracture_clip::tests::a_diagonal_cut_through_vertices_stays_closed:
    // test the public API's render areas and paired cuts, not closedness or volume again.
    for scale in [0.125, 32.0] {
        for triangulation in 0..3 {
            let fixture = fixture(false, scale, triangulation);
            for (label, sites, normal, offset) in [
                (
                    "edges: x+y=0",
                    [[-0.5, -0.5, 0.], [0.5, 0.5, 0.]],
                    [1., 1., 0.],
                    0.,
                ),
                (
                    "vertices: x+y+z=1",
                    [[0., 0., 0.25], [0.5, 0.5, 0.75]],
                    [1., 1., 1.],
                    scale,
                ),
            ] {
                let on_cut: std::collections::BTreeSet<_> = fixture
                    .positions
                    .iter()
                    .filter(|p| (dot(normal, p.map(f64::from)) - offset).abs() < 1e-12 * scale)
                    .map(|p| p.map(f32::to_bits))
                    .collect();
                assert!(
                    on_cut.len() >= 3,
                    "{label}: the cut must hit distinct authored vertices"
                );
                // Check the original vertical edges explicitly, independent of tessellation.
                let contained_edges = [-1., 1.]
                    .into_iter()
                    .flat_map(|x| [-1., 1.].into_iter().map(move |y| (x, y)))
                    .filter(|(x, y)| {
                        [-1., 1.].into_iter().all(|z| {
                            (dot(normal, [x * scale, y * scale, z * scale]) - offset).abs() < 1e-12 * scale
                        })
                    })
                    .count();
                assert_eq!(contained_edges, if offset == 0.0 { 2 } else { 0 }, "{label}");
                let mut settings = FractureSettings::random(2, 0);
                settings.sites = FractureSites::Explicit(
                    sites.into_iter().map(|p| p.map(|x| (x * scale) as f32)).collect(),
                );
                run(
                    &fixture,
                    settings,
                    2,
                    &format!("{label}, scale={scale}, triangulation={triangulation}"),
                )?;
            }
        }
    }
    Ok(())
}

/// The plane x=1 contains the step wall at y=1..2 and the internal cut at y=0..1.
/// Infinite-plane classification would incorrectly count the internal patch as exterior.
fn run_coplanar_step_wall(scale: f64, triangulation: usize) -> Result<(), FractureError> {
    let fixture = fixture(true, scale, triangulation);
    let mut settings = FractureSettings::random(2, 0);
    settings.sites = FractureSites::Explicit(
        [[0.5, 0.5, 0.0], [1.5, 0.5, 0.0]]
            .into_iter()
            .map(|p| p.map(|x| (x * scale) as f32))
            .collect(),
    );
    run(
        &fixture,
        settings,
        2,
        &format!("coplanar step wall: scale={scale}, triangulation={triangulation}"),
    )
}

// Independent test entries ensure a counterexample cannot hide the remaining scale/triangulations.
macro_rules! step_wall_case {
    ($name:ident, $scale:expr, $triangulation:expr) => {
        #[test]
        fn $name() -> Result<(), FractureError> {
            run_coplanar_step_wall($scale, $triangulation)
        }
    };
}

step_wall_case!(coplanar_step_wall_small_first_diagonal, 0.125, 0);
step_wall_case!(coplanar_step_wall_small_second_diagonal, 0.125, 1);
step_wall_case!(coplanar_step_wall_small_subdivided, 0.125, 2);
step_wall_case!(coplanar_step_wall_large_first_diagonal, 32.0, 0);
step_wall_case!(coplanar_step_wall_large_second_diagonal, 32.0, 1);
step_wall_case!(coplanar_step_wall_large_subdivided, 32.0, 2);
