// SPDX-License-Identifier: MPL-2.0
//! Live pose targets (`docs/specs/60-dcc-scene-feed.md` § 3.3): a rig that follows a streamed pose.
//!
//! [`FeedPoseBuffer`] is the only thing this crate sees of the DCC scene feed; it knows nothing
//! about sockets. A [`FeedPoseTarget`] on a skinned entity overrides the local pose by joint
//! **name** after the graph evaluated and before the pose reaches the joint entities and the skin
//! palette, so spring bones, drivers and the palette all see the streamed pose.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;

use crate::components::{Animator, SkinnedMesh};
use crate::pose::Pose;

/// Makes a skinned entity follow the streamed pose of a feed item (`blend` 0 = graph only,
/// 1 = stream only).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct FeedPoseTarget {
    /// The key of the skeleton item.
    pub item: String,
    /// Weight of the stream over the graph pose (default 1).
    pub blend: f32,
    /// Renames source joints to target joints; identical names match without an entry.
    pub joint_map: Option<Vec<(String, String)>>,
}

impl FeedPoseTarget {
    /// Follows `item` fully, matching joints by identical name.
    #[must_use]
    pub fn new(item: impl Into<String>) -> Self {
        Self {
            item: item.into(),
            blend: 1.0,
            joint_map: None,
        }
    }
}

/// One item's newest pose.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedPose {
    /// Joint names, index-matched with `pose`.
    pub joints: Arc<Vec<String>>,
    /// Local pose, in the skeleton item's joint order.
    pub pose: Pose,
}

/// The newest pose of every skeleton item, by key.
#[derive(Resource, Debug, Default)]
pub struct FeedPoseBuffer {
    poses: HashMap<String, FeedPose>,
    unmatched: BTreeMap<String, BTreeSet<String>>,
    reported: BTreeMap<String, BTreeSet<String>>,
}

impl FeedPoseBuffer {
    /// Stores the newest pose of `item`.
    pub fn set(&mut self, item: impl Into<String>, pose: FeedPose) {
        self.poses.insert(item.into(), pose);
    }

    /// The newest pose of `item`.
    #[must_use]
    pub fn get(&self, item: &str) -> Option<&FeedPose> {
        self.poses.get(item)
    }

    /// Forgets `item` (deleted, purged or baked).
    pub fn remove(&mut self, item: &str) {
        self.poses.remove(item);
        self.unmatched.remove(item);
        self.reported.remove(item);
    }

    /// Source joints of `item` with no target joint, each name once per item; the caller turns
    /// them into warnings.
    pub fn take_unmatched(&mut self) -> BTreeMap<String, BTreeSet<String>> {
        std::mem::take(&mut self.unmatched)
    }

    fn note_unmatched(&mut self, item: &str, joint: &str) {
        let seen = self.reported.entry(item.to_owned()).or_default();
        if seen.insert(joint.to_owned()) {
            self.unmatched
                .entry(item.to_owned())
                .or_default()
                .insert(joint.to_owned());
        }
    }
}

/// The target joint id of each source joint (`None`: unmatched).
fn map_joints(skinned: &SkinnedMesh, target: &FeedPoseTarget, joints: &[String]) -> Vec<Option<u32>> {
    joints
        .iter()
        .map(|source| {
            let renamed = target
                .joint_map
                .as_ref()
                .and_then(|map| map.iter().find(|(from, _)| from == source))
                .map_or(source.as_str(), |(_, to)| to.as_str());
            skinned.skeleton.joint_index(renamed)
        })
        .collect()
}

/// `PostUpdate/AnimationApply`, before `apply_animation`: writes each target's streamed pose over
/// the rig's pose (the animator's, or the joint entities' when the rig has no animator).
///
/// A rig without an animator blends against its bind pose, so a blend below 1 does not compound
/// from frame to frame.
pub fn apply_feed_pose_targets(
    buffer: Option<ResMut<FeedPoseBuffer>>,
    mut rigs: Query<(&SkinnedMesh, &FeedPoseTarget, Option<&mut Animator>)>,
    mut joints: Query<&mut Transform, Without<SkinnedMesh>>,
) {
    let Some(mut buffer) = buffer else { return };
    let _span = tracing::trace_span!("dcc_feed_apply").entered();
    for (skinned, target, animator) in &mut rigs {
        let blend = crate::num::unit_weight(target.blend);
        let Some(feed) = buffer.poses.get(&target.item).cloned() else {
            continue;
        };
        let map = map_joints(skinned, target, &feed.joints);
        for (source, mapped) in map.iter().enumerate() {
            if mapped.is_none() {
                buffer.note_unmatched(&target.item, &feed.joints[source]);
            }
        }
        // Blend 0 is graph only: nothing is written, so the pose stays bit-identical.
        if blend <= 0.0 {
            continue;
        }
        match animator {
            Some(mut animator) => {
                if animator.pose.len() != skinned.skeleton.len() {
                    continue; // not evaluated yet
                }
                for (source, mapped) in map.iter().enumerate() {
                    let Some(joint) = mapped.map(|j| j as usize) else {
                        continue;
                    };
                    let (t, r, s) = blended(
                        (
                            animator.pose.translations[joint],
                            animator.pose.rotations[joint],
                            animator.pose.scales[joint],
                        ),
                        &feed.pose,
                        source,
                        blend,
                    );
                    animator.pose.translations[joint] = t;
                    animator.pose.rotations[joint] = r;
                    animator.pose.scales[joint] = s;
                }
            }
            None => {
                for (source, mapped) in map.iter().enumerate() {
                    let Some(joint) = mapped.map(|j| j as usize) else {
                        continue;
                    };
                    let Some(mut transform) = skinned.joints.get(joint).and_then(|e| joints.get_mut(*e).ok())
                    else {
                        continue;
                    };
                    let Some(bind) = skinned.skeleton.bind_pose.get(joint) else {
                        continue;
                    };
                    let (t, r, s) = blended(
                        (bind.translation, bind.rotation, bind.scale),
                        &feed.pose,
                        source,
                        blend,
                    );
                    transform.translation = t;
                    transform.rotation = r;
                    transform.scale = s;
                }
            }
        }
    }
}

/// `base` moved toward source joint `source` of `pose` by `blend`.
fn blended(base: (Vec3, Quat, Vec3), pose: &Pose, source: usize, blend: f32) -> (Vec3, Quat, Vec3) {
    // A feed whose channels are shorter than its joint list leaves the joint where it is.
    let (Some(&t), Some(&r), Some(&s)) = (
        pose.translations.get(source),
        pose.rotations.get(source),
        pose.scales.get(source),
    ) else {
        return base;
    };
    if blend >= 1.0 {
        return (t, r.normalize(), s);
    }
    (
        base.0.lerp(t, blend),
        base.1.slerp(r, blend).normalize(),
        base.2.lerp(s, blend),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::{LocalBind, Skeleton};
    use regia_core::math::Mat4;

    fn skeleton() -> Arc<Skeleton> {
        let names = ["root", "spine", "head"];
        Arc::new(Skeleton {
            joint_names: names.iter().map(|n| (*n).to_owned()).collect(),
            parents: vec![None, Some(0), Some(1)],
            bind_pose: vec![LocalBind::default(); 3],
            inverse_bind_matrices: vec![Mat4::IDENTITY; 3],
            ..Skeleton::default()
        })
    }

    fn feed(names: &[&str], x: f32) -> FeedPose {
        let mut pose = Pose::from_identity(names.len());
        for t in &mut pose.translations {
            t.x = x;
        }
        FeedPose {
            joints: Arc::new(names.iter().map(|n| (*n).to_owned()).collect()),
            pose,
        }
    }

    fn app() -> (regia_app::App, Vec<Entity>, Entity) {
        let mut app = regia_app::App::new();
        app.init_resource::<FeedPoseBuffer>();
        app.add_systems(regia_app::PostUpdate, apply_feed_pose_targets);
        let joints: Vec<Entity> = (0..3)
            .map(|_| app.world_mut().spawn(Transform::default()).id())
            .collect();
        let rig = app
            .world_mut()
            .spawn(SkinnedMesh::new(skeleton(), joints.clone()))
            .id();
        (app, joints, rig)
    }

    /// Spec § 8 test 8: joints are driven by name, whatever the stream's order.
    #[test]
    fn a_rig_without_an_animator_follows_the_stream_by_joint_name() {
        let (mut app, joints, rig) = app();
        app.world_mut().entity_mut(rig).insert(FeedPoseTarget::new("arm"));
        app.world_mut()
            .resource_mut::<FeedPoseBuffer>()
            .set("arm", feed(&["head", "root"], 2.0));
        app.update();
        let x = |e: Entity, app: &regia_app::App| app.world().get::<Transform>(e).map(|t| t.translation.x);
        assert_eq!(x(joints[2], &app), Some(2.0), "head");
        assert_eq!(x(joints[0], &app), Some(2.0), "root");
        assert_eq!(x(joints[1], &app), Some(0.0), "spine is not in the stream");
    }

    #[test]
    fn blend_zero_leaves_the_pose_alone_and_half_blends_from_the_bind_pose_without_compounding() {
        let (mut app, joints, rig) = app();
        let mut target = FeedPoseTarget::new("arm");
        target.blend = 0.0;
        app.world_mut().entity_mut(rig).insert(target.clone());
        app.world_mut()
            .resource_mut::<FeedPoseBuffer>()
            .set("arm", feed(&["root"], 4.0));
        let before = *app.world().get::<Transform>(joints[0]).expect("joint");
        app.update();
        assert_eq!(*app.world().get::<Transform>(joints[0]).expect("joint"), before);

        target.blend = 0.5;
        app.world_mut().entity_mut(rig).insert(target);
        app.update();
        app.update();
        assert_eq!(
            app.world().get::<Transform>(joints[0]).map(|t| t.translation.x),
            Some(2.0)
        );
    }

    #[test]
    fn joint_map_renames_and_unmatched_names_are_reported_once() {
        let (mut app, joints, rig) = app();
        let mut target = FeedPoseTarget::new("arm");
        target.joint_map = Some(vec![("Hips".to_owned(), "root".to_owned())]);
        app.world_mut().entity_mut(rig).insert(target);
        app.world_mut()
            .resource_mut::<FeedPoseBuffer>()
            .set("arm", feed(&["Hips", "Tail"], 3.0));
        app.update();
        assert_eq!(
            app.world().get::<Transform>(joints[0]).map(|t| t.translation.x),
            Some(3.0)
        );
        let unmatched = app.world_mut().resource_mut::<FeedPoseBuffer>().take_unmatched();
        assert_eq!(
            unmatched
                .get("arm")
                .map(|s| s.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["Tail".to_owned()])
        );
        app.update();
        assert!(
            app.world_mut()
                .resource_mut::<FeedPoseBuffer>()
                .take_unmatched()
                .is_empty(),
            "reported once"
        );
    }

    #[test]
    fn a_nan_blend_writes_nothing() {
        let (mut app, joints, rig) = app();
        let mut target = FeedPoseTarget::new("arm");
        target.blend = f32::NAN;
        app.world_mut().entity_mut(rig).insert(target);
        app.world_mut()
            .resource_mut::<FeedPoseBuffer>()
            .set("arm", feed(&["root"], 4.0));
        let before = *app.world().get::<Transform>(joints[0]).expect("joint");
        app.update();
        assert_eq!(*app.world().get::<Transform>(joints[0]).expect("joint"), before);
    }

    /// A feed whose channels are shorter than its joint list must not abort the process.
    #[test]
    fn a_feed_with_short_channels_leaves_unbacked_joints_alone() {
        let (mut app, joints, rig) = app();
        app.world_mut().entity_mut(rig).insert(FeedPoseTarget::new("arm"));
        let mut pose = feed(&["root"], 2.0);
        pose.joints = Arc::new(vec!["root".to_owned(), "spine".to_owned(), "head".to_owned()]);
        app.world_mut().resource_mut::<FeedPoseBuffer>().set("arm", pose);
        app.update();
        let x = |e: Entity, app: &regia_app::App| app.world().get::<Transform>(e).map(|t| t.translation.x);
        assert_eq!(x(joints[0], &app), Some(2.0), "the backed joint follows");
        assert_eq!(x(joints[2], &app), Some(0.0), "the unbacked joint stays");
    }
}
