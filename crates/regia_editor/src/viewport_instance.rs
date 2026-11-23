// SPDX-License-Identifier: MPL-2.0
//! Several independent viewports per world (Tincal engine request F2, ADR-230).
//!
//! The editor's main viewport keeps its camera, layout and display settings in world singletons
//! ([`EditorCamera`], [`ViewportLayout`], [`ViewportSettings`]). A [`ViewportInstance`] carries
//! its own copy of all three, so a workspace view, a review lightbox, a before/after toggle and a
//! side-by-side compare each orbit, show and render at their own quality preset over the same
//! world. [`crate::viewport::viewport_panel_with`] draws one; the old
//! [`crate::viewport::viewport_panel`] is that panel over the singletons.
//!
//! A [`CameraLinkGroup`] keeps panes orbiting together (A/B compare): every member's
//! perspective camera follows the last member that moved. It is a shared handle, so the panes
//! may even show different worlds.
//!
//! Each drawn instance leaves a snapshot in the world's [`ViewportInstances`] registry: the
//! agent protocol lists, edits, links and captures instances through it
//! ([`crate::viewport_instance_agent`], `book/agents/viewports.md`), and the panel applies the
//! agent's edits on its next draw.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use regia_ecs::prelude::*;

use crate::viewport::{EditorCamera, ViewportSettings, ViewportStats};
use crate::views::{ViewCamera, ViewKind, ViewportLayout};

/// What one [`CameraLinkGroup::sync`] did to the member's camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSync {
    /// Another member moved since this one last synced: the member's camera now equals the
    /// group's.
    Adopted,
    /// This member's camera differed from the group's (it moved, or it seeded a new group):
    /// the group now follows it.
    Published,
    /// Already in step.
    Unchanged,
}

#[derive(Debug, Default)]
struct LinkShared {
    /// The group's camera (`None` until the first member syncs).
    camera: Option<EditorCamera>,
    /// Bumped on every publish.
    generation: u64,
    /// Per member (instance id): the generation it last saw.
    seen: HashMap<u64, u64>,
}

/// Panes whose perspective cameras move together (A/B compare, before/after).
///
/// A cheap shared handle: clone it into every [`ViewportInstance::link`] of the group. Members
/// are told apart by their instance id, which must be unique within the group. On each sync a
/// member adopts the group's camera when another member moved since its last sync, and
/// otherwise publishes its own camera if it changed, so the last pane moved leads. When two
/// members change in the same frame, the first to sync wins.
///
/// The whole [`EditorCamera`] is linked (target, distance, yaw, pitch, field of view, fly
/// speed). Orthographic panes of a quad layout keep their own cameras.
#[derive(Clone)]
pub struct CameraLinkGroup {
    id: u64,
    shared: Arc<Mutex<LinkShared>>,
}

impl std::fmt::Debug for CameraLinkGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraLinkGroup")
            .field("id", &self.id)
            .field("generation", &self.generation())
            .finish()
    }
}

impl PartialEq for CameraLinkGroup {
    /// The same group: clones of one handle (not merely an equal id).
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl CameraLinkGroup {
    /// A new, empty group. `id` is a label for menus and the agent protocol.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            shared: Arc::new(Mutex::new(LinkShared::default())),
        }
    }

    /// The group's label.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LinkShared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The group's current camera (`None` before any member synced).
    #[must_use]
    pub fn camera(&self) -> Option<EditorCamera> {
        self.lock().camera.clone()
    }

    /// How many times a member published a camera.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Instance ids that synced with the group and have not left it, ascending.
    #[must_use]
    pub fn members(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self.lock().seen.keys().copied().collect();
        out.sort_unstable();
        out
    }

    /// Brings `camera` (the perspective camera of instance `member`) in step with the group;
    /// see the type docs. The panel calls it before and after its input.
    pub fn sync(&self, member: u64, camera: &mut EditorCamera) -> LinkSync {
        let mut shared = self.lock();
        let seen = shared.seen.get(&member).copied().unwrap_or(0);
        let outcome = match shared.camera.as_ref() {
            Some(group) if seen < shared.generation => {
                *camera = group.clone();
                LinkSync::Adopted
            }
            Some(group) if group == camera => LinkSync::Unchanged,
            _ => {
                shared.camera = Some(camera.clone());
                shared.generation += 1;
                LinkSync::Published
            }
        };
        let generation = shared.generation;
        shared.seen.insert(member, generation);
        outcome
    }

    /// Instance `member` leaves the group (it no longer counts as a member; joining again adopts
    /// the group's camera).
    pub fn leave(&self, member: u64) {
        self.lock().seen.remove(&member);
    }
}

/// One independent viewport over a world: its own perspective camera, layout (panes, quality
/// preset, show flags, game view) and display settings, and an optional camera link.
///
/// Draw it with [`crate::viewport::viewport_panel_with`] (keep one
/// [`crate::viewport::ViewportPanelState`] per instance) or capture it headless with
/// [`crate::render::capture_instance`]. `id` names it in the [`ViewportInstances`] registry,
/// in egui widget ids and in its renderer cache; keep it unique per world (and per link group).
#[derive(Debug, Clone, Default)]
pub struct ViewportInstance {
    /// Stable id (unique per world).
    pub id: u64,
    /// The perspective camera (the [`EditorCamera`] resource's role for the main viewport).
    /// Orthographic panes keep theirs in [`ViewportLayout::panes`].
    pub camera: EditorCamera,
    /// Panes, active pane, quality preset, show flags, game view and play-mode possession.
    pub layout: ViewportLayout,
    /// Grid, labels, realtime and audio switches.
    pub settings: ViewportSettings,
    /// Camera link: members of one group orbit together.
    pub link: Option<CameraLinkGroup>,
}

impl ViewportInstance {
    /// An instance with the default camera, layout and settings.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    /// An instance starting from the world's main-viewport singletons (defaults where absent),
    /// unlinked. The world is not changed.
    #[must_use]
    pub fn from_world(world: &World, id: u64) -> Self {
        Self {
            id,
            camera: world.get_resource::<EditorCamera>().cloned().unwrap_or_default(),
            layout: world
                .get_resource::<ViewportLayout>()
                .cloned()
                .unwrap_or_default(),
            settings: world
                .get_resource::<ViewportSettings>()
                .cloned()
                .unwrap_or_default(),
            link: None,
        }
    }

    /// The camera of pane `index` (0–3): [`Self::camera`] for the perspective pane, the
    /// layout's own camera for orthographic ones.
    #[must_use]
    pub fn pane_camera(&self, index: usize) -> (ViewKind, EditorCamera) {
        let pane = &self.layout.panes[index.min(3)];
        if pane.kind == ViewKind::Perspective {
            (pane.kind, self.camera.clone())
        } else {
            (pane.kind, pane.camera.clone())
        }
    }

    /// Stores a camera back into pane `index` (see [`Self::pane_camera`]).
    pub fn set_pane_camera(&mut self, index: usize, camera: EditorCamera) {
        let pane = &mut self.layout.panes[index.min(3)];
        if pane.kind == ViewKind::Perspective {
            self.camera = camera;
        } else {
            pane.camera = camera;
        }
    }

    /// Joins `group` (leaving any previous one) and syncs: the instance adopts the group's
    /// camera, or seeds an empty group with its own.
    pub fn link_to(&mut self, group: &CameraLinkGroup) -> LinkSync {
        if let Some(old) = self.link.take()
            && old != *group
        {
            old.leave(self.id);
        }
        self.link = Some(group.clone());
        group.sync(self.id, &mut self.camera)
    }

    /// Leaves the camera link group; the camera stays where it is.
    pub fn unlink(&mut self) {
        if let Some(group) = self.link.take() {
            group.leave(self.id);
        }
    }

    /// Syncs with the link group, if any (`None` when unlinked).
    pub fn sync_link(&mut self) -> Option<LinkSync> {
        let group = self.link.clone()?;
        Some(group.sync(self.id, &mut self.camera))
    }
}

/// One rendered pane of an instance's last draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstancePane {
    /// Pane index (0–3).
    pub index: usize,
    /// Which way it looks.
    pub kind: ViewKind,
    /// The camera the picture used (the game camera when possessed).
    pub view: ViewCamera,
    /// Render size, pixels (0×0 when no GPU frame was rendered).
    pub size: [u32; 2],
}

/// An instance as last drawn (or published), plus its statistics.
#[derive(Debug, Clone, Default)]
pub struct InstanceRecord {
    /// The instance's state after its last draw, with any agent edits since.
    pub instance: ViewportInstance,
    /// One more than the egui frame number of the last draw (0 = never drawn by a panel).
    pub frame: u64,
    /// The panes of the last draw.
    pub panes: Vec<InstancePane>,
    /// Frame pacing and the last pane render's information, per instance.
    pub stats: ViewportStats,
    /// Bumped by every agent edit; the panel adopts the record when it is ahead of `applied`.
    edits: u64,
    applied: u64,
}

/// The world's viewport instances (a side table: hosts never build it). Panels publish here;
/// the agent protocol reads and edits it.
#[derive(Resource, Debug, Default)]
pub struct ViewportInstances {
    records: BTreeMap<u64, InstanceRecord>,
    /// The newest frame a panel drew in (egui frame number + 1).
    frame: u64,
    /// The main (singleton) viewport's last draw (egui frame number + 1; 0 = never).
    main_frame: u64,
}

impl ViewportInstances {
    /// The record of instance `id`.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&InstanceRecord> {
        self.records.get(&id)
    }

    /// Every registered instance id, ascending.
    #[must_use]
    pub fn ids(&self) -> Vec<u64> {
        self.records.keys().copied().collect()
    }

    /// Whether instance `id` was drawn by a panel in the newest or the previous egui frame.
    #[must_use]
    pub fn is_live(&self, id: u64) -> bool {
        self.records
            .get(&id)
            .is_some_and(|record| record.frame > 0 && record.frame + 1 >= self.frame)
    }

    /// Views drawing into one world this frame: live instances plus the main viewport.
    #[must_use]
    pub fn live_views(&self) -> usize {
        let instances = self.records.keys().filter(|id| self.is_live(**id)).count();
        let main = self.main_frame > 0 && self.main_frame + 1 >= self.frame;
        instances + usize::from(main)
    }

    /// Registers or refreshes `instance`'s snapshot without drawing it (a host rendering it
    /// headless, or one that wants it visible to the agent before its first draw). The
    /// snapshot is replaced: agent edits not yet taken are dropped.
    pub fn publish(world: &mut World, instance: &ViewportInstance) {
        let mut registry = world.get_resource_or_insert_with(Self::default);
        let record = registry.records.entry(instance.id).or_default();
        record.instance = instance.clone();
        record.applied = record.edits;
    }

    /// Applies the agent's edits since the instance last took them (camera, layout, settings,
    /// link). Returns whether anything was applied.
    pub fn take_agent_edits(world: &mut World, instance: &mut ViewportInstance) -> bool {
        let Some(mut registry) = world.get_resource_mut::<Self>() else {
            return false;
        };
        let Some(record) = registry.records.get_mut(&instance.id) else {
            return false;
        };
        if record.edits == record.applied {
            return false;
        }
        record.applied = record.edits;
        *instance = record.instance.clone();
        true
    }

    /// Removes instance `id`: forgets its record, leaves its link group and frees its
    /// viewport-owned renderers. Returns whether it was registered.
    pub fn remove(world: &mut World, id: u64) -> bool {
        crate::render::release_instance_renderers(id);
        let Some(mut registry) = world.get_resource_mut::<Self>() else {
            return false;
        };
        match registry.records.remove(&id) {
            Some(record) => {
                if let Some(group) = record.instance.link {
                    group.leave(id);
                }
                true
            }
            None => false,
        }
    }

    /// An agent edit of instance `id` (creating it when `create`); the panel adopts it on its
    /// next draw. Returns the edited snapshot, or `None` for an unknown id.
    pub(crate) fn edit(
        world: &mut World,
        id: u64,
        create: bool,
        f: impl FnOnce(&mut ViewportInstance),
    ) -> Option<ViewportInstance> {
        if create
            && world
                .get_resource::<Self>()
                .is_none_or(|r| !r.records.contains_key(&id))
        {
            let seed = ViewportInstance::from_world(world, id);
            let mut registry = world.get_resource_or_insert_with(Self::default);
            registry.records.insert(
                id,
                InstanceRecord {
                    instance: seed,
                    ..InstanceRecord::default()
                },
            );
        }
        let mut registry = world.get_resource_mut::<Self>()?;
        let record = registry.records.get_mut(&id)?;
        f(&mut record.instance);
        record.edits += 1;
        Some(record.instance.clone())
    }

    /// A panel starts drawing `instance` in frame `frame` (egui frame number + 1): agent edits are applied and the
    /// statistics are handed to the panel (returned to [`Self::end_draw`]).
    pub(crate) fn begin_draw(
        world: &mut World,
        instance: &mut ViewportInstance,
        frame: u64,
    ) -> ViewportStats {
        Self::take_agent_edits(world, instance);
        let mut registry = world.get_resource_or_insert_with(Self::default);
        registry.frame = registry.frame.max(frame);
        let record = registry.records.entry(instance.id).or_default();
        std::mem::take(&mut record.stats)
    }

    /// A panel finished drawing `instance`: the snapshot, panes and statistics are stored.
    pub(crate) fn end_draw(
        world: &mut World,
        instance: &ViewportInstance,
        frame: u64,
        panes: Vec<InstancePane>,
        stats: ViewportStats,
    ) {
        let mut registry = world.get_resource_or_insert_with(Self::default);
        let record = registry.records.entry(instance.id).or_default();
        record.instance = instance.clone();
        record.frame = frame;
        record.panes = panes;
        record.stats = stats;
        record.applied = record.edits;
    }

    /// The main viewport drew in frame `frame` (egui frame number + 1) (recorded only in worlds that use instances).
    pub(crate) fn main_drawn(world: &mut World, frame: u64) {
        if let Some(mut registry) = world.get_resource_mut::<Self>() {
            registry.frame = registry.frame.max(frame);
            registry.main_frame = frame;
        }
    }

    /// Several views draw into one world this frame, so temporal history must not be shared
    /// through the world's own renderer.
    pub(crate) fn shared_history(world: &World) -> bool {
        world
            .get_resource::<Self>()
            .is_some_and(|registry| registry.live_views() > 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;

    #[test]
    fn a_linked_member_adopts_the_leader_and_unlinking_stops_it() {
        let group = CameraLinkGroup::new(7);
        let mut a = ViewportInstance::new(1);
        let mut b = ViewportInstance::new(2);
        b.camera.yaw = 2.0;
        // A seeds the group; B joins and adopts A's camera.
        assert_eq!(a.link_to(&group), LinkSync::Published);
        assert_eq!(b.link_to(&group), LinkSync::Adopted);
        assert_eq!(a.camera, b.camera);
        assert_eq!(group.members(), vec![1, 2]);

        // A orbits: its next sync publishes, B's adopts.
        a.camera.orbit(40.0, 10.0);
        assert_eq!(a.sync_link(), Some(LinkSync::Published));
        assert_eq!(b.sync_link(), Some(LinkSync::Adopted));
        assert_eq!(a.camera, b.camera);
        assert_eq!(a.sync_link(), Some(LinkSync::Unchanged));

        // B leads the other way.
        b.camera.target = Vec3::new(3.0, 0.0, -1.0);
        assert_eq!(b.sync_link(), Some(LinkSync::Published));
        assert_eq!(a.sync_link(), Some(LinkSync::Adopted));
        assert_eq!(a.camera.target, Vec3::new(3.0, 0.0, -1.0));

        // Unlinked, B moves alone.
        b.unlink();
        assert_eq!(group.members(), vec![1]);
        b.camera.zoom(2.0);
        assert_eq!(b.sync_link(), None);
        assert_eq!(a.sync_link(), Some(LinkSync::Unchanged));
        assert_ne!(a.camera.distance, b.camera.distance);

        // Rejoining adopts the group's camera.
        assert_eq!(b.link_to(&group), LinkSync::Adopted);
        assert_eq!(a.camera, b.camera);
    }

    #[test]
    fn the_first_member_to_sync_wins_a_same_frame_conflict() {
        let group = CameraLinkGroup::new(1);
        let mut a = ViewportInstance::new(1);
        let mut b = ViewportInstance::new(2);
        a.link_to(&group);
        b.link_to(&group);
        a.camera.yaw = 1.0;
        b.camera.yaw = -1.0;
        assert_eq!(a.sync_link(), Some(LinkSync::Published));
        assert_eq!(b.sync_link(), Some(LinkSync::Adopted));
        assert!((b.camera.yaw - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn instance_panes_keep_their_own_cameras() {
        let mut view = ViewportInstance::new(3);
        let mut top = view.pane_camera(1).1;
        top.target = Vec3::new(5.0, 0.0, 5.0);
        view.set_pane_camera(1, top);
        let mut persp = view.pane_camera(0).1;
        persp.yaw = 1.25;
        view.set_pane_camera(0, persp);
        assert!((view.camera.yaw - 1.25).abs() < f32::EPSILON);
        assert_eq!(view.layout.panes[1].camera.target, Vec3::new(5.0, 0.0, 5.0));
        assert_eq!(view.pane_camera(1).0, ViewKind::Top);
    }

    #[test]
    fn agent_edits_reach_the_instance_once() {
        let mut world = World::new();
        let mut view = ViewportInstance::new(9);
        ViewportInstances::publish(&mut world, &view);
        assert!(!ViewportInstances::take_agent_edits(&mut world, &mut view));
        ViewportInstances::edit(&mut world, 9, false, |v| v.camera.distance = 3.0).expect("known id");
        assert!(ViewportInstances::take_agent_edits(&mut world, &mut view));
        assert!((view.camera.distance - 3.0).abs() < f32::EPSILON);
        assert!(!ViewportInstances::take_agent_edits(&mut world, &mut view));
        assert!(ViewportInstances::edit(&mut world, 10, false, |_| {}).is_none());
        assert!(ViewportInstances::edit(&mut world, 10, true, |_| {}).is_some());
        assert!(ViewportInstances::remove(&mut world, 10));
        assert!(!ViewportInstances::remove(&mut world, 10));
    }

    #[test]
    fn liveness_counts_instances_drawn_this_or_last_frame() {
        let mut world = World::new();
        let mut a = ViewportInstance::new(1);
        let mut b = ViewportInstance::new(2);
        for (view, frame) in [(&mut a, 5), (&mut b, 5)] {
            let stats = ViewportInstances::begin_draw(&mut world, view, frame);
            ViewportInstances::end_draw(&mut world, view, frame, Vec::new(), stats);
        }
        assert!(ViewportInstances::shared_history(&world));
        let stats = ViewportInstances::begin_draw(&mut world, &mut a, 7);
        ViewportInstances::end_draw(&mut world, &a, 7, Vec::new(), stats);
        let registry = world.resource::<ViewportInstances>();
        assert!(registry.is_live(1) && !registry.is_live(2));
        assert_eq!(registry.live_views(), 1);
        assert!(!ViewportInstances::shared_history(&world));
    }
}
