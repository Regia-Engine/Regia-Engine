// SPDX-License-Identifier: MPL-2.0
//! Applies drained messages to the world (`specs/60` § 3.4 `Apply` and `MeshSwap`): spawns and
//! updates entities, keeps pose and channel data, runs delayed playout and swaps finished meshes.
//!
//! Messages were validated on the reader thread. Everything here that can still refuse a frame
//! (a handle, a kind, a vertex count) answers with a non-fatal wire error for that handle.

use std::f32::consts::PI;
use std::path::Path;
use std::sync::{Arc, Mutex};

use regia_animation::skeleton::LocalBind;
use regia_animation::{FeedPose, FeedPoseBuffer, FeedPoseTarget, Pose, Skeleton, SkinnedMesh};
use regia_core::id::Guid;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_core::tasks::AsyncComputePool;
use regia_ecs::prelude::*;
use regia_render::{
    Camera, CpuMaterial, DirectionalLight, LabMaterial, LabTexture, MeshRenderer, PointLight, SpotLight,
};
use regia_scene::prelude::{EntityGuid, FeedAttach, FeedOwned, Name, Transform};

use super::core::{ASSET_KINDS, FeedCore};
use super::inbox::{InboxEntry, merge_sections};
use super::mesh::{FeedMeshData, SlotInfo};
use super::net::lock;
use super::playout::{PlayoutBuffer, Sample};
use super::plugin::{
    BuildSlot, BuiltMesh, DccFeedClock, DccFeedRegistry, DccFeedServer, DccFeedWarning, FeedChannels,
    FeedClip, FeedItem, FeedItemChange, FeedItemChanged, FeedMeshPart, FeedSessionChanged, FeedSkeleton,
    FeedTexture, MAX_MESH_SWAP_BYTES, MAX_MESH_SWAPS_PER_FRAME, MeshState,
};
use super::registry::{ItemRecord, ItemState};
use super::session::SessionId;
use super::settings::{DccFeedSettings, Playout};
use super::wire::{
    CameraFrame, ClipPush, ErrorCode, FeedKind, FrameInfo, ItemDeclare, Lens, LightFrame, MeshUpdate,
    Message, PoseFrame, SkeletonStatic, TexturePush, TextureSource, TransformBlock, TransformFrame,
    WireError, section,
};

/// Range of a point light the sender left unbounded (0 on the wire), in meters.
const DEFAULT_LIGHT_RANGE_M: f32 = 100.0;
/// Playout channel of an item's placement.
const CHANNEL_PLACEMENT: u32 = 0;
/// Playout channel of a camera's lens values or a light's color and intensity.
const CHANNEL_VALUES: u32 = 1;
/// First playout channel of a skeleton's joints.
const CHANNEL_JOINTS: u32 = 2;
/// The PNG container id of `texture_push` (spec § 4.4).
const CONTAINER_PNG: u8 = 0;
/// The JPEG container id.
const CONTAINER_JPEG: u8 = 1;

/// The sender's coordinate space (spec § 2.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Space {
    /// True for glTF space: root placements take the importer's default 180 degree turn.
    pub gltf: bool,
    /// Multiplies translations, positions and lens distances.
    pub unit_scale: f32,
}

/// Local transform of a block. A root placement in glTF space takes the importer's `Convert`
/// turn about +Y, written out so it stays exact.
pub(super) fn placement(block: &TransformBlock, space: Space, root: bool) -> Transform {
    let mut translation = Vec3::from(block.translation) * space.unit_scale;
    let mut rotation = Quat::from_array(block.rotation).normalize();
    if space.gltf && root {
        translation = Vec3::new(-translation.x, translation.y, -translation.z);
        rotation = Quat::from_xyzw(0.0, 1.0, 0.0, 0.0) * rotation;
    }
    Transform {
        translation,
        rotation,
        scale: Vec3::from(block.scale),
    }
}

fn matrix(t: &Transform) -> Mat4 {
    Mat4::from_scale_rotation_translation(t.scale, t.rotation, t.translation)
}

/// The world matrix of `entity`, walking the parents' local transforms: `GlobalTransform` is only
/// propagated later in the frame.
fn world_matrix(world: &World, entity: Entity) -> Mat4 {
    let mut out = Mat4::IDENTITY;
    let mut cursor = Some(entity);
    let mut steps = 0;
    while let Some(e) = cursor {
        if let Some(t) = world.get::<Transform>(e) {
            out = matrix(t) * out;
        }
        cursor = world.get::<ChildOf>(e).map(ChildOf::parent);
        steps += 1;
        if steps > 1024 {
            break; // a cycle would be a bug elsewhere; never hang the frame on it
        }
    }
    out
}

fn wire_err(code: ErrorCode, message: impl Into<String>) -> WireError {
    WireError::new(code, message)
}

/// One tick's shared inputs.
struct Tick<'a> {
    core: &'a Mutex<FeedCore>,
    settings: &'a DccFeedSettings,
    now_s: f64,
    inline_builds: bool,
    project_dir: Option<&'a Path>,
}

impl Tick<'_> {
    fn refuse(&self, session: SessionId, seq: u32, handle: u32, err: &WireError) {
        lock(self.core).send_error(session, seq, handle, err);
    }
}

/// `DccFeedSet::Apply`.
pub(super) fn apply(world: &mut World) {
    let Some(server) = world.get_resource::<DccFeedServer>() else {
        return;
    };
    if !server.is_live() {
        return;
    }
    let _span = tracing::trace_span!("dcc_feed_apply").entered();
    let core = server.core().clone();
    let project_dir = server.project_dir().map(Path::to_path_buf);
    let settings = world.resource::<DccFeedSettings>().clone();
    let clock = *world.resource::<DccFeedClock>();
    let replaying = lock(&core).replay.is_some();
    world.resource_scope(|world, mut registry: Mut<DccFeedRegistry>| {
        let tick = Tick {
            core: &core,
            settings: &settings,
            now_s: clock.now_s,
            inline_builds: replaying || clock.fixed_step_s.is_some(),
            project_dir: project_dir.as_deref(),
        };
        let output = std::mem::take(&mut registry.pending);
        for session in &output.closed {
            for guid in registry.table.detach_session(*session) {
                set_state(
                    world,
                    &mut registry,
                    guid,
                    ItemState::Detached,
                    FeedItemChange::Detached,
                );
            }
            registry.anchors.remove(session);
        }
        for (session, offset) in &output.offsets {
            registry.anchors.insert(*session, *offset);
        }
        for entry in output.entries {
            let space = output.spaces.get(&entry.session).map_or(
                Space {
                    gltf: true,
                    unit_scale: 1.0,
                },
                |(gltf, unit_scale)| Space {
                    gltf: *gltf,
                    unit_scale: *unit_scale,
                },
            );
            apply_entry(world, &mut registry, &tick, entry, space);
        }
        if settings.playout == Playout::Delayed {
            run_playout(world, &mut registry, &tick);
        }
        let stall_after_s = f64::from(settings.stall_after_ms) / 1000.0;
        for guid in registry.table.update_stalls(clock.now_s, stall_after_s) {
            set_state(
                world,
                &mut registry,
                guid,
                ItemState::Stalled,
                FeedItemChange::Stalled,
            );
        }
        report_unmatched_joints(world, &core);
        publish_state(world, &mut registry, &core);
    });
    lock(&core).flush();
}

/// Sends session changes and warnings to the bevy messages.
fn publish_state(world: &mut World, registry: &mut DccFeedRegistry, core: &Mutex<FeedCore>) {
    let (changes, warnings) = {
        let mut c = lock(core);
        let changes: Vec<FeedSessionChanged> = c
            .sessions
            .iter()
            .filter_map(|s| {
                let from = registry.phases.get(&s.id).copied();
                (from != Some(s.phase)).then_some(FeedSessionChanged {
                    session: s.id,
                    from,
                    to: s.phase,
                })
            })
            .collect();
        let live: Vec<SessionId> = c.sessions.iter().map(|s| s.id).collect();
        registry.phases.retain(|id, _| live.contains(id));
        (changes, c.take_new_warnings())
    };
    for change in changes {
        registry.phases.insert(change.session, change.to);
        world.write_message(change);
    }
    for (session, code, text) in warnings {
        world.write_message(DccFeedWarning { session, code, text });
    }
}

fn report_unmatched_joints(world: &mut World, core: &Mutex<FeedCore>) {
    let Some(mut buffer) = world.get_resource_mut::<FeedPoseBuffer>() else {
        return;
    };
    let unmatched = buffer.take_unmatched();
    if unmatched.is_empty() {
        return;
    }
    let mut c = lock(core);
    for (item, joints) in unmatched {
        let names: Vec<String> = joints.into_iter().collect();
        c.warn(
            0,
            "joint_unmatched",
            format!("item {item:?}: no target joint for {}", names.join(", ")),
        );
    }
}

fn set_state(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    guid: Guid,
    state: ItemState,
    change: FeedItemChange,
) {
    let entity = registry.table.get(guid).and_then(|i| i.entity);
    if let Some(entity) = entity
        && let Some(mut item) = world.get_mut::<FeedItem>(entity)
    {
        item.state = state;
        if state == ItemState::Detached {
            item.session = None;
        }
    }
    world.write_message(FeedItemChanged { guid, entity, change });
}

fn apply_entry(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    entry: InboxEntry,
    space: Space,
) {
    let InboxEntry {
        session, seq, msg, ..
    } = entry;
    let result = match msg {
        Message::ItemDeclare(d) => declare(world, registry, tick, session, &d),
        Message::ItemDelete(handle) => delete(world, registry, tick, session, seq, handle),
        Message::Transform(t) => on_transform(world, registry, tick, session, seq, &t, space),
        Message::Camera(c) => on_camera(world, registry, tick, session, seq, &c, space),
        Message::Light(l) => on_light(world, registry, tick, session, seq, &l, space),
        Message::Mesh(m) => on_mesh(world, registry, tick, session, seq, m, space),
        Message::SkeletonStatic(s) => on_skeleton(registry, tick, session, seq, &s, space),
        Message::Pose(p) => on_pose(world, registry, tick, session, seq, &p, space),
        Message::TexturePush(t) => on_texture(registry, tick, t),
        Message::ClipPush(c) => {
            on_clip(registry, session, c);
            Ok(())
        }
        // Everything else was handled on the reader thread and never reaches the inbox.
        _ => Ok(()),
    };
    if let Err((handle, err)) = result {
        tick.refuse(session, seq, handle, &err);
    }
}

/// A refusal for `handle`.
type Applied = Result<(), (u32, WireError)>;

fn declare(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    d: &ItemDeclare,
) -> Applied {
    if d.kind == FeedKind::Mesh && !ASSET_KINDS {
        let err = wire_err(ErrorCode::KindNotSupported, "mesh items need the editor receiver");
        return Err((d.handle, err));
    }
    let limits = lock(tick.core).limits();
    let declared = registry
        .table
        .declare(session, d, &limits, tick.now_s)
        .map_err(|e| (d.handle, e))?;
    let Some(item) = registry.table.get(declared.guid).cloned() else {
        return Ok(());
    };
    let parent_entity = item
        .parent
        .and_then(|p| registry.table.get(p))
        .and_then(|p| p.entity);
    let name = if d.display_name.is_empty() {
        d.key.as_str()
    } else {
        d.display_name.as_str()
    };
    let entity = match item.entity.filter(|e| world.get_entity(*e).is_ok()) {
        Some(entity) => {
            let mut e = world.entity_mut(entity);
            e.insert(Name::new(name));
            if let Some(mut feed) = e.get_mut::<FeedItem>() {
                feed.session = Some(session);
                feed.state = item.state;
                feed.frames_applied = item.frames_applied;
                feed.last_frame_id = item.last_frame_id;
            }
            entity
        }
        None => {
            let feed = FeedItem {
                key: d.key.clone(),
                guid: declared.guid.as_u128(),
                kind: d.kind,
                session: Some(session),
                state: item.state,
                owned: true,
                frames_applied: 0,
                last_frame_id: 0,
                last_source_time_ns: 0,
            };
            let id = world
                .spawn((
                    Name::new(name),
                    EntityGuid(declared.guid),
                    Transform::default(),
                    FeedOwned { key: d.key.clone() },
                    feed,
                ))
                .id();
            if let Some(record) = registry.table.get_mut(declared.guid) {
                record.entity = Some(id);
            }
            id
        }
    };
    match parent_entity {
        Some(parent) if parent != entity => {
            world.entity_mut(entity).insert(ChildOf(parent));
        }
        _ => {
            world.entity_mut(entity).remove::<ChildOf>();
        }
    }
    if d.kind == FeedKind::Generic {
        world.entity_mut(entity).insert(FeedChannels {
            names: d.channels.clone(),
            values: vec![0.0; d.channels.len()],
        });
        if let Some(record) = registry.table.get_mut(declared.guid) {
            record.detail.channels = u32::try_from(d.channels.len()).unwrap_or(u32::MAX);
        }
    }
    if d.kind == FeedKind::Mesh {
        registry.meshes.entry(declared.guid).or_default();
    }
    attach_waiting_children(world, &d.key, entity);
    if declared.fresh {
        world.write_message(FeedItemChanged {
            guid: declared.guid,
            entity: Some(entity),
            change: FeedItemChange::Added,
        });
    }
    Ok(())
}

/// Re-parents user entities saved with a `FeedAttach` for `key`, keeping their local transforms.
fn attach_waiting_children(world: &mut World, key: &str, entity: Entity) {
    let mut query = world.query_filtered::<(Entity, &FeedAttach), Without<ChildOf>>();
    let waiting: Vec<Entity> = query
        .iter(&*world)
        .filter(|(_, attach)| attach.key == key)
        .map(|(e, _)| e)
        .collect();
    for child in waiting {
        world.entity_mut(child).insert(ChildOf(entity));
    }
}

/// A `FeedAttach` that just appeared on an entity with no parent yet.
type NewAttachment = (Added<FeedAttach>, Without<ChildOf>);

/// `PreUpdate`: a `FeedAttach` that just appeared (a level load) goes under its item, or waits at
/// the root with one warning per key (spec § 2.4 rule 3).
pub(super) fn resolve_attachments(
    mut commands: Commands,
    added: Query<(Entity, &FeedAttach), NewAttachment>,
    registry: Res<DccFeedRegistry>,
    mut warned: Local<std::collections::BTreeSet<String>>,
) {
    for (child, attach) in &added {
        let entity = registry
            .table
            .get(super::item_guid(&attach.key))
            .and_then(|item| item.entity);
        match entity {
            Some(entity) => {
                commands.entity(child).insert(ChildOf(entity));
            }
            None => {
                if warned.insert(attach.key.clone()) {
                    tracing::warn!(target: "regia_agent", "no DCC feed item {:?} yet; its children stay at the scene root", attach.key);
                }
            }
        }
    }
}

/// Despawns an item's entity. User children survive at the root and keep waiting for the key.
pub(super) fn despawn_item(world: &mut World, registry: &mut DccFeedRegistry, item: &ItemRecord) {
    if let Some(state) = registry.meshes.remove(&item.guid) {
        for part in state.parts.into_iter().chain(state.joints) {
            if world.get_entity(part).is_ok() {
                world.despawn(part);
            }
        }
    }
    registry.skeletons.remove(&item.guid);
    registry.buffers.retain(|(guid, _), _| *guid != item.guid);
    if let Some(mut poses) = world.get_resource_mut::<FeedPoseBuffer>() {
        poses.remove(&item.key);
    }
    let Some(entity) = item.entity.filter(|e| world.get_entity(*e).is_ok()) else {
        return;
    };
    let mut query = world.query_filtered::<(Entity, &ChildOf), Without<FeedOwned>>();
    let children: Vec<Entity> = query
        .iter(&*world)
        .filter(|(_, link)| link.parent() == entity)
        .map(|(e, _)| e)
        .collect();
    for child in children {
        world.entity_mut(child).remove::<ChildOf>().insert(FeedAttach {
            key: item.key.clone(),
        });
    }
    world.despawn(entity);
}

fn delete(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    handle: u32,
) -> Applied {
    let Some(item) = registry.table.delete(session, handle) else {
        lock(tick.core).send_unknown_item(session, seq, handle);
        return Ok(());
    };
    despawn_item(world, registry, &item);
    world.write_message(FeedItemChanged {
        guid: item.guid,
        entity: None,
        change: FeedItemChange::Removed,
    });
    Ok(())
}

/// The item a frame addresses, or `None` after the once-per-second `unknown_item`.
fn frame_target(
    registry: &DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    handle: u32,
) -> Option<ItemRecord> {
    let found = registry
        .table
        .resolve(session, handle)
        .ok()
        .and_then(|guid| registry.table.get(guid));
    if found.is_none() {
        lock(tick.core).send_unknown_item(session, seq, handle);
    }
    found.cloned()
}

/// Accepts a frame's info for `item` and mirrors the counters onto its entity. False: the frame
/// is out of order and was dropped.
fn accept_frame(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    guid: Guid,
    info: &FrameInfo,
) -> bool {
    if !registry.table.note_frame(guid, info, tick.now_s) {
        return false;
    }
    let Some(item) = registry.table.get(guid) else {
        return false;
    };
    let (entity, frames, id, source) = (
        item.entity,
        item.frames_applied,
        item.last_frame_id,
        item.last_source_time_ns,
    );
    if let Some(entity) = entity {
        if let Some(mut feed) = world.get_mut::<FeedItem>(entity) {
            feed.state = ItemState::Active;
            feed.frames_applied = frames;
            feed.last_frame_id = id;
            feed.last_source_time_ns = source;
        }
        world.write_message(FeedItemChanged {
            guid,
            entity: Some(entity),
            change: FeedItemChange::Updated,
        });
    }
    true
}

/// A world-space block expressed in the parent's space; any other block unchanged.
fn local_placement(
    world: &World,
    item: &ItemRecord,
    registry: &DccFeedRegistry,
    block: &TransformBlock,
    space: Space,
) -> Transform {
    let parent = item
        .parent
        .and_then(|p| registry.table.get(p))
        .and_then(|p| p.entity);
    let local = placement(block, space, parent.is_none() || block.world_space);
    match parent {
        Some(parent) if block.world_space => {
            let inverse = world_matrix(world, parent).inverse();
            let (scale, rotation, translation) = (inverse * matrix(&local)).to_scale_rotation_translation();
            Transform {
                translation,
                rotation: rotation.normalize(),
                scale,
            }
        }
        _ => local,
    }
}

/// Maps a sender time to the server timeline. Before the first ping the first frame of the
/// session anchors the mapping (spec § 4.6).
fn server_time(registry: &mut DccFeedRegistry, session: SessionId, info: &FrameInfo, now_s: f64) -> f64 {
    let source = info.source_time_ns as f64 / 1e9;
    let offset = *registry.anchors.entry(session).or_insert(now_s - source);
    source + offset
}

fn buffer<'a>(
    registry: &'a mut DccFeedRegistry,
    tick: &Tick<'_>,
    guid: Guid,
    channel: u32,
) -> &'a mut PlayoutBuffer {
    let cap = tick.settings.delay_queue_len as usize;
    let age = f64::from(tick.settings.max_frame_age_ms) / 1000.0;
    registry
        .buffers
        .entry((guid, channel))
        .or_insert_with(|| PlayoutBuffer::new(cap, age))
}

/// What a placement belongs to.
#[derive(Clone, Copy)]
struct Placed<'a> {
    session: SessionId,
    item: &'a ItemRecord,
    info: &'a FrameInfo,
}

/// Applies a placement now (`snap`) or queues it (`delayed`).
fn place(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    frame: Placed<'_>,
    block: &TransformBlock,
    space: Space,
) {
    let Placed { session, item, info } = frame;
    let local = local_placement(world, item, registry, block, space);
    if tick.settings.playout == Playout::Delayed {
        let time_s = server_time(registry, session, info, tick.now_s);
        let dropped = !buffer(registry, tick, item.guid, CHANNEL_PLACEMENT).push(Sample {
            frame_id: info.frame_id,
            time_s,
            t: local.translation.to_array(),
            r: local.rotation.to_array(),
            s: local.scale.to_array(),
        });
        if dropped {
            count_out_of_order(registry, item.guid);
        }
    } else if let Some(entity) = item.entity {
        world.entity_mut(entity).insert(local);
    }
}

fn count_out_of_order(registry: &mut DccFeedRegistry, guid: Guid) {
    if let Some(item) = registry.table.get_mut(guid) {
        item.dropped_out_of_order += 1;
    }
}

fn on_transform(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    t: &TransformFrame,
    space: Space,
) -> Applied {
    let Some(item) = frame_target(registry, tick, session, seq, t.handle) else {
        return Ok(());
    };
    if matches!(item.kind, FeedKind::Skeleton | FeedKind::Generic) {
        return Err((t.handle, wrong_kind(&item, "transform")));
    }
    if accept_frame(world, registry, tick, item.guid, &t.info) {
        let frame = Placed {
            session,
            item: &item,
            info: &t.info,
        };
        place(world, registry, tick, frame, &t.xf, space);
    }
    Ok(())
}

fn wrong_kind(item: &ItemRecord, message: &str) -> WireError {
    wire_err(
        ErrorCode::KindMismatch,
        format!(
            "item {:?} is a {} item: no {message} frames",
            item.key,
            item.kind.name()
        ),
    )
}

fn camera_from(previous: Option<&Camera>, lens: &Lens, unit_scale: f32) -> Camera {
    let mut camera = previous.cloned().unwrap_or_default();
    let (near, far) = match *lens {
        Lens::Perspective {
            yfov_rad, near, far, ..
        } => {
            camera.orthographic = false;
            camera.fov_y = yfov_rad;
            (near, far)
        }
        Lens::Orthographic { ymag, near, far, .. } => {
            camera.orthographic = true;
            camera.ortho_size = ymag * unit_scale;
            (near, far)
        }
    };
    camera.near = near * unit_scale;
    camera.far = if far > 0.0 {
        far * unit_scale
    } else {
        f32::INFINITY
    };
    camera
}

fn on_camera(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    c: &CameraFrame,
    space: Space,
) -> Applied {
    let Some(item) = frame_target(registry, tick, session, seq, c.handle) else {
        return Ok(());
    };
    if item.kind != FeedKind::Camera {
        return Err((c.handle, wrong_kind(&item, "camera")));
    }
    if !accept_frame(world, registry, tick, item.guid, &c.info) {
        return Ok(());
    }
    let frame = Placed {
        session,
        item: &item,
        info: &c.info,
    };
    place(world, registry, tick, frame, &c.xf, space);
    let Some(entity) = item.entity else { return Ok(()) };
    let camera = camera_from(world.get::<Camera>(entity), &c.lens, space.unit_scale);
    if tick.settings.playout == Playout::Delayed {
        // Lens values travel in the sample's translation slot: near, far (0 = infinite) and fov.
        let time_s = server_time(registry, session, &c.info, tick.now_s);
        let (lens_a, lens_b) = if camera.orthographic {
            (camera.ortho_size, 0.0)
        } else {
            (camera.fov_y, 0.0)
        };
        let far = if camera.far.is_finite() { camera.far } else { 0.0 };
        let dropped = !buffer(registry, tick, item.guid, CHANNEL_VALUES).push(Sample {
            frame_id: c.info.frame_id,
            time_s,
            t: [lens_a, camera.near, far],
            r: [0.0, 0.0, 0.0, 1.0],
            s: [lens_b, 0.0, 0.0],
        });
        if dropped {
            count_out_of_order(registry, item.guid);
        }
    }
    world.entity_mut(entity).insert(camera);
    if let Some(p) = c.physical {
        let mut physical = world
            .get::<regia_render::PhysicalCamera>(entity)
            .copied()
            .unwrap_or_default();
        physical.focal_distance = p.focus_m * space.unit_scale;
        physical.aperture_f_stops = p.f_stop;
        physical.sensor_height = p.sensor_h_mm / 1000.0;
        physical.focal_length = (p.focal_mm > 0.0).then_some(p.focal_mm / 1000.0);
        world.entity_mut(entity).insert(physical);
    }
    Ok(())
}

/// A light's streamed values.
#[derive(Clone, Copy)]
struct LightValues {
    color: [f32; 3],
    intensity: f32,
    range: f32,
    inner: f32,
    outer: f32,
}

/// Writes a light's values onto its entity. Point lights take lumens, so glTF candela become
/// `4 pi cd` (the flux of an isotropic source); directional lux and spot candela carry over.
fn write_light(world: &mut World, entity: Entity, variant: u8, v: LightValues) {
    let forward = (world_matrix(world, entity).to_scale_rotation_translation().1 * Vec3::NEG_Z).to_array();
    match variant {
        0 => {
            let mut light = world.get::<DirectionalLight>(entity).cloned().unwrap_or_default();
            light.direction = forward;
            light.color = v.color;
            light.illuminance = v.intensity;
            world.entity_mut(entity).insert(light);
        }
        1 => {
            let mut light = world.get::<PointLight>(entity).cloned().unwrap_or_default();
            light.color = v.color;
            light.intensity = v.intensity * 4.0 * PI;
            light.radius = if v.range > 0.0 {
                v.range
            } else {
                DEFAULT_LIGHT_RANGE_M
            };
            world.entity_mut(entity).insert(light);
        }
        _ => {
            let mut light = world.get::<SpotLight>(entity).cloned().unwrap_or_default();
            light.color = v.color;
            light.intensity = v.intensity;
            light.inner_angle = v.inner;
            light.outer_angle = v.outer;
            light.range = if v.range > 0.0 {
                v.range
            } else {
                DEFAULT_LIGHT_RANGE_M
            };
            world.entity_mut(entity).insert(light);
        }
    }
}

fn on_light(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    l: &LightFrame,
    space: Space,
) -> Applied {
    let Some(item) = frame_target(registry, tick, session, seq, l.handle) else {
        return Ok(());
    };
    if item.kind != FeedKind::Light {
        return Err((l.handle, wrong_kind(&item, "light")));
    }
    if !accept_frame(world, registry, tick, item.guid, &l.info) {
        return Ok(());
    }
    let frame = Placed {
        session,
        item: &item,
        info: &l.info,
    };
    place(world, registry, tick, frame, &l.xf, space);
    let Some(entity) = item.entity else { return Ok(()) };
    let range = l.range * space.unit_scale;
    if tick.settings.playout == Playout::Delayed {
        let time_s = server_time(registry, session, &l.info, tick.now_s);
        let dropped = !buffer(registry, tick, item.guid, CHANNEL_VALUES).push(Sample {
            frame_id: l.info.frame_id,
            time_s,
            t: l.color,
            r: [0.0, 0.0, 0.0, 1.0],
            s: [l.intensity, range, l.spot_inner_rad],
        });
        if dropped {
            count_out_of_order(registry, item.guid);
        }
    }
    let values = LightValues {
        color: l.color,
        intensity: l.intensity,
        range,
        inner: l.spot_inner_rad,
        outer: l.spot_outer_rad,
    };
    write_light(world, entity, item.variant, values);
    Ok(())
}

fn on_mesh(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    m: MeshUpdate,
    space: Space,
) -> Applied {
    let Some(item) = frame_target(registry, tick, session, seq, m.handle) else {
        return Ok(());
    };
    if item.kind != FeedKind::Mesh {
        return Err((m.handle, wrong_kind(&item, "mesh")));
    }
    let handle = m.handle;
    let Some(state) = registry.meshes.get_mut(&item.guid) else {
        return Ok(());
    };
    let new_expected = if m.update_mode != 0 {
        super::wire::check_vertex_only(&m, state.expected_vertices).map_err(|e| (handle, e))?;
        None
    } else {
        Some(
            m.sections
                .iter()
                .find(|s| s.section_id == section::POSITIONS)
                .map_or(0, |s| s.count),
        )
    };
    if !accept_frame(world, registry, tick, item.guid, &m.info) {
        return Ok(());
    }
    // Only an accepted full update changes the vertex count later updates are checked against;
    // a frame dropped as out of order must not (audit item 7).
    if let (Some(n), Some(state)) = (new_expected, registry.meshes.get_mut(&item.guid)) {
        state.expected_vertices = n;
    }
    let Some(state) = registry.meshes.get_mut(&item.guid) else {
        return Ok(());
    };
    if tick.inline_builds {
        let mut data = FeedMeshData::clone(&state.data);
        let _span = tracing::trace_span!("dcc_feed_mesh_build").entered();
        data.apply(&m, space.unit_scale).map_err(|e| (handle, e))?;
        let built = BuiltMesh::new(Arc::new(data));
        state.data = built.data.clone();
        state.ready = Some(built);
        return Ok(());
    }
    if state.building.is_some() {
        match &mut state.next {
            Some((queued, _)) if m.update_mode != 0 => merge_sections(queued, &m),
            slot => *slot = Some((m, space.unit_scale)),
        }
        return Ok(());
    }
    start_build(state, m, space.unit_scale);
    Ok(())
}

/// Builds on the task pool: a fixed-order pass, so the thread count cannot change the result.
fn start_build(state: &mut MeshState, m: MeshUpdate, unit_scale: f32) {
    let slot: BuildSlot = Arc::new(Mutex::new(None));
    state.building = Some(slot.clone());
    let base = state.data.clone();
    AsyncComputePool::get()
        .spawn(async move {
            let _span = tracing::trace_span!("dcc_feed_mesh_build").entered();
            // A panicking build still fills the slot, so `collect_builds` clears `building` and the
            // item's later updates can build again (audit-concurrency-1103 agent L2).
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut data = FeedMeshData::clone(&base);
                data.apply(&m, unit_scale)
                    .map(|()| BuiltMesh::new(Arc::new(data)))
            }))
            .unwrap_or_else(|_| Err(wire_err(ErrorCode::Internal, "the mesh build panicked")));
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        })
        .detach();
}

/// Collects finished builds, then swaps what fits in this frame's budget.
pub(super) fn mesh_swap(world: &mut World) {
    let Some(server) = world.get_resource::<DccFeedServer>() else {
        return;
    };
    if !server.is_live() {
        return;
    }
    let core = server.core().clone();
    world.resource_scope(|world, mut registry: Mut<DccFeedRegistry>| {
        collect_builds(&mut registry, &core);
        let _span = tracing::trace_span!("dcc_feed_mesh_swap").entered();
        let mut bytes = 0;
        let guids: Vec<Guid> = registry
            .meshes
            .iter()
            .filter(|(_, s)| s.ready.is_some())
            .map(|(g, _)| *g)
            .collect();
        for (swaps, guid) in guids.into_iter().enumerate() {
            let size = registry
                .meshes
                .get(&guid)
                .and_then(|s| s.ready.as_ref())
                .map_or(0, |built| built.data.byte_size());
            if swaps >= MAX_MESH_SWAPS_PER_FRAME || (swaps > 0 && bytes + size > MAX_MESH_SWAP_BYTES) {
                break;
            }
            bytes += size;
            swap_mesh(world, &mut registry, guid);
        }
    });
}

fn collect_builds(registry: &mut DccFeedRegistry, core: &Mutex<FeedCore>) {
    for (guid, state) in &mut registry.meshes {
        let Some(slot) = state.building.clone() else {
            continue;
        };
        let finished = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(result) = finished else { continue };
        state.building = None;
        match result {
            Ok(built) => {
                state.data = built.data.clone();
                state.ready = Some(built);
            }
            Err(err) => lock(core).warn(0, err.code(), format!("mesh build of {guid:?}: {}", err.message)),
        }
        if let Some((next, unit_scale)) = state.next.take() {
            start_build(state, next, unit_scale);
        }
    }
}

fn swap_mesh(world: &mut World, registry: &mut DccFeedRegistry, guid: Guid) {
    let Some(entity) = registry.table.get(guid).and_then(|i| i.entity) else {
        return;
    };
    let tick_textures = std::mem::take(&mut registry.textures);
    let Some(state) = registry.meshes.get_mut(&guid) else {
        registry.textures = tick_textures;
        return;
    };
    let Some(BuiltMesh { data, parts }) = state.ready.take() else {
        registry.textures = tick_textures;
        return;
    };
    let materials: Vec<Arc<CpuMaterial>> = data
        .slots
        .iter()
        .map(|slot| Arc::new(material_of(slot, &tick_textures)))
        .collect();
    state.materials = materials;
    let fallback = Arc::new(LabMaterial::default());
    let material_for = |slot: u32, materials: &[Arc<CpuMaterial>]| {
        materials
            .get(slot as usize)
            .cloned()
            .unwrap_or_else(|| fallback.clone())
    };
    let mut pieces = parts.into_iter();
    if let Some((slot, mesh)) = pieces.next() {
        let renderer = MeshRenderer::new(mesh, material_for(slot, &state.materials));
        world.entity_mut(entity).insert(renderer);
    }
    let extra = pieces.len();
    for (index, (slot, mesh)) in pieces.enumerate() {
        let renderer = MeshRenderer::new(mesh, material_for(slot, &state.materials));
        match state.parts.get(index).copied() {
            Some(part) if world.get_entity(part).is_ok() => {
                world.entity_mut(part).insert((renderer, FeedMeshPart { slot }));
            }
            _ => {
                let part = world
                    .spawn((
                        renderer,
                        FeedMeshPart { slot },
                        Transform::default(),
                        FeedOwned { key: String::new() },
                        ChildOf(entity),
                    ))
                    .id();
                state.parts.insert(index, part);
            }
        }
    }
    for part in state.parts.drain(extra..) {
        if world.get_entity(part).is_ok() {
            world.despawn(part);
        }
    }
    registry.textures = tick_textures;
    if let Some(item) = registry.table.get_mut(guid) {
        item.detail.vertices = data.vertex_count();
        item.detail.triangles = data.triangle_count();
    }
    if data.is_skinned() {
        bind_skin(world, registry, guid, entity);
    }
}

/// Binds a skinned mesh to the skeleton item it is parented under (spec § 4.5 ids 9 and 10): joint
/// entities under the mesh, a `SkinnedMesh` on it and its parts, and a `FeedPoseTarget` that lets
/// pose frames move them. A mesh with no skeleton yet draws rigid until the `skeleton_static` lands.
fn bind_skin(world: &mut World, registry: &mut DccFeedRegistry, guid: Guid, entity: Entity) {
    let Some(parent) = registry.table.get(guid).and_then(|i| i.parent) else {
        return;
    };
    let (Some(skeleton), Some(key)) = (
        registry.skeletons.get(&parent).map(|s| s.asset.clone()),
        registry.table.get(parent).map(|i| i.key.clone()),
    ) else {
        return;
    };
    let Some(state) = registry.meshes.get_mut(&guid) else {
        return;
    };
    if state.joints.len() != skeleton.len() {
        for joint in state.joints.drain(..) {
            if world.get_entity(joint).is_ok() {
                world.despawn(joint);
            }
        }
        for (i, bind) in skeleton.bind_pose.iter().enumerate() {
            let parent_entity = skeleton.parents[i]
                .and_then(|p| state.joints.get(p as usize).copied())
                .unwrap_or(entity);
            let joint = world
                .spawn((
                    Transform {
                        translation: bind.translation,
                        rotation: bind.rotation,
                        scale: bind.scale,
                    },
                    FeedOwned { key: String::new() },
                    ChildOf(parent_entity),
                ))
                .id();
            state.joints.push(joint);
        }
    }
    let skinned = SkinnedMesh::new(skeleton, state.joints.clone());
    for part in &state.parts {
        world.entity_mut(*part).insert(skinned.clone());
    }
    world
        .entity_mut(entity)
        .insert((skinned, FeedPoseTarget::new(key)));
}

fn texture_of(name: &str, textures: &std::collections::BTreeMap<String, FeedTexture>) -> Option<LabTexture> {
    textures.get(name).and_then(|t| t.decoded.as_deref()).cloned()
}

/// The live material of a slot, with the textures that decoded.
fn material_of(slot: &SlotInfo, textures: &std::collections::BTreeMap<String, FeedTexture>) -> LabMaterial {
    let mut material = LabMaterial {
        base_color: slot.base_color,
        metallic: slot.metallic,
        roughness: slot.roughness,
        emissive: slot.emissive,
        emissive_intensity: 1.0,
        ..LabMaterial::default()
    };
    material.albedo = texture_of(&slot.textures[0], textures);
    material.normal_map = texture_of(&slot.textures[1], textures);
    if material.normal_map.is_some() {
        material.normal_scale = 1.0;
    }
    material.emissive_map = texture_of(&slot.textures[3], textures);
    material
}

/// The skeleton asset of a `skeleton_static`: bind pose, and inverse bind matrices from it (joints
/// arrive parents first, spec § 4.4).
fn skeleton_asset(s: &SkeletonStatic, unit_scale: f32) -> Skeleton {
    let mut asset = Skeleton::default();
    let mut globals: Vec<Mat4> = Vec::with_capacity(s.joints.len());
    for (i, joint) in s.joints.iter().enumerate() {
        let parent = u32::try_from(joint.parent).ok().filter(|p| (*p as usize) < i);
        let bind = LocalBind {
            translation: Vec3::from(joint.t) * unit_scale,
            rotation: Quat::from_array(joint.r).normalize(),
            scale: Vec3::from(joint.s),
        };
        let local = Mat4::from_scale_rotation_translation(bind.scale, bind.rotation, bind.translation);
        let global = parent.map_or(local, |p| globals[p as usize] * local);
        globals.push(global);
        asset.joint_names.push(joint.name.clone());
        asset.parents.push(parent);
        asset.bind_pose.push(bind);
        asset.inverse_bind_matrices.push(global.inverse());
    }
    asset
}

fn on_skeleton(
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    s: &SkeletonStatic,
    space: Space,
) -> Applied {
    let Some(item) = frame_target(registry, tick, session, seq, s.handle) else {
        return Ok(());
    };
    if item.kind != FeedKind::Skeleton {
        return Err((s.handle, wrong_kind(&item, "skeleton_static")));
    }
    let names: Vec<String> = s.joints.iter().map(|j| j.name.clone()).collect();
    registry.skeletons.insert(
        item.guid,
        FeedSkeleton {
            names: Arc::new(names),
            asset: Arc::new(skeleton_asset(s, space.unit_scale)),
        },
    );
    if let Some(record) = registry.table.get_mut(item.guid) {
        record.detail.joints = u32::try_from(s.joints.len()).unwrap_or(u32::MAX);
    }
    // A mesh that arrived first swaps again, now with its skeleton.
    let children: Vec<Guid> = registry
        .table
        .iter()
        .filter(|i| i.parent == Some(item.guid) && i.kind == FeedKind::Mesh)
        .map(|i| i.guid)
        .collect();
    for child in children {
        if let Some(state) = registry.meshes.get_mut(&child) {
            state.ready = Some(BuiltMesh::new(state.data.clone()));
        }
    }
    Ok(())
}

fn on_pose(
    world: &mut World,
    registry: &mut DccFeedRegistry,
    tick: &Tick<'_>,
    session: SessionId,
    seq: u32,
    p: &PoseFrame,
    space: Space,
) -> Applied {
    let Some(item) = frame_target(registry, tick, session, seq, p.handle) else {
        return Ok(());
    };
    let wanted = match (item.kind, p.layout) {
        (FeedKind::Skeleton, 0) => item.detail.joints,
        (FeedKind::Generic, 1) => item.detail.channels,
        _ => return Err((p.handle, wrong_kind(&item, "this pose layout's"))),
    };
    if u32::from(p.count) != wanted {
        let err = wire_err(
            ErrorCode::InvalidValue,
            format!("pose has {} values, the item declared {wanted}", p.count),
        );
        return Err((p.handle, err));
    }
    if !accept_frame(world, registry, tick, item.guid, &p.info) {
        return Ok(());
    }
    if item.kind == FeedKind::Generic {
        if let Some(mut channels) = item.entity.and_then(|e| world.get_mut::<FeedChannels>(e)) {
            channels.values.clone_from(&p.values);
        }
        return Ok(());
    }
    let joints = usize::from(p.count);
    let mut pose = Pose::from_identity(joints);
    for (i, chunk) in p.values.as_chunks::<10>().0.iter().enumerate() {
        pose.translations[i] = Vec3::new(chunk[0], chunk[1], chunk[2]) * space.unit_scale;
        pose.rotations[i] = Quat::from_xyzw(chunk[3], chunk[4], chunk[5], chunk[6]).normalize();
        pose.scales[i] = Vec3::new(chunk[7], chunk[8], chunk[9]);
    }
    if tick.settings.playout == Playout::Delayed {
        let time_s = server_time(registry, session, &p.info, tick.now_s);
        for i in 0..joints {
            let dropped = !buffer(registry, tick, item.guid, CHANNEL_JOINTS + i as u32).push(Sample {
                frame_id: p.info.frame_id,
                time_s,
                t: pose.translations[i].to_array(),
                r: pose.rotations[i].to_array(),
                s: pose.scales[i].to_array(),
            });
            if dropped {
                count_out_of_order(registry, item.guid);
                break;
            }
        }
        return Ok(());
    }
    store_pose(world, registry, &item, pose);
    Ok(())
}

fn store_pose(world: &mut World, registry: &DccFeedRegistry, item: &ItemRecord, pose: Pose) {
    let Some(joints) = registry.skeletons.get(&item.guid).map(|s| s.names.clone()) else {
        return;
    };
    if let Some(mut poses) = world.get_resource_mut::<FeedPoseBuffer>() {
        poses.set(item.key.clone(), FeedPose { joints, pose });
    }
}

/// Evaluates every playout buffer `playout_delay_ms` behind the clock (spec § 2.6).
fn run_playout(world: &mut World, registry: &mut DccFeedRegistry, tick: &Tick<'_>) {
    let at = tick.now_s - f64::from(tick.settings.playout_delay_ms) / 1000.0;
    let keys: Vec<(Guid, u32)> = registry.buffers.keys().copied().collect();
    let mut joints: std::collections::BTreeMap<Guid, Vec<Sample>> = std::collections::BTreeMap::new();
    for key in keys {
        let (guid, channel) = key;
        let Some(sample) = registry.buffers.get_mut(&key).and_then(|b| {
            b.evict(tick.now_s);
            b.evaluate(at)
        }) else {
            continue;
        };
        let Some(item) = registry.table.get(guid).cloned() else {
            continue;
        };
        match (channel, item.entity) {
            (CHANNEL_PLACEMENT, Some(entity)) => {
                world.entity_mut(entity).insert(Transform {
                    translation: Vec3::from(sample.t),
                    rotation: Quat::from_array(sample.r),
                    scale: Vec3::from(sample.s),
                });
            }
            (CHANNEL_VALUES, Some(entity)) => write_values(world, entity, &item, &sample),
            (c, _) if c >= CHANNEL_JOINTS => joints.entry(guid).or_default().push(sample),
            _ => {}
        }
    }
    for (guid, samples) in joints {
        let Some(item) = registry.table.get(guid).cloned() else {
            continue;
        };
        let mut pose = Pose::from_identity(samples.len());
        for (i, s) in samples.iter().enumerate() {
            pose.translations[i] = Vec3::from(s.t);
            pose.rotations[i] = Quat::from_array(s.r);
            pose.scales[i] = Vec3::from(s.s);
        }
        store_pose(world, registry, &item, pose);
    }
}

/// Writes interpolated lens or light values (see `on_camera` and `on_light` for the slots).
fn write_values(world: &mut World, entity: Entity, item: &ItemRecord, s: &Sample) {
    match item.kind {
        FeedKind::Camera => {
            if let Some(mut camera) = world.get_mut::<Camera>(entity) {
                if camera.orthographic {
                    camera.ortho_size = s.t[0];
                } else {
                    camera.fov_y = s.t[0];
                }
                camera.near = s.t[1];
                camera.far = if s.t[2] > 0.0 { s.t[2] } else { f32::INFINITY };
            }
        }
        FeedKind::Light => {
            let outer = world.get::<SpotLight>(entity).map_or(0.0, |l| l.outer_angle);
            let values = LightValues {
                color: s.t,
                intensity: s.s[0],
                range: s.s[1],
                inner: s.s[2],
                outer,
            };
            write_light(world, entity, item.variant, values);
        }
        _ => {}
    }
}

/// Reads `texture_push` into the transient store. A path must resolve inside the project or
/// `.regia/dcc_feed/inbox/` after symlink resolution (spec § 4.8), and is only opened here, on a
/// session already `active`.
fn on_texture(registry: &mut DccFeedRegistry, tick: &Tick<'_>, t: TexturePush) -> Applied {
    let limits = lock(tick.core).limits();
    let bytes_cap = limits.max_texture_bytes as usize;
    let (encoded, decoded, bytes) = match t.source {
        TextureSource::Path(path) => {
            let resolved = resolve_texture_path(tick.project_dir, &path).map_err(|e| (0, e))?;
            let data = read_capped(&resolved, bytes_cap).map_err(|e| (0, e))?;
            let container = if path.to_ascii_lowercase().ends_with(".png") {
                CONTAINER_PNG
            } else {
                CONTAINER_JPEG
            };
            let decoded = (container == CONTAINER_PNG)
                .then(|| decode_png(&data, limits.max_texture_dim, limits.max_transient_texture_bytes))
                .flatten();
            let n = charged_bytes(data.len(), decoded.as_ref());
            (Some((container, Arc::new(data))), decoded, n)
        }
        TextureSource::Encoded { container, bytes } => {
            let decoded = (container == CONTAINER_PNG)
                .then(|| decode_png(&bytes, limits.max_texture_dim, limits.max_transient_texture_bytes))
                .flatten();
            let n = charged_bytes(bytes.len(), decoded.as_ref());
            (Some((container, Arc::new(bytes))), decoded, n)
        }
        TextureSource::Raw {
            pixel_format,
            width,
            height,
            bytes,
        } => {
            let n = bytes.len() as u64;
            let decoded = (pixel_format == 0).then_some(LabTexture {
                width,
                height,
                rgba: bytes,
            });
            (None, decoded, n)
        }
    };
    let last_used = lock(tick.core).tick;
    registry.textures.insert(
        t.name,
        FeedTexture {
            colorspace: t.colorspace,
            encoded,
            decoded: decoded.map(Arc::new),
            bytes,
            last_used,
        },
    );
    evict_textures(registry, limits.max_transient_texture_bytes);
    Ok(())
}

/// Most transient textures kept at once, whatever their byte cost (zero-byte pushes included).
const MAX_TRANSIENT_TEXTURES: usize = 4096;

/// Drops the least recently pushed textures above `cap` bytes or above [`MAX_TRANSIENT_TEXTURES`].
fn evict_textures(registry: &mut DccFeedRegistry, cap: u64) {
    let mut total: u64 = registry.textures.values().map(|t| t.bytes).sum();
    while total > cap || registry.textures.len() > MAX_TRANSIENT_TEXTURES {
        let Some(oldest) = registry
            .textures
            .iter()
            .min_by_key(|(_, t)| t.last_used)
            .map(|(name, _)| name.clone())
        else {
            return;
        };
        if let Some(removed) = registry.textures.remove(&oldest) {
            total = total.saturating_sub(removed.bytes);
        }
    }
}

fn path_denied(path: &str) -> WireError {
    wire_err(
        ErrorCode::PathDenied,
        format!("{path:?} is not inside the project or the feed inbox"),
    )
}

/// Resolves a pushed path, relative to the project or absolute (what a DCC usually sends). After
/// symlinks it must lie in the project or the feed inbox (spec § 4.8).
fn resolve_texture_path(project: Option<&Path>, path: &str) -> Result<std::path::PathBuf, WireError> {
    let Some(project) = project else {
        return Err(path_denied(path));
    };
    let roots = [project.to_path_buf(), super::net::feed_dir(project).join("inbox")];
    let resolved = regia_core::project_path::resolve_in_roots(&roots, path).map_err(|_| path_denied(path))?;
    if resolved.is_file() {
        Ok(resolved)
    } else {
        Err(path_denied(path))
    }
}

fn read_capped(path: &Path, cap: usize) -> Result<Vec<u8>, WireError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| wire_err(ErrorCode::PathDenied, e.to_string()))?;
    let mut data = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| wire_err(ErrorCode::PathDenied, e.to_string()))?;
    if data.len() > cap {
        return Err(wire_err(
            ErrorCode::LimitExceeded,
            "texture above max_texture_bytes",
        ));
    }
    Ok(data)
}

/// Bytes a texture holds on the heap: the encoded copy plus the decoded RGBA pixels. This is what
/// `max_transient_texture_bytes` is charged, so a tiny PNG that decodes to a huge image still counts.
fn charged_bytes(encoded_len: usize, decoded: Option<&LabTexture>) -> u64 {
    (encoded_len as u64).saturating_add(decoded.map_or(0, |d| d.rgba.len() as u64))
}

/// Decodes 8-bit RGB or RGBA PNG; anything else stays undecoded (a bake still embeds it). The size
/// is checked from the header, against `max_dim` per edge and `max_decoded_bytes` for the decoded
/// RGBA, before pixels are allocated.
fn decode_png(bytes: &[u8], max_dim: u32, max_decoded_bytes: u64) -> Option<LabTexture> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    if width > max_dim || height > max_dim {
        return None;
    }
    // The RGBA result is `w * h * 4`; refuse before the (possibly larger) decode buffer exists.
    let rgba_len = u64::from(width) * u64::from(height) * 4;
    if rgba_len > max_decoded_bytes || reader.output_buffer_size()? as u64 > max_decoded_bytes {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let data = buf.get(..frame.buffer_size())?;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    Some(LabTexture { width, height, rgba })
}

fn on_clip(registry: &mut DccFeedRegistry, session: SessionId, c: ClipPush) {
    let bound = registry.table.resolve(session, c.bind_handle).ok();
    registry.clips.insert(c.name.clone(), FeedClip { clip: c, bound });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmatched_joint_warning_does_not_change_feed_item() {
        let mut app = regia_app::App::new();
        app.init_resource::<FeedPoseBuffer>();
        let skeleton = Arc::new(
            Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
                .expect("skeleton"),
        );
        let joint = app.world_mut().spawn(Transform::default()).id();
        app.world_mut().spawn((
            SkinnedMesh::new(skeleton, vec![joint]),
            FeedPoseTarget::new("arm"),
        ));
        let before = FeedItem {
            key: "arm".into(),
            guid: 7,
            kind: FeedKind::Skeleton,
            session: Some(0),
            state: ItemState::Active,
            owned: true,
            frames_applied: 4,
            last_frame_id: 9,
            last_source_time_ns: 11,
        };
        let entity = app.world_mut().spawn(before.clone()).id();
        app.world_mut().resource_mut::<FeedPoseBuffer>().set(
            "arm",
            FeedPose {
                joints: Arc::new(vec!["root".into(), "Tail".into()]),
                pose: Pose::from_identity(2),
            },
        );
        app.add_systems(regia_app::Update, regia_animation::live::apply_feed_pose_targets);
        app.update();
        let core = Arc::new(Mutex::new(FeedCore::new(&DccFeedSettings::default(), "token")));
        report_unmatched_joints(app.world_mut(), &core);
        let warnings = lock(&core).take_new_warnings();
        assert!(
            warnings
                .iter()
                .any(|(_, code, text)| code == "joint_unmatched" && text.contains("Tail")),
            "{warnings:?}"
        );
        let stored = app.world().get::<FeedItem>(entity).expect("feed item stays");
        assert_eq!(stored, &before);
    }

    #[test]
    fn gltf_root_placement_takes_the_exact_forward_turn() {
        let block = TransformBlock {
            translation: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            world_space: false,
        };
        let gltf = Space {
            gltf: true,
            unit_scale: 2.0,
        };
        let t = placement(&block, gltf, true);
        assert_eq!(t.translation, Vec3::new(-2.0, 4.0, -6.0));
        assert_eq!(t.rotation, Quat::from_xyzw(0.0, 1.0, 0.0, 0.0));
        let child = placement(&block, gltf, false);
        assert_eq!(
            child.translation,
            Vec3::new(2.0, 4.0, 6.0),
            "a child stays parent-local"
        );
        let native = placement(
            &block,
            Space {
                gltf: false,
                unit_scale: 1.0,
            },
            true,
        );
        assert_eq!(native.translation, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn texture_paths_stay_in_the_project() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_tex_path");
        std::fs::write(dir.join("a.png"), b"x").expect("write");
        assert!(resolve_texture_path(Some(&dir), "a.png").is_ok());
        let absolute = dir.join("a.png");
        assert!(resolve_texture_path(Some(&dir), absolute.to_str().expect("utf-8")).is_ok());
        let inbox = super::super::net::feed_dir(&dir).join("inbox");
        std::fs::create_dir_all(&inbox).expect("inbox");
        std::fs::write(inbox.join("b.png"), b"x").expect("write");
        let pushed = inbox.join("b.png");
        assert!(resolve_texture_path(Some(&dir), pushed.to_str().expect("utf-8")).is_ok());
        for bad in ["../../etc/passwd", "/etc/passwd", "missing.png", "C:/Windows/x"] {
            assert_eq!(
                resolve_texture_path(Some(&dir), bad).expect_err(bad).code(),
                "path_denied"
            );
        }
        assert_eq!(
            resolve_texture_path(None, "a.png")
                .expect_err("no project")
                .code(),
            "path_denied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_textures_evict_the_oldest_above_the_cap() {
        let mut registry = DccFeedRegistry::default();
        for (name, last_used) in [("old", 1), ("new", 2)] {
            registry.textures.insert(
                name.to_owned(),
                FeedTexture {
                    colorspace: 0,
                    encoded: None,
                    decoded: None,
                    bytes: 10,
                    last_used,
                },
            );
        }
        evict_textures(&mut registry, 15);
        assert!(registry.textures.contains_key("new") && !registry.textures.contains_key("old"));
    }

    fn solid_png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("header");
        writer
            .write_image_data(&vec![7u8; (w * h * 4) as usize])
            .expect("data");
        writer.finish().expect("finish");
        out
    }

    #[test]
    fn decode_png_refuses_images_whose_pixels_exceed_the_decode_cap() {
        let png = solid_png(64, 64); // 16 KiB decoded, a few dozen bytes encoded
        assert!(
            decode_png(&png, 8192, 1000).is_none(),
            "decoded size over the cap"
        );
        let ok = decode_png(&png, 8192, 64 * 64 * 4).expect("fits");
        assert_eq!(ok.rgba.len(), 64 * 64 * 4);
    }

    #[test]
    fn decoded_pixels_count_against_the_transient_budget() {
        let png = solid_png(64, 64);
        let tex = decode_png(&png, 8192, u64::MAX);
        let charged = charged_bytes(png.len(), tex.as_ref());
        assert_eq!(charged, png.len() as u64 + 64 * 64 * 4);
        // 200 distinct small-encoded textures no longer fit a 1 MiB budget.
        let mut registry = DccFeedRegistry::default();
        for i in 0..200u64 {
            registry.textures.insert(
                format!("t{i}"),
                FeedTexture {
                    colorspace: 0,
                    encoded: None,
                    decoded: None,
                    bytes: charged,
                    last_used: i,
                },
            );
        }
        evict_textures(&mut registry, 1 << 20);
        assert!(registry.textures.len() as u64 * charged <= 1 << 20);
    }

    #[test]
    fn transient_texture_count_is_capped() {
        let mut registry = DccFeedRegistry::default();
        for i in 0..(MAX_TRANSIENT_TEXTURES as u64 + 10) {
            registry.textures.insert(
                format!("t{i}"),
                FeedTexture {
                    colorspace: 0,
                    encoded: None,
                    decoded: None,
                    bytes: 0,
                    last_used: i,
                },
            );
        }
        evict_textures(&mut registry, u64::MAX);
        assert_eq!(registry.textures.len(), MAX_TRANSIENT_TEXTURES);
        assert!(!registry.textures.contains_key("t0") && registry.textures.contains_key("t4105"));
    }
}
