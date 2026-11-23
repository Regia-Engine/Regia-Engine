// SPDX-License-Identifier: MPL-2.0
//! A scalar quaternion for placement paths (spec 62 § 6.1).
//!
//! glam's `Quat` is SIMD-backed on SSE2 and NEON, and the two backends are free to order the
//! multiply-adds of a product differently, which can change the last bit. Placement must give the
//! same bytes on x86_64 and aarch64, so the evaluator uses this plain `[x, y, z, w]` type: every
//! operation is written out in scalar f32 in one fixed order (Hamilton product, textbook).

use std::ops::Mul;

use regia_core::math::{Mat3, Vec3};

/// A rotation quaternion `[x, y, z, w]` with scalar f32 arithmetic in a fixed order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rot(pub [f32; 4]);

impl Rot {
    /// No rotation.
    pub const IDENTITY: Rot = Rot([0.0, 0.0, 0.0, 1.0]);

    /// From components.
    pub const fn from_xyzw(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self([x, y, z, w])
    }

    /// The components `[x, y, z, w]`.
    pub fn to_array(self) -> [f32; 4] {
        self.0
    }

    /// Euclidean length.
    pub fn length(self) -> f32 {
        let [x, y, z, w] = self.0;
        (x * x + y * y + z * z + w * w).sqrt()
    }

    /// Unit length (identity when degenerate or non-finite).
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len.is_finite() && len > 1e-12 {
            let [x, y, z, w] = self.0;
            Self([x / len, y / len, z / len, w / len])
        } else {
            Self::IDENTITY
        }
    }

    /// The inverse of a unit quaternion (its conjugate).
    pub fn inverse(self) -> Self {
        let [x, y, z, w] = self.0;
        Self([-x, -y, -z, w])
    }

    /// The rotation matrix (columns are the rotated axes).
    pub fn to_mat3(self) -> Mat3 {
        Mat3::from_cols(self * Vec3::X, self * Vec3::Y, self * Vec3::Z)
    }
}

impl Mul for Rot {
    type Output = Rot;
    /// Hamilton product `self · rhs` (apply `rhs` first).
    fn mul(self, rhs: Rot) -> Rot {
        let [ax, ay, az, aw] = self.0;
        let [bx, by, bz, bw] = rhs.0;
        Rot([
            aw * bx + ax * bw + ay * bz - az * by,
            aw * by - ax * bz + ay * bw + az * bx,
            aw * bz + ax * by - ay * bx + az * bw,
            aw * bw - ax * bx - ay * by - az * bz,
        ])
    }
}

impl Mul<Vec3> for Rot {
    type Output = Vec3;
    /// Rotates a vector: `v + w·t + u × t` with `t = 2 (u × v)`.
    fn mul(self, v: Vec3) -> Vec3 {
        let [x, y, z, w] = self.0;
        let (ux, uy, uz) = (x, y, z);
        let tx = 2.0 * (uy * v.z - uz * v.y);
        let ty = 2.0 * (uz * v.x - ux * v.z);
        let tz = 2.0 * (ux * v.y - uy * v.x);
        Vec3::new(
            v.x + w * tx + (uy * tz - uz * ty),
            v.y + w * ty + (uz * tx - ux * tz),
            v.z + w * tz + (ux * ty - uy * tx),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rotation_it_stands_for() {
        // 90 degrees about Y: X → −Z.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let q = Rot::from_xyzw(0.0, s, 0.0, s);
        let v = q * Vec3::X;
        assert!((v - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-6, "{v:?}");
        let back = q.inverse() * v;
        assert!((back - Vec3::X).length() < 1e-6);
        let qq = q * q; // 180 degrees
        assert!((qq * Vec3::X - Vec3::new(-1.0, 0.0, 0.0)).length() < 1e-6);
        assert_eq!(Rot([0.0; 4]).normalize(), Rot::IDENTITY);
    }
}
