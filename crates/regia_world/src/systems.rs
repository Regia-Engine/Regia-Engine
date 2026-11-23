// SPDX-License-Identifier: MPL-2.0
//! The systems of the streaming frame order (spec 78 § 4.3). Thin: each one gathers what it needs
//! from the world and calls `WorldStreaming`.

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer};
use regia_core::math::{Affine3A, Vec3, Vec3A};
use regia_ecs::ecs::entity::EntityHashSet;
use regia_platform::{AppLifecycle, LifecycleEvent};
use regia_scene::Transform;
use regia_scene::components::EntityGuid;
use regia_scene::floating_origin::LargePosition;
use regia_scene::level::LevelUnloading;
use regia_scene::stream_grid::CellCoord;
use regia_scene::stream_plan::{SourceId, SourceView};
use regia_scene::streaming::{
    CellMember, CellRenderHidden, HlodCut, HlodCutEntry, HlodFade, HlodProxyMesh, HlodProxyResidency,
    HlodProxyVoxels, HlodVoxelResidency, PrimaryStreamView, StreamAutoMember, StreamFrozen, StreamGates,
    StreamGuarded, StreamTeleported, StreamViews, StreamingSource,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::hlod_fade::Drawable;
use crate::origin::WorldOrigin;
use crate::runtime::{MAX_STREAM_SOURCES, StreamTiming, WorldStreaming};

/// Speed clamp of the look-ahead point, m/s (spec 78 § 7.3).
const MAX_PREDICT_SPEED: f64 = 200.0;

/// The walk bound of [`step_pose`]: a `ChildOf` chain this deep is malformed. The bound backstops
/// the walk; a cycle is found by the visit check and returns no pose long before it.
const MAX_PARENT_DEPTH: usize = 1024;

/// Inline capacity of [`step_pose`]'s cycle check: the visited list lives on the stack for chains
/// this deep, and spills to a hash set beyond, so each level costs at most this many compares plus
/// one set lookup and the check stays linear at any depth.
const POSE_VISIT_INLINE: usize = 32;

// How many visited entries `step_pose`'s cycle check compared on this thread (a set lookup
// counts as one).
#[cfg(test)]
thread_local! {
    static POSE_VISIT_PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times the guard collected descendants. Stationary members must not add to it.
#[cfg(test)]
static DESCENDANT_WALKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run condition: a streamed level is open.
pub(crate) fn streaming_active(streaming: Res<WorldStreaming>) -> bool {
    streaming.is_active()
}

/// The world-space pose of an entity computed from this step's `Transform`s: the product of the
/// local `Transform` and every ancestor's, up to the root. `GlobalTransform` lags a changed
/// `Transform` until the `PostUpdate` propagation, and `FixedUpdate` may run several steps in one
/// frame, so the streaming sampling must not read it (spec 78 § 15.1: views are sampled from
/// simulation state at the step boundary).
///
/// Returns `None` when the parent chain cycles: a `ChildOf` cycle has no root, so — as with
/// propagation, which never visits a cycle — the entity has no world pose, and streaming must not
/// sample the garbage a capped walk would accumulate.
fn step_pose(transform: &Transform, parent: Option<&ChildOf>, parents: &ParentChain) -> Option<Affine3A> {
    let mut pose = transform.compute_affine();
    let mut next = parent.map(|link| link.parent());
    let mut visited = [Entity::PLACEHOLDER; POSE_VISIT_INLINE];
    let mut visited_len = 0usize;
    // Allocates only once a chain passes the inline capacity.
    let mut spilled = EntityHashSet::default();
    let mut depth = 0;
    while let Some(entity) = next {
        if depth >= MAX_PARENT_DEPTH {
            break;
        }
        #[cfg(test)]
        POSE_VISIT_PROBES.with(|p| p.set(p.get() + (visited_len + usize::from(!spilled.is_empty())) as u64));
        if visited[..visited_len].contains(&entity) || spilled.contains(&entity) {
            return None;
        }
        if visited_len < POSE_VISIT_INLINE {
            visited[visited_len] = entity;
            visited_len += 1;
        } else {
            spilled.insert(entity);
        }
        depth += 1;
        let Ok((parent_transform, grandparent)) = parents.get(entity) else {
            break;
        };
        if let Some(parent_transform) = parent_transform {
            pose = parent_transform.compute_affine() * pose;
        }
        next = grandparent.map(|link| link.parent());
    }
    Some(pose)
}

/// What one source contributes to the views.
type SourceData = (
    Entity,
    Option<&'static EntityGuid>,
    &'static StreamingSource,
    &'static Transform,
    Option<&'static ChildOf>,
);

/// What the guard reads of one body.
type GuardedBody = (
    Entity,
    &'static Transform,
    Option<&'static ChildOf>,
    Option<&'static CellMember>,
    Has<StreamFrozen>,
    Has<StreamGuarded>,
    Has<StreamingSource>,
    Has<StreamAutoMember>,
);

/// The bodies the guard walks: frozen candidates and members that follow their position.
type GuardedFilter = Or<(With<StreamGuarded>, With<StreamAutoMember>)>;

/// The parent-chain lookup of [`step_pose`]: every entity's `Transform` and parent link.
type ParentChain<'w, 's> = Query<'w, 's, (Option<&'static Transform>, Option<&'static ChildOf>)>;

/// `StreamSet::Sources`: samples every enabled source into [`StreamViews`] (spec 78 § 7.3).
pub(crate) fn sample_sources(
    mut streaming: ResMut<WorldStreaming>,
    origin: Res<WorldOrigin>,
    primary: Option<Res<PrimaryStreamView>>,
    time: Res<Time>,
    sources: Query<SourceData>,
    parents: ParentChain,
    mut views: ResMut<StreamViews>,
) {
    let _span = tracing::trace_span!("stream_sources").entered();
    let dt = f64::from(time.delta_secs());
    let mut sampled: Vec<(SourceId, &StreamingSource, SourceView)> = Vec::new();
    for (entity, guid, source, transform, parent) in &sources {
        if !source.enabled {
            continue;
        }
        let id = guid.map_or(SourceId::Entity(entity.to_bits()), |g| {
            SourceId::Guid(g.0.as_u128())
        });
        let Some(pose) = step_pose(transform, parent, &parents) else {
            // A parent cycle has no world pose; the source contributes no view this step.
            continue;
        };
        let pos = origin.to_world(Vec3::from(pose.translation).to_array());
        // One non-finite transform must not stall the plan for every other source.
        if pos.iter().any(|v| !v.is_finite()) {
            continue;
        }
        let dir = (source.kind == regia_scene::streaming::SourceKind::Camera)
            .then(|| Vec3::from(pose.matrix3 * Vec3A::NEG_Z).to_array());
        sampled.push((
            id,
            source,
            SourceView {
                source: id,
                pos,
                dir,
                radius_scale: source.radius_scale.clamp(0.1, 8.0),
                priority: source.priority,
                view_bias: source.view_bias.clamp(0.0, 1.0),
            },
        ));
    }
    sampled.sort_by_key(|(id, _, _)| *id);
    streaming.note_enabled_entity_sources(sampled.len());
    let pin_views = streaming.pin_views();
    let room = MAX_STREAM_SOURCES.saturating_sub(pin_views.len());
    if sampled.len() > room {
        let dropped = (sampled.len() - room) as u64;
        streaming.stats_mut().sources_dropped += dropped;
        tracing::warn!(target: "regia_world", "stream_source_limit: dropping {dropped} sources");
        sampled.truncate(room);
    }

    let mut out = Vec::with_capacity(sampled.len() * 2 + pin_views.len());
    let mut focus = None;
    let mut first_camera = None;
    // Look-ahead sources only add to what is wanted (a cutscene's next stretch of world).
    let mut first_real = None;
    for (id, source, view) in &sampled {
        out.push(*view);
        if !source.look_ahead {
            first_real = first_real.or(Some(view.pos));
            if source.drives_origin && focus.is_none() {
                focus = Some(view.pos);
            }
            if source.kind == regia_scene::streaming::SourceKind::Camera && first_camera.is_none() {
                first_camera = Some(view.pos);
            }
        }
        // Look-ahead: position change over the last step, clamped, times the prediction time.
        let predict = f64::from(source.predict_s.clamp(0.0, 4.0));
        if predict > 0.0
            && dt > 0.0
            && let Some(last) = streaming.last_point(*id)
        {
            let v: [f64; 3] = std::array::from_fn(|a| (view.pos[a] - last[a]) / dt);
            let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            let scale = if speed > MAX_PREDICT_SPEED {
                MAX_PREDICT_SPEED / speed
            } else {
                1.0
            };
            out.push(SourceView {
                pos: std::array::from_fn(|a| view.pos[a] + v[a] * scale * predict),
                ..*view
            });
        }
    }
    let focus = focus.or(first_camera);
    let first_real = first_real.or_else(|| pin_views.first().map(|v| v.pos));
    out.extend(pin_views);
    let implicit = first_real
        .is_none()
        .then(|| primary.and_then(|p| p.view))
        .flatten()
        .map(|(pos, dir)| SourceView {
            dir: Some(dir),
            view_bias: 0.5,
            ..SourceView::at(SourceId::Implicit, origin.to_world(pos))
        });
    out.extend(implicit);
    let focus = focus.or(first_real).or(implicit.map(|v| v.pos));
    streaming.set_focus(focus);
    views.0 = out;
}

/// `StreamSet::Plan`: the chunk check needs the asset server, so it is read here.
pub(crate) fn plan(
    mut streaming: ResMut<WorldStreaming>,
    views: Res<StreamViews>,
    assets: Option<Res<AssetServer>>,
) {
    let _span = tracing::info_span!("stream_plan_step").entered();
    let assets = assets.as_deref();
    let unavailable = |id: AssetId| assets.and_then(|a| a.unavailable_chunk(id).map(str::to_owned));
    streaming.plan_step(&views.0, &unavailable);
}

/// `StreamSet::Complete` in `PreUpdate`: async timing.
pub(crate) fn complete_frame(mut streaming: ResMut<WorldStreaming>) {
    if !streaming.timing().in_fixed_step() {
        let _span = tracing::trace_span!("stream_complete").entered();
        streaming.complete_async();
    }
}

/// `StreamSet::Complete` in `FixedUpdate`: lockstep and replay timing.
pub(crate) fn complete_fixed(mut streaming: ResMut<WorldStreaming>) {
    let _span = tracing::trace_span!("stream_complete").entered();
    match streaming.timing() {
        StreamTiming::Async => {}
        StreamTiming::Lockstep { .. } => streaming.complete_lockstep(),
        StreamTiming::Replay(_) => streaming.replay_complete(),
    }
}

fn run_apply(world: &mut World, in_fixed_step: bool) {
    let origin = world.get_resource::<WorldOrigin>().copied().unwrap_or_default();
    world.resource_scope(|world, mut streaming: Mut<WorldStreaming>| {
        if streaming.timing().in_fixed_step() == in_fixed_step {
            let _span = tracing::trace_span!("stream_apply").entered();
            streaming.apply_step(world, &origin.0);
        }
    });
}

/// `StreamSet::Apply` in `Update`: async timing.
pub(crate) fn apply_frame(world: &mut World) {
    run_apply(world, false);
}

/// `StreamSet::Apply` in `FixedUpdate`: lockstep and replay timing.
pub(crate) fn apply_fixed(world: &mut World) {
    run_apply(world, true);
}

/// `StreamSet::Guard`: enters the step, freezes or wakes bodies near cells that are not ready and
/// reassigns moved members (spec 78 §§ 10.4, 10.5). Runs before the physics step.
pub(crate) fn guard(
    mut streaming: ResMut<WorldStreaming>,
    gates: Res<StreamGates>,
    origin: Res<WorldOrigin>,
    (bodies, parents): (Query<GuardedBody, GuardedFilter>, ParentChain),
    frozen: Query<Entity, With<StreamFrozen>>,
    children: Query<&Children>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("stream_guard").entered();
    streaming.begin_step();
    if !streaming.is_active() {
        for entity in &frozen {
            commands.entity(entity).remove::<StreamFrozen>();
        }
        return;
    }
    let mut frozen_now = 0u32;
    let mut sources_frozen = 0u64;
    for (entity, transform, parent, member, is_frozen, guarded, is_source, auto) in &bodies {
        let Some(pose) = step_pose(transform, parent, &parents) else {
            // A parent cycle has no world pose: neither a cell move nor a freeze decision may
            // be made from it, so the body is left as it is this step.
            continue;
        };
        let pos = origin.to_world(Vec3::from(pose.translation).to_array());
        let (x, z) = (pos[0], pos[2]);
        // A character or an always-loaded body is no cell's member: only members and entities that
        // asked to be adopted follow their position (spec 78 § 10.5).
        if parent.is_none()
            && (member.is_some() || auto)
            && streaming
                .member_destination(entity, member.map(|m| m.0), x, z)
                .is_some()
        {
            // The cell changed. The hierarchy is walked only then: a still member has nothing to
            // reassign, and copying its children every step is the cost this gate avoids.
            let mut descendants: Vec<Entity> = Vec::new();
            #[cfg(test)]
            DESCENDANT_WALKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut stack: Vec<Entity> = children.get(entity).map(|c| c.to_vec()).unwrap_or_default();
            while let Some(child) = stack.pop() {
                descendants.push(child);
                if let Ok(more) = children.get(child) {
                    stack.extend(more.iter());
                }
            }
            if let Some(cell) = streaming.move_member(entity, &descendants, member.map(|m| m.0), x, z) {
                commands.entity(entity).insert(CellMember(cell));
                for child in descendants {
                    commands.entity(child).insert(CellMember(cell));
                }
            }
        }
        if !guarded {
            continue;
        }
        let ready = streaming.guard_ready(&gates, x, z);
        if ready && is_frozen {
            commands.entity(entity).remove::<StreamFrozen>();
        } else if !ready {
            if !is_frozen {
                commands.entity(entity).insert(StreamFrozen);
            }
            frozen_now += 1;
            sources_frozen += u64::from(is_source);
        }
    }
    let stats = streaming.stats_mut();
    stats.frozen_bodies = frozen_now;
    stats.source_frozen_steps += sources_frozen;
}

/// `StreamSet::Plan`, before planning: moves sources whose teleport target became ready.
pub(crate) fn run_teleports(
    mut streaming: ResMut<WorldStreaming>,
    gates: Res<StreamGates>,
    origin: Res<WorldOrigin>,
    mut transforms: Query<&mut Transform>,
    mut commands: Commands,
) {
    for moved in streaming.ready_moves(&gates) {
        if let Ok(local) = origin.from_world(moved.target)
            && let Ok(mut transform) = transforms.get_mut(moved.entity)
        {
            transform.translation = Vec3::from_array(local);
            commands.entity(moved.entity).insert(StreamTeleported);
        }
        streaming.unpin(moved.pin);
    }
}

/// FNV-1a over a set of node ids. Equal sets produce equal fingerprints.
fn fp_set(ids: &BTreeSet<u32>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for id in ids {
        hash ^= u64::from(*id);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `StreamSet::Hlod`: selects, repairs and publishes this frame's cut, keeps one entity per wanted
/// proxy within the budget, and hides the members a proxy covers (spec 78 §§ 11.2, 11.3).
/// An HLOD proxy entity of either kind: a mesh proxy or a voxel proxy (ADR-3253).
type AnyHlodProxy = Or<(With<HlodProxyMesh>, With<HlodProxyVoxels>)>;

/// Nodes the HLOD budget must keep: roots, proxies still fading out, proxy draws in the cut, and
/// proxy draws in the selection. `buf` is cleared and filled in place, then sorted, so a steady
/// frame whose set still fits the buffer allocates nothing. The caller probes it with `binary_search`.
fn remember_held(
    buf: &mut Vec<u32>,
    roots: &[u32],
    leaving: impl IntoIterator<Item = u32>,
    cut_proxies: impl IntoIterator<Item = u32>,
    selection_proxies: impl IntoIterator<Item = u32>,
) {
    buf.clear();
    buf.extend_from_slice(roots);
    buf.extend(leaving);
    buf.extend(cut_proxies);
    buf.extend(selection_proxies);
    buf.sort_unstable();
    buf.dedup();
}

pub(crate) fn hlod_step(
    mut streaming: ResMut<WorldStreaming>,
    views: Res<StreamViews>,
    (mut residency, mut voxel_residency): (ResMut<HlodProxyResidency>, ResMut<HlodVoxelResidency>),
    origin: Res<WorldOrigin>,
    mut cut: ResMut<HlodCut>,
    hidden_proxies: Query<Has<CellRenderHidden>, AnyHlodProxy>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("stream_hlod").entered();
    if views.0.is_empty() || streaming.is_sim_only() {
        return;
    }
    let Some(mut hlod) = streaming.take_hlod() else {
        return;
    };
    let viewers: Vec<[f64; 3]> = views.0.iter().map(|v| v.pos).collect();
    let budget = streaming.hlod_budget(hlod.level).unwrap_or(0);
    let fade_frames = streaming.hlod_fade_frames(hlod.level);
    if let Ok(selection) = hlod.tree.select(&viewers) {
        let level = hlod.level;
        let members_dirty = streaming.take_hlod_members_dirty();
        // A node with no mesh and no voxel proxy has nothing to draw, which counts as ready.
        let proxy_of = |node: u32| hlod.proxies.get(node as usize).copied().flatten();
        let voxels_of = |node: u32| hlod.voxels.get(node as usize).copied().flatten();
        let resident = |node: u32| {
            (proxy_of(node).is_none() || residency.resident.contains(&node))
                && (voxels_of(node).is_none() || voxel_residency.resident.contains(&node))
        };
        let mut full_visible = Vec::new();
        let mut proxies_resident = true;
        for draw in &selection {
            match draw.kind {
                regia_scene::hlod::HlodDrawKind::Full => {
                    if let Some(cell) = hlod.tree.node(draw.node).and_then(|node| node.cell) {
                        full_visible.push((draw.node, streaming.cell_draws_full(level, cell)));
                    }
                }
                regia_scene::hlod::HlodDrawKind::Proxy => {
                    if !resident(draw.node) {
                        proxies_resident = false;
                    }
                }
            }
        }
        full_visible.sort_by_key(|row| row.0);
        let visible_fp = if proxies_resident {
            0
        } else {
            streaming.visible_fp(level)
        };
        let key = crate::runtime::HlodCutKey {
            selection: selection.clone(),
            resident_fp: fp_set(&residency.resident),
            voxel_fp: fp_set(&voxel_residency.resident),
            full_visible,
            visible_fp,
        };
        let cache_hit = hlod.cut_key.as_ref() == Some(&key);
        if !cache_hit {
            // A leaf whose cell has no record streams nothing: its full detail is what draws without
            // streaming (the terrain's scatter, ADR-3253), so it is always ready to draw in full.
            let visible = |cell: CellCoord| streaming.cell_draws_full(level, cell);
            let mut walks = 0u64;
            *cut = crate::hlod::repair_counted(
                &hlod.tree,
                &hlod.parents,
                &selection,
                &visible,
                &resident,
                0.0,
                &mut crate::hlod::RepairLeaves {
                    counts: &hlod.leaf_counts,
                    walks: &mut walks,
                },
            );
            hlod.leaf_walks += walks;
            hlod.repairs += 1;
            hlod.cut_key = Some(key);
        }
        let fade_settled = cache_hit && hlod.fade.settled(fade_frames);

        // Cross-fades (opt-in): what left the cut keeps drawing, dithered out, while what entered
        // dithers in (ADR-2022). A steady instant fade keeps the previous drawable set.
        let drawable = |entry: &HlodCutEntry| match (entry.proxy, entry.cell) {
            (false, Some(cell)) => Drawable::Full(cell),
            _ => Drawable::Proxy(entry.node),
        };
        let fades = if fade_settled {
            BTreeMap::new()
        } else {
            let now: BTreeSet<Drawable> = cut.entries.iter().map(drawable).collect();
            hlod.fade.step(&now, fade_frames)
        };
        // A cached cut still holds last frame's dither. A drawable whose fade ended this frame
        // is absent from the map, and a fresh repair used to reset that dither to 0.
        for entry in &mut cut.entries {
            entry.fade = fades.get(&drawable(&*entry)).copied().unwrap_or(0.0).max(0.0);
        }
        let mut leaving_proxies = BTreeSet::new();
        let mut leaving_cells = BTreeSet::new();
        // A repaired cut starts with an empty `leaving`. A cached cut must drop the previous frame's.
        if cache_hit {
            cut.leaving.clear();
        }
        for (d, c) in &fades {
            if *c >= 0.0 {
                continue;
            }
            // The outgoing side keeps `1 - c` of the pixels.
            let share = 1.0 + *c;
            match d {
                Drawable::Proxy(node) => {
                    leaving_proxies.insert(*node);
                    cut.leaving.push(HlodCutEntry {
                        node: *node,
                        proxy: true,
                        cell: hlod.tree.node(*node).and_then(|n| n.cell),
                        fade: share,
                    });
                }
                Drawable::Full(cell) => {
                    leaving_cells.insert(*cell);
                    cut.leaving.push(HlodCutEntry {
                        node: hlod.tree.leaf_for(*cell).unwrap_or(u32::MAX),
                        proxy: false,
                        cell: Some(*cell),
                        fade: share,
                    });
                }
            }
        }
        // Wanted: the roots, the cut's proxies and the parents of the cut's nodes. Coarsest win
        // when the budget is short (spec 78 § 11.3).
        let mut wanted: BTreeSet<u32> = hlod.tree.roots().iter().copied().collect();
        wanted.extend(leaving_proxies.iter().copied());
        for entry in &cut.entries {
            if entry.proxy {
                wanted.insert(entry.node);
            }
            if let Some(parent) = hlod.parents.get(entry.node as usize).copied().flatten() {
                wanted.insert(parent);
            }
        }
        // Full leaves near a load want their own proxy for the fallback.
        for draw in &selection {
            wanted.insert(draw.node);
        }
        // What is on screen or asked for on screen is never over budget (spec 78 § 11.3,
        // ADR-3253): the roots, the cut's proxies, the proxies still dissolving out, and the
        // proxies the selection draws. The budget bounds the rest (parents and fallbacks), so a
        // zero budget means no prefetch, never a root standing in for the viewer's own cell.
        remember_held(
            &mut hlod.held_ids,
            hlod.tree.roots(),
            leaving_proxies.iter().copied(),
            cut.entries
                .iter()
                .filter(|entry| entry.proxy)
                .map(|entry| entry.node),
            selection
                .iter()
                .filter(|draw| draw.kind == regia_scene::hlod::HlodDrawKind::Proxy)
                .map(|draw| draw.node),
        );
        let mut ranked: Vec<(u32, u32, u64)> = wanted
            .into_iter()
            .filter_map(|n| hlod.tree.node(n).map(|node| (node.level, n, node.proxy_bytes)))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut used = 0u64;
        let mut keep = BTreeSet::new();
        for (_, node, bytes) in ranked {
            if hlod.held_ids.binary_search(&node).is_ok() || used.saturating_add(bytes) <= budget {
                used = used.saturating_add(bytes);
                keep.insert(node);
            }
        }

        let stale: Vec<u32> = hlod
            .spawned
            .keys()
            .copied()
            .filter(|n| !keep.contains(n))
            .collect();
        for node in stale {
            if let Some(entity) = hlod.spawned.remove(&node) {
                commands.entity(entity).despawn();
            }
            residency.resident.remove(&node);
        }
        let stale_voxels: Vec<u32> = hlod
            .spawned_voxels
            .keys()
            .copied()
            .filter(|n| !keep.contains(n))
            .collect();
        for node in stale_voxels {
            if let Some(entity) = hlod.spawned_voxels.remove(&node) {
                commands.entity(entity).despawn();
            }
            voxel_residency.resident.remove(&node);
        }
        let mut drawn: BTreeSet<u32> = cut.entries.iter().filter(|e| e.proxy).map(|e| e.node).collect();
        drawn.extend(leaving_proxies.iter().copied());
        for node in keep {
            let draw = drawn.contains(&node);
            if let Some(voxels) = hlod.voxels.get(node as usize).copied().flatten() {
                match hlod.spawned_voxels.get(&node) {
                    Some(entity) => {
                        if hidden_proxies.get(*entity).is_ok_and(|hidden| hidden == draw) {
                            if draw {
                                commands.entity(*entity).try_remove::<CellRenderHidden>();
                            } else {
                                commands.entity(*entity).try_insert(CellRenderHidden);
                            }
                        }
                    }
                    None => {
                        let local = origin.0.to_local(&LargePosition {
                            cell: voxels.anchor,
                            local: [0.0; 3],
                        });
                        let mut entity = commands.spawn((
                            HlodProxyVoxels {
                                node,
                                proxy: voxels.proxy.0,
                                bytes: voxels.bytes,
                                voxel_size_m: voxels.voxel_size_m,
                            },
                            Transform::from_translation(Vec3::from_array(local)),
                            regia_scene::level::LevelMember::from_level(level),
                        ));
                        if !draw {
                            entity.insert(CellRenderHidden);
                        }
                        let entity = entity.id();
                        hlod.spawned_voxels.insert(node, entity);
                    }
                }
            }
            let Some(id) = hlod.proxies.get(node as usize).copied().flatten() else {
                continue;
            };
            match hlod.spawned.get(&node) {
                Some(entity) => {
                    if hidden_proxies.get(*entity).is_ok_and(|hidden| hidden == draw) {
                        if draw {
                            commands.entity(*entity).try_remove::<CellRenderHidden>();
                        } else {
                            commands.entity(*entity).try_insert(CellRenderHidden);
                        }
                    }
                }
                None => {
                    let anchor = hlod.anchors.get(node as usize).copied().unwrap_or([0; 3]);
                    let local = origin.0.to_local(&LargePosition {
                        cell: anchor,
                        local: [0.0; 3],
                    });
                    let mut entity = commands.spawn((
                        HlodProxyMesh {
                            node,
                            mesh: id.0,
                            bytes: hlod.tree.node(node).map_or(0, |n| n.proxy_bytes),
                            color: hlod.colors.get(node as usize).copied().flatten(),
                            material: hlod
                                .materials
                                .get(node as usize)
                                .copied()
                                .flatten()
                                .map(|id| id.0),
                        },
                        Transform::from_translation(Vec3::from_array(local)),
                        regia_scene::level::LevelMember::from_level(level),
                    ));
                    if !draw {
                        entity.insert(CellRenderHidden);
                    }
                    let entity = entity.id();
                    hlod.spawned.insert(node, entity);
                }
            }
        }

        // Members hide behind a proxy and come back when their cell draws in full. A steady cut
        // whose entity lists did not change keeps the marks from the frame that built it.
        if !cache_hit || members_dirty || !fade_settled {
            let hide: BTreeMap<CellCoord, u32> = cut
                .hidden
                .iter()
                .filter(|(cell, _)| !leaving_cells.contains(*cell))
                .map(|(cell, node)| (*cell, *node))
                .collect();
            let shown: Vec<CellCoord> = hlod
                .hidden
                .keys()
                .copied()
                .filter(|cell| !hide.contains_key(cell))
                .collect();
            for cell in shown {
                hlod.hidden.remove(&cell);
                for entity in streaming.entities(level, cell) {
                    commands.entity(*entity).try_remove::<CellRenderHidden>();
                }
            }
            for cell in hide.keys() {
                let members = streaming.entities(level, *cell);
                if hlod.hidden.get(cell) != Some(&members.len()) {
                    for entity in members {
                        commands.entity(*entity).try_insert(CellRenderHidden);
                    }
                    hlod.hidden.insert(*cell, members.len());
                }
            }
        }
        // The dither goes on the proxies and members in a fade, and comes off when it ends.
        if fade_settled {
            let holes = cut.holes;
            if !streaming.is_opening() {
                // A gauge of the cached cut, as on the repaired path below: adding it again on
                // every steady frame made the count grow while the camera stood still.
                streaming.stats_mut().holes = holes;
            }
            streaming.put_hlod(hlod);
            return;
        }
        let mut desired: BTreeMap<Entity, f32> = BTreeMap::new();
        for (d, c) in &fades {
            match d {
                Drawable::Proxy(node) => {
                    for entity in [hlod.spawned.get(node), hlod.spawned_voxels.get(node)]
                        .into_iter()
                        .flatten()
                    {
                        desired.insert(*entity, *c);
                    }
                }
                Drawable::Full(cell) => {
                    for entity in streaming.entities(level, *cell) {
                        desired.insert(*entity, *c);
                    }
                }
            }
        }
        for entity in hlod.faded.iter().filter(|e| !desired.contains_key(*e)) {
            commands.entity(*entity).try_remove::<HlodFade>();
        }
        for (entity, c) in &desired {
            commands.entity(*entity).try_insert(HlodFade(*c));
        }
        hlod.faded = desired.into_keys().collect();

        let holes = cut.holes;
        if !streaming.is_opening() {
            streaming.stats_mut().holes = holes;
        }
    }
    streaming.put_hlod(hlod);
}

/// Follows the app's suspension (spec 78 § 19): no new reads while suspended.
pub(crate) fn watch_suspended(mut streaming: ResMut<WorldStreaming>, time: Res<Time>, mut seen: Local<bool>) {
    let suspended = time.is_suspended();
    if *seen != suspended {
        *seen = suspended;
        streaming.set_suspended(suspended);
    }
}

/// `Lifecycle` schedule: half the memory budget from a low-memory signal until the next resume
/// (spec 78 § 19). `AppLifecycle::last_event` is only set while this schedule runs.
pub(crate) fn watch_memory_trim(mut streaming: ResMut<WorldStreaming>, lifecycle: Option<Res<AppLifecycle>>) {
    match lifecycle.and_then(|l| l.last_event) {
        Some(LifecycleEvent::LowMemory) => streaming.set_trimmed(true),
        Some(LifecycleEvent::Resumed) => streaming.set_trimmed(false),
        _ => {}
    }
}

/// `StreamSet::Guard`: publishes whether a streamed level is open as `StreamingActive` (opening and
/// closing set it at once too; this catches any other path). Writes only on a change.
pub(crate) fn sync_streaming_active(
    streaming: Res<WorldStreaming>,
    active: Option<ResMut<regia_scene::streaming::StreamingActive>>,
) {
    if let Some(mut active) = active {
        active.set_if_neq(regia_scene::streaming::StreamingActive(streaming.is_active()));
    }
}

/// `StreamSet::Guard`: keeps `StreamLevelLayouts` in step with the open levels, for hooks that map
/// cells to tiles of their own (spec 78 § 13.1). Writes only on a change.
pub(crate) fn publish_layouts(
    streaming: Res<WorldStreaming>,
    layouts: Option<ResMut<regia_scene::streaming::StreamLevelLayouts>>,
) {
    let Some(mut layouts) = layouts else {
        return;
    };
    // Cheap check first: the same levels with the same grids, bounds and record counts.
    let same = layouts.0.len() == streaming.levels().count()
        && streaming.levels().all(|level| {
            layouts.0.get(&level).is_some_and(|known| {
                streaming.level_shape(level) == Some((known.grid, known.bounds, known.records.len()))
            })
        });
    if same {
        return;
    }
    let mut next = BTreeMap::new();
    for level in streaming.levels() {
        if let Some(layout) = streaming.level_layout(level) {
            next.insert(level, layout);
        }
    }
    if layouts.0 != next {
        layouts.0 = next;
    }
}

/// `OnLevelUnloading`: stops streaming a level as it unloads, before its entities go. It must not
/// wait for a frame system: a replace opens the next level in the same call, and a read landing in
/// between would spawn into a level that is gone. Its entities are level members, so the level
/// manager despawns them.
pub(crate) fn close_unloading_levels(
    mut streaming: ResMut<WorldStreaming>,
    mut gates: ResMut<StreamGates>,
    (mut residency, mut voxel_residency): (ResMut<HlodProxyResidency>, ResMut<HlodVoxelResidency>),
    mut unloading: MessageReader<LevelUnloading>,
    mut views: ResMut<StreamViews>,
    (load_sets, mut active): (
        Option<Res<regia_asset::LoadSets>>,
        Option<ResMut<regia_scene::streaming::StreamingActive>>,
    ),
) {
    for message in unloading.read() {
        let had_proxies = streaming.hlod_level() == Some(message.level);
        if streaming.close_level(message.level) {
            if let Some(sets) = &load_sets {
                let name = crate::runtime::stream_cells_set(message.level);
                if let Some(set) = sets.get(&name) {
                    for item in &set.items {
                        sets.mark_evicted(item.kind, &item.key);
                    }
                    sets.remove(&name);
                }
            }
            gates.forget_level(message.level);
            if had_proxies {
                residency.resident.clear();
                voxel_residency.resident.clear();
            }
            // Nothing samples the sources now; a reader of the views must not see the old ones.
            if !streaming.is_active() {
                views.0.clear();
                if let Some(active) = active.as_mut() {
                    active.set_if_neq(regia_scene::streaming::StreamingActive(false));
                }
            }
        }
    }
}

#[cfg(test)]
mod steady_alloc_tests {
    use super::*;
    use regia_scene::streaming::StreamAutoMember;

    use crate::alloc_count;

    fn steady_guard_allocs(child_count: usize) -> u64 {
        let mut world = World::new();
        let mut streaming = WorldStreaming::default();
        crate::runtime::open_empty_level_for_test(&mut streaming);
        world.insert_resource(streaming);
        world.insert_resource(StreamGates::default());
        world.insert_resource(WorldOrigin::default());
        let root = world.spawn((Transform::default(), StreamAutoMember)).id();
        for _ in 0..child_count {
            world.spawn((Transform::default(), ChildOf(root)));
        }
        let id = world.register_system(guard);
        world.run_system(id).expect("warmup");
        alloc_count::least(32, || {
            world.run_system(id).expect("guard");
        })
    }

    /// A root that stays in its cell must not walk or copy its children. The walk used to run on
    /// every fixed step, so a still member with children allocated and a childless one did not.
    #[test]
    fn a_stable_member_with_children_does_not_walk_its_hierarchy() {
        let childless = steady_guard_allocs(0);
        let with_children = steady_guard_allocs(4);
        assert_eq!(
            childless, 0,
            "steady guard of a childless member allocated {childless} times"
        );
        assert_eq!(
            with_children, childless,
            "steady guard allocations: childless {childless}, four children {with_children}"
        );
    }

    /// The held-node buffer is filled every HLOD frame. After it has grown to this frame's set,
    /// filling the same set again allocates nothing.
    #[test]
    fn the_hlod_held_set_allocates_nothing_once_the_buffer_fits() {
        let roots = [1_u32, 2];
        let leaving = [2_u32, 8];
        let cut = [3_u32];
        let selection = [4_u32, 1];
        let mut buf = Vec::new();
        remember_held(&mut buf, &roots, leaving, cut, selection);
        assert_eq!(buf, vec![1, 2, 3, 4, 8]);
        let least = alloc_count::least(32, || {
            remember_held(&mut buf, &roots, leaving, cut, selection);
        });
        assert_eq!(least, 0, "steady held-set fill allocated {least} times");
    }
}

#[cfg(test)]
mod stationary_member_tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use regia_app::prelude::App;
    use regia_core::id::Guid;
    use regia_core::math::{Affine3A, Vec3};
    use regia_ecs::ecs::system::SystemState;
    use regia_ecs::prelude::{ChildOf, Entity, Query, World};
    use regia_ecs::reflect::AppTypeRegistry;
    use regia_scene::ScenePlugin;
    use regia_scene::Transform;
    use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene, cook_scene};
    use regia_scene::format::{SceneEntity, SceneFile};
    use regia_scene::level::{LevelId, LoadMode};
    use regia_scene::stream_grid::CellCoord;
    use regia_scene::streaming::{CellMember, LevelStreaming, StreamAutoMember, StreamBudgets, StreamTuning};

    use super::{DESCENDANT_WALKS, POSE_VISIT_PROBES, ParentChain, step_pose};
    use crate::format::{
        CELL_BLOB_FORMAT_VERSION, CellBlob, CellRecord, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id,
    };
    use crate::{MemoryCellSource, WorldStreamingPlugin, open_streamed_from};

    fn level() -> LevelId {
        LevelId(Guid::from_u128(0xfeed))
    }

    fn streaming_settings() -> LevelStreaming {
        LevelStreaming {
            enabled: true,
            cell_size_m: 100.0,
            load_radius_m: 250.0,
            unload_radius_m: 300.0,
            hlod: None,
            budgets: Some(StreamBudgets {
                max_loads_per_step: 0,
                max_inflight: 4,
                max_pending_bytes: 1 << 30,
                apply_budget_entities: 100,
                ..StreamBudgets::default()
            }),
            tuning: StreamTuning {
                retry_after_steps: 10,
                ..StreamTuning::default()
            },
            ..LevelStreaming::default()
        }
    }

    fn cook_root(app: &App, cell: CellCoord) -> CookedScene {
        let guid = Guid::from_u128(((cell.x as u128 + 3) << 32) | (cell.z as u128 + 9));
        let text = format!(
            r#"(guid: "{guid}", name: "e", components: {{ "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)) }})"#
        );
        let mut file = SceneFile::new();
        file.entities
            .push(ron::from_str::<SceneEntity>(&text).expect("root"));
        let (cooked, warnings) = cook_scene(app.world(), &file).expect("cook");
        assert!(warnings.is_empty(), "{warnings:?}");
        cooked
    }

    fn open_two_cells(app: &mut App) {
        let source = MemoryCellSource::new();
        let mut cells = Vec::new();
        for x in 0..2 {
            let coord = CellCoord::new(x, 0);
            let scene = cook_root(app, coord);
            let blob = CellBlob {
                format_version: CELL_BLOB_FORMAT_VERSION,
                coord,
                scene,
                anchor: [0, 0, 0],
                nav: None,
            };
            let id = cell_blob_id(level(), coord);
            let bytes = blob.to_bytes(id, Vec::new()).expect("encode");
            cells.push(CellRecord {
                coord,
                blob: id,
                blob_hash: *blake3::hash(&bytes).as_bytes(),
                entity_count: 1,
                own_bytes: bytes.len() as u64,
                dep_bytes: 0,
                vram_bytes: 0,
                min_y: 0.0,
                max_y: 1.0,
                priority: 0,
                deps: vec![],
                nav_tiles: 0,
                has_terrain_collision: false,
            });
            source.insert(id, bytes);
        }
        let index = WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: level().0,
            content_hash: WorldIndex::hash_cells(&cells),
            settings: streaming_settings(),
            cells,
            bounds: [CellCoord::new(0, 0), CellCoord::new(1, 0)],
            hlod: None,
            replicated: vec![],
            nav: None,
        };
        let empty = CookedScene {
            format_version: COOKED_SCENE_VERSION,
            strings: vec![],
            entities: vec![],
            resources: vec![],
        };
        let world = app.world_mut();
        let _ = world.resource::<AppTypeRegistry>();
        open_streamed_from(world, level(), LoadMode::Replace, index, &empty, Arc::new(source)).expect("open");
    }

    fn member_cell(app: &App, entity: Entity) -> Option<CellCoord> {
        app.world().get::<CellMember>(entity).map(|member| member.0)
    }

    /// A member that stays in its cell must not walk its descendants on later steps.
    /// A real move still collects them once.
    #[test]
    fn a_stationary_member_does_not_walk_descendants() {
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        regia_app::enable_fixed_step(&mut app, 60.0);
        open_two_cells(&mut app);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
            regia_scene::streaming::StreamingSource {
                kind: regia_scene::streaming::SourceKind::Player,
                ..Default::default()
            },
        ));
        for _ in 0..12 {
            app.update();
        }
        let root = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
                StreamAutoMember,
            ))
            .id();
        let child = app.world_mut().spawn((Transform::default(), ChildOf(root))).id();
        let mut joined = false;
        for _ in 0..8 {
            app.update();
            if member_cell(&app, root) == Some(CellCoord::new(0, 0))
                && member_cell(&app, child) == Some(CellCoord::new(0, 0))
            {
                joined = true;
                break;
            }
        }
        assert!(joined, "the member never joined cell (0, 0)");
        let before = DESCENDANT_WALKS.load(Ordering::Relaxed);
        for _ in 0..4 {
            app.update();
        }
        let stationary = DESCENDANT_WALKS.load(Ordering::Relaxed) - before;
        assert_eq!(
            stationary, 0,
            "a member that did not change cell walked descendants {stationary} times"
        );
        assert_eq!(member_cell(&app, root), Some(CellCoord::new(0, 0)));

        app.world_mut()
            .get_mut::<Transform>(root)
            .expect("root")
            .translation
            .x = 150.0;
        let before_move = DESCENDANT_WALKS.load(Ordering::Relaxed);
        let mut walked = None;
        for _ in 0..8 {
            app.update();
            if member_cell(&app, child) == Some(CellCoord::new(1, 0)) {
                walked = Some(DESCENDANT_WALKS.load(Ordering::Relaxed) - before_move);
                break;
            }
        }
        let walked = walked.expect("the child never reached cell (1, 0)");
        assert!(
            (1..=2).contains(&walked),
            "a cell change collected descendants {walked} times"
        );
        assert_eq!(member_cell(&app, root), Some(CellCoord::new(1, 0)));
    }

    /// Spawns a `ChildOf` chain of `depth` parents above a leaf, each 1 m along +X from its
    /// parent, and returns the leaf and the chain from the leaf's parent up to the root.
    fn parent_chain(world: &mut World, depth: usize) -> (Entity, Vec<Entity>) {
        let mut chain = Vec::with_capacity(depth);
        let mut parent: Option<Entity> = None;
        for _ in 0..depth {
            let mut entity = world.spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)));
            if let Some(parent) = parent {
                entity.insert(ChildOf(parent));
            }
            parent = Some(entity.id());
            chain.push(entity.id());
        }
        chain.reverse();
        let leaf = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                ChildOf(*chain.first().expect("depth > 0")),
            ))
            .id();
        (leaf, chain)
    }

    /// The pose of `leaf` through `step_pose`, and how many visited entries its cycle check compared.
    fn pose_and_probes(world: &mut World, leaf: Entity) -> (Option<Affine3A>, u64) {
        let mut state = SystemState::<(Query<(&Transform, Option<&ChildOf>)>, ParentChain)>::new(world);
        let (leaves, parents) = state.get(world).expect("the chain queries are valid");
        let (transform, parent) = leaves.get(leaf).expect("leaf");
        POSE_VISIT_PROBES.with(|p| p.set(0));
        let pose = step_pose(transform, parent, &parents);
        (pose, POSE_VISIT_PROBES.with(std::cell::Cell::get))
    }

    /// rev-kimi-15 finding 3: the cycle check of a deep chain stays linear past the inline
    /// capacity, so a malformed chain near `MAX_PARENT_DEPTH` costs O(depth), not O(depth²).
    #[test]
    fn a_deep_parent_chain_pose_check_is_linear() {
        const DEPTH: usize = 1000;
        let mut world = World::new();
        let (leaf, _) = parent_chain(&mut world, DEPTH);
        let (pose, probes) = pose_and_probes(&mut world, leaf);
        let pose = pose.expect("an acyclic chain has a pose");
        assert!(
            (pose.translation.x - (DEPTH + 1) as f32).abs() < 1e-3,
            "pose {:?}",
            pose.translation
        );
        // Linear: at most the inline capacity plus one set lookup per level.
        let bound = (super::POSE_VISIT_INLINE as u64 + 1) * DEPTH as u64;
        assert!(
            probes <= bound,
            "{probes} compares for a chain of {DEPTH} (bound {bound})"
        );
    }

    /// A cycle deep in a long chain, past the inline capacity, still returns no pose.
    #[test]
    fn a_deep_parent_cycle_has_no_pose() {
        const DEPTH: usize = 900;
        let mut world = World::new();
        let (leaf, chain) = parent_chain(&mut world, DEPTH);
        // The root's parent is the 500th link, so the chain loops through 400 entities.
        let root = *chain.last().expect("root");
        world.entity_mut(root).insert(ChildOf(chain[500]));
        let (pose, probes) = pose_and_probes(&mut world, leaf);
        assert!(pose.is_none(), "a parent cycle has no pose: {pose:?}");
        let bound = (super::POSE_VISIT_INLINE as u64 + 1) * (DEPTH as u64 + 1);
        assert!(probes <= bound, "{probes} compares (bound {bound})");
    }
}
