// SPDX-License-Identifier: MPL-2.0
//! The world side of joins and travel (spec 77 §§ 6.1-6.2, ADR-7212).
//!
//! - **Join snapshot.** [`join_snapshot_for`] builds what a joining peer gets before `InGame` from
//!   the server's replication state: item 1 is the `EntityGuid`s of level-placed replicated
//!   entities removed since the level loaded; item 2 is one [`JoinRecord`] per join-critical
//!   entity (the peer's own entities and `sent_to_all` game state). The record names the entity
//!   (`NetId`) and why it is critical; its state travels in the replication stream's join frame,
//!   which sends these spawns outside the spawn budget (one copy table per connection, so the
//!   session never spawns a second copy). Every other relevant entity streams in through the
//!   spawn budget afterwards. `NetSession` installs it as the server core's join source and the
//!   client applies item 1 before its first `InGame` frame: removed level entities never show.
//! - **Travel.** [`despawn_for_travel`] despawns every replicated entity except
//!   [`NetPersistAcrossTravel`] ones (a remote player's `NetPlayer` entity has it by default; a game
//!   adds it to a team or score entity) and starts a new level in the replication state: the
//!   join windows open again and the old level's removed entities are forgotten. Clients keep the
//!   persistent copies under the same `NetId`.
//! - **Client side of travel (ADR-7259).** In the same step, [`travel_keep_for`] reads the
//!   persistent `NetId`s and the first unassigned `NetId`; `TravelBegin` carries them
//!   ([`TravelKeep`]), and the client drops every other copy of the old level at once
//!   ([`drop_copies_for_travel`]) instead of one send tick later, when the despawn records arrive
//!   (they then find nothing to do).

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_scene::components::EntityGuid;
use regia_scene::level::LevelMember;
use serde::Serialize;

use crate::model::{NetId, PeerId};
use crate::profile::{NetProfileRef, NetProfiles};
use crate::replication::{Replicated, ServerReplication};
use crate::session::lifecycle::JoinSnapshot;
use crate::session::proto::{MAX_TRAVEL_KEEP_IDS, TravelKeep};

/// Keeps a replicated entity (and its `NetId`) across a level travel (spec 77 § 6.2). Player
/// entities have it by default; add it to a team or score entity that must outlive the level.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct NetPersistAcrossTravel;

/// What one travel despawn did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TravelCounts {
    /// Replicated entities despawned.
    pub despawned: u32,
    /// Replicated entities kept (`NetPersistAcrossTravel`), with their `NetId`s.
    pub persistent: u32,
}

/// Despawns every replicated entity except [`NetPersistAcrossTravel`] ones and starts a new level
/// in the server's replication state (spec 77 § 6.2 step 2). Entities that are not replicated are
/// left to the level loader.
pub fn despawn_for_travel(world: &mut World) -> TravelCounts {
    let _span = tracing::debug_span!(target: "net", "net_travel_despawn").entered();
    let mut q = world.query_filtered::<(Entity, Has<NetPersistAcrossTravel>), With<Replicated>>();
    let mut counts = TravelCounts::default();
    let mut gone: Vec<Entity> = Vec::new();
    for (entity, keep) in q.iter(world) {
        if keep {
            counts.persistent += 1;
        } else {
            gone.push(entity);
        }
    }
    // The removals the travel itself causes are the ones a joiner into the new level need not
    // hear about; the next despawn pass forgets exactly this set (spec 77 § 6.2).
    let mut travel_removed = std::collections::BTreeSet::new();
    for entity in gone {
        // A child may already be gone with its parent.
        if world.get_entity(entity).is_ok() {
            if let (Some(level), Some(guid)) =
                (world.get::<LevelMember>(entity), world.get::<EntityGuid>(entity))
            {
                travel_removed.insert((level.0, guid.0.as_u128()));
            }
            world.despawn(entity);
            counts.despawned += 1;
        }
    }
    if let Some(mut rep) = world.get_resource_mut::<ServerReplication>() {
        rep.begin_level();
        rep.set_travel_removed(travel_removed);
    }
    tracing::info!(
        target: "net",
        despawned = counts.despawned,
        persistent = counts.persistent,
        "net travel: replicated entities despawned"
    );
    counts
}

/// The copies clients keep at a travel (spec 77 § 6.2 step 3, ADR-7259), read on the server in the
/// step that despawns the old level: the `NetId`s of the replicated `NetPersistAcrossTravel`
/// entities (what [`despawn_for_travel`] keeps) and the first `NetId` the server has not assigned
/// (every id below it belongs to the old level; ids are never reused). `None` when more than
/// [`MAX_TRAVEL_KEEP_IDS`] entities persist: clients then wait for the despawn records.
pub fn travel_keep_for(world: &mut World) -> Option<TravelKeep> {
    let below = world
        .get_resource::<ServerReplication>()
        .map_or(NetId(0), ServerReplication::next_net_id);
    let mut q = world.query_filtered::<&NetId, (With<Replicated>, With<NetPersistAcrossTravel>)>();
    let mut ids: Vec<NetId> = q.iter(world).copied().collect();
    if ids.len() > MAX_TRAVEL_KEEP_IDS {
        tracing::warn!(
            target: "net",
            persistent = ids.len(),
            max = MAX_TRAVEL_KEEP_IDS,
            "net travel: too many persistent entities for the keep list; clients wait for despawns"
        );
        return None;
    }
    ids.sort_unstable();
    Some(TravelKeep { below, ids })
}

/// Client, at `TravelBegin` (spec 77 § 6.2 step 3, ADR-7259): despawns every replicated copy of the
/// old level (`NetId` below `keep.below`) that is not in `keep.ids`, the same way a despawn record
/// does (despawn events, children detached, references pending), without waiting for the
/// interpolation tick: the level is gone. Copies of entities spawned since the travel and the
/// persistent copies stay under their `NetId`s. The server's despawn records for the dropped copies
/// arrive later and find nothing.
pub fn drop_copies_for_travel(world: &mut World, keep: &TravelKeep) -> TravelCounts {
    let _span = tracing::debug_span!(target: "net", "net_travel_client_drop").entered();
    let kept: std::collections::BTreeSet<NetId> = keep.ids.iter().copied().collect();
    let (gone, persistent): (Vec<NetId>, Vec<NetId>) = world
        .get_resource::<crate::runtime::NetEntities>()
        .map(|ids| {
            ids.iter()
                .map(|(id, _)| id)
                .filter(|id| *id < keep.below)
                .partition(|id| !kept.contains(id))
        })
        .unwrap_or_default();
    // The reliable tick stream (ADR-7295): `TravelBegin` overtakes the stream, so records of the
    // old level still in flight must be dropped, not applied: the floor covers every id below
    // `keep.below` except the persistent ones. The held copies go through the usual despawn
    // (events, children, references); the floor covers the ids this client never held too.
    if let Some(mut client) = world.get_resource_mut::<crate::replication::ClientReplication>() {
        client.state.begin_travel_floor(keep.below, &keep.ids);
    }
    let despawned = crate::replication::client_drop_for_travel(world, &gone);
    let counts = TravelCounts {
        despawned,
        persistent: persistent.len() as u32,
    };
    tracing::info!(
        target: "net",
        despawned = counts.despawned,
        persistent = counts.persistent,
        "net travel: client dropped the old level's copies"
    );
    counts
}

/// Why an entity is in a join snapshot (spec 77 § 6.1 item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinRecordKind {
    /// Owned by the joining peer (its controller, its character, what it holds).
    Own,
    /// Relevant everywhere: a `sent_to_all` profile (game state, team scores).
    GameState,
}

/// One join snapshot record: a join-critical entity the replication join frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct JoinRecord {
    /// The entity's `NetId` (the same on the server and every client).
    pub net_id: NetId,
    /// Why it is in the snapshot.
    pub kind: JoinRecordKind,
}

/// Bytes of one encoded [`JoinRecord`].
pub const JOIN_RECORD_BYTES: usize = 5;

impl JoinRecord {
    /// The record's bytes: the kind (`0` own, `1` game state), then the `NetId` (u32 LE).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(JOIN_RECORD_BYTES);
        out.push(match self.kind {
            JoinRecordKind::Own => 0,
            JoinRecordKind::GameState => 1,
        });
        out.extend_from_slice(&self.net_id.0.to_le_bytes());
        out
    }

    /// Reads a record; `None` for bytes of another shape (a game's own join source).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let [kind, a, b, c, d] = *bytes else {
            return None;
        };
        let kind = match kind {
            0 => JoinRecordKind::Own,
            1 => JoinRecordKind::GameState,
            _ => return None,
        };
        Some(Self {
            net_id: NetId(u32::from_le_bytes([a, b, c, d])),
            kind,
        })
    }
}

/// The join-critical entities of `peer`, in `NetId` order: replicated entities owned by `peer`
/// (the owner the last replication gather resolved), else those whose profile is `sent_to_all`.
pub fn join_records_for(world: &World, peer: PeerId) -> Vec<JoinRecord> {
    let mut by_peer = join_records_by_peer(world, &[peer]);
    by_peer.remove(&peer).unwrap_or_default()
}

/// [`join_records_for`] for several peers in one pass over the replicated entities.
fn join_records_by_peer(world: &World, peers: &[PeerId]) -> BTreeMap<PeerId, Vec<JoinRecord>> {
    let mut out: BTreeMap<PeerId, Vec<JoinRecord>> = peers.iter().map(|p| (*p, Vec::new())).collect();
    let Some(rep) = world.get_resource::<ServerReplication>() else {
        return out;
    };
    let profiles = world.get_resource::<NetProfiles>();
    let default_profile = profiles.and_then(|p| p.get(p.default_key()));
    for (net_id, entity, owner) in rep.entity_owners() {
        let state = world.get_entity(entity).is_ok_and(|e| {
            e.get::<NetProfileRef>()
                .and_then(|r| profiles.and_then(|p| p.get(&r.0)))
                .or(default_profile)
                .is_some_and(|p| p.sent_to_all)
        });
        for (peer, records) in &mut out {
            let kind = if *peer == owner {
                JoinRecordKind::Own
            } else if state {
                JoinRecordKind::GameState
            } else {
                continue;
            };
            records.push(JoinRecord { net_id, kind });
        }
    }
    for records in out.values_mut() {
        records.sort_by_key(|r| r.net_id.0);
    }
    out
}

/// The join snapshot of `peer` from the server's replication state (spec 77 § 6.1), captured at
/// `tick`: the removed level entities' guids and one encoded [`JoinRecord`] per join-critical
/// entity.
pub fn join_snapshot_for(world: &World, peer: PeerId, tick: u64) -> JoinSnapshot {
    join_snapshots_for(world, &[peer], tick)
        .remove(&peer)
        .unwrap_or_default()
}

/// [`join_snapshot_for`] for several peers in one pass (what `NetSession` captures each step for
/// the peers still loading).
pub fn join_snapshots_for(world: &World, peers: &[PeerId], tick: u64) -> BTreeMap<PeerId, JoinSnapshot> {
    let _span = tracing::debug_span!(target: "net", "net_join_snapshot", peers = peers.len()).entered();
    let removed = world
        .get_resource::<ServerReplication>()
        .map(ServerReplication::level_removed_guids)
        .unwrap_or_default();
    join_records_by_peer(world, peers)
        .into_iter()
        .map(|(peer, records)| {
            (
                peer,
                JoinSnapshot {
                    removed: removed.clone(),
                    records: records.iter().map(JoinRecord::encode).collect(),
                    tick,
                },
            )
        })
        .collect()
}

/// Join snapshot counters of a session (agent `net.session`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct JoinSnapshotCounts {
    /// Server: join snapshots sent. Client: join snapshots completed (`InGame` markers).
    pub snapshots: u64,
    /// Removed level entities named (server: sent; client: received).
    pub removed: u64,
    /// Removed level entities the client found and despawned before `InGame`.
    pub removed_applied: u64,
    /// Join records (server: sent; client: received).
    pub records: u64,
    /// Server: join-snapshot capture passes over the replication state (one pass refreshes every
    /// Loading peer whose inputs changed; a quiet frame adds none).
    pub captures: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_records_round_trip_and_refuse_other_shapes() {
        for kind in [JoinRecordKind::Own, JoinRecordKind::GameState] {
            let r = JoinRecord {
                net_id: NetId(0x0102_0304),
                kind,
            };
            assert_eq!(JoinRecord::decode(&r.encode()), Some(r));
            assert_eq!(r.encode().len(), JOIN_RECORD_BYTES);
        }
        assert_eq!(JoinRecord::decode(&[1, 2, 3]), None);
        assert_eq!(JoinRecord::decode(&[9, 0, 0, 0, 0]), None);
    }
}
