// SPDX-License-Identifier: MPL-2.0
//! `<map>.viscells.ron` version 1: cells with oriented-box bounds and convex portal quads between
//! cell pairs (`docs/specs/25-sim-bridge-and-do27.md` § 7.7, ADR-7586). Loading validates every
//! rule and refuses a bad file with a documented error code ([`VisCellsError::code`]); a valid
//! file becomes [`VisCells`], the runtime resource the traversal reads.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

/// The `<map>.viscells.ron` version this engine writes and the newest it reads.
pub const VISCELLS_FORMAT_VERSION: u32 = 1;

/// The outdoor cell: terrain, sky, and every view outside all cell boxes. Never has bounds.
pub const OUTDOOR_CELL: u16 = 0;

/// The most cells one file may list (the outdoor cell included).
pub const MAX_VIS_CELLS: usize = 4096;

/// The most portals one file may list.
pub const MAX_VIS_PORTALS: usize = 16384;

/// The most oriented boxes one cell may have.
pub const MAX_CELL_BOXES: usize = 64;

/// How far a portal corner may sit off the quad's plane, as a share of the quad's longest
/// diagonal (at least [`PORTAL_PLANAR_MIN_M`]).
pub const PORTAL_PLANAR_TOLERANCE: f32 = 0.01;

/// The smallest off-plane distance a portal is allowed, metres (small quads).
pub const PORTAL_PLANAR_MIN_M: f32 = 0.01;

fn one() -> u32 {
    1
}

/// One oriented box of a cell: a box of `half_extents` around `center`, turned by
/// `rotation_radians` about +Y (world metres).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellBox {
    /// Box centre, world metres.
    pub center: [f32; 3],
    /// Half sizes along the box's own X, Y and Z, metres (each above 0).
    pub half_extents: [f32; 3],
    /// Turn about +Y, radians (counter-clockwise seen from above).
    #[serde(default)]
    pub rotation_radians: f32,
}

impl CellBox {
    /// `point` in the box's own frame (centre at the origin, unrotated).
    #[must_use]
    pub fn to_local(&self, point: Vec3) -> Vec3 {
        let d = point - Vec3::from_array(self.center);
        let (s, c) = (-self.rotation_radians).sin_cos();
        // Rotation about +Y by -angle.
        Vec3::new(c * d.x + s * d.z, d.y, -s * d.x + c * d.z)
    }

    /// Whether `point` lies inside the box (its surface included).
    #[must_use]
    pub fn contains(&self, point: Vec3) -> bool {
        let local = self.to_local(point);
        local.x.abs() <= self.half_extents[0]
            && local.y.abs() <= self.half_extents[1]
            && local.z.abs() <= self.half_extents[2]
    }

    /// The distance from `point` to the box, metres (0 inside or on it).
    #[must_use]
    pub fn distance(&self, point: Vec3) -> f32 {
        let local = self.to_local(point).abs();
        let half = Vec3::from_array(self.half_extents);
        (local - half).max(Vec3::ZERO).length()
    }

    /// The box volume, cubic metres (the smaller box wins where cells overlap).
    #[must_use]
    pub fn volume(&self) -> f32 {
        8.0 * self.half_extents[0] * self.half_extents[1] * self.half_extents[2]
    }

    /// The eight corners in world space: bottom face (y −) then top face, each counter-clockwise
    /// from (−x, −z) in the box frame.
    #[must_use]
    pub fn corners(&self) -> [Vec3; 8] {
        let [hx, hy, hz] = self.half_extents;
        let (s, c) = self.rotation_radians.sin_cos();
        let center = Vec3::from_array(self.center);
        let place = |x: f32, y: f32, z: f32| center + Vec3::new(c * x + s * z, y, -s * x + c * z);
        [
            place(-hx, -hy, -hz),
            place(hx, -hy, -hz),
            place(hx, -hy, hz),
            place(-hx, -hy, hz),
            place(-hx, hy, -hz),
            place(hx, hy, -hz),
            place(hx, hy, hz),
            place(-hx, hy, hz),
        ]
    }
}

/// One cell as the file lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisCellDef {
    /// Cell id, unique in the file. 0 is the outdoor cell.
    pub id: u16,
    /// A name for people and the debug read (optional).
    #[serde(default)]
    pub name: String,
    /// Marks the outdoor cell; only id 0 may set it (optional, documentation only).
    #[serde(default)]
    pub outdoor: bool,
    /// The cell's oriented boxes (at least one for an indoor cell, none for cell 0).
    #[serde(default)]
    pub bounds: Vec<CellBox>,
}

/// One portal: a convex quad between cells `a` and `b`, four world points in order. Portals are
/// two-way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisPortalDef {
    /// One side's cell id.
    pub a: u16,
    /// The other side's cell id (never `a`).
    pub b: u16,
    /// The quad's four corners in order around it (either winding), world metres. Exactly four.
    pub quad: Vec<[f32; 3]>,
}

/// The serialized `<map>.viscells.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisCellsDocument {
    /// Format version (absent = 1).
    #[serde(default = "one")]
    pub version: u32,
    /// The cells. Cell 0 (outdoors) exists whether or not it is listed.
    #[serde(default)]
    pub cells: Vec<VisCellDef>,
    /// The portals between cell pairs.
    #[serde(default)]
    pub portals: Vec<VisPortalDef>,
}

impl Default for VisCellsDocument {
    fn default() -> Self {
        Self {
            version: VISCELLS_FORMAT_VERSION,
            cells: Vec::new(),
            portals: Vec::new(),
        }
    }
}

/// Why a `<map>.viscells.ron` was refused. [`Self::code`] is the stable error code (agent
/// protocol, `book/agents/viscells.md`).
#[derive(Clone, Debug, PartialEq)]
pub enum VisCellsError {
    /// Not RON, or not this document's shape.
    Parse(String),
    /// The file's version is newer than [`VISCELLS_FORMAT_VERSION`].
    NewerVersion(u32),
    /// `version: 0`.
    ZeroVersion,
    /// More than [`MAX_VIS_CELLS`] cells, [`MAX_VIS_PORTALS`] portals or [`MAX_CELL_BOXES`] boxes
    /// in a cell.
    TooMany(&'static str),
    /// Two cells share an id.
    DuplicateCell(u16),
    /// An indoor cell (id above 0) has no boxes.
    CellWithoutBounds(u16),
    /// Cell 0 lists boxes (the outdoor cell is everything outside every box).
    OutdoorHasBounds,
    /// A cell other than 0 is marked `outdoor`.
    OutdoorNotZero(u16),
    /// A box has a non-finite value or a half extent at or below 0.
    BadBox(u16),
    /// A portal names a cell the file does not list (0 is always known).
    UnknownCell {
        /// The portal's index in `portals`.
        portal: usize,
        /// The unknown id.
        cell: u16,
    },
    /// A portal joins a cell to itself.
    SelfPortal(usize),
    /// A portal does not list exactly four corners.
    NotQuadPortal(usize),
    /// A portal corner is not finite.
    NonFinitePortal(usize),
    /// A portal's corners do not lie on one plane (within [`PORTAL_PLANAR_TOLERANCE`]).
    NonPlanarPortal(usize),
    /// A portal quad is not convex (a bow tie, a dent, repeated or collinear corners, no area).
    NonConvexPortal(usize),
}

impl VisCellsError {
    /// The stable error code: lowercase ascii, digits and `_`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "viscells_parse",
            Self::NewerVersion(_) => "viscells_version_newer",
            Self::ZeroVersion => "viscells_version_zero",
            Self::TooMany(_) => "viscells_too_many",
            Self::DuplicateCell(_) => "viscells_duplicate_cell",
            Self::CellWithoutBounds(_) => "viscells_cell_no_bounds",
            Self::OutdoorHasBounds => "viscells_outdoor_bounds",
            Self::OutdoorNotZero(_) => "viscells_outdoor_not_zero",
            Self::BadBox(_) => "viscells_bad_box",
            Self::UnknownCell { .. } => "viscells_unknown_cell",
            Self::SelfPortal(_) => "viscells_self_portal",
            Self::NotQuadPortal(_) => "viscells_portal_not_quad",
            Self::NonFinitePortal(_) => "viscells_portal_not_finite",
            Self::NonPlanarPortal(_) => "viscells_portal_not_planar",
            Self::NonConvexPortal(_) => "viscells_portal_not_convex",
        }
    }
}

impl std::fmt::Display for VisCellsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match self {
            Self::Parse(e) => write!(f, "{code}: {e}"),
            Self::NewerVersion(v) => write!(
                f,
                "{code}: viscells version {v} is newer than this engine reads ({VISCELLS_FORMAT_VERSION})"
            ),
            Self::ZeroVersion => write!(f, "{code}: version 0 is not a viscells version"),
            Self::TooMany(what) => write!(f, "{code}: too many {what}"),
            Self::DuplicateCell(id) => write!(f, "{code}: cell {id} is listed twice"),
            Self::CellWithoutBounds(id) => write!(f, "{code}: cell {id} has no bounds"),
            Self::OutdoorHasBounds => write!(f, "{code}: the outdoor cell 0 cannot have bounds"),
            Self::OutdoorNotZero(id) => write!(f, "{code}: cell {id} is marked outdoor; only cell 0 is"),
            Self::BadBox(id) => write!(
                f,
                "{code}: a box of cell {id} is not finite or has a half extent at or below 0"
            ),
            Self::UnknownCell { portal, cell } => {
                write!(
                    f,
                    "{code}: portal {portal} names cell {cell}, which is not listed"
                )
            }
            Self::SelfPortal(p) => write!(f, "{code}: portal {p} joins a cell to itself"),
            Self::NotQuadPortal(p) => write!(f, "{code}: portal {p} does not list exactly four corners"),
            Self::NonFinitePortal(p) => write!(f, "{code}: portal {p} has a corner that is not finite"),
            Self::NonPlanarPortal(p) => write!(f, "{code}: portal {p}'s corners are not on one plane"),
            Self::NonConvexPortal(p) => write!(f, "{code}: portal {p} is not a convex quad"),
        }
    }
}

impl std::error::Error for VisCellsError {}

/// One validated cell.
#[derive(Clone, Debug, PartialEq)]
pub struct VisCellInfo {
    /// The cell id.
    pub id: u16,
    /// Its name (empty when the file gave none).
    pub name: String,
    /// Its boxes (empty for cell 0).
    pub bounds: Vec<CellBox>,
    /// Indices into [`VisCells::portals`] of the portals touching this cell, ascending.
    pub portals: Vec<u32>,
}

/// One validated portal with its plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisPortal {
    /// One side's cell index in [`VisCells::cells`].
    pub a: u32,
    /// The other side's cell index.
    pub b: u32,
    /// The corners, as the file gave them.
    pub quad: [Vec3; 4],
    /// The unit plane normal (Newell), any orientation.
    pub normal: Vec3,
    /// The centroid of the corners.
    pub center: Vec3,
    /// How far from the portal's plane (either side) a point over the quad still stands in this
    /// portal's doorway, metres (ADR-7639, ADR-7646): the larger of the distances from
    /// [`Self::center`] to side `a`'s nearest box and to side `b`'s (0 for the outdoor cell),
    /// that is the wider of the gaps the room boxes leave open on each side of the door, plus
    /// [`Self::tolerance`]. An eye walking through the door stands at most that far from the
    /// plane while in neither box (each box's face parallel to the portal and covering the
    /// opening). Derived from the file when it loads.
    pub doorway_depth: f32,
    /// The portal's tolerance, metres: [`PORTAL_PLANAR_TOLERANCE`] of the longest diagonal, at
    /// least [`PORTAL_PLANAR_MIN_M`] (how far a corner may sit off the plane). The doorway test
    /// also grows the quad's outline by it.
    pub tolerance: f32,
}

impl VisPortal {
    /// The cell index on the other side of this portal from `from`.
    #[must_use]
    pub fn other(&self, from: u32) -> u32 {
        if self.a == from { self.b } else { self.a }
    }

    /// Whether `point` stands in this portal's doorway: within [`Self::doorway_depth`] of the
    /// portal's plane, and over the quad (its outline grown by [`Self::tolerance`]).
    #[must_use]
    pub fn in_doorway(&self, point: Vec3) -> bool {
        let off = (point - self.center).dot(self.normal);
        if off.abs() > self.doorway_depth {
            return false;
        }
        let on_plane = point - self.normal * off;
        // The corners wind counter-clockwise about the (Newell) normal, so inside is to the left
        // of every edge.
        (0..4).all(|i| {
            let (p, q) = (self.quad[i], self.quad[(i + 1) % 4]);
            let edge = q - p;
            let length = edge.length();
            length <= 0.0 || edge.cross(on_plane - p).dot(self.normal) >= -self.tolerance * length
        })
    }
}

/// A validated `<map>.viscells.ron` (`docs/specs/25-sim-bridge-and-do27.md` § 7.7): the resource
/// the portal traversal reads. Insert it to turn cell culling on; remove it to turn it off.
///
/// # Example
///
/// ```
/// use regia_render::viscells::VisCells;
///
/// let cells = VisCells::from_ron(
///     "(cells: [(id: 1, bounds: [(center: (0.0, 0.0, 0.0), half_extents: (2.0, 2.0, 2.0))])],
///       portals: [(a: 0, b: 1, quad: [(-1.0, -1.0, 2.0), (1.0, -1.0, 2.0), (1.0, 1.0, 2.0), (-1.0, 1.0, 2.0)])])",
/// )
/// .expect("a valid file");
/// assert_eq!(cells.locate([0.0, 0.0, 0.0].into()), 1);
/// assert_eq!(cells.locate([0.0, 0.0, 9.0].into()), 0);
/// ```
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct VisCells {
    /// The document as loaded (for the round trip and the debug read).
    document: VisCellsDocument,
    /// Cells ascending by id; index 0 is always the outdoor cell.
    pub cells: Vec<VisCellInfo>,
    /// Portals in file order.
    pub portals: Vec<VisPortal>,
}

/// The format version of a `<map>.viscells.ron` text: its top-level `version`, 1 when absent.
/// Read before the body, so a newer file is recognized even when its body no longer parses.
///
/// # Errors
///
/// [`VisCellsError::Parse`] when the text is not a RON map or `version` is not a whole number.
pub fn viscells_file_version(text: &str) -> Result<u32, VisCellsError> {
    let value: ron::Value = ron::from_str(text)
        .map_err(|e| VisCellsError::Parse(format!("cannot read the format version: {e}")))?;
    let ron::Value::Map(map) = value else {
        return Err(VisCellsError::Parse("the top level is not a map".into()));
    };
    match map.get(&ron::Value::String("version".to_owned())) {
        None => Ok(1),
        Some(ron::Value::Number(number)) => ron::Value::Number(*number)
            .into_rust::<u32>()
            .map_err(|_| VisCellsError::Parse("`version` is not a whole number".into())),
        Some(_) => Err(VisCellsError::Parse("`version` is not a whole number".into())),
    }
}

/// The `<map>.viscells.ron` next to a map file, named like `<map>.holes.ron`
/// (`maps/cinder_steps.level.ron` → `maps/cinder_steps.viscells.ron`).
#[must_use]
pub fn viscells_ron_path(map: &std::path::Path) -> std::path::PathBuf {
    let name = map.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let mut stem = name.strip_suffix(".ron").unwrap_or(name);
    if let Some((base, kind)) = stem.rsplit_once('.')
        && !base.is_empty()
        && !kind.is_empty()
        && kind.chars().all(|c| c.is_ascii_alphabetic())
    {
        stem = base;
    }
    map.with_file_name(format!("{stem}.viscells.ron"))
}

/// Loads a `<map>.viscells.ron` ([`VisCells::from_ron`]). `Ok(None)` when the file does not exist
/// (the map has no cells: nothing is culled).
///
/// # Errors
///
/// The file exists but cannot be read ([`VisCellsError::Parse`]) or is refused.
pub fn load_viscells_ron(path: &std::path::Path) -> Result<Option<VisCells>, VisCellsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(VisCellsError::Parse(format!(
                "cannot read {}: {err}",
                path.display()
            )));
        }
    };
    VisCells::from_ron(&text).map(Some)
}

/// Versions [`VisCells::from_ron`] migrates from: none yet (version 1 is the first).
#[must_use]
pub fn viscells_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the `<map>.viscells.ron` format (hashed by
/// `cargo xtask check-formats`).
#[must_use]
pub fn viscells_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<VisCellsDocument>(),
        serde_field_names::<VisCellDef>(),
        serde_field_names::<CellBox>(),
        serde_field_names::<VisPortalDef>(),
    ])
}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// Checks a portal quad: finite, planar, convex. Returns its corners, unit normal, centroid and
/// tolerance.
fn check_quad(index: usize, quad: &[[f32; 3]]) -> Result<([Vec3; 4], Vec3, Vec3, f32), VisCellsError> {
    let Ok(quad) = <&[[f32; 3]; 4]>::try_from(quad) else {
        return Err(VisCellsError::NotQuadPortal(index));
    };
    if !quad.iter().all(|p| finite3(*p)) {
        return Err(VisCellsError::NonFinitePortal(index));
    }
    let p = quad.map(Vec3::from_array);
    // Newell's normal: robust for any simple polygon, zero for a degenerate one.
    let mut newell = Vec3::ZERO;
    for i in 0..4 {
        let (u, v) = (p[i], p[(i + 1) % 4]);
        newell += Vec3::new(
            (u.y - v.y) * (u.z + v.z),
            (u.z - v.z) * (u.x + v.x),
            (u.x - v.x) * (u.y + v.y),
        );
    }
    let diagonal = (p[2] - p[0]).length().max((p[3] - p[1]).length());
    // Newell's vector has twice the area as its length.
    let area = 0.5 * newell.length();
    if !area.is_finite() || diagonal <= 0.0 || area <= 1.0e-6 * diagonal * diagonal {
        return Err(VisCellsError::NonConvexPortal(index));
    }
    let normal = newell / newell.length();
    let center = (p[0] + p[1] + p[2] + p[3]) * 0.25;
    let tolerance = (PORTAL_PLANAR_TOLERANCE * diagonal).max(PORTAL_PLANAR_MIN_M);
    if p.iter().any(|q| (*q - center).dot(normal).abs() > tolerance) {
        return Err(VisCellsError::NonPlanarPortal(index));
    }
    // Convex and simple: every corner turns the same way about the normal, clearly.
    let eps = 1.0e-6 * diagonal * diagonal;
    for i in 0..4 {
        let (prev, here, next) = (p[(i + 3) % 4], p[i], p[(i + 1) % 4]);
        if (here - prev).cross(next - here).dot(normal) <= eps {
            return Err(VisCellsError::NonConvexPortal(index));
        }
    }
    Ok((p, normal, center, tolerance))
}

/// The distance from `point` to the nearest box of `cell` (0 for the outdoor cell, which has none).
fn cell_distance(cell: &VisCellInfo, point: Vec3) -> f32 {
    cell.bounds
        .iter()
        .map(|b| b.distance(point))
        .reduce(f32::min)
        .unwrap_or(0.0)
}

impl VisCells {
    /// Parses and validates a `<map>.viscells.ron` (version 1; absent `version` = 1).
    ///
    /// # Errors
    ///
    /// Any [`VisCellsError`]: a newer version is refused before the body is read.
    pub fn from_ron(text: &str) -> Result<Self, VisCellsError> {
        let version = viscells_file_version(text)?;
        if version > VISCELLS_FORMAT_VERSION {
            return Err(VisCellsError::NewerVersion(version));
        }
        if version == 0 {
            return Err(VisCellsError::ZeroVersion);
        }
        let document: VisCellsDocument =
            ron::from_str(text).map_err(|e| VisCellsError::Parse(e.to_string()))?;
        Self::from_document(document)
    }

    /// Validates a document built in code.
    ///
    /// # Errors
    ///
    /// Any [`VisCellsError`] but [`VisCellsError::Parse`].
    pub fn from_document(mut document: VisCellsDocument) -> Result<Self, VisCellsError> {
        if document.version > VISCELLS_FORMAT_VERSION {
            return Err(VisCellsError::NewerVersion(document.version));
        }
        if document.version == 0 {
            return Err(VisCellsError::ZeroVersion);
        }
        document.version = VISCELLS_FORMAT_VERSION;
        if document.cells.len() > MAX_VIS_CELLS {
            return Err(VisCellsError::TooMany("cells"));
        }
        if document.portals.len() > MAX_VIS_PORTALS {
            return Err(VisCellsError::TooMany("portals"));
        }
        let mut cells: Vec<VisCellInfo> = Vec::with_capacity(document.cells.len() + 1);
        let mut outdoor_name = String::from("outdoor");
        let mut ids: Vec<u16> = Vec::with_capacity(document.cells.len());
        for def in &document.cells {
            if ids.contains(&def.id) {
                return Err(VisCellsError::DuplicateCell(def.id));
            }
            ids.push(def.id);
            if def.bounds.len() > MAX_CELL_BOXES {
                return Err(VisCellsError::TooMany("boxes in a cell"));
            }
            if def.id == OUTDOOR_CELL {
                if !def.bounds.is_empty() {
                    return Err(VisCellsError::OutdoorHasBounds);
                }
                if !def.name.is_empty() {
                    outdoor_name.clone_from(&def.name);
                }
                continue;
            }
            if def.outdoor {
                return Err(VisCellsError::OutdoorNotZero(def.id));
            }
            if def.bounds.is_empty() {
                return Err(VisCellsError::CellWithoutBounds(def.id));
            }
            for b in &def.bounds {
                let ok = finite3(b.center)
                    && b.half_extents.iter().all(|h| h.is_finite() && *h > 0.0)
                    && b.rotation_radians.is_finite();
                if !ok {
                    return Err(VisCellsError::BadBox(def.id));
                }
            }
            cells.push(VisCellInfo {
                id: def.id,
                name: def.name.clone(),
                bounds: def.bounds.clone(),
                portals: Vec::new(),
            });
        }
        cells.push(VisCellInfo {
            id: OUTDOOR_CELL,
            name: outdoor_name,
            bounds: Vec::new(),
            portals: Vec::new(),
        });
        cells.sort_by_key(|c| c.id);
        let index_of = |cells: &[VisCellInfo], id: u16| cells.binary_search_by_key(&id, |c| c.id).ok();
        let mut portals = Vec::with_capacity(document.portals.len());
        for (i, def) in document.portals.iter().enumerate() {
            let a = index_of(&cells, def.a).ok_or(VisCellsError::UnknownCell {
                portal: i,
                cell: def.a,
            })?;
            let b = index_of(&cells, def.b).ok_or(VisCellsError::UnknownCell {
                portal: i,
                cell: def.b,
            })?;
            if a == b {
                return Err(VisCellsError::SelfPortal(i));
            }
            let (quad, normal, center, tolerance) = check_quad(i, &def.quad)?;
            // The doorway's depth: the gap the two cells' boxes leave around the portal
            // (ADR-7639), the larger side's (ADR-7646). An eye walking through stands at most one
            // side's gap from the plane (a box whose face is parallel to the portal and covers
            // the opening holds it beyond); the sum would let the doorway reach twice a
            // symmetric wall's gap past each face, into ground behind the door in no box.
            let gap = cell_distance(&cells[a], center).max(cell_distance(&cells[b], center));
            portals.push(VisPortal {
                a: a as u32,
                b: b as u32,
                quad,
                normal,
                center,
                doorway_depth: gap + tolerance,
                tolerance,
            });
            cells[a].portals.push(i as u32);
            cells[b].portals.push(i as u32);
        }
        Ok(Self {
            document,
            cells,
            portals,
        })
    }

    /// The document re-encoded at [`VISCELLS_FORMAT_VERSION`] (pretty RON).
    ///
    /// # Errors
    ///
    /// [`VisCellsError::Parse`] when RON cannot encode a value (never for a validated set).
    pub fn to_ron(&self) -> Result<String, VisCellsError> {
        ron::ser::to_string_pretty(&self.document, regia_core::ron_pretty())
            .map_err(|e| VisCellsError::Parse(e.to_string()))
    }

    /// The document as loaded (version set to [`VISCELLS_FORMAT_VERSION`]).
    #[must_use]
    pub fn document(&self) -> &VisCellsDocument {
        &self.document
    }

    /// The index in [`Self::cells`] of cell `id`, if listed (cell 0 always is, at index 0).
    #[must_use]
    pub fn index_of(&self, id: u16) -> Option<usize> {
        self.cells.binary_search_by_key(&id, |c| c.id).ok()
    }

    /// The index in [`Self::cells`] of the cell holding `point`: the cell with a box containing
    /// it, the smallest such box on overlap (the lower id on a tie); 0 (outdoors) in none.
    #[must_use]
    pub fn locate_index(&self, point: Vec3) -> usize {
        let mut best: Option<(f32, usize)> = None;
        for (index, cell) in self.cells.iter().enumerate() {
            for b in &cell.bounds {
                if b.contains(point) {
                    let volume = b.volume();
                    if best.is_none_or(|(v, _)| volume < v) {
                        best = Some((volume, index));
                    }
                }
            }
        }
        best.map_or(0, |(_, index)| index)
    }

    /// The id of the cell holding `point` ([`Self::locate_index`]).
    #[must_use]
    pub fn locate(&self, point: Vec3) -> u16 {
        self.cells[self.locate_index(point)].id
    }

    /// The cells a view whose eye is at `point` starts its walk in, as indices into
    /// [`Self::cells`], ascending, written to `out` (cleared first; allocation-free once `out`
    /// has grown). Returns true when the doorway rule chose them (ADR-7639).
    ///
    /// - In a cell box: that one cell ([`Self::locate_index`]).
    /// - In no box but in the doorway of one or more portals ([`VisPortal::in_doorway`]: the wall
    ///   thickness that room boxes stopping at the wall faces leave open): both cells of every
    ///   such portal. The eye is in all of them; the walk reaches the union of what each sees.
    /// - Otherwise: the outdoor cell (index 0).
    pub fn locate_view_cells(&self, point: Vec3, out: &mut Vec<u32>) -> bool {
        out.clear();
        let located = self.locate_index(point);
        if located != 0 {
            out.push(located as u32);
            return false;
        }
        for portal in &self.portals {
            if portal.in_doorway(point) {
                for cell in [portal.a, portal.b] {
                    if !out.contains(&cell) {
                        out.push(cell);
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(0);
            return false;
        }
        out.sort_unstable();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM: &str = "(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))";

    fn file(cells: &str, portals: &str) -> String {
        format!("(version: 1, cells: [{cells}], portals: [{portals}])")
    }

    fn door(a: u16, b: u16) -> String {
        format!(
            "(a: {a}, b: {b}, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)])"
        )
    }

    fn code(text: &str) -> &'static str {
        match VisCells::from_ron(text) {
            Ok(_) => "ok",
            Err(e) => e.code(),
        }
    }

    #[test]
    fn a_two_room_file_loads_with_the_outdoor_cell_first() {
        let text = file(
            &format!(
                "(id: 1, name: \"a\", bounds: [{ROOM}]), (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))])"
            ),
            &door(1, 2),
        );
        let cells = VisCells::from_ron(&text).expect("loads");
        assert_eq!(cells.cells.len(), 3);
        assert_eq!(cells.cells[0].id, 0);
        assert_eq!(cells.cells[0].name, "outdoor");
        assert_eq!(cells.portals.len(), 1);
        assert_eq!(cells.cells[1].portals, vec![0]);
        assert_eq!(cells.cells[2].portals, vec![0]);
        assert_eq!(cells.locate(Vec3::new(0.0, 1.0, 0.0)), 1);
        assert_eq!(cells.locate(Vec3::new(0.0, 1.0, -8.0)), 2);
        assert_eq!(cells.locate(Vec3::new(50.0, 1.0, 0.0)), 0);
        // Round trip: re-encoded text loads to the same set.
        let again = VisCells::from_ron(&cells.to_ron().expect("encode")).expect("reload");
        assert_eq!(again, cells);
    }

    #[test]
    fn version_rules() {
        assert_eq!(code("(cells: [])"), "ok", "absent version is 1");
        assert_eq!(code("(version: 2, cells: [])"), "viscells_version_newer");
        assert_eq!(code("(version: 0, cells: [])"), "viscells_version_zero");
        assert_eq!(code("[1, 2]"), "viscells_parse");
        assert_eq!(code("(version: \"one\")"), "viscells_parse");
        // A newer file is refused before its body is read.
        assert_eq!(
            code("(version: 9, cells: [(future_field: 1)])"),
            "viscells_version_newer"
        );
        assert_eq!(viscells_file_version("(cells: [])").expect("v"), 1);
    }

    #[test]
    fn cell_rules_have_codes() {
        let r = format!("bounds: [{ROOM}]");
        assert_eq!(
            code(&file(&format!("(id: 1, {r}), (id: 1, {r})"), "")),
            "viscells_duplicate_cell"
        );
        assert_eq!(code(&file("(id: 3)", "")), "viscells_cell_no_bounds");
        assert_eq!(
            code(&file(&format!("(id: 0, {r})"), "")),
            "viscells_outdoor_bounds"
        );
        assert_eq!(
            code(&file(&format!("(id: 4, outdoor: true, {r})"), "")),
            "viscells_outdoor_not_zero"
        );
        assert_eq!(
            code(&file("(id: 0, name: \"sky\", outdoor: true, bounds: [])", "")),
            "ok"
        );
        assert_eq!(
            code(&file(
                "(id: 5, bounds: [(center: (0.0, 0.0, 0.0), half_extents: (1.0, 0.0, 1.0))])",
                ""
            )),
            "viscells_bad_box"
        );
        assert_eq!(code(&file("(id: 5, bounds: [], extra: 1)", "")), "viscells_parse");
    }

    #[test]
    fn portal_rules_have_codes() {
        let one = format!("(id: 1, bounds: [{ROOM}])");
        assert_eq!(code(&file(&one, &door(0, 1))), "ok", "a portal to outdoors");
        assert_eq!(code(&file(&one, &door(1, 7))), "viscells_unknown_cell");
        assert_eq!(code(&file(&one, &door(1, 1))), "viscells_self_portal");
        let quad = |q: &str| file(&one, &format!("(a: 0, b: 1, quad: [{q}])"));
        // The fourth corner 0.5 m off a 2 m doorway's plane.
        assert_eq!(
            code(&quad(
                "(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -3.5)"
            )),
            "viscells_portal_not_planar"
        );
        // A bow tie (corners 2 and 3 swapped).
        assert_eq!(
            code(&quad(
                "(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (-1.0, 2.0, -4.0), (1.0, 2.0, -4.0)"
            )),
            "viscells_portal_not_convex"
        );
        // A dent: corner 2 pushed inside the triangle of the others.
        assert_eq!(
            code(&quad(
                "(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (0.0, 0.5, -4.0), (-1.0, 2.0, -4.0)"
            )),
            "viscells_portal_not_convex"
        );
        // Collinear corners: no area.
        assert_eq!(
            code(&quad(
                "(0.0, 0.0, -4.0), (1.0, 0.0, -4.0), (2.0, 0.0, -4.0), (3.0, 0.0, -4.0)"
            )),
            "viscells_portal_not_convex"
        );
        // A repeated corner (a triangle in disguise).
        assert_eq!(
            code(&quad(
                "(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (-1.0, 2.0, -4.0)"
            )),
            "viscells_portal_not_convex"
        );
        // Three or five corners are not a quad.
        assert_eq!(
            code(&quad("(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0)")),
            "viscells_portal_not_quad"
        );
        assert_eq!(
            code(&quad(
                "(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (0.0, 3.0, -4.0), (-1.0, 2.0, -4.0)"
            )),
            "viscells_portal_not_quad"
        );
        // Either winding is fine.
        assert_eq!(
            code(&quad(
                "(-1.0, 2.0, -4.0), (1.0, 2.0, -4.0), (1.0, 0.0, -4.0), (-1.0, 0.0, -4.0)"
            )),
            "ok"
        );
    }

    #[test]
    fn non_finite_corners_are_refused_in_code() {
        let mut doc = VisCellsDocument::default();
        doc.cells.push(VisCellDef {
            id: 1,
            name: String::new(),
            outdoor: false,
            bounds: vec![CellBox {
                center: [0.0; 3],
                half_extents: [1.0; 3],
                rotation_radians: 0.0,
            }],
        });
        doc.portals.push(VisPortalDef {
            a: 0,
            b: 1,
            quad: vec![
                [f32::NAN, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
        });
        assert_eq!(
            VisCells::from_document(doc).map(|_| ()).map_err(|e| e.code()),
            Err("viscells_portal_not_finite")
        );
    }

    #[test]
    fn a_rotated_box_contains_by_its_own_frame_and_the_smallest_box_wins() {
        let b = CellBox {
            center: [10.0, 0.0, 0.0],
            half_extents: [4.0, 1.0, 0.5],
            rotation_radians: std::f32::consts::FRAC_PI_2,
        };
        // Turned 90 degrees, the long axis runs along world Z.
        assert!(b.contains(Vec3::new(10.0, 0.0, 3.5)));
        assert!(!b.contains(Vec3::new(13.5, 0.0, 0.0)));
        let text = file(
            "(id: 1, bounds: [(center: (0.0, 0.0, 0.0), half_extents: (10.0, 10.0, 10.0))]),
             (id: 2, bounds: [(center: (0.0, 0.0, 0.0), half_extents: (1.0, 1.0, 1.0))])",
            "",
        );
        let cells = VisCells::from_ron(&text).expect("loads");
        assert_eq!(cells.locate(Vec3::ZERO), 2, "the smaller box wins");
        assert_eq!(cells.locate(Vec3::new(5.0, 0.0, 0.0)), 1);
    }

    #[test]
    fn the_doorway_depth_is_the_wall_gap_the_boxes_leave_plus_the_tolerance() {
        // Boxes stop at the faces of a 0.3 m wall (z −3.85 and −4.15), the portal in its middle:
        // 0.15 m to each side (ADR-7646: the larger side's gap, not the 0.3 m sum of both).
        let wall = file(
            "(id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 3.85))]),
             (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 3.85))])",
            &door(1, 2),
        );
        let cells = VisCells::from_ron(&wall).expect("loads");
        let portal = cells.portals[0];
        // A 2 × 2 m quad: tolerance 1 % of its 2.83 m diagonal.
        let tolerance = PORTAL_PLANAR_TOLERANCE * 8.0_f32.sqrt();
        assert!((portal.tolerance - tolerance).abs() < 1.0e-5, "{portal:?}");
        assert!(
            (portal.doorway_depth - (0.15 + tolerance)).abs() < 1.0e-4,
            "{portal:?}"
        );
        let mut out = Vec::new();
        // In the gap over the opening: both rooms (indices 1 and 2).
        for z in [-3.86_f32, -4.0, -4.14] {
            let eye = Vec3::new(0.5, 1.0, z);
            assert!(portal.in_doorway(eye), "z {z}");
            assert!(cells.locate_view_cells(eye, &mut out), "z {z}");
            assert_eq!(out, vec![1, 2], "z {z}");
        }
        // In a box: that cell only, the doorway rule not used.
        assert!(!cells.locate_view_cells(Vec3::new(0.0, 1.0, -3.0), &mut out));
        assert_eq!(out, vec![1]);
        // Beside the opening (in the wall), above it, far away: outdoors.
        for eye in [
            Vec3::new(1.2, 1.0, -4.0),
            Vec3::new(0.0, 2.2, -4.0),
            Vec3::new(0.0, 3.5, -4.0),
            Vec3::new(50.0, 1.0, 0.0),
        ] {
            assert!(!cells.locate_view_cells(eye, &mut out), "{eye}");
            assert_eq!(out, vec![0], "{eye}");
        }
        // Further from the plane than the doorway's depth is not in the doorway.
        assert!(!portal.in_doorway(Vec3::new(0.0, 1.0, -4.0 + 0.3 + 2.0 * tolerance)));
        assert!(!portal.in_doorway(Vec3::new(0.0, 1.0, -4.0 + 0.15 + 2.0 * tolerance)));
        // Within the tolerance of the opening's edge still counts.
        assert!(portal.in_doorway(Vec3::new(1.0 + 0.5 * tolerance, 1.0, -4.0)));

        // Boxes that meet at the portal leave no gap: the doorway is only the tolerance deep.
        let flush = file(
            &format!(
                "(id: 1, bounds: [{ROOM}]), (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))])"
            ),
            &door(1, 2),
        );
        let flush = VisCells::from_ron(&flush).expect("loads");
        assert!((flush.portals[0].doorway_depth - tolerance).abs() < 1.0e-5);

        // A door to outdoors whose room box stops 0.4 m short of it: the outdoor side adds
        // nothing, the doorway holds the room and outdoors.
        let outside = file(
            "(id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 3.6))])",
            &door(1, 0),
        );
        let outside = VisCells::from_ron(&outside).expect("loads");
        assert!((outside.portals[0].doorway_depth - (0.4 + tolerance)).abs() < 1.0e-4);
        assert!(outside.locate_view_cells(Vec3::new(0.0, 1.0, -3.8), &mut out));
        assert_eq!(out, vec![0, 1]);
    }

    /// ADR-7646 (review finding 2.1): the doorway reaches the larger of the two sides' gaps from
    /// the plane (plus the tolerance), not their sum, so an eye in no box behind the door, past
    /// the farther box's gap, is outdoors; a walk through the door, either gap the larger, is in
    /// a box or the doorway at every step.
    #[test]
    fn the_doorway_reaches_the_larger_gap_not_the_sum_of_both() {
        let tolerance = PORTAL_PLANAR_TOLERANCE * 8.0_f32.sqrt();
        // Room 1 stops at the wall's face (0.15 m from the plane). Room 2 is an L whose box
        // starts beside the door, 1.15 m to the side and 0.15 m behind the wall: the space
        // right behind the door (a porch) is in no box. The portal's centre (0, 1, −4) lies
        // √(1.15² + 0.15²) ≈ 1.16 m from room 2's box.
        let porch = file(
            "(id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 3.85))]),
             (id: 2, bounds: [(center: (5.15, 1.5, -8.15), half_extents: (4.0, 1.5, 4.0))])",
            &door(1, 2),
        );
        let cells = VisCells::from_ron(&porch).expect("loads");
        let portal = cells.portals[0];
        let far_gap = (1.15_f32 * 1.15 + 0.15 * 0.15).sqrt();
        assert!(
            (portal.doorway_depth - (far_gap + tolerance)).abs() < 1.0e-4,
            "{portal:?}"
        );
        let mut out = Vec::new();
        // On the porch behind the door, past the larger gap (within the sum of both): outdoors.
        for z in [
            -4.0 - far_gap - 2.0 * tolerance,
            -4.0 - far_gap - 0.1,
            -4.0 - (far_gap + 0.15) + 0.01,
        ] {
            let eye = Vec3::new(0.5, 1.0, z);
            assert!(!portal.in_doorway(eye), "z {z}");
            assert!(!cells.locate_view_cells(eye, &mut out), "z {z}");
            assert_eq!(out, vec![0], "z {z}");
        }
        // Within the larger gap: still the doorway (both rooms).
        assert!(cells.locate_view_cells(Vec3::new(0.5, 1.0, -4.0 - far_gap + 0.01), &mut out));
        assert_eq!(out, vec![1, 2]);

        // Walking through a door whose wall gaps differ (0.05 m on room 1's side, 0.25 m on room
        // 2's): every step is in a box or in the doorway, never outdoors.
        let uneven = file(
            "(id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 3.95))]),
             (id: 2, bounds: [(center: (0.0, 1.5, -8.25), half_extents: (4.0, 1.5, 4.0))])",
            &door(1, 2),
        );
        let uneven = VisCells::from_ron(&uneven).expect("loads");
        assert!(
            (uneven.portals[0].doorway_depth - (0.25 + tolerance)).abs() < 1.0e-4,
            "{:?}",
            uneven.portals[0]
        );
        for step in 0..=400 {
            let z = -3.5 - step as f32 * 0.0025;
            for x in [-0.99_f32, 0.0, 0.99] {
                let eye = Vec3::new(x, 1.0, z);
                uneven.locate_view_cells(eye, &mut out);
                assert_ne!(out, vec![0], "({x}, {z}) is never outdoors");
            }
        }
    }

    #[test]
    fn a_box_measures_its_distance_in_its_own_frame() {
        let b = CellBox {
            center: [10.0, 0.0, 0.0],
            half_extents: [4.0, 1.0, 0.5],
            rotation_radians: std::f32::consts::FRAC_PI_2,
        };
        assert_eq!(b.distance(Vec3::new(10.0, 0.0, 3.0)), 0.0, "inside");
        assert!((b.distance(Vec3::new(10.0, 0.0, 5.0)) - 1.0).abs() < 1.0e-5);
        assert!((b.distance(Vec3::new(11.5, 0.0, 0.0)) - 1.0).abs() < 1.0e-5);
        assert!((b.distance(Vec3::new(11.5, 4.0, 0.0)) - 10.0_f32.sqrt()).abs() < 1.0e-5);
    }

    #[test]
    fn the_file_sits_next_to_the_map() {
        use std::path::Path;
        assert_eq!(
            viscells_ron_path(Path::new("maps/cinder_steps.level.ron")),
            Path::new("maps/cinder_steps.viscells.ron")
        );
        assert_eq!(
            viscells_ron_path(Path::new("maps/v1.2.level.ron")),
            Path::new("maps/v1.2.viscells.ron")
        );
    }
}
