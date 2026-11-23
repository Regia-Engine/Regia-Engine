// SPDX-License-Identifier: MIT-0
//! Destruction level 2 end to end: pre-fractured chunks, the server decides breaks, clients play
//! them (ADR-2573, guide `book/agents/destruction-net.md`).
//!
//! One process holds three apps: a dedicated server, client 1, and client 0, the host. Client 0 is
//! the app a caller runs. It can draw ([`view::LabView`]) and answer the agent, and each of its fixed
//! ticks runs one frame of the other two ([`drive_lab`]), so pausing or stepping it with `play.pause`
//! and `play.step` moves the whole lab in lockstep. Datagrams cross from app to app through each
//! process's `NetNode`, the in-memory path the destruction network tests use.
//!
//! An explosion goes off on the server ([`LabSession::fire`], `lab.fire`). `ExplosionPlugin` queues
//! the damage there, and the break events reach the clients through `DestructionNetPlugin`. The
//! blast itself reaches them as the game's own message ([`LabBlast`]), so their `ExplosionPlugin`
//! pushes the debris they are already simulating. Nothing else about a blast runs on a client.
//!
//! The library takes its kit from the caller ([`LabKit`]): a sample's runtime may not link the
//! pipeline, so the tests and the example cut the wall and the tower with the fracture tool (see
//! `kit/mod.rs`).

pub mod agent;
pub mod view;
pub mod wire;

use std::sync::Arc;

use regia::app::enable_fixed_step;
use regia::core::math::Vec3;
use regia::framework::destruction_draw::DestructionMeshSet;
use regia::framework::destruction_fx::{
    DESTRUCTION_FX_FORMAT_VERSION, DestructionFxPlugin, DestructionFxRow, DestructionFxState,
    DestructionFxStats, DestructionFxTable, FxDecal,
};
use regia::framework::destruction_net::{
    DestructionNet, DestructionNetPlugin, DestructionPlayback, destruction_net_client, destruction_playback,
};
use regia::framework::explosion::{
    Explosion, ExplosionEvent, ExplosionPlugin, ExplosionStats, apply_explosions, explode,
};
use regia::net::message::{NetMessageAppExt, NetMessages, NetSender};
use regia::net::model::{LocalPeer, NetMode, PeerId};
use regia::net::runtime::{NetNode, net_receive, server_add_peer};
use regia::net::{NetPlugin, NetSet};
use regia::physics::PhysicsPlugin;
use regia::physics::destruction::{
    Authority, DestructibleAsset, DestructiblePose, Destruction, DestructionError, DestructionPlugin,
    add_fixed_box,
};
use regia::physics::world::PhysicsWorld;
use regia::prelude::*;
use regia_render::{CpuMaterial, CpuMesh};
use serde::Serialize;

pub use wire::LabBlast;

/// Fixed ticks per second of all three apps.
pub const TICK_HZ: f64 = 60.0;
/// Top of the ground plate (meters).
pub const GROUND_Y: f32 = 0.0;
/// Half extents of the ground plate (meters).
pub const GROUND_HALF: [f32; 3] = [40.0, 0.5, 40.0];
/// The first client's peer: the host.
pub const PEER_HOST: PeerId = PeerId(1);
/// The second client's peer.
pub const PEER_OTHER: PeerId = PeerId(2);

/// What one structure looks like, on the client that draws.
#[derive(Clone, Debug)]
pub struct LabLook {
    /// Chunk `i` here is chunk `i` of the asset.
    pub set: DestructionMeshSet,
    /// The whole structure, drawn until it first breaks.
    pub intact: Arc<CpuMesh>,
    /// The whole structure's material.
    pub material: Arc<CpuMaterial>,
}

/// One pre-fractured structure.
#[derive(Clone, Debug)]
pub struct LabStructure {
    /// The chunks and bonds.
    pub asset: Arc<DestructibleAsset>,
    /// Where the server places it.
    pub pose: DestructiblePose,
    /// Seed of its chunk velocities and effects.
    pub seed: u64,
    /// How it draws; `None` for a headless run.
    pub look: Option<LabLook>,
}

/// Everything the lab plays with.
#[derive(Clone, Debug, Default)]
pub struct LabKit {
    /// The structures, in the order the server defines them.
    pub structures: Vec<LabStructure>,
}

/// An explosion at a fixed tick of the lab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScriptedBlast {
    /// The lab tick it goes off on (1 is the first).
    pub tick: u64,
    /// The explosion.
    pub explosion: Explosion,
}

/// The default script: a blast into the wall, a second one while its chunks fly, one at the tower.
#[must_use]
pub fn default_script() -> Vec<ScriptedBlast> {
    vec![
        ScriptedBlast {
            tick: 30,
            explosion: wall_blast(),
        },
        ScriptedBlast {
            tick: 42,
            explosion: wall_second_blast(),
        },
        ScriptedBlast {
            tick: 60,
            explosion: tower_blast(),
        },
    ]
}

/// The blast in front of the wall's middle.
#[must_use]
pub fn wall_blast() -> Explosion {
    Explosion {
        radius: 3.0,
        impulse: 9_000.0,
        ..Explosion::barrel(Vec3::new(WALL_X, 1.2, 1.0))
    }
}

/// A smaller blast behind the wall, which throws what the first one broke back through the hole.
#[must_use]
pub fn wall_second_blast() -> Explosion {
    Explosion {
        radius: 4.0,
        impulse: 3_000.0,
        ..Explosion::grenade(Vec3::new(WALL_X, 1.5, -3.0))
    }
}

/// The blast at the foot of the tower.
#[must_use]
pub fn tower_blast() -> Explosion {
    Explosion {
        radius: 3.0,
        impulse: 20_000.0,
        ..Explosion::barrel(Vec3::new(TOWER_X - 1.3, 0.8, 0.0))
    }
}

/// Where the wall's middle stands, along X (meters).
pub const WALL_X: f32 = -4.0;
/// Where the tower's axis stands, along X (meters).
pub const TOWER_X: f32 = 4.0;

/// One process of the lab.
fn process(mode: NetMode, peer: PeerId, kit: &LabKit) -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, TICK_HZ);
    let authority = if mode.is_client() {
        Authority::Replica
    } else {
        Authority::Server
    };
    app.add_plugins((
        PhysicsPlugin,
        NetPlugin { mode },
        DestructionPlugin { authority },
        DestructionNetPlugin,
        ExplosionPlugin,
        // Sound, dust and scorch marks for every break, from one table (ADR-2734).
        DestructionFxPlugin {
            table: Some(lab_fx_table()),
        },
    ));
    app.add_net_message::<LabBlast>();
    app.add_systems(
        FixedUpdate,
        (
            announce_blasts.after(apply_explosions).before(NetSet::Send),
            hear_blasts
                .after(NetSet::Apply)
                .after(destruction_net_client)
                .before(apply_explosions),
        ),
    );
    app.insert_resource(LocalPeer(Some(peer)));
    add_fixed_box(
        &mut app.world_mut().resource_mut::<PhysicsWorld>(),
        [0.0, GROUND_Y - GROUND_HALF[1], 0.0],
        GROUND_HALF,
    );
    let mut net = app.world_mut().resource_mut::<DestructionNet>();
    for structure in &kit.structures {
        net.register_asset(structure.asset.clone());
    }
    app
}

/// Server: tells the clients about each blast, so they push their own debris.
fn announce_blasts(mode: Res<NetMode>, mut events: MessageReader<ExplosionEvent>, mut sender: NetSender) {
    if !mode.is_server() {
        events.clear();
        return;
    }
    for ExplosionEvent(explosion) in events.read() {
        if sender.broadcast_session(LabBlast(*explosion)).is_err() {
            warn!(target: "destruction_lab", "a blast did not go out");
        }
    }
}

/// Client: a blast the server announced goes off here too. A replica pushes its debris, no more.
fn hear_blasts(
    mode: Res<NetMode>,
    inbox: Option<Res<NetMessages<LabBlast>>>,
    mut events: MessageWriter<ExplosionEvent>,
) {
    if !mode.is_client() {
        return;
    }
    let Some(inbox) = inbox else {
        return;
    };
    for received in inbox.iter() {
        // The wire checks nothing about what the numbers mean.
        if received.msg.0.validate().is_ok() {
            events.write(ExplosionEvent(received.msg.0));
        }
    }
}

/// The two apps the host runs, with the lab's script. A non-send resource of the host: an `App`
/// stays on its thread.
pub struct LabSession {
    server: App,
    other: App,
    script: Vec<ScriptedBlast>,
    pending: Vec<Explosion>,
    ticks: u64,
}

impl LabSession {
    /// Fires `explosion` on the server on the next tick.
    pub fn fire(&mut self, explosion: Explosion) {
        self.pending.push(explosion);
    }

    /// Lab ticks so far.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// The server's world.
    #[must_use]
    pub fn server_world(&self) -> &World {
        self.server.world()
    }

    /// The other client's world.
    #[must_use]
    pub fn other_world(&self) -> &World {
        self.other.world()
    }

    /// The other client's world, to disturb it in a test.
    pub fn other_world_mut(&mut self) -> &mut World {
        self.other.world_mut()
    }
}

/// The host's fixed tick starts here: what the clients sent goes to the server, the server runs a
/// frame, what it sent goes to both clients, and the other client runs a frame. Then the host's own
/// tick goes on from the same datagrams. (Span `lab_drive`.)
pub fn drive_lab(world: &mut World) {
    let _span = tracing::debug_span!(target: "destruction_lab", "lab_drive").entered();
    let Some(mut lab) = world.remove_non_send::<LabSession>() else {
        return;
    };
    lab.ticks += 1;
    let host_up = std::mem::take(&mut world.resource_mut::<NetNode>().outbox);
    let other_up = std::mem::take(&mut lab.other.world_mut().resource_mut::<NetNode>().outbox);
    {
        let mut node = lab.server.world_mut().resource_mut::<NetNode>();
        for (peer, datagrams) in [(PEER_HOST, host_up), (PEER_OTHER, other_up)] {
            node.inbox.extend(datagrams.into_iter().map(|mut d| {
                d.peer = peer;
                d
            }));
        }
    }
    let now = lab.ticks;
    let scripted = lab.script.iter().filter(|b| b.tick == now).map(|b| b.explosion);
    let fired: Vec<Explosion> = scripted.chain(lab.pending.drain(..)).collect();
    for explosion in fired {
        if let Err(e) = explode(lab.server.world_mut(), explosion) {
            warn!(target: "destruction_lab", "a blast was refused: {}", e.code());
        }
    }
    lab.server.update();
    let down = std::mem::take(&mut lab.server.world_mut().resource_mut::<NetNode>().outbox);
    for d in down {
        if d.peer == PEER_HOST {
            world.resource_mut::<NetNode>().inbox.push(d);
        } else {
            lab.other.world_mut().resource_mut::<NetNode>().inbox.push(d);
        }
    }
    lab.other.update();
    world.insert_non_send(lab);
    // `PreUpdate` already ran for the host: take this tick's datagrams in now.
    net_receive(world);
}

/// Builds the lab: client 0's app, holding the server and client 1. Add a renderer, [`view::LabView`]
/// and the agent to the result as wanted, then run it.
///
/// # Errors
/// The server refused a structure.
pub fn lab_app(kit: &LabKit, script: Vec<ScriptedBlast>) -> Result<App, DestructionError> {
    let mut server = process(NetMode::DedicatedServer, PeerId::SERVER, kit);
    server_add_peer(server.world_mut(), PEER_HOST);
    server_add_peer(server.world_mut(), PEER_OTHER);
    server
        .world_mut()
        .resource_scope(|world, mut d: Mut<Destruction>| -> Result<(), DestructionError> {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            for s in &kit.structures {
                d.define(&mut physics, s.asset.clone(), s.pose, s.seed)?;
            }
            Ok(())
        })?;
    let mut other = process(NetMode::Client, PEER_OTHER, kit);
    server.finish_plugins();
    other.finish_plugins();
    let mut host = process(NetMode::Client, PEER_HOST, kit);
    host.world_mut().insert_non_send(LabSession {
        server,
        other,
        script,
        pending: Vec::new(),
        ticks: 0,
    });
    host.add_systems(FixedUpdate, drive_lab.before(NetSet::Apply));
    Ok(host)
}

/// One process in a [`LabReport`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProcessReport {
    /// What it plays.
    pub playback: DestructionPlayback,
    /// Whether it applied everything it got and lost no step.
    pub in_step: bool,
    /// Hash checks that differed.
    pub checks_failed: u64,
    /// Explosions processed.
    pub explosions: u64,
    /// Flying chunks pushed.
    pub chunks_pushed: u64,
    /// Chunks the blasts reached (see `ExplosionStats`).
    pub chunks_hit: u64,
    /// What the destruction effects table played (see `DestructionFxStats`).
    pub fx: DestructionFxStats,
}

fn report_of(world: &World) -> Option<ProcessReport> {
    let d = world.get_resource::<Destruction>()?;
    let mode = world.get_resource::<NetMode>().copied().unwrap_or_default();
    let net = world.get_resource::<DestructionNet>()?;
    let stats = world
        .get_resource::<ExplosionStats>()
        .copied()
        .unwrap_or_default();
    Some(ProcessReport {
        playback: destruction_playback(d, world.get_resource::<PhysicsWorld>()),
        in_step: net.in_step(mode),
        checks_failed: net.stats().checks_failed,
        explosions: stats.explosions,
        chunks_pushed: stats.chunks_pushed,
        chunks_hit: stats.chunks_hit,
        fx: world
            .get_resource::<DestructionFxState>()
            .map(|f| f.player.stats())
            .unwrap_or_default(),
    })
}

/// The lab's effects table: every tag falls back to `default` (the fracture tool's tag). The repo
/// holds no sound or particle assets for the lab, so a cue or system the app has not registered
/// is counted as refused or unknown; the decals are plain tints.
#[must_use]
pub fn lab_fx_table() -> DestructionFxTable {
    DestructionFxTable {
        format_version: DESTRUCTION_FX_FORMAT_VERSION,
        name: "destruction_lab".into(),
        max_per_step: 24,
        fallback: Some("default".into()),
        rows: vec![DestructionFxRow {
            tag: "default".into(),
            kinds: Vec::new(),
            sound: Some("destruction_crack".into()),
            vfx: Some("destruction_dust".into()),
            decal: Some(FxDecal {
                half_size: [0.35, 0.35, 0.25],
                color: [0.08, 0.07, 0.06, 0.8],
                lifetime_secs: 30.0,
            }),
            max_per_step: 12,
            min_magnitude: 0.0,
            scale_jitter: 0.3,
        }],
    }
}

/// What the three processes play (agent `lab.report`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LabReport {
    /// Lab ticks so far.
    pub ticks: u64,
    /// The server.
    pub server: ProcessReport,
    /// Client 0, then client 1.
    pub clients: [ProcessReport; 2],
    /// Both clients show the same bonds broken and every chunk in the same place.
    pub clients_agree: bool,
}

/// Reads all three processes of the lab in `world`, the host's; `None` when it is not a lab.
#[must_use]
pub fn lab_report(world: &World) -> Option<LabReport> {
    let lab = world.get_non_send::<LabSession>()?;
    let host = report_of(world)?;
    let other = report_of(lab.other_world())?;
    let clients_agree = host.playback == other.playback;
    Some(LabReport {
        ticks: lab.ticks(),
        server: report_of(lab.server_world())?,
        clients: [host, other],
        clients_agree,
    })
}
