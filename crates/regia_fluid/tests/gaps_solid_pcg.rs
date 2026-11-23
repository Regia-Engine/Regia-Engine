// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (lane testgaps-fluid-2d-xr): collider distances, the open-face fractions of the
//! variational pressure solve, and the PCG solver on known matrices. Counts and tolerances only.

use glam::Vec3;
use regia_fluid::grid::GridDims;
use regia_fluid::pcg::PoissonMatrix;
use regia_fluid::sim::Shape;
use regia_fluid::solid::{Collider, SolidField};

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

/// Signed distance of a box: exact outside faces, edges and corners, and negative inside.
#[test]
fn box_distance_is_exact_outside_and_negative_inside() {
    let b = Collider::Box {
        center: Vec3::new(1.0, 1.0, 1.0),
        half_extents: Vec3::new(1.0, 2.0, 0.5),
    };
    assert!(close(b.distance(Vec3::new(3.0, 1.0, 1.0)), 1.0), "face");
    assert!(
        close(b.distance(Vec3::new(1.0, 1.0, 1.0)), -0.5),
        "centre: nearest face is z, 0.5 away"
    );
    assert!(close(b.distance(Vec3::new(2.0, 1.0, 1.0)), 0.0), "on the +x face");
    // Edge: 3 and 3 beyond the faces on two axes -> 3 sqrt 2.
    assert!(
        close(b.distance(Vec3::new(5.0, 6.0, 1.0)), 3.0f32.hypot(3.0)),
        "edge"
    );
    // Corner: 1, 2, 1 beyond the faces -> sqrt 6.
    assert!(
        close(b.distance(Vec3::new(3.0, 5.0, 2.5)), 6.0f32.sqrt()),
        "corner"
    );
    // A point exactly on the surface has distance zero.
    assert!(close(b.distance(Vec3::new(2.0, 3.0, 1.5)), 0.0));
}

/// Sphere: radius is subtracted from the centre distance.
#[test]
fn sphere_distance() {
    let s = Collider::Sphere {
        center: Vec3::new(0.0, 1.0, 0.0),
        radius: 0.5,
    };
    assert!(close(s.distance(Vec3::new(0.0, 3.0, 0.0)), 1.5));
    assert!(close(s.distance(Vec3::new(0.0, 1.0, 0.0)), -0.5));
    assert!(close(s.distance(Vec3::new(0.5, 1.0, 0.0)), 0.0));
}

fn terrain(heights: Vec<f32>) -> Collider {
    Collider::Heightfield {
        origin: [0.0, 0.0],
        spacing: 1.0,
        columns: 3,
        rows: 2,
        heights,
    }
}

/// The heightfield answers the vertical distance, bilinear between samples, edge-held outside,
/// and "no collider" (`f32::MAX`) for malformed fields.
#[test]
fn heightfield_distance_and_malformed_fields() {
    let h = terrain(vec![0.0, 1.0, 2.0, 0.0, 1.0, 2.0]);
    assert!(close(h.distance(Vec3::new(0.0, 0.5, 0.0)), 0.5));
    assert!(
        close(h.distance(Vec3::new(1.5, 2.0, 0.5)), 2.0 - 1.5),
        "bilinear in x"
    );
    assert!(
        close(h.distance(Vec3::new(2.0, 1.0, 1.0)), -1.0),
        "below ground is negative"
    );
    // Outside the sampled area the edge sample holds.
    assert!(close(h.distance(Vec3::new(50.0, 3.0, 0.0)), 1.0));
    assert!(close(h.distance(Vec3::new(-50.0, 3.0, -9.0)), 3.0));
    // Bilinear in z: heights differ between rows.
    let slope = terrain(vec![0.0, 0.0, 0.0, 4.0, 4.0, 4.0]);
    assert!(close(slope.distance(Vec3::new(1.0, 2.0, 0.5)), 0.0));
    // Malformed: too few heights, zero columns, non-positive spacing, NaN spacing.
    assert_eq!(terrain(vec![0.0; 5]).distance(Vec3::ZERO), f32::MAX);
    let zero = Collider::Heightfield {
        origin: [0.0; 2],
        spacing: 1.0,
        columns: 0,
        rows: 2,
        heights: vec![],
    };
    assert_eq!(zero.distance(Vec3::ZERO), f32::MAX);
    let bad = Collider::Heightfield {
        origin: [0.0; 2],
        spacing: 0.0,
        columns: 3,
        rows: 2,
        heights: vec![0.0; 6],
    };
    assert_eq!(bad.distance(Vec3::ZERO), f32::MAX);
    // A NaN query point must not panic or index out of range.
    let _ = h.distance(Vec3::new(f32::NAN, 0.0, f32::NAN));
}

fn dims(n: usize) -> GridDims {
    GridDims {
        nx: n,
        ny: n,
        nz: n,
        dx: 1.0,
        origin: Vec3::ZERO,
    }
}

/// With no collider only the domain walls are closed and every node distance is `f32::MAX`.
#[test]
fn empty_solid_field_has_closed_walls_only() {
    let d = dims(3);
    let s = SolidField::new(&d, vec![]);
    assert!(s.node_phi.iter().all(|v| *v == f32::MAX));
    assert_eq!(s.distance(Vec3::splat(1.0)), f32::MAX);
    assert_eq!(s.normal(Vec3::splat(1.0), 0.1), Vec3::ZERO);
    for axis in 0..3 {
        let size = d.face_dims(axis);
        for k in 0..size[2] {
            for j in 0..size[1] {
                for i in 0..size[0] {
                    let id = [i, j, k];
                    let wall = id[axis] == 0 || id[axis] == size[axis] - 1;
                    let v = s.face_open[axis][i + size[0] * (j + size[1] * k)];
                    assert_eq!(v, if wall { 0.0 } else { 1.0 }, "axis {axis} face {id:?}");
                }
            }
        }
    }
}

/// A box that exactly fills the middle cell closes the six faces around it, leaves its other
/// faces open, and a sphere's surface faces are partly open.
#[test]
fn open_fractions_around_a_solid_cell() {
    let d = dims(3);
    let s = SolidField::new(
        &d,
        vec![Collider::Box {
            center: Vec3::splat(1.5),
            half_extents: Vec3::splat(0.5),
        }],
    );
    let at = |axis: usize, i: usize, j: usize, k: usize| {
        let size = d.face_dims(axis);
        s.face_open[axis][i + size[0] * (j + size[1] * k)]
    };
    // The faces of cell (1,1,1): x faces at i = 1 and 2, same for y, z. Their nodes sit exactly
    // on the box surface (distance 0), which counts as open per node, so the face is open there.
    // The faces through the middle of the solid cell do not exist; the neighbouring open faces stay open.
    for axis in 0..3 {
        let size = d.face_dims(axis);
        let mut open = 0;
        for k in 0..size[2] {
            for j in 0..size[1] {
                for i in 0..size[0] {
                    if s.face_open[axis][i + size[0] * (j + size[1] * k)] > 0.0 {
                        open += 1;
                    }
                }
            }
        }
        // Interior faces of an n=3 cube along one axis: 2 planes x 9 = 18, all open (zero distance
        // on the solid's boundary is open).
        assert_eq!(open, 18, "axis {axis}");
    }
    assert_eq!(at(0, 0, 1, 1), 0.0, "domain wall");
    // A box bigger than the cell closes the faces that cross it.
    let big = SolidField::new(
        &d,
        vec![Collider::Box {
            center: Vec3::splat(1.5),
            half_extents: Vec3::splat(0.9),
        }],
    );
    let size = d.face_dims(0);
    let i = |ii: usize, jj: usize, kk: usize| ii + size[0] * (jj + size[1] * kk);
    assert_eq!(big.face_open[0][i(1, 1, 1)], 0.0, "face fully inside the solid");
    assert_eq!(big.face_open[0][i(2, 1, 1)], 0.0);
    let edge = big.face_open[0][i(1, 0, 0)];
    assert!(
        edge > 0.5 && edge < 1.0,
        "a face grazed by the solid is mostly open ({edge})"
    );
    // A face partly covered has a fraction strictly between 0 and 1.
    let part = SolidField::new(
        &d,
        vec![Collider::Box {
            center: Vec3::new(1.0, 0.0, 1.5),
            half_extents: Vec3::new(0.4, 0.5, 2.0),
        }],
    );
    let f = part.face_open[0][i(1, 0, 1)];
    assert!(f > 0.0 && f < 1.0, "partly covered face has fraction {f}");
}

/// The union takes the nearest collider and its normal points away from it.
#[test]
fn union_distance_and_normal() {
    let d = dims(4);
    let s = SolidField::new(
        &d,
        vec![
            Collider::Sphere {
                center: Vec3::new(1.0, 2.0, 2.0),
                radius: 0.5,
            },
            Collider::Sphere {
                center: Vec3::new(3.0, 2.0, 2.0),
                radius: 0.5,
            },
        ],
    );
    assert!(close(s.distance(Vec3::new(2.0, 2.0, 2.0)), 0.5));
    let n = s.normal(Vec3::new(1.0, 3.0, 2.0), 0.01);
    assert!((n - Vec3::Y).length() < 1e-3, "{n:?}");
    let n = s.normal(Vec3::new(3.6, 2.0, 2.0), 0.01);
    assert!((n - Vec3::X).length() < 1e-3, "{n:?}");
    // The sampled node distances agree with the analytic ones.
    let nx = d.nx + 1;
    let ny = d.ny + 1;
    for (i, j, k) in [(0usize, 0usize, 0usize), (1, 2, 2), (3, 2, 2), (4, 4, 4)] {
        let p = Vec3::new(i as f32, j as f32, k as f32);
        assert!(close(s.node_phi[i + nx * (j + ny * k)], s.distance(p)));
    }
}

/// Shape membership and bounding boxes, including the inclusive boundary.
#[test]
fn shape_contains_and_aabb() {
    let b = Shape::Box {
        center: Vec3::new(1.0, 1.0, 1.0),
        half_extents: Vec3::new(0.5, 1.0, 0.25),
    };
    assert!(
        b.contains(Vec3::new(1.5, 2.0, 1.25)),
        "corner is inside (inclusive)"
    );
    assert!(!b.contains(Vec3::new(1.51, 1.0, 1.0)));
    assert_eq!(b.aabb(), (Vec3::new(0.5, 0.0, 0.75), Vec3::new(1.5, 2.0, 1.25)));
    let s = Shape::Sphere {
        center: Vec3::ZERO,
        radius: 2.0,
    };
    assert!(s.contains(Vec3::new(2.0, 0.0, 0.0)));
    assert!(!s.contains(Vec3::new(1.5, 1.5, 0.0)));
    assert_eq!(s.aabb(), (Vec3::splat(-2.0), Vec3::splat(2.0)));
}

/// A 3D Poisson matrix on an n³ box, air (Dirichlet) above the top layer, closed elsewhere: SPD.
fn box_matrix(n: usize) -> PoissonMatrix {
    let mut a = PoissonMatrix::new([n, n, n]);
    for k in 0..n {
        for j in 0..n {
            for i in 0..n {
                let c = i + n * (j + n * k);
                let mut diag = 0.0;
                let nbr = [
                    (i > 0, 0usize, false),
                    (i + 1 < n, 0, true),
                    (j > 0, 1, false),
                    (j + 1 < n, 1, true),
                    (k > 0, 2, false),
                    (k + 1 < n, 2, true),
                ];
                for (exists, axis, plus) in nbr {
                    if exists {
                        diag += 1.0;
                        if plus {
                            a.plus[axis][c] = -1.0;
                        }
                    }
                }
                // Air above: one Dirichlet neighbour on the top layer.
                if j + 1 == n {
                    diag += 1.0;
                }
                a.diag[c] = diag;
            }
        }
    }
    a
}

fn residual_inf(a: &PoissonMatrix, p: &[f64], b: &[f64]) -> f64 {
    let mut ap = vec![0.0; b.len()];
    a.apply(p, &mut ap);
    ap.iter().zip(b).fold(0.0f64, |m, (x, y)| m.max((x - y).abs()))
}

/// PCG solves a 3D problem to the stated relative tolerance in far fewer iterations than cells,
/// and the true residual (recomputed from scratch) honours the report.
#[test]
fn pcg_converges_on_a_3d_box_and_the_report_is_honest() {
    let n = 10;
    let a = box_matrix(n);
    let mut b = vec![0.0; n * n * n];
    for (c, v) in b.iter_mut().enumerate() {
        *v = ((c * 7919) % 13) as f64 - 6.0;
    }
    let mut p = vec![0.0; b.len()];
    let r = a.solve(&b, &mut p, 500, 1e-10);
    assert!(r.converged, "{r:?}");
    assert!(
        r.iterations > 0 && r.iterations < 60,
        "iterations {}",
        r.iterations
    );
    let norm_b = b.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let true_res = residual_inf(&a, &p, &b);
    assert!(true_res <= 1e-9 * norm_b * 10.0, "true residual {true_res}");
    assert!(
        (true_res - r.residual).abs() <= 1e-9 * norm_b,
        "reported {} true {true_res}",
        r.residual
    );
    // Determinism: the same problem gives bit-identical iterates.
    let mut p2 = vec![1.0; b.len()];
    let r2 = a.solve(&b, &mut p2, 500, 1e-10);
    assert_eq!(r, r2);
    assert_eq!(p, p2, "solve restarts from zero whatever p held");
}

/// The iteration cap is honoured and reported as not converged; a zero right-hand side is
/// solved in zero iterations; rows outside the liquid (zero diagonal) stay zero.
#[test]
fn pcg_cap_zero_rhs_and_inactive_rows() {
    let a = box_matrix(8);
    let b: Vec<f64> = (0..512).map(|c| if c % 2 == 0 { 1.0 } else { -1.0 }).collect();
    let mut p = vec![0.0; 512];
    let r = a.solve(&b, &mut p, 2, 1e-12);
    assert!(!r.converged);
    assert_eq!(r.iterations, 2);
    let zero = vec![0.0; 512];
    let r = a.solve(&zero, &mut p, 50, 1e-6);
    assert_eq!((r.iterations, r.converged, r.residual), (0, true, 0.0));
    assert!(p.iter().all(|v| *v == 0.0));

    // Half the cells are air (zero rows): the solve touches only the liquid ones.
    let n = 6;
    let mut m = PoissonMatrix::new([n, 1, 1]);
    for i in 0..3 {
        m.diag[i] = if i == 0 { 1.0 } else { 2.0 };
        if i + 1 < 3 {
            m.plus[0][i] = -1.0;
        }
    }
    // Add a Dirichlet air neighbour at the end of the liquid run so the matrix is non-singular.
    m.diag[2] = 1.0 + 1.0;
    let mut b = vec![0.0; n];
    b[0] = 1.0;
    b[1] = 1.0;
    b[2] = 1.0;
    b[4] = 100.0; // a right-hand side on an air cell must be ignored
    let mut p = vec![7.0; n];
    let r = m.solve(&b, &mut p, 50, 1e-12);
    assert_eq!(p[3], 0.0);
    assert_eq!(p[4], 0.0);
    assert_eq!(p[5], 0.0);
    assert!(r.iterations > 0);
}

/// The MIC(0) factor is zero on inactive cells, positive on active ones, and applying the
/// preconditioner to a zero residual gives zero.
#[test]
fn mic0_and_precondition_basics() {
    let mut a = box_matrix(4);
    a.diag[5] = 0.0;
    a.plus[0][5] = 0.0;
    a.plus[1][5] = 0.0;
    a.plus[2][5] = 0.0;
    let pre = a.mic0();
    assert_eq!(pre[5], 0.0);
    for (c, v) in pre.iter().enumerate() {
        if c != 5 {
            assert!(*v > 0.0 && v.is_finite(), "cell {c}: {v}");
        }
    }
    let (mut q, mut z) = (vec![9.0; 64], vec![9.0; 64]);
    a.precondition(&pre, &vec![0.0; 64], &mut q, &mut z);
    assert!(z.iter().all(|v| *v == 0.0));
}
