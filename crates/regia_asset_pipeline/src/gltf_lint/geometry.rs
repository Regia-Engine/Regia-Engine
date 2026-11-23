// SPDX-License-Identifier: MPL-2.0
//! Coincident triangle diagnostic (DO27 requirements, d6ed37677, ADR-8068).
//! Kept read-only: the caller decides what to do with a reported pair.

/// Numeric policy in glTF source units (meters) and UV coordinates.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeometrySettings {
    /// Position quantization grid, in meters.
    pub quantization: f64,
    /// Exclusive maximum matched-corner separation, in meters.
    pub separation: f64,
    /// Exclusive separation below which UVs do not matter.
    pub exact_separation: f64,
    /// Exclusive mean UV-corner distance for matching shading.
    pub uv_distance: f64,
    /// Exclusive anisotropy ratio between the suspect and keeper.
    pub anisotropy_ratio: f64,
    /// Exclusive minimum suspect anisotropy.
    pub anisotropy_floor: f64,
}

impl Default for GeometrySettings {
    fn default() -> Self {
        Self {
            quantization: 1e-4,
            separation: 1e-4,
            exact_separation: 1e-6,
            uv_distance: 0.02,
            anisotropy_ratio: 2.0,
            anisotropy_floor: 3.0,
        }
    }
}

/// A suspect triangle and the preferred copy, as zero-based triangle indices.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CoincidentPair {
    /// Less anisotropic triangle (earlier index breaks ties).
    pub keeper: usize,
    /// Triangle that overlaps the keeper.
    pub suspect: usize,
}

/// Reports coincident triangles without changing the positions, UVs or indices.
///
/// # Errors
/// Invalid settings, non-finite coordinates, mismatched UV counts, invalid indices,
/// or coordinates outside the quantization range.
pub fn coincident_pairs(
    positions: &[[f32; 3]],
    uvs: Option<&[[f32; 2]]>,
    triangles: &[[u32; 3]],
    settings: &GeometrySettings,
) -> Result<Vec<CoincidentPair>, String> {
    settings.validate()?;
    if positions.iter().flatten().any(|v| !v.is_finite())
        || uvs.is_some_and(|uv| uv.len() != positions.len() || uv.iter().flatten().any(|v| !v.is_finite()))
    {
        return Err("positions and UVs must be finite and have matching counts".into());
    }
    if triangles.iter().flatten().any(|&i| i as usize >= positions.len()) {
        return Err("triangle index is outside the position accessor".into());
    }
    let quantized = positions
        .iter()
        .map(|p| {
            let mut q = [0_i64; 3];
            for (i, &v) in p.iter().enumerate() {
                let v = (f64::from(v) / settings.quantization).round_ties_even();
                if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&v) {
                    return Err("position is outside the quantization range".to_string());
                }
                q[i] = v as i64;
            }
            Ok(q)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut groups = std::collections::BTreeMap::<[[i64; 3]; 3], Vec<(f64, usize)>>::new();
    for (i, &triangle) in triangles.iter().enumerate() {
        let mut key = triangle.map(|v| quantized[v as usize]);
        key.sort();
        let p = triangle.map(|v| positions[v as usize].map(f64::from));
        let uv = uvs.map(|uv| triangle.map(|v| uv[v as usize].map(f64::from)));
        groups.entry(key).or_default().push((anisotropy(p, uv), i));
    }
    let mut pairs = Vec::new();
    for members in groups.values_mut().filter(|g| g.len() > 1) {
        members.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let (keeper_aniso, keeper) = members[0];
        let a = triangles[keeper];
        for &(aniso, suspect) in &members[1..] {
            let b = triangles[suspect];
            let mut used = [false; 3];
            let mut permutation = [0; 3];
            for (i, &av) in a.iter().enumerate() {
                let Some(j) =
                    (0..3).find(|&j| !used[j] && quantized[av as usize] == quantized[b[j] as usize])
                else {
                    return Err("internal quantized corner mismatch".into());
                };
                used[j] = true;
                permutation[i] = b[j] as usize;
            }
            let separation = a
                .iter()
                .zip(permutation)
                .map(|(&av, bv)| {
                    distance(
                        positions[av as usize].map(f64::from),
                        positions[bv].map(f64::from),
                    )
                })
                .fold(0.0_f64, f64::max);
            if separation >= settings.separation {
                continue;
            }
            let uv_match = uvs.is_some_and(|uv| {
                a.iter()
                    .zip(permutation)
                    .map(|(&av, bv)| distance(uv[av as usize].map(f64::from), uv[bv].map(f64::from)))
                    .sum::<f64>()
                    / 3.0
                    < settings.uv_distance
            });
            let stretched = uvs.is_some()
                && aniso > settings.anisotropy_floor
                && aniso / keeper_aniso.max(1e-9) > settings.anisotropy_ratio;
            if separation < settings.exact_separation || uv_match || stretched {
                pairs.push(CoincidentPair { keeper, suspect });
            }
        }
    }
    pairs.sort_by_key(|p| (p.suspect, p.keeper));
    Ok(pairs)
}

impl GeometrySettings {
    /// Checks that tolerances are finite, positive and internally consistent.
    ///
    /// # Errors
    /// A threshold is non-finite, non-positive, or exact separation exceeds separation.
    pub fn validate(&self) -> Result<(), String> {
        if [
            self.quantization,
            self.separation,
            self.exact_separation,
            self.uv_distance,
            self.anisotropy_ratio,
            self.anisotropy_floor,
        ]
        .iter()
        .any(|v| !v.is_finite() || *v <= 0.0)
            || self.exact_separation > self.separation
            || self.anisotropy_ratio < 1.0
            || self.anisotropy_floor < 1.0
        {
            return Err("lint thresholds must be finite and positive; exact_separation <= separation; anisotropy thresholds >= 1".into());
        }
        Ok(())
    }
}

fn distance<const N: usize>(a: [f64; N], b: [f64; N]) -> f64 {
    a.into_iter()
        .zip(b)
        .map(|(a, b)| (a - b) * (a - b))
        .sum::<f64>()
        .sqrt()
}

fn anisotropy(positions: [[f64; 3]; 3], uvs: Option<[[f64; 2]; 3]>) -> f64 {
    let Some(uvs) = uvs else {
        return 1.0;
    };
    let mut minimum = f64::INFINITY;
    let mut maximum = 0.0_f64;
    for (a, b) in [(0, 1), (1, 2), (2, 0)] {
        let edge = distance(positions[a], positions[b]);
        if edge <= 1e-6 {
            continue;
        }
        let uv_edge = distance(uvs[a], uvs[b]);
        if uv_edge <= 1e-12 {
            return 1e6;
        }
        let scale = uv_edge / edge;
        minimum = minimum.min(scale);
        maximum = maximum.max(scale);
    }
    if minimum.is_infinite() {
        1.0
    } else {
        maximum / minimum.max(1e-12)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretched_uv_coincident_pair_flagged() {
        let positions = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 0.00002],
            [1.0, 0.0, 0.00002],
            [0.0, 1.0, 0.00002],
        ];
        let uvs = [
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [5.0, 0.0],
            [105.0, 0.0],
            [5.0, 1.0],
        ];
        let triangles = [[0, 1, 2], [5, 4, 3]];
        let pairs = coincident_pairs(&positions, Some(&uvs), &triangles, &GeometrySettings::default())
            .expect("valid geometry");
        assert_eq!(
            pairs,
            vec![CoincidentPair {
                keeper: 0,
                suspect: 1
            }]
        );
    }
    fn pair(z: f32, uv: [[f32; 2]; 3], settings: &GeometrySettings) -> Vec<CoincidentPair> {
        let positions = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, z],
            [1.0, 0.0, z],
            [0.0, 1.0, z],
        ];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], uv[0], uv[1], uv[2]];
        coincident_pairs(&positions, Some(&uvs), &[[0, 1, 2], [5, 4, 3]], settings).expect("valid")
    }

    #[test]
    fn distinct_thin_shell_is_preserved() {
        let different_uvs = [[5.0, 0.0], [6.0, 0.0], [5.0, 1.0]];
        assert!(pair(0.00002, different_uvs, &GeometrySettings::default()).is_empty());
        let stretched = [[5.0, 0.0], [105.0, 0.0], [5.0, 1.0]];
        assert!(pair(0.00011, stretched, &GeometrySettings::default()).is_empty());
    }

    #[test]
    fn matching_uvs_and_exact_copies_are_reported() {
        assert_eq!(
            pair(
                0.00002,
                [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                &GeometrySettings::default()
            )
            .len(),
            1
        );
        assert_eq!(
            pair(
                0.0,
                [[5.0, 0.0], [6.0, 0.0], [5.0, 1.0]],
                &GeometrySettings::default()
            )
            .len(),
            1
        );
        let p = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert_eq!(
            coincident_pairs(&p, None, &[[0, 1, 2], [2, 1, 0]], &GeometrySettings::default()).expect("valid"),
            vec![CoincidentPair {
                keeper: 0,
                suspect: 1
            }]
        );
    }

    #[test]
    fn keeper_is_less_stretched_even_when_it_is_later() {
        let p = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 0.00002],
            [1.0, 0.0, 0.00002],
            [0.0, 1.0, 0.00002],
        ];
        let uv = [
            [5.0, 0.0],
            [105.0, 0.0],
            [5.0, 1.0],
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
        ];
        assert_eq!(
            coincident_pairs(
                &p,
                Some(&uv),
                &[[0, 1, 2], [3, 4, 5]],
                &GeometrySettings::default()
            )
            .expect("valid"),
            vec![CoincidentPair {
                keeper: 1,
                suspect: 0
            }]
        );
    }

    #[test]
    fn separation_threshold_is_exclusive() {
        let settings = GeometrySettings {
            quantization: 1.0,
            separation: 0.125,
            ..GeometrySettings::default()
        };
        assert!(pair(0.125, [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &settings).is_empty());
        assert_eq!(
            pair(0.124, [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &settings).len(),
            1
        );
    }

    #[test]
    fn invalid_coordinates_indices_and_settings_are_errors() {
        let p = [[0.0, 0.0, 0.0]; 3];
        let s = GeometrySettings::default();
        assert!(coincident_pairs(&p, None, &[[0, 1, 3]], &s).is_err());
        assert!(coincident_pairs(&p, Some(&[[0.0, 0.0]]), &[], &s).is_err());
        assert!(coincident_pairs(&[[f32::NAN, 0.0, 0.0]], None, &[], &s).is_err());
        assert!(coincident_pairs(&[[f32::MAX, 0.0, 0.0]], None, &[], &s).is_err());
        let s = GeometrySettings {
            quantization: 0.0,
            ..s
        };
        assert!(coincident_pairs(&p, None, &[], &s).is_err());
        let s = GeometrySettings {
            uv_distance: f64::INFINITY,
            ..GeometrySettings::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn repeated_corners_and_degenerate_uvs_are_deterministic() {
        let p = [[0.0, 0.0, 0.0]; 3];
        let uv = [[0.0, 0.0]; 3];
        assert_eq!(
            coincident_pairs(
                &p,
                Some(&uv),
                &[[0, 0, 1], [1, 0, 0]],
                &GeometrySettings::default()
            )
            .expect("valid"),
            vec![CoincidentPair {
                keeper: 0,
                suspect: 1
            }]
        );
    }

    #[test]
    fn quantized_cell_boundary_is_not_claimed_as_a_match() {
        let p = [
            [0.0, 0.0, 0.000049],
            [1.0, 0.0, 0.000049],
            [0.0, 1.0, 0.000049],
            [0.0, 0.0, 0.000051],
            [1.0, 0.0, 0.000051],
            [0.0, 1.0, 0.000051],
        ];
        let uv = [
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
        ];
        assert!(
            coincident_pairs(
                &p,
                Some(&uv),
                &[[0, 1, 2], [3, 4, 5]],
                &GeometrySettings::default()
            )
            .expect("valid")
            .is_empty()
        );
    }
    #[test]
    fn corner_permutations_do_not_change_the_pair() {
        let p = [
            [-1.0, -1.0, 0.0],
            [0.0, -1.0, 0.0],
            [-1.0, 0.0, 0.0],
            [-1.0, -1.0, 0.00002],
            [0.0, -1.0, 0.00002],
            [-1.0, 0.0, 0.00002],
        ];
        let uv = [
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [5.0, 0.0],
            [105.0, 0.0],
            [5.0, 1.0],
        ];
        for a in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            for b in [[3, 4, 5], [3, 5, 4], [4, 3, 5], [4, 5, 3], [5, 3, 4], [5, 4, 3]] {
                assert_eq!(
                    coincident_pairs(&p, Some(&uv), &[a, b], &GeometrySettings::default()).expect("valid"),
                    vec![CoincidentPair {
                        keeper: 0,
                        suspect: 1
                    }]
                );
            }
        }
    }
    #[test]
    fn quantized_corner_order_is_lexicographic_even_for_old_weighted_key_ties() {
        // Quantized corners [1, 0, 0] and [0, 10, -27] have the same old weighted
        // key (x * 1_000_003 + y * 100_003 + z). Full tuples cannot lose a pair.
        let p = [[0.0001, 0.0, 0.0], [0.0, 0.001, -0.0027], [0.0, 0.0, 1.0]];
        assert_eq!(
            coincident_pairs(&p, None, &[[0, 1, 2], [2, 1, 0]], &GeometrySettings::default()).expect("valid"),
            vec![CoincidentPair {
                keeper: 0,
                suspect: 1
            }]
        );
    }
}
