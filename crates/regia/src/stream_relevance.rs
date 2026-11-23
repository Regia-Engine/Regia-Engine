// SPDX-License-Identifier: MPL-2.0
//! Loaded-cell relevance (spec 78 § 21, spec 72).
//!
//! The filter `stream_cells` drops a replicated entity whose stream cell is outside the peer's
//! loaded cells and a one-cell margin. An entity with no cell stays relevant. The filter only
//! returns false: it never adds interest. A peer whose enabled streaming source has no position
//! (no `Transform`, or a NaN or infinite one) fails closed: it holds an empty loaded set, so every
//! cell member is dropped for it until the source has a finite position. [`StreamCellRelevancePlugin`] registers it on a server
//! once a streamed level is open. A client does not register it.
//!
//! Dedicated servers get the plugin from `regia::server::SimulationPlugins`. A listen server
//! built with the client plugins adds [`StreamCellRelevancePlugin`] itself.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_net::NetSet;
use regia_net::model::{NetMode, NetOwner, PeerId, on_server};
use regia_net::profile::{NetProfile, NetProfileRef, NetProfiles, profile_key};
use regia_net::relevance::{
    NetInterest, NetRelevanceFilter, NetRelevanceFilters, add_net_relevance_filter, install_net_interest,
};
use regia_net::replication::Replicated;
use regia_net::runtime::NetNode;
use regia_net::settings::NetSettings;
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_scene::streaming::{CellMember, StreamSet, StreamingSource};
use regia_scene::{GlobalTransform, Transform};
use regia_world::{WorldOrigin, WorldStreaming};

/// Name profiles use for the loaded-cell filter.
pub const FILTER_NAME: &str = "stream_cells";

/// Cells this far from a loaded cell, in Chebyshev distance, stay relevant.
pub const MARGIN_CELLS: i64 = 1;

/// What [`report`] returns for one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRelevanceReport {
    /// The peer id.
    pub peer: u32,
    /// Loaded stream cells, `"<x>_<z>"`, in cell order.
    pub loaded: Vec<String>,
    /// The margin, in cells. See [`MARGIN_CELLS`].
    pub margin: i64,
    /// Entities this filter dropped for the peer on the last published snapshot.
    pub dropped: u64,
}

/// Registers the loaded-cell filter on a server while world streaming is active.
#[derive(Debug, Default)]
pub struct StreamCellRelevancePlugin;

/// Per-peer loaded cells and the drop counts of the newest snapshot.
#[derive(Resource, Default)]
struct StreamCellRelevance {
    registered: bool,
    /// Profiles whose stamp was refused, so a later frame does not try them again.
    stamp_refused: BTreeSet<String>,
    /// Resident cells inside the peer's load radius, sorted. Absent means "do not narrow".
    loaded: BTreeMap<PeerId, Vec<CellCoord>>,
    /// Drops since the last [`publish_dropped`].
    inflight: Mutex<Option<BTreeMap<PeerId, BTreeSet<Entity>>>>,
    /// Original custom filters, indexed by the resolved profile key.
    chained: BTreeMap<String, String>,
    /// Drops of the last published snapshot.
    dropped_last: BTreeMap<PeerId, u64>,
}

struct LevelSnap {
    grid: StreamGrid,
    load_radius_m: f64,
    resident: Vec<CellCoord>,
}

struct StreamCells;

impl NetRelevanceFilter for StreamCells {
    fn relevant(world: &World, entity: Entity, peer: PeerId) -> bool {
        if !previous_filter_passes(world, entity, peer) {
            return false;
        }
        if !outside_loaded(world, entity, peer) {
            return true;
        }
        // Diagnostics keep NetInterest in the world; the send pipeline takes it out.
        if !world.contains_resource::<NetInterest>()
            && let Some(state) = world.get_resource::<StreamCellRelevance>()
        {
            state.note_drop(peer, entity);
        }
        false
    }
}

impl StreamCellRelevance {
    fn note_drop(&self, peer: PeerId, entity: Entity) {
        let mut counts = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(counts) = counts.as_mut() {
            counts.entry(peer).or_default().insert(entity);
        }
    }
}

impl Plugin for StreamCellRelevancePlugin {
    fn build(&self, app: &mut App) {
        register_components(app.world_mut());
        app.init_resource::<StreamCellRelevance>();
        #[cfg(feature = "agent")]
        app.insert_resource(regia_agent::CellRelevanceHost::new(host_call));
        app.add_systems(
            FixedUpdate,
            sync_loaded_cells
                .after(StreamSet::Apply)
                .before(NetSet::Send)
                .run_if(on_server()),
        );
        app.add_systems(
            FixedUpdate,
            publish_dropped.after(NetSet::Send).run_if(on_server()),
        );
    }
}

fn register_components(world: &mut World) {
    let _ = world.register_component::<StreamingSource>();
    let _ = world.register_component::<NetOwner>();
    let _ = world.register_component::<Transform>();
    let _ = world.register_component::<GlobalTransform>();
    let _ = world.register_component::<CellMember>();
    let _ = world.register_component::<ChildOf>();
}

/// The host hook for `stream.cell_relevance`. Maps [`report`] into the agent reply.
#[cfg(feature = "agent")]
fn host_call(world: &World, peer: u32) -> Result<regia_agent::CellRelevanceReply, &'static str> {
    let report = report(world, PeerId(peer))?;
    Ok(regia_agent::CellRelevanceReply {
        peer: report.peer,
        loaded: report.loaded,
        margin: report.margin,
        dropped: report.dropped,
    })
}

/// Copies peer cells, enables relevance, and composes cell filtering with profile filters.
///
/// A client returns immediately. So does a server with no streamed level open: the filter stays
/// unregistered and profiles are left alone. A peer with an enabled streaming source and no
/// resident cell in range, or whose source has no finite position, is stored with an empty list,
/// so cell-members are dropped. A peer with no entry is not narrowed.
pub fn sync_loaded_cells(world: &mut World) {
    let _span = tracing::debug_span!("stream_cell_relevance").entered();
    if !world
        .get_resource::<NetMode>()
        .copied()
        .unwrap_or_default()
        .is_server()
    {
        return;
    }
    if !streaming_enabled(world) {
        return;
    }
    let snaps = level_snaps(world);
    let origin = world.get_resource::<WorldOrigin>().copied();
    let loaded = peer_cells(world, &snaps, origin);
    world.init_resource::<StreamCellRelevance>();
    install_net_interest(world);
    ensure_registered(world);
    stamp_profiles(world);
    if let Some(mut state) = world.get_resource_mut::<StreamCellRelevance>() {
        state.loaded = loaded;
    }
    begin_snapshot(world);
}

fn begin_snapshot(world: &mut World) {
    let tick = world.get_resource::<NetNode>().map_or(0, |node| node.tick.0);
    let every = world
        .get_resource::<NetSettings>()
        .map_or(1, |settings| settings.replication.send_every_ticks.max(1));
    if let Some(state) = world.get_resource::<StreamCellRelevance>() {
        let mut counts = state.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        *counts = tick.is_multiple_of(every).then(BTreeMap::new);
    }
}

/// Publishes unique entity drops after a snapshot send.
///
/// Fixed steps without a snapshot and diagnostic evaluations preserve the last snapshot's count.
pub fn publish_dropped(world: &mut World) {
    let _span = tracing::trace_span!("stream_cell_relevance_publish").entered();
    let published = {
        let Some(state) = world.get_resource::<StreamCellRelevance>() else {
            return;
        };
        state
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    };
    if let Some(published) = published
        && let Some(mut state) = world.get_resource_mut::<StreamCellRelevance>()
    {
        state.dropped_last = published
            .into_iter()
            .map(|(peer, entities)| (peer, entities.len() as u64))
            .collect();
    }
}

fn previous_filter_passes(world: &World, entity: Entity, peer: PeerId) -> bool {
    let Some(profiles) = world.get_resource::<NetProfiles>() else {
        return true;
    };
    let name = world
        .get::<NetProfileRef>(entity)
        .map(|r| r.0.as_str())
        .or_else(|| world.get::<Replicated>(entity).and_then(|r| r.profile.as_deref()));
    let key = name
        .map(profile_key)
        .filter(|name| profiles.get(name).is_some())
        .unwrap_or(profiles.default_key());
    let original = world
        .get_resource::<StreamCellRelevance>()
        .and_then(|state| state.chained.get(key));
    let filter = original.and_then(|name| {
        world
            .get_resource::<NetRelevanceFilters>()
            .and_then(|filters| filters.get(name))
    });
    filter.is_none_or(|filter| filter(world, entity, peer))
}

/// Loaded cells, margin and last-snapshot drops for `peer`.
///
/// `Err("peer_unknown")` when the peer has no loaded-cell entry and is not a connected peer.
pub fn report(world: &World, peer: PeerId) -> Result<CellRelevanceReport, &'static str> {
    let in_cache = world
        .get_resource::<StreamCellRelevance>()
        .is_some_and(|state| state.loaded.contains_key(&peer));
    let connected = world
        .get_resource::<NetNode>()
        .is_some_and(|node| node.has_peer(peer));
    if !in_cache && !connected {
        return Err("peer_unknown");
    }
    let state = world.get_resource::<StreamCellRelevance>();
    let loaded = state
        .and_then(|state| state.loaded.get(&peer))
        .map(|cells| cells.iter().map(CellCoord::label).collect())
        .unwrap_or_default();
    let dropped = state
        .and_then(|state| state.dropped_last.get(&peer).copied())
        .unwrap_or(0);
    Ok(CellRelevanceReport {
        peer: peer.0,
        loaded,
        margin: MARGIN_CELLS,
        dropped,
    })
}

fn streaming_enabled(world: &World) -> bool {
    world.get_resource::<WorldStreaming>().is_some_and(|streaming| {
        streaming
            .levels()
            .any(|level| streaming.settings(level).is_some_and(|settings| settings.enabled))
    })
}

fn outside_loaded(world: &World, entity: Entity, peer: PeerId) -> bool {
    if !streaming_enabled(world) {
        return false;
    }
    let Some(cell) = world.get::<CellMember>(entity).map(|member| member.0) else {
        return false;
    };
    let Some(state) = world.get_resource::<StreamCellRelevance>() else {
        return false;
    };
    match state.loaded.get(&peer) {
        Some(loaded) => !within_margin(loaded, cell),
        None => false,
    }
}

fn within_margin(loaded: &[CellCoord], cell: CellCoord) -> bool {
    let margin = MARGIN_CELLS.unsigned_abs();
    loaded.iter().any(|loaded_cell| {
        loaded_cell.x.abs_diff(cell.x) <= margin && loaded_cell.z.abs_diff(cell.z) <= margin
    })
}

fn level_snaps(world: &World) -> Vec<LevelSnap> {
    let Some(streaming) = world.get_resource::<WorldStreaming>() else {
        return Vec::new();
    };
    let levels: Vec<_> = streaming.levels().collect();
    let mut snaps = Vec::new();
    for level in levels {
        let Some(settings) = streaming.settings(level) else {
            continue;
        };
        if !settings.enabled {
            continue;
        }
        let Ok(grid) = settings.grid() else {
            continue;
        };
        let resident = streaming
            .cells(level)
            .filter(|(_, state)| state.is_spawned())
            .map(|(cell, _)| cell)
            .collect();
        snaps.push(LevelSnap {
            grid,
            load_radius_m: settings.load_radius_m,
            resident,
        });
    }
    snaps
}

fn peer_cells(
    world: &World,
    snaps: &[LevelSnap],
    origin: Option<WorldOrigin>,
) -> BTreeMap<PeerId, Vec<CellCoord>> {
    // Shared `&World`: `World::query` would need `&mut World` while streaming is borrowed.
    let mut sets: BTreeMap<PeerId, BTreeSet<CellCoord>> = BTreeMap::new();
    for entity in world.iter_entities() {
        let Some(source) = entity.get::<StreamingSource>().copied() else {
            continue;
        };
        let Some(peer) = entity.get::<NetOwner>().map(|owner| owner.0) else {
            continue;
        };
        if !source.enabled {
            continue;
        }
        // The peer is narrowed even when no resident cell is in range, and when its source has no
        // usable position (no transform, or a non-finite one): fail closed to an empty set.
        sets.entry(peer).or_default();
        let Some(local) = local_position(&entity) else {
            continue;
        };
        let Some((x, z)) = world_xz(origin, local) else {
            continue;
        };
        let scale = if source.radius_scale.is_finite() {
            source.radius_scale.clamp(0.1, 8.0)
        } else {
            1.0
        };
        for snap in snaps {
            let radius = snap.load_radius_m * f64::from(scale);
            if !radius.is_finite() {
                continue;
            }
            for cell in &snap.resident {
                if snap.grid.distance_to_cell(x, z, *cell) <= radius {
                    sets.entry(peer).or_default().insert(*cell);
                }
            }
        }
    }
    sets.into_iter()
        .map(|(peer, cells)| (peer, cells.into_iter().collect()))
        .collect()
}

fn local_position(entity: &EntityRef) -> Option<[f32; 3]> {
    if entity.get::<ChildOf>().is_some() {
        let global = entity.get::<GlobalTransform>()?;
        return Some(Vec3::from(global.0.translation).to_array());
    }
    Some(entity.get::<Transform>()?.translation.to_array())
}

fn world_xz(origin: Option<WorldOrigin>, local: [f32; 3]) -> Option<(f64, f64)> {
    let (x, z) = match origin {
        Some(origin) => {
            let world = origin.to_world(local);
            (world[0], world[2])
        }
        None => (f64::from(local[0]), f64::from(local[2])),
    };
    (x.is_finite() && z.is_finite()).then_some((x, z))
}

fn ensure_registered(world: &mut World) {
    if world
        .get_resource::<StreamCellRelevance>()
        .is_some_and(|state| state.registered)
    {
        return;
    }
    let present = world
        .get_resource::<NetRelevanceFilters>()
        .is_some_and(|filters| filters.get(FILTER_NAME).is_some());
    if !present {
        add_net_relevance_filter::<StreamCells>(world, FILTER_NAME);
    }
    if let Some(mut state) = world.get_resource_mut::<StreamCellRelevance>() {
        state.registered = true;
    }
}

fn stamp_profiles(world: &mut World) {
    let skipped: Vec<String> = world
        .get_resource::<StreamCellRelevance>()
        .filter(|state| !state.stamp_refused.is_empty())
        .map(|state| state.stamp_refused.iter().cloned().collect())
        .unwrap_or_default();
    let Some(profiles) = world.get_resource::<NetProfiles>() else {
        return;
    };
    let names: Vec<String> = profiles
        .iter()
        .filter(|(name, profile)| needs_stamp(profile) && !skipped.iter().any(|skip| skip.as_str() == *name))
        .map(|(name, _)| name.to_string())
        .collect();
    if names.is_empty() {
        return;
    }
    let mut refused = Vec::new();
    let mut chained = Vec::new();
    if let Some(mut profiles) = world.get_resource_mut::<NetProfiles>() {
        for name in names {
            let Some(mut profile) = profiles.get(&name).cloned() else {
                continue;
            };
            let original = profile.custom_filter.take();
            profile.custom_filter = Some(FILTER_NAME.to_string());
            if let Err(error) = profile.validate() {
                refused.push((name, error.to_string()));
                continue;
            }
            if let Err(error) = profiles.insert(&name, profile) {
                refused.push((name, error.to_string()));
            } else {
                chained.push((name, original));
            }
        }
    }
    let Some(mut state) = world.get_resource_mut::<StreamCellRelevance>() else {
        return;
    };
    for (name, original) in chained {
        if let Some(original) = original {
            state.chained.insert(name, original);
        } else {
            state.chained.remove(&name);
        }
    }
    for (name, error) in refused {
        if state.stamp_refused.insert(name.clone()) {
            tracing::error!(
                target: "stream_cell_relevance",
                profile = %name,
                error = %error,
                "stream_cell_profile_stamp_refused"
            );
        }
    }
}

fn needs_stamp(profile: &NetProfile) -> bool {
    profile.custom_filter.as_deref() != Some(FILTER_NAME)
        && !profile.sent_to_all
        && !profile.owner_only
        && !profile.follow_parent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(x: i64, z: i64) -> CellCoord {
        CellCoord::new(x, z)
    }

    #[test]
    fn one_cell_margin_is_chebyshev() {
        let loaded = [cell(0, 0)];
        assert!(within_margin(&loaded, cell(0, 0)));
        assert!(within_margin(&loaded, cell(1, 0)));
        assert!(within_margin(&loaded, cell(1, -1)));
        assert!(!within_margin(&loaded, cell(2, 0)));
        assert!(!within_margin(&loaded, cell(8, 0)));
        assert!(!within_margin(&[], cell(0, 0)));
    }
}
