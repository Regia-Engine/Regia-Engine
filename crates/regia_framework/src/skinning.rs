// SPDX-License-Identifier: MPL-2.0
//! Skinned meshes to the renderer (Tincal engine item C1, `docs/specs/17-animation.md` § 2,
//! ADR-242).
//!
//! [`SkinnedMeshPlugin`] fills the render-owned joint palette ([`SkinJoints`]) of every
//! [`SkinnedMesh`] entity that draws a [`MeshRenderer`], so a game needs no palette glue:
//!
//! - an entity with an [`Animator`] gets the [`SkinPalette`] the animation apply system
//!   publishes (skipped until the animator's first evaluation);
//! - an entity without one gets the palette of its joint entities' world transforms
//!   ([`SkinnedMesh::palette_from_joint_worlds`]): joints posed by a glTF hierarchy, a pose
//!   sampler, physics or the editor.
//!
//! A missing [`SkinJoints`] is inserted. The plugin is opt-in: a host that fills
//! [`SkinJoints`] itself (an external simulation's poses) keeps doing so without it.
//!
//! The palette maths is the glTF 2.0 § 3.7.3 joint matrix (`inverse(mesh world) · joint world ·
//! inverse bind`); nothing is ported.

use regia_animation::{Animator, SkinPalette, SkinnedMesh};
use regia_app::prelude::*;
use regia_app::{PostUpdate, PostUpdateSet};
use regia_core::math::Mat4;
use regia_render::{JointMatrix, MeshRenderer, SkinJoints};
use regia_scene::GlobalTransform;

/// Fills [`SkinJoints`] from [`SkinnedMesh`] every frame (see the module docs).
#[derive(Default)]
pub struct SkinnedMeshPlugin;

impl Plugin for SkinnedMeshPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            sync_skin_joints
                .after(PostUpdateSet::AnimationApply)
                .after(PostUpdateSet::TransformPropagate),
        );
    }
}

/// What [`sync_skin_joints`] reads from a skinned mesh entity.
type SkinnedMeshData = (
    Entity,
    &'static SkinnedMesh,
    &'static GlobalTransform,
    Option<Ref<'static, SkinPalette>>,
    Has<Animator>,
    Option<&'static mut SkinJoints>,
);

/// `PostUpdate` (after animation and transform propagation): writes each [`SkinnedMesh`]
/// entity's [`SkinJoints`] (see the module docs).
#[tracing::instrument(skip_all)]
pub fn sync_skin_joints(
    mut commands: Commands,
    mut meshes: Query<SkinnedMeshData, With<MeshRenderer>>,
    worlds: Query<&GlobalTransform>,
) {
    for (entity, skinned, transform, palette, animated, joints) in &mut meshes {
        match (palette, joints) {
            (Some(palette), Some(mut joints)) => {
                let n = palette.matrices.len();
                if !palette.is_changed() && joints.palette.len() == n {
                    continue;
                }
                write_palette(&mut joints.palette, palette.matrices.iter().copied());
            }
            (Some(palette), None) => {
                let mut rows = Vec::new();
                write_palette(&mut rows, palette.matrices.iter().copied());
                commands.entity(entity).insert(SkinJoints { palette: rows });
            }
            // Not evaluated yet: keep whatever the entity has (its bind pose).
            (None, _) if animated => continue,
            (None, joints) => {
                let mesh_world = Mat4::from(transform.0);
                if !skinned.is_complete() || mesh_world.determinant().abs() < f32::EPSILON {
                    continue;
                }
                if skinned.joints.iter().any(|&joint| worlds.get(joint).is_err()) {
                    continue;
                }
                let to_mesh = mesh_world.inverse();
                let rows = skinned
                    .joints
                    .iter()
                    .zip(&skinned.skeleton.inverse_bind_matrices)
                    .map(|(&joint, inverse_bind)| {
                        let world = worlds
                            .get(joint)
                            .map(|w| Mat4::from(w.0))
                            .unwrap_or(Mat4::IDENTITY);
                        to_mesh * world * *inverse_bind
                    });
                match joints {
                    Some(mut joints) => write_palette(&mut joints.palette, rows),
                    None => {
                        let mut palette = Vec::new();
                        write_palette(&mut palette, rows);
                        commands.entity(entity).insert(SkinJoints { palette });
                    }
                }
            }
        }
    }
}

/// Writes `matrices` into `palette`. The buffer grows or shrinks only when the length differs.
fn write_palette(palette: &mut Vec<JointMatrix>, matrices: impl ExactSizeIterator<Item = Mat4>) {
    let n = matrices.len();
    if palette.len() != n {
        palette.resize(n, JointMatrix::from_mat4(Mat4::IDENTITY));
    }
    for (slot, matrix) in palette.iter_mut().zip(matrices) {
        *slot = JointMatrix::from_mat4(matrix);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_animation::Skeleton;
    use regia_animation::skeleton::LocalBind;
    use regia_core::math::{Quat, Vec3};
    use regia_render::{CpuMaterial, CpuMesh};
    use regia_scene::Transform;

    use super::*;

    fn skeleton() -> Arc<Skeleton> {
        Arc::new(
            Skeleton::from_local_bind(
                vec!["root".into(), "tip".into()],
                vec![None, Some(0)],
                vec![
                    LocalBind::default(),
                    LocalBind {
                        translation: Vec3::new(0.0, 1.0, 0.0),
                        ..LocalBind::default()
                    },
                ],
            )
            .expect("skeleton"),
        )
    }

    fn mesh() -> MeshRenderer {
        MeshRenderer::new(
            Arc::new(CpuMesh {
                vertices: vec![regia_render::LabVertex::from_pos_normal_uv(
                    [0.0, 1.0, 0.0],
                    [0.0, 0.0, 1.0],
                    [0.0, 0.0],
                )],
                indices: vec![0, 0, 0],
            }),
            Arc::new(CpuMaterial::default()),
        )
    }

    #[test]
    fn joint_world_transforms_give_the_palette() {
        let mut app = App::new();
        app.add_plugins((
            regia_app::MinimalPlugins,
            regia_scene::ScenePlugin,
            SkinnedMeshPlugin,
        ));
        let skeleton = skeleton();
        let rig_at = Vec3::new(5.0, 0.0, 0.0);
        let rig = app.world_mut().spawn(Transform::from_translation(rig_at)).id();
        // Joints posed in the world (a glTF-style hierarchy under the rig): the tip turned 90°
        // about Z at its bind position.
        let root = app.world_mut().spawn((Transform::IDENTITY, ChildOf(rig))).id();
        let turn = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let tip = app
            .world_mut()
            .spawn((
                Transform {
                    translation: Vec3::new(0.0, 1.0, 0.0),
                    rotation: turn,
                    ..Transform::IDENTITY
                },
                ChildOf(root),
            ))
            .id();
        app.world_mut()
            .entity_mut(rig)
            .insert((SkinnedMesh::new(skeleton, vec![root, tip]), mesh()));
        app.update();
        app.update();
        let joints = app.world().get::<SkinJoints>(rig).expect("palette inserted");
        assert_eq!(joints.palette.len(), 2);
        // Joint 0 sits at the rig origin: identity. Joint 1: rotated about its bind position.
        let p = joints.palette[0].transform_point([0.3, 0.2, 0.1]);
        assert!(
            (Vec3::from(p) - Vec3::new(0.3, 0.2, 0.1)).length() < 1e-5,
            "{p:?}"
        );
        // The vertex at the tip's bind position (0, 1, 0) stays there; (0, 2, 0) turns to (-1, 1, 0).
        let tip_vertex = joints.palette[1].transform_point([0.0, 1.0, 0.0]);
        assert!(
            (Vec3::from(tip_vertex) - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-5,
            "{tip_vertex:?}"
        );
        let above = joints.palette[1].transform_point([0.0, 2.0, 0.0]);
        assert!(
            (Vec3::from(above) - Vec3::new(-1.0, 1.0, 0.0)).length() < 1e-5,
            "{above:?}"
        );
    }

    #[test]
    fn an_animated_rig_takes_the_skin_palette() {
        let mut app = App::new();
        app.add_plugins((
            regia_app::MinimalPlugins,
            regia_scene::ScenePlugin,
            SkinnedMeshPlugin,
        ));
        let skeleton = skeleton();
        let binding = regia_animation::components::spawn_skeleton(app.world_mut(), skeleton.clone());
        let shift = Mat4::from_translation(Vec3::new(0.0, 0.0, 2.0));
        let rig = app
            .world_mut()
            .spawn((
                Transform::IDENTITY,
                binding,
                mesh(),
                SkinPalette {
                    matrices: vec![shift, shift],
                },
                Animator::single(regia_animation::AnimationClip::empty("idle", 1.0), true),
            ))
            .id();
        app.update();
        let joints = app.world().get::<SkinJoints>(rig).expect("palette");
        assert_eq!(
            joints.palette[1].transform_point([0.0, 0.0, 0.0]),
            [0.0, 0.0, 2.0]
        );
    }

    /// An idle animated rig's second palette sync writes the existing buffer and allocates nothing.
    #[test]
    fn idle_skinned_mesh_allocates_nothing_on_the_second_frame() {
        let mut app = App::new();
        app.add_plugins((
            regia_app::MinimalPlugins,
            regia_scene::ScenePlugin,
            SkinnedMeshPlugin,
        ));
        let skeleton = skeleton();
        let binding = regia_animation::components::spawn_skeleton(app.world_mut(), skeleton.clone());
        let shift = Mat4::from_translation(Vec3::new(0.0, 0.0, 2.0));
        let rig = app
            .world_mut()
            .spawn((
                Transform::IDENTITY,
                binding,
                mesh(),
                SkinPalette {
                    matrices: vec![shift, shift],
                },
                Animator::single(regia_animation::AnimationClip::empty("idle", 1.0), true),
            ))
            .id();
        // `World::run_system_cached` takes the cached id with `resource_scope` on every call
        // (24 heap requests for an empty system on this build). An initialized system is the
        // steady frame itself.
        use regia_ecs::ecs::system::{IntoSystem, System};
        let mut system = IntoSystem::into_system(sync_skin_joints);
        {
            let world = app.world_mut();
            system.initialize(world);
            system.run((), world).expect("first sync");
        }
        let before = app
            .world()
            .get::<SkinJoints>(rig)
            .expect("palette")
            .palette
            .as_ptr();
        // Dirty the component the way animation apply does, without replacing the matrix buffer.
        {
            let mut palette = app.world_mut().get_mut::<SkinPalette>(rig).expect("palette");
            palette.matrices[0].w_axis.z = 2.0;
        }
        let allocated = crate::test_alloc::count(|| {
            system.run((), app.world_mut()).expect("second sync");
        });
        let after = app.world().get::<SkinJoints>(rig).expect("palette");
        assert_eq!(after.palette.as_ptr(), before, "the stored palette buffer moved");
        assert_eq!(allocated, 0, "the second sync allocated {allocated} times");
        assert_eq!(after.palette[1].transform_point([0.0, 0.0, 0.0]), [0.0, 0.0, 2.0]);
    }
}
