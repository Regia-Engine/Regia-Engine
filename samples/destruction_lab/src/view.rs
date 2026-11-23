// SPDX-License-Identifier: MIT-0
//! How client 0 draws: a ground plate, a sun, a fixed camera, each structure whole until it first
//! breaks, its chunks through `DestructionDrawPlugin` after that, and each blast's fire and smoke
//! through `ExplosionFxPlugin`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regia::core::math::{Quat, Vec3};
use regia::framework::destruction_draw::{DestructionDrawPlugin, DestructionIntactMesh, DestructionMeshes};
use regia::framework::explosion::fx::ExplosionFxPlugin;
use regia::physics::destruction::{DestructibleId, Destruction};
use regia::prelude::*;
use regia_render::{Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_scene::transform::Transform;
use regia_vfx::{VfxPlugin, VfxRuntimePlugin};

use crate::{GROUND_HALF, GROUND_Y, LabKit, LabLook};

/// Adds the drawing to the host, with the looks of `kit`.
pub struct LabView {
    /// The structures' looks.
    pub kit: LabKit,
}

impl Plugin for LabView {
    fn build(&self, app: &mut App) {
        let looks: BTreeMap<String, LabLook> = self
            .kit
            .structures
            .iter()
            .filter_map(|s| Some((s.asset.name.clone(), s.look.clone()?)))
            .collect();
        app.add_plugins((
            regia_scene::ScenePlugin,
            DestructionDrawPlugin,
            VfxPlugin,
            VfxRuntimePlugin,
            ExplosionFxPlugin,
        ));
        {
            let mut meshes = app.world_mut().resource_mut::<DestructionMeshes>();
            for (name, look) in &looks {
                meshes.register(name.clone(), look.set.clone());
            }
        }
        app.insert_resource(Looks(looks))
            .init_resource::<Dressed>()
            .add_systems(Startup, setup_view)
            .add_systems(Update, dress_roots);
    }
}

#[derive(Resource)]
struct Looks(BTreeMap<String, LabLook>);

/// The instances that have their whole mesh.
#[derive(Resource, Default)]
struct Dressed(BTreeSet<DestructibleId>);

fn setup_view(mut commands: Commands) {
    let half = Vec3::from_array(GROUND_HALF);
    let at = Transform::from_translation(Vec3::new(0.0, GROUND_Y - half.y, 0.0));
    commands.spawn((
        at,
        GlobalTransform(at.compute_affine()),
        MeshRenderer::new(
            Arc::new(box_mesh(half)),
            Arc::new(CpuMaterial {
                base_color: [0.32, 0.34, 0.3, 1.0],
                ..CpuMaterial::default()
            }),
        ),
        DirectionalLight {
            direction: [0.4, -1.0, -0.5],
            color: [1.0, 0.97, 0.92],
            illuminance: 90_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
    let eye = Transform {
        translation: Vec3::new(0.0, 4.0, 15.0),
        rotation: Quat::from_rotation_x(-0.12),
        scale: Vec3::ONE,
    };
    commands.spawn((
        eye,
        GlobalTransform(eye.compute_affine()),
        Camera {
            exposure_ev: Some(15.0),
            clear: ClearMode::Color([0.5, 0.62, 0.78, 1.0]),
            ..Camera::default()
        },
    ));
}

/// Gives each instance the server placed its whole mesh, which the draw plugin hides once it
/// breaks. Clients place nothing: the instances arrive from the server.
fn dress_roots(
    mut commands: Commands,
    destruction: Res<Destruction>,
    looks: Res<Looks>,
    mut dressed: ResMut<Dressed>,
) {
    for (id, inst) in destruction.world.instances() {
        if dressed.0.contains(&id) {
            continue;
        }
        let Some(look) = looks.0.get(&inst.asset().name) else {
            continue;
        };
        dressed.0.insert(id);
        let pose = inst.pose();
        let at = Transform {
            translation: Vec3::from_array(pose.translation),
            rotation: Quat::from_array(pose.rotation),
            scale: Vec3::ONE,
        };
        commands.spawn((
            at,
            GlobalTransform(at.compute_affine()),
            MeshRenderer::new(look.intact.clone(), look.material.clone()),
            DestructionIntactMesh(id),
        ));
    }
}

/// An axis-aligned box with the given half extents.
fn box_mesh(half: Vec3) -> CpuMesh {
    let h = [half.x, half.y, half.z];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    let faces: [([f32; 3], usize, usize); 6] = [
        ([1.0, 0.0, 0.0], 1, 2),
        ([-1.0, 0.0, 0.0], 2, 1),
        ([0.0, 1.0, 0.0], 2, 0),
        ([0.0, -1.0, 0.0], 0, 2),
        ([0.0, 0.0, 1.0], 0, 1),
        ([0.0, 0.0, -1.0], 1, 0),
    ];
    for (normal, ua, va) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let mut position = [0.0_f32; 3];
            for (axis, value) in position.iter_mut().enumerate() {
                *value = normal[axis] * h[axis]
                    + if axis == ua { su * h[ua] } else { 0.0 }
                    + if axis == va { sv * h[va] } else { 0.0 };
            }
            vertices.push(LabVertex::from_pos_normal_uv(
                position,
                normal,
                [su * 0.5 + 0.5, sv * 0.5 + 0.5],
            ));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    CpuMesh { vertices, indices }
}
