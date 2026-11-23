// SPDX-License-Identifier: MPL-2.0
//! The two pinned unit conversions of spec 121 § 3 (ADR-8084, review W9).
//!
//! Cooked geometry is `i32` millimetres; the run time works in `f32` metres. These two functions are
//! the only ways a number crosses between them on the lockstep path, so every target gets the same
//! bits:
//!
//! - [`mm_to_m`]: one exact integer-to-float conversion of the `i64` world sum, then one IEEE
//!   division by 1000. The result depends only on the world millimetre, so a tile-local vertex and a
//!   lattice vertex at the same world millimetre get equal `f32` bits.
//! - [`m_to_mm`]: the exact `f64` product by 1000, IEEE round-ties-to-even, then Rust's saturating
//!   `as i32` cast (NaN maps to 0).
//!
//! Pinned property (`m_to_mm_inverts_mm_to_m_below_2_23`): `m_to_mm(mm_to_m(0, v)) == v` for every
//! `|v| < 2^23` mm.

/// The side of a cooked tile, millimetres (1024 m; spec 121 § 3). Storage only: no answer depends on
/// it (DO27 sign-off E3).
pub const TILE_MM: i32 = 1_024_000;

/// The largest world coordinate a cooked map may use, millimetres: below this every local and world
/// coordinate converts to `f32` exactly (spec 121 § 3, `geom_extent_too_large`).
pub const MAX_EXTENT_MM: i64 = 1 << 24;

/// World millimetres (`origin_mm + local_mm`) to `f32` metres: `((origin + local) as i64 as f32) /
/// 1000.0` (spec 121 § 3). The sum must be below 2^24 in magnitude for the conversion to be exact;
/// the cook refuses larger maps (`geom_extent_too_large`).
#[inline]
#[must_use]
pub fn mm_to_m(origin_mm: i32, local_mm: i32) -> f32 {
    let world = origin_mm as i64 + local_mm as i64;
    (world as f32) / 1000.0
}

/// `f32` metres to `i32` millimetres: `((m as f64) * 1000.0).round_ties_even() as i32` (spec 121 §
/// 3). The `f64` product is exact, the rounding is IEEE `roundToIntegralTiesToEven`, and the cast
/// saturates at `i32::MIN` and `i32::MAX` and maps NaN to 0. No other rounding is allowed on the
/// lockstep path.
#[inline]
#[must_use]
pub fn m_to_mm(m: f32) -> i32 {
    ((m as f64) * 1000.0).round_ties_even() as i32
}

/// Splits a world millimetre coordinate into its cooked tile origin (a multiple of [`TILE_MM`],
/// floor division) and the local offset in `0..TILE_MM`.
#[inline]
#[must_use]
pub fn split_tile(world_mm: i32) -> (i32, i32) {
    let tile = world_mm.div_euclid(TILE_MM);
    (tile * TILE_MM, world_mm.rem_euclid(TILE_MM))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 121 § 3 (W-06): the round trip is the identity for every `|v| < 2^23` mm, exhaustively.
    /// Failed first on a truncating `m_to_mm` (`(m * 1000.0) as i32`).
    #[test]
    fn m_to_mm_inverts_mm_to_m_below_2_23() {
        let limit: i32 = 1 << 23;
        let mut bad = 0u64;
        let mut first = None;
        for v in (1 - limit)..limit {
            let back = m_to_mm(mm_to_m(0, v));
            if back != v {
                bad += 1;
                first.get_or_insert((v, back));
            }
        }
        assert_eq!(bad, 0, "first mismatch (v, back) = {first:?}");
    }

    #[test]
    fn mm_to_m_depends_only_on_the_world_sum() {
        for (origin, local) in [
            (TILE_MM, 12_345),
            (-TILE_MM, 1_036_345),
            (2 * TILE_MM, -1_011_655),
        ] {
            assert_eq!(
                mm_to_m(origin, local).to_bits(),
                mm_to_m(0, origin + local).to_bits(),
                "{origin} + {local}"
            );
        }
    }

    #[test]
    fn m_to_mm_rounds_ties_to_even_and_saturates() {
        assert_eq!(m_to_mm(0.0), 0);
        assert_eq!(m_to_mm(-0.0), 0);
        // 0.0005 m is just above a tie in f32 (0.000500000024 m): rounds up.
        assert_eq!(m_to_mm(0.0005), 1);
        // The f32 nearest 0.0025 m is 0.00249999994 m: 2.49999994 mm rounds to 2.
        assert_eq!(m_to_mm(0.0025), 2);
        // Exact ties after the exact f64 product (1/16 m = 62.5 mm, 3/16 m = 187.5 mm) go to even.
        assert_eq!(m_to_mm(0.0625), 62);
        assert_eq!(m_to_mm(0.1875), 188);
        assert_eq!(m_to_mm(-0.0625), -62);
        assert_eq!(m_to_mm(f32::NAN), 0);
        assert_eq!(m_to_mm(f32::INFINITY), i32::MAX);
        assert_eq!(m_to_mm(f32::NEG_INFINITY), i32::MIN);
        assert_eq!(m_to_mm(1e12), i32::MAX);
        assert_eq!(m_to_mm(-1e12), i32::MIN);
        assert_eq!(m_to_mm(-1.2345), -1235);
    }

    #[test]
    fn split_tile_floors_toward_negative_infinity() {
        assert_eq!(split_tile(0), (0, 0));
        assert_eq!(split_tile(TILE_MM + 5), (TILE_MM, 5));
        assert_eq!(split_tile(-1), (-TILE_MM, TILE_MM - 1));
        assert_eq!(split_tile(-TILE_MM), (-TILE_MM, 0));
    }
}
