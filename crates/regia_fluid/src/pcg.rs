// SPDX-License-Identifier: MPL-2.0
//! Pressure projection: the 7-point Poisson matrix over the liquid cells, solved by conjugate
//! gradients with the modified incomplete Cholesky MIC(0) preconditioner (Bridson, "Fluid
//! Simulation for Computer Graphics", 2nd ed., ch. 5). Sequential and in f64, so the same matrix
//! gives the same iterates on every run.

/// The matrix in the textbook's compact form: per cell its diagonal and its coupling to the +x,
/// +y and +z neighbours (non-positive). Zero rows are cells outside the liquid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoissonMatrix {
    /// Cells along x, y, z.
    pub n: [usize; 3],
    /// Diagonal.
    pub diag: Vec<f64>,
    /// Coupling to `(i + 1, j, k)`, `(i, j + 1, k)`, `(i, j, k + 1)`.
    pub plus: [Vec<f64>; 3],
}

/// Vectors reused by [`PoissonMatrix::solve_with`]. The public [`PoissonMatrix::solve`] builds a
/// fresh one.
#[derive(Debug, Clone, Default)]
pub(crate) struct CgWorkspace {
    /// Residual.
    pub r: Vec<f64>,
    /// MIC(0) inverse diagonal.
    pub precon: Vec<f64>,
    /// Forward-substitution scratch.
    pub q: Vec<f64>,
    /// Preconditioned residual, also the matrix-vector product slot.
    pub z: Vec<f64>,
    /// Search direction.
    pub s: Vec<f64>,
}

/// What one solve did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SolveReport {
    /// Iterations run.
    pub iterations: u32,
    /// Infinity norm of the final residual.
    pub residual: f64,
    /// Whether the tolerance was met inside the cap.
    pub converged: bool,
}

/// MIC(0) tuning (the textbook values).
const TAU: f64 = 0.97;
const SIGMA: f64 = 0.25;

impl PoissonMatrix {
    /// A zero matrix for `n` cells.
    #[must_use]
    pub fn new(n: [usize; 3]) -> Self {
        let len = n[0] * n[1] * n[2];
        Self {
            n,
            diag: vec![0.0; len],
            plus: [vec![0.0; len], vec![0.0; len], vec![0.0; len]],
        }
    }

    fn stride(&self, axis: usize) -> usize {
        match axis {
            0 => 1,
            1 => self.n[0],
            _ => self.n[0] * self.n[1],
        }
    }

    fn coord(&self, c: usize, axis: usize) -> usize {
        match axis {
            0 => c % self.n[0],
            1 => (c / self.n[0]) % self.n[1],
            _ => c / (self.n[0] * self.n[1]),
        }
    }

    /// `out = A x`.
    pub fn apply(&self, x: &[f64], out: &mut [f64]) {
        for c in 0..self.diag.len() {
            if self.diag[c] == 0.0 {
                out[c] = 0.0;
                continue;
            }
            let mut sum = self.diag[c] * x[c];
            for axis in 0..3 {
                let s = self.stride(axis);
                let at = self.coord(c, axis);
                if at + 1 < self.n[axis] {
                    sum += self.plus[axis][c] * x[c + s];
                }
                if at > 0 {
                    sum += self.plus[axis][c - s] * x[c - s];
                }
            }
            out[c] = sum;
        }
    }

    /// Writes the MIC(0) inverse diagonal into `precon` (same length as [`Self::diag`]).
    pub fn mic0_into(&self, precon: &mut [f64]) {
        precon[..self.diag.len()].fill(0.0);
        for c in 0..self.diag.len() {
            if self.diag[c] == 0.0 {
                continue;
            }
            let mut e = self.diag[c];
            for axis in 0..3 {
                if self.coord(c, axis) == 0 {
                    continue;
                }
                let b = c - self.stride(axis);
                let a = self.plus[axis][b] * precon[b];
                e -= a * a;
                // The two other couplings of the lower neighbour, dropped from the factor and
                // compensated on the diagonal (the "modified" part).
                let mut other = 0.0;
                for (o, plus) in self.plus.iter().enumerate() {
                    if o != axis {
                        other += plus[b];
                    }
                }
                e -= TAU * self.plus[axis][b] * other * precon[b] * precon[b];
            }
            if e < SIGMA * self.diag[c] {
                e = self.diag[c];
            }
            // The software sqrt (ADR-4753): the preconditioner reaches every solve's iterates,
            // so a platform `sqrt` ulp would fork the pressure pins across platforms.
            precon[c] = 1.0 / crate::math::sqrt_f64(e);
        }
    }

    /// The MIC(0) factor's inverse diagonal.
    #[must_use]
    pub fn mic0(&self) -> Vec<f64> {
        let mut precon = vec![0.0; self.diag.len()];
        self.mic0_into(&mut precon);
        precon
    }

    /// `z = M^-1 r` with the factor `precon` (a forward and a backward substitution).
    pub fn precondition(&self, precon: &[f64], r: &[f64], q: &mut [f64], z: &mut [f64]) {
        for c in 0..self.diag.len() {
            if self.diag[c] == 0.0 {
                q[c] = 0.0;
                continue;
            }
            let mut t = r[c];
            for axis in 0..3 {
                if self.coord(c, axis) > 0 {
                    let b = c - self.stride(axis);
                    t -= self.plus[axis][b] * precon[b] * q[b];
                }
            }
            q[c] = t * precon[c];
        }
        for c in (0..self.diag.len()).rev() {
            if self.diag[c] == 0.0 {
                z[c] = 0.0;
                continue;
            }
            let mut t = q[c];
            for axis in 0..3 {
                if self.coord(c, axis) + 1 < self.n[axis] {
                    let f = c + self.stride(axis);
                    t -= self.plus[axis][c] * precon[c] * z[f];
                }
            }
            z[c] = t * precon[c];
        }
    }

    /// Solves `A p = b` into `p` (starting from zero) with at most `max_iterations` steps, stopping
    /// when the residual's infinity norm is at most `tolerance` times the right-hand side's.
    ///
    /// `work`'s vectors must already be `diag.len()` long. They are reused, not reallocated.
    pub(crate) fn solve_with(
        &self,
        b: &[f64],
        p: &mut [f64],
        work: &mut CgWorkspace,
        max_iterations: u32,
        tolerance: f64,
    ) -> SolveReport {
        let len = self.diag.len();
        p.iter_mut().for_each(|v| *v = 0.0);
        work.r.copy_from_slice(b);
        let norm_b = inf_norm(&work.r);
        if norm_b == 0.0 {
            return SolveReport {
                iterations: 0,
                residual: 0.0,
                converged: true,
            };
        }
        let target = tolerance * norm_b;
        self.mic0_into(&mut work.precon);
        self.precondition(&work.precon, &work.r, &mut work.q, &mut work.z);
        work.s.copy_from_slice(&work.z);
        let mut sigma = dot(&work.z, &work.r);
        let mut residual = norm_b;
        for iteration in 0..max_iterations {
            self.apply(&work.s, &mut work.z);
            let denom = dot(&work.z, &work.s);
            if denom == 0.0 || !denom.is_finite() {
                return SolveReport {
                    iterations: iteration,
                    residual,
                    converged: false,
                };
            }
            let alpha = sigma / denom;
            for (c, pressure) in p.iter_mut().enumerate().take(len) {
                *pressure += alpha * work.s[c];
                work.r[c] -= alpha * work.z[c];
            }
            residual = inf_norm(&work.r);
            if residual <= target {
                return SolveReport {
                    iterations: iteration + 1,
                    residual,
                    converged: true,
                };
            }
            self.precondition(&work.precon, &work.r, &mut work.q, &mut work.z);
            let sigma_new = dot(&work.z, &work.r);
            let beta = sigma_new / sigma;
            for c in 0..len {
                work.s[c] = work.z[c] + beta * work.s[c];
            }
            sigma = sigma_new;
        }
        SolveReport {
            iterations: max_iterations,
            residual,
            converged: false,
        }
    }

    /// Solves `A p = b` into `p` (starting from zero) with at most `max_iterations` steps, stopping
    /// when the residual's infinity norm is at most `tolerance` times the right-hand side's.
    pub fn solve(&self, b: &[f64], p: &mut [f64], max_iterations: u32, tolerance: f64) -> SolveReport {
        let len = self.diag.len();
        let mut work = CgWorkspace {
            r: vec![0.0; len],
            precon: vec![0.0; len],
            q: vec![0.0; len],
            z: vec![0.0; len],
            s: vec![0.0; len],
        };
        self.solve_with(b, p, &mut work, max_iterations, tolerance)
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn inf_norm(a: &[f64]) -> f64 {
    a.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1D Dirichlet Laplacian (a column of liquid under air) solves to the exact quadratic.
    #[test]
    fn column_laplacian_solves() {
        let n = 16;
        let mut a = PoissonMatrix::new([1, n, 1]);
        for j in 0..n {
            a.diag[j] = 2.0;
            if j + 1 < n {
                a.plus[1][j] = -1.0;
            }
        }
        // Floor: Neumann (one neighbour fewer on the diagonal).
        a.diag[0] = 1.0;
        let b = vec![1.0; n];
        let mut p = vec![0.0; n];
        let report = a.solve(&b, &mut p, 200, 1e-10);
        assert!(report.converged, "{report:?}");
        let mut ap = vec![0.0; n];
        a.apply(&p, &mut ap);
        for (x, y) in ap.iter().zip(&b) {
            assert!((x - y).abs() < 1e-8, "{x} vs {y}");
        }
    }
}
