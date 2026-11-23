// SPDX-License-Identifier: MPL-2.0
//! Polygon area costs (ADR-1934): which kind of ground a polygon is, what it costs to cross, and
//! who may cross it.
//!
//! Every [`crate::NavPoly`] has an area id below [`MAX_NAV_AREAS`]; id 0 is plain ground. Area
//! ids come from [`NavAreaVolume`]s painted over the walkable cells, so polygons split where an
//! area ends. A [`NavAreaCosts`] table on the mesh says what each area costs for everyone (mud
//! costs 3 per metre), and the [`crate::NavQueryFilter`] of a query or crowd agent multiplies it
//! and can leave areas out altogether (this unit does not swim).
//!
//! Paths compare costs, so a cost above 1 makes an area less attractive, not slower: crowd speed
//! does not change with the area.

use thiserror::Error;

use crate::heightfield::WalkableGrid;
use crate::tile_cache::{NavObstacleShape, NavTileCacheError};

/// How many area ids exist (`0..MAX_NAV_AREAS`).
pub const MAX_NAV_AREAS: usize = 64;

/// The area id of plain ground.
pub const NAV_AREA_DEFAULT: u8 = 0;

/// Tunnel floor. [`NavAreaCosts::passages`] prices a metre here at [`TUNNEL_AREA_COST`].
pub const NAV_AREA_TUNNEL: u8 = 60;

/// A flank route beside a tunnel. [`NavAreaCosts::passages`] prices a metre here at
/// [`FLANK_AREA_COST`].
pub const NAV_AREA_FLANK: u8 = 61;

/// Cost of one metre of [`NAV_AREA_TUNNEL`]: three quarters of a metre of open ground.
pub const TUNNEL_AREA_COST: f32 = 0.75;

/// Cost of one metre of [`NAV_AREA_FLANK`]: one fifth of a metre of open ground.
pub const FLANK_AREA_COST: f32 = 0.20;

/// The most area volumes one tile cache or one build takes.
pub const MAX_NAV_AREA_VOLUMES: usize = 4096;

/// Why an area operation failed. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NavAreaError {
    /// An area id at or above [`MAX_NAV_AREAS`].
    #[error("area id {0} is outside 0..=63")]
    InvalidArea(u32),
    /// A cost that is not finite and positive.
    #[error("area cost must be finite and positive, got {0}")]
    InvalidCost(f32),
}

impl NavAreaError {
    /// The stable lowercase reason code (agent protocol `data.reason`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidArea(_) => "nav_invalid_area",
            Self::InvalidCost(_) => "nav_invalid_area_cost",
        }
    }
}

/// A cost per area id. Every entry starts at 1. An id outside the table reads as 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavAreaCosts {
    costs: [f32; MAX_NAV_AREAS],
}

impl Default for NavAreaCosts {
    fn default() -> Self {
        Self {
            costs: [1.0; MAX_NAV_AREAS],
        }
    }
}

impl NavAreaCosts {
    /// The cost of `area`.
    pub fn get(&self, area: u8) -> f32 {
        self.costs.get(area as usize).copied().unwrap_or(1.0)
    }

    /// Sets the cost of `area`. Errors: an id outside the table, or a cost that is not finite and
    /// positive.
    pub fn set(&mut self, area: u8, cost: f32) -> Result<(), NavAreaError> {
        let slot = self
            .costs
            .get_mut(area as usize)
            .ok_or(NavAreaError::InvalidArea(u32::from(area)))?;
        if !(cost.is_finite() && cost > 0.0) {
            return Err(NavAreaError::InvalidCost(cost));
        }
        *slot = cost;
        Ok(())
    }

    /// Open ground at 1, a tunnel at [`TUNNEL_AREA_COST`] and a flank at [`FLANK_AREA_COST`].
    /// Both passage costs are below 1, so a path of the same length prefers the tunnel and the
    /// straight-line heuristic stays admissible.
    pub fn passages() -> Self {
        let mut costs = Self::default();
        costs.costs[usize::from(NAV_AREA_TUNNEL)] = TUNNEL_AREA_COST;
        costs.costs[usize::from(NAV_AREA_FLANK)] = FLANK_AREA_COST;
        costs
    }

    /// The entries that differ from 1, as `(area, cost)` in id order.
    pub fn changed(&self) -> impl Iterator<Item = (u8, f32)> + '_ {
        self.costs
            .iter()
            .enumerate()
            .filter(|(_, c)| **c != 1.0)
            .map(|(a, c)| (a as u8, *c))
    }
}

/// The bit of `area` in an area mask, or 0 for an id outside the table.
pub(crate) fn area_bit(area: u8) -> u64 {
    if (area as usize) < MAX_NAV_AREAS {
        1 << area
    } else {
        0
    }
}

/// One query's view of the costs: the mesh's table times the filter's, with the areas it leaves
/// out, worked out once per path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StepCosts {
    table: [f32; MAX_NAV_AREAS],
    excluded: u64,
    /// The smallest cost per metre any allowed area has: the A* heuristic scales the straight-line
    /// distance by this, so an area never makes it overestimate.
    pub(crate) lowest: f32,
}

impl StepCosts {
    pub(crate) fn new(mesh: &NavAreaCosts, filter: &NavAreaCosts, excluded: u64) -> Self {
        let mut table = [1.0; MAX_NAV_AREAS];
        let mut lowest = f32::INFINITY;
        for (area, slot) in table.iter_mut().enumerate() {
            *slot = mesh.costs[area] * filter.costs[area];
            if excluded & (1 << area) == 0 {
                lowest = lowest.min(*slot);
            }
        }
        Self {
            table,
            excluded,
            lowest: if lowest.is_finite() { lowest } else { 1.0 },
        }
    }

    /// The cost per metre of `area`.
    pub(crate) fn of(&self, area: u8) -> f32 {
        self.table.get(area as usize).copied().unwrap_or(1.0)
    }

    /// Whether the query may use `area`.
    pub(crate) fn allows(&self, area: u8) -> bool {
        self.excluded & area_bit(area) == 0
    }
}

/// A shape painted over the walkable surface: every cell whose centre and surface height lie in
/// the shape becomes `area`. Where volumes overlap, the later one wins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavAreaVolume {
    /// The painted shape (the agent radius does not inflate it).
    pub shape: NavObstacleShape,
    /// The area id (`< MAX_NAV_AREAS`).
    pub area: u8,
}

impl NavAreaVolume {
    /// A volume painting `area` over `shape`.
    pub fn new(shape: NavObstacleShape, area: u8) -> Self {
        Self { shape, area }
    }

    /// Checks the shape and the id; returns the normalised volume. Errors:
    /// [`NavTileCacheError::InvalidObstacle`] for the shape, [`NavTileCacheError::Area`] for the id.
    pub fn validated(self) -> Result<Self, NavTileCacheError> {
        if self.area as usize >= MAX_NAV_AREAS {
            return Err(NavAreaError::InvalidArea(u32::from(self.area)).into());
        }
        Ok(Self {
            shape: self.shape.validated()?,
            area: self.area,
        })
    }
}

/// The area of the cell `(x, z)` with surface height `y`: the last of `volumes` covering it, or
/// `base`. `climb` is the agent's: a surface slightly under a shape's bottom still counts, as for
/// obstacles.
pub(crate) fn painted_area(
    grid: &WalkableGrid,
    volumes: &[NavAreaVolume],
    (x, z, y): (usize, usize, f32),
    climb: f32,
    base: u8,
) -> u8 {
    let cx = grid.origin.x + (x as f32 + 0.5) * grid.cell_size;
    let cz = grid.origin.z + (z as f32 + 0.5) * grid.cell_size;
    volumes
        .iter()
        .rev()
        .find(|v| v.shape.blocks(cx, y, cz, 0.0, climb))
        .map_or(base, |v| v.area)
}

/// Area volumes as per-triangle area ids (ADR-2420): each triangle takes the area of the last of
/// `volumes` holding its centroid (`climb` as for [`NavAreaVolume`] cells), or `base[i]` (0 when
/// `base` is empty). A cook uses it to turn a level's area volumes into triangle areas, so the
/// cooked tiles carry them without the volumes. The result feeds
/// [`crate::NavMeshBuilder::build_with_triangle_areas`] and
/// [`crate::NavTileCache::build_with_triangle_areas`]. Invalid volumes paint nothing.
pub fn paint_triangle_areas(
    triangles: &[[regia_core::math::Vec3; 3]],
    base: &[u8],
    volumes: &[NavAreaVolume],
    climb: f32,
) -> Vec<u8> {
    let valid: Vec<NavAreaVolume> = volumes.iter().filter_map(|v| v.validated().ok()).collect();
    triangles
        .iter()
        .enumerate()
        .map(|(i, tri)| {
            let own = base.get(i).copied().unwrap_or(NAV_AREA_DEFAULT);
            let c = (tri[0] + tri[1] + tri[2]) / 3.0;
            valid
                .iter()
                .rev()
                .find(|v| v.shape.blocks(c.x, c.y, c.z, 0.0, climb))
                .map_or(own, |v| v.area)
        })
        .collect()
}

/// Paints `volumes` over every walkable cell of `grid`.
pub(crate) fn paint_grid(grid: &mut WalkableGrid, volumes: &[NavAreaVolume], climb: f32) {
    if volumes.is_empty() {
        return;
    }
    let mut areas = vec![0u8; grid.cells.len()];
    for (i, area) in areas.iter_mut().enumerate() {
        let (x, z) = (i % grid.width, i / grid.width);
        if let Some(y) = grid.cells[i] {
            *area = painted_area(grid, volumes, (x, z, y), climb, grid.area_at(i));
        }
    }
    grid.areas = areas;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_table_is_all_ones_and_rejects_bad_values() {
        let mut t = NavAreaCosts::default();
        assert_eq!(t.get(0), 1.0);
        assert_eq!(t.get(63), 1.0);
        assert_eq!(t.get(200), 1.0, "an id outside the table reads as 1");
        assert_eq!(t.changed().count(), 0);
        assert!(matches!(t.set(64, 2.0), Err(NavAreaError::InvalidArea(64))));
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(t.set(1, bad).map_err(|e| e.code()), Err("nav_invalid_area_cost"));
        }
        t.set(5, 2.5).expect("a valid cost");
        assert_eq!(t.changed().collect::<Vec<_>>(), vec![(5, 2.5)]);
    }

    #[test]
    fn step_costs_multiply_and_the_heuristic_scale_ignores_left_out_areas() {
        let mut mesh = NavAreaCosts::default();
        let mut filter = NavAreaCosts::default();
        mesh.set(1, 4.0).expect("cost");
        filter.set(1, 0.5).expect("cost");
        filter.set(2, 0.25).expect("cost");
        let open = StepCosts::new(&mesh, &filter, 0);
        assert_eq!(open.of(1), 2.0);
        assert_eq!(open.lowest, 0.25);
        let closed = StepCosts::new(&mesh, &filter, area_bit(2));
        assert_eq!(
            closed.lowest, 1.0,
            "area 2 is left out, so its low cost does not count"
        );
        assert!(!closed.allows(2));
        assert!(closed.allows(1));
        assert!(closed.allows(200), "no bit, no exclusion");
    }
}
