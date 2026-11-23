// SPDX-License-Identifier: MPL-2.0
//! Mesh items (`specs/60` § 4.5, § 3.4 step 4): the CPU mesh built from `mesh` messages, vertex-only
//! updates, generated normals, and the render mesh handed to `regia_render::MeshRenderer`.
//!
//! Everything here is a fixed-order single pass, so the same messages give bit-identical arrays on
//! one platform (spec § 7). The messages were validated on the reader thread (counts, finite
//! floats, index ranges), so these readers only reinterpret bytes.

use std::sync::Arc;

use regia_render::{CpuMesh, LabVertex};

use super::wire::{Elem, ErrorCode, MeshSection, MeshUpdate, WireError, check_vertex_only, section};

/// One material slot of a mesh item (`material slots` section).
#[derive(Debug, Clone, PartialEq)]
pub struct SlotInfo {
    /// Slot name.
    pub name: String,
    /// Linear base color and alpha.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Linear emissive color.
    pub emissive: [f32; 3],
    /// Texture names: base color, normal, metal-rough, emissive (empty = none).
    pub textures: [String; 4],
}

/// The current geometry of one mesh item, in the sender's object space times `unit_scale`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedMeshData {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals (sent, or generated when the sender sent none).
    pub normals: Vec<[f32; 3]>,
    /// True when `normals` were generated.
    pub normals_generated: bool,
    /// UV set 0 (empty when absent).
    pub uv0: Vec<[f32; 2]>,
    /// Vertex colors (empty when absent).
    pub colors: Vec<[f32; 4]>,
    /// Skin joint indices (empty on a rigid mesh).
    pub joints: Vec<[u16; 4]>,
    /// Skin weights, renormalized (empty on a rigid mesh).
    pub weights: Vec<[f32; 4]>,
    /// Triangle-list indices.
    pub indices: Vec<u32>,
    /// Draw ranges `[slot, first_index, index_count]`.
    pub submeshes: Vec<[u32; 3]>,
    /// Material slots.
    pub slots: Vec<SlotInfo>,
}

fn f32s<const N: usize>(s: &MeshSection) -> Vec<[f32; N]> {
    let (words, _) = s.data.as_chunks::<4>();
    let floats: Vec<f32> = words.iter().map(|w| f32::from_le_bytes(*w)).collect();
    floats.as_chunks::<N>().0.to_vec()
}

fn colors(s: &MeshSection) -> Vec<[f32; 4]> {
    match s.elem {
        Elem::U8 => s
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| c.map(|b| f32::from(b) / 255.0))
            .collect(),
        _ => f32s::<4>(s),
    }
}

fn indices(s: &MeshSection) -> Vec<u32> {
    match s.elem {
        Elem::U16 => s
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u32::from(u16::from_le_bytes(*c)))
            .collect(),
        _ => s
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect(),
    }
}

fn skin_joints(s: &MeshSection) -> Vec<[u16; 4]> {
    s.data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| {
            [
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]]),
                u16::from_le_bytes([c[4], c[5]]),
                u16::from_le_bytes([c[6], c[7]]),
            ]
        })
        .collect()
}

/// Weights scaled to sum to 1 (spec § 4.5); a vertex with no weight follows joint 0.
fn skin_weights(s: &MeshSection) -> Vec<[f32; 4]> {
    f32s::<4>(s)
        .into_iter()
        .map(|w| {
            let sum: f32 = w.iter().sum();
            if sum > 0.0 && sum.is_finite() {
                w.map(|v| v / sum)
            } else {
                [1.0, 0.0, 0.0, 0.0]
            }
        })
        .collect()
}

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }
    fn f32(&mut self) -> Option<f32> {
        let b = self.take(4)?;
        Some(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str(&mut self) -> Option<String> {
        let b = self.take(2)?;
        let n = usize::from(u16::from_le_bytes([b[0], b[1]]));
        let s = self.take(n)?;
        std::str::from_utf8(s).ok().map(str::to_owned)
    }
}

/// Parses the `material slots` section (spec § 4.5 id 8).
///
/// # Errors
///
/// `malformed_frame` for a truncated table or bad UTF-8, `invalid_value` for a non-finite factor.
pub fn parse_slots(s: &MeshSection) -> Result<Vec<SlotInfo>, WireError> {
    let bad = || WireError::new(ErrorCode::MalformedFrame, "material slot table is truncated");
    let mut r = Bytes(&s.data);
    let mut out = Vec::new();
    for _ in 0..s.count {
        let name = r.str().ok_or_else(bad)?;
        let mut base_color = [0.0; 4];
        for v in &mut base_color {
            *v = r.f32().ok_or_else(bad)?;
        }
        let metallic = r.f32().ok_or_else(bad)?;
        let roughness = r.f32().ok_or_else(bad)?;
        let mut emissive = [0.0; 3];
        for v in &mut emissive {
            *v = r.f32().ok_or_else(bad)?;
        }
        let textures = [
            r.str().ok_or_else(bad)?,
            r.str().ok_or_else(bad)?,
            r.str().ok_or_else(bad)?,
            r.str().ok_or_else(bad)?,
        ];
        let finite = base_color
            .iter()
            .chain(&emissive)
            .chain([&metallic, &roughness])
            .all(|v| v.is_finite());
        if !finite {
            return Err(WireError::new(
                ErrorCode::InvalidValue,
                "non-finite material factor",
            ));
        }
        out.push(SlotInfo {
            name,
            base_color,
            metallic,
            roughness,
            emissive,
            textures,
        });
    }
    Ok(out)
}

impl FeedMeshData {
    /// Vertex count.
    #[must_use]
    pub fn vertex_count(&self) -> u32 {
        u32::try_from(self.positions.len()).unwrap_or(u32::MAX)
    }

    /// Triangle count.
    #[must_use]
    pub fn triangle_count(&self) -> u32 {
        u32::try_from(self.indices.len() / 3).unwrap_or(u32::MAX)
    }

    /// True when every vertex has joints and weights.
    #[must_use]
    pub fn is_skinned(&self) -> bool {
        !self.positions.is_empty()
            && self.joints.len() == self.positions.len()
            && self.weights.len() == self.positions.len()
    }

    /// Bytes this mesh takes as a render mesh, for the swap budget.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        self.positions.len() * size_of::<LabVertex>() + self.indices.len() * size_of::<u32>()
    }

    /// Applies a full or vertex-only update. Positions are multiplied by `unit_scale`. Normals are
    /// generated when the result has none.
    ///
    /// # Errors
    ///
    /// `topology_mismatch` for a vertex-only update whose counts differ from the current mesh,
    /// or the slot-table errors of [`parse_slots`]. On error the mesh is unchanged.
    pub fn apply(&mut self, m: &MeshUpdate, unit_scale: f32) -> Result<(), WireError> {
        let mut next = if m.update_mode == 0 {
            Self::default()
        } else {
            check_vertex_only(m, self.vertex_count())?;
            self.clone()
        };
        let mut sent_normals = false;
        for s in &m.sections {
            match s.section_id {
                section::POSITIONS => {
                    next.positions = f32s::<3>(s)
                        .into_iter()
                        .map(|p| p.map(|v| v * unit_scale))
                        .collect();
                }
                section::NORMALS => {
                    next.normals = f32s::<3>(s);
                    sent_normals = true;
                }
                section::UV if s.index == 0 => next.uv0 = f32s::<2>(s),
                section::COLOR => next.colors = colors(s),
                section::JOINTS => next.joints = skin_joints(s),
                section::WEIGHTS => next.weights = skin_weights(s),
                section::INDICES => next.indices = indices(s),
                section::SUBMESHES => {
                    next.submeshes = s
                        .data
                        .as_chunks::<12>()
                        .0
                        .iter()
                        .map(|c| {
                            [
                                u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                                u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                                u32::from_le_bytes([c[8], c[9], c[10], c[11]]),
                            ]
                        })
                        .collect();
                }
                section::MATERIAL_SLOTS => next.slots = parse_slots(s)?,
                // Tangents and other uv sets are carried by the protocol; the render mesh
                // generates tangents itself.
                _ => {}
            }
        }
        if next.submeshes.is_empty() {
            next.submeshes = vec![[0, 0, u32::try_from(next.indices.len()).unwrap_or(u32::MAX)]];
        }
        // Normals: keep sent ones; regenerate when positions moved and the sender never sent
        // any (a vertex-only position update must not leave stale generated normals).
        if sent_normals {
            next.normals_generated = false;
        } else if m.update_mode == 0 || next.normals_generated || next.normals.is_empty() {
            next.normals = smooth_normals(&next.positions, &next.indices);
            next.normals_generated = true;
        }
        *self = next;
        Ok(())
    }

    /// The render mesh: one vertex per position, white when no colors were sent.
    #[must_use]
    pub fn to_cpu_mesh(&self) -> CpuMesh {
        let vertices = self
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut v = LabVertex::from_pos_normal_uv(
                    *p,
                    self.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
                    self.uv0.get(i).copied().unwrap_or([0.0, 0.0]),
                );
                if let Some(c) = self.colors.get(i) {
                    v.color = *c;
                }
                if let (Some(j), Some(w)) = (self.joints.get(i), self.weights.get(i)) {
                    v.joints = j.map(u32::from);
                    v.weights = *w;
                }
                v
            })
            .collect();
        CpuMesh {
            vertices,
            indices: self.indices.clone(),
        }
    }
}

/// One render mesh per draw range, `(slot, mesh)`, with each range's vertices remapped in first-use
/// order, so a part holds only what it draws. Ranges were validated against the index list.
#[must_use]
pub fn split_parts(data: &FeedMeshData) -> Vec<(u32, Arc<CpuMesh>)> {
    let all = data.to_cpu_mesh();
    if let [[slot, 0, count]] = data.submeshes.as_slice()
        && *count as usize == all.indices.len()
    {
        return vec![(*slot, Arc::new(all))];
    }
    let mut remap = vec![u32::MAX; all.vertices.len()];
    let mut parts = Vec::with_capacity(data.submeshes.len());
    for [slot, first, count] in &data.submeshes {
        let range = (*first as usize)..(*first as usize + *count as usize);
        let Some(indices) = all.indices.get(range) else {
            continue;
        };
        let mut vertices = Vec::new();
        let mut used = Vec::new();
        let mut local = Vec::with_capacity(indices.len());
        for index in indices {
            let Some(mapped) = remap.get_mut(*index as usize) else {
                continue;
            };
            if *mapped == u32::MAX {
                *mapped = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
                vertices.push(all.vertices[*index as usize]);
                used.push(*index as usize);
            }
            local.push(*mapped);
        }
        for index in used {
            remap[index] = u32::MAX;
        }
        parts.push((
            *slot,
            Arc::new(CpuMesh {
                vertices,
                indices: local,
            }),
        ));
    }
    parts
}

/// Area-weighted smooth normals: each triangle adds its unnormalized face normal (whose length is
/// twice its area) to its three vertices, then every sum is normalized; a vertex in no triangle,
/// or whose sum is zero, gets +Y. One fixed-order pass, so the result is deterministic.
#[must_use]
pub fn smooth_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut sums = vec![[0.0f32; 3]; positions.len()];
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| i as usize);
        let (Some(pa), Some(pb), Some(pc)) = (positions.get(a), positions.get(b), positions.get(c)) else {
            continue;
        };
        let e1 = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
        let e2 = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
        let n = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        for i in [a, b, c] {
            for (sum, v) in sums[i].iter_mut().zip(n) {
                *sum += v;
            }
        }
    }
    sums.into_iter()
        .map(|s| {
            let len = (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt();
            if len > 0.0 && len.is_finite() {
                [s[0] / len, s[1] / len, s[2] / len]
            } else {
                [0.0, 1.0, 0.0]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcc_feed::wire::FrameInfo;

    fn sec(section_id: u8, elem: Elem, count: u32, data: Vec<u8>) -> MeshSection {
        MeshSection {
            section_id,
            elem,
            index: 0,
            count,
            data,
        }
    }

    fn floats(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn quad() -> MeshUpdate {
        // Two triangles in the XZ plane, counter-clockwise seen from +Y.
        let positions = [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0];
        let idx: Vec<u8> = [0u16, 1, 2, 0, 2, 3]
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect();
        MeshUpdate {
            handle: 1,
            info: FrameInfo {
                frame_id: 1,
                source_time_ns: 0,
                timecode: None,
            },
            update_mode: 0,
            sections: vec![
                sec(section::POSITIONS, Elem::F32, 4, floats(&positions)),
                sec(section::UV, Elem::F32, 4, floats(&[0.0; 8])),
                sec(section::INDICES, Elem::U16, 6, idx),
            ],
        }
    }

    #[test]
    fn full_update_generates_normals_and_one_submesh() {
        let mut m = FeedMeshData::default();
        m.apply(&quad(), 2.0).expect("applies");
        assert_eq!(m.vertex_count(), 4);
        assert_eq!(m.triangle_count(), 2);
        assert_eq!(m.positions[2], [2.0, 0.0, 2.0], "unit_scale multiplies positions");
        assert!(m.normals_generated);
        for n in &m.normals {
            assert!((n[1] - 1.0).abs() < 1e-6, "{n:?}");
        }
        assert_eq!(m.submeshes, vec![[0, 0, 6]]);
        let cpu = m.to_cpu_mesh();
        assert_eq!(cpu.vertices.len(), 4);
        assert_eq!(cpu.indices, vec![0, 1, 2, 0, 2, 3]);
    }

    /// Spec § 8 test 7: generated normals equal a reference computation within 1e-6 (here a
    /// hand-computed tent: two faces meeting at a ridge).
    #[test]
    fn generated_normals_match_a_reference() {
        let positions = [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [1.0, 1.0, 1.0],
            [2.0, 0.0, 0.0],
            [2.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
        ];
        let indices = [0, 5, 2, 0, 2, 1, 1, 2, 4, 1, 4, 3];
        let n = smooth_normals(&positions, &indices);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        // Each quad is two triangles, so a ridge vertex touches one on one side and two on the
        // other: (-1, 1, 0) + 2 (1, 1, 0) = (1, 3, 0), and the mirror at the other end.
        let (x, y) = (1.0 / 10.0f32.sqrt(), 3.0 / 10.0f32.sqrt());
        assert!(
            (n[1][0] - x).abs() < 1e-6 && (n[1][1] - y).abs() < 1e-6,
            "{:?}",
            n[1]
        );
        assert!(
            (n[2][0] + x).abs() < 1e-6 && (n[2][1] - y).abs() < 1e-6,
            "{:?}",
            n[2]
        );
        // Left foot: the left face only, (-1, 1, 0)/sqrt 2.
        assert!(
            (n[0][0] + s).abs() < 1e-6 && (n[0][1] - s).abs() < 1e-6,
            "{:?}",
            n[0]
        );
        assert!(
            (n[3][0] - s).abs() < 1e-6 && (n[3][1] - s).abs() < 1e-6,
            "{:?}",
            n[3]
        );
    }

    #[test]
    fn vertex_only_update_keeps_indices_and_checks_counts() {
        let mut m = FeedMeshData::default();
        m.apply(&quad(), 1.0).expect("full");
        let mut v = quad();
        v.update_mode = 1;
        v.sections = vec![sec(
            section::POSITIONS,
            Elem::F32,
            4,
            floats(&[0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0]),
        )];
        m.apply(&v, 1.0).expect("vertex-only");
        assert_eq!(m.positions[0], [0.0, 1.0, 0.0]);
        assert_eq!(m.indices.len(), 6, "indices kept");
        let mut wrong = v.clone();
        wrong.sections[0].count = 3;
        wrong.sections[0].data.truncate(36);
        let before = m.clone();
        assert_eq!(
            m.apply(&wrong, 1.0).expect_err("count").code(),
            "topology_mismatch"
        );
        assert_eq!(m, before, "unchanged on error");
    }

    #[test]
    fn skin_sections_reach_the_render_vertices_with_weights_renormalized() {
        let mut m = FeedMeshData::default();
        let mut update = quad();
        let joints: Vec<u8> = [1u16, 2, 0, 0]
            .iter()
            .cycle()
            .take(16)
            .flat_map(|j| j.to_le_bytes())
            .collect();
        update.sections.push(sec(section::JOINTS, Elem::U16, 4, joints));
        update.sections.push(sec(
            section::WEIGHTS,
            Elem::F32,
            4,
            floats(&[2.0, 2.0, 0.0, 0.0].repeat(4)),
        ));
        m.apply(&update, 1.0).expect("full");
        assert!(m.is_skinned());
        let cpu = m.to_cpu_mesh();
        assert_eq!(cpu.vertices[1].joints, [1, 2, 0, 0]);
        assert_eq!(cpu.vertices[1].weights, [0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn submeshes_split_into_compact_parts_by_slot() {
        let mut m = FeedMeshData::default();
        m.apply(&quad(), 1.0).expect("full");
        m.submeshes = vec![[0, 0, 3], [1, 3, 3]];
        let parts = split_parts(&m);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, 0);
        assert_eq!(parts[1].0, 1);
        assert_eq!(parts[0].1.indices, vec![0, 1, 2]);
        assert_eq!(
            parts[1].1.indices,
            vec![0, 1, 2],
            "remapped to the part's own vertices"
        );
        assert_eq!(parts[1].1.vertices.len(), 3);
        assert_eq!(parts[1].1.vertices[2].position, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn slot_table_parses_and_truncation_is_refused() {
        let mut data = Vec::new();
        let s = |data: &mut Vec<u8>, t: &str| {
            data.extend_from_slice(&(t.len() as u16).to_le_bytes());
            data.extend_from_slice(t.as_bytes());
        };
        s(&mut data, "Paint");
        data.extend(floats(&[1.0, 0.5, 0.25, 1.0, 0.0, 0.5, 0.0, 0.0, 0.0]));
        for t in ["albedo", "", "", ""] {
            s(&mut data, t);
        }
        let section = sec(section::MATERIAL_SLOTS, Elem::U8, 1, data.clone());
        let slots = parse_slots(&section).expect("parses");
        assert_eq!(slots[0].name, "Paint");
        assert_eq!(slots[0].base_color, [1.0, 0.5, 0.25, 1.0]);
        assert_eq!(slots[0].textures[0], "albedo");
        data.truncate(data.len() - 3);
        let section = sec(section::MATERIAL_SLOTS, Elem::U8, 1, data);
        assert_eq!(
            parse_slots(&section).expect_err("short").code(),
            "malformed_frame"
        );
    }
}
