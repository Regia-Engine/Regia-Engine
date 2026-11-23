// SPDX-License-Identifier: MIT-0
//! Presentation physics next to an authoritative sim (`book/systems/sim-bridge.md`, "Presentation
//! physics").
//!
//! The engine API is `regia_framework::sim_bridge::presentation` (ADR-8356). This sample keeps the
//! windowed demo, the sample terrain constants and the cube mesh, and calls that plugin. The
//! integration promises live in `crates/regia_framework/tests/presentation_physics.rs`. The window
//! shows the demo with `--presentation-physics` ([`PresentationPhysicsDemo`]).

use std::collections::HashSet;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::sim_bridge::SimEntityMap;
use regia_physics::{ChunkDesc, Collider, ColliderShape, DebrisTag};
use regia_render::{CpuMaterial, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::Transform;
use regia_terrain::{HoleEllipse, HolesMask, TerrainMap};

pub use regia_framework::sim_bridge::presentation::{
    DebrisBurst, HullShapes, PresentationGround, PresentationPhysicsPlugin, PresentationPhysicsStats,
    cube_chunk,
};

/// Samples per side of [`PresentationTerrain::sample`] (1 m apart).
pub const SAMPLE_TERRAIN_SAMPLES: u32 = 33;
/// World width of [`PresentationTerrain::sample`], metres (centered on the origin).
pub const SAMPLE_TERRAIN_WIDTH_M: f32 = 32.0;
/// Center `(x, z)` of the sample terrain's one hole. Off the X = Z diagonal on purpose: a collider
/// built from the map without the transpose would put the hole at `(z, x)` instead.
pub const SAMPLE_HOLE_CENTER: [f32; 2] = [8.0, 0.0];
/// Radius of the sample terrain's hole, metres.
pub const SAMPLE_HOLE_RADIUS_M: f32 = 2.5;

// ---------------------------------------------------------------------------
// Terrain: one map and its holes for both the draw and the collider
// ---------------------------------------------------------------------------

/// The terrain the presentation draws and collides with: one height map and its holes. Insert it
/// as a resource before [`PresentationPhysicsDemo`]'s `Startup` runs. The demo also inserts the
/// engine [`PresentationGround`] built from the same map, and the plugin spawns that collider.
#[derive(Resource, Debug)]
pub struct PresentationTerrain {
    /// Heights, the same map the ground collider comes from.
    pub map: TerrainMap,
    /// Tunnel mouths and craters cut out of the ground (draw and collider).
    pub holes: Option<HolesMask>,
}

impl PresentationTerrain {
    /// The sample's terrain: [`SAMPLE_TERRAIN_WIDTH_M`] square, flat at y = 0, with one hole of
    /// [`SAMPLE_HOLE_RADIUS_M`] at [`SAMPLE_HOLE_CENTER`].
    ///
    /// # Errors
    ///
    /// A message if the map or the hole fails validation (it does not, for these constants).
    pub fn sample() -> Result<Self, String> {
        let n = SAMPLE_TERRAIN_SAMPLES;
        let heights = vec![0.0; (n * n) as usize];
        let map = TerrainMap::from_heights(n, n, heights, SAMPLE_TERRAIN_WIDTH_M, 1.0, 0.0)
            .map_err(|e| format!("sample terrain map: {e}"))?;
        let holes = HolesMask::new(vec![HoleEllipse {
            center: SAMPLE_HOLE_CENTER,
            radius: [SAMPLE_HOLE_RADIUS_M; 2],
            rotation_radians: 0.0,
        }])
        .map_err(|e| format!("sample terrain hole: {e}"))?;
        Ok(Self {
            map,
            holes: Some(holes),
        })
    }

    /// The center of the first elliptical hole, if the terrain has one.
    #[must_use]
    pub fn hole_center(&self) -> Option<[f32; 2]> {
        self.holes
            .as_ref()
            .and_then(|m| m.holes.first())
            .map(|h| h.center)
    }

    /// The engine ground: the heightfield collider and the hole cells the draw skips.
    #[must_use]
    pub fn ground(&self) -> PresentationGround {
        PresentationGround::from_terrain(&self.map, self.holes.as_ref())
    }

    /// The static heightfield collider of this terrain, holes cut. Place it at the world origin
    /// (the map is centered there).
    #[must_use]
    pub fn collider(&self) -> Collider {
        self.ground().collider
    }

    /// A draw mesh of the same field the collider uses. Cells listed in
    /// [`PresentationGround::hole_cells`] are left out, and each remaining cell splits along the
    /// collider's diagonal from `(x0, z1)` to `(x1, z0)`.
    #[must_use]
    pub fn mesh(&self) -> (Vec<LabVertex>, Vec<u32>) {
        let ground = self.ground();
        let rows = self.map.height() as usize;
        let cols = self.map.width() as usize;
        let heights = self.map.heights();
        let mut vertices = Vec::with_capacity(rows * cols);
        for row in 0..rows {
            for col in 0..cols {
                let [x, z] = self.map.sample_to_world(col as u32, row as u32);
                let height = heights.get(row * cols + col).copied().unwrap_or(0.0);
                vertices.push(LabVertex {
                    position: [x, height, z],
                    normal: self.map.normal_at(x, z),
                    uv: [
                        col as f32 / (cols - 1).max(1) as f32,
                        row as f32 / (rows - 1).max(1) as f32,
                    ],
                    tangent: [1.0, 0.0, 0.0, 1.0],
                    color: [1.0; 4],
                    joints: [0; 4],
                    weights: [0.0; 4],
                });
            }
        }
        let skipped: HashSet<(u32, u32)> = ground.hole_cells.iter().copied().collect();
        let mut indices = Vec::new();
        for row in 0..rows.saturating_sub(1) {
            for col in 0..cols.saturating_sub(1) {
                if skipped.contains(&(row as u32, col as u32)) {
                    continue;
                }
                let i = (row * cols + col) as u32;
                let below = i + cols as u32; // (x0, z1)
                indices.extend_from_slice(&[i, below, i + 1, i + 1, below, below + 1]);
            }
        }
        (vertices, indices)
    }
}

/// `count` cube chunks on a ring of `radius` around the burst origin (a simple explosion).
#[must_use]
pub fn ring_burst(count: u32, radius: f32, half: f32, mass: f32) -> Vec<ChunkDesc> {
    (0..count)
        .map(|i| {
            let angle = i as f32 / count.max(1) as f32 * std::f32::consts::TAU;
            cube_chunk(
                Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius),
                half,
                mass,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The windowed demo (`--presentation-physics`)
// ---------------------------------------------------------------------------

/// Half extents of the hover discs' hulls in the demo (the disc mesh is 0.5 m in radius).
pub const DISC_HULL_HALF: Vec3 = Vec3::new(0.5, 0.05, 0.5);
/// Half size of the demo's debris cubes.
pub const DEMO_CHUNK_HALF: f32 = 0.12;
/// Fixed steps between the demo's debris bursts (3 s at 60 Hz).
pub const DEMO_BURST_EVERY_STEPS: u64 = 180;

/// The windowed sample's opt-in demo (`sim_bridge --presentation-physics`): the sample terrain
/// with its hole (drawn and collided), kinematic hulls on the hover discs, and a burst of debris
/// over disc 2 every [`DEMO_BURST_EVERY_STEPS`] fixed steps. Without the flag the sample is
/// unchanged.
pub struct PresentationPhysicsDemo;

impl Plugin for PresentationPhysicsDemo {
    fn build(&self, app: &mut App) {
        match PresentationTerrain::sample() {
            Ok(terrain) => {
                app.insert_resource(terrain.ground());
                app.insert_resource(terrain);
            }
            Err(e) => tracing::warn!("presentation physics demo: {e}; no terrain collider"),
        }
        app.add_plugins((
            regia_physics::PhysicsPlugin,
            PresentationPhysicsPlugin::new(HullShapes::single(
                "disc",
                ColliderShape::Box {
                    half_extents: DISC_HULL_HALF,
                },
            )),
        ))
        .init_resource::<DemoBurstClock>()
        .add_systems(FixedUpdate, demo_bursts)
        .add_systems(PostUpdate, attach_debris_meshes);
    }
}

/// Fixed steps since the demo's last burst.
#[derive(Resource, Debug, Default)]
pub struct DemoBurstClock(pub u64);

fn demo_bursts(
    mut clock: ResMut<DemoBurstClock>,
    map: Res<SimEntityMap>,
    transforms: Query<&Transform>,
    mut bursts: MessageWriter<DebrisBurst>,
) {
    clock.0 += 1;
    if clock.0 < DEMO_BURST_EVERY_STEPS {
        return;
    }
    clock.0 = 0;
    let Some(disc) = map.entity(2).and_then(|e| transforms.get(e).ok()) else {
        return;
    };
    bursts.write(DebrisBurst {
        origin: disc.translation + Vec3::new(0.0, 1.5, 0.0),
        chunks: ring_burst(8, 0.3, DEMO_CHUNK_HALF, 2.0),
        speed: 1.5,
    });
}

/// The demo's shared debris cube mesh and material.
#[derive(Resource)]
struct DebrisDraw {
    mesh: Arc<CpuMesh>,
    material: Arc<CpuMaterial>,
}

fn attach_debris_meshes(
    mut commands: Commands,
    draw: Option<Res<DebrisDraw>>,
    chunks: Query<Entity, (With<DebrisTag>, Without<MeshRenderer>)>,
) {
    let (mesh, material) = match draw {
        Some(draw) => (Arc::clone(&draw.mesh), Arc::clone(&draw.material)),
        None => {
            let (vertices, indices) = build_cube_mesh(DEMO_CHUNK_HALF);
            let mesh = Arc::new(CpuMesh { vertices, indices });
            let material = Arc::new(CpuMaterial {
                base_color: [0.35, 0.33, 0.3, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            });
            commands.insert_resource(DebrisDraw {
                mesh: Arc::clone(&mesh),
                material: Arc::clone(&material),
            });
            (mesh, material)
        }
    };
    for entity in &chunks {
        commands
            .entity(entity)
            .insert(MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)));
    }
}

/// A cube of half size `half` centered at the origin, flat-shaded (four vertices per face).
#[must_use]
pub fn build_cube_mesh(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    // (normal, tangent) per face; the face's corners are normal ± tangent ± bitangent.
    const FACES: [([f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, tangent) in FACES {
        let n = Vec3::from(normal);
        let t = Vec3::from(tangent);
        let b = n.cross(t);
        let base = vertices.len() as u32;
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            let p = (n + t * (u * 2.0 - 1.0) + b * (v * 2.0 - 1.0)) * half;
            vertices.push(LabVertex {
                position: p.to_array(),
                normal,
                uv: [u, v],
                tangent: [tangent[0], tangent[1], tangent[2], 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_mesh_leaves_out_exactly_the_cells_the_collider_removes() {
        let terrain = PresentationTerrain::sample().expect("sample terrain");
        let (vertices, indices) = terrain.mesh();
        let n = SAMPLE_TERRAIN_SAMPLES as usize;
        assert_eq!(vertices.len(), n * n);
        let full_cells = (n - 1) * (n - 1);
        let drawn_cells = indices.len() / 6;
        assert!(
            drawn_cells < full_cells,
            "the hole removes cells: {drawn_cells} of {full_cells}"
        );
        // No drawn triangle has a vertex inside the hole.
        for tri in indices.as_chunks::<3>().0 {
            for &i in tri {
                let [x, _, z] = vertices[i as usize].position;
                let holes = terrain.holes.as_ref().expect("holes");
                assert!(
                    !holes.contains(x, z),
                    "drawn vertex ({x}, {z}) is inside the hole"
                );
            }
        }
    }

    #[test]
    fn the_collider_is_the_maps_collision_heightfield() {
        let terrain = PresentationTerrain::sample().expect("sample terrain");
        let field = terrain.map.collision_heightfield(terrain.holes.as_ref());
        assert!(field.cut_samples > 0, "the hole cuts samples");
        match terrain.collider().shape {
            ColliderShape::HeightField {
                nx,
                nz,
                heights,
                scale,
            } => {
                assert_eq!((nx, nz), (field.nx, field.nz));
                assert_eq!(scale, Vec3::from(field.scale));
                assert_eq!(heights.len(), field.heights.len());
                for (a, b) in heights.iter().zip(&field.heights) {
                    assert_eq!(a.to_bits(), b.to_bits());
                }
            }
            other => panic!("expected a heightfield, got {other:?}"),
        }
    }

    #[test]
    fn a_cube_mesh_has_six_outward_faces() {
        let (vertices, indices) = build_cube_mesh(0.5);
        assert_eq!((vertices.len(), indices.len()), (24, 36));
        for tri in indices.as_chunks::<3>().0 {
            let [a, b, c] = tri.map(|i| Vec3::from(vertices[i as usize].position));
            let face_normal = (b - a).cross(c - a);
            let n = Vec3::from(vertices[tri[0] as usize].normal);
            assert!(face_normal.dot(n) > 0.0, "counter-clockwise seen from outside");
        }
    }
}
