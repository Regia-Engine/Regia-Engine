// SPDX-License-Identifier: MPL-2.0
//! The navmesh builder (`docs/specs/22-tier2.md` § T2.2): the top of the Recast-style pipeline,
//! running voxelisation → walkable marking → regions → polygon mesh (a greedy rectangle merge of
//! the walkable grid) over a triangle soup. Contours are traced beside that for display only; there
//! is no detail mesh.

use regia_core::math::Vec3;
use thiserror::Error;

use crate::areas::{MAX_NAV_AREA_VOLUMES, MAX_NAV_AREAS, NavAreaVolume, paint_grid};
use std::f32::consts::FRAC_PI_2;

use crate::config::{NavAgentParams, NavMeshBuildParams};
use crate::heightfield::{HeightField, WalkableGrid, WalkableLayers, WallExtents};
use crate::mesh::NavMesh;

/// Why a navmesh build failed.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NavBuildError {
    /// The input contained no triangles.
    #[error("navmesh build received no geometry")]
    NoGeometry,
    /// No cell survived walkable marking: the grid was empty after the slope, headroom and
    /// agent-radius erosion passes (a floor smaller than the radius can fail even with spans).
    #[error("no walkable surface found for the given agent parameters")]
    NoWalkableSurface,
    /// The build parameters or the geometry cannot be voxelised (a non-finite or non-positive
    /// voxel size, a negative border, non-finite vertices, or a grid too large to allocate).
    #[error("invalid navmesh build input: {0}")]
    InvalidInput(&'static str),
}

/// The most voxel columns one build may allocate.
const MAX_GRID_CELLS: f64 = 64.0 * 1024.0 * 1024.0;

/// Checks the build parameters and the XZ extent of the (padded) geometry bounds.
fn validate(params: &NavMeshBuildParams, min: Vec3, max: Vec3) -> Result<(), NavBuildError> {
    let size_ok = |v: f32| v.is_finite() && v > 0.0;
    if !size_ok(params.cell_size) {
        return Err(NavBuildError::InvalidInput(
            "cell_size must be finite and positive",
        ));
    }
    if !size_ok(params.cell_height) {
        return Err(NavBuildError::InvalidInput(
            "cell_height must be finite and positive",
        ));
    }
    if !(params.border_size.is_finite() && params.border_size >= 0.0) {
        return Err(NavBuildError::InvalidInput(
            "border_size must be finite and non-negative",
        ));
    }
    validate_agent(&params.agent)?;
    let border = f64::from(params.border_size);
    let cell = f64::from(params.cell_size);
    let w = (f64::from(max.x) - f64::from(min.x) + 2.0 * border) / cell + 2.0;
    let h = (f64::from(max.z) - f64::from(min.z) + 2.0 * border) / cell + 2.0;
    if w * h > MAX_GRID_CELLS {
        return Err(NavBuildError::InvalidInput(
            "cell_size is too small for the geometry bounds",
        ));
    }
    Ok(())
}

/// Checks the agent the mesh is built for: a finite `radius >= 0`, a finite `height > 0`, a finite
/// `max_climb >= 0` and a finite `max_slope` in `[0, pi/2)`. A NaN or infinite value would turn the
/// erosion, headroom, step or slope test off without a word (a NaN `max_climb` joins cells across
/// any height, a negative one joins none), so it is an error instead.
pub(crate) fn validate_agent(agent: &NavAgentParams) -> Result<(), NavBuildError> {
    if !(agent.radius.is_finite() && agent.radius >= 0.0) {
        return Err(NavBuildError::InvalidInput(
            "agent radius must be finite and non-negative",
        ));
    }
    if !(agent.height.is_finite() && agent.height > 0.0) {
        return Err(NavBuildError::InvalidInput(
            "agent height must be finite and positive",
        ));
    }
    if !(agent.max_climb.is_finite() && agent.max_climb >= 0.0) {
        return Err(NavBuildError::InvalidInput(
            "agent max_climb must be finite and non-negative",
        ));
    }
    if !(agent.max_slope.is_finite() && agent.max_slope >= 0.0 && agent.max_slope < FRAC_PI_2) {
        return Err(NavBuildError::InvalidInput(
            "agent max_slope must be finite and within [0, pi/2) radians",
        ));
    }
    Ok(())
}

/// Builds a [`NavMesh`] from a triangle soup (typically extracted from level colliders).
///
/// The pipeline is Recast-style: conservative voxelisation into a span heightfield (a cell takes
/// every triangle overlapping its square, so geometry thinner than `cell_size` is seen; ADR-8899), walkable
/// marking (slope + headroom + radius erosion), region flood fill (step connectivity), then
/// polygonisation into convex polygons (a greedy rectangle merge of the walkable grid; contours
/// are traced separately for display). See `docs/provenance.md` for the cited algorithms.
///
/// [`Self::build`] and the other `build*` methods keep the top walkable surface of each column only;
/// [`Self::build_layered`] keeps the walkable surfaces under it too (a tunnel under the ground, the
/// ground under a bridge, the floors of a building; ADR-8076).
#[derive(Default)]
pub struct NavMeshBuilder {
    /// The build parameters.
    pub params: NavMeshBuildParams,
}

impl NavMeshBuilder {
    /// A builder with the given parameters.
    pub fn new(params: NavMeshBuildParams) -> Self {
        Self { params }
    }

    /// [`Self::build_with_triangle_areas`] keeping up to `max_layers` walkable layers (ADR-8076).
    /// 1 (or 0) keeps the top walkable surface of each column only: the same mesh as
    /// `build_with_triangle_areas`. Above 1, walkable surfaces under the top one become polygons
    /// too, linked where an agent can step between them; a vertical wall through a lower
    /// surface's headroom closes it, so the inside of a closed box does not join the floor around
    /// it. Surfaces past the last layer are dropped. `triangle_areas` and `volumes` are as for
    /// `build_with_triangle_areas` (both may be empty), and so are the errors.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_nav::NavMeshBuilder;
    ///
    /// let quad = |y: f32| {
    ///     let h = 4.0;
    ///     [
    ///         [Vec3::new(-h, y, -h), Vec3::new(h, y, -h), Vec3::new(h, y, h)],
    ///         [Vec3::new(-h, y, -h), Vec3::new(h, y, h), Vec3::new(-h, y, h)],
    ///     ]
    /// };
    /// // Two floors 3 m apart.
    /// let floors: Vec<[Vec3; 3]> = quad(0.0).into_iter().chain(quad(3.0)).collect();
    /// let builder = NavMeshBuilder::default();
    /// let top_only = builder.build(&floors).unwrap();
    /// let layered = builder.build_layered(&floors, &[], &[], 4).unwrap();
    /// assert_eq!(top_only.layer_count(), 1);
    /// assert_eq!(layered.layer_count(), 2);
    /// ```
    pub fn build_layered(
        &self,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
        volumes: &[NavAreaVolume],
        max_layers: u16,
    ) -> Result<NavMesh, NavBuildError> {
        let checked = checked_volumes(volumes)?;
        let mut layers = self.build_layers_with_areas(triangles, triangle_areas, max_layers.max(1))?;
        layers.paint(&checked, self.params.agent.max_climb);
        Ok(NavMesh::from_layers(&layers, self.params.clone()))
    }

    /// [`Self::build_layered`] that refuses a column with more walkable levels than
    /// `max_nav_levels`. The count is the spans that pass the slope test, the headroom test and
    /// wall closure, before radius erosion. [`Self::build_layered`] still drops surfaces past its
    /// count; this method does not.
    ///
    /// A column over the cap is [`NavBuildError::InvalidInput`] with the reason
    /// `max_nav_levels exceeded: a column has more walkable levels than the setting`.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_nav::{NavBuildError, NavMeshBuilder};
    ///
    /// let quad = |y: f32| {
    ///     [
    ///         [Vec3::new(-2.0, y, -2.0), Vec3::new(2.0, y, -2.0), Vec3::new(2.0, y, 2.0)],
    ///         [Vec3::new(-2.0, y, -2.0), Vec3::new(2.0, y, 2.0), Vec3::new(-2.0, y, 2.0)],
    ///     ]
    /// };
    /// let floors: Vec<[Vec3; 3]> = quad(0.0).into_iter().chain(quad(3.0)).collect();
    /// let builder = NavMeshBuilder::default();
    /// assert!(builder.build_with_nav_level_cap(&floors, &[], &[], 2).is_ok());
    /// let err = builder.build_with_nav_level_cap(&floors, &[], &[], 1).unwrap_err();
    /// assert!(matches!(err, NavBuildError::InvalidInput(_)));
    /// ```
    pub fn build_with_nav_level_cap(
        &self,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
        volumes: &[NavAreaVolume],
        max_nav_levels: u16,
    ) -> Result<NavMesh, NavBuildError> {
        let checked = checked_volumes(volumes)?;
        let heightfield = self.rasterize_checked(triangles, triangle_areas)?;
        let _span = tracing::info_span!(
            "nav_build_level_cap",
            columns = heightfield.columns.len(),
            max_nav_levels
        )
        .entered();
        let walls = WallExtents::rasterize(triangles, &heightfield);
        let levels = heightfield.max_walkable_levels(&self.params.agent, &walls);
        if levels > max_nav_levels {
            return Err(NavBuildError::InvalidInput(
                "max_nav_levels exceeded: a column has more walkable levels than the setting",
            ));
        }
        let mut layers = if max_nav_levels <= 1 {
            let top = heightfield.walkable_grid(&self.params.agent);
            if top.cells.iter().all(Option::is_none) {
                return Err(NavBuildError::NoWalkableSurface);
            }
            WalkableLayers {
                top,
                lower: Vec::new(),
                links: Vec::new(),
            }
        } else {
            let layers = heightfield.walkable_layers(&self.params.agent, max_nav_levels, &walls);
            let empty = |g: &WalkableGrid| g.cells.iter().all(Option::is_none);
            if empty(&layers.top) && layers.lower.iter().all(empty) {
                return Err(NavBuildError::NoWalkableSurface);
            }
            layers
        };
        layers.paint(&checked, self.params.agent.max_climb);
        Ok(NavMesh::from_layers(&layers, self.params.clone()))
    }

    /// Builds a navmesh from `triangles` (world space). No triangles is
    /// [`NavBuildError::NoGeometry`]; a non-finite vertex or a non-finite, non-positive or too
    /// small voxel size is [`NavBuildError::InvalidInput`] with the reason.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_nav::{NavBuildError, NavMeshBuilder};
    ///
    /// let h = 3.0;
    /// let floor = [
    ///     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
    ///     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
    /// ];
    /// let builder = NavMeshBuilder::default();
    /// assert!(builder.build(&floor).is_ok());
    /// // Nothing to build from is an error, not an empty mesh.
    /// assert_eq!(builder.build(&[]).err(), Some(NavBuildError::NoGeometry));
    /// ```
    pub fn build(&self, triangles: &[[Vec3; 3]]) -> Result<NavMesh, NavBuildError> {
        let grid = self.build_grid(triangles)?;
        Ok(NavMesh::from_grid(&grid, self.params.clone()))
    }

    /// [`Self::build`] with `volumes` painted over the walkable surface, so polygons split along
    /// them and carry their area ids. Later volumes win. An invalid volume, or more than
    /// `MAX_NAV_AREA_VOLUMES`, is [`NavBuildError::InvalidInput`].
    pub fn build_with_areas(
        &self,
        triangles: &[[Vec3; 3]],
        volumes: &[NavAreaVolume],
    ) -> Result<NavMesh, NavBuildError> {
        if volumes.len() > MAX_NAV_AREA_VOLUMES {
            return Err(NavBuildError::InvalidInput("too many area volumes"));
        }
        let mut checked = Vec::with_capacity(volumes.len());
        for volume in volumes {
            checked.push(
                volume
                    .validated()
                    .map_err(|_| NavBuildError::InvalidInput("invalid area volume"))?,
            );
        }
        let mut grid = self.build_grid(triangles)?;
        paint_grid(&mut grid, &checked, self.params.agent.max_climb);
        Ok(NavMesh::from_grid(&grid, self.params.clone()))
    }

    /// [`Self::build_with_areas`] with an area id per input triangle (ADR-2420):
    /// `triangle_areas[i]` paints the walkable surface `triangles[i]` makes, so a road mesh or a
    /// mud decal brings its own area. `volumes` paint over that, later volumes winning. An
    /// empty `triangle_areas` is plain ground everywhere (the same mesh as
    /// [`Self::build_with_areas`]). Errors as there, plus [`NavBuildError::InvalidInput`] for a
    /// length that is neither 0 nor the triangle count, or an id past 63.
    pub fn build_with_triangle_areas(
        &self,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
        volumes: &[NavAreaVolume],
    ) -> Result<NavMesh, NavBuildError> {
        if volumes.len() > MAX_NAV_AREA_VOLUMES {
            return Err(NavBuildError::InvalidInput("too many area volumes"));
        }
        let mut checked = Vec::with_capacity(volumes.len());
        for volume in volumes {
            checked.push(
                volume
                    .validated()
                    .map_err(|_| NavBuildError::InvalidInput("invalid area volume"))?,
            );
        }
        let mut grid = self.build_grid_with_areas(triangles, triangle_areas)?;
        paint_grid(&mut grid, &checked, self.params.agent.max_climb);
        Ok(NavMesh::from_grid(&grid, self.params.clone()))
    }

    /// Validates the input and runs voxelisation and walkable marking, stopping before
    /// polygonisation (the tile cache keeps this grid and polygonises it per tile).
    pub(crate) fn build_grid(&self, triangles: &[[Vec3; 3]]) -> Result<WalkableGrid, NavBuildError> {
        self.build_grid_with_areas(triangles, &[])
    }

    /// [`Self::build_grid`] with an area id per triangle (empty: all area 0).
    pub(crate) fn build_grid_with_areas(
        &self,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
    ) -> Result<WalkableGrid, NavBuildError> {
        let heightfield = self.rasterize_checked(triangles, triangle_areas)?;
        let grid = heightfield.walkable_grid(&self.params.agent);
        if grid.cells.iter().all(Option::is_none) {
            return Err(NavBuildError::NoWalkableSurface);
        }
        Ok(grid)
    }

    /// [`Self::build_grid_with_areas`] keeping up to `max_layers` walkable layers (ADR-8076): the
    /// top grid plus the layers under it. The top grid is the one `build_grid_with_areas` returns;
    /// with one layer there is nothing else.
    fn build_layers_with_areas(
        &self,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
        max_layers: u16,
    ) -> Result<WalkableLayers, NavBuildError> {
        if max_layers <= 1 {
            let top = self.build_grid_with_areas(triangles, triangle_areas)?;
            return Ok(WalkableLayers {
                top,
                lower: Vec::new(),
                links: Vec::new(),
            });
        }
        let heightfield = self.rasterize_checked(triangles, triangle_areas)?;
        let _span = tracing::info_span!("nav_build_layers", columns = heightfield.columns.len()).entered();
        let walls = WallExtents::rasterize(triangles, &heightfield);
        let layers = heightfield.walkable_layers(&self.params.agent, max_layers, &walls);
        let empty = |g: &WalkableGrid| g.cells.iter().all(Option::is_none);
        if empty(&layers.top) && layers.lower.iter().all(empty) {
            return Err(NavBuildError::NoWalkableSurface);
        }
        Ok(layers)
    }

    /// Validates the input and rasterises it.
    fn rasterize_checked(
        &self,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
    ) -> Result<HeightField, NavBuildError> {
        check_triangle_areas(triangles.len(), triangle_areas)?;
        if triangles.is_empty() {
            return Err(NavBuildError::NoGeometry);
        }
        if !triangles.iter().flatten().all(|v| v.is_finite()) {
            return Err(NavBuildError::InvalidInput(
                "geometry contains non-finite vertices",
            ));
        }
        let (min, max) = bounds(triangles);
        validate(&self.params, min, max)?;
        Ok(HeightField::rasterize_with_areas(
            triangles,
            triangle_areas,
            &self.params,
            min,
            max,
        ))
    }
}

/// Validates area volumes for a build: at most `MAX_NAV_AREA_VOLUMES`, each valid.
fn checked_volumes(volumes: &[NavAreaVolume]) -> Result<Vec<NavAreaVolume>, NavBuildError> {
    if volumes.len() > MAX_NAV_AREA_VOLUMES {
        return Err(NavBuildError::InvalidInput("too many area volumes"));
    }
    volumes
        .iter()
        .map(|volume| {
            volume
                .validated()
                .map_err(|_| NavBuildError::InvalidInput("invalid area volume"))
        })
        .collect()
}

/// Checks a per-triangle area list: empty, or one id below 64 per triangle.
pub(crate) fn check_triangle_areas(triangles: usize, areas: &[u8]) -> Result<(), NavBuildError> {
    if !areas.is_empty() && areas.len() != triangles {
        return Err(NavBuildError::InvalidInput(
            "triangle_areas must be empty or hold one id per triangle",
        ));
    }
    if areas.iter().any(|&a| a as usize >= MAX_NAV_AREAS) {
        return Err(NavBuildError::InvalidInput("a triangle area id is past 63"));
    }
    Ok(())
}

/// The world bounds of the triangle soup.
fn bounds(triangles: &[[Vec3; 3]]) -> (Vec3, Vec3) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for tri in triangles {
        for v in tri {
            min = min.min(*v);
            max = max.max(*v);
        }
    }
    (min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_geometry_fails() {
        let builder = NavMeshBuilder::default();
        assert!(matches!(builder.build(&[]), Err(NavBuildError::NoGeometry)));
    }

    #[test]
    fn a_flat_floor_builds_a_mesh() {
        let c = 4.0;
        let triangles = vec![
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, -c),
                Vec3::new(c, 0.0, c),
            ],
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, c),
                Vec3::new(-c, 0.0, c),
            ],
        ];
        let builder = NavMeshBuilder::default();
        let mesh = builder.build(&triangles).expect("builds");
        assert!(mesh.poly_count() > 0);
        assert_eq!(mesh.region_count, 1);
    }

    fn floor_triangles() -> Vec<[Vec3; 3]> {
        vec![[
            Vec3::new(-4.0, 0.0, -4.0),
            Vec3::new(4.0, 0.0, -4.0),
            Vec3::new(4.0, 0.0, 4.0),
        ]]
    }

    #[test]
    fn a_zero_or_non_finite_cell_size_is_an_error_not_a_panic() {
        for cell_size in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let builder = NavMeshBuilder::new(NavMeshBuildParams {
                cell_size,
                ..Default::default()
            });
            assert!(
                matches!(
                    builder.build(&floor_triangles()).err(),
                    Some(NavBuildError::InvalidInput(_))
                ),
                "cell_size {cell_size}"
            );
        }
        let builder = NavMeshBuilder::new(NavMeshBuildParams {
            cell_height: 0.0,
            ..Default::default()
        });
        assert!(matches!(
            builder.build(&floor_triangles()).err(),
            Some(NavBuildError::InvalidInput(_))
        ));
    }

    #[test]
    fn non_finite_geometry_is_an_error() {
        let mut triangles = floor_triangles();
        triangles[0][1].y = f32::NAN;
        assert!(matches!(
            NavMeshBuilder::default().build(&triangles).err(),
            Some(NavBuildError::InvalidInput(_))
        ));
    }

    fn floor() -> Vec<[Vec3; 3]> {
        vec![
            [
                Vec3::new(-4.0, 0.0, -4.0),
                Vec3::new(4.0, 0.0, -4.0),
                Vec3::new(4.0, 0.0, 4.0),
            ],
            [
                Vec3::new(-4.0, 0.0, -4.0),
                Vec3::new(4.0, 0.0, 4.0),
                Vec3::new(-4.0, 0.0, 4.0),
            ],
        ]
    }

    fn build_with(edit: impl FnOnce(&mut NavMeshBuildParams)) -> Result<NavMesh, NavBuildError> {
        let mut params = NavMeshBuildParams::default();
        edit(&mut params);
        NavMeshBuilder::new(params).build(&floor())
    }

    #[test]
    fn bad_voxel_sizes_are_invalid_input_not_a_missing_surface() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, 1e-20] {
            assert!(
                matches!(
                    build_with(|p| p.cell_size = bad),
                    Err(NavBuildError::InvalidInput(_))
                ),
                "cell_size {bad}"
            );
        }
        for bad in [0.0, -0.5, f32::NAN] {
            assert!(
                matches!(
                    build_with(|p| p.cell_height = bad),
                    Err(NavBuildError::InvalidInput(_))
                ),
                "cell_height {bad}"
            );
        }
        for bad in [-100.0, f32::NAN, f32::INFINITY] {
            assert!(
                matches!(
                    build_with(|p| p.border_size = bad),
                    Err(NavBuildError::InvalidInput(_))
                ),
                "border_size {bad}"
            );
        }
    }

    #[test]
    fn non_finite_geometry_is_invalid_input() {
        let mut tris = floor();
        tris[0][0].x = f32::NAN;
        assert!(matches!(
            NavMeshBuilder::default().build(&tris),
            Err(NavBuildError::InvalidInput(_))
        ));
    }

    #[test]
    fn no_walkable_surface_fails() {
        // A vertical-only surface (a wall) has no walkable area.
        let triangles = vec![[
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, 3.0, -1.0),
            Vec3::new(0.0, 3.0, 1.0),
        ]];
        let builder = NavMeshBuilder::default();
        assert!(matches!(
            builder.build(&triangles),
            Err(NavBuildError::NoWalkableSurface)
        ));
    }

    #[test]
    fn every_value_the_agent_ranges_offer_passes_validation() {
        // The editor's reflect `@Range` hints (config.rs) clamp sliders and `types.describe`
        // bounds; every endpoint must be an agent the builder accepts (rev-claude-62: `height` 0
        // and `max_slope` 1.6 were offered and refused).
        for field in ["radius", "height", "max_climb", "max_slope"] {
            let range = regia_ecs::describe::field_attrs::<NavAgentParams>(field)
                .and_then(|a| a.range)
                .unwrap_or_else(|| panic!("{field} has a range"));
            for end in [*range.start(), *range.end()] {
                let mut agent = NavAgentParams::default();
                let slot = match field {
                    "radius" => &mut agent.radius,
                    "height" => &mut agent.height,
                    "max_climb" => &mut agent.max_climb,
                    _ => &mut agent.max_slope,
                };
                *slot = end as f32;
                assert!(
                    validate_agent(&agent).is_ok(),
                    "{field} = {end} is offered and refused"
                );
            }
        }
    }
}
