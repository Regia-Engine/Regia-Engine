// SPDX-License-Identifier: MPL-2.0
//! The `.navlinks.ron` file (`docs/specs/92-navmesh-offmesh.md` § 3.5, ADR-1821): a list of
//! authored off-mesh links, `format_version` 1. See `docs/formats/navlinks-ron.md`.
//!
//! Only `from` and `to` are required per link; every other field takes the [`OffMeshLink::new`]
//! default. Migration note: v1 is the first version and has no migrators.
//!
//! Runtime loading and scene links (ADR-2420): [`NavLinkSet::load`] and [`NavLaneSet::load`] read
//! `.navlinks.ron` and `.navlanes.ron` files at run time; [`NavLinkAnchor`] is an off-mesh link
//! authored as a scene component; [`NavAuthoredLinksPlugin`] keeps the navmesh's links equal to
//! the files' links plus the anchors', and the lane graph equal to the lane files'.

use std::path::Path;

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_scene::spawn::ReflectFormatVersion;
use regia_scene::{GlobalTransform, Transform};
use serde::{Deserialize, Serialize};

use crate::crowd::NavCrowdStep;
use crate::lanes::{NAV_LANES_FORMAT_VERSION, NavLaneCrowd, NavLaneError, NavLaneSet};
use crate::links::{NavLinkError, NavLinkFlags, NavLinkKind, validate_links};
use crate::mesh::{DEFAULT_LINK_RADIUS, OffMeshLink};
use crate::region_queue::NavRegionStep;

/// The current `.navlinks.ron` version.
pub const NAV_LINKS_FORMAT_VERSION: u32 = 1;

/// The file root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavLinkSet {
    /// The format version (1).
    pub format_version: u32,
    /// The links, in order.
    pub links: Vec<NavLinkDef>,
}

/// One link as written in the file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NavLinkDef {
    /// Start point `(x, y, z)`.
    pub from: (f32, f32, f32),
    /// End point `(x, y, z)`.
    pub to: (f32, f32, f32),
    /// Usable both ways (default true).
    #[serde(default = "yes")]
    pub bidirectional: bool,
    /// Area / cost category (default 0).
    #[serde(default)]
    pub area: u8,
    /// The game's id (default 0 = unnamed).
    #[serde(default)]
    pub user_id: u64,
    /// The move kind (default `Jump`).
    #[serde(default)]
    pub kind: NavLinkKind,
    /// Endpoint snap radius, metres (default 2).
    #[serde(default = "default_radius")]
    pub radius: f32,
    /// Cost multiplier (default 1).
    #[serde(default = "one")]
    pub cost_scale: f32,
    /// Filter bits (default 1).
    #[serde(default = "default_flags")]
    pub flags: u16,
    /// The runtime switch (default true).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Traversal seconds (default 0 = automatic).
    #[serde(default)]
    pub duration: f32,
}

fn yes() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

fn default_radius() -> f32 {
    DEFAULT_LINK_RADIUS
}

fn default_flags() -> u16 {
    NavLinkFlags::WALK
}

fn v3(t: (f32, f32, f32)) -> Vec3 {
    Vec3::new(t.0, t.1, t.2)
}

impl From<&NavLinkDef> for OffMeshLink {
    fn from(d: &NavLinkDef) -> Self {
        Self {
            from: v3(d.from),
            to: v3(d.to),
            bidirectional: d.bidirectional,
            area: d.area,
            user_id: d.user_id,
            kind: d.kind,
            radius: d.radius,
            cost_scale: d.cost_scale,
            flags: d.flags,
            enabled: d.enabled,
            duration: d.duration,
        }
    }
}

impl From<&OffMeshLink> for NavLinkDef {
    fn from(l: &OffMeshLink) -> Self {
        Self {
            from: (l.from.x, l.from.y, l.from.z),
            to: (l.to.x, l.to.y, l.to.z),
            bidirectional: l.bidirectional,
            area: l.area,
            user_id: l.user_id,
            kind: l.kind,
            radius: l.radius,
            cost_scale: l.cost_scale,
            flags: l.flags,
            enabled: l.enabled,
            duration: l.duration,
        }
    }
}

impl NavLinkSet {
    /// Parses and checks a `.navlinks.ron` text. Errors: malformed RON (`nav_link_parse`), a
    /// `format_version` of 0 or above 1 (`nav_link_version`), too many links or an invalid value
    /// (`nav_invalid_link`).
    pub fn from_ron_str(text: &str) -> Result<Vec<OffMeshLink>, NavLinkError> {
        let set: NavLinkSet = ron::from_str(text).map_err(|e| NavLinkError::Parse(e.to_string()))?;
        if set.format_version > NAV_LINKS_FORMAT_VERSION || set.format_version == 0 {
            return Err(NavLinkError::Version {
                found: set.format_version,
                supported: NAV_LINKS_FORMAT_VERSION,
            });
        }
        let links: Vec<OffMeshLink> = set.links.iter().map(OffMeshLink::from).collect();
        validate_links(&links)?;
        Ok(links)
    }

    /// Writes `links` as a `.navlinks.ron` text at the current version (every field written).
    pub fn to_ron_string(links: &[OffMeshLink]) -> Result<String, NavLinkError> {
        validate_links(links)?;
        let set = NavLinkSet {
            format_version: NAV_LINKS_FORMAT_VERSION,
            links: links.iter().map(NavLinkDef::from).collect(),
        };
        ron::ser::to_string_pretty(&set, regia_core::ron_pretty())
            .map_err(|e| NavLinkError::Parse(e.to_string()))
    }
}

/// The `from` versions of `.navlinks.ron`'s migration steps (none: v1 is the first).
pub fn nav_links_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.navlinks.ron` (hashed by `cargo xtask check-formats`).
pub fn nav_links_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<NavLinkSet>(),
        serde_field_names::<NavLinkDef>(),
        serde_variant_names::<NavLinkKind>(),
    ])
}

/// The largest `.navlinks.ron` or `.navlanes.ron` file the runtime loaders read (16 MiB).
pub const MAX_NAV_FILE_BYTES: usize = 16 * 1024 * 1024;

/// A file's bytes as text, size and encoding checked.
fn file_text(bytes: &[u8]) -> Result<&str, String> {
    if bytes.len() > MAX_NAV_FILE_BYTES {
        return Err(format!(
            "{} bytes, the limit is {MAX_NAV_FILE_BYTES}",
            bytes.len()
        ));
    }
    std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))
}

/// Reads a whole file, refusing one past [`MAX_NAV_FILE_BYTES`] before reading it.
fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > MAX_NAV_FILE_BYTES as u64 {
        return Err(format!(
            "{}: {len} bytes, the limit is {MAX_NAV_FILE_BYTES}",
            path.display()
        ));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

impl NavLinkSet {
    /// [`Self::from_ron_str`] over a file's bytes (ADR-2420). Errors: more than
    /// [`MAX_NAV_FILE_BYTES`] or not UTF-8 (`nav_link_io`), then those of [`Self::from_ron_str`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Vec<OffMeshLink>, NavLinkError> {
        Self::from_ron_str(file_text(bytes).map_err(NavLinkError::Io)?)
    }

    /// Reads a `.navlinks.ron` file at run time. Errors: a missing or unreadable file
    /// (`nav_link_io`), then those of [`Self::from_bytes`].
    pub fn load(path: &Path) -> Result<Vec<OffMeshLink>, NavLinkError> {
        let _span = tracing::debug_span!("nav_links_load").entered();
        Self::from_bytes(&read_file(path).map_err(NavLinkError::Io)?)
    }
}

impl NavLaneSet {
    /// [`Self::parse`] over a file's bytes (ADR-2420). Errors: more than [`MAX_NAV_FILE_BYTES`]
    /// or not UTF-8 (`nav_lane_io`), then those of [`Self::parse`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, NavLaneError> {
        Self::parse(file_text(bytes).map_err(NavLaneError::Io)?)
    }

    /// Reads a `.navlanes.ron` file at run time. Errors: a missing or unreadable file
    /// (`nav_lane_io`), then those of [`Self::from_bytes`].
    pub fn load(path: &Path) -> Result<Self, NavLaneError> {
        let _span = tracing::debug_span!("nav_lanes_load").entered();
        Self::from_bytes(&read_file(path).map_err(NavLaneError::Io)?)
    }

    /// Appends `other`'s lanes after these, shifting its `next` and `opposite` indices; the
    /// larger `join_distance` wins. Errors: more than `MAX_NAV_LANES` lanes together.
    pub fn append(&mut self, other: NavLaneSet) -> Result<(), NavLaneError> {
        let base = self.lanes.len();
        let total = base + other.lanes.len();
        if total > crate::lanes::MAX_NAV_LANES {
            return Err(NavLaneError::TooManyLanes(total));
        }
        let shift = base as u32;
        for mut lane in other.lanes {
            lane.next.iter_mut().for_each(|n| *n += shift);
            lane.opposite = lane.opposite.map(|o| o + shift);
            self.lanes.push(lane);
        }
        self.join_distance = self.join_distance.max(other.join_distance);
        self.format_version = NAV_LANES_FORMAT_VERSION;
        Ok(())
    }
}

/// `format_version` of the saved [`NavLinkAnchor`] component (`nav-link-anchor`).
pub const NAV_LINK_ANCHOR_FORMAT_VERSION: u32 = 1;

/// An off-mesh link authored as a scene component (ADR-2420): it takes off at the entity and
/// lands at `to`, a point in the entity's own space, so moving or turning the entity moves the
/// link. The other fields are [`OffMeshLink`]'s. [`NavAuthoredLinksPlugin`] puts every anchor's
/// link on the navmesh; an invalid anchor (a non-finite point, a radius or cost scale not above
/// 0, a bad duration) stays off it with a warning. Saved in scenes (`nav-link-anchor` v1).
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default, FormatVersion)]
#[serde(default)]
pub struct NavLinkAnchor {
    /// The landing in the entity's local space (default 2 m ahead along -Z).
    pub to: Vec3,
    /// Usable both ways (default true).
    pub bidirectional: bool,
    /// Area / cost category (default 0).
    pub area: u8,
    /// The game's id (default 0 = unnamed).
    pub user_id: u64,
    /// The move kind (default `Jump`).
    pub kind: NavLinkKind,
    /// Endpoint snap radius, metres (default 2).
    pub radius: f32,
    /// Cost multiplier (default 1).
    pub cost_scale: f32,
    /// Filter bits (default 1).
    pub flags: u16,
    /// The runtime switch (default true).
    pub enabled: bool,
    /// Traversal seconds (default 0 = automatic).
    pub duration: f32,
    /// Always [`NAV_LINK_ANCHOR_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for NavLinkAnchor {
    const CURRENT: u32 = NAV_LINK_ANCHOR_FORMAT_VERSION;
}

impl Default for NavLinkAnchor {
    fn default() -> Self {
        Self {
            to: Vec3::new(0.0, 0.0, -2.0),
            bidirectional: true,
            area: 0,
            user_id: 0,
            kind: NavLinkKind::Jump,
            radius: DEFAULT_LINK_RADIUS,
            cost_scale: 1.0,
            flags: NavLinkFlags::WALK,
            enabled: true,
            duration: 0.0,
            format_version: NAV_LINK_ANCHOR_FORMAT_VERSION,
        }
    }
}

impl NavLinkAnchor {
    /// An anchor landing at `to` (local space) with the default settings.
    pub fn new(to: Vec3) -> Self {
        Self {
            to,
            ..Self::default()
        }
    }

    /// The link this anchor makes with the entity at `pose`.
    pub fn link(&self, pose: &Affine3A) -> OffMeshLink {
        OffMeshLink {
            from: pose.transform_point3(Vec3::ZERO),
            to: pose.transform_point3(self.to),
            bidirectional: self.bidirectional,
            area: self.area,
            user_id: self.user_id,
            kind: self.kind,
            radius: self.radius,
            cost_scale: self.cost_scale,
            flags: self.flags,
            enabled: self.enabled,
            duration: self.duration,
        }
    }

    /// Upgrades an older saved shape (none yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > NAV_LINK_ANCHOR_FORMAT_VERSION {
            return Err(format!(
                "nav-link-anchor format_version {} is newer than {NAV_LINK_ANCHOR_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = NAV_LINK_ANCHOR_FORMAT_VERSION;
        Ok(())
    }
}

/// The shape fingerprint of the saved `nav-link-anchor` component (`cargo xtask check-formats`).
pub fn nav_link_anchor_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<NavLinkAnchor>(),
        serde_variant_names::<NavLinkKind>(),
    ])
}

/// The `from` versions of `nav-link-anchor`'s migration steps (none: v1 is the first).
pub fn nav_link_anchor_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The link and lane files [`NavAuthoredLinksPlugin`] loads (ADR-2420): paths as the platform's
/// file system takes them (a game resolves its project root first). Changing the resource
/// reloads every file. A level stores it in its `resources` map.
#[derive(Resource, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct NavLinkFiles {
    /// `.navlinks.ron` files, read in order; their links come first, in file order.
    pub links: Vec<String>,
    /// `.navlanes.ron` files, appended in order into one lane graph.
    pub lanes: Vec<String>,
}

/// A file [`NavAuthoredLinksPlugin`] could not use.
#[derive(Debug, Clone, PartialEq)]
pub struct NavFileError {
    /// The path as given.
    pub path: String,
    /// The protocol reason (`nav_link_io`, `nav_link_parse`, `nav_lane_io`, ...).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

/// What [`NavAuthoredLinksPlugin`] did last (ADR-2420).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct NavAuthoredLinksStatus {
    /// Links read from the link files.
    pub file_links: usize,
    /// Lanes read from the lane files.
    pub file_lanes: usize,
    /// Anchors on the navmesh.
    pub anchor_links: usize,
    /// Anchors left off as invalid.
    pub invalid_anchors: usize,
    /// Times the navmesh's link list was replaced.
    pub applied: u64,
    /// Files that failed on the last load, in list order.
    pub errors: Vec<NavFileError>,
}

/// The links the files gave on the last load.
#[derive(Resource, Debug, Default)]
struct NavFileLinks(Vec<OffMeshLink>);

/// Loads `.navlinks.ron` and `.navlanes.ron` files at run time and puts scene link anchors on the
/// navmesh (ADR-2420). Opt-in: an app without it steps exactly as before.
///
/// - [`NavLinkFiles`] changed (or was inserted): every file is read again in `PreUpdate`. The lane
///   files become the [`crate::NavLaneGraph`] resource (a lane crowd is dropped, as lane indices do
///   not carry over); a file that fails is skipped and reported in [`NavAuthoredLinksStatus`].
/// - Each fixed step, before `NavRegionStep` and `NavCrowdStep`, the links of the files then the
///   anchors (by user id, then entity) replace the off-mesh links of the [`NavTileCache`](crate::tile_cache::NavTileCache) (or,
///   without one, the [`NavMesh`](crate::mesh::NavMesh) resource, whose crowd then replans), when they differ from what
///   was applied last. Until the first file link or anchor appears the plugin changes nothing;
///   from then on it owns that list: links set another way are replaced on the next change.
///
/// A root entity's pose is its `Transform`, a child's its `GlobalTransform`, as for area zones.
pub struct NavAuthoredLinksPlugin;

impl Plugin for NavAuthoredLinksPlugin {
    fn build(&self, app: &mut App) {
        crate::origin::ensure(app);
        app.register_type::<NavLinkAnchor>()
            .register_type::<NavLinkKind>()
            .register_type::<NavLinkFiles>()
            .init_resource::<NavAuthoredLinksStatus>()
            .init_resource::<NavFileLinks>()
            .add_systems(PreUpdate, load_nav_files)
            .add_systems(
                FixedUpdate,
                sync_authored_links.before(NavRegionStep).before(NavCrowdStep),
            );
    }
}

/// Loads every file of [`NavLinkFiles`] when it changed.
fn load_nav_files(
    files: Option<Res<NavLinkFiles>>,
    mut file_links: ResMut<NavFileLinks>,
    mut status: ResMut<NavAuthoredLinksStatus>,
    mut commands: Commands,
) {
    let Some(files) = files else {
        return;
    };
    if !files.is_changed() {
        return;
    }
    let _span = tracing::info_span!(
        "nav_files_load",
        links = files.links.len(),
        lanes = files.lanes.len()
    )
    .entered();
    status.errors.clear();
    let mut links = Vec::new();
    for path in &files.links {
        match NavLinkSet::load(Path::new(path)) {
            Ok(mut more) => links.append(&mut more),
            Err(e) => status.errors.push(NavFileError {
                path: path.clone(),
                code: e.code(),
                message: e.to_string(),
            }),
        }
    }
    if links.len() > crate::links::MAX_NAV_LINKS {
        status.errors.push(NavFileError {
            path: String::new(),
            code: "nav_invalid_link",
            message: format!("{} links in the files together", links.len()),
        });
        links.truncate(crate::links::MAX_NAV_LINKS);
    }
    status.file_links = links.len();
    file_links.0 = links;
    let mut lanes: Option<NavLaneSet> = None;
    for path in &files.lanes {
        let loaded = NavLaneSet::load(Path::new(path)).and_then(|set| match lanes.as_mut() {
            Some(all) => all.append(set),
            None => {
                lanes = Some(set);
                Ok(())
            }
        });
        if let Err(e) = loaded {
            status.errors.push(NavFileError {
                path: path.clone(),
                code: e.code(),
                message: e.to_string(),
            });
        }
    }
    status.file_lanes = lanes.as_ref().map_or(0, |l| l.lanes.len());
    if let Some(set) = lanes {
        match set.graph() {
            Ok(graph) => {
                commands.insert_resource(graph);
                commands.remove_resource::<NavLaneCrowd>();
            }
            Err(e) => status.errors.push(NavFileError {
                path: String::new(),
                code: e.code(),
                message: e.to_string(),
            }),
        }
    }
    for error in &status.errors {
        tracing::warn!(target: "regia::nav", path = %error.path, code = error.code, "{}", error.message);
    }
}

/// What [`sync_authored_links`] reads of an anchor's entity.
type AnchorItem = (
    Entity,
    &'static NavLinkAnchor,
    &'static Transform,
    Option<&'static GlobalTransform>,
    Has<ChildOf>,
);

/// Puts the files' links and the anchors' on the navmesh when they changed.
fn sync_authored_links(
    anchors: Query<AnchorItem>,
    file_links: Res<NavFileLinks>,
    mut status: ResMut<NavAuthoredLinksStatus>,
    mut applied: Local<Option<Vec<OffMeshLink>>>,
    mut stores: crate::origin::NavStores,
    // ECS agent states move here too when this system drains the shared cursor first
    // (audit kimirev-fix-nav-determinism H2).
    mut agents: Query<&mut crate::agent::NavAgentState>,
    mut origin: crate::origin::NavOriginShiftParams,
) {
    let _span = tracing::trace_span!("nav_authored_links").entered();
    // M2: shift the stores and agents before reading anchor poses or writing link endpoints
    // into the mesh/cache, so a rebase tick never stamps new-frame endpoints into a
    // still-old-frame mesh (which the later drain would shift a second time).
    origin.apply(&mut stores, &mut agents);
    let mut found: Vec<(u64, Entity, OffMeshLink)> = Vec::new();
    let mut invalid = 0;
    for (entity, anchor, transform, global, child) in &anchors {
        let pose = match (child, global) {
            (true, Some(g)) => g.0,
            _ => transform.compute_affine(),
        };
        let link = anchor.link(&pose);
        if validate_links(std::slice::from_ref(&link)).is_err() {
            invalid += 1;
            continue;
        }
        found.push((anchor.user_id, entity, link));
    }
    found.sort_by_key(|(user_id, entity, _)| (*user_id, *entity));
    let anchor_links = found
        .len()
        .min(crate::links::MAX_NAV_LINKS.saturating_sub(file_links.0.len()));
    let mut links = file_links.0.clone();
    links.extend(found.into_iter().take(anchor_links).map(|(_, _, link)| link));
    if invalid != status.invalid_anchors && invalid > 0 {
        tracing::warn!(target: "regia::nav", invalid, "nav link anchors left off the navmesh");
    }
    status.invalid_anchors = invalid;
    status.anchor_links = anchor_links;
    // Inert until there is something to apply: links set another way survive until then.
    if applied.as_ref() == Some(&links) || (applied.is_none() && links.is_empty()) {
        return;
    }
    let crate::origin::NavStores {
        cache, mesh, crowd, ..
    } = &mut stores;
    if let Some(cache) = cache.as_mut() {
        cache.set_off_mesh_links(links.clone());
    } else if let Some(mesh) = mesh.as_mut() {
        mesh.off_mesh_links = links.clone();
        if let Some(crowd) = crowd.as_mut() {
            crowd.request_replan_all();
        }
    } else {
        // Nothing to apply to yet: try again when a navmesh arrives.
        return;
    }
    status.applied += 1;
    *applied = Some(links);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::NavMesh;

    /// A 20 m floor centered at `center`, 0.5 m cells, a zero-radius agent (enough for links).
    fn floor(center: Vec3) -> NavMesh {
        let h = 10.0;
        let c = |x: f32, z: f32| center + Vec3::new(x, 0.0, z);
        let tris = vec![[c(-h, -h), c(h, -h), c(h, h)], [c(-h, -h), c(h, h), c(-h, h)]];
        let mut params = crate::NavMeshBuildParams {
            cell_size: 0.5,
            ..Default::default()
        };
        params.agent.radius = 0.0;
        crate::NavMeshBuilder::new(params).build(&tris).expect("builds")
    }

    #[test]
    fn a_minimal_file_takes_the_defaults() {
        let links = NavLinkSet::from_ron_str(
            "(format_version: 1, links: [(from: (0.0, 0.0, 0.0), to: (0.0, -2.0, 3.0))])",
        )
        .expect("valid");
        assert_eq!(
            links,
            vec![OffMeshLink::new(Vec3::ZERO, Vec3::new(0.0, -2.0, 3.0))]
        );
    }

    #[test]
    fn the_spec_example_reads() {
        let text = r#"(
            format_version: 1,
            links: [
                (from: (0.0, 0.0, 0.0), to: (0.0, -2.0, 3.0), bidirectional: false, kind: Drop, user_id: 7),
                (from: (4.0, 0.0, 0.0), to: (4.0, 3.0, 0.0), kind: Climb, flags: 2, duration: 1.5),
                (from: (8.0, 0.0, 0.0), to: (40.0, 0.0, 0.0), kind: Custom(12)),
            ],
        )"#;
        let links = NavLinkSet::from_ron_str(text).expect("valid");
        assert_eq!(links.len(), 3);
        assert_eq!(
            (links[0].kind, links[0].user_id, links[0].bidirectional),
            (NavLinkKind::Drop, 7, false)
        );
        assert_eq!(
            (links[1].kind, links[1].flags, links[1].duration),
            (NavLinkKind::Climb, 2, 1.5)
        );
        assert_eq!(links[2].kind, NavLinkKind::Custom(12));
    }

    #[test]
    fn a_round_trip_keeps_every_field() {
        let links = vec![
            OffMeshLink {
                cost_scale: 2.5,
                enabled: false,
                duration: 0.75,
                area: 3,
                ..OffMeshLink::new(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0))
                    .one_way()
                    .with_kind(NavLinkKind::Teleport)
                    .with_user_id(u64::MAX)
                    .with_flags(0x8001)
                    .with_radius(0.25)
            },
            OffMeshLink::new(Vec3::ZERO, Vec3::X),
        ];
        let text = NavLinkSet::to_ron_string(&links).expect("valid");
        assert_eq!(NavLinkSet::from_ron_str(&text).expect("reads back"), links);
    }

    #[test]
    fn bad_files_are_typed_errors() {
        let e = NavLinkSet::from_ron_str("(format_version: 1, links: [").expect_err("parse");
        assert_eq!(e.code(), "nav_link_parse");
        let e = NavLinkSet::from_ron_str("(format_version: 2, links: [])").expect_err("newer");
        assert_eq!(
            e,
            NavLinkError::Version {
                found: 2,
                supported: 1
            }
        );
        assert_eq!(e.code(), "nav_link_version");
        let e = NavLinkSet::from_ron_str("(format_version: 0, links: [])").expect_err("zero");
        assert_eq!(e.code(), "nav_link_version");
        let e = NavLinkSet::from_ron_str(
            "(format_version: 1, links: [(from: (0.0, 0.0, 0.0), to: (1.0, 0.0, 0.0), radius: 0.0)])",
        )
        .expect_err("invalid");
        assert_eq!(e.code(), "nav_invalid_link");
        let e = NavLinkSet::from_ron_str("(links: [])").expect_err("no version");
        assert_eq!(e.code(), "nav_link_parse");
    }

    #[test]
    fn the_fingerprint_names_every_field_and_kind() {
        let f = nav_links_schema_fingerprint();
        for name in ["format_version", "cost_scale", "duration", "Teleport", "Custom"] {
            assert!(f.contains(name), "{name} in {f}");
        }
        assert!(nav_links_migration_sources().is_empty());
    }

    /// M2 (audit kimirev-fix-nav-determinism): on the rebase tick `sync_authored_links` reads the
    /// (already moved) anchor poses and writes them into the mesh/cache. Forced ahead of every
    /// cursor drainer it used to stamp new-frame endpoints into the still-old-frame mesh, record
    /// them as applied, and let the later drain shift them a second time. Draining the shared
    /// cursor first keeps the endpoints exactly at the moved anchors.
    #[test]
    fn a_rebase_tick_syncs_moved_anchors_exactly_once() {
        use crate::origin::{NavOriginShiftCursor, shift_nav_origin};
        use regia_scene::floating_origin::Rebase;
        use regia_scene::streaming::OriginShifted;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let at = Vec3::new(2000.0, 0.0, 2000.0);
        let to = Vec3::new(0.0, 0.0, -2.0);

        let mut app = App::new();
        app.add_message::<OriginShifted>()
            .init_resource::<NavOriginShiftCursor>()
            .init_resource::<NavFileLinks>()
            .init_resource::<NavAuthoredLinksStatus>()
            .insert_resource(FixedTime::default())
            .insert_resource(floor(at));
        let anchor = app
            .world_mut()
            .spawn((NavLinkAnchor::new(to), Transform::from_translation(at)))
            .id();
        // An ECS agent present too: whoever drains the cursor first must shift its waypoints.
        let agent_goal = at + Vec3::new(4.0, 0.0, 0.0);
        let agent = app
            .world_mut()
            .spawn((
                crate::NavAgent::default(),
                crate::NavAgentState {
                    target: Some(agent_goal),
                    waypoints: vec![agent_goal],
                    ..crate::NavAgentState::default()
                },
                Transform::from_translation(at),
            ))
            .id();
        // The streaming runtime's side of the rebase, applied to the world directly (what
        // `StreamSet::Rebase` does for cell members once per rebase): the anchor and agent
        // transforms move with the cell and the shift is published. The sync is ordered before
        // `shift_nav_origin`, the order that used to double-shift the endpoints.
        app.world_mut()
            .get_mut::<Transform>(anchor)
            .expect("anchor transform")
            .translation += delta;
        app.world_mut()
            .get_mut::<Transform>(agent)
            .expect("agent transform")
            .translation += delta;
        app.world_mut().write_message(OriginShifted {
            rebase: Rebase {
                cell_delta: [1, 0, 0],
                shift,
            },
            origin_cell: [1, 0, 0],
        });
        app.add_systems(FixedUpdate, sync_authored_links.before(shift_nav_origin));
        app.add_systems(FixedUpdate, shift_nav_origin);
        app.world_mut().run_schedule(FixedUpdate);

        let mesh = app.world().resource::<NavMesh>();
        assert_eq!(mesh.off_mesh_links.len(), 1, "{:?}", mesh.off_mesh_links);
        assert_eq!(mesh.off_mesh_links[0].from, at + delta);
        assert_eq!(mesh.off_mesh_links[0].to, at + delta + to);
        // The cursor was drained exactly once: the next tick changes nothing.
        app.world_mut().run_schedule(FixedUpdate);
        let mesh = app.world().resource::<NavMesh>();
        assert_eq!(mesh.off_mesh_links[0].from, at + delta);
        assert_eq!(mesh.off_mesh_links[0].to, at + delta + to);
        // The ECS agent's plan moved exactly once, by whoever drained first.
        let state = app.world().get::<crate::NavAgentState>(agent).expect("state");
        assert_eq!(state.target, Some(agent_goal + delta));
        assert_eq!(state.waypoints, vec![agent_goal + delta]);
    }
}
