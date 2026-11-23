// SPDX-License-Identifier: MIT-0
//! Arena meshes and materials: a shared set of procedural box meshes (floor, walls, crates, bot
//! bodies) so the sample renders without any art assets, plus the system that attaches a body
//! mesh to every pawn.

use regia_app::prelude::*;
use regia_render::{CpuMaterial, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::{GlobalTransform, Transform};

use crate::pawn::Pawn;

/// Shared procedural meshes and materials (uploaded once; the renderer caches by `Arc` identity).
#[derive(Resource)]
pub struct ArenaAssets {
    /// The floor box (24 × 24 m).
    pub floor_mesh: std::sync::Arc<CpuMesh>,
    /// A perimeter wall box.
    pub wall_mesh: std::sync::Arc<CpuMesh>,
    /// A crate (1 m cube).
    pub crate_mesh: std::sync::Arc<CpuMesh>,
    /// A pawn body box (~0.7 × 1.2 × 0.7).
    pub bot_mesh: std::sync::Arc<CpuMesh>,
    /// Neutral grey concrete material.
    pub floor_material: std::sync::Arc<CpuMaterial>,
    /// Wall material.
    pub wall_material: std::sync::Arc<CpuMaterial>,
    /// Crate material (warm wood).
    pub crate_material: std::sync::Arc<CpuMaterial>,
    /// Bot body material (red) and the local player (blue) — see [`ArenaAssets::bot_material`].
    pub bot_material: std::sync::Arc<CpuMaterial>,
    /// Player material (blue).
    pub player_material: std::sync::Arc<CpuMaterial>,
}

impl ArenaAssets {
    /// Builds every mesh and material.
    pub fn build() -> Self {
        let mesh = |half: [f32; 3]| {
            std::sync::Arc::new(CpuMesh {
                vertices: box_vertices(half),
                indices: BOX_INDICES.to_vec(),
            })
        };
        let material = |c: [f32; 4], roughness: f32| {
            std::sync::Arc::new(CpuMaterial {
                base_color: c,
                roughness,
                ..CpuMaterial::default()
            })
        };
        Self {
            floor_mesh: mesh([12.5, 0.5, 12.5]),
            wall_mesh: mesh([12.5, 1.5, 0.5]),
            crate_mesh: mesh([0.5, 0.5, 0.5]),
            bot_mesh: mesh([0.35, 0.6, 0.35]),
            floor_material: material([0.35, 0.38, 0.42, 1.0], 0.9),
            wall_material: material([0.5, 0.5, 0.55, 1.0], 0.8),
            crate_material: material([0.6, 0.42, 0.24, 1.0], 0.7),
            bot_material: material([0.8, 0.2, 0.15, 1.0], 0.6),
            player_material: material([0.2, 0.35, 0.8, 1.0], 0.6),
        }
    }

    /// The mesh + material for a pawn. Bots cycle through distinct colours so an overhead
    /// shot can tell them apart; the local player is blue.
    pub fn pawn_parts(
        &self,
        slot: u8,
        is_bot: bool,
    ) -> (std::sync::Arc<CpuMesh>, std::sync::Arc<CpuMaterial>) {
        if is_bot {
            (self.bot_mesh.clone(), bot_color_material(slot))
        } else {
            (self.bot_mesh.clone(), self.player_material.clone())
        }
    }
}

/// Distinct bot body colours (linear RGBA), indexed by slot.
const BOT_COLORS: [[f32; 4]; 8] = [
    [0.85, 0.18, 0.14, 1.0],
    [0.15, 0.72, 0.28, 1.0],
    [0.95, 0.78, 0.15, 1.0],
    [0.62, 0.22, 0.85, 1.0],
    [0.95, 0.45, 0.12, 1.0],
    [0.15, 0.75, 0.82, 1.0],
    [0.90, 0.35, 0.62, 1.0],
    [0.55, 0.85, 0.25, 1.0],
];

/// A material in one of the bot colours (slot 0 uses the first entry when it is a bot).
fn bot_color_material(slot: u8) -> std::sync::Arc<CpuMaterial> {
    let index = slot.saturating_sub(1) as usize % BOT_COLORS.len();
    std::sync::Arc::new(CpuMaterial {
        base_color: BOT_COLORS[index],
        roughness: 0.55,
        ..CpuMaterial::default()
    })
}

/// `Startup`: inserts the shared [`ArenaAssets`].
pub fn build_arena_assets(world: &mut World) {
    if world.get_resource::<ArenaAssets>().is_none() {
        world.insert_resource(ArenaAssets::build());
    }
}

/// `PostUpdate`: attaches a body mesh to every pawn that lacks one.
pub fn attach_pawn_meshes(
    mut commands: Commands,
    assets: Option<Res<ArenaAssets>>,
    pawns: Query<(Entity, &Pawn), Without<MeshRenderer>>,
) {
    let Some(assets) = assets else {
        return;
    };
    for (entity, pawn) in &pawns {
        let (mesh, material) = assets.pawn_parts(pawn.slot, pawn.is_bot);
        let mut renderer = MeshRenderer::new(mesh, material);
        if !pawn.is_bot {
            // The first-person camera masks this layer (see `camera::PLAYER_MESH_LAYER`).
            renderer.layer = crate::camera::PLAYER_MESH_LAYER;
        }
        commands.entity(entity).insert(renderer);
    }
}

/// Index list of a unit box: 6 faces × 2 triangles.
const BOX_INDICES: [u32; 36] = [
    0, 1, 2, 2, 3, 0, // +Z
    4, 6, 5, 6, 4, 7, // -Z
    8, 10, 9, 10, 8, 11, // +X
    12, 13, 14, 14, 15, 12, // -X
    16, 17, 18, 18, 19, 16, // +Y
    20, 22, 21, 22, 20, 23, // -Y
];

/// Builds a box mesh with half extents `half`, 4 vertices per face with per-face normals.
fn box_vertices(half: [f32; 3]) -> Vec<LabVertex> {
    let [hx, hy, hz] = half;
    // Each face: normal, 4 corners (counter-clockwise from outside).
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [0.0, 0.0, 1.0],
            [[-hx, -hy, hz], [hx, -hy, hz], [hx, hy, hz], [-hx, hy, hz]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[hx, -hy, -hz], [-hx, -hy, -hz], [-hx, hy, -hz], [hx, hy, -hz]],
        ),
        (
            [1.0, 0.0, 0.0],
            [[hx, -hy, hz], [hx, -hy, -hz], [hx, hy, -hz], [hx, hy, hz]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[-hx, -hy, -hz], [-hx, -hy, hz], [-hx, hy, hz], [-hx, hy, -hz]],
        ),
        (
            [0.0, 1.0, 0.0],
            [[-hx, hy, hz], [hx, hy, hz], [hx, hy, -hz], [-hx, hy, -hz]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[-hx, -hy, -hz], [hx, -hy, -hz], [hx, -hy, hz], [-hx, -hy, hz]],
        ),
    ];
    let mut out = Vec::with_capacity(24);
    for (normal, corners) in faces {
        for (i, corner) in corners.iter().enumerate() {
            let uv = match i {
                0 => [0.0, 0.0],
                1 => [1.0, 0.0],
                2 => [1.0, 1.0],
                _ => [0.0, 1.0],
            };
            out.push(LabVertex {
                position: *corner,
                normal,
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    out
}

/// Spawns a static box collider plus a matching visible mesh (floor and walls).
pub fn spawn_static_visible(
    world: &mut World,
    name: &str,
    position: regia_core::math::Vec3,
    half_extents: regia_core::math::Vec3,
    color: [f32; 4],
) -> Entity {
    let mesh = std::sync::Arc::new(CpuMesh {
        vertices: box_vertices([half_extents.x, half_extents.y, half_extents.z]),
        indices: BOX_INDICES.to_vec(),
    });
    let material = std::sync::Arc::new(CpuMaterial {
        base_color: color,
        roughness: 0.85,
        ..CpuMaterial::default()
    });
    let transform = Transform::from_translation(position);
    world
        .spawn((
            regia_scene::components::Name::new(name.to_string()),
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(mesh, material),
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box { half_extents },
                friction: 0.8,
                ..regia_physics::Collider::default()
            },
        ))
        .id()
}

/// Spawns a visible box with no collider (a flag pole, the hill pad): shots and pawns pass
/// through it. Team modes tag it with an objective id so the match moves it (`crate::modes`).
pub fn spawn_marker(
    world: &mut World,
    name: &str,
    position: regia_core::math::Vec3,
    half_extents: regia_core::math::Vec3,
    color: [f32; 4],
) -> Entity {
    let mesh = std::sync::Arc::new(CpuMesh {
        vertices: box_vertices([half_extents.x, half_extents.y, half_extents.z]),
        indices: BOX_INDICES.to_vec(),
    });
    let material = std::sync::Arc::new(CpuMaterial {
        base_color: color,
        roughness: 0.6,
        ..CpuMaterial::default()
    });
    spawn_static_mesh(world, name, position, mesh, material)
}

/// Spawns a static box with a [`MeshRenderer`] from the shared assets (floor/walls).
pub fn spawn_static_mesh(
    world: &mut World,
    name: &str,
    position: regia_core::math::Vec3,
    mesh: std::sync::Arc<CpuMesh>,
    material: std::sync::Arc<CpuMaterial>,
) -> Entity {
    let transform = Transform::from_translation(position);
    world
        .spawn((
            regia_scene::components::Name::new(name.to_string()),
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(mesh, material),
        ))
        .id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_mesh_has_24_vertices_and_36_indices() {
        let vertices = box_vertices([0.5, 0.5, 0.5]);
        assert_eq!(vertices.len(), 24);
        assert_eq!(BOX_INDICES.len(), 36);
        // Every index is in range.
        assert!(BOX_INDICES.iter().all(|&i| (i as usize) < 24));
    }

    #[test]
    fn assets_have_distinct_bot_and_player_materials() {
        let assets = ArenaAssets::build();
        let (bot_mesh, bot_mat) = assets.pawn_parts(1, true);
        let (player_mesh, player_mat) = assets.pawn_parts(0, false);
        let (_, other_bot) = assets.pawn_parts(2, true);
        assert!(std::sync::Arc::ptr_eq(&bot_mesh, &player_mesh));
        assert_ne!(bot_mat.base_color, player_mat.base_color);
        assert_ne!(bot_mat.base_color, other_bot.base_color);
    }
}
