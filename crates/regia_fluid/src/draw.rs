// SPDX-License-Identifier: MPL-2.0
//! Drawing (`draw` feature): each simulation's liquid surface and whitewater points become two
//! `MeshRenderer` entities, re-published when the entry's revision changes. The surface uses the
//! marching-cubes mesh; the whitewater points are small octahedra coloured by type (vertex colour).

use std::collections::BTreeMap;
use std::sync::Arc;

use glam::Vec3;
use regia_app::prelude::*;
use regia_render::{ALPHA_BLEND, CpuMaterial, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::{GlobalTransform, Transform};

use crate::frame::{FluidFrame, PointKind};
use crate::plugin::FluidWorld;
use crate::surface::{SurfaceMesh, surface_mesh_of};

/// Most whitewater points drawn per simulation (the rest are skipped in order).
pub const MAX_DRAWN_POINTS: usize = 65_536;

/// The two entities of one simulation.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FluidDrawn {
    /// Simulation id.
    pub id: u32,
    /// `false` = surface, `true` = whitewater points.
    pub points: bool,
}

/// Last published revision per simulation id.
#[derive(Resource, Debug, Default)]
pub struct FluidDrawState {
    published: BTreeMap<u32, u64>,
}

fn write_surface(mesh: &mut CpuMesh, surface: &SurfaceMesh) {
    mesh.vertices.clear();
    mesh.indices.clear();
    mesh.vertices.reserve(surface.positions.len());
    for (p, n) in surface.positions.iter().zip(&surface.normals) {
        mesh.vertices
            .push(LabVertex::from_pos_normal_uv(*p, *n, [0.0, 0.0]));
    }
    mesh.indices.extend_from_slice(&surface.indices);
}

/// [`write_surface`] into a new mesh (the cached-playback test compares whole meshes).
#[cfg(test)]
fn surface_cpu_mesh(surface: &SurfaceMesh) -> CpuMesh {
    let mut mesh = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    write_surface(&mut mesh, surface);
    mesh
}

fn write_points(mesh: &mut CpuMesh, frame: &FluidFrame, size: f32) {
    let CpuMesh { vertices, indices } = mesh;
    vertices.clear();
    indices.clear();
    let axes = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    // The eight faces of an octahedron as corners of `axes`.
    const FACES: [[u32; 3]; 8] = [
        [0, 2, 4],
        [2, 1, 4],
        [1, 3, 4],
        [3, 0, 4],
        [2, 0, 5],
        [1, 2, 5],
        [3, 1, 5],
        [0, 3, 5],
    ];
    let mut drawn = 0;
    for (p, kind) in frame.positions.iter().zip(&frame.kinds) {
        if *kind == PointKind::Liquid as u8 {
            continue;
        }
        if drawn >= MAX_DRAWN_POINTS {
            break;
        }
        drawn += 1;
        let color = match PointKind::from_tag(*kind) {
            Some(PointKind::Spray) => [0.85, 0.9, 1.0, 0.8],
            Some(PointKind::Bubble) => [0.7, 0.85, 1.0, 0.5],
            _ => [1.0, 1.0, 1.0, 0.9],
        };
        let base = vertices.len() as u32;
        for a in axes {
            let mut v = LabVertex::from_pos_normal_uv(
                [p[0] + a[0] * size, p[1] + a[1] * size, p[2] + a[2] * size],
                a,
                [0.0, 0.0],
            );
            v.color = color;
            vertices.push(v);
        }
        for f in FACES {
            indices.extend(f.map(|i| base + i));
        }
    }
}

/// Publishes changed simulations and despawns the entities of removed ones.
pub fn publish_fluids(
    mut commands: Commands,
    fluids: Option<Res<FluidWorld>>,
    mut state: Local<FluidDrawState>,
    mut drawn: Query<(Entity, &FluidDrawn, &mut MeshRenderer)>,
    mut extracted: Option<ResMut<regia_render::ExtractedFrame>>,
) {
    let _span = tracing::info_span!(target: "regia_fluid", "fluid_publish").entered();
    let Some(fluids) = fluids else {
        return;
    };
    for (entity, tag, _) in &drawn {
        if fluids.get(tag.id).is_none() {
            commands.entity(entity).despawn();
            state.published.remove(&tag.id);
        }
    }
    for entry in fluids.entries() {
        if state.published.get(&entry.id) == Some(&entry.revision) {
            continue;
        }
        state.published.insert(entry.id, entry.revision);
        let frame = entry.shown_frame();
        let point_size = entry.sim.dims().dx * 0.08;
        let surface = if entry.surface_resolution == 0 {
            SurfaceMesh::default()
        } else if entry.playing.is_some() {
            // Playback: the surface of the played frame's liquid, never the live simulator's
            // (the point mesh draws only whitewater, so this is the liquid on screen).
            let liquid: Vec<Vec3> = frame
                .positions
                .iter()
                .zip(&frame.kinds)
                .filter(|(_, k)| **k == PointKind::Liquid as u8)
                .map(|(p, _)| Vec3::from(*p))
                .collect();
            surface_mesh_of(entry.sim.dims(), &liquid, entry.surface_resolution)
        } else {
            entry.sim.surface_mesh(entry.surface_resolution)
        };
        let mut found = [false, false];
        for (_, tag, mut renderer) in &mut drawn {
            if tag.id != entry.id {
                continue;
            }
            if let Some(frame) = extracted.as_mut() {
                let ptr = Arc::as_ptr(&renderer.mesh);
                frame
                    .meshes
                    .retain(|extracted_mesh| Arc::as_ptr(&extracted_mesh.mesh) != ptr);
            }
            // `mesh_key` (the render cache, and this test's snapshot) holds a `Weak`. That
            // weak makes `Arc::get_mut` fail on the unique owner, so every later revision
            // allocated a new `Arc` and a new upload. Park the weak, edit, keep the key.
            let reused = regia_render::with_unique_mesh_mut(&mut renderer.mesh, |mesh| {
                if tag.points {
                    write_points(mesh, &frame, point_size);
                } else {
                    write_surface(mesh, &surface);
                }
            })
            .is_some();
            if reused {
                regia_render::note_mesh_content_changed(&renderer.mesh);
                found[usize::from(tag.points)] = true;
            }
        }
        for points_flag in [false, true] {
            if found[usize::from(points_flag)] {
                continue;
            }
            let mut cpu = CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            };
            if points_flag {
                write_points(&mut cpu, &frame, point_size);
            } else {
                write_surface(&mut cpu, &surface);
            }
            let mesh = Arc::new(cpu);
            let mut placed = false;
            for (_, tag, mut renderer) in &mut drawn {
                if tag.id == entry.id && tag.points == points_flag {
                    renderer.mesh = Arc::clone(&mesh);
                    placed = true;
                    break;
                }
            }
            if placed {
                continue;
            }
            let material = Arc::new(CpuMaterial {
                base_color: if points_flag {
                    [1.0; 4]
                } else {
                    [0.35, 0.55, 0.75, 0.6]
                },
                roughness: 0.05,
                metallic: 0.0,
                alpha_mode: ALPHA_BLEND,
                ..CpuMaterial::EMPTY
            });
            let mut renderer = MeshRenderer::new(mesh, material);
            renderer.gi_proxy = false;
            commands.spawn((
                renderer,
                Transform::default(),
                GlobalTransform::default(),
                FluidDrawn {
                    id: entry.id,
                    points: points_flag,
                },
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use regia_ecs::ecs::system::RunSystemOnce;

    use super::*;
    use crate::presets;
    use crate::sim::FluidParams;

    /// One surface and one point entity per simulation, updated in place, despawned with it.
    #[test]
    fn publishes_two_entities_per_simulation() {
        let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx * 2.0,
            ..FluidParams::default()
        };
        let mut sim = presets::build("dam_break", params).expect("dam");
        for _ in 0..20 {
            sim.step();
        }
        let mut fluids = FluidWorld::default();
        let id = fluids.insert(sim, None).expect("id");
        let mut world = World::new();
        world.insert_resource(fluids);
        world.run_system_once(publish_fluids).expect("publish");
        let count = |world: &mut World| world.query::<&FluidDrawn>().iter(world).count();
        assert_eq!(count(&mut world), 2);
        let surface_triangles = world
            .query::<(&FluidDrawn, &MeshRenderer)>()
            .iter(&world)
            .find(|(t, _)| !t.points)
            .map(|(_, r)| r.mesh.indices.len() / 3);
        assert!(surface_triangles.is_some_and(|n| n > 0), "{surface_triangles:?}");
        world.run_system_once(publish_fluids).expect("again");
        assert_eq!(count(&mut world), 2, "re-publishing must update, not spawn");
        world.resource_mut::<FluidWorld>().remove(id);
        world.run_system_once(publish_fluids).expect("remove");
        assert_eq!(count(&mut world), 0);
    }

    /// (Audit G-fluid #10) Playing a cache frame draws that frame's liquid: the surface is built
    /// from the cached positions (not the live simulator, and not left empty).
    #[test]
    fn playback_draws_cached_liquid() {
        let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx * 2.0,
            ..FluidParams::default()
        };
        let mut sim = presets::build("dam_break", params).expect("dam");
        for _ in 0..10 {
            sim.step();
        }
        let mut cache = crate::FluidCache::for_sim(&sim, false);
        cache.record(&sim).expect("record");
        let cached = cache.play(0).expect("frame 0");
        // The live liquid moves on, so a live surface would not match the bake.
        for _ in 0..20 {
            sim.step();
        }
        let resolution = 2;
        let mut baked = sim.clone();
        let positions: Vec<glam::Vec3> = cached.positions.iter().map(|p| glam::Vec3::from(*p)).collect();
        let velocities = vec![glam::Vec3::ZERO; positions.len()];
        baked.set_particles(positions, velocities);
        let expected = surface_cpu_mesh(&baked.surface_mesh(resolution));
        let live = surface_cpu_mesh(&sim.surface_mesh(resolution));
        assert!(!expected.indices.is_empty());
        assert_ne!(expected.indices.len(), live.indices.len());

        let mut fluids = FluidWorld::default();
        let id = fluids.insert(sim, None).expect("id");
        let entry = fluids.get_mut(id).expect("entry");
        entry.cache = Some(cache);
        entry.playing = Some(0);
        entry.surface_resolution = resolution;
        let mut world = World::new();
        world.insert_resource(fluids);
        world.run_system_once(publish_fluids).expect("publish");
        let surface = world
            .query::<(&FluidDrawn, &MeshRenderer)>()
            .iter(&world)
            .find(|(t, _)| !t.points)
            .map(|(_, r)| (r.mesh.vertices.len(), r.mesh.indices.clone()))
            .expect("surface entity");
        assert_eq!(surface.0, expected.vertices.len(), "surface vertices");
        assert_eq!(surface.1, expected.indices, "surface of the cached frame");
    }

    /// A second publish of the same simulation keeps the mesh `Arc` (and therefore
    /// [`regia_render::mesh_key`]). A stand-in of the render cache counts one upload per mesh,
    /// not one per step: a new `Arc` is a new key and a new upload.
    #[test]
    fn republish_keeps_mesh_key() {
        use std::collections::BTreeSet;

        let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx * 2.0,
            particle_cap: 65_536,
            ..FluidParams::default()
        };
        let mut sim = presets::build("dam_break", params).expect("dam");
        for _ in 0..8 {
            sim.step();
        }
        let mut fluids = FluidWorld::default();
        let id = fluids.insert(sim, None).expect("id");
        let mut world = World::new();
        world.insert_resource(fluids);
        world.run_system_once(publish_fluids).expect("publish");
        let snapshot = |world: &mut World| {
            let mut keys = Vec::new();
            let mut ptrs = Vec::new();
            let mut tris = 0usize;
            for (tag, renderer) in world.query::<(&FluidDrawn, &MeshRenderer)>().iter(world) {
                keys.push((tag.points, regia_render::mesh_key(&renderer.mesh)));
                ptrs.push((tag.points, Arc::as_ptr(&renderer.mesh) as usize));
                if !tag.points {
                    tris = renderer.mesh.indices.len() / 3;
                }
            }
            keys.sort_by_key(|(points, _)| *points);
            ptrs.sort_by_key(|(points, _)| *points);
            (keys, ptrs, tris)
        };
        let (keys, ptrs, tris) = snapshot(&mut world);
        assert!(tris > 0, "the first publish drew no surface");
        {
            let mut fluids = world.resource_mut::<FluidWorld>();
            let entry = fluids.get_mut(id).expect("entry");
            for _ in 0..4 {
                entry.sim.step();
            }
            entry.revision += 1;
        }
        world.run_system_once(publish_fluids).expect("republish");
        let (keys2, ptrs2, tris2) = snapshot(&mut world);
        let mut uploads = BTreeSet::new();
        for (_, key) in keys.iter().chain(&keys2) {
            uploads.insert(*key);
        }
        eprintln!(
            "republish keys {keys:?} -> {keys2:?}, stand-in uploads {}, triangles {tris} -> {tris2}",
            uploads.len()
        );
        assert_eq!(keys, keys2, "a new Arc changes the mesh key and re-uploads");
        assert_eq!(ptrs, ptrs2, "publish replaced the mesh Arc");
        assert_eq!(
            uploads.len(),
            2,
            "stand-in counted one upload per step instead of one per mesh"
        );
    }
}
