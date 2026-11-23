// SPDX-License-Identifier: MPL-2.0
// Source: G. Taubin, "A Signal Processing Approach to Fair Surface Design", SIGGRAPH 1995 (the
// relax edit is one umbrella-operator Laplacian smoothing pass per iteration over mesh edges).
// The other edits are plain data operations. Original code (spec 55 § 3.4).

//! Vertex-map edits (spec 55 § 3.4): the data API the editor's paint mode and the agent's
//! `physics.cloth_edit_map` share, so every edit is reproducible without a GUI.
//!
//! [`apply_map_edit`] changes one map of LOD 0 of a [`ClothAsset`] and resamples the coarser
//! LODs from it (nearest rest vertex, ties to the lower index); [`transfer_maps`] copies every
//! map between assets; [`store_maps`] writes the maps back into a [`ClothSource`] as dense
//! arrays, so the `.cloth.ron` is rewritten canonically on save.
//!
//! An edit that would leave a value out of its map's range (`reach < 0`, `pose_pull > 1`, ...)
//! is refused and changes nothing. Editing a map the asset does not have yet starts from the
//! map's default (`reach` from 1,000,000 m, which is unlimited in practice).

use serde::{Deserialize, Serialize};

use regia_core::math::Vec3;

use super::ClothError;
use super::asset::{
    ClothAsset, ClothMaps, ClothSource, MAP_NAMES, MapData, bind_point, nearest_vertex, vertex_normals,
};

/// Value a `reach` map starts from when it is first edited.
pub const REACH_UNSET: f32 = 1.0e6;

/// Brush falloff of [`MapEdit::Sphere`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Falloff {
    /// Weight `1 - d / radius`.
    Linear,
    /// Smoothstep of `1 - d / radius`.
    Smooth,
    /// Weight 1 inside the sphere.
    Hard,
}

/// A color channel of [`MapEdit::FromVertexColor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Red.
    R,
    /// Green.
    G,
    /// Blue.
    B,
    /// Alpha.
    A,
}

/// One vertex-map edit (spec 55 § 3.4). In JSON the variant is the `op` field
/// (`{"op": "sphere", "center": [0, 1, 0], ...}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MapEdit {
    /// Explicit `(vertex, value)` pairs.
    Set {
        /// The pairs.
        vertices: Vec<(u32, f32)>,
    },
    /// Blend toward `value` for vertices within `radius` of `center` (rest pose, model space).
    Sphere {
        /// Brush center.
        center: [f32; 3],
        /// Brush radius in meters (positive).
        radius: f32,
        /// Target value.
        value: f32,
        /// Falloff.
        falloff: Falloff,
        /// Blend strength in `0..=1`.
        strength: f32,
    },
    /// Laplacian smoothing over mesh edges.
    Relax {
        /// Passes (1..=64).
        iterations: u32,
        /// Move toward the neighbor mean per pass, `0..=1`.
        strength: f32,
    },
    /// Linear ramp from `a` at vertex `from` to `b` at vertex `to` (projection on the rest
    /// segment, clamped).
    Ramp {
        /// Start vertex.
        from: u32,
        /// End vertex.
        to: u32,
        /// Value at `from`.
        a: f32,
        /// Value at `to`.
        b: f32,
    },
    /// Sets the edge-connected region around `seed` whose values are within `tolerance` of
    /// the seed's value.
    Flood {
        /// Seed vertex.
        seed: u32,
        /// Value tolerance (zero or more).
        tolerance: f32,
        /// New value.
        value: f32,
    },
    /// Maps a render-vertex color channel to `[low, high]`: each sim vertex takes the color of
    /// the render vertex nearest to it at rest. `colors` has one RGBA per render vertex.
    FromVertexColor {
        /// Channel.
        channel: Channel,
        /// Value at channel 0.
        low: f32,
        /// Value at channel 1.
        high: f32,
        /// Render-vertex colors.
        colors: Vec<[f32; 4]>,
    },
}

fn bad(what: &'static str) -> ClothError {
    ClothError::BadParam { what }
}

fn finite(v: f32, what: &'static str) -> Result<f32, ClothError> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(ClothError::NonFinite { what })
    }
}

/// Sorted, deduplicated edge neighbors of every vertex.
fn neighbors(n: usize, triangles: &[[u32; 3]]) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); n];
    for t in triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            out[a as usize].push(b);
            out[b as usize].push(a);
        }
    }
    for list in &mut out {
        list.sort_unstable();
        list.dedup();
    }
    out
}

fn map_name(map: &str) -> Result<&'static str, ClothError> {
    MAP_NAMES
        .iter()
        .find(|m| **m == map)
        .copied()
        .ok_or(ClothError::BadParam { what: "map" })
}

fn edited(
    values: &[f32],
    rest: &[Vec3],
    triangles: &[[u32; 3]],
    render_points: &[Vec3],
    edit: &MapEdit,
) -> Result<Vec<f32>, ClothError> {
    let n = values.len();
    let mut out = values.to_vec();
    let vertex = |i: u32| -> Result<usize, ClothError> {
        if (i as usize) < n {
            Ok(i as usize)
        } else {
            Err(ClothError::ParticleOutOfRange {
                index: i as usize,
                len: n,
            })
        }
    };
    match edit {
        MapEdit::Set { vertices } => {
            for &(i, v) in vertices {
                out[vertex(i)?] = finite(v, "value")?;
            }
        }
        MapEdit::Sphere {
            center,
            radius,
            value,
            falloff,
            strength,
        } => {
            let center = Vec3::from_array(*center);
            if !center.is_finite() {
                return Err(ClothError::NonFinite { what: "center" });
            }
            if finite(*radius, "radius")? <= 0.0 {
                return Err(bad("radius"));
            }
            let value = finite(*value, "value")?;
            let strength = finite(*strength, "strength")?;
            if !(0.0..=1.0).contains(&strength) {
                return Err(bad("strength"));
            }
            for (i, p) in rest.iter().enumerate() {
                let d = p.distance(center);
                if d > *radius {
                    continue;
                }
                let t = 1.0 - d / radius;
                let w = match falloff {
                    Falloff::Linear => t,
                    Falloff::Smooth => t * t * (3.0 - 2.0 * t),
                    Falloff::Hard => 1.0,
                } * strength;
                out[i] = if w >= 1.0 {
                    value
                } else {
                    out[i] + (value - out[i]) * w
                };
            }
        }
        MapEdit::Relax { iterations, strength } => {
            if *iterations == 0 || *iterations > 64 {
                return Err(bad("iterations"));
            }
            let strength = finite(*strength, "strength")?;
            if !(0.0..=1.0).contains(&strength) {
                return Err(bad("strength"));
            }
            let adjacency = neighbors(n, triangles);
            let mut next = out.clone();
            for _ in 0..*iterations {
                for (i, list) in adjacency.iter().enumerate() {
                    if list.is_empty() {
                        continue;
                    }
                    let mean = list.iter().map(|&j| out[j as usize]).sum::<f32>() / list.len() as f32;
                    next[i] = out[i] + (mean - out[i]) * strength;
                }
                out.copy_from_slice(&next);
            }
        }
        MapEdit::Ramp { from, to, a, b } => {
            let (from, to) = (vertex(*from)?, vertex(*to)?);
            let (a, b) = (finite(*a, "a")?, finite(*b, "b")?);
            let (p0, p1) = (rest[from], rest[to]);
            let axis = p1 - p0;
            let len2 = axis.length_squared();
            if len2 <= f32::EPSILON {
                return Err(bad("ramp"));
            }
            for (i, p) in rest.iter().enumerate() {
                let t = ((*p - p0).dot(axis) / len2).clamp(0.0, 1.0);
                out[i] = a + (b - a) * t;
            }
        }
        MapEdit::Flood {
            seed,
            tolerance,
            value,
        } => {
            let seed = vertex(*seed)?;
            let tolerance = finite(*tolerance, "tolerance")?;
            if tolerance < 0.0 {
                return Err(ClothError::Negative { what: "tolerance" });
            }
            let value = finite(*value, "value")?;
            let adjacency = neighbors(n, triangles);
            let reference = values[seed];
            let mut seen = vec![false; n];
            let mut queue = std::collections::VecDeque::from([seed]);
            seen[seed] = true;
            while let Some(i) = queue.pop_front() {
                out[i] = value;
                for &j in &adjacency[i] {
                    let j = j as usize;
                    if !seen[j] && (values[j] - reference).abs() <= tolerance {
                        seen[j] = true;
                        queue.push_back(j);
                    }
                }
            }
        }
        MapEdit::FromVertexColor {
            channel,
            low,
            high,
            colors,
        } => {
            let (low, high) = (finite(*low, "low")?, finite(*high, "high")?);
            if colors.len() != render_points.len() || colors.is_empty() {
                return Err(ClothError::MapSizeMismatch {
                    map: "colors",
                    len: colors.len(),
                    expected: render_points.len(),
                });
            }
            let k = match channel {
                Channel::R => 0,
                Channel::G => 1,
                Channel::B => 2,
                Channel::A => 3,
            };
            for (i, p) in rest.iter().enumerate() {
                let nearest = nearest_vertex(render_points, *p);
                let c = finite(colors[nearest][k], "color")?.clamp(0.0, 1.0);
                out[i] = low + (high - low) * c;
            }
        }
    }
    Ok(out)
}

/// Applies `edit` to map `map` of LOD 0 of `asset` (LODs 1+ are resampled from it) and marks
/// the asset dirty. Returns how many LOD 0 values changed.
///
/// # Errors
/// [`ClothError::BadParam`] for an unknown map or a bad edit parameter (or a result out of
/// the map's range), [`ClothError::ParticleOutOfRange`] for a vertex past the end,
/// [`ClothError::NonFinite`] for NaN or infinity. Nothing changes on error.
pub fn apply_map_edit(
    asset: &mut ClothAsset,
    lod: usize,
    map: &str,
    edit: &MapEdit,
) -> Result<usize, ClothError> {
    if lod != 0 {
        return Err(bad("lod"));
    }
    let name = map_name(map)?;
    let Some(lod0) = asset.lods.first() else {
        return Err(bad("lod"));
    };
    let n = lod0.rest_positions.len();
    if lod0.triangles.iter().any(|t| t.iter().any(|&i| i as usize >= n)) {
        return Err(ClothError::ParticleOutOfRange {
            index: lod0
                .triangles
                .iter()
                .flatten()
                .map(|&i| i as usize)
                .max()
                .unwrap_or(0),
            len: n,
        });
    }
    let current = lod0.maps.get(name).cloned().unwrap_or_default();
    let start = if current.is_empty() {
        let fill = if name == "reach" {
            REACH_UNSET
        } else {
            ClothMaps::default_of(name)
        };
        vec![fill; n]
    } else {
        current.clone()
    };
    // Render vertices at rest (color edits only).
    let render_points: Vec<Vec3> = if matches!(edit, MapEdit::FromVertexColor { .. }) {
        let mut normals = Vec::new();
        vertex_normals(&lod0.rest_positions, &lod0.triangles, &mut normals);
        lod0.render
            .vertices
            .iter()
            .map(|b| bind_point(b, &lod0.triangles, &lod0.rest_positions, &normals).0)
            .collect()
    } else {
        Vec::new()
    };
    let values = edited(
        &start,
        &lod0.rest_positions,
        &lod0.triangles,
        &render_points,
        edit,
    )?;
    let mut maps = lod0.maps.clone();
    if let Some(slot) = maps.get_mut(name) {
        *slot = values;
    }
    maps.validate(n)?;
    let changed = if current.is_empty() {
        n
    } else {
        current
            .iter()
            .zip(maps.get(name).map_or(&[][..], Vec::as_slice))
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count()
    };
    let rest0 = lod0.rest_positions.clone();
    asset.lods[0].maps = maps;
    resample_lods(asset, &rest0);
    asset.dirty = true;
    Ok(changed)
}

fn resample_lods(asset: &mut ClothAsset, rest0: &[Vec3]) {
    let maps0 = asset.lods[0].maps.clone();
    for lod in asset.lods.iter_mut().skip(1) {
        lod.maps = resample(&maps0, rest0, &lod.rest_positions);
    }
}

fn resample(from: &ClothMaps, from_rest: &[Vec3], to: &[Vec3]) -> ClothMaps {
    let same = from_rest == to;
    let nearest: Vec<usize> = if same {
        (0..to.len()).collect()
    } else {
        to.iter().map(|&p| nearest_vertex(from_rest, p)).collect()
    };
    let mut out = ClothMaps::default();
    for name in MAP_NAMES {
        let src = from.get(name).map_or(&[][..], Vec::as_slice);
        // A map that does not cover every `from` vertex cannot be resampled; it is dropped.
        if !src.is_empty()
            && src.len() == from_rest.len()
            && let Some(dst) = out.get_mut(name)
        {
            *dst = nearest.iter().map(|&i| src[i]).collect();
        }
    }
    out
}

/// Copies every map of `from`'s LOD 0 to `to`'s LOD 0: by index when the sim meshes have the
/// same vertex count and triangles, else by nearest rest vertex (ties: lower index). `to`'s
/// LODs 1+ are resampled and `to` is marked dirty.
pub fn transfer_maps(from: &ClothAsset, to: &mut ClothAsset) {
    let (Some(src), Some(dst)) = (from.lods.first(), to.lods.first()) else {
        return;
    };
    let maps = if src.rest_positions.len() == dst.rest_positions.len() && src.triangles == dst.triangles {
        src.maps.clone()
    } else {
        resample(&src.maps, &src.rest_positions, &dst.rest_positions)
    };
    let rest0 = dst.rest_positions.clone();
    to.lods[0].maps = maps;
    resample_lods(to, &rest0);
    to.dirty = true;
}

/// Writes LOD 0's maps into `source` as dense arrays (maps the asset does not have are
/// removed), for a canonical rewrite of the `.cloth.ron`.
pub fn store_maps(asset: &ClothAsset, source: &mut ClothSource) {
    source.maps.clear();
    for name in MAP_NAMES {
        if let Some(values) = asset.lod0().maps.get(name)
            && !values.is_empty()
        {
            source
                .maps
                .insert(name.to_string(), MapData::Dense(values.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloth::asset::{ClothSource, SimMeshInput};

    /// A 3 x 3 grid in the XY plane, spacing 1 (vertex `r * 3 + c` at `(c, -r, 0)`).
    fn asset() -> ClothAsset {
        let mut mesh = SimMeshInput::default();
        for r in 0..3u32 {
            for c in 0..3u32 {
                mesh.positions.push(Vec3::new(c as f32, -(r as f32), 0.0));
            }
        }
        for r in 0..2u32 {
            for c in 0..2u32 {
                let i = r * 3 + c;
                mesh.triangles.push([i, i + 3, i + 1]);
                mesh.triangles.push([i + 1, i + 3, i + 4]);
            }
        }
        let source = ClothSource::from_ron_str(
            r#"(format_version: 1, name: "g", sim_mesh: (source: "g.glb", node: "G"),
                maps: { "pose_pull": Dense([0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 1.0, 1.0, 1.0]) })"#,
        )
        .expect("parses");
        ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks")
    }

    fn pull(a: &ClothAsset) -> Vec<f32> {
        a.lod0().maps.pose_pull.clone()
    }

    #[test]
    fn set_sphere_ramp_relax_flood_give_the_expected_arrays() {
        let mut a = asset();
        let changed = apply_map_edit(
            &mut a,
            0,
            "pose_pull",
            &MapEdit::Set {
                vertices: vec![(0, 0.25), (8, 1.0)],
            },
        )
        .expect("set");
        assert_eq!(changed, 1);
        assert_eq!(pull(&a), vec![0.25, 0.0, 0.0, 0.5, 0.5, 0.5, 1.0, 1.0, 1.0]);
        assert!(a.dirty);

        let mut a = asset();
        apply_map_edit(
            &mut a,
            0,
            "pose_pull",
            &MapEdit::Sphere {
                center: [1.0, -1.0, 0.0],
                radius: 1.0,
                value: 1.0,
                falloff: Falloff::Linear,
                strength: 1.0,
            },
        )
        .expect("sphere");
        // Center vertex 4 at weight 1; edge neighbors at distance 1 get weight 0.
        assert_eq!(pull(&a), vec![0.0, 0.0, 0.0, 0.5, 1.0, 0.5, 1.0, 1.0, 1.0]);

        let mut a = asset();
        apply_map_edit(
            &mut a,
            0,
            "standoff",
            &MapEdit::Ramp {
                from: 0,
                to: 2,
                a: 0.0,
                b: 0.2,
            },
        )
        .expect("ramp");
        let s = a.lod0().maps.standoff.clone();
        assert_eq!(s, vec![0.0, 0.1, 0.2, 0.0, 0.1, 0.2, 0.0, 0.1, 0.2]);

        let mut a = asset();
        apply_map_edit(
            &mut a,
            0,
            "pose_pull",
            &MapEdit::Relax {
                iterations: 1,
                strength: 1.0,
            },
        )
        .expect("relax");
        let p = pull(&a);
        // Vertex 0's neighbors: 1 and 3 -> mean 0.25.
        assert_eq!(p[0], 0.25);
        // Vertex 4's neighbors: 1, 2, 3, 5, 6, 7 -> (0 + 0 + 0.5 + 0.5 + 1 + 1) / 6.
        assert!((p[4] - 0.5).abs() < 1e-6);

        let mut a = asset();
        apply_map_edit(
            &mut a,
            0,
            "pose_pull",
            &MapEdit::Flood {
                seed: 4,
                tolerance: 0.0,
                value: 0.75,
            },
        )
        .expect("flood");
        assert_eq!(pull(&a), vec![0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn color_edit_reads_the_nearest_render_vertex() {
        let mut a = asset();
        let colors: Vec<[f32; 4]> = (0..9).map(|i| [i as f32 / 8.0, 0.0, 0.0, 1.0]).collect();
        apply_map_edit(
            &mut a,
            0,
            "sim_mix",
            &MapEdit::FromVertexColor {
                channel: Channel::R,
                low: 0.0,
                high: 1.0,
                colors,
            },
        )
        .expect("color");
        let s = a.lod0().maps.sim_mix.clone();
        for (i, v) in s.iter().enumerate() {
            assert!((v - i as f32 / 8.0).abs() < 1e-6);
        }
    }

    #[test]
    fn bad_edits_change_nothing() {
        let mut a = asset();
        let before = a.clone();
        let edits = [
            (
                "pose_pull",
                MapEdit::Set {
                    vertices: vec![(0, 2.0)],
                },
            ),
            (
                "pose_pull",
                MapEdit::Set {
                    vertices: vec![(9, 0.0)],
                },
            ),
            (
                "pose_pull",
                MapEdit::Set {
                    vertices: vec![(0, f32::NAN)],
                },
            ),
            ("stiffness", MapEdit::Set { vertices: vec![] }),
            (
                "reach",
                MapEdit::Relax {
                    iterations: 0,
                    strength: 1.0,
                },
            ),
            (
                "layer",
                MapEdit::Set {
                    vertices: vec![(0, 1.5)],
                },
            ),
        ];
        for (map, edit) in &edits {
            assert!(apply_map_edit(&mut a, 0, map, edit).is_err(), "{map} {edit:?}");
        }
        assert_eq!(a, before);
    }

    /// Spec 55 § 3.4 and test 14: `transfer_maps` between sim meshes of different topology
    /// takes each value from the nearest rest vertex of the source, and a tie goes to the lower
    /// index.
    #[test]
    fn transfer_maps_by_nearest_vertex_breaks_ties_to_the_lower_index() {
        let mut from = asset();
        apply_map_edit(
            &mut from,
            0,
            "pose_pull",
            &MapEdit::Set {
                vertices: vec![(4, 0.9)],
            },
        )
        .expect("set");
        // Source rows: pose_pull 0 / (0.5, 0.9, 0.5) / 1.
        let mesh = SimMeshInput {
            positions: vec![
                Vec3::new(0.0, 0.0, 0.0),  // vertex 0
                Vec3::new(2.1, 0.0, 0.0),  // nearest: vertex 2
                Vec3::new(0.5, -1.0, 0.0), // tie between 3 and 4: vertex 3
                Vec3::new(1.4, -1.0, 0.0), // nearest: vertex 4
            ],
            triangles: vec![[0, 2, 1], [1, 2, 3]],
            ..SimMeshInput::default()
        };
        let source = ClothSource::from_ron_str(
            r#"(format_version: 1, name: "t", sim_mesh: (source: "t.glb", node: "T"))"#,
        )
        .expect("parses");
        let mut to = ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks");
        assert!(to.lod0().maps.pose_pull.is_empty());
        transfer_maps(&from, &mut to);
        assert_eq!(to.lod0().maps.pose_pull, [0.0, 0.0, 0.5, 0.9]);
        assert!(to.dirty, "a transfer marks the asset dirty");
    }

    #[test]
    fn reach_starts_unlimited_and_transfer_and_store_round_trip() {
        let mut a = asset();
        apply_map_edit(
            &mut a,
            0,
            "reach",
            &MapEdit::Set {
                vertices: vec![(0, 0.0)],
            },
        )
        .expect("reach");
        assert_eq!(a.lod0().maps.reach[0], 0.0);
        assert_eq!(a.lod0().maps.reach[1], REACH_UNSET);
        let mut b = asset();
        transfer_maps(&a, &mut b);
        assert_eq!(b.lod0().maps, a.lod0().maps);
        let mut source = ClothSource::from_ron_str(
            r#"(format_version: 1, name: "g", sim_mesh: (source: "g.glb", node: "G"))"#,
        )
        .expect("parses");
        store_maps(&a, &mut source);
        let text = source.to_ron_string().expect("writes");
        let back = ClothSource::from_ron_str(&text).expect("reads");
        assert_eq!(back.maps.len(), 2);
        assert_eq!(
            back.maps.get("reach"),
            Some(&MapData::Dense(a.lod0().maps.reach.clone()))
        );
    }
}
