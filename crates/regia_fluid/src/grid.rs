// SPDX-License-Identifier: MPL-2.0
//! The staggered MAC grid (Harlow and Welch 1965): pressure and cell flags at cell centres, each
//! velocity component on the faces normal to it.
//!
//! Local coordinates `q = (p - origin) / dx`: cell `(i, j, k)` covers `[i, i + 1)` on each axis. The
//! `u` sample of face `(i, j, k)` sits at `(i, j + 0.5, k + 0.5)`, `v` at `(i + 0.5, j, k + 0.5)` and `w`
//! at `(i + 0.5, j + 0.5, k)`. Every array is a plain `Vec` in `x`-fastest order.

use glam::Vec3;

/// Grid dimensions and placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridDims {
    /// Cells along x.
    pub nx: usize,
    /// Cells along y.
    pub ny: usize,
    /// Cells along z.
    pub nz: usize,
    /// Cell edge length in metres.
    pub dx: f32,
    /// World position of the grid's minimum corner.
    pub origin: Vec3,
}

impl GridDims {
    /// Number of cells.
    #[must_use]
    pub fn cells(&self) -> usize {
        self.nx * self.ny * self.nz
    }

    /// Flat index of cell `(i, j, k)`.
    #[must_use]
    pub fn cell(&self, i: usize, j: usize, k: usize) -> usize {
        i + self.nx * (j + self.ny * k)
    }

    /// The world maximum corner.
    #[must_use]
    pub fn max(&self) -> Vec3 {
        self.origin + Vec3::new(self.nx as f32, self.ny as f32, self.nz as f32) * self.dx
    }

    /// Local coordinates of world point `p`.
    #[must_use]
    pub fn local(&self, p: Vec3) -> Vec3 {
        (p - self.origin) / self.dx
    }

    /// World position of the centre of cell `(i, j, k)`.
    #[must_use]
    pub fn cell_center(&self, i: usize, j: usize, k: usize) -> Vec3 {
        self.origin + (Vec3::new(i as f32, j as f32, k as f32) + Vec3::splat(0.5)) * self.dx
    }

    /// The cell holding world point `p`, if it is inside the grid.
    #[must_use]
    pub fn cell_of(&self, p: Vec3) -> Option<(usize, usize, usize)> {
        let q = self.local(p);
        if !(q.x >= 0.0 && q.y >= 0.0 && q.z >= 0.0) {
            return None;
        }
        let (i, j, k) = (q.x as usize, q.y as usize, q.z as usize);
        (i < self.nx && j < self.ny && k < self.nz).then_some((i, j, k))
    }

    /// The `u`, `v` and `w` face arrays' dimensions.
    #[must_use]
    pub fn face_dims(&self, axis: usize) -> [usize; 3] {
        let mut d = [self.nx, self.ny, self.nz];
        d[axis] += 1;
        d
    }
}

/// One velocity component on its faces, with the sample offset of that component.
#[derive(Debug, Clone, PartialEq)]
pub struct FaceField {
    /// Size along x, y, z.
    pub size: [usize; 3],
    /// Sample offset in local units (0 on the component's own axis, 0.5 on the others).
    pub offset: Vec3,
    /// Values.
    pub data: Vec<f32>,
}

impl FaceField {
    /// A zero field for component `axis` of `dims`.
    #[must_use]
    pub fn new(dims: &GridDims, axis: usize) -> Self {
        let size = dims.face_dims(axis);
        let mut offset = Vec3::splat(0.5);
        offset[axis] = 0.0;
        Self {
            size,
            offset,
            data: vec![0.0; size[0] * size[1] * size[2]],
        }
    }

    /// Flat index of face `(i, j, k)`.
    #[must_use]
    pub fn idx(&self, i: usize, j: usize, k: usize) -> usize {
        i + self.size[0] * (j + self.size[1] * k)
    }

    /// The base face and the trilinear fractions of local point `q` (clamped into the field).
    #[must_use]
    pub fn stencil(&self, q: Vec3) -> ([usize; 3], [f32; 3]) {
        let s = q - self.offset;
        let mut base = [0usize; 3];
        let mut frac = [0.0f32; 3];
        for a in 0..3 {
            let hi = self.size[a].saturating_sub(2) as f32;
            let v = if s[a].is_finite() {
                s[a].clamp(0.0, hi.max(0.0))
            } else {
                0.0
            };
            let b = v.floor();
            base[a] = b as usize;
            frac[a] = if self.size[a] < 2 {
                0.0
            } else {
                (s[a] - b).clamp(0.0, 1.0)
            };
            if !frac[a].is_finite() {
                frac[a] = 0.0;
            }
        }
        (base, frac)
    }

    /// The eight (index, weight) pairs of the trilinear kernel at local point `q`.
    #[must_use]
    pub fn weights(&self, q: Vec3) -> [(usize, f32); 8] {
        let (b, f) = self.stencil(q);
        let mut out = [(0usize, 0.0f32); 8];
        let mut n = 0;
        for dk in 0..2 {
            let wk = if dk == 0 { 1.0 - f[2] } else { f[2] };
            let k = (b[2] + dk).min(self.size[2] - 1);
            for dj in 0..2 {
                let wj = if dj == 0 { 1.0 - f[1] } else { f[1] };
                let j = (b[1] + dj).min(self.size[1] - 1);
                for di in 0..2 {
                    let wi = if di == 0 { 1.0 - f[0] } else { f[0] };
                    let i = (b[0] + di).min(self.size[0] - 1);
                    out[n] = (self.idx(i, j, k), wi * wj * wk);
                    n += 1;
                }
            }
        }
        out
    }

    /// Trilinear sample at local point `q`.
    #[must_use]
    pub fn sample(&self, q: Vec3) -> f32 {
        let mut sum = 0.0;
        for (i, w) in self.weights(q) {
            sum += self.data[i] * w;
        }
        sum
    }
}

/// The three face fields of a velocity.
#[derive(Debug, Clone, PartialEq)]
pub struct MacVelocity {
    /// The three components.
    pub c: [FaceField; 3],
}

impl MacVelocity {
    /// A zero velocity on `dims`.
    #[must_use]
    pub fn new(dims: &GridDims) -> Self {
        Self {
            c: [
                FaceField::new(dims, 0),
                FaceField::new(dims, 1),
                FaceField::new(dims, 2),
            ],
        }
    }

    /// Trilinear sample of the velocity at local point `q`.
    #[must_use]
    pub fn sample(&self, q: Vec3) -> Vec3 {
        Vec3::new(self.c[0].sample(q), self.c[1].sample(q), self.c[2].sample(q))
    }
}

/// FNV-1a 64-bit, for state digests (no dependency, the same on every platform).
#[derive(Debug, Clone, Copy)]
pub struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self::new()
    }
}

impl Fnv1a {
    /// The offset basis.
    #[must_use]
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    /// Hashes `bytes`.
    pub fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// Hashes the bits of `v`.
    pub fn write_f32(&mut self, v: f32) {
        self.write(&v.to_bits().to_le_bytes());
    }

    /// The digest.
    #[must_use]
    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// xorshift64* (Vigna 2016): the seeded generator of reseeding, emission jitter and whitewater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed` (never zero inside).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut s = seed ^ 0x9e37_79b9_7f4a_7c15;
        if s == 0 {
            s = 1;
        }
        Self(s)
    }

    /// The generator's state word (the fluid digest hashes it, ADR-4753).
    #[must_use]
    pub(crate) fn state(&self) -> u64 {
        self.0
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A float in `[0, 1)` from 24 random bits.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A float in `[-1, 1)`.
    pub fn signed(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }
}
