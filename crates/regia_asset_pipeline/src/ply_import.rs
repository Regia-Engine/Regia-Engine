// SPDX-License-Identifier: MPL-2.0
//! Stanford PLY mesh import (pure Rust; `book/agents/mesh-image-import.md`, ADR-3244).
//!
//! A `.ply` file (photogrammetry and scan meshes, vertex colors) is converted into an in-memory
//! glTF 2.0 binary and cooked by the glTF importer, as OBJ is (ADR-1602). The format is Greg
//! Turk's public "The PLY Polygon File Format" description: an ASCII header naming `element`s
//! and their `property`s, then the body in `ascii`, `binary_little_endian` or
//! `binary_big_endian`.
//!
//! Read:
//!
//! * the `vertex` element: `x y z`, `nx ny nz`, colors (`red green blue [alpha]`, also `r g b a`
//!   and `diffuse_red ...`; 8-bit and 16-bit integers are normalized, floats are taken as `0..1`),
//!   and texture coordinates (`s t`, `u v`, `texture_u texture_v`, `texture_s texture_t`);
//! * the `face` element: a list property `vertex_indices` (or `vertex_index`), fan-triangulated
//!   from the first corner, and an optional per-corner `texcoord` list (two floats a corner, as
//!   MeshLab writes it; such corners split vertices where their coordinates differ);
//! * every other element and property is read and ignored (so the stream stays in step).
//!
//! V is flipped to glTF's top-left origin, as for OBJ. PLY has no unit or axis metadata: it is
//! read as meters and +Y up unless the `.meta` settings say otherwise (`scale`, `up_axis`).
//! A file with vertices but no faces (a point cloud) is refused with `ply_no_faces`.

use std::collections::HashMap;
use std::path::Path;

use regia_asset::AssetId;
use serde::{Deserialize, Serialize};

use crate::gltf_import::{GltfImportResult, import_gltf_converted};
use crate::gltf_write::GlbPrimitive;
use crate::lod_classes::LodThresholdInputs;
use crate::mesh_convert::{UpAxis, read_capped, sane_scale, single_primitive_glb};
use crate::texture_compress::TextureCookQuality;
use crate::{Forward, ImportError};

/// Caps on one PLY import (untrusted input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlyLimits {
    /// Largest file, in bytes (default 1 GiB: scans are big).
    pub max_file_bytes: u64,
    /// Most vertices (default 16 Mi).
    pub max_vertices: usize,
    /// Most faces (default 16 Mi).
    pub max_faces: usize,
    /// Most output triangles (default 32 Mi).
    pub max_triangles: usize,
    /// Most corners in one face (default 1,024).
    pub max_polygon_corners: usize,
    /// Most elements and most properties in one element (default 64).
    pub max_header_items: usize,
    /// Largest header, in bytes (default 64 KiB).
    pub max_header_bytes: usize,
}

impl Default for PlyLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1 << 30,
            max_vertices: 16 << 20,
            max_faces: 16 << 20,
            max_triangles: 32 << 20,
            max_polygon_corners: 1024,
            max_header_items: 64,
            max_header_bytes: 64 << 10,
        }
    }
}

/// PLY importer settings (the `.meta` `settings` string, next to the glTF keys).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlyImportSettings {
    /// Uniform scale (PLY has no unit; default 1 = meters).
    pub scale: f32,
    /// Which axis of the file points up (default `y`).
    pub up_axis: UpAxis,
    /// Hard caps.
    pub limits: PlyLimits,
}

impl Default for PlyImportSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            up_axis: UpAxis::Y,
            limits: PlyLimits::default(),
        }
    }
}

impl PlyImportSettings {
    /// Settings from the `.meta` `settings` string; blank is the defaults, glTF keys are
    /// ignored, a malformed string is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, PlyError> {
        match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| PlyError::Settings(e.to_string())),
            None => Ok(Self::default()),
        }
    }
}

/// Why a PLY import failed. [`PlyError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PlyError {
    /// The file could not be read.
    #[error("PLY file read error: {0}")]
    Io(String),
    /// The file is larger than [`PlyLimits::max_file_bytes`].
    #[error("PLY file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// A malformed header or body.
    #[error("PLY: {0}")]
    Parse(String),
    /// A count limit was exceeded.
    #[error("PLY import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("PLY importer settings: {0}")]
    Settings(String),
    /// The file has no faces (a point cloud).
    #[error("PLY file has no faces (point clouds are not imported)")]
    NoFaces,
}

impl PlyError {
    /// The agent-protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            PlyError::Io(_) => "ply_io",
            PlyError::TooLarge { .. } => "ply_too_large",
            PlyError::Parse(_) => "ply_parse",
            PlyError::Limit(_) => "ply_limit",
            PlyError::Settings(_) => "ply_bad_settings",
            PlyError::NoFaces => "ply_no_faces",
        }
    }
}

/// How the body is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlyEncoding {
    /// `format ascii 1.0`.
    Ascii,
    /// `format binary_little_endian 1.0`.
    BinaryLittleEndian,
    /// `format binary_big_endian 1.0`.
    BinaryBigEndian,
}

impl PlyEncoding {
    fn header_name(self) -> &'static str {
        match self {
            PlyEncoding::Ascii => "ascii",
            PlyEncoding::BinaryLittleEndian => "binary_little_endian",
            PlyEncoding::BinaryBigEndian => "binary_big_endian",
        }
    }
}

/// What a PLY conversion found.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlyImportReport {
    /// The body encoding.
    pub encoding: &'static str,
    /// Output vertices.
    pub vertices: usize,
    /// Output triangles.
    pub triangles: usize,
    /// The file carries vertex normals.
    pub has_normals: bool,
    /// The file carries vertex colors.
    pub has_colors: bool,
    /// The file carries texture coordinates.
    pub has_uvs: bool,
    /// Faces with fewer than three corners, skipped.
    pub faces_skipped: usize,
    /// Problems that did not stop the import.
    pub warnings: Vec<String>,
}

/// A PLY file converted to an in-memory glTF 2.0 binary.
#[derive(Clone, Debug)]
pub struct PlyConversion {
    /// The GLB bytes the glTF importer cooks.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: PlyImportReport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Ty {
    fn parse(name: &str) -> Option<Ty> {
        Some(match name {
            "char" | "int8" => Ty::I8,
            "uchar" | "uint8" => Ty::U8,
            "short" | "int16" => Ty::I16,
            "ushort" | "uint16" => Ty::U16,
            "int" | "int32" => Ty::I32,
            "uint" | "uint32" => Ty::U32,
            "float" | "float32" => Ty::F32,
            "double" | "float64" => Ty::F64,
            _ => return None,
        })
    }

    fn is_float(self) -> bool {
        matches!(self, Ty::F32 | Ty::F64)
    }

    /// A color channel value as `0..=1`.
    fn normalize(self, v: f64) -> f32 {
        let scaled = match self {
            Ty::U8 => v / 255.0,
            Ty::U16 => v / 65535.0,
            Ty::U32 => v / f64::from(u32::MAX),
            Ty::I8 => v / 127.0,
            Ty::I16 => v / 32767.0,
            Ty::I32 => v / f64::from(i32::MAX),
            Ty::F32 | Ty::F64 => v,
        };
        if scaled.is_finite() {
            scaled.clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug)]
enum PropKind {
    Scalar(Ty),
    List { count: Ty, item: Ty },
}

#[derive(Clone, Debug)]
struct Property {
    name: String,
    kind: PropKind,
}

#[derive(Clone, Debug)]
struct Element {
    name: String,
    count: u64,
    props: Vec<Property>,
}

struct Header {
    encoding: PlyEncoding,
    elements: Vec<Element>,
    /// Byte offset of the body.
    body: usize,
}

fn parse_header(bytes: &[u8], limits: &PlyLimits) -> Result<Header, PlyError> {
    let bad = |m: String| PlyError::Parse(m);
    let mut pos = 0usize;
    let mut encoding = None;
    let mut elements: Vec<Element> = Vec::new();
    let mut first = true;
    loop {
        if pos >= bytes.len() || pos > limits.max_header_bytes {
            return Err(bad("the header has no end_header line".into()));
        }
        let end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|n| pos + n)
            .ok_or_else(|| bad("the header has no end_header line".into()))?;
        let line = String::from_utf8_lossy(&bytes[pos..end]);
        let line = line.trim();
        pos = end + 1;
        let mut words = line.split_whitespace();
        let keyword = words.next().unwrap_or("");
        if first {
            if keyword != "ply" {
                return Err(bad("not a PLY file (the first line is not \"ply\")".into()));
            }
            first = false;
            continue;
        }
        match keyword {
            "format" => {
                encoding = Some(match words.next() {
                    Some("ascii") => PlyEncoding::Ascii,
                    Some("binary_little_endian") => PlyEncoding::BinaryLittleEndian,
                    Some("binary_big_endian") => PlyEncoding::BinaryBigEndian,
                    other => return Err(bad(format!("unknown format {other:?}"))),
                });
            }
            "element" => {
                if elements.len() >= limits.max_header_items {
                    return Err(PlyError::Limit(format!(
                        "more than {} elements",
                        limits.max_header_items
                    )));
                }
                let name = words.next().ok_or_else(|| bad("an element has no name".into()))?;
                let count: u64 = words
                    .next()
                    .and_then(|c| c.parse().ok())
                    .ok_or_else(|| bad(format!("element {name} has no valid count")))?;
                elements.push(Element {
                    name: name.to_string(),
                    count,
                    props: Vec::new(),
                });
            }
            "property" => {
                let element = elements
                    .last_mut()
                    .ok_or_else(|| bad("a property before any element".into()))?;
                if element.props.len() >= limits.max_header_items {
                    return Err(PlyError::Limit(format!(
                        "more than {} properties in element {}",
                        limits.max_header_items, element.name
                    )));
                }
                let first = words.next().ok_or_else(|| bad("an empty property".into()))?;
                let ty = |t: Option<&str>| {
                    t.and_then(Ty::parse)
                        .ok_or_else(|| bad(format!("unknown property type {t:?}")))
                };
                let kind = if first == "list" {
                    let count = ty(words.next())?;
                    if count.is_float() {
                        return Err(bad("a list count must be an integer type".into()));
                    }
                    PropKind::List {
                        count,
                        item: ty(words.next())?,
                    }
                } else {
                    PropKind::Scalar(ty(Some(first))?)
                };
                let name = words.next().ok_or_else(|| bad("a property has no name".into()))?;
                element.props.push(Property {
                    name: name.to_string(),
                    kind,
                });
            }
            "end_header" => break,
            // comment, obj_info and anything unknown.
            _ => {}
        }
    }
    let encoding = encoding.ok_or_else(|| bad("the header has no format line".into()))?;
    Ok(Header {
        encoding,
        elements,
        body: pos,
    })
}

/// Sequential reader of the body.
struct Body<'a> {
    data: &'a [u8],
    pos: usize,
    encoding: PlyEncoding,
}

impl Body<'_> {
    fn truncated() -> PlyError {
        PlyError::Parse("the body ends early".into())
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], PlyError> {
        let end = self.pos.checked_add(N).ok_or_else(Self::truncated)?;
        let slice = self.data.get(self.pos..end).ok_or_else(Self::truncated)?;
        self.pos = end;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn ascii_token(&mut self) -> Result<&str, PlyError> {
        while self.data.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
        let start = self.pos;
        while self.data.get(self.pos).is_some_and(|b| !b.is_ascii_whitespace()) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(Self::truncated());
        }
        std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| PlyError::Parse("non-text body bytes".into()))
    }

    fn scalar(&mut self, ty: Ty) -> Result<f64, PlyError> {
        if self.encoding == PlyEncoding::Ascii {
            let token = self.ascii_token()?;
            return token
                .parse::<f64>()
                .map_err(|_| PlyError::Parse(format!("{token:?} is not a number")));
        }
        let big = self.encoding == PlyEncoding::BinaryBigEndian;
        macro_rules! num {
            ($t:ty, $n:expr) => {{
                let raw = self.take::<$n>()?;
                if big {
                    <$t>::from_be_bytes(raw)
                } else {
                    <$t>::from_le_bytes(raw)
                }
            }};
        }
        Ok(match ty {
            Ty::I8 => f64::from(num!(i8, 1)),
            Ty::U8 => f64::from(num!(u8, 1)),
            Ty::I16 => f64::from(num!(i16, 2)),
            Ty::U16 => f64::from(num!(u16, 2)),
            Ty::I32 => f64::from(num!(i32, 4)),
            Ty::U32 => f64::from(num!(u32, 4)),
            Ty::F32 => f64::from(num!(f32, 4)),
            Ty::F64 => num!(f64, 8),
        })
    }

    /// A list length: a non-negative integer.
    fn list_len(&mut self, ty: Ty) -> Result<usize, PlyError> {
        let v = self.scalar(ty)?;
        if v.fract() != 0.0 || !(0.0..=f64::from(u32::MAX)).contains(&v) {
            return Err(PlyError::Parse(format!("list length {v} is not a count")));
        }
        Ok(v as usize)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    X,
    Y,
    Z,
    Nx,
    Ny,
    Nz,
    R,
    G,
    B,
    A,
    U,
    V,
}

fn vertex_role(name: &str) -> Option<Role> {
    Some(match name {
        "x" => Role::X,
        "y" => Role::Y,
        "z" => Role::Z,
        "nx" => Role::Nx,
        "ny" => Role::Ny,
        "nz" => Role::Nz,
        "red" | "r" | "diffuse_red" => Role::R,
        "green" | "g" | "diffuse_green" => Role::G,
        "blue" | "b" | "diffuse_blue" => Role::B,
        "alpha" | "a" | "diffuse_alpha" => Role::A,
        "s" | "u" | "texture_u" | "texture_s" => Role::U,
        "t" | "v" | "texture_v" | "texture_t" => Role::V,
        _ => return None,
    })
}

struct Vertices {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    uvs: Vec<[f32; 2]>,
}

fn finite(v: f64) -> f32 {
    if v.is_finite() { v as f32 } else { 0.0 }
}

fn read_vertices(body: &mut Body<'_>, element: &Element, limits: &PlyLimits) -> Result<Vertices, PlyError> {
    if element.count > limits.max_vertices as u64 {
        return Err(PlyError::Limit(format!(
            "{} vertices (limit {})",
            element.count, limits.max_vertices
        )));
    }
    let n = element.count as usize;
    let roles: Vec<Option<Role>> = element
        .props
        .iter()
        .map(|p| match p.kind {
            PropKind::Scalar(_) => vertex_role(&p.name),
            PropKind::List { .. } => None,
        })
        .collect();
    let has = |r: Role| roles.contains(&Some(r));
    if !(has(Role::X) && has(Role::Y) && has(Role::Z)) {
        return Err(PlyError::Parse("the vertex element lacks x, y and z".into()));
    }
    let want_normals = has(Role::Nx) && has(Role::Ny) && has(Role::Nz);
    let want_colors = has(Role::R) && has(Role::G) && has(Role::B);
    let want_uvs = has(Role::U) && has(Role::V);
    let cap = n.min(1 << 20);
    let mut out = Vertices {
        positions: Vec::with_capacity(cap),
        normals: Vec::new(),
        colors: Vec::new(),
        uvs: Vec::new(),
    };
    for _ in 0..n {
        let mut p = [0.0f32; 3];
        let mut nrm = [0.0f32; 3];
        let mut c = [0.0f32, 0.0, 0.0, 1.0];
        let mut uv = [0.0f32; 2];
        for (prop, role) in element.props.iter().zip(&roles) {
            match prop.kind {
                PropKind::Scalar(ty) => {
                    let v = body.scalar(ty)?;
                    match role {
                        Some(Role::X) => p[0] = finite(v),
                        Some(Role::Y) => p[1] = finite(v),
                        Some(Role::Z) => p[2] = finite(v),
                        Some(Role::Nx) => nrm[0] = finite(v),
                        Some(Role::Ny) => nrm[1] = finite(v),
                        Some(Role::Nz) => nrm[2] = finite(v),
                        Some(Role::R) => c[0] = ty.normalize(v),
                        Some(Role::G) => c[1] = ty.normalize(v),
                        Some(Role::B) => c[2] = ty.normalize(v),
                        Some(Role::A) => c[3] = ty.normalize(v),
                        Some(Role::U) => uv[0] = finite(v),
                        Some(Role::V) => uv[1] = 1.0 - finite(v),
                        None => {}
                    }
                }
                PropKind::List { count, item } => {
                    let len = body.list_len(count)?;
                    if len > limits.max_polygon_corners {
                        return Err(PlyError::Limit(format!("a vertex list of {len} items")));
                    }
                    for _ in 0..len {
                        body.scalar(item)?;
                    }
                }
            }
        }
        out.positions.push(p);
        if want_normals {
            out.normals.push(nrm);
        }
        if want_colors {
            out.colors.push(c);
        }
        if want_uvs {
            out.uvs.push(uv);
        }
    }
    Ok(out)
}

/// One face: corner vertex indices and, when the file stores them per corner, the corner UVs.
struct Face {
    corners: Vec<u32>,
    corner_uvs: Option<Vec<[f32; 2]>>,
}

fn read_faces(
    body: &mut Body<'_>,
    element: &Element,
    vertex_count: usize,
    limits: &PlyLimits,
) -> Result<Vec<Face>, PlyError> {
    if element.count > limits.max_faces as u64 {
        return Err(PlyError::Limit(format!(
            "{} faces (limit {})",
            element.count, limits.max_faces
        )));
    }
    let index_prop = element
        .props
        .iter()
        .position(|p| {
            matches!(p.kind, PropKind::List { .. })
                && matches!(p.name.as_str(), "vertex_indices" | "vertex_index")
        })
        .ok_or_else(|| PlyError::Parse("the face element has no vertex_indices list".into()))?;
    let uv_prop = element
        .props
        .iter()
        .position(|p| matches!(p.kind, PropKind::List { .. }) && p.name == "texcoord");
    let mut faces = Vec::with_capacity((element.count as usize).min(1 << 20));
    for _ in 0..element.count {
        let mut corners = Vec::new();
        let mut corner_uvs = None;
        for (i, prop) in element.props.iter().enumerate() {
            match prop.kind {
                PropKind::Scalar(ty) => {
                    body.scalar(ty)?;
                }
                PropKind::List { count, item } => {
                    let len = body.list_len(count)?;
                    if len > limits.max_polygon_corners * 2 {
                        return Err(PlyError::Limit(format!("a face list of {len} items")));
                    }
                    if i == index_prop {
                        if len > limits.max_polygon_corners {
                            return Err(PlyError::Limit(format!(
                                "a face with {len} corners (limit {})",
                                limits.max_polygon_corners
                            )));
                        }
                        for _ in 0..len {
                            let v = body.scalar(item)?;
                            if v.fract() != 0.0 || v < 0.0 || v >= vertex_count as f64 {
                                return Err(PlyError::Parse(format!(
                                    "face index {v} outside the {vertex_count} vertices"
                                )));
                            }
                            corners.push(v as u32);
                        }
                    } else if Some(i) == uv_prop {
                        let mut values = Vec::with_capacity(len);
                        for _ in 0..len {
                            values.push(finite(body.scalar(item)?));
                        }
                        corner_uvs = Some(
                            values
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|c| [c[0], 1.0 - c[1]])
                                .collect::<Vec<_>>(),
                        );
                    } else {
                        for _ in 0..len {
                            body.scalar(item)?;
                        }
                    }
                }
            }
        }
        faces.push(Face { corners, corner_uvs });
    }
    Ok(faces)
}

/// Skips the instances of an element the importer does not use.
fn skip_element(body: &mut Body<'_>, element: &Element, limits: &PlyLimits) -> Result<(), PlyError> {
    let max = limits.max_vertices.max(limits.max_faces) as u64;
    if element.count > max {
        return Err(PlyError::Limit(format!(
            "element {} has {} items (limit {max})",
            element.name, element.count
        )));
    }
    if element.props.is_empty() {
        return Ok(());
    }
    for _ in 0..element.count {
        for prop in &element.props {
            match prop.kind {
                PropKind::Scalar(ty) => {
                    body.scalar(ty)?;
                }
                PropKind::List { count, item } => {
                    let len = body.list_len(count)?;
                    if len > limits.max_polygon_corners * 64 {
                        return Err(PlyError::Limit(format!(
                            "a list of {len} items in {}",
                            element.name
                        )));
                    }
                    for _ in 0..len {
                        body.scalar(item)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Converts PLY bytes to an in-memory glTF 2.0 binary.
pub fn convert_ply_to_glb(bytes: &[u8], settings: &PlyImportSettings) -> Result<PlyConversion, PlyError> {
    let _span = tracing::info_span!("ply_convert", bytes = bytes.len()).entered();
    let limits = &settings.limits;
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(PlyError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    let header = parse_header(bytes, limits)?;
    let mut body = Body {
        data: bytes,
        pos: header.body,
        encoding: header.encoding,
    };
    let mut vertices: Option<Vertices> = None;
    let mut faces: Option<Vec<Face>> = None;
    for element in &header.elements {
        match element.name.as_str() {
            "vertex" if vertices.is_none() => {
                vertices = Some(read_vertices(&mut body, element, limits)?);
            }
            "face" if faces.is_none() => {
                let vertex_count = vertices.as_ref().map_or(0, |v| v.positions.len());
                if vertices.is_none() {
                    return Err(PlyError::Parse(
                        "the face element comes before the vertex element".into(),
                    ));
                }
                faces = Some(read_faces(&mut body, element, vertex_count, limits)?);
            }
            _ => skip_element(&mut body, element, limits)?,
        }
    }
    let vertices = vertices.ok_or_else(|| PlyError::Parse("the file has no vertex element".into()))?;
    let faces = faces.ok_or(PlyError::NoFaces)?;

    let scale = sane_scale(settings.scale);
    let up = settings.up_axis;
    let has_normals = !vertices.normals.is_empty();
    let has_colors = !vertices.colors.is_empty();
    let per_corner_uvs = faces.iter().any(|f| f.corner_uvs.is_some());
    let has_uvs = !vertices.uvs.is_empty() || per_corner_uvs;

    let mut report = PlyImportReport {
        encoding: header.encoding.header_name(),
        vertices: 0,
        triangles: 0,
        has_normals,
        has_colors,
        has_uvs,
        faces_skipped: 0,
        warnings: Vec::new(),
    };
    let mut primitive = GlbPrimitive::default();
    let mut corner_vertex: HashMap<(u32, u32, u32), u32> = HashMap::new();
    let mut uv_layer: Vec<[f32; 2]> = Vec::new();
    let mut emit = |v: u32, uv: Option<[f32; 2]>, primitive: &mut GlbPrimitive| -> Result<u32, PlyError> {
        let key = match uv {
            Some(uv) => (v, uv[0].to_bits(), uv[1].to_bits()),
            None => (v, 0, 0),
        };
        if let Some(&existing) = corner_vertex.get(&key) {
            return Ok(existing);
        }
        let index = primitive.positions.len();
        if index >= limits.max_vertices {
            return Err(PlyError::Limit(format!(
                "more than {} output vertices",
                limits.max_vertices
            )));
        }
        let i = v as usize;
        let p = vertices.positions[i];
        primitive
            .positions
            .push(up.apply([p[0] * scale, p[1] * scale, p[2] * scale]));
        if has_normals {
            primitive.normals.push(up.apply(vertices.normals[i]));
        }
        if has_colors {
            primitive.colors.push(vertices.colors[i]);
        }
        if has_uvs {
            uv_layer.push(uv.unwrap_or_else(|| vertices.uvs.get(i).copied().unwrap_or([0.0; 2])));
        }
        corner_vertex.insert(key, index as u32);
        Ok(index as u32)
    };
    for face in &faces {
        if face.corners.len() < 3 {
            report.faces_skipped += 1;
            continue;
        }
        let uv_at = |k: usize| {
            face.corner_uvs
                .as_ref()
                .filter(|u| u.len() == face.corners.len())
                .map(|u| u[k])
        };
        for k in 1..face.corners.len() - 1 {
            if primitive.indices.len() / 3 >= limits.max_triangles {
                return Err(PlyError::Limit(format!(
                    "more than {} triangles",
                    limits.max_triangles
                )));
            }
            for corner in [0, k, k + 1] {
                let index = emit(face.corners[corner], uv_at(corner), &mut primitive)?;
                primitive.indices.push(index);
            }
        }
    }
    if primitive.indices.is_empty() {
        return Err(PlyError::NoFaces);
    }
    if has_uvs {
        primitive.uvs.push(uv_layer);
    }
    if faces
        .iter()
        .any(|f| f.corner_uvs.as_ref().is_some_and(|u| u.len() != f.corners.len()))
    {
        report
            .warnings
            .push("a face's texcoord list does not match its corner count; the vertex UVs are used".into());
    }
    report.vertices = primitive.positions.len();
    report.triangles = primitive.indices.len() / 3;
    for warning in &report.warnings {
        tracing::warn!(target: "regia_asset_pipeline", "PLY import: {warning}");
    }
    Ok(PlyConversion {
        glb: single_primitive_glb("Regia Engine PLY import", "ply", primitive)
            .map_err(|err| PlyError::Parse(err.to_string()))?,
        report,
    })
}

/// Reads a `.ply` file within `limits.max_file_bytes`.
pub fn read_ply_file(path: &Path, limits: &PlyLimits) -> Result<Vec<u8>, PlyError> {
    match read_capped(path, limits.max_file_bytes) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(message)) => Err(PlyError::Io(message)),
        Err(size) => Err(PlyError::TooLarge {
            size,
            limit: limits.max_file_bytes,
        }),
    }
}

/// A PLY file imported through the glTF importer.
#[derive(Clone, Debug)]
pub struct PlyImportResult {
    /// The glTF importer's result for the converted scene.
    pub gltf: GltfImportResult,
    /// The converted GLB.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: PlyImportReport,
}

/// Imports a `.ply` from disk through the glTF importer, with the `.meta` settings string.
pub fn import_ply_path(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
) -> Result<PlyImportResult, ImportError> {
    let settings = PlyImportSettings::try_from_meta_string(meta_settings)?;
    let bytes = read_ply_file(source, &settings.limits)?;
    let conversion = convert_ply_to_glb(&bytes, &settings)?;
    let lod = LodThresholdInputs::resolve(&[], "", None).0;
    let gltf = import_gltf_converted(
        &conversion.glb,
        source.parent(),
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &lod,
    )?;
    Ok(PlyImportResult {
        gltf,
        glb: conversion.glb,
        report: conversion.report,
    })
}

/// A valid PLY cube in `encoding`: 8 vertices (`x y z`, `nx ny nz`, `uchar red green blue`,
/// `s t`) and 6 quad faces. The fuzz fixture and the tests build on it.
#[must_use]
pub fn cube_fixture(encoding: PlyEncoding) -> Vec<u8> {
    const CORNERS: [[f32; 3]; 8] = [
        [-1.0, -1.0, -1.0],
        [1.0, -1.0, -1.0],
        [1.0, 1.0, -1.0],
        [-1.0, 1.0, -1.0],
        [-1.0, -1.0, 1.0],
        [1.0, -1.0, 1.0],
        [1.0, 1.0, 1.0],
        [-1.0, 1.0, 1.0],
    ];
    const FACES: [[u32; 4]; 6] = [
        [0, 3, 2, 1],
        [4, 5, 6, 7],
        [0, 1, 5, 4],
        [2, 3, 7, 6],
        [0, 4, 7, 3],
        [1, 2, 6, 5],
    ];
    let mut out = format!(
        "ply\nformat {} 1.0\ncomment Regia Engine fixture\nelement vertex 8\n\
         property float x\nproperty float y\nproperty float z\n\
         property float nx\nproperty float ny\nproperty float nz\n\
         property uchar red\nproperty uchar green\nproperty uchar blue\n\
         property float s\nproperty float t\n\
         element face 6\nproperty list uchar int vertex_indices\nend_header\n",
        encoding.header_name()
    )
    .into_bytes();
    let n = 1.0 / 3.0f32.sqrt();
    for (i, c) in CORNERS.iter().enumerate() {
        let normal = [c[0] * n, c[1] * n, c[2] * n];
        let rgb = [(i * 30) as u8, 255 - (i * 30) as u8, 128u8];
        let st = [(i % 2) as f32, ((i / 2) % 2) as f32];
        match encoding {
            PlyEncoding::Ascii => {
                let text = format!(
                    "{} {} {} {} {} {} {} {} {} {} {}\n",
                    c[0], c[1], c[2], normal[0], normal[1], normal[2], rgb[0], rgb[1], rgb[2], st[0], st[1]
                );
                out.extend_from_slice(text.as_bytes());
            }
            _ => {
                let big = encoding == PlyEncoding::BinaryBigEndian;
                let f = |v: f32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
                for v in c.iter().chain(&normal) {
                    out.extend_from_slice(&f(*v));
                }
                out.extend_from_slice(&rgb);
                for v in st {
                    out.extend_from_slice(&f(v));
                }
            }
        }
    }
    for face in FACES {
        match encoding {
            PlyEncoding::Ascii => {
                let text = format!("4 {} {} {} {}\n", face[0], face[1], face[2], face[3]);
                out.extend_from_slice(text.as_bytes());
            }
            _ => {
                let big = encoding == PlyEncoding::BinaryBigEndian;
                out.push(4);
                for v in face {
                    let v = i32::try_from(v).unwrap_or(0);
                    out.extend_from_slice(&if big { v.to_be_bytes() } else { v.to_le_bytes() });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODINGS: [PlyEncoding; 3] = [
        PlyEncoding::Ascii,
        PlyEncoding::BinaryLittleEndian,
        PlyEncoding::BinaryBigEndian,
    ];

    #[test]
    fn error_codes_are_protocol_shaped() {
        let errors = [
            PlyError::Io(String::new()),
            PlyError::TooLarge { size: 0, limit: 0 },
            PlyError::Parse(String::new()),
            PlyError::Limit(String::new()),
            PlyError::Settings(String::new()),
            PlyError::NoFaces,
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("ply_"), "{code}");
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn every_encoding_converts_the_cube_alike() {
        let mut globs = Vec::new();
        for encoding in ENCODINGS {
            let c = convert_ply_to_glb(&cube_fixture(encoding), &PlyImportSettings::default())
                .unwrap_or_else(|e| panic!("{encoding:?}: {e}"));
            assert_eq!(c.report.vertices, 8, "{encoding:?}");
            assert_eq!(c.report.triangles, 12, "{encoding:?}");
            assert!(c.report.has_normals && c.report.has_colors && c.report.has_uvs);
            globs.push(c.glb);
        }
        assert_eq!(globs[0], globs[1], "ascii and little-endian write the same mesh");
        assert_eq!(globs[1], globs[2], "little- and big-endian write the same mesh");
    }

    #[test]
    fn a_point_cloud_is_refused() {
        let text = "ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\nend_header\n0 0 0\n1 1 1\n";
        let e = convert_ply_to_glb(text.as_bytes(), &PlyImportSettings::default()).expect_err("no faces");
        assert_eq!(e.code(), "ply_no_faces");
    }

    #[test]
    fn hostile_files_are_refused_without_a_panic() {
        let cube = cube_fixture(PlyEncoding::BinaryLittleEndian);
        let s = PlyImportSettings::default();
        // Every truncation of a valid file.
        for len in 0..cube.len() {
            let _ = convert_ply_to_glb(&cube[..len], &s);
        }
        let cases: [&str; 7] = [
            "ply\nformat ascii 1.0\nelement vertex 4000000000\nproperty float x\nproperty float y\nproperty float z\nend_header\n",
            "ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n0 0 0\n3 0 0 9\n",
            "ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n0 0 0\n200 0 0 0\n",
            "ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n0 0 0\n-1\n",
            "ply\nformat bogus 1.0\nend_header\n",
            "ply\nformat ascii 1.0\nelement vertex 1\nproperty banana x\nend_header\n",
            "plyx\n",
        ];
        for text in cases {
            let e = convert_ply_to_glb(text.as_bytes(), &s).expect_err(text);
            assert!(e.code().starts_with("ply_"), "{e}");
        }
        assert!(convert_ply_to_glb(b"", &s).is_err());
    }

    #[test]
    fn per_corner_texcoords_split_vertices() {
        let text = "ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\n\
            element face 2\nproperty list uchar int vertex_indices\nproperty list uchar float texcoord\nend_header\n\
            0 0 0\n1 0 0\n0 1 0\n\
            3 0 1 2 6 0 0 1 0 0 1\n\
            3 0 2 1 6 0.5 0.5 1 1 0 0\n";
        let c = convert_ply_to_glb(text.as_bytes(), &PlyImportSettings::default()).expect("convert");
        assert_eq!(c.report.triangles, 2);
        assert_eq!(c.report.vertices, 6, "no corner shares both position and UV");
        assert!(c.report.has_uvs);
    }

    #[test]
    fn scale_and_up_axis_apply() {
        let s = PlyImportSettings {
            scale: 2.0,
            up_axis: UpAxis::Z,
            ..PlyImportSettings::default()
        };
        let c = convert_ply_to_glb(&cube_fixture(PlyEncoding::Ascii), &s).expect("convert");
        assert_eq!(c.report.vertices, 8);
    }
}
