// SPDX-License-Identifier: MIT-0
//! Net Game: the template networked game (`regia new --template net-game`, ADR-2039).
//!
//! One crate of shared simulation that a dedicated server and a client both run:
//!
//! - **Transport**: [`regia::net::NetLink`] over raw UDP with encryption required (ADR-1901), or
//!   any other `DatagramSocket` (the in-memory network in tests, Steam's relay through
//!   `regia::steam::SteamSocket`).
//! - **Replication**: every avatar is a replicated entity with an [`Avatar`]; the engine's
//!   match state replication ([`MatchStateReplicationPlugin`]) adds its ledger row as a
//!   [`PlayerScoreView`].
//! - **Prediction** (spec 73): each client predicts its own avatar from its [`Pad`] commands; the
//!   server runs the same [`move_avatars`] for every player.
//! - **Interpolation** (spec 74): the other players' avatars are proxies, shown between two server
//!   states on the link's clock.
//! - **Lag compensation** (spec 75): a shot is a ray judged on the server against the hit shapes
//!   as the shooter saw them.
//! - **Lobbies** (spec 89): [`lobby`] publishes and follows the server address through
//!   [`regia::net::Lobbies`].
//! - **Dedicated server** (spec 76, ADR-2440): [`server`] runs the server side under the engine's
//!   server host (`server.toml`, console, `/health`, join codes); the link binds the game port.
//!
//! The rules: players run around a square arena; a `fire` command tags whoever the ray hits. The
//! server keeps the tally in the engine's match session ([`MatchRules`]): a joining player gets a
//! seat on the team with fewer players (two teams, [`TeamId`] 1 and 2), its avatar starts at one of
//! that team's [`PlayerStart`]s ([`team_starts`], picked by the session's spawn policy), a tag
//! credits the shooter's seat a kill and the target's seat a death, and clients read it from the
//! replicated [`PlayerScoreView`] ([`Tally::tags`] is its kills, [`Tally::tagged`] its deaths) and
//! the team totals from the replicated `TeamScoresView`.
//! Strip points for your own game are marked `TODO(template)`.

use regia::core::math::{Ray, Vec3};
use regia::framework::player::{PlayerStart, SpawnPoint};
use regia::framework::rules::match_flow::{FlowInput, SeatId};
use regia::framework::rules::session::{MatchSession, SessionSettings};
use regia::framework::rules::spawn::SpawnPolicy;
use regia::framework::rules::teams::{SeatRequest, TeamDef};
use regia::framework::rules::{
    LedgerSlot, MatchRules, MatchSessionPlugin, MatchStateReplicationPlugin, PlayerScoreView, TeamId,
};
use regia::net as regia_net;
use regia::prelude::*;
use regia_net::interp::install_interpolation;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::replication::NetPredicted;
use regia_net::runtime::NetNode;
use regia_net::{
    CryptoMode, HitFilter, HitShape, HitShapeKind, HitShapes, InputCommand, LagCompPlugin, LagCompensation,
    NetGame, NetInput, NetInputAppExt, NetMode, NetOwnedLocally, NetOwner, NetPlayer, NetPlayerId, NetPlugin,
    NetRole, NetSet, NetSettings, PoseSource, PredictAppExt, PredictedScheduleExt, PredictionCx, Replicate,
    ReplicateAppExt, ReplicateOptions, Replicated, Tolerance, TransformReplication,
};
use regia_scene::components::Name;
use regia_scene::transform::Transform;

#[cfg(feature = "client")]
pub mod hud;
pub mod lobby;
pub mod server;
#[cfg(feature = "client")]
pub mod view;
// The template match over Steam, the N08 leg of the owner's two-account run (ADR-5375).
#[cfg(feature = "steam")]
pub mod steam_leg;
// The windowed client's SteamPlugin and its HUD line for the overlay check (ADR-7317).
#[cfg(feature = "steam")]
pub mod steam_overlay;

/// Fixed simulation rate of server and clients.
pub const TICK_HZ: f64 = 60.0;
/// Run speed in meters per second.
pub const RUN_SPEED: f32 = 6.0;
/// Half the side of the square arena, in meters.
pub const ARENA_HALF: f32 = 12.0;
/// How far a shot reaches.
pub const SHOT_RANGE: f32 = 60.0;
/// Height of a capsule's center above the avatar's feet.
pub const BODY_CENTER: f32 = 1.0;
/// How far a predicted position may be from the server's before the owner corrects (1 mm). Only
/// a command that reached the server too late can make them differ, so a tight tolerance costs
/// almost nothing and keeps every process within a millimeter.
pub const PREDICTION_TOLERANCE_M: f32 = 0.001;
/// Default UDP port of the server.
pub const DEFAULT_PORT: u16 = 7777;

/// The game's identity on the wire. A client of another game, or of another
/// [`NetGame::PROTOCOL_VERSION`], is refused at the handshake.
/// TODO(template): give your game its own id; bump the version on every wire change.
pub struct NetGameWire;

impl NetGame for NetGameWire {
    type Message = regia_net::bridge::NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "com.regiaengine.net_game";
    const PROTOCOL_VERSION: u32 = 2;
}

/// One player's command for one tick (spec 73 § 5).
#[derive(Clone, Debug, PartialEq, Default, Replicate)]
pub struct Pad {
    /// Run direction on the ground plane, x component, in `-1..=1`.
    pub move_x: f32,
    /// Run direction on the ground plane, z component, in `-1..=1`.
    pub move_z: f32,
    /// Fire this tick.
    pub fire: bool,
    /// The point the shot goes through.
    pub aim: Vec3,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        // A lost command keeps running but never fires twice.
        Self {
            fire: false,
            ..last.clone()
        }
    }
}

/// A player's avatar: replicated to everyone, predicted by its owner.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
pub struct Avatar {
    /// Join order on the server (0 for the first player); picks a color on clients.
    pub slot: u32,
}

/// An avatar's tally as every process reads it: the replicated [`PlayerScoreView`] of its seat
/// (zero until the first view arrives).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Tally {
    /// Players this avatar's owner tagged (the seat's kills).
    pub tags: u32,
    /// Times this avatar was tagged (the seat's deaths).
    pub tagged: u32,
}

impl Tally {
    /// The tally of a score view (`None`: nothing yet).
    pub fn of(view: Option<&PlayerScoreView>) -> Self {
        view.map_or_else(Self::default, |v| Self {
            tags: v.kills,
            tagged: v.deaths,
        })
    }
}

/// The number of teams. A joining player takes the team with fewer players ([`TeamId`] 1 or 2).
pub const TEAMS: u8 = 2;

/// Seats on each team.
pub const SEATS_PER_TEAM: u8 = 127;

/// The team colors (RGBA): team 1, team 2.
pub const TEAM_COLORS: [[u8; 4]; 2] = [[220, 60, 50, 255], [60, 110, 230, 255]];

/// The number of [`PlayerStart`]s the server places ([`team_starts`]).
pub const START_COUNT: u32 = 8;

/// The server's match session: two teams of [`SEATS_PER_TEAM`] seats, scored by the ledger, and a
/// round-robin spawn policy (each team's starts in turn).
/// TODO(template): your modes, teams and limits (`regia::framework::rules::session`).
pub fn tag_session() -> MatchSession {
    let mut settings = SessionSettings::default();
    settings.flow.max_teams = TEAMS;
    settings.flow.seats_per_team = SEATS_PER_TEAM;
    settings.objectives.max_teams = TEAMS;
    settings.teams.defs = (1..=TEAMS)
        .map(|team| TeamDef::unnamed(team, TEAM_COLORS[usize::from(team - 1)], SEATS_PER_TEAM))
        .collect();
    settings.respawn.spawn = SpawnPolicy::RoundRobin;
    MatchSession::new(settings)
}

/// The server's spawn points: [`START_COUNT`] [`PlayerStart`]s at [`spawn_point`]`(i)`, start `i`
/// for team `1 + i % 2` ([`SpawnPoint::for_team`]). Players join on alternate teams, so the `n`-th
/// player starts at `spawn_point(n)`.
/// TODO(template): place your starts in the level instead (`PlayerStart` + `SpawnPoint`).
pub fn team_starts() -> Vec<(Vec3, u8)> {
    (0..START_COUNT)
        .map(|i| (spawn_point(i), 1 + (i % 2) as u8))
        .collect()
}

/// Server, startup: spawns [`team_starts`] as [`PlayerStart`] entities.
fn spawn_team_starts(mut commands: Commands) {
    for (i, (position, team)) in team_starts().into_iter().enumerate() {
        commands.spawn((
            Name::new(format!("Start{i}")),
            Transform::from_translation(position),
            PlayerStart,
            SpawnPoint::for_team(team),
        ));
    }
}

/// Server: what the shots did.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct ShotLog {
    /// Fired commands judged.
    pub shots: u32,
    /// Shots that hit an avatar.
    pub hits: u32,
    /// Shots judged at a tick earlier than the server's present (rewound).
    pub rewound: u32,
    /// The newest judged command tick per player (a resimulated tick is judged once).
    last_tick: Vec<(NetPlayerId, u32)>,
}

/// Server: the next slot to hand out.
#[derive(Resource, Debug, Default)]
struct Slots {
    next: u32,
}

/// Which side this app plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The dedicated server: simulates everyone, judges shots, owns the scores.
    Server,
    /// A client: predicts its own avatar, interpolates the rest.
    Client,
}

/// The game plugin: simulation for both sides. Presentation (meshes, camera, keyboard) is the
/// `view` module behind the `client` feature, so the server links no renderer.
pub struct NetGamePlugin {
    /// Which side this app plays.
    pub side: Side,
}

impl Plugin for NetGamePlugin {
    fn build(&self, app: &mut App) {
        let mode = match self.side {
            Side::Server => NetMode::DedicatedServer,
            Side::Client => NetMode::Client,
        };
        app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
        // Registration order feeds the protocol hash: identical on both sides.
        app.replicate::<Avatar>(ReplicateOptions::default());
        // The engine's match views (`PlayerScoreView` on each avatar, the match-state entity).
        app.add_plugins(MatchStateReplicationPlugin);
        app.add_net_input::<Pad>()
            .predict::<Transform>(Tolerance::position(PREDICTION_TOLERANCE_M))
            .add_predicted_systems((move_avatars, judge_shots).chain());
        app.init_resource::<PadSource>().init_resource::<ShotLog>();
        app.add_systems(FixedUpdate, feed_pad.after(NetSet::Apply).before(NetSet::Predict));
        match self.side {
            Side::Server => {
                app.insert_resource(MatchRules {
                    session: tag_session(),
                })
                .add_plugins(MatchSessionPlugin);
                app.add_plugins(LagCompPlugin);
                app.world_mut()
                    .resource_mut::<NetSettings>()
                    .lag_compensation
                    .enabled = true;
                // Spawn order is the starts' arrival order (installed before any start exists).
                if let Err(error) =
                    regia::ecs::spawn_order::try_track_spawn_order::<PlayerStart>(app.world_mut())
                {
                    warn!("player starts keep position order: {error:?}");
                }
                app.init_resource::<Slots>()
                    .add_systems(Startup, spawn_team_starts)
                    .add_systems(
                        FixedUpdate,
                        sync_avatars.after(NetSet::Apply).before(NetSet::Predict),
                    );
                // Under the server host the link binds the game port (a test may insert its own).
                if !app
                    .world()
                    .contains_resource::<regia_net::server_host::HostedLink>()
                {
                    app.insert_resource(server::hosted_link());
                }
            }
            Side::Client => {
                install_interpolation(app.world_mut(), TICK_HZ, 1.0);
                app.add_plugins(regia_net::link::LinkInterpolationPlugin);
                app.init_resource::<LinkBanner>().add_systems(
                    PreUpdate,
                    follow_link
                        .after(regia_net::link::net_link_receive)
                        .before(NetSet::Receive),
                );
            }
        }
    }
}

/// What the player sees of the link to the server (spec 120 § 10.2 and § 10.3, ADR-8352). The
/// client follows the link's events into it, and the HUD shows a reconnecting overlay from it.
///
/// TODO(template): a game that pauses input or shows a menu on a lost link reads this too.
#[derive(Resource, Debug, Clone, PartialEq, Eq, Default)]
pub enum LinkBanner {
    /// Connected, traffic flowing (or not connected yet).
    #[default]
    Online,
    /// The server went quiet (`PeerInterrupted`); the game keeps running.
    Interrupted,
    /// The connection timed out and the client reconnects by itself (`Reconnecting`).
    Reconnecting {
        /// The attempt, from 1.
        attempt: u32,
    },
    /// The link ended for good (`ReconnectFailed` or `Ended`).
    Lost {
        /// The lowercase reason code.
        reason: String,
    },
}

impl LinkBanner {
    /// Follows one link event.
    pub fn apply(&mut self, event: &LinkEvent) {
        match event {
            LinkEvent::PeerInterrupted { .. } => *self = Self::Interrupted,
            LinkEvent::Reconnecting { attempt } => *self = Self::Reconnecting { attempt: *attempt },
            LinkEvent::PeerRestored { .. } | LinkEvent::Connected { .. } => *self = Self::Online,
            LinkEvent::ReconnectFailed { reason, .. } | LinkEvent::Ended { reason } => {
                *self = Self::Lost {
                    reason: reason.clone(),
                }
            }
            _ => {}
        }
    }

    /// The overlay's line; `None` while online.
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Online => None,
            Self::Interrupted => Some("Connection interrupted".to_owned()),
            Self::Reconnecting { attempt } => Some(format!("Reconnecting (attempt {attempt})")),
            Self::Lost { reason } => Some(format!("Disconnected: {reason}")),
        }
    }
}

/// Client: the link's events into [`LinkBanner`]. The client side of this game reads no other link
/// event, so it takes them.
fn follow_link(link: Option<ResMut<NetLink>>, mut banner: ResMut<LinkBanner>) {
    let Some(mut link) = link else {
        return;
    };
    for event in link.take_events() {
        banner.apply(&event);
    }
}

/// Where the client's commands come from.
#[derive(Resource, Debug, Clone, Default)]
pub enum PadSource {
    /// Nothing: stand still (and the server's own app).
    #[default]
    Idle,
    /// Written by the client each frame (keyboard, gamepad, an agent).
    Manual(Pad),
    /// A deterministic bot (tests, soak runs, the `--bot` flag).
    Bot(Bot),
}

/// A scripted player: runs a circle and fires at the nearest other avatar it sees.
#[derive(Debug, Clone, PartialEq)]
pub struct Bot {
    /// Phase of its circle, in radians.
    pub phase: f32,
    /// Fire every this many ticks (0 never fires).
    pub fire_every: u32,
    /// Fixed steps run so far.
    pub step: u32,
    /// Stop moving and firing after this many steps (`u32::MAX` never stops).
    pub stop_after: u32,
}

impl Bot {
    /// A bot with its own circle phase that fires every `fire_every` ticks for `stop_after` steps.
    pub fn new(phase: f32, fire_every: u32, stop_after: u32) -> Self {
        Self {
            phase,
            fire_every,
            step: 0,
            stop_after,
        }
    }

    /// The command for this step, aiming at `target` (the shown center of another avatar).
    pub fn next(&mut self, target: Option<Vec3>) -> Pad {
        let step = self.step;
        self.step = self.step.saturating_add(1);
        if step >= self.stop_after {
            return Pad::default();
        }
        // One lap every four seconds.
        let angle = self.phase + step as f32 * std::f32::consts::TAU / (4.0 * TICK_HZ as f32);
        let fire = self.fire_every > 0 && step > 0 && step.is_multiple_of(self.fire_every);
        Pad {
            move_x: -angle.sin(),
            move_z: angle.cos(),
            fire: fire && target.is_some(),
            aim: target.unwrap_or_default(),
        }
    }
}

/// Avatars used to locate the local player and choose a remote aim target.
type AvatarQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static Transform,
        Option<&'static NetRole>,
        Option<&'static NetOwnedLocally>,
    ),
    With<Avatar>,
>;

/// Client, fixed step: writes this tick's command from the [`PadSource`].
fn feed_pad(
    mut source: ResMut<PadSource>,
    mode: Res<NetMode>,
    mut commands: Query<&mut InputCommand<Pad>>,
    others: AvatarQuery,
) {
    if mode.is_server() {
        return;
    }
    let pad = match &mut *source {
        PadSource::Idle => Pad::default(),
        PadSource::Manual(pad) => pad.clone(),
        PadSource::Bot(bot) => {
            let here = others
                .iter()
                .find(|(_, _, mine)| mine.is_some())
                .map_or(Vec3::ZERO, |(t, _, _)| t.translation);
            // The nearest other avatar, as the documentation of `Bot` says.
            let proxies = others
                .iter()
                .filter(|(_, role, _)| matches!(role, Some(NetRole::Proxy)))
                .map(|(t, _, _)| t.translation);
            bot.next(nearest_center(here, proxies))
        }
    };
    for mut command in &mut commands {
        command.value = pad.clone();
    }
}

/// The shown center of the avatar nearest `here`: what the bot and the keyboard aim at.
pub fn nearest_center(here: Vec3, avatars: impl IntoIterator<Item = Vec3>) -> Option<Vec3> {
    avatars
        .into_iter()
        .min_by(|a, b| a.distance(here).total_cmp(&b.distance(here)))
        .map(|t| t + Vec3::Y * BODY_CENTER)
}

type InputQuery<'w, 's> = Query<'w, 's, (&'static NetPlayer, &'static InputCommand<Pad>)>;

/// An avatar's position and who simulates it.
type AvatarMotion = (
    &'static mut Transform,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);

fn pad_of(inputs: &InputQuery, player: Option<NetPlayerId>) -> Option<Pad> {
    let player = player?;
    inputs
        .iter()
        .find(|(p, _)| p.id == player)
        .map(|(_, c)| c.value.clone())
}

/// One step of an avatar's movement: the same code on the server and in the owner's prediction.
pub fn step_position(position: Vec3, pad: &Pad) -> Vec3 {
    let dir = Vec3::new(pad.move_x, 0.0, pad.move_z);
    // Commands come off the network: a non-finite stick (a broken or hostile client) stands still
    // instead of making the server's position NaN for good (NaN survives the arena clamp).
    let dir = if !dir.is_finite() {
        Vec3::ZERO
    } else if dir.length_squared() > 1.0 {
        dir.normalize()
    } else {
        dir
    };
    let next = position + dir * (RUN_SPEED / TICK_HZ as f32);
    Vec3::new(
        next.x.clamp(-ARENA_HALF, ARENA_HALF),
        0.0,
        next.z.clamp(-ARENA_HALF, ARENA_HALF),
    )
}

/// Predicted schedule: moves every avatar this process simulates.
/// TODO(template): replace with your character movement (`regia::framework::character`).
pub fn move_avatars(cx: PredictionCx, inputs: InputQuery, mut avatars: Query<AvatarMotion, With<Avatar>>) {
    for (mut t, owner, role) in &mut avatars {
        if !cx.simulates(role) {
            continue;
        }
        if let Some(pad) = pad_of(&inputs, cx.owner_player(owner)) {
            t.translation = step_position(t.translation, &pad);
        }
    }
}

/// Minimum ticks between two judged shots of one player (6 shots per second at 60 Hz). A held
/// fire button, or a frame that runs several catch-up steps, still counts as one shot per cooldown.
pub const FIRE_COOLDOWN_TICKS: u32 = 10;

/// Whether a shot at `tick` is judged, given the tick of this player's previous judged shot.
pub fn shot_allowed(last: u32, tick: u32) -> bool {
    tick >= last.saturating_add(FIRE_COOLDOWN_TICKS)
}

/// Predicted schedule, server only: each fired command is one ray, judged where the shooter saw
/// the targets (spec 75).
fn judge_shots(world: &mut World) {
    if !world.resource::<NetMode>().is_server() {
        return;
    }
    let mut q = world.query::<(&NetPlayer, &InputCommand<Pad>)>();
    let shots: Vec<(NetPlayerId, u32, Vec3)> = q
        .iter(world)
        // A non-finite aim (from a broken or hostile client) is no shot.
        .filter(|(_, c)| c.value.fire && c.value.aim.is_finite())
        .map(|(p, c)| (p.id, c.tick.0, c.value.aim))
        .collect();
    for (shooter, tick, aim) in shots {
        let fresh = world
            .resource::<ShotLog>()
            .last_tick
            .iter()
            .find(|(p, _)| *p == shooter)
            .is_none_or(|(_, t)| shot_allowed(*t, tick));
        if !fresh {
            continue;
        }
        let Some(avatar) = avatar_of(world, shooter.peer) else {
            continue;
        };
        let origin = world
            .get::<Transform>(avatar)
            .map_or(Vec3::ZERO, |t| t.translation + Vec3::Y * BODY_CENTER);
        let judged = LagCompensation::new(world).ok().map(|lc| {
            let rewound = lc
                .rewind_tick(shooter)
                .is_ok_and(|at| at.0 < f64::from(lc.current_tick().0));
            let filter = HitFilter {
                exclude: vec![avatar],
                ..HitFilter::default()
            };
            let hit = lc
                .raycast(shooter, Ray::new(origin, aim - origin), SHOT_RANGE, &filter)
                .ok()
                .flatten();
            (hit.map(|h| h.entity), rewound)
        });
        let (hit, rewound) = judged.unwrap_or((None, false));
        {
            let mut log = world.resource_mut::<ShotLog>();
            log.shots += 1;
            log.rewound += u32::from(rewound);
            log.hits += u32::from(hit.is_some());
            match log.last_tick.iter_mut().find(|(p, _)| *p == shooter) {
                Some(entry) => entry.1 = tick,
                None => log.last_tick.push((shooter, tick)),
            }
        }
        if let Some(target) = hit {
            credit_tag(world, avatar, target);
        }
    }
}

/// The seat an avatar holds in the match session.
fn seat_of(world: &World, avatar: Entity) -> Option<SeatId> {
    let team = world.get::<TeamId>(avatar)?;
    let slot = world.get::<LedgerSlot>(avatar)?;
    Some(SeatId::new(team.0, slot.0))
}

/// Server: `shooter` tagged `target`: a kill on the shooter's seat, a death on the target's.
fn credit_tag(world: &mut World, shooter: Entity, target: Entity) {
    let (Some(killer), Some(victim)) = (seat_of(world, shooter), seat_of(world, target)) else {
        return;
    };
    if let Some(mut rules) = world.get_resource_mut::<MatchRules>() {
        rules.session.credit_kill(killer, victim);
    }
}

/// The avatar a peer owns.
fn avatar_of(world: &mut World, peer: regia_net::PeerId) -> Option<Entity> {
    let mut q = world.query_filtered::<(Entity, &NetOwner), With<Avatar>>();
    q.iter(world).find(|(_, o)| o.0 == peer).map(|(e, _)| e)
}

/// Prints `left slot=<n> x=<x> z=<z> tags=<n> tagged=<n>`: the final state of an avatar whose
/// player left, in the `summary` line's words. A match script judges a client that finished early
/// against it, because the server's `summary` at stop no longer has that avatar (ADR-3075).
fn print_left(world: &World, avatar: Entity) {
    use std::io::Write;
    let (Some(a), Some(t)) = (world.get::<Avatar>(avatar), world.get::<Transform>(avatar)) else {
        return;
    };
    let score = Tally::of(world.get::<PlayerScoreView>(avatar));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "left slot={} x={:.3} z={:.3} tags={} tagged={}",
        a.slot, t.translation.x, t.translation.z, score.tags, score.tagged
    );
    let _ = out.flush();
}

/// Server, fixed step: one avatar per connected peer. A new peer gets one at its slot's corner;
/// a peer that left loses its avatar.
fn sync_avatars(world: &mut World) {
    let peers = world.resource::<NetNode>().peers();
    let mut q = world.query_filtered::<(Entity, &NetOwner), With<Avatar>>();
    let owned: Vec<(Entity, regia_net::PeerId)> = q.iter(world).map(|(e, o)| (e, o.0)).collect();
    for (e, peer) in &owned {
        if !peers.contains(peer) {
            print_left(world, *e);
            if let Some(seat) = seat_of(world, *e)
                && let Some(mut rules) = world.get_resource_mut::<MatchRules>()
            {
                let _ = rules.session.apply(&FlowInput::Leave {
                    team: seat.team,
                    slot: seat.slot,
                });
            }
            world.despawn(*e);
        }
    }
    for peer in peers {
        if owned.iter().any(|(_, p)| *p == peer) {
            continue;
        }
        let slot = {
            let mut slots = world.resource_mut::<Slots>();
            let s = slots.next;
            slots.next += 1;
            s
        };
        // A seat on the team with fewer players, its lowest free slot (none when both teams are
        // full: that avatar is not scored).
        let seat = world.get_resource_mut::<MatchRules>().and_then(|mut rules| {
            rules
                .session
                .assign_team(&SeatRequest::default(), &[])
                .ok()
                .and_then(|seats| seats.first().copied())
        });
        let id = world
            .spawn((
                Name::new(format!("Avatar{slot}")),
                Replicated {
                    transform: TransformReplication::PositionOnly,
                    ..Replicated::default()
                },
                Transform::from_translation(spawn_point(slot)),
                NetPredicted,
                NetOwner(peer),
                Avatar { slot },
                HitShapes {
                    shapes: vec![
                        HitShape::new(
                            HitShapeKind::Capsule {
                                radius: 0.5,
                                half_height: 0.5,
                            },
                            "body",
                        )
                        .at(Vec3::Y * BODY_CENTER),
                    ],
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        if let Some(seat) = seat {
            world
                .entity_mut(id)
                .insert((TeamId(seat.team), LedgerSlot(seat.slot)));
            // The team's start the session's spawn policy picks (with no start, the slot's spot).
            let start = regia::framework::spawn_select::choose_spawn(world, id).map(|pick| pick.position);
            if let Some(start) = start
                && let Some(mut transform) = world.get_mut::<Transform>(id)
            {
                transform.translation = start;
            }
        }
    }
}

/// Where slot `slot` starts: around a circle of 6 m.
pub fn spawn_point(slot: u32) -> Vec3 {
    let angle = slot as f32 * 2.399_963; // the golden angle spreads any number of players
    Vec3::new(6.0 * angle.cos(), 0.0, 6.0 * angle.sin())
}

/// One avatar as a process sees it: the agreement check compares these across processes.
#[derive(Debug, Clone, PartialEq)]
pub struct AvatarView {
    /// Its slot.
    pub slot: u32,
    /// Its position.
    pub position: Vec3,
    /// Its tally (from the replicated [`PlayerScoreView`]).
    pub score: Tally,
}

/// Every avatar this world knows, sorted by slot.
pub fn avatars(world: &mut World) -> Vec<AvatarView> {
    let mut q = world.query::<(&Avatar, &Transform, Option<&PlayerScoreView>)>();
    let mut out: Vec<AvatarView> = q
        .iter(world)
        .map(|(a, t, s)| AvatarView {
            slot: a.slot,
            position: t.translation,
            score: Tally::of(s),
        })
        .collect();
    out.sort_by_key(|a| a.slot);
    out
}

/// Link options of this game: encryption required on every link (loopback too), the given clock
/// and conditioner.
pub fn link_options(clock: LinkClock, conditioner: regia_net::ConditionerConfig) -> LinkOptions {
    LinkOptions {
        crypto: CryptoMode::Required,
        conditioner,
        clock,
        tick_rate_hz: TICK_HZ as u32,
        ..LinkOptions::default()
    }
}

/// Builds a headless app of one side: the minimal plugins, the fixed step and [`NetGamePlugin`].
/// The server binary, the end-to-end test and the client's `--headless` mode use it.
pub fn headless_app(side: Side) -> App {
    let mut app = App::new();
    regia::app::enable_fixed_step(&mut app, TICK_HZ);
    app.add_plugins(NetGamePlugin { side });
    app
}

/// Starts the server side of `app` on UDP `bind`.
///
/// # Errors
///
/// The port cannot be bound; the message carries the link error's code.
pub fn host_udp(app: &mut App, bind: std::net::SocketAddr, opts: &LinkOptions) -> Result<(), String> {
    app.world_mut()
        .resource_mut::<NetLink>()
        .host_udp::<NetGameWire>(bind, opts)
        .map_err(|e| format!("{}: {e}", e.code()))
}

/// Starts the client side of `app` toward `server` over UDP.
///
/// # Errors
///
/// No port can be bound; the message carries the link error's code.
pub fn connect_udp(app: &mut App, server: std::net::SocketAddr, opts: &LinkOptions) -> Result<(), String> {
    app.world_mut()
        .resource_mut::<NetLink>()
        .connect_udp::<NetGameWire>(server, opts)
        .map_err(|e| format!("{}: {e}", e.code()))
}

/// One `netstats` line for match scripts (`cargo xtask net-match`, ADR-3075): `who` printed it,
/// then the link's state, peers, endpoint totals and conditioner counters, and for a client its
/// link counters (round trip, loss, bytes each way, resends) and prediction counters
/// (corrections, resets, resyncs, verify failures). `key=value` pairs separated by spaces; the
/// conditioner is `cond=` with its fields joined by commas.
pub fn net_stats_line(world: &World, who: &str) -> String {
    use std::fmt::Write as _;
    let mut line = format!("netstats who={who}");
    // A hosted server's stop hook runs after the host closed the port: its last status is kept.
    let status = world
        .get_resource::<NetLink>()
        .and_then(NetLink::status)
        .or_else(|| {
            world
                .get_resource::<regia_net::server_host::HostedLink>()
                .and_then(|h| h.final_status().cloned())
        });
    let Some(status) = status else {
        line.push_str(" state=none");
        return line;
    };
    let t = &status.totals;
    let c = &status.conditioner_stats;
    let _ = write!(
        line,
        " role={} state={} tick={} peers={} sealed_out={} datagrams_in={} datagrams_out={} malformed={} auth_failed={} \
         replayed={} socket_errors={} cond_passed={} cond_dropped={} cond_duplicated={} cond_delayed={} cond={}",
        status.role,
        status.state.replace(char::is_whitespace, "_"),
        status.tick,
        status.peer_count,
        t.sealed_out,
        t.datagrams_in,
        t.datagrams_out,
        t.malformed,
        t.auth_failed,
        t.replayed,
        t.socket_errors,
        c.passed,
        c.dropped,
        c.duplicated,
        c.delayed,
        status.conditioner.describe().replace(' ', ","),
    );
    if status.role == "server"
        && let Some(shots) = world.get_resource::<ShotLog>()
    {
        let _ = write!(
            line,
            " shots={} hits={} rewound={}",
            shots.shots, shots.hits, shots.rewound
        );
    }
    if let Some(l) = &status.link {
        let _ = write!(
            line,
            " rtt_ms={:.1} jitter_ms={:.1} loss={:.4} packets_sent={} packets_received={} packets_lost={} \
             bytes_sent={} bytes_received={} resends={} duplicates={} out_of_window={} inbound_dropped={}",
            l.rtt_ms,
            l.jitter_ms,
            l.loss_fraction,
            l.packets_sent,
            l.packets_received,
            l.packets_lost,
            l.bytes_sent,
            l.bytes_received,
            l.reliable_resends,
            l.duplicates,
            l.out_of_window,
            l.inbound_dropped,
        );
    }
    if status.role == "client"
        && let Some(p) = world.get_resource::<regia_net::predict::ClientPrediction>()
    {
        let s = &p.stats;
        let _ = write!(
            line,
            " corrections={} max_error_m={:.4} resets={} resyncs={} teleports={} verify_failures={}",
            s.corrections, s.max_error_m, s.resets, s.resyncs, s.teleports, s.verify_failures
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client's command is untrusted: NaN or infinite stick values used to make the server's
    /// avatar position NaN (the arena clamp keeps NaN), and every later step kept it there.
    #[test]
    fn a_non_finite_command_does_not_poison_the_position() {
        let start = Vec3::new(1.0, 0.0, -2.0);
        for (move_x, move_z) in [
            (f32::NAN, 0.0),
            (0.0, f32::NAN),
            (f32::INFINITY, 0.0),
            (f32::NEG_INFINITY, f32::INFINITY),
        ] {
            let pad = Pad {
                move_x,
                move_z,
                ..Pad::default()
            };
            let next = step_position(start, &pad);
            assert!(next.is_finite(), "{move_x} {move_z}: {next}");
            assert_eq!(next, start, "a broken command stands still");
        }
    }

    #[test]
    fn a_held_fire_button_is_judged_once_per_cooldown() {
        assert!(!shot_allowed(100, 100));
        assert!(!shot_allowed(100, 100 + FIRE_COOLDOWN_TICKS - 1));
        assert!(shot_allowed(100, 100 + FIRE_COOLDOWN_TICKS));
        // The first shot (last tick 0) is allowed once the cooldown has passed.
        assert!(shot_allowed(0, FIRE_COOLDOWN_TICKS));
    }

    #[test]
    fn movement_stays_in_the_arena_and_is_speed_limited() {
        let pad = Pad {
            move_x: 1.0,
            move_z: 1.0,
            ..Pad::default()
        };
        let next = step_position(Vec3::ZERO, &pad);
        assert!((next.length() - RUN_SPEED / TICK_HZ as f32).abs() < 1e-5);
        let edge = step_position(Vec3::new(ARENA_HALF, 0.0, 0.0), &pad);
        assert_eq!(edge.x, ARENA_HALF);
    }

    #[test]
    fn a_bot_fires_on_its_period_only_with_a_target_and_stops() {
        let mut bot = Bot::new(0.0, 10, 25);
        let fired: Vec<u32> = (0..40).filter(|_| bot.next(Some(Vec3::ONE)).fire).collect();
        assert_eq!(fired, vec![10, 20]);
        let mut blind = Bot::new(0.0, 10, 100);
        assert!((0..40).all(|_| !blind.next(None).fire));
    }

    #[test]
    fn bot_aims_at_the_nearest_proxy() {
        use regia::ecs::ecs::system::RunSystemOnce;
        // Slot 1 is farther from slot 0 than slot 2, and it is spawned first. The bot measures from
        // the avatar this process owns (`NetOwnedLocally`); without it, it would measure from the
        // origin, where every spawn point ties at the same radius and float rounding picks one.
        assert!(spawn_point(1).distance(spawn_point(0)) > spawn_point(2).distance(spawn_point(0)));
        let mut world = World::new();
        world.insert_resource(NetMode::Client);
        world.insert_resource(PadSource::Bot(Bot::new(0.0, 1, u32::MAX)));
        world.spawn((
            Avatar { slot: 0 },
            Transform::from_translation(spawn_point(0)),
            NetRole::Predicted,
            // `regia_net` marks the client's own copy; `feed_pad` finds "here" by it.
            NetOwnedLocally,
            InputCommand::<Pad> {
                tick: regia_net::NetTick(0),
                value: Pad::default(),
            },
        ));
        for slot in [1, 2] {
            world.spawn((
                Avatar { slot },
                Transform::from_translation(spawn_point(slot)),
                NetRole::Proxy,
            ));
        }
        world.run_system_once(feed_pad).expect("feed_pad runs");
        let mut commands = world.query::<&InputCommand<Pad>>();
        let pad = commands.single(&world).expect("one command").value.clone();
        assert_eq!(pad.aim, spawn_point(2) + Vec3::Y * BODY_CENTER);
    }

    #[test]
    fn spawn_points_are_distinct() {
        let points: Vec<Vec3> = (0..8).map(spawn_point).collect();
        for (i, a) in points.iter().enumerate() {
            for b in &points[i + 1..] {
                assert!(a.distance(*b) > 1.0);
            }
        }
    }
}
