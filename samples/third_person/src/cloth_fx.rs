// SPDX-License-Identifier: MIT-0
//! Cloth and soft body in the level: a cape on the player character and a squashy ball.
//!
//! - **Cape.** An 11 x 6 particle sheet ([`cape_asset`]) cooked in memory with the cooker's own
//!   [`ClothAsset::cook`]. Its top row is pinned (`reach` 0) to a one-joint "cape mount" skeleton
//!   whose joint rides on the character at shoulder height; the rest hangs and swings under the
//!   solver, collides with a capsule [`ClothBodyProxy`] on that joint, and is drawn by the section
//!   entity the glue publishes into. Spawned for every character by [`attach_cape`].
//! - **Soft ball.** A pressure-model octahedron ([`ball_asset`]) dropped at [`BALL_START`]; it lands
//!   on the ground and rests squashed. It is a free body (no pose).
//!
//! Both are agent-visible through `physics.cloth_inspect { target: { entity } }` on the entities
//! named `cape-<player>` and `soft-ball` (see `book/agents/third-person-demo.md`).

use std::sync::Arc;

use regia::prelude::*;
use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_framework::cloth::{ClothBodyProxy, ClothSim, ProxyShape, SoftSim};
use regia_physics::cloth::ClothAsset;
use regia_physics::cloth::asset::{ClothSource, SimMeshInput, SoftAsset, SoftSource};
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer};
use regia_scene::Transform;

/// Cape columns (particles across).
pub const CAPE_COLS: u32 = 11;
/// Cape rows (particles from the shoulders down).
pub const CAPE_ROWS: u32 = 6;
/// Spacing of the cape particles in metres.
const CAPE_SPACING: f32 = 0.05;
/// Row spacing down the back in metres.
const CAPE_DROP: f32 = 0.11;
/// Height of the cape mount above the capsule centre (the shoulders).
pub const SHOULDER_HEIGHT: f32 = 0.45;
/// How far behind the capsule centre the cape starts (the character faces -Z).
const BACK_OFFSET: f32 = 0.34;

/// Where the soft ball starts.
pub const BALL_START: Vec3 = Vec3::new(-2.0, 2.5, -7.0);
/// Radius of the soft ball in metres.
pub const BALL_RADIUS: f32 = 0.4;

/// Marks the entity carrying a player's [`ClothSim`] cape.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct Cape {
    /// The player index the cape belongs to.
    pub player: u8,
}

/// Marks the soft-body prop.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct SoftProp;

/// The cape's sim mesh in the mount joint's frame: the top row at the shoulders, the sheet
/// hanging down the back.
fn cape_mesh() -> SimMeshInput {
    let mut mesh = SimMeshInput::default();
    for r in 0..CAPE_ROWS {
        for c in 0..CAPE_COLS {
            mesh.positions.push(Vec3::new(
                (c as f32 - (CAPE_COLS - 1) as f32 * 0.5) * CAPE_SPACING,
                SHOULDER_HEIGHT - r as f32 * CAPE_DROP,
                BACK_OFFSET + r as f32 * 0.01,
            ));
            mesh.uvs.push([
                c as f32 / (CAPE_COLS - 1) as f32,
                r as f32 / (CAPE_ROWS - 1) as f32,
            ]);
            mesh.joints.push([0; 4]);
            mesh.weights.push([1.0, 0.0, 0.0, 0.0]);
        }
    }
    for r in 0..CAPE_ROWS - 1 {
        for c in 0..CAPE_COLS - 1 {
            let i = r * CAPE_COLS + c;
            mesh.triangles.push([i, i + 1, i + CAPE_COLS]);
            mesh.triangles.push([i + 1, i + CAPE_COLS + 1, i + CAPE_COLS]);
        }
    }
    mesh
}

/// The cooked cape: the source text below plus `cape_mesh`, taken through the cooked payload
/// the way a game gets a cooked asset.
///
/// # Errors
/// The cooker's error text when the source or the mesh is refused.
pub fn cape_asset() -> Result<Arc<ClothAsset>, String> {
    let entries: Vec<String> = (0..CAPE_COLS).map(|i| format!("({i}, 0.0)")).collect();
    let text = format!(
        r#"(format_version: 1, name: "cape", skeleton: Some("cape-mount"),
            sim_mesh: (source: "procedural", node: "Cape"),
            params: (substeps: 8, damping: 1.0),
            maps: {{ "reach": Sparse(default: 4.0, entries: [{}]) }})"#,
        entries.join(", ")
    );
    let source = ClothSource::from_ron_str(&text).map_err(|e| e.to_string())?;
    let asset = ClothAsset::cook(&source, &cape_mesh(), None, &[]).map_err(|e| e.to_string())?;
    let bytes = asset.to_bytes().map_err(|e| e.to_string())?;
    ClothAsset::from_bytes(&bytes)
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// The soft ball's cooked asset: an octahedron of [`BALL_RADIUS`] at [`BALL_START`] with a
/// pressure model.
///
/// # Errors
/// The cooker's error text when the source or the surface is refused.
pub fn ball_asset() -> Result<Arc<SoftAsset>, String> {
    let source = SoftSource::from_ron_str(
        r#"(format_version: 1, name: "ball", model: Pressure(pressure: 1.0, volume_compliance: 0.0),
            surface: (source: "procedural", node: "Ball"))"#,
    )
    .map_err(|e| e.to_string())?;
    let corners = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
    let mesh = SimMeshInput {
        positions: corners.iter().map(|c| BALL_START + *c * BALL_RADIUS).collect(),
        triangles: vec![
            [0, 2, 4],
            [2, 1, 4],
            [1, 3, 4],
            [3, 0, 4],
            [2, 0, 5],
            [1, 2, 5],
            [3, 1, 5],
            [0, 3, 5],
        ],
        ..SimMeshInput::default()
    };
    SoftAsset::cook(&source, &mesh, None)
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

fn section_entity(world: &mut World, color: [f32; 4]) -> Entity {
    let empty = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let material = Arc::new(CpuMaterial {
        base_color: color,
        double_sided: true,
        roughness: 0.9,
        ..CpuMaterial::default()
    });
    world
        .spawn((MeshRenderer::new(empty, material), Transform::IDENTITY))
        .id()
}

/// Hangs a cape on `character`: a one-joint mount skeleton parented to the character, a body
/// proxy on its joint, the cooked cape bound to it, and a section entity to draw into. The cape
/// and the section are children of `character` too: they despawn with it, and a respawned
/// character cannot leave an orphaned cape simulating at the world origin. Returns the cape
/// entity, or `None` when the asset does not cook (logged) or this player already has a cape.
pub fn attach_cape(world: &mut World, character: Entity, player: u8) -> Option<Entity> {
    let mut existing = world.query::<(Entity, &Cape)>();
    if existing.iter(world).any(|(_, cape)| cape.player == player) {
        return None;
    }
    let asset = match cape_asset() {
        Ok(asset) => asset,
        Err(err) => {
            tracing::warn!(target: "third_person", "cape: {err}");
            return None;
        }
    };
    let skeleton =
        match Skeleton::from_local_bind(vec!["cape-mount".into()], vec![None], vec![LocalBind::default()]) {
            Ok(skeleton) => Arc::new(skeleton),
            Err(err) => {
                tracing::warn!(target: "third_person", "cape skeleton: {err}");
                return None;
            }
        };
    let binding = spawn_skeleton(world, skeleton);
    let joint = *binding.joints.first()?;
    world.entity_mut(joint).insert((
        ChildOf(character),
        ClothBodyProxy {
            name: "torso".into(),
            shape: ProxyShape::Capsule {
                a: Vec3::new(0.0, -0.3, 0.0),
                b: Vec3::new(0.0, SHOULDER_HEIGHT - 0.1, 0.0),
                radius: crate::character::CAPSULE_RADIUS,
            },
            friction: 0.3,
        },
    ));
    let owner = world
        .spawn((Transform::IDENTITY, binding, ChildOf(character)))
        .id();
    let section = section_entity(world, [0.72, 0.1, 0.12, 1.0]);
    // The section mesh is published in the section's local space, so parenting it under the
    // character (identity local) draws the same world-space result while tying its lifetime
    // (and visibility) to the character.
    world.entity_mut(section).insert(ChildOf(character));
    let mut sim = ClothSim::new(asset);
    sim.skinned_mesh = Some(owner);
    sim.section = Some(section);
    let name = format!("cape-{player}");
    let cape = world
        .spawn((
            sim,
            Transform::IDENTITY,
            Cape { player },
            ChildOf(character),
            Name::new(name.clone()),
            regia_scene::components::Name::new(name),
        ))
        .id();
    Some(cape)
}

/// `Startup`: the soft ball prop.
pub fn setup(world: &mut World) {
    let asset = match ball_asset() {
        Ok(asset) => asset,
        Err(err) => {
            tracing::warn!(target: "third_person", "soft ball: {err}");
            return;
        }
    };
    let section = section_entity(world, [0.2, 0.55, 0.85, 1.0]);
    let mut sim = SoftSim::new(asset);
    sim.section = Some(section);
    world.spawn((
        sim,
        Transform::IDENTITY,
        SoftProp,
        Name::new("soft-ball"),
        regia_scene::components::Name::new("soft-ball"),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_cape_parents_everything_and_skips_a_second_cape() {
        let mut world = World::new();
        let character = world.spawn(Transform::IDENTITY).id();
        let first = attach_cape(&mut world, character, 0);
        assert!(first.is_some(), "the first cape attaches");
        let capes: Vec<Entity> = world
            .query_filtered::<Entity, With<Cape>>()
            .iter(&world)
            .collect();
        assert_eq!(capes.len(), 1, "exactly one cape");
        let cape = capes[0];
        assert_eq!(
            world.get::<ChildOf>(cape).map(ChildOf::parent),
            Some(character),
            "the cape is a child of the character, so it despawns with it"
        );
        let Some(section) = world.get::<ClothSim>(cape).and_then(|sim| sim.section) else {
            panic!("the cape has a render section");
        };
        assert_eq!(
            world.get::<ChildOf>(section).map(ChildOf::parent),
            Some(character),
            "the section is a child of the character too"
        );
        // A respawn through `spawn_player_character` attaches again: the existing cape wins and
        // no duplicate name is left behind.
        assert!(attach_cape(&mut world, character, 0).is_none());
        assert_eq!(
            world.query_filtered::<Entity, With<Cape>>().iter(&world).count(),
            1
        );
        // A different player still gets a cape.
        assert!(attach_cape(&mut world, character, 1).is_some());
    }
}
