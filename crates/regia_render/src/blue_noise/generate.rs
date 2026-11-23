// SPDX-License-Identifier: MPL-2.0
//! The spatiotemporal blue-noise generator (ADR-2075). Plain `std` and exact arithmetic only: it is
//! compiled into the build script too, and every host must produce the same bytes (no `exp` from
//! a platform maths library, no random state).
//!
//! Each of the [`SLICES`] slices is a [`SIZE`] × [`SIZE`] threshold mask whose every byte value
//! appears equally often. Void-and-cluster ranking (Ulichney, "The void-and-cluster method for
//! dither array generation", SPIE 1913, 1993) gives the blue-noise spectrum; the spatiotemporal
//! energy of Wolfe et al., "Spatiotemporal Blue Noise Masks", EGSR 2022 keeps it along time as
//! well: a voxel's energy counts the set voxels of its own slice (a toroidal Gaussian in x and y)
//! and of its own pixel in the other slices (a toroidal Gaussian in time).
//!
//! Each rank is placed once per slice, slice by slice. The first half of the ranks each go to the
//! lowest-energy voxel still free (the largest void). From the half on the free voxels are the
//! minority: the energy is rebuilt from them and each rank goes to the free voxel of highest
//! energy (the tightest cluster of what is left), Ulichney's phase 3. Ties break on a fixed hash
//! of the voxel, so the first rank is not a lattice.

/// Texels along each side of a slice.
pub const SIZE: usize = 64;
/// Slices along time.
pub const SLICES: usize = 16;
/// Bytes of the table: one per voxel, slice by slice, row by row.
pub const BYTES: usize = SIZE * SIZE * SLICES;
/// Bumped whenever [`generate`]'s output changes; names the build script's cached file.
pub const REVISION: u32 = 1;

/// Spatial and temporal σ², the paper's σ = 1.9.
const SIGMA2: f64 = 1.9 * 1.9;
/// The spatial kernel's radius: the Gaussian is below 1 % past it.
const RADIUS: usize = 6;
/// Energy of a voxel on itself: the fixed point of the integer energies.
const ONE: f64 = 65536.0;

/// `e^(−x)` for `x ≥ 0` from additions and multiplications alone, which IEEE 754 fixes on every
/// host: `(1 − x / 2¹⁰)^(2¹⁰)` by repeated squaring. A slightly different Gaussian than the true
/// exponential is immaterial; identical bytes everywhere are not.
fn exp_neg(x: f64) -> f64 {
    let mut v = 1.0 - x / 1024.0;
    if v <= 0.0 {
        return 0.0;
    }
    for _ in 0..10 {
        v *= v;
    }
    v
}

fn weight(d2: usize) -> u32 {
    (exp_neg(d2 as f64 / (2.0 * SIGMA2)) * ONE + 0.5) as u32
}

/// A fixed hash of a voxel index (the PCG output permutation of the index).
fn tie(i: usize) -> u32 {
    let state = (i as u32).wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

struct Volume {
    energy: Vec<u32>,
    spatial: Vec<(usize, usize, u32)>,
    temporal: Vec<(usize, u32)>,
}

impl Volume {
    fn new() -> Self {
        let mut spatial = Vec::new();
        let r = RADIUS as isize;
        for dy in -r..=r {
            for dx in -r..=r {
                let d2 = (dx * dx + dy * dy) as usize;
                let (wx, wy) = (dx.rem_euclid(SIZE as isize), dy.rem_euclid(SIZE as isize));
                spatial.push((wx as usize, wy as usize, weight(d2)));
            }
        }
        let mut temporal = Vec::new();
        for dz in 1..SLICES {
            let d = dz.min(SLICES - dz);
            temporal.push((dz, weight(d * d)));
        }
        Self {
            energy: vec![0; BYTES],
            spatial,
            temporal,
        }
    }

    /// Adds or removes voxel `i`'s contribution to everything it reaches.
    fn splat(&mut self, i: usize, add: bool) {
        let z = i / (SIZE * SIZE);
        let (y, x) = ((i % (SIZE * SIZE)) / SIZE, i % SIZE);
        let base = z * SIZE * SIZE;
        for &(dx, dy, w) in &self.spatial {
            let j = base + ((y + dy) % SIZE) * SIZE + (x + dx) % SIZE;
            self.energy[j] = if add {
                self.energy[j] + w
            } else {
                self.energy[j] - w
            };
        }
        for &(dz, w) in &self.temporal {
            let j = ((z + dz) % SLICES) * SIZE * SIZE + y * SIZE + x;
            self.energy[j] = if add {
                self.energy[j] + w
            } else {
                self.energy[j] - w
            };
        }
    }
}

/// The table: one byte per voxel, `rank >> 4`.
#[must_use]
pub fn generate() -> Vec<u8> {
    let area = SIZE * SIZE;
    let ties: Vec<u32> = (0..BYTES).map(tie).collect();
    let mut free = vec![true; BYTES];
    let mut out = vec![0u8; BYTES];
    let mut volume = Volume::new();
    let shift = (area / 256).trailing_zeros();
    for rank in 0..area / 2 {
        for z in 0..SLICES {
            let base = z * area;
            let mut best = usize::MAX;
            let mut key = (u32::MAX, u32::MAX);
            for i in base..base + area {
                if free[i] && (volume.energy[i], ties[i]) < key {
                    key = (volume.energy[i], ties[i]);
                    best = i;
                }
            }
            free[best] = false;
            out[best] = (rank >> shift) as u8;
            volume.splat(best, true);
        }
    }
    // The free voxels are the minority from here on: energy is theirs.
    volume.energy.fill(0);
    for (i, is_free) in free.clone().into_iter().enumerate() {
        if is_free {
            volume.splat(i, true);
        }
    }
    for rank in area / 2..area {
        for z in 0..SLICES {
            let base = z * area;
            let mut best = usize::MAX;
            let mut key = (0u32, 0u32);
            for i in base..base + area {
                if free[i] && (volume.energy[i], ties[i]) >= key {
                    key = (volume.energy[i], ties[i]);
                    best = i;
                }
            }
            free[best] = false;
            out[best] = (rank >> shift) as u8;
            volume.splat(best, false);
        }
    }
    out
}
