// SPDX-License-Identifier: MPL-2.0
//! Terrain scatter in the HLOD cook (ADR-3253, spec 78 § 11.6): the cook places a level's scatter
//! exactly as the runtime does (`regia_terrain::place_set_with_origin` under the set's
//! [`StreamScatter::placement_origin`], the deterministic per-cell placement),
//! poses each instance as the runtime's field poses it, gathers the instanced triangles per
//! streaming cell and voxelizes them per HLOD node with `voxel_far_cook`. A node's voxel proxy
//! stands in for the forest under it where the cut draws the node, with no instance data resident.
//!
//! The scatter runtime is not touched: this reads the cooked `.scatter.ron` set
//! ([`StreamScatter::from_cooked`]) and the terrain it stands on, as the host supplies them
//! ([`super::StreamAssets::scatter`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_terrain::scatter::ScatterSet;
use regia_terrain::scatter::placement::{
    DensityMaps, PlacedInstance, PlacementInputs, PlacementOrigin, place_set_with_origin,
};
use regia_terrain::{HolesMask, TerrainLayers, TerrainMap};

use crate::hlod_cook::MeshSoup;
use crate::voxel_far_cook::{VoxelCoverage, VoxelFarCookSettings, VoxelizeInput};

/// Default voxels along the longest axis of a node's scatter
/// ([`super::StreamAssets::scatter_voxel_resolution`]): 32 over a 128 m cell is 4 m voxels.
pub const HLOD_SCATTER_VOXEL_RESOLUTION: u32 = 32;

/// A terrain's scatter as the cook sees it: the set, the terrain it stands on and where that
/// terrain sits in the cook's world space.
#[derive(Clone, Debug)]
pub struct StreamScatter {
    /// The scatter set (the cooked `.scatter.ron`).
    pub set: ScatterSet,
    /// The heightfield the set is placed over (the terrain's source of truth).
    pub map: TerrainMap,
    /// Holes the classes avoid.
    pub holes: Option<HolesMask>,
    /// The layer stack for layer masks.
    pub layers: Option<TerrainLayers>,
    /// The density maps the set names, by path.
    pub density_maps: DensityMaps,
    /// World position of the terrain's local origin (the map's center), meters.
    pub origin: [f64; 3],
    /// The leaf material's alpha test, for alpha-tested leaf cards (ADR-3108, ADR-3227): the whole
    /// set is voxelized through it. `None`: opaque, unless another set of the level has one. A
    /// level has one alpha test for all its scatter (ADR-3253 known limit): the first set's that
    /// has one is applied to every set's triangles, opaque sets included.
    pub coverage: Option<ScatterCoverage>,
    /// Where the set's placement cell grids are anchored, in the map's local frame (Q-08,
    /// ADR-8496): the [`PlacementOrigin`] the runtime field draws the set with
    /// (`ScatterField::set_placement_origin`), so the voxel proxies stand where the drawn
    /// instances stand. [`PlacementOrigin::MapEdge`] (the default) is every earlier release's
    /// placement.
    pub placement_origin: PlacementOrigin,
}

impl StreamScatter {
    /// A scatter over `map` from a cooked `.scatter.ron` (`ScatterSet::to_cooked`), at the world
    /// origin, with no holes, layers, density maps or alpha test.
    ///
    /// # Errors
    ///
    /// The cooked set does not read or validate.
    pub fn from_cooked(bytes: &[u8], map: TerrainMap) -> Result<Self, regia_terrain::scatter::ScatterError> {
        Ok(Self {
            set: ScatterSet::from_cooked(bytes, None)?,
            map,
            holes: None,
            layers: None,
            density_maps: DensityMaps::new(),
            origin: [0.0; 3],
            coverage: None,
            placement_origin: PlacementOrigin::MapEdge,
        })
    }
}

/// An alpha test the voxelizer applies (`voxel_far_cook::VoxelCoverage`), owned.
#[derive(Clone, Debug, PartialEq)]
pub struct ScatterCoverage {
    /// Texture width in texels.
    pub width: u32,
    /// Texture height in texels.
    pub height: u32,
    /// Row-major alpha, `width * height` bytes.
    pub alpha: Vec<u8>,
    /// The base color factor's alpha.
    pub factor: f32,
    /// The alpha cutoff.
    pub cutoff: f32,
}

impl ScatterCoverage {
    fn view(&self) -> VoxelCoverage<'_> {
        VoxelCoverage {
            width: self.width,
            height: self.height,
            alpha: &self.alpha,
            factor: self.factor,
            cutoff: self.cutoff,
            vertex_alpha: false,
            uv_transform: crate::gltf_extensions::TextureTransform::IDENTITY,
        }
    }
}

/// Triangles in world space (`f64`, so a far cell keeps its precision until it is anchored).
#[derive(Clone, Debug, Default)]
pub(crate) struct WorldSoup {
    pub(crate) positions: Vec<[f64; 3]>,
    pub(crate) normals: Vec<[f32; 3]>,
    pub(crate) uvs: Vec<[f32; 2]>,
    pub(crate) colors: Vec<[f32; 4]>,
    pub(crate) indices: Vec<u32>,
}

impl WorldSoup {
    fn append(&mut self, other: &WorldSoup) {
        let base = self.positions.len() as u32;
        self.positions.extend_from_slice(&other.positions);
        self.normals.extend_from_slice(&other.normals);
        self.uvs.extend_from_slice(&other.uvs);
        self.colors.extend_from_slice(&other.colors);
        self.indices.extend(other.indices.iter().map(|i| i + base));
    }
}

/// One cell's scatter: its instanced triangles and their vertical range.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScatterCell {
    pub(crate) soup: WorldSoup,
    pub(crate) instances: u32,
    pub(crate) min_y: f64,
    pub(crate) max_y: f64,
}

/// Every cell's scatter, plus the alpha test (the first scatter's that has one).
#[derive(Clone, Debug, Default)]
pub(crate) struct ScatterCells {
    pub(crate) cells: BTreeMap<CellCoord, ScatterCell>,
    pub(crate) coverage: Option<ScatterCoverage>,
    pub(crate) resolution: u32,
    pub(crate) warnings: Vec<super::StreamWarning>,
}

impl ScatterCells {
    /// The cells' triangles merged, in cell order.
    pub(crate) fn merged<'a>(&self, cells: impl IntoIterator<Item = &'a CellCoord>) -> (WorldSoup, u32) {
        let mut out = WorldSoup::default();
        let mut instances = 0;
        for cell in cells {
            if let Some(c) = self.cells.get(cell) {
                out.append(&c.soup);
                instances += c.instances;
            }
        }
        (out, instances)
    }
}

/// The pose shape of a scatter mesh, as the runtime field measures it: height, the pivot-centered
/// footprint radius of its lowest fifth, and its bounding radius.
#[derive(Clone, Copy, Debug)]
struct Shape {
    height: f32,
    footprint: f32,
}

fn shape_of(mesh: &MeshSoup) -> Shape {
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in &mesh.positions {
        min_y = min_y.min(p[1]);
        max_y = max_y.max(p[1]);
    }
    let height = (max_y - min_y).max(1e-3);
    let centre_y = (min_y + max_y) * 0.5;
    let (mut radius, mut footprint) = (0.0f32, 0.0f32);
    for p in &mesh.positions {
        radius = radius.max((p[0] * p[0] + (p[1] - centre_y).powi(2) + p[2] * p[2]).sqrt());
        if p[1] <= min_y + 0.2 * height {
            footprint = footprint.max((p[0] * p[0] + p[2] * p[2]).sqrt());
        }
    }
    Shape {
        height,
        footprint: footprint.max(0.05 * radius.max(1e-3)),
    }
}

/// The model matrix the runtime field draws `p` with (`regia_terrain` scatter field: up-axis tilt,
/// yaw about it, uniform scale, and the pivot sunk by `sink` of the height plus the downhill gap a
/// tilted base leaves, capped at 0.6 of the height).
fn instance_model(p: &PlacedInstance, sink_share: f32, shape: Shape) -> Mat4 {
    let up = Vec3::from_array(p.up).normalize_or(Vec3::Y);
    let normal = Vec3::from_array(p.normal).normalize_or(Vec3::Y);
    let rotation = Quat::from_rotation_arc(Vec3::Y, up) * Quat::from_rotation_y(p.yaw);
    let cos = up.dot(normal).clamp(0.2, 1.0);
    let residual_tan = (1.0 - cos * cos).sqrt() / cos;
    let height = shape.height * p.scale;
    let sink = (sink_share * height + shape.footprint * p.scale * residual_tan).min(0.6 * height);
    let pivot = Vec3::from_array(p.position) - up * sink;
    Mat4::from_scale_rotation_translation(Vec3::splat(p.scale), rotation, pivot)
}

/// Places every scatter of `scatter`, poses the instances, and gathers their triangles by the
/// streaming cell their ground point falls in. A mesh the cook cannot read is skipped with a
/// `stream_hlod_scatter_mesh` warning; a set that does not place, or whose instances stand outside
/// the addressable cell range, with `stream_hlod_scatter_failed`.
pub(crate) fn gather(
    scatter: &[StreamScatter],
    resolution: u32,
    grid: &StreamGrid,
    mesh_of: &dyn Fn(&str) -> Option<MeshSoup>,
) -> ScatterCells {
    let _span = tracing::info_span!("hlod_scatter_gather", sets = scatter.len()).entered();
    let mut out = ScatterCells {
        resolution,
        ..ScatterCells::default()
    };
    // Meshes by path, read once; `None` when the cook cannot read one.
    let mut meshes: BTreeMap<String, Option<Arc<(MeshSoup, Shape)>>> = BTreeMap::new();
    for source in scatter {
        if out.coverage.is_none() {
            out.coverage.clone_from(&source.coverage);
        }
        let inputs = PlacementInputs {
            map: &source.map,
            holes: source.holes.as_ref(),
            layers: source.layers.as_ref(),
            density_maps: Some(&source.density_maps),
        };
        let placed = match place_set_with_origin(&source.set, inputs, &source.placement_origin) {
            Ok(placed) => placed,
            Err(error) => {
                out.warnings.push(super::StreamWarning {
                    code: "stream_hlod_scatter_failed",
                    message: format!("scatter set {}: {error}; it has no voxel proxy", source.set.seed),
                });
                continue;
            }
        };
        // Instances whose ground point no cell can hold (a mis-authored origin): reported below.
        let mut out_of_range = 0usize;
        for p in &placed {
            let Some(class) = source.set.classes.get(usize::from(p.class)) else {
                continue;
            };
            let Some(mesh_ref) = class.meshes.get(usize::from(p.mesh)) else {
                continue;
            };
            let entry = meshes.entry(mesh_ref.mesh.clone()).or_insert_with(|| {
                let soup = mesh_of(&mesh_ref.mesh).filter(|m| !m.is_empty())?;
                let shape = shape_of(&soup);
                Some(Arc::new((soup, shape)))
            });
            let Some(mesh) = entry.clone() else {
                continue;
            };
            let (soup, shape) = (&mesh.0, mesh.1);
            let model = instance_model(p, class.sink, shape);
            let ground = [
                source.origin[0] + f64::from(p.position[0]),
                source.origin[2] + f64::from(p.position[2]),
            ];
            let Ok(cell) = grid.cell_of(ground[0], ground[1]) else {
                out_of_range += 1;
                continue;
            };
            let target = out.cells.entry(cell).or_insert_with(|| ScatterCell {
                min_y: f64::INFINITY,
                max_y: f64::NEG_INFINITY,
                ..ScatterCell::default()
            });
            target.instances += 1;
            let tint: [f32; 3] = std::array::from_fn(|k| class.tint[k] * p.shade);
            let normal_matrix = regia_core::glam::Mat3::from_mat4(model).inverse().transpose();
            let base = target.soup.positions.len() as u32;
            for (i, position) in soup.positions.iter().enumerate() {
                let w = model.transform_point3(Vec3::from_array(*position));
                let world = [
                    source.origin[0] + f64::from(w.x),
                    source.origin[1] + f64::from(w.y),
                    source.origin[2] + f64::from(w.z),
                ];
                target.min_y = target.min_y.min(world[1]);
                target.max_y = target.max_y.max(world[1]);
                target.soup.positions.push(world);
                let n = soup.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
                target.soup.normals.push(
                    (normal_matrix * Vec3::from_array(n))
                        .normalize_or(Vec3::Y)
                        .to_array(),
                );
                target.soup.uvs.push(soup.uvs.get(i).copied().unwrap_or([0.0; 2]));
                let c = soup.colors.get(i).copied().unwrap_or([1.0; 4]);
                target
                    .soup
                    .colors
                    .push([c[0] * tint[0], c[1] * tint[1], c[2] * tint[2], c[3]]);
            }
            target.soup.indices.extend(soup.indices.iter().map(|i| i + base));
        }
        if out_of_range > 0 {
            out.warnings.push(super::StreamWarning {
                code: "stream_hlod_scatter_failed",
                message: format!(
                    "scatter set {}: {out_of_range} instances stand outside the cell range (origin {:?}); they \
                     have no voxel proxy",
                    source.set.seed, source.origin
                ),
            });
        }
    }
    for (path, mesh) in &meshes {
        if mesh.is_none() {
            out.warnings.push(super::StreamWarning {
                code: "stream_hlod_scatter_mesh",
                message: format!(
                    "scatter mesh `{path}` has no cooked triangles; its instances have no voxel proxy"
                ),
            });
        }
    }
    out
}

/// Voxelizes `soup` moved by `-offset` (ADR-3253): the alpha test when given, else opaque.
pub(crate) fn voxelize(
    path: &str,
    soup: &WorldSoup,
    offset: [f64; 3],
    coverage: Option<&ScatterCoverage>,
    resolution: u32,
) -> Result<regia_render::visbuffer::VoxelFarProxy, crate::voxel_far_cook::VoxelFarCookError> {
    let positions: Vec<[f32; 3]> = soup
        .positions
        .iter()
        .map(|p| std::array::from_fn(|a| (p[a] - offset[a]) as f32))
        .collect();
    let input = VoxelizeInput {
        positions: &positions,
        normals: &soup.normals,
        uvs: &soup.uvs,
        colors: &soup.colors,
        indices: &soup.indices,
    };
    let settings = VoxelFarCookSettings { resolution };
    let view = coverage.map(ScatterCoverage::view);
    crate::voxel_far_cook::voxelize_with_coverage(path, &input, view.as_ref(), &settings)
}

/// The cooked file of a node's voxel proxy: the standard cooked header with the `voxel-far` type
/// tag, then the proxy blob (`VoxelFarProxy::to_bytes`), as `regia cook` writes a mesh's.
pub(crate) fn cooked_voxel_file(
    id: regia_asset::AssetId,
    proxy: &regia_render::visbuffer::VoxelFarProxy,
) -> Result<Vec<u8>, String> {
    let payload = proxy.to_bytes();
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(regia_render::visbuffer::VOXEL_FAR_TYPE_PATH),
        asset_id: id,
        importer_version: regia_asset::pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    regia_asset::encode_cooked_file(&header, &payload).map_err(|e| e.to_string())
}
