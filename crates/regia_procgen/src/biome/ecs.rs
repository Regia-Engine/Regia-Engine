// SPDX-License-Identifier: MPL-2.0
//! Biomes in the world: the [`BiomeArea`] component (`biome-area` v1), the capture of areas,
//! exclusions and water for a run, and the grass multiplier hand-off (spec 65 §§ 4, 8).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_scene::components::{EntityGuid, Generated, Name, Tags};
use regia_scene::{GlobalTransform, Transform};

use crate::biome::area::{AreaOutline, BiomeAreaSnap, KeyImage, KeyTables};
use crate::biome::def::BiomeDef;
use crate::biome::water::{WaterLevel, WaterSnap};
use crate::biome::{BiomeWorld, EXCLUDE_TAG, ExclusionRegion};
use crate::data::{ShapeData, ShapeForm};
use crate::world::{
    ProcDependency, ProcGenSettings, ProcGenerator, ProcOutput, read_graph_text, world_transform,
};
use regia_scene::spawn::ReflectFormatVersion;

/// `format_version` of the saved [`BiomeArea`] component (`biome-area`).
pub const BIOME_AREA_FORMAT_VERSION: u32 = 1;

/// The shape fingerprint of the saved `biome-area` component (`cargo xtask check-formats`).
pub fn biome_area_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            TypeInfo::Enum(e) => e.iter().map(|v| v.name()).collect::<Vec<_>>().join("|"),
            _ => String::new(),
        }
    }
    format!(
        "BiomeArea({});AreaShape({})",
        names(BiomeArea::type_info()),
        names(AreaShape::type_info())
    )
}

/// `biome-area` migration sources (none: version 1 is the first).
pub fn biome_area_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Where a biome area applies.
#[derive(Clone, Debug, Default, PartialEq, Reflect)]
pub enum AreaShape {
    /// The entity's `Shape` when it has one, in world space: a box or sphere as that shape (a sphere
    /// uses the largest axis scale), a capsule or hull as its bounding box. With no `Shape`, the
    /// transform as a unit box (half extents `0.5 × scale`; ADR-1665).
    #[default]
    FromShape,
    /// A closed spline on the entity (spec 64; captured once `Spline` exists, ADR-1665).
    FromSpline,
    /// A biome map texture over an XZ rectangle; biomes match by their map key.
    Map {
        /// The key texture (CPU pixels from [`ProcBiomeMaps`]).
        texture: String,
        /// `[min_x, min_z, max_x, max_z]`.
        rect: [f32; 4],
    },
}

/// A level entity whose shape, spline or map says where a biome applies (spec 65 § 4).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
pub struct BiomeArea {
    /// The `.biome.ron` (`None` for map areas, which use `map_biomes`).
    pub biome: Option<String>,
    /// The outline.
    pub shape: AreaShape,
    /// Width of the soft edge inside the area, meters (default 8).
    pub blend_m: f32,
    /// `(scale_m, amplitude 0..1)` of the edge wobble.
    pub blend_noise: Option<(f32, f32)>,
    /// Added to the biome priority for this area only.
    pub priority_offset: i32,
    /// Editor: regenerate this area alone (no resolve with neighbors).
    pub preview_alone: bool,
    /// Map areas: candidate biomes, matched by map key.
    pub map_biomes: Vec<String>,
    /// Always [`BIOME_AREA_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for BiomeArea {
    const CURRENT: u32 = BIOME_AREA_FORMAT_VERSION;
}

impl Default for BiomeArea {
    fn default() -> Self {
        Self {
            biome: None,
            shape: AreaShape::FromShape,
            blend_m: 8.0,
            blend_noise: None,
            priority_offset: 0,
            preview_alone: false,
            map_biomes: Vec::new(),
            format_version: BIOME_AREA_FORMAT_VERSION,
        }
    }
}

impl BiomeArea {
    /// An area of one biome over the entity's shape.
    pub fn new(biome: impl Into<String>) -> Self {
        Self {
            biome: Some(biome.into()),
            ..Self::default()
        }
    }

    /// Upgrades an older saved shape (none exists yet).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > BIOME_AREA_FORMAT_VERSION {
            return Err(format!(
                "biome-area format_version {} is newer than {BIOME_AREA_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = BIOME_AREA_FORMAT_VERSION;
        Ok(())
    }
}

/// CPU key images of biome map textures by asset path (filled by the host, like
/// [`crate::draw::ProcMeshLibrary`]).
#[derive(Resource, Clone, Debug, Default)]
pub struct ProcBiomeMaps(pub BTreeMap<String, Arc<KeyImage>>);

/// CPU density maps of `.scatter.ron` classes by the path the set names (filled by the host, like
/// [`ProcBiomeMaps`]).
#[derive(Resource, Clone, Debug, Default)]
pub struct ProcDensityMaps(pub regia_terrain::scatter::placement::DensityMaps);

/// Why an area was left out of a capture (for `biome.areas` and the log).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AreaProblem {
    /// The area entity's guid.
    pub guid: Guid,
    /// Stable code (`biome_unknown`, `biome_invalid`, `biome_area_open`).
    pub code: String,
    /// Detail.
    pub message: String,
}

/// Loads a biome definition through the generator file source (overlay first).
///
/// # Errors
///
/// `(code, message)`: `biome_unknown` when the file is missing, else the definition's code.
pub fn load_def(world: &World, path: &str) -> Result<BiomeDef, (String, String)> {
    let text = read_graph_text(world, path)
        .ok_or_else(|| ("biome_unknown".to_string(), format!("no biome file `{path}`")))?;
    BiomeDef::from_ron(&text).map_err(|e| (e.code().to_string(), format!("`{path}`: {e}")))
}

fn unit_box(tf: &Transform) -> ShapeData {
    ShapeData {
        form: ShapeForm::Box {
            half_extents: (Vec3::splat(0.5) * tf.scale.abs()).to_array(),
        },
        position: tf.translation.to_array(),
        rotation: tf.rotation.to_array(),
        edge_m: 0.0,
        clip_xz: None,
    }
}

/// Reads the ecology profile beside the biome at `path` once (ADR-2456): a present file is a
/// dependency of the run; a missing one is not an error (the biome has no profile).
fn load_ecology(
    world: &World,
    path: &str,
    guid: Guid,
    ecology: &mut BTreeMap<String, Arc<crate::biome::ecology::EcologyProfile>>,
    deps: &mut Vec<ProcDependency>,
    problems: &mut Vec<AreaProblem>,
) {
    if ecology.contains_key(path) {
        return;
    }
    let eco_path = crate::biome::ecology::ecology_path_for(path);
    let Some(text) = read_graph_text(world, &eco_path) else {
        return;
    };
    deps.push(ProcDependency::File {
        path: eco_path.clone(),
    });
    match crate::biome::ecology::EcologyProfile::from_ron(&text) {
        Ok(p) => {
            ecology.insert(path.to_string(), Arc::new(p));
        }
        Err(e) => {
            tracing::warn!(target: "regia_procgen", "biome area {guid}: `{eco_path}`: {e}");
            problems.push(AreaProblem {
                guid,
                code: "ecology_invalid".into(),
                message: format!("`{eco_path}`: {e}"),
            });
        }
    }
}

/// One captured area: component, guid, name, world transform, world `Shape`, `(closed, XZ polyline)`.
type RawArea = (
    BiomeArea,
    Guid,
    String,
    Transform,
    Option<ShapeData>,
    Option<(bool, Vec<[f32; 2]>)>,
);

/// A world-space spline as an XZ polyline (0.25 m tolerance).
fn spline_xz(world_spline: &regia_scene::spline::Spline) -> Vec<[f32; 2]> {
    world_spline
        .to_polyline(0.25)
        .iter()
        .map(|p| [p.x, p.z])
        .collect()
}

/// XZ bounds of an exclusion region (corridors padded by their half width).
fn region_xz_bounds(r: &ExclusionRegion) -> [f32; 4] {
    let pts_bounds = |pts: &[[f32; 2]], pad: f32| {
        pts.iter().fold(
            [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY],
            |b, p| {
                [
                    b[0].min(p[0] - pad),
                    b[1].min(p[1] - pad),
                    b[2].max(p[0] + pad),
                    b[3].max(p[1] + pad),
                ]
            },
        )
    };
    match r {
        ExclusionRegion::Shape(s) => {
            let (lo, hi) = s.world_aabb();
            [lo[0], lo[2], hi[0], hi[2]]
        }
        ExclusionRegion::Polygon(pts) => pts_bounds(pts, 0.0),
        ExclusionRegion::Corridor { points, half_width } => pts_bounds(points, *half_width),
    }
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
}

/// Captures what biome nodes read over `rect` (spec 65 § 4): areas touching it (clipped by the
/// nodes), their definitions, exclusion regions and water. Records the dependencies.
pub fn capture_biomes(
    world: &mut World,
    rect: [f32; 4],
    deps: &mut Vec<ProcDependency>,
) -> (BiomeWorld, Vec<AreaProblem>) {
    capture_biomes_with(world, rect, deps, true)
}

/// [`capture_biomes`], optionally without the scatter placements (`ScatterClass` sources), which
/// only a run needs (the agent's area and claim queries skip them).
pub fn capture_biomes_with(
    world: &mut World,
    rect: [f32; 4],
    deps: &mut Vec<ProcDependency>,
    with_scatter: bool,
) -> (BiomeWorld, Vec<AreaProblem>) {
    let _span = tracing::info_span!("biome_capture").entered();
    let editor = world.get_resource::<ProcGenSettings>().is_some_and(|s| s.editor);
    let mut q = world.query_filtered::<(
        &BiomeArea,
        Option<&EntityGuid>,
        Option<&Name>,
        Option<&GlobalTransform>,
        Option<&Transform>,
        Has<ChildOf>,
        Option<&regia_scene::shape::Shape>,
        Option<&regia_scene::spline::Spline>,
    ), Without<Generated>>();
    let raw: Vec<RawArea> = q
        .iter(world)
        .map(|(a, g, n, gt, t, parent, shape, spline)| {
            let tf = world_transform(gt, t, parent);
            (
                a.clone(),
                g.map_or(Guid::NIL, |g| g.0),
                n.map(|n| n.as_str().to_string()).unwrap_or_default(),
                tf,
                shape.map(|s| crate::world::world_shape(s, &tf)),
                spline.map(|s| (s.closed, spline_xz(&crate::world::world_spline(s, &tf)))),
            )
        })
        .collect();
    let mut defs: BTreeMap<String, Arc<BiomeDef>> = BTreeMap::new();
    let mut problems = Vec::new();
    let mut load = |world: &World, path: &str, guid: Guid, problems: &mut Vec<AreaProblem>| {
        if let Some(d) = defs.get(path) {
            return Some(d.clone());
        }
        match load_def(world, path) {
            Ok(d) => {
                let d = Arc::new(d);
                defs.insert(path.to_string(), d.clone());
                Some(d)
            }
            Err((code, message)) => {
                tracing::warn!(target: "regia_procgen", "biome area {guid}: {message}");
                problems.push(AreaProblem { guid, code, message });
                None
            }
        }
    };
    let mut ecology: BTreeMap<String, Arc<crate::biome::ecology::EcologyProfile>> = BTreeMap::new();
    let mut areas = Vec::new();
    for (a, guid, name, tf, shape, spline) in raw {
        let outline = match &a.shape {
            AreaShape::FromShape => AreaOutline::Shape(shape.unwrap_or_else(|| unit_box(&tf))),
            AreaShape::FromSpline => match spline {
                Some((true, pts)) if pts.len() >= 3 => AreaOutline::Polygon(pts),
                Some(_) => {
                    problems.push(AreaProblem {
                        guid,
                        code: "biome_area_open".into(),
                        message: "a spline area needs a closed spline".into(),
                    });
                    continue;
                }
                None => {
                    problems.push(AreaProblem {
                        guid,
                        code: "biome_area_open".into(),
                        message: "the entity has no Spline".into(),
                    });
                    continue;
                }
            },
            AreaShape::Map { texture, rect } => {
                let image = world
                    .get_resource::<ProcBiomeMaps>()
                    .and_then(|m| m.0.get(texture).cloned());
                let Some(image) = image else {
                    problems.push(AreaProblem {
                        guid,
                        code: "biome_unknown".into(),
                        message: format!("no CPU pixels for biome map `{texture}`"),
                    });
                    continue;
                };
                deps.push(ProcDependency::File {
                    path: texture.clone(),
                });
                AreaOutline::Map {
                    rect: *rect,
                    tables: Arc::new(KeyTables::build(&image, &[])),
                    image,
                }
            }
        };
        if !overlaps(outline.xz_bounds(), rect) {
            continue;
        }
        let paths: Vec<String> = match (&a.shape, &a.biome) {
            (AreaShape::Map { .. }, _) => a.map_biomes.clone(),
            (_, Some(p)) => vec![p.clone()],
            (_, None) => {
                problems.push(AreaProblem {
                    guid,
                    code: "biome_unknown".into(),
                    message: "the area names no biome".into(),
                });
                continue;
            }
        };
        let mut biomes = Vec::new();
        for p in paths {
            deps.push(ProcDependency::File { path: p.clone() });
            if let Some(d) = load(world, &p, guid, &mut problems) {
                load_ecology(world, &p, guid, &mut ecology, deps, &mut problems);
                biomes.push((p, d));
            }
        }
        if biomes.is_empty() {
            continue;
        }
        let outline = match outline {
            AreaOutline::Map { rect, image, .. } => {
                let keys: Vec<[u8; 3]> = biomes.iter().map(|(_, d)| d.key()).collect();
                AreaOutline::Map {
                    rect,
                    tables: Arc::new(KeyTables::build(&image, &keys)),
                    image,
                }
            }
            o => o,
        };
        deps.push(ProcDependency::Entity { guid });
        areas.push(BiomeAreaSnap {
            guid,
            name,
            outline,
            blend_m: a.blend_m.max(0.0),
            blend_noise: a.blend_noise,
            priority_offset: a.priority_offset,
            preview_alone: a.preview_alone,
            biomes,
        });
    }
    if editor && areas.iter().any(|a| a.preview_alone) {
        areas.retain(|a| a.preview_alone);
    }
    let unknown_map_texels = areas
        .iter()
        .map(|a| match &a.outline {
            AreaOutline::Map { tables, .. } => tables.unknown_texels,
            _ => 0,
        })
        .sum();
    // Exclusion regions: tagged level entities — their Shape (else the transform as a unit box), a
    // closed Spline as a polygon, an open Spline as a corridor of its half width.
    let mut exclusions = Vec::new();
    let mut eq = world.query_filtered::<(
        &Tags,
        Option<&EntityGuid>,
        Option<&GlobalTransform>,
        Option<&Transform>,
        Has<ChildOf>,
        Option<&regia_scene::shape::Shape>,
        Option<&regia_scene::spline::Spline>,
    ), Without<Generated>>();
    let mut tagged: Vec<(Guid, ExclusionRegion)> = eq
        .iter(world)
        .filter(|(tags, ..)| tags.0.iter().any(|t| t.as_str() == EXCLUDE_TAG))
        .map(|(_, g, gt, t, parent, shape, spline)| {
            let tf = world_transform(gt, t, parent);
            let region = match (shape, spline) {
                (Some(s), _) => ExclusionRegion::Shape(crate::world::world_shape(s, &tf)),
                (None, Some(sp)) => {
                    let w = crate::world::world_spline(sp, &tf);
                    let pts = spline_xz(&w);
                    if sp.closed {
                        ExclusionRegion::Polygon(pts)
                    } else {
                        let half_width = w
                            .points
                            .iter()
                            .map(|p| p.scale.x.abs() * 0.5)
                            .fold(0.0f32, f32::max);
                        ExclusionRegion::Corridor {
                            points: pts,
                            half_width,
                        }
                    }
                }
                (None, None) => ExclusionRegion::Shape(unit_box(&tf)),
            };
            (g.map_or(Guid::NIL, |g| g.0), region)
        })
        .collect();
    tagged.sort_by_key(|(g, _)| *g);
    for (guid, region) in tagged {
        if overlaps(region_xz_bounds(&region), rect) {
            deps.push(ProcDependency::Entity { guid });
            exclusions.push(region);
        }
    }
    // Water: the terrain's shoreline plane and water bodies.
    let mut water = WaterSnap::default();
    if let Some(level) = primary_terrain(world)
        .and_then(|e| world.get::<regia_terrain::Terrain>(e))
        .and_then(|t| t.water.as_ref().map(|w| w.level))
    {
        water.levels.push(WaterLevel { level, rect: None });
    }
    let mut bodies: Vec<(Guid, regia_terrain::water_body::WaterShape)> = world
        .query::<(&regia_terrain::water_body::WaterBody, Option<&EntityGuid>)>()
        .iter(world)
        .map(|(b, g)| (g.map_or(Guid::NIL, |g| g.0), b.shape.clone()))
        .collect();
    bodies.sort_by_key(|(g, _)| *g);
    for (_, shape) in bodies {
        match shape {
            regia_terrain::water_body::WaterShape::Plane {
                center, half_extent, ..
            } => water.levels.push(WaterLevel {
                level: center[1],
                rect: Some([
                    center[0] - half_extent[0],
                    center[2] - half_extent[1],
                    center[0] + half_extent[0],
                    center[2] + half_extent[1],
                ]),
            }),
            regia_terrain::water_body::WaterShape::Ribbon { points, .. } => {
                let outline = ribbon_outline(&points);
                if outline.len() >= 3 {
                    water.ribbons.push(outline);
                }
            }
        }
    }
    let scatter = if with_scatter {
        capture_scatter(world, rect, &defs, deps, &mut problems)
    } else {
        BTreeMap::new()
    };
    let terrain_layers = primary_terrain(world)
        .and_then(|e| world.get::<regia_terrain::Terrain>(e))
        .map(|t| Arc::new(t.layers.clone()));
    let painted_layers = primary_terrain(world).and_then(|e| {
        let t = world.get::<regia_terrain::Terrain>(e)?;
        let p = world.get::<regia_terrain::TerrainPaint>(e)?;
        (!p.0.is_blank()).then_some(crate::biome::PaintedLayers::new(
            Arc::new(p.0.clone()),
            t.map.world_width(),
        ))
    });
    let mut out = BiomeWorld {
        areas,
        exclusions,
        water,
        defs,
        unknown_map_texels,
        scatter,
        terrain_layers,
        painted_layers,
        ecology,
    };
    out.sort();
    (out, problems)
}

/// The terrain the biome capture binds to: the lowest-guid `Terrain`, the same pick the ground
/// capture makes (`capture_inner`). ECS query order follows spawn and archetype history, which a
/// save/load cycle changes, so the water level, layers, paint, scatter and the grass hand-off
/// must all agree on one terrain by guid (audit: procgen determinism). `None`: no terrain entity
/// (the callers fall back to the `ProcGenTerrain` resource where one exists).
fn primary_terrain(world: &mut World) -> Option<Entity> {
    let mut terrains: Vec<(bool, Guid, Entity)> = world
        .query_filtered::<(Entity, Option<&EntityGuid>), With<regia_terrain::Terrain>>()
        .iter(world)
        .map(|(e, g)| (g.is_none(), g.map_or(Guid::NIL, |g| g.0), e))
        .collect();
    terrains.sort();
    terrains.first().map(|(_, _, e)| *e)
}

/// Places the `.scatter.ron` classes that `ScatterClass` sources name, over the cells that touch
/// `rect`, with the terrain's own scatter placement (same positions as the scatter set).
fn capture_scatter(
    world: &mut World,
    rect: [f32; 4],
    defs: &BTreeMap<String, Arc<BiomeDef>>,
    deps: &mut Vec<ProcDependency>,
    problems: &mut Vec<AreaProblem>,
) -> BTreeMap<(String, String), Arc<Vec<crate::biome::ScatterCandidate>>> {
    use crate::biome::def::CandidateSource;
    use regia_terrain::scatter::placement::{PlacementInputs, place_class_cell};
    let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for def in defs.values() {
        for l in &def.layers {
            if let CandidateSource::ScatterClass { set, class } = &l.source {
                let list = wanted.entry(set.clone()).or_default();
                if !list.contains(class) {
                    list.push(class.clone());
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    if wanted.is_empty() {
        return out;
    }
    let texts: Vec<(String, Option<String>, Vec<String>)> = wanted
        .into_iter()
        .map(|(set, classes)| (set.clone(), read_graph_text(world, &set), classes))
        .collect();
    let terrain = primary_terrain(world).and_then(|e| world.get::<regia_terrain::Terrain>(e));
    let fallback = world.get_resource::<crate::world::ProcGenTerrain>();
    let density_maps = world.get_resource::<ProcDensityMaps>();
    let (map, layers, holes) = match (terrain, fallback) {
        (Some(t), _) => (&t.map, Some(&t.layers), t.holes.as_ref()),
        (None, Some(f)) => (&f.0, None, None),
        (None, None) => return out,
    };
    for (set_path, text, classes) in texts {
        deps.push(ProcDependency::File {
            path: set_path.clone(),
        });
        let parsed = text
            .ok_or_else(|| format!("no scatter set `{set_path}`"))
            .and_then(|t| {
                regia_terrain::scatter::ScatterSet::from_ron(&t, layers).map_err(|e| e.to_string())
            });
        let set = match parsed {
            Ok(s) => s,
            Err(message) => {
                problems.push(AreaProblem {
                    guid: Guid::NIL,
                    code: "biome_unknown".into(),
                    message,
                });
                continue;
            }
        };
        let inputs = PlacementInputs {
            map,
            holes,
            layers,
            density_maps: density_maps.map(|m| &m.0),
        };
        let world_width = map.world_width();
        let half = world_width * 0.5;
        for class_name in classes {
            let Some((index, class)) = set.classes.iter().enumerate().find(|(_, c)| c.name == class_name)
            else {
                problems.push(AreaProblem {
                    guid: Guid::NIL,
                    code: "biome_unknown".into(),
                    message: format!("no class `{class_name}` in `{set_path}`"),
                });
                continue;
            };
            let density = match &class.density_map {
                None => None,
                Some(path) => {
                    deps.push(ProcDependency::File { path: path.clone() });
                    let Some(d) = density_maps.and_then(|m| m.0.get(path)) else {
                        problems.push(AreaProblem {
                            guid: Guid::NIL,
                            code: "biome_unknown".into(),
                            message: format!(
                                "class `{class_name}` of `{set_path}` needs density map `{path}`: no CPU pixels in ProcDensityMaps"
                            ),
                        });
                        continue;
                    };
                    Some(d)
                }
            };
            let cell = class.cell_size.max(1e-3);
            let cells = (world_width / cell).ceil() as i32;
            let cl = |v: f32| (((v + half) / cell).floor() as i32).clamp(0, (cells - 1).max(0));
            let mut placed = Vec::new();
            for cz in cl(rect[1])..=cl(rect[3]) {
                for cx in cl(rect[0])..=cl(rect[2]) {
                    place_class_cell(set.seed, index, class, density, inputs, cx, cz, &mut placed);
                }
            }
            let list = placed
                .iter()
                .map(|p| crate::biome::ScatterCandidate {
                    position: p.position,
                    up: p.up,
                    yaw: p.yaw,
                    scale: p.scale,
                    mesh: p.mesh,
                })
                .collect();
            out.insert((set_path.clone(), class_name), Arc::new(list));
        }
    }
    out
}

/// Marks every finished generator whose last run read `path` (a `.biome.ron`, a scatter set, a
/// map texture) stale, so live generators re-run (spec 65 § 7: a changed definition re-runs every
/// generator whose areas use it). Returns how many were marked. `biome.def_edit` calls it; a file
/// watcher would too.
pub fn mark_readers_stale(world: &mut World, path: &str) -> usize {
    let mut q = world.query::<&mut crate::world::ProcGenState>();
    let mut n = 0;
    for mut st in q.iter_mut(world) {
        let reads = st
            .deps
            .iter()
            .any(|d| matches!(d, ProcDependency::File { path: p } if p == path));
        if reads && !st.state.in_flight() && st.run_id != 0 {
            st.stale = true;
            n += 1;
        }
    }
    n
}

/// A ribbon's banks as a closed XZ polygon: the left bank downstream, then the right bank back.
fn ribbon_outline(points: &[regia_terrain::water_body::RibbonPoint]) -> Vec<[f32; 2]> {
    let n = points.len();
    if n < 2 {
        return Vec::new();
    }
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let a = points[i.saturating_sub(1)].position;
        let b = points[(i + 1).min(n - 1)].position;
        let (tx, tz) = (b[0] - a[0], b[2] - a[2]);
        let len = (tx * tx + tz * tz).sqrt().max(1e-6);
        let (nx, nz) = (-tz / len, tx / len);
        let p = points[i].position;
        let h = points[i].width * 0.5;
        left.push([p[0] + nx * h, p[2] + nz * h]);
        right.push([p[0] - nx * h, p[2] - nz * h]);
    }
    right.reverse();
    left.extend(right);
    left
}

/// The areas, exclusions and water a generator's run would read (for `biome.claim` and
/// `biome.areas`; without scatter placements).
pub fn biome_world_for(world: &mut World, generator: Entity) -> (BiomeWorld, Vec<AreaProblem>) {
    let bounds = crate::world::generator_bounds(world, generator);
    let (lo, hi) = bounds.world_aabb();
    let mut deps = Vec::new();
    capture_biomes_with(world, [lo[0], lo[2], hi[0], hi[2]], &mut deps, false)
}

/// The host's grass, for the biome re-bake (spec 65 § 8, ADR-7214). A host that draws a
/// `regia_terrain::GrassLayer` inserts this with the set it baked the layer's mask from and the
/// layer's handle; [`grass_rebake`] then re-bakes the mask from the primary terrain whenever
/// [`regia_terrain::grass::GrassMultipliers`] changes (a biome generator ran or was cleared) and
/// queues it on the handle, so biome grass multipliers reach the GPU grass. Without it nothing is
/// re-baked (the pre-ADR-7214 behavior).
#[derive(Resource, Clone)]
pub struct GrassRebake {
    /// The set the layer draws.
    pub set: regia_terrain::GrassSet,
    /// The layer's handle ([`regia_terrain::GrassLayer::handle`]).
    pub handle: regia_terrain::TerrainGrassHandle,
    /// Mask texel edge, meters (the one the layer's mask was baked with, so the side matches).
    pub texel_m: f32,
    /// Masks re-baked and queued.
    pub rebakes: u64,
    /// The last bake failure (a class naming a density map that [`Self::density_maps`] does not
    /// hold).
    pub last_error: Option<String>,
    /// The density maps the set's classes name, by the asset path their `.grass.ron` gives
    /// (`density_map`), loaded by the host as it did for the layer's first bake (ADR-7262). A class
    /// that names a map missing here fails the re-bake with [`Self::last_error`] and keeps the old
    /// mask.
    pub density_maps: regia_terrain::DensityMaps,
}

impl GrassRebake {
    /// A re-bake for `set` drawn through `handle`, with 1 m texels.
    #[must_use]
    pub fn new(set: regia_terrain::GrassSet, handle: regia_terrain::TerrainGrassHandle) -> Self {
        Self {
            set,
            handle,
            texel_m: 1.0,
            rebakes: 0,
            last_error: None,
            density_maps: regia_terrain::DensityMaps::new(),
        }
    }
}

/// The terrains [`grass_rebake`] picks the primary one from.
type RebakeTerrains<'w, 's> = Query<'w, 's, (&'static regia_terrain::Terrain, Option<&'static EntityGuid>)>;

/// Re-bakes the host's grass mask when the biome multipliers change (spec 65 § 8, ADR-7214):
/// runs after [`biome_grass_handoff`]; does nothing without a [`GrassRebake`] resource. A changed
/// `GrassRebake` (inserted again, or its set or density maps edited) re-bakes too. The bake
/// reads the primary terrain (lowest guid; else `ProcGenTerrain`, heights only) with its layers
/// and holes, and the current multipliers (none once the resource is removed), and is a pure
/// function of those inputs.
pub fn grass_rebake(
    multipliers: Option<Res<regia_terrain::grass::GrassMultipliers>>,
    rebake: Option<ResMut<GrassRebake>>,
    terrains: RebakeTerrains,
    fallback: Option<Res<crate::world::ProcGenTerrain>>,
    mut had_multipliers: Local<bool>,
) {
    let Some(mut rebake) = rebake else {
        return;
    };
    let present = multipliers.is_some();
    // `is_changed` covers a new, replaced or edited resource (new density maps, ADR-7262); the
    // system's own writes below (`rebakes`, `last_error`) are not seen by its next run.
    let changed = rebake.is_changed()
        || multipliers.as_ref().is_some_and(|m| m.is_changed())
        || (*had_multipliers && !present);
    *had_multipliers = present;
    if !changed {
        return;
    }
    let primary = terrains
        .iter()
        .min_by_key(|(_, g)| (g.is_none(), g.map_or(Guid::NIL, |g| g.0)))
        .map(|(t, _)| t);
    let inputs = match (primary, fallback.as_deref()) {
        (Some(t), _) => regia_terrain::GrassMaskInputs {
            layers: Some(&t.layers),
            holes: t.holes.as_ref(),
            texel_m: rebake.texel_m,
            multipliers: multipliers.as_deref(),
            density_maps: Some(&rebake.density_maps),
            ..regia_terrain::GrassMaskInputs::new(&t.map)
        },
        (None, Some(f)) => regia_terrain::GrassMaskInputs {
            texel_m: rebake.texel_m,
            multipliers: multipliers.as_deref(),
            density_maps: Some(&rebake.density_maps),
            ..regia_terrain::GrassMaskInputs::new(&f.0)
        },
        (None, None) => return,
    };
    let _span = tracing::info_span!("procgen.grass_rebake", classes = rebake.set.classes.len()).entered();
    match regia_terrain::GrassMask::build(&rebake.set, &inputs) {
        Ok(mask) => {
            rebake.handle.replace_mask(mask);
            rebake.rebakes += 1;
            rebake.last_error = None;
        }
        Err(e) => {
            tracing::warn!(target: "regia_procgen", code = "procgen_grass_rebake_failed", "grass re-bake failed: {e}");
            rebake.last_error = Some(e.to_string());
        }
    }
}

/// A generator publishing grass multipliers: priority, guid, output hash, output.
type GrassSource = (i32, Guid, Option<[u8; 32]>, Arc<crate::exec::RunOutput>);

/// What [`biome_grass_handoff`] last saw: per generator `(priority, guid, output hash)` and the
/// terrain width.
#[derive(Default)]
pub struct GrassHandoffSeen(Vec<(i32, Guid, Option<[u8; 32]>)>, u32);

/// Gathers each grass class's multiplier from the generators' `grass_multiplier/<class>` masks
/// into [`regia_terrain::grass::GrassMultipliers`] (spec 65 § 8): first by `dependency_priority`,
/// then guid; resampled nearest-texel onto a 1 m grid over the terrain map (outside a generator's
/// mask the multiplier is 1). Runs after the lifecycle and only recomputes when a generator's
/// published output changes (a run finished or was cleared); writes the resource only when the
/// result differs, so the grass re-bakes only then.
pub fn biome_grass_handoff(world: &mut World, mut seen: Local<GrassHandoffSeen>) {
    let Some(world_width) = primary_terrain(world)
        .and_then(|e| world.get::<regia_terrain::Terrain>(e))
        .map(|t| t.map.world_width())
        .or_else(|| {
            world
                .get_resource::<crate::world::ProcGenTerrain>()
                .map(|t| t.0.world_width())
        })
    else {
        return;
    };
    let mut gens: Vec<GrassSource> = world
        .query::<(
            &ProcGenerator,
            Option<&EntityGuid>,
            Option<&crate::world::ProcGenState>,
            &ProcOutput,
        )>()
        .iter(world)
        .filter(|(.., o)| o.0.masks.keys().any(|k| k.starts_with("grass_multiplier/")))
        .map(|(g, id, st, o)| {
            (
                g.dependency_priority,
                id.map_or(Guid::NIL, |i| i.0),
                st.and_then(|s| s.output_hash),
                o.0.clone(),
            )
        })
        .collect();
    gens.sort_by_key(|(p, g, ..)| (*p, *g));
    let signature: Vec<(i32, Guid, Option<[u8; 32]>)> =
        gens.iter().map(|(p, g, h, _)| (*p, *g, *h)).collect();
    if signature == seen.0 && world_width.to_bits() == seen.1 {
        return;
    }
    seen.0 = signature;
    seen.1 = world_width.to_bits();
    let _span = tracing::info_span!("biome_grass_handoff", generators = gens.len()).entered();
    let side = (world_width.ceil() as u32).clamp(2, regia_terrain::grass::mask::MAX_MASK_SIDE);
    let half = world_width * 0.5;
    let texel = world_width / side as f32;
    let mut by_class: BTreeMap<String, regia_terrain::grass::MultiplierImage> = BTreeMap::new();
    for (.., out) in &gens {
        for (name, mask) in &out.masks {
            let Some(class) = name.strip_prefix("grass_multiplier/") else {
                continue;
            };
            if by_class.contains_key(class) {
                continue;
            }
            let mut texels = Vec::with_capacity((side * side) as usize);
            for r in 0..side {
                let z = (r as f32 + 0.5) * texel - half;
                for c in 0..side {
                    let x = (c as f32 + 0.5) * texel - half;
                    let inside =
                        x >= mask.rect[0] && x <= mask.rect[2] && z >= mask.rect[1] && z <= mask.rect[3];
                    let v = if inside { mask.sample(x, z) } else { 1.0 };
                    texels.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
                }
            }
            by_class.insert(
                class.to_string(),
                regia_terrain::grass::MultiplierImage { side, texels },
            );
        }
    }
    let next = regia_terrain::grass::GrassMultipliers { by_class };
    let changed = match world.get_resource::<regia_terrain::grass::GrassMultipliers>() {
        Some(cur) => *cur != next,
        None => !next.by_class.is_empty(),
    };
    if changed {
        world.insert_resource(next);
    }
}
