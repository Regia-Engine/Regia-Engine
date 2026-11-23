// SPDX-License-Identifier: MPL-2.0
//! Independent, tiny dense oracles for the existing zero-start MIC(0)-PCG contract.
//! No fluid stepping, random inputs, production matrix indexing or production residual helper.

use regia_fluid::pcg::{PoissonMatrix, SolveReport};

const TOLERANCE: f64 = 1.0 / (1u64 << 30) as f64;
const CAP: u32 = 48;

struct Fixture {
    compact: PoissonMatrix,
    dense: Vec<Vec<f64>>,
    active: Vec<usize>,
    known: Vec<f64>,
    margin: f64,
}

// All coefficients and known solutions are small dyadic numbers. The coordinate-pair dense
// assembly never reads compact.diag/plus and never uses the solver's stride/coord/apply.
fn edge(lower: [usize; 3], axis: usize) -> f64 {
    -((axis + 1) as f64 / 8.0 + (1 + lower[0] + 2 * lower[1] + 3 * lower[2]) as f64 / 32.0)
}

fn margin(at: [usize; 3]) -> f64 {
    1.0 + (at[0] + 2 * at[1] + 3 * at[2]) as f64 / 8.0
}

fn fixture(n: [usize; 3], holes: bool) -> Fixture {
    let mut coords = Vec::new();
    for z in 0..n[2] {
        for y in 0..n[1] {
            for x in 0..n[0] {
                coords.push([x, y, z]);
            }
        }
    }
    let len = coords.len();
    assert!(len <= 12);
    let enabled = |at: [usize; 3]| !holes || (at != [0, 0, 0] && at != [1, 1, 0] && at != n.map(|v| v - 1));
    let active: Vec<_> = coords
        .iter()
        .enumerate()
        .filter_map(|(i, &at)| enabled(at).then_some(i))
        .collect();
    let mut dense = vec![vec![0.0; len]; len];
    for (i, &a) in coords.iter().enumerate() {
        if !enabled(a) {
            continue;
        }
        for (j, &b) in coords.iter().enumerate() {
            if !enabled(b) || a.iter().zip(b).map(|(&v, w)| v.abs_diff(w)).sum::<usize>() != 1 {
                continue;
            }
            let axis = a.iter().zip(b).position(|(&v, w)| v != w).expect("neighbor axis");
            dense[i][j] = edge(if a[axis] < b[axis] { a } else { b }, axis);
        }
        dense[i][i] = margin(a) - dense[i].iter().sum::<f64>();
    }

    // Assemble the compact representation separately, by positive coordinate edges. Both endpoints
    // must be active: no incoming OR outgoing coupling may touch an inactive row.
    let mut compact = PoissonMatrix::new(n);
    let index = |at: [usize; 3]| at[0] + n[0] * at[1] + n[0] * n[1] * at[2];
    for &at in &coords {
        if !enabled(at) {
            continue;
        }
        compact.diag[index(at)] += margin(at);
        for axis in 0..3 {
            let mut next = at;
            next[axis] += 1;
            if next[axis] < n[axis] && enabled(next) {
                let value = edge(at, axis);
                compact.plus[axis][index(at)] = value;
                compact.diag[index(at)] -= value;
                compact.diag[index(next)] -= value;
            }
        }
    }
    let known = coords
        .iter()
        .map(|&at| {
            if enabled(at) {
                (1.0 + at[0] as f64 - 3.0 * at[1] as f64 + 5.0 * at[2] as f64) / 8.0
            } else {
                0.0
            }
        })
        .collect();
    let min_margin = active
        .iter()
        .map(|&i| margin(coords[i]))
        .fold(f64::INFINITY, f64::min);
    Fixture {
        compact,
        dense,
        active,
        known,
        margin: min_margin,
    }
}

fn fixtures() -> impl Iterator<Item = Fixture> {
    // Distinct extents expose swapped strides; every axis is also exercised as the singleton.
    [
        [3, 2, 2],
        [2, 3, 2],
        [2, 2, 3],
        [3, 2, 1],
        [2, 3, 1],
        [3, 1, 2],
        [2, 1, 3],
        [1, 3, 2],
        [1, 2, 3],
        [4, 3, 1],
        [1, 4, 3],
        [3, 1, 4],
    ]
    .into_iter()
    .flat_map(|n| [fixture(n, false), fixture(n, true)])
}

fn product(a: &[Vec<f64>], x: &[f64]) -> Vec<f64> {
    a.iter()
        .map(|row| row.iter().zip(x).map(|(v, x)| v * x).sum())
        .collect()
}

fn norm(x: &[f64]) -> f64 {
    x.iter().map(|v| v.abs()).fold(0.0, f64::max)
}

fn matrix_norm(a: &[Vec<f64>]) -> f64 {
    a.iter()
        .map(|row| row.iter().map(|v| v.abs()).sum::<f64>())
        .fold(0.0, f64::max)
}

// Standard floating-point accumulation bound, u = 2^-53, gamma_k = ku/(1-ku).
// Budgets are fixed from dimensions/operations, never from measured errors (see the recipe).
fn gamma(operations: usize) -> f64 {
    let ku = operations as f64 * (f64::EPSILON / 2.0);
    assert!(ku < 1.0);
    ku / (1.0 - ku)
}

fn assert_close(actual: f64, expected: f64, bound: f64) {
    assert!(
        actual.is_finite() && expected.is_finite() && (actual - expected).abs() <= bound,
        "actual {actual:e}, expected {expected:e}, bound {bound:e}"
    );
}

// Original textbook Gaussian elimination with partial pivoting, confined to this integration
// test. Solve the active principal block: zero rows make the full matrix singular by design.
fn eliminate(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for column in 0..n {
        let pivot = (column..n)
            .max_by(|&i, &j| a[i][column].abs().total_cmp(&a[j][column].abs()))
            .expect("nonempty pivot range");
        a.swap(column, pivot);
        b.swap(column, pivot);
        assert!(a[column][column].is_finite() && a[column][column] != 0.0);
        let (upper, lower) = a.split_at_mut(column + 1);
        let pivot_row = &upper[column];
        for (offset, row) in lower.iter_mut().enumerate() {
            let i = column + 1 + offset;
            let factor = row[column] / pivot_row[column];
            row[column] = 0.0;
            for (entry, &value) in row.iter_mut().zip(pivot_row).skip(column + 1) {
                *entry -= factor * value;
            }
            b[i] -= factor * b[column];
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let upper_sum: f64 = a[i].iter().zip(&x).skip(i + 1).map(|(v, x)| v * x).sum();
        x[i] = (b[i] - upper_sum) / a[i][i];
    }
    x
}

fn dense_solution(f: &Fixture, b: &[f64]) -> Vec<f64> {
    let block = f
        .active
        .iter()
        .map(|&i| f.active.iter().map(|&j| f.dense[i][j]).collect())
        .collect();
    let rhs = f.active.iter().map(|&i| b[i]).collect();
    let active_x = eliminate(block, rhs);
    let mut x = vec![0.0; b.len()];
    for (&i, value) in f.active.iter().zip(active_x) {
        x[i] = value;
    }
    x
}

fn check_report(f: &Fixture, b: &[f64], p: &[f64], report: SolveReport, cap: u32) -> f64 {
    let ap = product(&f.dense, p);
    let true_residual = ap.iter().zip(b).map(|(ap, b)| (b - ap).abs()).fold(0.0, f64::max);
    // A conservative per-step allowance covers the recursively updated residual vs b - A p:
    // <=64m rounded operations per step, scaled by the system magnitude. This fixture family has
    // modest coefficients, strict DD margin >=1, no under/overflow and <=48 iterations.
    let gap =
        gamma(64 * b.len() * (report.iterations as usize + 1)) * (matrix_norm(&f.dense) * norm(p) + norm(b));
    assert!(report.iterations <= cap, "{report:?}");
    assert!(report.residual >= 0.0);
    assert_close(report.residual, true_residual, gap);
    let target = TOLERANCE * norm(b);
    assert_eq!(report.converged, report.residual <= target, "{report:?}");
    if report.converged {
        assert!(
            true_residual <= target + gap,
            "{report:?}, true {true_residual:e}"
        );
    } else {
        assert!(true_residual > target - gap, "{report:?}, true {true_residual:e}");
    }
    gap
}

#[test]
fn dense_reference_pivots_rows() {
    let a = vec![vec![0.0, 2.0, 0.0], vec![1.0, 3.0, 0.0], vec![0.0, 0.0, 4.0]];
    assert_eq!(eliminate(a, vec![4.0, 7.0, 12.0]), vec![1.0, 2.0, 3.0]);
}

#[test]
fn fixtures_have_strictly_dominant_active_blocks_and_zero_inactive_couplings() {
    for f in fixtures() {
        assert!(!f.active.is_empty() && f.active.len() <= 12);
        assert!(f.margin >= 1.0);
        for (i, row) in f.dense.iter().enumerate() {
            if f.active.contains(&i) {
                let off_diagonal: f64 = row
                    .iter()
                    .enumerate()
                    .filter(|&(j, _)| i != j)
                    .map(|(_, v)| v.abs())
                    .sum();
                assert!(row[i] - off_diagonal >= f.margin);
                assert!(f.compact.diag[i] > 0.0);
            } else {
                assert!(row.iter().all(|&v| v == 0.0));
                assert_eq!(f.compact.diag[i], 0.0);
                assert_eq!(f.known[i], 0.0);
            }
            for (j, &value) in row.iter().enumerate() {
                assert_eq!(value, f.dense[j][i]);
                if i != j {
                    assert!(value <= 0.0);
                }
            }
        }
    }
}

#[test]
fn apply_matches_coordinate_dense_matrix_on_every_basis_vector() {
    for f in fixtures() {
        let n = f.known.len();
        for column in 0..=n {
            let mut x = f.known.clone();
            if column < n {
                x.fill(0.0);
                x[column] = 1.0;
            }
            let expected = product(&f.dense, &x);
            let mut actual = vec![f64::NAN; n];
            f.compact.apply(&x, &mut actual);
            let bound = gamma(2 * n) * matrix_norm(&f.dense) * norm(&x);
            for (&actual, &expected) in actual.iter().zip(&expected) {
                assert_close(actual, expected, bound);
            }
        }
    }
}

#[test]
fn heterogeneous_solves_match_pivoted_dense_reference_and_residual_reports() {
    for f in fixtures() {
        let mut normalized = None;
        for scale in [1.0 / 65536.0, 1.0, 65536.0] {
            let known: Vec<_> = f.known.iter().map(|v| v * scale).collect();
            let b = product(&f.dense, &known);
            let reference = dense_solution(&f, &b);
            let n = b.len();
            // For these symmetric strictly DD M-matrices, elimination has no element growth,
            // |multipliers|<=1 and ||A^-1||inf<=1/margin. gamma_(8n²) conservatively covers
            // elimination, substitution and the two dense products for the oracle comparison.
            let oracle_error =
                gamma(8 * n * n) * (matrix_norm(&f.dense) * norm(&known) + norm(&b)) / f.margin;
            for (&x, &known) in reference.iter().zip(&known) {
                assert_close(x, known, oracle_error);
            }
            let mut p = vec![17.0; n];
            let report = f.compact.solve(&b, &mut p, CAP, TOLERANCE);
            assert!(
                report.converged && report.iterations > 0,
                "{:?}: {report:?}",
                f.compact.n
            );
            let gap = check_report(&f, &b, &p, report, CAP);
            // ||p-x||inf <= ||b-Ap||inf / margin; include both residual/oracle rounding.
            let error = (TOLERANCE * norm(&b) + gap) / f.margin + oracle_error;
            for (i, (&p, &reference)) in p.iter().zip(&reference).enumerate() {
                assert_close(p, reference, error);
                if f.compact.diag[i] == 0.0 {
                    assert_eq!(p, 0.0);
                    assert_eq!(b[i], 0.0);
                }
            }
            // Existing contract discards caller contents, even an exact solution.
            let mut again = known;
            let repeated = f.compact.solve(&b, &mut again, CAP, TOLERANCE);
            assert_eq!(again, p);
            assert_eq!(repeated, report);
            // Exact powers of two do not change floating-point rounding here. Relative stopping
            // therefore gives the same iteration count and normalized iterate/residual at each scale.
            let current = (
                report.iterations,
                report.residual / scale,
                p.iter().map(|v| v / scale).collect::<Vec<_>>(),
            );
            if let Some(previous) = &normalized {
                assert_eq!(&current, previous);
            }
            normalized = Some(current);
        }
    }
}

#[test]
fn zero_rhs_and_iteration_caps_report_the_independent_residual() {
    for f in fixtures() {
        let b = product(&f.dense, &f.known);
        let mut p = f.known.clone();
        let zero_cap = f.compact.solve(&b, &mut p, 0, TOLERANCE);
        assert_eq!(
            zero_cap,
            SolveReport {
                iterations: 0,
                residual: norm(&b),
                converged: false
            }
        );
        assert_eq!(p, vec![0.0; b.len()]);
        check_report(&f, &b, &p, zero_cap, 0);

        // Use the 3D block with cycles, including its holed variant: some smaller holed fixtures
        // reduce to isolated cells or chains for which one preconditioned step can be exact.
        if f.compact.n == [3, 2, 2] {
            let capped = f.compact.solve(&b, &mut p, 1, TOLERANCE);
            assert_eq!(capped.iterations, 1);
            assert!(!capped.converged, "{capped:?}");
            check_report(&f, &b, &p, capped, 1);
        }

        for cap in [0, CAP] {
            p.fill(17.0);
            let zero = vec![0.0; b.len()];
            let report = f.compact.solve(&zero, &mut p, cap, TOLERANCE);
            assert_eq!(
                report,
                SolveReport {
                    iterations: 0,
                    residual: 0.0,
                    converged: true
                }
            );
            assert_eq!(p, zero);
            check_report(&f, &zero, &p, report, cap);
        }
    }
}
