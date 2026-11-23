# Fluid pressure dense-oracle regression

`crates/regia_fluid/tests/poisson_dense_oracle.rs` checks the existing
`regia_fluid::pcg::PoissonMatrix` pressure primitive. It has no direct agent method:
custom matrices, iteration caps and individual pressure vectors are library inputs.
The existing [particle liquid methods](fluid.md) expose simulation-level pressure
statistics. The crate's `default` feature set is empty. This regression adds no runtime behavior or protocol surface.

Run the deterministic CPU tests:

```sh
cargo test -p regia_fluid --test poisson_dense_oracle
```

The tests construct 24 nonsquare grids, each with at most 12 active unknowns:
three orientations of 3 x 2 x 2, all six permutations of 3 x 2 x 1, and three
orientations of 4 x 3 x 1, each fully active and with holes. Positive diagonals
vary by coordinate; nonpositive neighbor coefficients vary by coordinate and axis.
Every active diagonal exceeds the absolute off-diagonal row sum by at least one.
The active principal block is symmetric positive definite. Inactive rows and all
incoming and outgoing couplings are zero; their RHS entries are zero.

The dense matrix is assembled by comparing coordinate pairs, independently of
compact storage, solver strides and `apply`. Every basis vector and a mixed-sign
known solution check every matrix entry, including wraparound boundaries and
inactive columns. The RHS is a dense matrix product. Original test-only Gaussian
elimination with partial pivoting solves the active principal block; a separate
row-swap example exercises pivot selection. Both dense and PCG solutions are
checked against the known solution. No production solver code is copied into the
oracle.

The solve checks recompute `||b - A p||inf` using the dense matrix and compare it
with `SolveReport.residual`. They validate convergence against the documented
RHS-relative target, enforce iteration caps, and test RHS/solution scales
`2^-16`, `1`, `2^16`. Exact binary scaling must preserve normalized iterates,
reported residuals and iteration counts. Caller output contents are discarded:
starting with the known solution must reproduce the solve starting with unrelated
values. Zero RHS converges in zero iterations even with a zero cap. Nonzero RHS
with a zero cap clears the output and reports its initial infinity norm without
converging. A one-step cap on the 3 x 2 x 2 fixture, with and without holes, reports
nonconvergence and the independently recomputed remaining residual.

## Numerical allowances

These checks use f64 throughout, small dyadic coefficients and moderate magnitudes.
The tolerances are fixed from arithmetic budgets, rather than observed test errors:

- Unit roundoff is `u = f64::EPSILON / 2 = 2^-53`; accumulation uses
  `gamma(k) = k*u / (1-k*u)`. Dense/apply comparisons allow
  `gamma(2*n) * ||A||inf * ||x||inf` for up to `n` multiply-add terms.
- With minimum strict diagonal-dominance margin `d`,
  `||A^-1||inf <= 1/d`: apply the row with maximal solution magnitude to prove it.
  The symmetric strictly dominant M-matrix active blocks have bounded elimination
  multipliers and no element growth. A conservative `gamma(8*n*n)` allowance
  covers elimination, substitution and dense-product rounding, scaled by
  `(||A||inf * ||x||inf + ||b||inf) / d`.
- The recursive PCG residual and a fresh dense product have different rounding.
  Their comparison allows `gamma(64*n*(iterations+1))` times
  `||A||inf * ||p||inf + ||b||inf`. The fixed per-step budget covers the
  short stencil products, reductions, preconditioner sweeps and vector updates
  for this bounded, well-conditioned fixture family (48-step maximum).
- The PCG stopping tolerance is `2^-30`. Solution error is bounded by
  `(tolerance * ||b||inf + residual_roundoff) / d + oracle_roundoff`.
  No absolute floor is added that would hide errors in the small-RHS cases.

This is a bounded regression allowance for these fixtures, not a new numerical
policy or a guarantee for arbitrary ill-conditioned systems. It does not test
unsupported malformed matrices or nonzero RHS values on inactive rows.

The oracle's sensitivity was checked with two temporary production mutations:
reversing a lower-neighbor sign failed the basis-vector and solve checks; replacing
the reported infinity norm with a Euclidean norm failed the independent residual
checks. Both mutations were restored before the final verification run.

## Existing agent smoke

Pressure solves are reached normally through `fluid.create`, `fluid.step` and
`fluid.stats`; the response includes `pcg_iterations`, `pcg_residual` and
`pcg_converged`. The existing handler tests exercise the happy path and errors:

```sh
cargo test -p regia_agent --lib fluid::tests::fluid_methods_round_trip -- --exact
cargo test -p regia_agent --lib fluid::tests::fluid_errors_have_reasons -- --exact
```

The happy-path test creates a dam break, steps it, reads stats, bakes and plays,
then destroys it. It checks `fluid_frame_out_of_range` and `fluid_not_found`.
The error test checks `fluid_bad_bounds`, `fluid_unknown_preset`,
`fluid_too_many_cells`, `fluid_not_found`, `fluid_no_cache`, `fluid_bad_param`
and `path_outside_project`. These lowercase strings are `data.reason`; the
existing RPC envelope uses `F0002` or `F0003` in `data.code`.

These CPU regressions do not establish physical platform or performance passes,
GPU agreement, hydrostatic accuracy, or a new fluid model.
