// SPDX-License-Identifier: MPL-2.0
//! What a player needs from the net runtime (spec 103 § 8). The player itself lives in
//! `regia_framework::net_replay`; these work on any client world, so a tool can drive a file without it.

use regia_ecs::prelude::*;
use regia_scene::level::LevelMember;

use super::format::NetDemoError;
use crate::bits::{BitReader, BitWriter};
use crate::model::{NetId, NetTick};
use crate::replication::{self, ClientReplication, NetEvents, NetUnbound};
use crate::runtime::{self, NetEntities, NetNode};

/// While this resource exists the host's real-time interpolation systems stand down: the player passes playback time
/// to `net_interpolate` itself, so pause freezes proxies and speed scales them.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct NetReplayClock;

/// Checks a frame the way the client path will, so a bad one is an error here and not a closed connection there.
///
/// # Errors
/// [`NetDemoError::Corrupt`].
pub fn validate_frame(bytes: &[u8]) -> Result<(), NetDemoError> {
    runtime::validate_server_frame(bytes)
        .map_err(|e| NetDemoError::Corrupt(format!("a frame does not decode: {e:?}")))
}

/// A `TICK` frame without its messages, for a silent seek. `None` for any other frame: those carry only messages.
///
/// # Errors
/// [`NetDemoError::Corrupt`].
pub fn state_only_frame(bytes: &[u8]) -> Result<Option<Vec<u8>>, NetDemoError> {
    runtime::state_only_tick_frame(bytes)
        .map_err(|e| NetDemoError::Corrupt(format!("a frame does not decode: {e:?}")))
}

/// The same `TICK` frame stamped with another tick (the first pass of a keyframe that holds references, spec 103 § 3.6).
///
/// # Errors
/// [`NetDemoError::Corrupt`].
pub fn restamp_tick_frame(bytes: &[u8], tick: NetTick) -> Result<Vec<u8>, NetDemoError> {
    let corrupt = |e| NetDemoError::Corrupt(format!("a frame does not decode: {e:?}"));
    let mut r = BitReader::new(bytes);
    let tag = r.read_bits(8).map_err(corrupt)?;
    r.read_varint().map_err(corrupt)?;
    let at = r.bit_pos();
    let total = bytes.len() * 8;
    let mut w = BitWriter::with_capacity(bytes.len() + 4);
    w.write_bits(tag, 8);
    w.write_varint(u64::from(tick.0));
    let mut rest = BitReader::new(bytes);
    for _ in 0..at / 8 {
        rest.read_bits(8).map_err(corrupt)?;
    }
    rest.read_bits((at % 8) as u32).map_err(corrupt)?;
    let mut left = total - at;
    while left > 0 {
        let n = left.min(64);
        w.write_bits(rest.read_bits(n as u32).map_err(corrupt)?, n as u32);
        left -= n;
    }
    Ok(w.finish())
}

/// A keyframe with the messages of its tick's frame: where a linear play starts (spec 103 § 8).
///
/// # Errors
/// [`NetDemoError::Corrupt`].
pub fn keyframe_with_messages(keyframe: &[u8], tick_frame: &[u8]) -> Result<Vec<u8>, NetDemoError> {
    runtime::tick_frame_with_messages_of(keyframe, tick_frame)
        .map_err(|e| NetDemoError::Corrupt(format!("a frame does not decode: {e:?}")))
}

/// Whether a frame is a `TICK` frame.
#[must_use]
pub fn is_tick_frame(bytes: &[u8]) -> bool {
    bytes.first().is_some_and(|tag| *tag == runtime::TAG_TICK)
}

/// Whether any registered component can hold an entity reference.
#[must_use]
pub fn registry_has_refs(world: &World) -> bool {
    world
        .get_resource::<replication::ReplicationRegistry>()
        .is_some_and(|r| r.components().iter().any(|c| c.has_refs))
}

/// Returns a client world to "connected to nothing" (spec 103 § 8.1): every replicated copy despawns, except
/// level-bound ones, which lose their id and wait for a bind, as on a fresh join.
pub fn reset_client_world(world: &mut World) {
    let _span = tracing::trace_span!("net_replay_reset").entered();
    runtime::ensure_resources(world);
    let mut q = world.query_filtered::<(Entity, Has<LevelMember>), With<NetId>>();
    let copies: Vec<(Entity, bool)> = q.iter(world).collect();
    for (e, level) in copies {
        if level {
            if let Ok(mut ent) = world.get_entity_mut(e) {
                ent.remove::<NetId>();
                ent.insert(NetUnbound);
            }
        } else if world.get_entity(e).is_ok() {
            // Children of a despawned parent are gone by now.
            let _ = world.despawn(e);
        }
    }
    world.resource_mut::<ClientReplication>().forget_everything();
    world.insert_resource(NetEntities::default());
    world.insert_resource(NetEvents::default());
    world.resource_mut::<NetNode>().tick = NetTick(0);
    replication::client_session_started(world);
    world.remove_resource::<runtime::NetInterpTick>();
}
