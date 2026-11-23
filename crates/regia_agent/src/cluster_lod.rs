// SPDX-License-Identifier: MPL-2.0
//! Vertex-cluster far LOD on the agent protocol (`book/agents/cluster-lod.md`, ADR-8464):
//! `render.cluster_lod` clusters the meshes of the pieces of one model on a shared grid
//! ([`regia_render::cluster_pieces`]) and, unless `dry_run`, appends the result to each piece as
//! the coarsest level of its [`MeshLodChain`].

use std::collections::HashSet;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Mat3, Vec3};
use regia_render::{
    ClusterPiece, CpuMesh, MeshLodChain, MeshLodLevel, MeshRenderer, SkinJoints, VertexClusterError,
    VertexClusterSettings, clustered_lab_vertices,
};
use regia_scene::GlobalTransform;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Pieces one call clusters at most.
pub(crate) const MAX_PIECES: usize = 256;
/// Largest triangle budget a call accepts.
pub(crate) const MAX_TARGET: u64 = 10_000_000;
/// Default projected size below which the far level draws.
const DEFAULT_SCREEN_SIZE: f32 = 0.05;

fn reason(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, reason, message, "see book/agents/cluster-lod.md")
}

fn params_of(params: &Value) -> Result<(Vec<Entity>, VertexClusterSettings, f32, bool), RpcError> {
    let Some(list) = crate::params::opt_array(params, "entities")? else {
        return Err(RpcError::invalid_params(
            "`entities` (the ids of a model's pieces, from scene.query_entities) is required",
        ));
    };
    if list.is_empty() || list.len() > MAX_PIECES {
        return Err(RpcError::invalid_params(format!(
            "`entities` must hold 1 to {MAX_PIECES} entity ids"
        )));
    }
    let mut entities = Vec::with_capacity(list.len());
    let mut seen = HashSet::new();
    for item in list {
        let bits = item
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`entities` must hold entity ids"))?;
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
        if !seen.insert(bits) {
            return Err(RpcError::invalid_params(format!("entity {bits} is listed twice")));
        }
        entities.push(entity);
    }
    let target = match crate::params::opt_u64(params, "target_triangles")? {
        None => VertexClusterSettings::default().target_triangles,
        Some(t) if (1..=MAX_TARGET).contains(&t) => t as usize,
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "`target_triangles` must be an integer 1 to {MAX_TARGET}"
            )));
        }
    };
    let screen_size = match crate::params::opt_f64(params, "screen_size")? {
        None => DEFAULT_SCREEN_SIZE,
        Some(s) if s.is_finite() && s > 0.0 && s <= 1.0 => s as f32,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`screen_size` must be a number in (0, 1]",
            ));
        }
    };
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    Ok((
        entities,
        VertexClusterSettings::with_target(target),
        screen_size,
        dry_run,
    ))
}

/// One piece read from the world: its mesh, the finest index list, and the map from its space
/// into the first piece's (identity when the pieces share a transform).
struct Source {
    entity: Entity,
    mesh: Arc<CpuMesh>,
    lod0: Arc<Vec<u32>>,
    levels: usize,
    to_first: Option<Affine3A>,
    /// The coarsest existing level's screen size must stay above the new switch size.
    coarsest_switch: Option<f32>,
}

fn read_sources(world: &World, entities: &[Entity]) -> Result<Vec<Source>, RpcError> {
    let first_world = entities
        .first()
        .and_then(|&e| world.get::<GlobalTransform>(e))
        .map_or(Affine3A::IDENTITY, |g| g.0);
    let first_inverse = first_world.inverse();
    let mut sources = Vec::with_capacity(entities.len());
    for &entity in entities {
        if world.get_entity(entity).is_err() {
            return Err(RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "use scene.query_entities (entities with a mesh)",
            ));
        }
        let Some(renderer) = world.get::<MeshRenderer>(entity) else {
            return Err(reason(
                protocol::INVALID_PARAMS,
                "cluster_lod_no_mesh",
                format!("entity {} has no MeshRenderer", entity.to_bits()),
            ));
        };
        let mesh = Arc::clone(&renderer.mesh);
        if mesh.vertices.is_empty() || mesh.indices.is_empty() {
            return Err(reason(
                protocol::INVALID_PARAMS,
                "cluster_lod_no_mesh",
                format!(
                    "entity {}'s mesh keeps no CPU vertices (a counted or empty mesh)",
                    entity.to_bits()
                ),
            ));
        }
        let skinned = world.get::<SkinJoints>(entity).is_some()
            || mesh.vertices.iter().any(|v| v.weights.iter().any(|&w| w != 0.0));
        if skinned
            || world
                .get::<regia_render::morph::MeshMorphTargets>(entity)
                .is_some()
        {
            return Err(reason(
                protocol::INVALID_PARAMS,
                "cluster_lod_deforming",
                format!(
                    "entity {}'s mesh is skinned or has morph targets; far levels are for rigid meshes",
                    entity.to_bits()
                ),
            ));
        }
        let chain = world.get::<MeshLodChain>(entity);
        let lod0 = chain
            .and_then(|c| c.levels.first())
            .map_or_else(|| Arc::new(mesh.indices.clone()), |l| Arc::clone(&l.indices));
        let levels = chain.map_or(1, |c| c.levels.len());
        let coarsest_switch = chain.and_then(|c| {
            let n = c.levels.len();
            (n >= 2).then(|| c.levels[n - 2].screen_size)
        });
        let here = world
            .get::<GlobalTransform>(entity)
            .map_or(Affine3A::IDENTITY, |g| g.0);
        let to_first = (here != first_world).then(|| first_inverse * here);
        sources.push(Source {
            entity,
            mesh,
            lod0,
            levels,
            to_first,
            coarsest_switch,
        });
    }
    Ok(sources)
}

/// One piece's positions and normals in the first piece's space.
type PieceArrays = (Vec<[f32; 3]>, Vec<[f32; 3]>);

fn cluster_error(e: &VertexClusterError) -> RpcError {
    reason(protocol::INVALID_PARAMS, "cluster_lod_bad_mesh", e.to_string())
}

/// `render.cluster_lod { entities, target_triangles?, screen_size?, dry_run? }`.
pub(crate) fn cluster_lod(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (entities, settings, screen_size, dry_run) = params_of(params)?;
    let sources = read_sources(world, &entities)?;
    for source in &sources {
        if let Some(switch) = source.coarsest_switch
            && screen_size >= switch
        {
            return Err(reason(
                protocol::INVALID_PARAMS,
                "cluster_lod_screen_size",
                format!(
                    "entity {}: screen_size {screen_size} must be below its coarsest level's switch size {switch}",
                    source.entity.to_bits()
                ),
            ));
        }
    }

    // Positions and normals in the first piece's space.
    let mut arrays: Vec<PieceArrays> = Vec::with_capacity(sources.len());
    for source in &sources {
        let m = source.to_first.unwrap_or(Affine3A::IDENTITY);
        let normal_m = Mat3::from(m.matrix3).inverse().transpose();
        arrays.push(
            source
                .mesh
                .vertices
                .iter()
                .map(|v| {
                    (
                        m.transform_point3(Vec3::from_array(v.position)).to_array(),
                        (normal_m * Vec3::from_array(v.normal))
                            .normalize_or_zero()
                            .to_array(),
                    )
                })
                .unzip(),
        );
    }
    let pieces: Vec<ClusterPiece<'_>> = sources
        .iter()
        .zip(&arrays)
        .map(|(source, (positions, normals))| ClusterPiece {
            positions,
            normals,
            indices: source.lod0.as_slice(),
        })
        .collect();
    let lod = regia_render::cluster_pieces(&pieces, &settings).map_err(|e| cluster_error(&e))?;

    let mut rows = Vec::with_capacity(sources.len());
    for (source, piece) in sources.iter().zip(&lod.pieces) {
        let mut far = clustered_lab_vertices(&source.mesh.vertices, piece);
        if let Some(m) = source.to_first {
            let back = m.inverse();
            let normal_back = Mat3::from(back.matrix3).inverse().transpose();
            for v in &mut far {
                v.position = back.transform_point3(Vec3::from_array(v.position)).to_array();
                v.normal = (normal_back * Vec3::from_array(v.normal))
                    .normalize_or_zero()
                    .to_array();
            }
        }
        let base = source.mesh.vertices.len() as u32;
        rows.push(json!({
            "entity": source.entity.to_bits(),
            "source_triangles": source.lod0.len() / 3,
            "triangles": piece.triangles(),
            "vertices_added": far.len(),
            "levels": source.levels + 1,
        }));
        if dry_run {
            continue;
        }
        let mut vertices = Vec::with_capacity(source.mesh.vertices.len() + far.len());
        vertices.extend_from_slice(&source.mesh.vertices);
        vertices.extend_from_slice(&far);
        let mesh = CpuMesh {
            vertices,
            indices: source.mesh.indices.clone(),
        };
        let far_indices: Vec<u32> = piece.indices.iter().map(|&i| i + base).collect();
        let mut chain = world
            .get::<MeshLodChain>(source.entity)
            .cloned()
            .unwrap_or_else(|| first_level_chain(&mesh, &source.lod0));
        if let Some(last) = chain.levels.last_mut() {
            last.screen_size = screen_size;
        }
        chain.levels.push(MeshLodLevel {
            indices: Arc::new(far_indices),
            screen_size: 0.0,
        });
        let mut entity = world.entity_mut(source.entity);
        if let Some(mut renderer) = entity.get_mut::<MeshRenderer>() {
            renderer.mesh = Arc::new(mesh);
        }
        entity.insert(chain);
    }
    Ok(json!({
        "dry_run": dry_run,
        "source_triangles": lod.source_triangles,
        "triangles": lod.triangles(),
        "cell_size": lod.cell_size,
        "max_error": lod.max_error,
        "passes": lod.passes,
        "screen_size": screen_size,
        "pieces": rows,
    }))
}

/// A chain whose only level is the mesh's own triangles, with its bounding sphere.
fn first_level_chain(mesh: &CpuMesh, lod0: &Arc<Vec<u32>>) -> MeshLodChain {
    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    for v in &mesh.vertices {
        lo = lo.min(Vec3::from_array(v.position));
        hi = hi.max(Vec3::from_array(v.position));
    }
    let center = (lo + hi) * 0.5;
    let radius = mesh
        .vertices
        .iter()
        .map(|v| Vec3::from_array(v.position).distance(center))
        .fold(0.0f32, f32::max);
    MeshLodChain {
        levels: vec![MeshLodLevel {
            indices: Arc::clone(lod0),
            screen_size: 0.0,
        }],
        bounds_center: center.to_array(),
        bounds_radius: radius,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMaterial, LabVertex};

    /// A cylinder band split in `n` pieces around, `seg` segments per piece.
    fn band_piece(i: u32, n: u32, seg: u32) -> CpuMesh {
        let mut mesh = CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        };
        let rows = 24;
        for r in 0..=rows {
            for s in 0..=seg {
                let a = std::f32::consts::TAU * (i * seg + s) as f32 / (n * seg) as f32;
                mesh.vertices.push(LabVertex::from_pos_normal_uv(
                    [a.cos(), r as f32 * 0.1, a.sin()],
                    [a.cos(), 0.0, a.sin()],
                    [s as f32, r as f32],
                ));
            }
        }
        for r in 0..rows {
            for s in 0..seg {
                let a = r * (seg + 1) + s;
                let b = a + seg + 1;
                mesh.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        mesh
    }

    fn spawn_pieces(world: &mut World, n: u32) -> Vec<Entity> {
        (0..n)
            .map(|i| {
                world
                    .spawn(MeshRenderer::new(
                        Arc::new(band_piece(i, n, 32)),
                        Arc::new(CpuMaterial::default()),
                    ))
                    .id()
            })
            .collect()
    }

    fn ids(entities: &[Entity]) -> Value {
        json!(entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>())
    }

    #[test]
    fn dry_run_reports_and_changes_nothing() -> Result<(), RpcError> {
        let mut world = World::new();
        let pieces = spawn_pieces(&mut world, 4);
        let out = cluster_lod(
            &mut world,
            &json!({ "entities": ids(&pieces), "target_triangles": 300, "dry_run": true }),
        )?;
        assert_eq!(out["source_triangles"], json!(4 * 24 * 32 * 2));
        assert!(
            out["triangles"].as_u64().is_some_and(|t| t > 0 && t <= 300),
            "{out}"
        );
        assert!(out["max_error"].as_f64().is_some_and(|e| e > 0.0));
        assert_eq!(out["pieces"].as_array().map(Vec::len), Some(4));
        for &piece in &pieces {
            assert!(world.get::<MeshLodChain>(piece).is_none());
        }
        Ok(())
    }

    #[test]
    fn apply_appends_a_far_level_to_every_piece() -> Result<(), RpcError> {
        let mut world = World::new();
        let pieces = spawn_pieces(&mut world, 4);
        let before = world
            .get::<MeshRenderer>(pieces[0])
            .map(|r| r.mesh.vertices.len());
        let out = cluster_lod(
            &mut world,
            &json!({ "entities": ids(&pieces), "target_triangles": 300, "screen_size": 0.1 }),
        )?;
        for (k, &piece) in pieces.iter().enumerate() {
            let chain = world
                .get::<MeshLodChain>(piece)
                .ok_or_else(|| RpcError::internal("no chain"))?;
            assert_eq!(chain.levels.len(), 2);
            assert_eq!(chain.levels[0].screen_size, 0.1);
            assert_eq!(chain.levels[1].screen_size, 0.0);
            let mesh = &world
                .get::<MeshRenderer>(piece)
                .ok_or_else(|| RpcError::internal("no renderer"))?
                .mesh;
            assert!(
                chain.levels[1]
                    .indices
                    .iter()
                    .all(|&i| (i as usize) < mesh.vertices.len())
            );
            assert_eq!(chain.levels[0].indices.as_slice(), mesh.indices.as_slice());
            assert_eq!(
                chain.levels[1].indices.len() / 3,
                out["pieces"][k]["triangles"].as_u64().unwrap_or(0) as usize
            );
            assert!(chain.bounds_radius > 1.0);
        }
        let after = world
            .get::<MeshRenderer>(pieces[0])
            .map(|r| r.mesh.vertices.len());
        assert!(after > before);
        // A second call needs a smaller switch size than the level it pushes past.
        let again = cluster_lod(
            &mut world,
            &json!({ "entities": ids(&pieces), "target_triangles": 100, "screen_size": 0.2 }),
        );
        assert_eq!(
            again.err().and_then(|e| e.data).map(|d| d["code"].clone()),
            Some(json!("cluster_lod_screen_size"))
        );
        cluster_lod(
            &mut world,
            &json!({ "entities": ids(&pieces), "target_triangles": 100, "screen_size": 0.02 }),
        )?;
        assert_eq!(
            world.get::<MeshLodChain>(pieces[0]).map(|c| c.levels.len()),
            Some(3)
        );
        Ok(())
    }

    #[test]
    fn refusals_carry_their_reason() {
        let code = |r: Result<Value, RpcError>| r.err().and_then(|e| e.data).map(|d| d["code"].clone());
        let mut world = World::new();
        let empty = world.spawn_empty().id().to_bits();
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        let counted = world
            .spawn(MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                }),
                Arc::new(CpuMaterial::default()),
            ))
            .id()
            .to_bits();
        let mut skinned_mesh = band_piece(0, 1, 8);
        for v in &mut skinned_mesh.vertices {
            v.weights = [1.0, 0.0, 0.0, 0.0];
        }
        let skinned = world
            .spawn(MeshRenderer::new(
                Arc::new(skinned_mesh),
                Arc::new(CpuMaterial::default()),
            ))
            .id()
            .to_bits();
        let mut nan_mesh = band_piece(0, 1, 8);
        nan_mesh.vertices[0].position[0] = f32::NAN;
        let nan = world
            .spawn(MeshRenderer::new(
                Arc::new(nan_mesh),
                Arc::new(CpuMaterial::default()),
            ))
            .id()
            .to_bits();
        assert_eq!(
            code(cluster_lod(&mut world, &json!({ "entities": [empty] }))),
            Some(json!("cluster_lod_no_mesh"))
        );
        assert_eq!(
            code(cluster_lod(&mut world, &json!({ "entities": [counted] }))),
            Some(json!("cluster_lod_no_mesh"))
        );
        assert_eq!(
            code(cluster_lod(&mut world, &json!({ "entities": [skinned] }))),
            Some(json!("cluster_lod_deforming"))
        );
        assert_eq!(
            code(cluster_lod(
                &mut world,
                &json!({ "entities": [nan], "target_triangles": 10 })
            )),
            Some(json!("cluster_lod_bad_mesh"))
        );
        assert_eq!(
            code(cluster_lod(&mut world, &json!({ "entities": [gone.to_bits()] }))),
            Some(json!("F0003"))
        );
        for bad in [
            json!({}),
            json!({ "entities": [] }),
            json!({ "entities": [counted, counted] }),
            json!({ "entities": [counted], "target_triangles": 0 }),
            json!({ "entities": [counted], "screen_size": 0.0 }),
            json!({ "entities": [counted], "screen_size": 2.0 }),
            json!({ "entities": [counted], "dry_run": 1 }),
        ] {
            assert_eq!(code(cluster_lod(&mut world, &bad)), Some(json!("F0002")), "{bad}");
        }
    }

    /// Pieces under different transforms are clustered in one space: a seam two pieces share in
    /// the world lands on the same world point in both.
    #[test]
    fn pieces_under_different_transforms_share_one_grid() -> Result<(), RpcError> {
        let mut world = World::new();
        let pieces = spawn_pieces(&mut world, 2);
        world
            .entity_mut(pieces[0])
            .insert(GlobalTransform(Affine3A::from_translation(Vec3::new(
                5.0, 0.0, 0.0,
            ))));
        world
            .entity_mut(pieces[1])
            .insert(GlobalTransform(Affine3A::from_translation(Vec3::new(
                5.0, 0.0, 0.0,
            ))));
        // Piece 1's local frame is moved, its world placement is the same.
        let shifted = world
            .get::<MeshRenderer>(pieces[1])
            .map(|r| {
                let mut vertices = r.mesh.vertices.clone();
                for v in &mut vertices {
                    v.position[1] -= 1.0;
                }
                CpuMesh {
                    vertices,
                    indices: r.mesh.indices.clone(),
                }
            })
            .ok_or_else(|| RpcError::internal("no renderer"))?;
        if let Some(mut r) = world.get_mut::<MeshRenderer>(pieces[1]) {
            r.mesh = Arc::new(shifted);
        }
        world
            .entity_mut(pieces[1])
            .insert(GlobalTransform(Affine3A::from_translation(Vec3::new(
                5.0, 1.0, 0.0,
            ))));
        cluster_lod(
            &mut world,
            &json!({ "entities": ids(&pieces), "target_triangles": 120 }),
        )?;
        let world_points = |k: usize| -> Vec<Vec3> {
            let Some(r) = world.get::<MeshRenderer>(pieces[k]) else {
                return Vec::new();
            };
            let offset = if k == 0 { [5.0, 0.0, 0.0] } else { [5.0, 1.0, 0.0] };
            let Some(chain) = world.get::<MeshLodChain>(pieces[k]) else {
                return Vec::new();
            };
            chain.levels[1]
                .indices
                .iter()
                .map(|&i| Vec3::from_array(r.mesh.vertices[i as usize].position) + Vec3::from_array(offset))
                .collect()
        };
        let (a, b) = (world_points(0), world_points(1));
        // The round trip through the inverse transform moves a point by float rounding only.
        let shared = a
            .iter()
            .filter(|p| b.iter().any(|q| p.distance(*q) < 1.0e-4))
            .count();
        assert!(shared > 4, "{shared} shared far-level points: the seam opened");
        Ok(())
    }

    /// The method round-trips through the protocol dispatcher (permission edit_scene).
    #[test]
    fn cluster_lod_round_trips_through_the_protocol() -> Result<(), RpcError> {
        let mut world = World::new();
        world.insert_resource(regia_app::Time::default());
        let pieces = spawn_pieces(&mut world, 3);
        let out = crate::dispatch::call_in_process(
            &mut world,
            "render.cluster_lod",
            json!({ "entities": ids(&pieces), "target_triangles": 200 }),
        )
        .ok_or_else(|| RpcError::internal("render.cluster_lod is not synchronous"))??;
        assert!(
            out["triangles"].as_u64().is_some_and(|t| t > 0 && t <= 200),
            "{out}"
        );
        for &piece in &pieces {
            assert_eq!(world.get::<MeshLodChain>(piece).map(|c| c.levels.len()), Some(2));
        }
        Ok(())
    }
}
