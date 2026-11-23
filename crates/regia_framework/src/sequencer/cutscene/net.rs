// SPDX-License-Identifier: MPL-2.0
//! Net cutscenes over `regia_net` (feature `net`, ADR-2948): the server announces a start tick to
//! every peer, a peer that joins while the cutscene runs gets the same announcement, and every peer
//! schedules it in its [`NetCutscenes`]. Each peer plays its own player; nothing of the cutscene is
//! replicated.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_net::model::PeerId;
use regia_net::runtime::NetNode;
use regia_net::{NetMessage, NetMessageAppExt, NetMessages, NetSender, NetSet};

use super::{NetCutscenes, run_cutscenes};

/// Server to one peer: play cutscene `name` from tick `start_tick` (a late joiner gets the start of
/// a cutscene already running).
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "to_owner")]
pub struct CutsceneStart {
    /// The cutscene name every peer registered in its [`NetCutscenes`].
    #[replicate(max_len = 256)]
    pub name: String,
    /// The server tick the cutscene starts at.
    pub start_tick: u32,
}

/// The server's announced cutscenes: name, start tick and the peers that got it.
#[derive(Resource, Debug, Default)]
pub struct NetCutsceneServer {
    announced: BTreeMap<String, (u32, BTreeSet<PeerId>)>,
    sent: u64,
}

impl NetCutsceneServer {
    /// Announces `name` from `start_tick` to every peer now and to every peer that joins until
    /// [`end`](Self::end); the server's own [`NetCutscenes`] schedules it too (a listen server
    /// plays it; a dedicated server without the name registered only logs).
    pub fn announce(&mut self, name: impl Into<String>, start_tick: u32) {
        self.announced
            .entry(name.into())
            .or_insert_with(|| (start_tick, BTreeSet::new()));
    }

    /// Stops announcing `name` to new peers.
    pub fn end(&mut self, name: &str) {
        self.announced.remove(name);
    }

    /// Announcements sent since the app started.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent
    }
}

/// Server: sends each announcement to the peers that do not have it yet.
fn announce(
    mut server: ResMut<NetCutsceneServer>,
    mut sender: NetSender,
    mut cutscenes: ResMut<NetCutscenes>,
) {
    if server.announced.is_empty() {
        return;
    }
    let peers = sender.peers();
    let mut sent = 0u64;
    for (name, (tick, got)) in &mut server.announced {
        cutscenes.schedule(name.clone(), u64::from(*tick));
        for &peer in &peers {
            if got.contains(&peer) {
                continue;
            }
            let msg = CutsceneStart {
                name: name.clone(),
                start_tick: *tick,
            };
            if sender.send_session_to_peer(peer, msg).is_ok() {
                got.insert(peer);
                sent += 1;
            }
        }
    }
    server.sent += sent;
}

/// Every peer: schedules the announced starts.
fn receive(messages: Option<Res<NetMessages<CutsceneStart>>>, mut cutscenes: ResMut<NetCutscenes>) {
    let Some(messages) = messages else {
        return;
    };
    for received in messages.iter() {
        cutscenes.schedule(received.msg.name.clone(), u64::from(received.msg.start_tick));
    }
}

/// Every peer: the session tick is the cutscene clock's reference.
fn follow_tick(node: Option<Res<NetNode>>, mut cutscenes: ResMut<NetCutscenes>) {
    if let Some(node) = node {
        cutscenes.set_tick(u64::from(node.tick.0));
    }
}

/// Carries net cutscene starts over `regia_net`. Opt-in: it registers the [`CutsceneStart`]
/// message, which is part of the session's protocol, so add it on every process of a session (after
/// `NetPlugin` and the `SequencerPlugin`), in the same order relative to other net messages.
pub struct NetCutscenePlugin;

impl Plugin for NetCutscenePlugin {
    fn build(&self, app: &mut App) {
        super::super::player::install(app);
        app.init_resource::<NetCutsceneServer>()
            .add_net_message::<CutsceneStart>()
            .add_systems(
                FixedUpdate,
                (receive, announce, follow_tick)
                    .chain()
                    .after(NetSet::Apply)
                    .run_if(resource_exists::<NetNode>),
            )
            .add_systems(
                PostUpdate,
                follow_tick
                    .before(run_cutscenes)
                    .run_if(resource_exists::<NetNode>),
            );
    }
}
