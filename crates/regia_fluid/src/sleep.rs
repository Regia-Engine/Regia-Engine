// SPDX-License-Identifier: MPL-2.0
//! Skip fluid cells that have stayed still (ADR-3949).
//!
//! The caller owns a dense grid of speeds, one `f32` per cell, in the same x-fastest order as
//! [`crate::grid::GridDims::cell`]: flat index `i + nx * (j + ny * k)`. This module does not read
//! the solver. A cell whose speed stays strictly under `epsilon` for `k` steps is asleep. A speed
//! at or above `epsilon`, or a 6-connected neighbor that is awake because of its own speed, wakes
//! the cell for this step. The wake is one hop: a cell woken only by a neighbor does not wake
//! its other neighbors in the same step, and a neighbor wake does not reset the quiet count.
//!
//! `k == 0` sleeps nobody. Every cell is listed.
//!
//! The solver does not call this yet (`sim.rs` belongs to the fluid lanes). After `mark_liquid`
//! in `FluidSim::substep`, the lead inserts one line once those lanes have merged:
//!
//! ```text
//! let sleep = crate::sleep::cells_to_integrate(self.dims.nx, self.dims.ny, self.dims.nz, &cell_speed, &cell_still, epsilon, k);
//! ```
//!
//! `cell_speed` is one speed per cell (the magnitude sampled at the cell center). `cell_still` is
//! empty on the first step and `sleep.still` after that. `epsilon` and `k` default to
//! [`DEFAULT_EPSILON`] and [`DEFAULT_QUIET_STEPS`] when the caller has no preference. Cells to
//! integrate this step are `sleep.integrate`, in ascending index order.

use crate::grid::Fnv1a;
use crate::sim::MAX_CELLS;

/// Speed strictly under this (m/s) counts as quiet when the caller does not pass an epsilon.
pub const DEFAULT_EPSILON: f32 = 1.0e-3;

/// Quiet steps before a cell may sleep when the caller does not pass `k`.
pub const DEFAULT_QUIET_STEPS: u32 = 4;

/// Why a sleep step was refused. [`SleepError::code`] is the stable protocol code
/// (`book/agents/fluid.md`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SleepError {
    /// `speeds.len()` is not `nx * ny * nz`, the product does not fit in `usize`, or `prev_still`
    /// is non-empty and a different length.
    #[error("fluid grid {nx}*{ny}*{nz} does not match speeds length {speeds_len} (quiet counts {still_len})")]
    BadGrid {
        /// Cells along x.
        nx: usize,
        /// Cells along y.
        ny: usize,
        /// Cells along z.
        nz: usize,
        /// `speeds.len()`.
        speeds_len: usize,
        /// `prev_still.len()` (0 when the caller passed an empty slice).
        still_len: usize,
    },
    /// The grid is larger than the solver cap of 64³ (`MAX_CELLS`).
    #[error("the grid would have {cells} cells (most {max})")]
    TooManyCells {
        /// Requested cells.
        cells: usize,
        /// The cap.
        max: usize,
    },
}

impl SleepError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadGrid { .. } => "fluid_sleep_bad_grid",
            Self::TooManyCells { .. } => "fluid_too_many_cells",
        }
    }
}

/// One sleep step: which cells to integrate, and the quiet counts to pass in next time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SleepStep {
    /// Flat cell indices to integrate this step, ascending.
    pub integrate: Vec<u32>,
    /// Quiet-step count per cell after this step, x-fastest, same length as `speeds`.
    pub still: Vec<u32>,
    /// FNV-1a of the dimensions, thresholds, integrate list and quiet counts.
    ///
    /// Two calls with the same inputs return the same digest on every platform.
    pub digest: u64,
}

/// Cells to integrate this step.
///
/// `speeds[i]` is the speed of flat cell `i`. A finite speed strictly under `epsilon` adds one
/// quiet step; any other speed (at or above `epsilon`, or not finite) resets that cell to zero.
/// `prev_still` is the quiet counts from the previous step, or empty (treated as all zeros) on
/// the first step. A non-empty `prev_still` of a different length is [`SleepError::BadGrid`].
///
/// The inputs are not modified. The `Result` must be handled: a bad grid is
/// [`SleepError::BadGrid`].
pub fn cells_to_integrate(
    nx: usize,
    ny: usize,
    nz: usize,
    speeds: &[f32],
    prev_still: &[u32],
    epsilon: f32,
    k: u32,
) -> Result<SleepStep, SleepError> {
    let cells = match nx.checked_mul(ny).and_then(|n| n.checked_mul(nz)) {
        Some(n) => n,
        None => {
            return Err(SleepError::BadGrid {
                nx,
                ny,
                nz,
                speeds_len: speeds.len(),
                still_len: prev_still.len(),
            });
        }
    };
    if speeds.len() != cells || (!prev_still.is_empty() && prev_still.len() != cells) {
        return Err(SleepError::BadGrid {
            nx,
            ny,
            nz,
            speeds_len: speeds.len(),
            still_len: prev_still.len(),
        });
    }
    if cells > MAX_CELLS {
        return Err(SleepError::TooManyCells {
            cells,
            max: MAX_CELLS,
        });
    }

    let _span = tracing::info_span!(target: "regia_fluid", "fluid_sleep", cells, k).entered();

    let mut still = vec![0u32; cells];
    if !prev_still.is_empty() {
        still.copy_from_slice(prev_still);
    }
    // 1 = awake from its own speed history. A neighbor wake is not stored here.
    let mut own = vec![0u8; cells];
    for (idx, speed) in speeds.iter().copied().enumerate() {
        let quiet = speed.is_finite() && speed < epsilon;
        if quiet {
            still[idx] = still[idx].saturating_add(1);
        } else {
            still[idx] = 0;
        }
        if k == 0 || still[idx] < k {
            own[idx] = 1;
        }
    }

    // Count first so the integrate list is one exact allocation. Pushing as we go grows by
    // powers of two and copies the awake set (up to 64³ cells) on the way.
    let mut n_integrate = 0usize;
    if nx > 0 && ny > 0 {
        for idx in 0..cells {
            if own[idx] != 0 || neighbor_awake(&own, idx, nx, ny, nz) {
                n_integrate += 1;
            }
        }
    }
    let mut integrate = Vec::new();
    integrate.reserve_exact(n_integrate);
    if nx > 0 && ny > 0 {
        for idx in 0..cells {
            if own[idx] != 0 || neighbor_awake(&own, idx, nx, ny, nz) {
                let Ok(index) = u32::try_from(idx) else {
                    return Err(SleepError::BadGrid {
                        nx,
                        ny,
                        nz,
                        speeds_len: speeds.len(),
                        still_len: prev_still.len(),
                    });
                };
                integrate.push(index);
            }
        }
    }

    let digest = digest_of(nx, ny, nz, epsilon, k, &integrate, &still);
    Ok(SleepStep {
        integrate,
        still,
        digest,
    })
}

/// Whether a 6-connected neighbor is awake from its own speed.
fn neighbor_awake(own: &[u8], idx: usize, nx: usize, ny: usize, nz: usize) -> bool {
    let i = idx % nx;
    let t = idx / nx;
    let j = t % ny;
    let k = t / ny;
    let plane = nx.saturating_mul(ny);
    (i > 0 && own[idx - 1] != 0)
        || (i + 1 < nx && own[idx + 1] != 0)
        || (j > 0 && own[idx - nx] != 0)
        || (j + 1 < ny && own[idx + nx] != 0)
        || (k > 0 && plane > 0 && own[idx - plane] != 0)
        || (k + 1 < nz && idx + plane < own.len() && own[idx + plane] != 0)
}

fn digest_of(nx: usize, ny: usize, nz: usize, epsilon: f32, k: u32, integrate: &[u32], still: &[u32]) -> u64 {
    let mut hash = Fnv1a::new();
    hash.write(&1u32.to_le_bytes());
    hash.write(&(nx as u64).to_le_bytes());
    hash.write(&(ny as u64).to_le_bytes());
    hash.write(&(nz as u64).to_le_bytes());
    hash.write(&k.to_le_bytes());
    hash.write(&epsilon.to_bits().to_le_bytes());
    hash.write(&(integrate.len() as u64).to_le_bytes());
    for cell in integrate {
        hash.write(&cell.to_le_bytes());
    }
    hash.write(&(still.len() as u64).to_le_bytes());
    for count in still {
        hash.write(&count.to_le_bytes());
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1.0e-3;
    const N: usize = 4;

    fn zeros() -> Vec<f32> {
        vec![0.0; N * N * N]
    }

    fn index(i: usize, j: usize, k: usize) -> usize {
        i + N * (j + N * k)
    }

    fn run(speeds: &[f32], k: u32, steps: u32) -> SleepStep {
        let mut prev = Vec::new();
        let mut last = None;
        for _ in 0..steps {
            let step = cells_to_integrate(N, N, N, speeds, &prev, EPS, k).expect("grid matches");
            prev.clone_from(&step.still);
            last = Some(step);
        }
        last.expect("at least one step")
    }

    #[test]
    fn still_grid_sleeps_after_k_steps() {
        let speeds = zeros();
        let after_two = run(&speeds, 3, 2);
        assert_eq!(after_two.integrate.len(), N * N * N, "two quiet steps stay awake");
        let after_three = run(&speeds, 3, 3);
        assert!(
            after_three.integrate.is_empty(),
            "after 3 steps the integrate list is empty"
        );
        assert!(after_three.still.iter().all(|c| *c == 3));
    }

    #[test]
    fn a_fast_cell_wakes_itself_and_its_six_neighbors() {
        let mut speeds = zeros();
        let asleep = run(&speeds, 3, 3);
        assert!(asleep.integrate.is_empty());
        let cell = index(1, 1, 1);
        speeds[cell] = EPS + 1.0;
        let step = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("grid");
        let mut expect = vec![
            index(1, 1, 0),
            index(1, 0, 1),
            index(0, 1, 1),
            cell,
            index(2, 1, 1),
            index(1, 2, 1),
            index(1, 1, 2),
        ];
        expect.sort_unstable();
        let got: Vec<usize> = step.integrate.iter().map(|c| *c as usize).collect();
        assert_eq!(got, expect, "only the fast cell and its 6 neighbors");
        assert_eq!(step.still[cell], 0);
        assert_eq!(
            step.still[index(0, 1, 1)],
            4,
            "a neighbor wake does not reset the count"
        );
    }

    #[test]
    fn a_face_cell_does_not_wrap() {
        let mut speeds = zeros();
        let asleep = run(&speeds, 3, 3);
        let cell = index(0, 1, 1);
        speeds[cell] = 1.0;
        let step = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("grid");
        let mut expect = vec![
            index(0, 1, 0),
            index(0, 0, 1),
            cell,
            index(1, 1, 1),
            index(0, 2, 1),
            index(0, 1, 2),
        ];
        expect.sort_unstable();
        let got: Vec<usize> = step.integrate.iter().map(|c| *c as usize).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn distant_fast_cells_do_not_wake_the_middle() {
        let mut speeds = zeros();
        let asleep = run(&speeds, 3, 3);
        speeds[index(0, 0, 0)] = 1.0;
        speeds[index(3, 3, 3)] = 1.0;
        let step = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("grid");
        assert_eq!(step.integrate.len(), 8);
        assert!(!step.integrate.contains(&(index(1, 1, 1) as u32)));
    }

    #[test]
    fn speed_at_epsilon_wakes_and_just_under_stays_quiet() {
        let mut speeds = zeros();
        let asleep = run(&speeds, 3, 3);
        speeds[index(1, 1, 1)] = EPS;
        let at = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("grid");
        assert_eq!(at.integrate.len(), 7);
        speeds[index(1, 1, 1)] = 0.0005;
        let under = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("grid");
        assert!(under.integrate.is_empty());
    }

    #[test]
    fn a_non_finite_speed_wakes() {
        let mut speeds = zeros();
        let asleep = run(&speeds, 3, 3);
        speeds[index(1, 1, 1)] = f32::NAN;
        let step = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("grid");
        assert_eq!(step.integrate.len(), 7);
    }

    #[test]
    fn bad_length_is_fluid_sleep_bad_grid() {
        let err = cells_to_integrate(2, 2, 2, &[0.0, 0.0, 0.0], &[], EPS, 3).expect_err("short");
        assert_eq!(err.code(), "fluid_sleep_bad_grid");
        let err = cells_to_integrate(2, 2, 2, &[0.0; 8], &[0, 0, 0], EPS, 3).expect_err("still");
        assert_eq!(err.code(), "fluid_sleep_bad_grid");
    }

    #[test]
    fn two_runs_share_a_digest() {
        let speeds = zeros();
        let a = run(&speeds, 3, 3);
        let b = run(&speeds, 3, 3);
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.integrate, b.integrate);
        assert_eq!(a.still, b.still);
        let mut moved = speeds.clone();
        moved[0] = 1.0;
        let c = run(&moved, 3, 3);
        assert_ne!(a.digest, c.digest);
    }

    #[test]
    fn k_zero_sleeps_nobody() {
        let speeds = zeros();
        let step = run(&speeds, 0, 3);
        assert_eq!(step.integrate.len(), N * N * N);
        assert!(step.still.iter().all(|c| *c == 3), "quiet counts still advance");
        let again = run(&speeds, 0, 3);
        assert_eq!(step.digest, again.digest);
    }

    #[test]
    fn the_halo_does_not_grow_on_later_quiet_steps() {
        let mut speeds = zeros();
        let asleep = run(&speeds, 3, 3);
        speeds[index(1, 1, 1)] = 1.0;
        let woken = cells_to_integrate(N, N, N, &speeds, &asleep.still, EPS, 3).expect("wake");
        speeds[index(1, 1, 1)] = 0.0;
        let next = cells_to_integrate(N, N, N, &speeds, &woken.still, EPS, 3).expect("quiet");
        assert_eq!(next.integrate, woken.integrate, "the same seven cells, one hop");
        // The fast cell's quiet count is 1 here. Two more quiet steps bring it to k, and the halo sleeps.
        let mut prev = next.still.clone();
        let mut last = next;
        for _ in 0..2 {
            last = cells_to_integrate(N, N, N, &speeds, &prev, EPS, 3).expect("quiet");
            prev.clone_from(&last.still);
        }
        assert!(
            last.integrate.is_empty(),
            "three quiet steps after the wake, the cell sleeps again"
        );
    }

    #[test]
    fn inputs_are_not_mutated_and_an_empty_history_matches_zeros() {
        let speeds = zeros();
        let before = speeds.clone();
        let history = vec![0u32; speeds.len()];
        let history_before = history.clone();
        let from_empty = cells_to_integrate(N, N, N, &speeds, &[], EPS, 3).expect("empty");
        let from_zeros = cells_to_integrate(N, N, N, &speeds, &history, EPS, 3).expect("zeros");
        assert_eq!(speeds, before);
        assert_eq!(history, history_before);
        assert_eq!(from_empty.integrate, from_zeros.integrate);
        assert_eq!(from_empty.still, from_zeros.still);
        assert_eq!(from_empty.digest, from_zeros.digest);
    }

    #[test]
    fn a_product_that_does_not_fit_is_a_bad_grid() {
        let err = cells_to_integrate(usize::MAX, 2, 2, &[], &[], EPS, 1).expect_err("overflow");
        assert_eq!(err.code(), "fluid_sleep_bad_grid");
    }

    #[test]
    fn too_many_cells_is_fluid_too_many_cells() {
        let cells = 512 * 512 * 2;
        assert!(cells > MAX_CELLS);
        let speeds = vec![0.0f32; cells];
        let err = cells_to_integrate(512, 512, 2, &speeds, &[], EPS, 3).expect_err("cap");
        assert_eq!(err.code(), "fluid_too_many_cells");
        assert!(matches!(
            err,
            SleepError::TooManyCells { cells: n, max } if n == cells && max == MAX_CELLS
        ));
    }

    #[test]
    fn a_non_cubic_grid_uses_each_axis_stride() {
        let mut speeds = vec![0.0; 3 * 4 * 5];
        let asleep = cells_to_integrate(3, 4, 5, &speeds, &[3; 60], EPS, 3).expect("grid");
        assert!(asleep.integrate.is_empty());
        // (1, 2, 3): x stride 1, y stride 3, z stride 12.
        speeds[43] = EPS;
        let woken = cells_to_integrate(3, 4, 5, &speeds, &asleep.still, EPS, 3).expect("grid");
        assert_eq!(woken.integrate, vec![31, 40, 42, 43, 44, 46, 55]);
    }

    #[test]
    fn saturated_quiet_counts_stay_asleep_until_the_cell_moves() {
        let still = [u32::MAX; 4];
        let asleep = cells_to_integrate(1, 1, 4, &[0.0; 4], &still, EPS, u32::MAX).expect("grid");
        assert!(asleep.integrate.is_empty());
        assert_eq!(asleep.still, still);
        // A thin grid has only z neighbors; saturation must not prevent a real wake.
        let woken =
            cells_to_integrate(1, 1, 4, &[0.0, EPS, 0.0, 0.0], &asleep.still, EPS, u32::MAX).expect("grid");
        assert_eq!(woken.integrate, vec![0, 1, 2]);
        assert_eq!(woken.still, vec![u32::MAX, 0, u32::MAX, u32::MAX]);
    }

    #[test]
    fn an_empty_grid_integrates_nothing() {
        let step = cells_to_integrate(0, 1, 1, &[], &[], EPS, 3).expect("empty product");
        assert!(step.integrate.is_empty());
        assert!(step.still.is_empty());
    }

    /// The integrate list is one exact allocation: geometric growth copies the awake set on the
    /// way up to 64³ cells (the per-step scan the solver will call).
    #[test]
    fn integrate_list_is_one_exact_allocation() {
        let awake = cells_to_integrate(5, 4, 4, &[1.0; 80], &[], EPS, 0).expect("grid");
        assert_eq!(awake.integrate.len(), 80);
        assert_eq!(
            awake.integrate.capacity(),
            awake.integrate.len(),
            "geometric growth left capacity {}",
            awake.integrate.capacity()
        );
        let asleep = cells_to_integrate(5, 4, 4, &[0.0; 80], &[3; 80], EPS, 3).expect("grid");
        assert!(asleep.integrate.is_empty());
        assert_eq!(asleep.integrate.capacity(), 0);
    }
}
