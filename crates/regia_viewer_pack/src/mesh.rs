// SPDX-License-Identifier: MPL-2.0
//! The cooked mesh file `mesh.rvm` (Regia viewer mesh, format version 1): every mesh of a product
//! package in one compact little-endian blob.
//!
//! - positions: three `u16` per vertex, quantized to the mesh's bounds (error at most half a step,
//!   1/131070 of the extent; 0.08 um for a 10 mm part);
//! - normals: two `i16` per vertex, octahedral encoding (Cigolle et al. 2014), error under 0.05 deg
//!   (largest near the fold of the lower hemisphere);
//! - texture coordinates (optional): two `u16` per vertex, quantized to their own bounds;
//! - indices: `u16` when the mesh has at most 65536 vertices, else `u32`.
//!
//! ```text
//! "RVM1"  u32 format_version  u32 mesh_count  u32 reserved(0)
//! per mesh:
//!   u16 name_len  name  u16 slot_len  slot  (pad to 4)
//!   u32 flags (1 = uvs, 2 = u32 indices)  u32 vertex_count  u32 index_count
//!   f32x3 bounds_min  f32x3 bounds_max  [f32x2 uv_min  f32x2 uv_max]
//!   u16x3 positions  (pad to 4)  i16x2 normals  [u16x2 uvs]  indices  (pad to 4)
//! ```
//!
//! The decoder checks every count against the bytes that are left before it allocates, so a
//! damaged or hostile file is refused, never trusted (`pack_bad_mesh`).

use glam::{Vec2, Vec3};

use crate::PackError;

/// File magic.
pub const MAGIC: &[u8; 4] = b"RVM1";
/// The mesh format version this build writes and reads.
pub const MESH_FORMAT_VERSION: u32 = 1;
/// Most meshes in one file.
pub const MAX_MESHES: usize = 4096;
/// Most vertices in one mesh.
pub const MAX_VERTICES: usize = 1 << 22;
/// Most indices in one mesh.
pub const MAX_INDICES: usize = 3 << 22;

const FLAG_UV: u32 = 1;
const FLAG_IDX32: u32 = 2;

/// One mesh with full-precision attributes: what the cook writes and the decoder returns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeshData {
    /// Key the runtime looks the mesh up by (`part:<part_ref>`, `cut:<cut_id>`, ...).
    pub name: String,
    /// The recipe material zone it is drawn with (`body`, `gem:s1`, ...).
    pub material_slot: String,
    /// Positions in mm.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals, one per position.
    pub normals: Vec<[f32; 3]>,
    /// Optional texture coordinates, one per position.
    pub uvs: Option<Vec<[f32; 2]>>,
    /// Triangle list.
    pub indices: Vec<u32>,
}

impl MeshData {
    /// Triangle count.
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// Axis-aligned bounds `(min, max)`; zero for an empty mesh.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        if self.positions.is_empty() {
            return (Vec3::ZERO, Vec3::ZERO);
        }
        let mut lo = Vec3::splat(f32::INFINITY);
        let mut hi = Vec3::splat(f32::NEG_INFINITY);
        for p in &self.positions {
            let p = Vec3::from(*p);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        (lo, hi)
    }

    /// Check the mesh is well formed: matching attribute counts, finite values, indices in range,
    /// a whole number of triangles, within the caps.
    pub fn validate(&self) -> Result<(), PackError> {
        let bad = |m: String| PackError::new("pack_bad_mesh", format!("mesh {}: {m}", self.name));
        let n = self.positions.len();
        if n > MAX_VERTICES || self.indices.len() > MAX_INDICES {
            return Err(bad(format!(
                "{n} vertices / {} indices over the cap",
                self.indices.len()
            )));
        }
        if self.normals.len() != n || self.uvs.as_ref().is_some_and(|u| u.len() != n) {
            return Err(bad("attribute counts differ".into()));
        }
        if !self.indices.len().is_multiple_of(3) {
            return Err(bad("index count is not a multiple of 3".into()));
        }
        if self.indices.iter().any(|&i| i as usize >= n) {
            return Err(bad("index out of range".into()));
        }
        let finite = |a: &[f32]| a.iter().all(|x| x.is_finite());
        if !self.positions.iter().all(|p| finite(p))
            || !self.normals.iter().all(|p| finite(p))
            || !self.uvs.iter().flatten().all(|p| finite(p))
        {
            return Err(bad("non-finite attribute".into()));
        }
        // The file quantizes to the bounds: their span must be finite too, or every coordinate comes
        // back as infinity or NaN (two finite corners near +-f32::MAX overflow their difference).
        let (lo, hi) = self.bounds();
        let uv_span_finite = self.uvs.as_ref().is_none_or(|uvs| {
            let (a, b) = uvs.iter().fold(
                (Vec2::splat(f32::INFINITY), Vec2::splat(f32::NEG_INFINITY)),
                |(a, b), uv| (a.min(Vec2::from(*uv)), b.max(Vec2::from(*uv))),
            );
            uvs.is_empty() || (b - a).is_finite()
        });
        if !(hi - lo).is_finite() || !uv_span_finite {
            return Err(bad("bounds span is not finite".into()));
        }
        if self.name.len() > u16::MAX as usize || self.material_slot.len() > u16::MAX as usize {
            return Err(bad("name too long".into()));
        }
        Ok(())
    }
}

/// Octahedral encoding of a unit vector into two snorm16 values (Cigolle et al. 2014, § 3.1).
pub fn oct_encode(n: Vec3) -> [i16; 2] {
    let n = n.normalize_or(Vec3::Z);
    let l1 = n.x.abs() + n.y.abs() + n.z.abs();
    let mut p = Vec2::new(n.x, n.y) / l1;
    if n.z < 0.0 {
        let s = Vec2::new(p.x.signum_or_one(), p.y.signum_or_one());
        p = (Vec2::ONE - Vec2::new(p.y.abs(), p.x.abs())) * s;
    }
    let q = |x: f32| (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    [q(p.x), q(p.y)]
}

/// Decode `oct_encode`.
pub fn oct_decode(e: [i16; 2]) -> Vec3 {
    let p = Vec2::new(e[0] as f32 / 32767.0, e[1] as f32 / 32767.0).clamp(Vec2::splat(-1.0), Vec2::ONE);
    let mut v = Vec3::new(p.x, p.y, 1.0 - p.x.abs() - p.y.abs());
    if v.z < 0.0 {
        let s = Vec2::new(v.x.signum_or_one(), v.y.signum_or_one());
        let xy = (Vec2::ONE - Vec2::new(v.y.abs(), v.x.abs())) * s;
        v.x = xy.x;
        v.y = xy.y;
    }
    v.normalize()
}

trait SignumOrOne {
    fn signum_or_one(self) -> f32;
}

impl SignumOrOne for f32 {
    fn signum_or_one(self) -> f32 {
        if self >= 0.0 { 1.0 } else { -1.0 }
    }
}

fn quant(x: f32, lo: f32, hi: f32) -> u16 {
    if hi <= lo {
        return 0;
    }
    (((x - lo) / (hi - lo)).clamp(0.0, 1.0) * 65535.0).round() as u16
}

fn dequant(q: u16, lo: f32, hi: f32) -> f32 {
    lo + (hi - lo) * (q as f32 / 65535.0)
}

fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Encode meshes into one `mesh.rvm` blob. Every mesh is validated first.
pub fn encode(meshes: &[MeshData]) -> Result<Vec<u8>, PackError> {
    if meshes.len() > MAX_MESHES {
        return Err(PackError::new(
            "pack_bad_mesh",
            format!("{} meshes over the cap", meshes.len()),
        ));
    }
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&MESH_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(meshes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for m in meshes {
        m.validate()?;
        for s in [&m.name, &m.material_slot] {
            out.extend_from_slice(&(s.len() as u16).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        pad4(&mut out);
        let n = m.positions.len();
        let idx32 = n > 65536;
        let flags = if m.uvs.is_some() { FLAG_UV } else { 0 } | if idx32 { FLAG_IDX32 } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&(m.indices.len() as u32).to_le_bytes());
        let (lo, hi) = m.bounds();
        for v in lo.to_array().into_iter().chain(hi.to_array()) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let uv_bounds = m.uvs.as_ref().map(|uvs| {
            let mut a = Vec2::splat(f32::INFINITY);
            let mut b = Vec2::splat(f32::NEG_INFINITY);
            for uv in uvs {
                a = a.min(Vec2::from(*uv));
                b = b.max(Vec2::from(*uv));
            }
            if uvs.is_empty() {
                (Vec2::ZERO, Vec2::ZERO)
            } else {
                (a, b)
            }
        });
        if let Some((a, b)) = uv_bounds {
            for v in [a.x, a.y, b.x, b.y] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        for p in &m.positions {
            for ((&x, a), b) in p.iter().zip(lo.to_array()).zip(hi.to_array()) {
                out.extend_from_slice(&quant(x, a, b).to_le_bytes());
            }
        }
        pad4(&mut out);
        for nrm in &m.normals {
            for c in oct_encode(Vec3::from(*nrm)) {
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
        if let (Some(uvs), Some((a, b))) = (&m.uvs, uv_bounds) {
            for uv in uvs {
                out.extend_from_slice(&quant(uv[0], a.x, b.x).to_le_bytes());
                out.extend_from_slice(&quant(uv[1], a.y, b.y).to_le_bytes());
            }
        }
        for &i in &m.indices {
            if idx32 {
                out.extend_from_slice(&i.to_le_bytes());
            } else {
                out.extend_from_slice(&(i as u16).to_le_bytes());
            }
        }
        pad4(&mut out);
    }
    Ok(out)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PackError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or_else(|| {
                PackError::new(
                    "pack_bad_mesh",
                    format!("truncated at byte {} (wanted {n} more)", self.at),
                )
            })?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, PackError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, PackError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f32(&mut self) -> Result<f32, PackError> {
        let v = f32::from_bits(self.u32()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(PackError::new("pack_bad_mesh", "non-finite bound"))
        }
    }
    fn string(&mut self) -> Result<String, PackError> {
        let n = self.u16()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| PackError::new("pack_bad_mesh", "name is not utf-8"))
    }
    fn pad4(&mut self) -> Result<(), PackError> {
        let n = (4 - self.at % 4) % 4;
        self.take(n).map(|_| ())
    }
    fn left(&self) -> usize {
        self.b.len() - self.at
    }
}

/// Bytes from `at` through this mesh's closing pad: bounds, positions, normals, optional UVs,
/// indices, and the 4-byte pads the encoder writes. `None` on overflow.
fn mesh_payload_bytes(at: usize, n: usize, index_count: usize, has_uv: bool, idx32: bool) -> Option<usize> {
    let mut rel = 24usize;
    if has_uv {
        rel = rel.checked_add(16)?;
    }
    rel = rel.checked_add(n.checked_mul(6)?)?;
    let mut end = at.checked_add(rel)?.checked_next_multiple_of(4)?;
    rel = end.checked_sub(at)?;
    rel = rel.checked_add(n.checked_mul(4)?)?;
    if has_uv {
        rel = rel.checked_add(n.checked_mul(4)?)?;
    }
    let index_bytes = if idx32 { 4 } else { 2 };
    rel = rel.checked_add(index_count.checked_mul(index_bytes)?)?;
    end = at.checked_add(rel)?.checked_next_multiple_of(4)?;
    end.checked_sub(at)
}

/// Decode a `mesh.rvm` blob.
pub fn decode(bytes: &[u8]) -> Result<Vec<MeshData>, PackError> {
    let mut r = Reader { b: bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return Err(PackError::new("pack_bad_mesh", "not a Regia viewer mesh (magic)"));
    }
    let version = r.u32()?;
    if version != MESH_FORMAT_VERSION {
        return Err(PackError::new(
            "pack_version",
            format!("mesh format_version {version}; this build reads {MESH_FORMAT_VERSION}"),
        ));
    }
    let count = r.u32()? as usize;
    let _reserved = r.u32()?;
    if count > MAX_MESHES {
        return Err(PackError::new(
            "pack_bad_mesh",
            format!("{count} meshes over the cap"),
        ));
    }
    let mut meshes = Vec::with_capacity(count.min(r.left() / 40 + 1));
    for _ in 0..count {
        let name = r.string()?;
        let material_slot = r.string()?;
        r.pad4()?;
        let flags = r.u32()?;
        if flags & !(FLAG_UV | FLAG_IDX32) != 0 {
            return Err(PackError::new(
                "pack_bad_mesh",
                format!("mesh {name}: unknown flags {flags:#x}"),
            ));
        }
        let n = r.u32()? as usize;
        let ic = r.u32()? as usize;
        if n > MAX_VERTICES || ic > MAX_INDICES || !ic.is_multiple_of(3) {
            return Err(PackError::new(
                "pack_bad_mesh",
                format!("mesh {name}: {n} vertices / {ic} indices"),
            ));
        }
        let has_uv = flags & FLAG_UV != 0;
        let idx32 = flags & FLAG_IDX32 != 0;
        // Everything this mesh still needs must be in the file before anything is allocated.
        let Some(need) = mesh_payload_bytes(r.at, n, ic, has_uv, idx32) else {
            return Err(PackError::new(
                "pack_bad_mesh",
                format!("mesh {name}: counts exceed the file"),
            ));
        };
        if need > r.left() {
            return Err(PackError::new(
                "pack_bad_mesh",
                format!("mesh {name}: counts exceed the file"),
            ));
        }
        let mut lo = [0f32; 3];
        let mut hi = [0f32; 3];
        for v in lo.iter_mut().chain(hi.iter_mut()) {
            *v = r.f32()?;
        }
        let uvb = if has_uv {
            Some([r.f32()?, r.f32()?, r.f32()?, r.f32()?])
        } else {
            None
        };
        let mut positions = Vec::with_capacity(n);
        for _ in 0..n {
            let mut p = [0f32; 3];
            for ((slot, a), b) in p.iter_mut().zip(lo).zip(hi) {
                *slot = dequant(r.u16()?, a, b);
            }
            positions.push(p);
        }
        r.pad4()?;
        let mut normals = Vec::with_capacity(n);
        for _ in 0..n {
            let e = [r.u16()? as i16, r.u16()? as i16];
            normals.push(oct_decode(e).to_array());
        }
        let uvs = match uvb {
            Some([ax, ay, bx, by]) => {
                let mut uvs = Vec::with_capacity(n);
                for _ in 0..n {
                    uvs.push([dequant(r.u16()?, ax, bx), dequant(r.u16()?, ay, by)]);
                }
                Some(uvs)
            }
            None => None,
        };
        let mut indices = Vec::with_capacity(ic);
        for _ in 0..ic {
            let i = if idx32 { r.u32()? } else { r.u16()? as u32 };
            if i as usize >= n {
                return Err(PackError::new(
                    "pack_bad_mesh",
                    format!("mesh {name}: index {i} >= {n}"),
                ));
            }
            indices.push(i);
        }
        r.pad4()?;
        let mesh = MeshData {
            name,
            material_slot,
            positions,
            normals,
            uvs,
            indices,
        };
        // Finite bounds can still dequantize to infinity or NaN (a span past f32::MAX): what comes
        // out must pass the same check the encoder applies, or a hostile file feeds NaN downstream.
        mesh.validate()?;
        meshes.push(mesh);
    }
    if r.left() != 0 {
        return Err(PackError::new(
            "pack_bad_mesh",
            format!("{} trailing bytes", r.left()),
        ));
    }
    Ok(meshes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri() -> MeshData {
        MeshData {
            name: "part:x".into(),
            material_slot: "body".into(),
            positions: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 5.0, -2.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, -1.0, 0.0], [0.577, -0.577, -0.577]],
            uvs: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]),
            indices: vec![0, 1, 2],
        }
    }

    #[test]
    fn round_trip_within_quantization_error() {
        let m = tri();
        let back = decode(&encode(std::slice::from_ref(&m)).expect("ok")).expect("ok");
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!((b.name.as_str(), b.material_slot.as_str()), ("part:x", "body"));
        assert_eq!(b.indices, m.indices);
        for (p, q) in m.positions.iter().zip(&b.positions) {
            for (x, y) in p.iter().zip(q) {
                assert!((x - y).abs() <= 10.0 / 65535.0, "{p:?} {q:?}");
            }
        }
        for (n, q) in m.normals.iter().zip(&b.normals) {
            let angle = Vec3::from(*n)
                .normalize()
                .angle_between(Vec3::from(*q))
                .to_degrees();
            assert!(angle < 0.05, "{angle}");
        }
    }

    #[test]
    fn octahedral_covers_the_sphere() {
        let mut worst: f32 = 0.0;
        for i in 0..2000 {
            let z = -1.0 + 2.0 * (i as f32 + 0.5) / 2000.0;
            let phi = i as f32 * 2.399_963;
            let r = (1.0 - z * z).sqrt();
            let n = Vec3::new(r * phi.cos(), r * phi.sin(), z);
            worst = worst.max(n.angle_between(oct_decode(oct_encode(n))).to_degrees());
        }
        for n in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
            worst = worst.max(n.angle_between(oct_decode(oct_encode(n))).to_degrees());
        }
        assert!(worst < 0.05, "{worst}");
    }

    #[test]
    fn large_meshes_use_32_bit_indices() {
        let n = 70_000;
        let m = MeshData {
            name: "big".into(),
            material_slot: "body".into(),
            positions: (0..n).map(|i| [i as f32, 0.0, 0.0]).collect(),
            normals: vec![[0.0, 0.0, 1.0]; n],
            uvs: None,
            indices: vec![0, 1, 69_999],
        };
        let back = decode(&encode(std::slice::from_ref(&m)).expect("ok")).expect("ok");
        assert_eq!(back[0].indices, vec![0, 1, 69_999]);
    }

    #[test]
    fn every_truncation_and_bit_flip_is_refused_or_valid() {
        let bytes = encode(&[
            tri(),
            MeshData {
                name: "b".into(),
                uvs: None,
                ..tri()
            },
        ])
        .expect("ok");
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_err(), "truncated at {cut} decoded");
        }
        for i in 0..bytes.len() {
            for bit in 0..8 {
                let mut b = bytes.clone();
                b[i] ^= 1 << bit;
                if let Ok(meshes) = decode(&b) {
                    for m in meshes {
                        m.validate().expect("ok");
                    }
                }
            }
        }
    }

    /// Correctness audit (corr-web-viewer): finite bounds whose span overflows f32 dequantize to NaN.
    /// The decoder refuses such a file instead of handing NaN positions to the renderer.
    #[test]
    fn corr_overflowing_bounds_are_refused_by_the_decoder() {
        let mut bytes = encode(&[tri()]).expect("ok");
        // tri()'s bounds: min (0, 0, -2), max (10, 5, 0), stored as six f32 in a row.
        let pattern: Vec<u8> = [0.0f32, 0.0, -2.0, 10.0, 5.0, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let at = bytes
            .windows(pattern.len())
            .position(|w| w == pattern.as_slice())
            .expect("bounds in the file");
        bytes[at..at + 4].copy_from_slice(&(-3.0e38f32).to_le_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&3.0e38f32.to_le_bytes());
        let err = decode(&bytes).expect_err("a span past f32::MAX is refused");
        assert_eq!(err.code, "pack_bad_mesh", "{err}");
    }

    /// Correctness audit (corr-web-viewer): the encoder refuses a mesh it cannot quantize, instead of
    /// writing a file that decodes to NaN.
    #[test]
    fn corr_overflowing_extent_is_not_encoded() {
        let mut m = tri();
        m.positions[0][0] = -3.0e38;
        m.positions[1][0] = 3.0e38;
        assert_eq!(encode(&[m]).expect_err("refused").code, "pack_bad_mesh");
        let mut m = tri();
        m.uvs = Some(vec![[-3.0e38, 0.0], [3.0e38, 0.0], [0.0, 1.0]]);
        assert_eq!(encode(&[m]).expect_err("refused").code, "pack_bad_mesh");
    }

    #[test]
    fn invalid_meshes_are_not_encoded() {
        let mut m = tri();
        m.indices = vec![0, 1, 3];
        assert_eq!(encode(&[m]).expect_err("refused").code, "pack_bad_mesh");
        let mut m = tri();
        m.positions[0][0] = f32::NAN;
        assert_eq!(encode(&[m]).expect_err("refused").code, "pack_bad_mesh");
    }
}
