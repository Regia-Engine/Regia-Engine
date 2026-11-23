// SPDX-License-Identifier: MPL-2.0
//! Off-mesh link resolution (`docs/specs/92-navmesh-offmesh.md` § 3.1, ADR-1821): the per-mesh
//! table of link graph edges, the per-agent link filter, and each link's status.
//!
//! A link joins two points of the mesh. Resolving it finds the polygon under each endpoint (within
//! the link's snap radius) and turns it into one graph edge, or two when it is bidirectional. The
//! table is built once whenever the mesh or its links change (never per query) and is kept sorted
//! by source polygon, so A* finds the links leaving a polygon with a binary search.
//!
//! A link is left out of the graph when it is disabled, when an endpoint does not resolve, or when
//! an active obstacle covers an endpoint (a crate on a ladder's foot, a closed door modelled as an
//! obstacle). Its [`NavLinkState`] says which.

use regia_app::prelude::{Reflect, ReflectDefault, bevy_reflect};
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::areas::{NavAreaCosts, NavAreaError, area_bit};
use crate::config::NavAgentParams;
use crate::mesh::{NavMesh, OffMeshLink};
use crate::query::find_nearest_on;
use crate::tile_cache::NavObstacleShape;

/// The most links one mesh or link file holds.
pub const MAX_NAV_LINKS: usize = 65_536;

/// Link filter bits used by the engine's defaults (games define their own above these).
pub struct NavLinkFlags;

impl NavLinkFlags {
    /// The default bit of every link: usable by any agent with the default filter.
    pub const WALK: u16 = 1;
    /// A ladder or a run of deck stairs. [`crate::pilot_layer_links`] sets this bit alone, so a
    /// filter that includes only [`Self::WALK`] leaves the climb out.
    pub const PILOT: u16 = 1 << 8;
    /// Every bit.
    pub const ALL: u16 = u16::MAX;
}

/// Endpoint snap radius for a ladder or deck-stair link (metres). Narrower than the default 2 m
/// snap, so a storey a few metres away does not steal the other endpoint.
pub const LAYER_LINK_SNAP_RADIUS: f32 = 0.5;

/// `user_id` base for a ladder from [`pilot_layer_links`]. The link's index in the list is added.
pub const NAV_LADDER_USER_ID: u64 = 0x4E41_4C00;

/// `user_id` base for deck stairs from [`pilot_layer_links`]. The link's index in the list is added.
pub const NAV_DECK_STAIRS_USER_ID: u64 = 0x4E41_5300;

/// A ladder or a run of deck stairs joining two walkable layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavLayerLinkKind {
    /// A ladder. The Atacama-style case is 15 m between the endpoints.
    Ladder,
    /// Stairs between the decks of a building.
    DeckStairs,
}

/// One climb a pilot may take between two layers. A hull bake does not receive it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLayerLink {
    /// Ladder or deck stairs.
    pub kind: NavLayerLinkKind,
    /// Lower endpoint, on its layer.
    pub from: Vec3,
    /// Upper endpoint, on its layer.
    pub to: Vec3,
}

impl NavLayerLink {
    /// A ladder from `from` to `to`.
    pub fn ladder(from: Vec3, to: Vec3) -> Self {
        Self {
            kind: NavLayerLinkKind::Ladder,
            from,
            to,
        }
    }

    /// Deck stairs from `from` to `to`.
    pub fn deck_stairs(from: Vec3, to: Vec3) -> Self {
        Self {
            kind: NavLayerLinkKind::DeckStairs,
            from,
            to,
        }
    }
}

/// Climb links for `agent` when it is a pilot, and an empty list for every other agent. Each link
/// is a bidirectional [`NavLinkKind::Climb`] with only [`NavLinkFlags::PILOT`] set, snap radius
/// [`LAYER_LINK_SNAP_RADIUS`], and a `user_id` of [`NAV_LADDER_USER_ID`] or
/// [`NAV_DECK_STAIRS_USER_ID`] plus the index in `links`.
pub fn pilot_layer_links(agent: &NavAgentParams, links: &[NavLayerLink]) -> Vec<OffMeshLink> {
    if !agent.is_pilot() {
        return Vec::new();
    }
    links
        .iter()
        .enumerate()
        .map(|(index, link)| {
            let base = match link.kind {
                NavLayerLinkKind::Ladder => NAV_LADDER_USER_ID,
                NavLayerLinkKind::DeckStairs => NAV_DECK_STAIRS_USER_ID,
            };
            OffMeshLink::new(link.from, link.to)
                .with_kind(NavLinkKind::Climb)
                .with_user_id(base + index as u64)
                .with_radius(LAYER_LINK_SNAP_RADIUS)
                .with_flags(NavLinkFlags::PILOT)
        })
        .collect()
}

/// What kind of move a link is. The crowd moves every kind along a straight segment over the
/// traversal time, except Jump and Drop with [`NavLinkArcConfig`] on (a ballistic arc, ADR-2589);
/// gameplay picks the animation from the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub enum NavLinkKind {
    /// A jump across a gap or onto a ledge.
    #[default]
    Jump,
    /// A one-way drop down from a ledge.
    Drop,
    /// A ladder or a climb.
    Climb,
    /// A teleport, portal or elevator.
    Teleport,
    /// A game-defined kind.
    Custom(u16),
}

impl NavLinkKind {
    /// The lowercase name used by the agent protocol (`custom` for every custom kind).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Jump => "jump",
            Self::Drop => "drop",
            Self::Climb => "climb",
            Self::Teleport => "teleport",
            Self::Custom(_) => "custom",
        }
    }

    /// Parses a protocol name (`custom` needs the separate custom value).
    pub fn from_name(name: &str, custom: u16) -> Option<Self> {
        Some(match name {
            "jump" => Self::Jump,
            "drop" => Self::Drop,
            "climb" => Self::Climb,
            "teleport" => Self::Teleport,
            "custom" => Self::Custom(custom),
            _ => return None,
        })
    }
}

/// What a path query or a crowd agent may use and what it pays: a link passes when its `flags`
/// share a bit with `include_flags` and none with `exclude_flags`, and when its area is not left
/// out; a polygon passes when its area is not left out. The cost of crossing an area is the mesh's
/// [`NavMesh::area_costs`] entry times `area_costs` here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavQueryFilter {
    /// A link must have at least one of these bits.
    pub include_flags: u16,
    /// A link must have none of these bits.
    pub exclude_flags: u16,
    /// Multiplies the mesh's area costs (default: all 1).
    pub area_costs: NavAreaCosts,
    /// Bit `n` set = area `n` is left out: no path crosses it and an end snaps off it.
    pub excluded_areas: u64,
}

impl Default for NavQueryFilter {
    fn default() -> Self {
        Self {
            include_flags: NavLinkFlags::ALL,
            exclude_flags: 0,
            area_costs: NavAreaCosts::default(),
            excluded_areas: 0,
        }
    }
}

impl NavQueryFilter {
    /// Whether a link with `flags` passes.
    pub fn passes_flags(&self, flags: u16) -> bool {
        flags & self.include_flags != 0 && flags & self.exclude_flags == 0
    }

    /// Whether `link` passes (its enable switch is checked separately, when the table is built).
    pub fn passes(&self, link: &OffMeshLink) -> bool {
        self.passes_flags(link.flags) && self.allows_area(link.area)
    }

    /// Whether `area` may be crossed.
    pub fn allows_area(&self, area: u8) -> bool {
        self.excluded_areas & area_bit(area) == 0
    }

    /// The same filter with `area` left out. Errors: an id outside the table.
    pub fn without_area(mut self, area: u8) -> Result<Self, NavAreaError> {
        let bit = area_bit(area);
        if bit == 0 {
            return Err(NavAreaError::InvalidArea(u32::from(area)));
        }
        self.excluded_areas |= bit;
        Ok(self)
    }

    /// The same filter with `area` costing `scale` times the mesh's cost. Errors: an id outside
    /// the table, or a scale that is not finite and positive.
    pub fn with_area_cost(mut self, area: u8, scale: f32) -> Result<Self, NavAreaError> {
        self.area_costs.set(area, scale)?;
        Ok(self)
    }

    /// Walk links plus pilot climbs ([`NavLinkFlags::WALK`] and [`NavLinkFlags::PILOT`]).
    pub fn pilot() -> Self {
        Self {
            include_flags: NavLinkFlags::WALK | NavLinkFlags::PILOT,
            ..Self::default()
        }
    }

    /// Walk links only. A link that carries [`NavLinkFlags::PILOT`] is left out, including one
    /// that also carries [`NavLinkFlags::WALK`].
    pub fn hull() -> Self {
        Self {
            include_flags: NavLinkFlags::WALK,
            exclude_flags: NavLinkFlags::PILOT,
            ..Self::default()
        }
    }

    fn passes_edge(&self, edge: &LinkEdge) -> bool {
        self.passes_flags(edge.flags) && self.allows_area(edge.area)
    }
}

/// Why a link operation failed. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NavLinkError {
    /// A link value is invalid (non-finite point, a radius or cost scale `<= 0`, a bad duration).
    #[error("invalid off-mesh link {index}: {reason}")]
    InvalidLink {
        /// The link's index in its list.
        index: usize,
        /// What is wrong.
        reason: &'static str,
    },
    /// More than `MAX_NAV_LINKS` links.
    #[error("too many off-mesh links: {0} (the maximum is 65536)")]
    TooManyLinks(usize),
    /// The `.navlinks.ron` text is not valid RON for the format.
    #[error("cannot parse the link file: {0}")]
    Parse(String),
    /// The file's `format_version` is newer than this build reads.
    #[error("link file format_version {found} is newer than the supported {supported}")]
    Version {
        /// The file's version.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// A link generator parameter is invalid.
    #[error("invalid link generator parameters: {0}")]
    InvalidParams(&'static str),
    /// No link has the user id.
    #[error("no off-mesh link with user id {0}")]
    UnknownLink(u64),
    /// A link file could not be read (ADR-2420): missing, unreadable, larger than
    /// `MAX_NAV_FILE_BYTES`, or not UTF-8.
    #[error("cannot read the link file: {0}")]
    Io(String),
}

impl NavLinkError {
    /// The stable lowercase reason code (agent protocol `data.reason`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidLink { .. } | Self::TooManyLinks(_) => "nav_invalid_link",
            Self::Parse(_) => "nav_link_parse",
            Self::Version { .. } => "nav_link_version",
            Self::InvalidParams(_) => "nav_invalid_link_params",
            Self::UnknownLink(_) => "nav_unknown_link",
            Self::Io(_) => "nav_link_io",
        }
    }
}

/// Checks every link of a list and its length.
pub fn validate_links(links: &[OffMeshLink]) -> Result<(), NavLinkError> {
    if links.len() > MAX_NAV_LINKS {
        return Err(NavLinkError::TooManyLinks(links.len()));
    }
    for (index, link) in links.iter().enumerate() {
        link.check()
            .map_err(|reason| NavLinkError::InvalidLink { index, reason })?;
    }
    Ok(())
}

/// Where a link stands in the current graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavLinkState {
    /// Both endpoints resolved; the link is a graph edge.
    Active {
        /// The polygon under `from`.
        from_poly: u32,
        /// The polygon under `to`.
        to_poly: u32,
    },
    /// The link's `enabled` switch is off.
    Disabled,
    /// An active obstacle covers an endpoint.
    Blocked,
    /// An endpoint has no polygon within the link's radius.
    Unresolved {
        /// Whether `from` resolved.
        from: bool,
        /// Whether `to` resolved.
        to: bool,
    },
}

impl NavLinkState {
    /// The lowercase name used by the agent protocol.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Active { .. } => "active",
            Self::Disabled => "disabled",
            Self::Blocked => "blocked",
            Self::Unresolved { .. } => "unresolved",
        }
    }
}

/// One link's status, from [`NavMesh::link_statuses`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavLinkStatus {
    /// The link's index in `NavMesh::off_mesh_links`.
    pub index: usize,
    /// The link's user id.
    pub user_id: u64,
    /// Its state.
    pub state: NavLinkState,
}

/// The obstacles that block link endpoints, with the margins of their carve.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LinkBlockers {
    /// The obstacle shapes.
    pub(crate) shapes: Vec<NavObstacleShape>,
    /// Horizontal inflation (the agent radius for tile-cache obstacles, 0 for carved boxes).
    pub(crate) inflate: f32,
    /// How far below an obstacle's bottom a surface still counts as covered.
    pub(crate) climb: f32,
    /// The tiles of a streamed tile cache that are not loaded (ADR-2589): an endpoint on one does
    /// not resolve.
    pub(crate) unloaded: Option<UnloadedTiles>,
}

/// The unloaded tiles of a streamed tile cache's grid, for link resolution (ADR-2589).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct UnloadedTiles {
    /// World X of the grid's minimum corner.
    pub(crate) origin_x: f32,
    /// World Z of the grid's minimum corner.
    pub(crate) origin_z: f32,
    /// The cell size.
    pub(crate) cell_size: f32,
    /// Cells along X and Z.
    pub(crate) cells: (usize, usize),
    /// The tile edge in cells.
    pub(crate) tile_size: usize,
    /// Tiles along X.
    pub(crate) tiles_x: usize,
    /// Per tile (row-major): not loaded.
    pub(crate) missing: Vec<bool>,
}

impl UnloadedTiles {
    /// Whether the XZ point lies on a tile of the grid that is not loaded.
    pub(crate) fn covers(&self, x: f32, z: f32) -> bool {
        if !(x.is_finite() && z.is_finite() && self.cell_size > 0.0) {
            return false;
        }
        let fx = ((x - self.origin_x) / self.cell_size).floor();
        let fz = ((z - self.origin_z) / self.cell_size).floor();
        if fx < 0.0 || fz < 0.0 || fx as usize >= self.cells.0 || fz as usize >= self.cells.1 {
            return false;
        }
        let ts = self.tile_size.max(1);
        let t = (fz as usize / ts) * self.tiles_x + fx as usize / ts;
        self.missing.get(t).copied().unwrap_or(false)
    }
}

impl LinkBlockers {
    /// Whether the surface point `(x, y, z)` lies under an obstacle.
    fn covers(&self, x: f32, y: f32, z: f32) -> bool {
        self.shapes
            .iter()
            .any(|s| s.blocks(x, y, z, self.inflate, self.climb))
    }
}

/// One direction of an active link as a graph edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LinkEdge {
    /// The take-off polygon.
    pub(crate) from_poly: u32,
    /// The landing polygon.
    pub(crate) to_poly: u32,
    /// The stored step before area pricing (3D length times `cost_scale`). A* multiplies this by
    /// the mesh cost of `area` and by the query filter's scale for that area.
    pub(crate) cost: f32,
    /// The take-off point (the authored endpoint).
    pub(crate) from: Vec3,
    /// The landing point (the authored endpoint).
    pub(crate) to: Vec3,
    /// The link's index in `NavMesh::off_mesh_links`.
    pub(crate) link: u32,
    /// The link's filter bits.
    pub(crate) flags: u16,
    /// The link's area id.
    pub(crate) area: u8,
}

/// The resolved links of a mesh (see the module docs).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LinkTable {
    /// The fingerprint of the link list the table was resolved from.
    pub(crate) fingerprint: u64,
    /// The graph edges sorted by `(from_poly, link, direction)`.
    pub(crate) edges: Vec<LinkEdge>,
    /// Each link's state, by link index.
    pub(crate) states: Vec<NavLinkState>,
    /// The lowest cost per metre any edge has between its polygons' centres, capped at 1: a link
    /// can cost far less than the distance it covers, and the A* heuristic must not count on more.
    pub(crate) lowest_rate: f32,
}

impl LinkTable {
    /// Resolves every link of `mesh` against its polygons and `blockers`.
    pub(crate) fn resolve(mesh: &NavMesh, blockers: &LinkBlockers) -> Self {
        let links = &mesh.off_mesh_links;
        let _span = tracing::debug_span!("nav_link_resolve", links = links.len()).entered();
        let climb = mesh.config.agent.max_climb;
        let mut edges = Vec::new();
        let mut states = Vec::with_capacity(links.len());
        for (i, link) in links.iter().enumerate() {
            if !link.enabled {
                states.push(NavLinkState::Disabled);
                continue;
            }
            let r = if link.radius.is_finite() && link.radius > 0.0 {
                link.radius
            } else {
                0.0
            };
            let extents = Vec3::new(r, r.max(climb), r);
            // An end on a tile that is not loaded does not resolve, even when the snap radius
            // reaches a loaded neighbour (ADR-2589).
            if let Some(gone) = &blockers.unloaded {
                let (from_gone, to_gone) = (
                    gone.covers(link.from.x, link.from.z),
                    gone.covers(link.to.x, link.to.z),
                );
                if from_gone || to_gone {
                    states.push(NavLinkState::Unresolved {
                        from: !from_gone && find_nearest_on(mesh, link.from, extents).is_some(),
                        to: !to_gone && find_nearest_on(mesh, link.to, extents).is_some(),
                    });
                    continue;
                }
            }
            let from = find_nearest_on(mesh, link.from, extents);
            let to = find_nearest_on(mesh, link.to, extents);
            // An end under an obstacle is blocked even when the carve left no polygon in reach;
            // the surface height is the snapped one when there is one.
            let from_y = from.map_or(link.from.y, |(_, p)| p.y);
            let to_y = to.map_or(link.to.y, |(_, p)| p.y);
            if blockers.covers(link.from.x, from_y, link.from.z)
                || blockers.covers(link.to.x, to_y, link.to.z)
            {
                states.push(NavLinkState::Blocked);
                continue;
            }
            let (Some((from_poly, _)), Some((to_poly, _))) = (from, to) else {
                states.push(NavLinkState::Unresolved {
                    from: from.is_some(),
                    to: to.is_some(),
                });
                continue;
            };
            states.push(NavLinkState::Active { from_poly, to_poly });
            let scale = if link.cost_scale.is_finite() && link.cost_scale > 0.0 {
                link.cost_scale
            } else {
                1.0
            };
            let cost = (link.to - link.from).length() * scale;
            edges.push(LinkEdge {
                from_poly,
                to_poly,
                cost,
                from: link.from,
                to: link.to,
                link: i as u32,
                flags: link.flags,
                area: link.area,
            });
            if link.bidirectional {
                edges.push(LinkEdge {
                    from_poly: to_poly,
                    to_poly: from_poly,
                    cost,
                    from: link.to,
                    to: link.from,
                    link: i as u32,
                    flags: link.flags,
                    area: link.area,
                });
            }
        }
        // A stable sort keeps the forward edge before the reverse one of the same link.
        edges.sort_by_key(|e| (e.from_poly, e.link));
        let mut lowest_rate = 1.0f32;
        for e in &edges {
            let (a, b) = (
                mesh.polys[e.from_poly as usize].center_xz(),
                mesh.polys[e.to_poly as usize].center_xz(),
            );
            let d = ((a.x - b.x).powi(2) + (a.z - b.z).powi(2)).sqrt();
            if d > 0.0 {
                lowest_rate = lowest_rate.min(e.cost / d);
            }
        }
        Self {
            fingerprint: links_fingerprint(links),
            edges,
            states,
            lowest_rate,
        }
    }

    /// The edges leaving polygon `poly` (every filter; callers check flags).
    pub(crate) fn edges_from(&self, poly: u32) -> &[LinkEdge] {
        let start = self.edges.partition_point(|e| e.from_poly < poly);
        let end = start + self.edges[start..].partition_point(|e| e.from_poly == poly);
        &self.edges[start..end]
    }

    /// The cheapest edge from `a` to `b` that passes `filter` (the lowest link index on a tie).
    /// `costs` is the mesh's area table.
    pub(crate) fn between(
        &self,
        a: u32,
        b: u32,
        filter: &NavQueryFilter,
        costs: &NavAreaCosts,
    ) -> Option<&LinkEdge> {
        let price = |e: &LinkEdge| e.cost * costs.get(e.area) * filter.area_costs.get(e.area);
        let mut best: Option<&LinkEdge> = None;
        for e in self.edges_from(a) {
            if e.to_poly == b && filter.passes_edge(e) && best.is_none_or(|x| price(e) < price(x)) {
                best = Some(e);
            }
        }
        best
    }

    /// Whether the table has no edges.
    pub(crate) fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Ballistic link crossings (ADR-2589): with `enabled`, crowd agents cross [`NavLinkKind::Jump`]
/// links along a jump arc and [`NavLinkKind::Drop`] links along a falling parabola, and the
/// crossing takes the arc's flight time; the other kinds keep the straight segment. Off by
/// default, so a crowd without it steps exactly as before.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLinkArcConfig {
    /// Jump and Drop links follow arcs.
    pub enabled: bool,
    /// Gravity in m/s² (`> 0`, finite).
    pub gravity: f32,
    /// The fastest vertical take-off speed in m/s (`> 0`, finite): it caps the apex at
    /// `max_jump_speed² / (2 * gravity)` above the take-off point.
    pub max_jump_speed: f32,
    /// How far the apex of a jump rises above its higher end, in metres (`>= 0`, finite), when
    /// `max_jump_speed` allows.
    pub apex_clearance: f32,
}

impl Default for NavLinkArcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gravity: 9.81,
            max_jump_speed: 6.0,
            apex_clearance: 0.5,
        }
    }
}

impl NavLinkArcConfig {
    /// Arcs on, with the default gravity, jump speed and clearance.
    #[must_use]
    pub fn on() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Whether every value is in range (`enabled` aside).
    pub fn is_valid(&self) -> bool {
        let pos = |v: f32| v.is_finite() && v > 0.0;
        pos(self.gravity)
            && pos(self.max_jump_speed)
            && self.apex_clearance.is_finite()
            && self.apex_clearance >= 0.0
    }

    /// The arc for a crossing of a `kind` link from `start` to `end`, or `None` when the kind
    /// moves straight (Climb, Teleport, Custom), arcs are off, or the arc has no flight time
    /// (both ends at one point).
    #[must_use]
    pub fn arc(&self, kind: NavLinkKind, start: Vec3, end: Vec3) -> Option<NavLinkArc> {
        if !self.enabled || !self.is_valid() || !start.is_finite() || !end.is_finite() {
            return None;
        }
        let g = self.gravity;
        let up = (end.y - start.y).max(0.0);
        let reach = self.max_jump_speed * self.max_jump_speed / (2.0 * g);
        let rise = match kind {
            // A drop leaves level: the apex is the take-off point. A "drop" that goes up jumps.
            NavLinkKind::Drop if end.y <= start.y => 0.0,
            NavLinkKind::Jump | NavLinkKind::Drop => {
                // Not `clamp`: the reach may lie below the ledge, and then the ledge wins.
                let wanted = (up + self.apex_clearance).min(reach);
                if wanted < up { up } else { wanted }
            }
            _ => return None,
        };
        let limited = up > reach;
        let vy = (2.0 * g * rise).sqrt();
        let fall = (start.y + rise - end.y).max(0.0);
        let rise_seconds = vy / g;
        let flight_seconds = rise_seconds + (2.0 * fall / g).sqrt();
        if !(flight_seconds.is_finite() && flight_seconds > 1e-6) {
            return None;
        }
        let horizontal = Vec3::new(end.x - start.x, 0.0, end.z - start.z) / flight_seconds;
        let takeoff_velocity = Vec3::new(horizontal.x, vy, horizontal.z);
        let apex = Vec3::new(
            start.x + horizontal.x * rise_seconds,
            start.y + rise,
            start.z + horizontal.z * rise_seconds,
        );
        Some(NavLinkArc {
            start,
            end,
            apex,
            takeoff_velocity,
            gravity: g,
            flight_seconds,
            speed_limited: limited,
        })
    }
}

/// The ballistic path of one link crossing ([`NavLinkArcConfig::arc`]): a parabola from `start`
/// to `end` under `gravity`, with constant horizontal speed. Gameplay drives a jump animation
/// from it (take-off speed, apex, flight time).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLinkArc {
    /// The take-off point.
    pub start: Vec3,
    /// The landing point.
    pub end: Vec3,
    /// The highest point of the flight (`start` for a drop).
    pub apex: Vec3,
    /// The velocity at take-off (m/s).
    pub takeoff_velocity: Vec3,
    /// Gravity (m/s²).
    pub gravity: f32,
    /// Seconds from take-off to landing.
    pub flight_seconds: f32,
    /// The landing is higher than `max_jump_speed` reaches: the arc still lands there (its apex
    /// is the landing height) and the take-off is faster than allowed. Gameplay may play a climb.
    pub speed_limited: bool,
}

impl NavLinkArc {
    /// The point `seconds` after take-off (clamped to the flight).
    #[must_use]
    pub fn point_at(&self, seconds: f32) -> Vec3 {
        let t = seconds.clamp(0.0, self.flight_seconds);
        if t >= self.flight_seconds {
            return self.end;
        }
        let v = self.takeoff_velocity;
        Vec3::new(
            self.start.x + v.x * t,
            self.start.y + v.y * t - 0.5 * self.gravity * t * t,
            self.start.z + v.z * t,
        )
    }

    /// The velocity `seconds` after take-off (clamped to the flight).
    #[must_use]
    pub fn velocity_at(&self, seconds: f32) -> Vec3 {
        let t = seconds.clamp(0.0, self.flight_seconds);
        let v = self.takeoff_velocity;
        Vec3::new(v.x, v.y - self.gravity * t, v.z)
    }
}

/// A 64-bit FNV-1a fingerprint of every field of every link, in order.
pub(crate) fn links_fingerprint(links: &[OffMeshLink]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(&(links.len() as u64).to_le_bytes());
    for l in links {
        for v in [l.from, l.to] {
            for c in [v.x, v.y, v.z] {
                eat(&c.to_bits().to_le_bytes());
            }
        }
        let kind: u32 = match l.kind {
            NavLinkKind::Jump => 0,
            NavLinkKind::Drop => 1,
            NavLinkKind::Climb => 2,
            NavLinkKind::Teleport => 3,
            NavLinkKind::Custom(c) => 0x1_0000 | u32::from(c),
        };
        eat(&[u8::from(l.bidirectional), l.area, u8::from(l.enabled)]);
        eat(&l.user_id.to_le_bytes());
        eat(&kind.to_le_bytes());
        eat(&l.flags.to_le_bytes());
        for f in [l.radius, l.cost_scale, l.duration] {
            eat(&f.to_bits().to_le_bytes());
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NavMeshBuildParams;
    use crate::heightfield::WalkableGrid;
    use crate::query::NavQuery;

    /// Two 4 x 4 m islands at `x ∈ [0, 4)` and `x ∈ [6, 10)`, `z ∈ [0, 4)`, both at height 0.
    fn islands() -> NavMesh {
        let (w, h) = (10usize, 4usize);
        let mut cells = vec![None; w * h];
        for z in 0..h {
            for x in (0..4).chain(6..10) {
                cells[z * w + x] = Some(0.0);
            }
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: w,
            height: h,
            cells,
            areas: Vec::new(),
        };
        NavMesh::from_grid(&grid, NavMeshBuildParams::default())
    }

    fn jump(z: f32) -> OffMeshLink {
        OffMeshLink::new(Vec3::new(3.5, 0.0, z), Vec3::new(6.5, 0.0, z)).with_radius(0.5)
    }

    fn path(mesh: &NavMesh, filter: &NavQueryFilter) -> Option<crate::NavPath> {
        NavQuery {
            mesh,
            obstacles: None,
        }
        .find_path_filtered(
            Vec3::new(0.5, 0.0, 2.0),
            Vec3::new(9.5, 0.0, 2.0),
            Vec3::splat(1.0),
            filter,
        )
    }

    #[test]
    fn new_links_carry_the_documented_defaults() {
        let l = OffMeshLink::new(Vec3::ZERO, Vec3::X);
        assert!(l.bidirectional && l.enabled);
        assert_eq!(l.kind, NavLinkKind::Jump);
        assert_eq!(l.flags, NavLinkFlags::WALK);
        assert_eq!(
            (l.radius, l.cost_scale, l.duration, l.user_id, l.area),
            (2.0, 1.0, 0.0, 0, 0)
        );
        let l = l
            .one_way()
            .with_kind(NavLinkKind::Drop)
            .with_user_id(7)
            .with_flags(4);
        assert!(!l.bidirectional);
        assert_eq!((l.kind, l.user_id, l.flags), (NavLinkKind::Drop, 7, 4));
    }

    #[test]
    fn validation_rejects_nonsense_values() {
        let ok = OffMeshLink::new(Vec3::ZERO, Vec3::X);
        assert!(validate_links(&[ok]).is_ok());
        let bad = [
            OffMeshLink {
                from: Vec3::new(f32::NAN, 0.0, 0.0),
                ..ok
            },
            OffMeshLink {
                to: Vec3::new(0.0, f32::INFINITY, 0.0),
                ..ok
            },
            OffMeshLink { radius: 0.0, ..ok },
            OffMeshLink {
                cost_scale: -1.0,
                ..ok
            },
            OffMeshLink {
                cost_scale: f32::NAN,
                ..ok
            },
            OffMeshLink { duration: -0.5, ..ok },
            OffMeshLink {
                duration: f32::INFINITY,
                ..ok
            },
        ];
        for (i, link) in bad.iter().enumerate() {
            let e = validate_links(&[ok, *link]).expect_err("invalid link");
            assert_eq!(e.code(), "nav_invalid_link", "case {i}");
            assert!(
                matches!(e, NavLinkError::InvalidLink { index: 1, .. }),
                "case {i}: {e}"
            );
        }
        let many = vec![ok; MAX_NAV_LINKS + 1];
        assert_eq!(
            validate_links(&many),
            Err(NavLinkError::TooManyLinks(MAX_NAV_LINKS + 1))
        );
    }

    #[test]
    fn the_table_is_resolved_once_and_reused_by_queries() {
        let mut mesh = islands();
        mesh.set_off_mesh_links(vec![jump(2.0)]);
        assert!(matches!(mesh.link_table(), std::borrow::Cow::Borrowed(_)));
        for _ in 0..10 {
            assert!(path(&mesh, &NavQueryFilter::default()).is_some());
            assert!(matches!(mesh.link_table(), std::borrow::Cow::Borrowed(_)));
        }
        // An in-place edit is noticed: the query resolves a temporary table and stays correct.
        mesh.off_mesh_links[0].enabled = false;
        assert!(matches!(mesh.link_table(), std::borrow::Cow::Owned(_)));
        assert!(
            path(&mesh, &NavQueryFilter::default()).is_none(),
            "the edited link is off"
        );
        mesh.off_mesh_links[0].enabled = true;
        assert!(matches!(mesh.link_table(), std::borrow::Cow::Borrowed(_)));
        assert!(path(&mesh, &NavQueryFilter::default()).is_some());
    }

    #[test]
    fn disabled_and_filtered_links_are_not_used() {
        let mut mesh = islands();
        mesh.set_off_mesh_links(vec![jump(2.0).with_user_id(9).with_flags(2)]);
        assert!(path(&mesh, &NavQueryFilter::default()).is_some());
        let no_ladders = NavQueryFilter {
            exclude_flags: 2,
            ..Default::default()
        };
        assert!(path(&mesh, &no_ladders).is_none(), "excluded flag");
        let walk_only = NavQueryFilter {
            include_flags: NavLinkFlags::WALK,
            ..Default::default()
        };
        assert!(path(&mesh, &walk_only).is_none(), "missing include bit");

        assert_eq!(mesh.set_link_enabled(9, false), 1);
        assert_eq!(mesh.set_link_enabled(9, false), 0, "already off");
        assert_eq!(mesh.link_statuses()[0].state, NavLinkState::Disabled);
        assert!(path(&mesh, &NavQueryFilter::default()).is_none());
        assert_eq!(mesh.set_link_enabled(9, true), 1);
        assert!(matches!(
            mesh.link_statuses()[0].state,
            NavLinkState::Active { .. }
        ));
        assert!(path(&mesh, &NavQueryFilter::default()).is_some());
    }

    #[test]
    fn the_cheaper_link_wins_and_cost_scale_flips_the_choice() {
        let mut mesh = islands();
        // Two links across the same gap at z = 0.5 and z = 3.5; the start and goal sit at z = 0.5.
        let near = jump(0.5).with_user_id(1);
        let far = jump(3.5).with_user_id(2);
        mesh.set_off_mesh_links(vec![near, far]);
        let q = |mesh: &NavMesh| {
            NavQuery {
                mesh,
                obstacles: None,
            }
            .find_path(
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::new(9.5, 0.0, 0.5),
                Vec3::splat(1.0),
            )
            .expect("a route")
        };
        let p = q(&mesh);
        assert!(
            p.waypoints
                .iter()
                .any(|w| (w.z - 0.5).abs() < 1e-3 && (w.x - 3.5).abs() < 1e-3)
        );
        // Both links join the same polygon pair, so the cheaper edge is the one A* and the path use.
        mesh.set_off_mesh_links(vec![
            OffMeshLink {
                cost_scale: 10.0,
                ..near
            },
            far,
        ]);
        let p = q(&mesh);
        assert!(
            p.waypoints
                .iter()
                .any(|w| (w.z - 3.5).abs() < 1e-3 && (w.x - 3.5).abs() < 1e-3),
            "the expensive near link loses: {:?}",
            p.waypoints
        );
    }

    /// The searched cost is length times `cost_scale` times the mesh area cost times the filter
    /// scale. A short link on an expensive area loses to a longer link on plain ground, and a
    /// filter scale can hand the short link the win again.
    #[test]
    fn link_search_cost_includes_area_cost_and_filter_scale() {
        let mut mesh = islands();
        let mut near = jump(0.5);
        near.area = 3;
        let far = jump(3.5);
        mesh.set_off_mesh_links(vec![near, far]);
        let mut costs = crate::NavAreaCosts::default();
        costs.set(3, 8.0).expect("area 3");
        mesh.set_area_costs(costs);
        let route = |mesh: &NavMesh, filter: &NavQueryFilter| {
            NavQuery {
                mesh,
                obstacles: None,
            }
            .find_path_filtered(
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::new(9.5, 0.0, 0.5),
                Vec3::splat(1.0),
                filter,
            )
            .expect("a route")
        };
        let uses = |path: &crate::NavPath, z: f32| {
            path.waypoints
                .iter()
                .any(|w| (w.z - z).abs() < 1e-3 && (w.x - 3.5).abs() < 1e-3)
        };
        let expensive = route(&mesh, &NavQueryFilter::default());
        assert!(
            uses(&expensive, 3.5),
            "area cost 8 makes the near link lose: {:?}",
            expensive.waypoints
        );
        let cheap = NavQueryFilter::default()
            .with_area_cost(3, 0.05)
            .expect("filter scale");
        let scaled = route(&mesh, &cheap);
        assert!(
            uses(&scaled, 0.5),
            "filter scale 0.05 brings the near link back: {:?}",
            scaled.waypoints
        );
    }

    #[test]
    fn statuses_report_unresolved_ends() {
        let mut mesh = islands();
        mesh.set_off_mesh_links(vec![
            jump(2.0),
            OffMeshLink::new(Vec3::new(3.5, 0.0, 2.0), Vec3::new(5.0, 0.0, 30.0)).with_radius(0.5),
            OffMeshLink::new(Vec3::new(3.5, 9.0, 2.0), Vec3::new(6.5, 0.0, 2.0)).with_radius(0.5),
        ]);
        let s = mesh.link_statuses();
        assert!(matches!(s[0].state, NavLinkState::Active { .. }));
        assert_eq!(
            s[1].state,
            NavLinkState::Unresolved {
                from: true,
                to: false
            }
        );
        assert_eq!(
            s[2].state,
            NavLinkState::Unresolved {
                from: false,
                to: true
            }
        );
        assert_eq!(s[2].state.name(), "unresolved");
    }

    #[test]
    fn a_carved_box_on_a_landing_blocks_the_link() {
        let mut mesh = islands();
        mesh.set_off_mesh_links(vec![jump(2.0)]);
        let mut obstacles = crate::NavObstacles::default();
        // A box over the landing's cell only: the island stays walkable around it.
        let id = obstacles.add_box(Vec3::new(6.0, -1.0, 1.0), Vec3::new(7.0, 2.0, 3.0));
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_path(
                Vec3::new(0.5, 0.0, 2.0),
                Vec3::new(9.5, 0.0, 2.0),
                Vec3::splat(1.0)
            )
            .is_none(),
            "the landing is covered"
        );
        obstacles.remove(id);
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_path(
                Vec3::new(0.5, 0.0, 2.0),
                Vec3::new(9.5, 0.0, 2.0),
                Vec3::splat(1.0)
            )
            .is_some()
        );
    }

    #[test]
    fn equal_cost_links_between_one_polygon_pair_break_ties_by_link_index() {
        // Spec 92 § 5: two links of the same length join the same two polygons; the lower index wins,
        // so swapping the list order swaps the crossing.
        let crosses_at = |p: &crate::NavPath, z: f32| {
            p.waypoints
                .iter()
                .any(|w| (w.z - z).abs() < 1e-3 && (w.x - 3.5).abs() < 1e-3)
        };
        let mut mesh = islands();
        mesh.set_off_mesh_links(vec![jump(1.0), jump(3.0)]);
        let p = path(&mesh, &NavQueryFilter::default()).expect("a route");
        assert!(crosses_at(&p, 1.0) && !crosses_at(&p, 3.0));
        assert_eq!(
            p,
            path(&mesh, &NavQueryFilter::default()).expect("a route"),
            "repeatable"
        );
        mesh.set_off_mesh_links(vec![jump(3.0), jump(1.0)]);
        let p = path(&mesh, &NavQueryFilter::default()).expect("a route");
        assert!(crosses_at(&p, 3.0) && !crosses_at(&p, 1.0));
    }

    #[test]
    fn kinds_and_states_have_protocol_names() {
        for (k, n) in [
            (NavLinkKind::Jump, "jump"),
            (NavLinkKind::Drop, "drop"),
            (NavLinkKind::Climb, "climb"),
            (NavLinkKind::Teleport, "teleport"),
            (NavLinkKind::Custom(3), "custom"),
        ] {
            assert_eq!(k.name(), n);
            assert_eq!(NavLinkKind::from_name(n, 3), Some(k));
        }
        assert_eq!(NavLinkKind::from_name("fly", 0), None);
        assert_eq!(NavLinkState::Blocked.name(), "blocked");
        assert_eq!(NavLinkState::Disabled.name(), "disabled");
    }
}
