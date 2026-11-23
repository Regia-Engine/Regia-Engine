// SPDX-License-Identifier: MIT-0
//! Match flow (`docs/specs/14` M20 `samples/arena`): warm-up → round → match over, with a score
//! limit and a timer, respawning at [`PlayerStart`]s — implemented as a [`GameRules`] object
//! ([`ArenaRules`]) over the framework's hosted match session (GR-1.3).
//!
//! The match is the core's [`MatchSession`] in the [`MatchRules`] resource, stepped once per fixed
//! tick by [`MatchSessionPlugin`](regia_framework::rules::MatchSessionPlugin) (ADR-8646). The
//! warm-up and the timer are the session's phase settings ([`ArenaConfig::phase_settings`], ADR-8485;
//! a team mode keeps the rule set's phase fields where it sets them,
//! `crate::modes::merged_phase_settings`); kills land on the per-seat ledger (the
//! damage step credits them; [`resolve_deaths`] credits a death raised without one); the arena's
//! own score limit is reported to the core as a game condition. Each player slot is its own team
//! (slot `s` is team `s + 1`, seat slot 0), so the session's ledger is the frag table.
//!
//! [`ArenaState`] stays only as the view the HUD, the bots and `play.expect` read: the phase, the
//! kills per slot and the clocks, copied from the session after each step ([`mirror_match_state`]).
//!
//! The arena drives the [`GameRules`] methods from its systems: [`spawn_world`] (builds the arena
//! and calls `on_level_start`), [`tick_match`] (calls `tick` each fixed step) and
//! [`resolve_deaths`] (`on_character_died` on each [`DiedEvent`]).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::damage::{Died, DiedEvent, Health};
use regia_framework::player::PlayerStart;
use regia_framework::rules::match_flow::{FlowSettings, SeatId};
use regia_framework::rules::objective::{EndReason, ObjectiveSettings};
use regia_framework::rules::phases::{MatchPhase, PhaseSettings};
use regia_framework::rules::session::{MatchSession, SessionSettings};
use regia_framework::rules::teams::{SeatRequest, TeamDef, TeamRules};
use regia_framework::rules::{
    DeathCredited, GameRules, GameRulesResource, LedgerSlot, MatchRules, MatchSessionRules, RulesCtx,
    ScoreSheet, SpawnRequest, TeamId,
};
use regia_scene::{GlobalTransform, Transform};

use crate::config::{ArenaConfig, MatchRng};
use crate::pawn::{Pawn, spawn_bot, spawn_pawn, spawn_player};

/// The arena's half-extent in metres (the playable floor is `2 × ARENA_HALF` square).
pub const ARENA_HALF: f32 = 12.0;

/// A phase of the match.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default)]
pub enum Phase {
    /// Bots idle; no scoring.
    #[default]
    Warmup,
    /// The fight.
    Round,
    /// A score limit or the timer ended the match.
    MatchOver,
}

/// The match's live state as the HUD, the bots and the agent protocol read it (reflected so it
/// survives hot reload and is agent-readable). A view of the hosted session: the session decides,
/// this copies.
///
/// A singleton component on the `"match"` entity rather than a resource: `play.expect` field
/// atoms read reflected *components* (the engine's resource predicate is a closed set of typed
/// handlers), so the arena keeps its match state where the agent protocol can inspect it.
#[derive(Component, Reflect, Debug, Clone)]
#[reflect(Component, Default)]
pub struct ArenaState {
    /// Current phase.
    pub phase: Phase,
    /// Kills by player slot (0 = local player, 1.. = bots), from the session's ledger.
    pub scores: Vec<i32>,
    /// Match time elapsed since level start (seconds).
    pub elapsed: f32,
    /// Time spent in the current phase (seconds).
    pub phase_elapsed: f32,
    /// The round/match duration (seconds), from [`ArenaConfig::match_secs`].
    pub match_secs: f32,
    /// Lockstep steps the session has run.
    pub ticks: u64,
}

impl Phase {
    /// The arena phase showing the core's `phase`.
    pub fn from_core(phase: MatchPhase) -> Self {
        match phase {
            MatchPhase::Warmup | MatchPhase::Countdown => Self::Warmup,
            MatchPhase::Live | MatchPhase::Overtime | MatchPhase::RoundEnd => Self::Round,
            MatchPhase::PostMatch => Self::MatchOver,
        }
    }
}

impl Default for ArenaState {
    fn default() -> Self {
        Self {
            phase: Phase::Warmup,
            scores: Vec::new(),
            elapsed: 0.0,
            phase_elapsed: 0.0,
            match_secs: 180.0,
            ticks: 0,
        }
    }
}

impl ArenaState {
    /// The score of `slot` (0 when the slot is unknown).
    pub fn score(&self, slot: u8) -> i32 {
        self.scores.get(slot as usize).copied().unwrap_or(0)
    }

    /// The highest score currently held.
    pub fn leading_score(&self) -> i32 {
        self.scores.iter().copied().max().unwrap_or(0)
    }

    /// The leader's slot, breaking ties by lowest slot.
    pub fn leader(&self) -> Option<u8> {
        let best = self.leading_score();
        self.scores.iter().position(|&s| s == best).map(|i| i as u8)
    }

    /// Seconds remaining in the round (clamped at zero; full duration outside the round).
    pub fn match_time_left(&self) -> f32 {
        (self.match_secs - self.phase_elapsed).max(0.0)
    }
}

/// The rules team of player slot `slot` (slot `s` is team `s + 1`; team 0 is neutral).
pub fn slot_team(slot: u8) -> u8 {
    slot.saturating_add(1)
}

/// The match session of an arena with `slots` player slots: one team per slot with one seat each,
/// every slot seated, the phases from `config`, started (so the first `phase_changed` is sent).
pub fn arena_session(config: &ArenaConfig, slots: u8, tick_hz: f32) -> MatchSession {
    let teams = slots.max(1);
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            max_teams: teams,
            seats_per_team: 1,
            phases: config.phase_settings(tick_hz),
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            max_teams: teams,
            ..ObjectiveSettings::default()
        },
        teams: TeamRules {
            defs: (1..=teams)
                .map(|id| TeamDef::unnamed(id, [255, 255, 255, 255], 1))
                .collect(),
            ..TeamRules::default()
        },
        ..SessionSettings::default()
    });
    session.start_now();
    for slot in 0..teams {
        let request = SeatRequest {
            team: Some(slot_team(slot)),
            slot: Some(0),
            ..SeatRequest::default()
        };
        if let Err(error) = session.assign_team(&request, &[]) {
            tracing::warn!(target: "arena", "slot {slot} has no seat: {error}");
        }
    }
    session
}

/// The arena's record of the phase settings it applied to the session ([`ArenaRules::tick`]
/// keeps it current): the phase fields the level's rule set set, and the config-derived
/// settings they were merged with ([`crate::modes::merged_phase_settings`]). The tick
/// re-applies phases only when the config-derived settings change; phases a rule set set are
/// never clobbered (the rules-guide review's finding A).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct ArenaPhaseSync {
    /// Phase fields the level's rule set set (all off when it set none, and always so for the
    /// free-for-all, which resolves no rule set).
    pub rule_set: PhaseSettings,
    /// The config-derived phase settings last merged into the session.
    pub config: PhaseSettings,
}

/// The arena's [`GameRules`]: warm-up → round → match-over flow with a score limit and timer.
///
/// Holds no state of its own: the match lives in [`MatchRules`] (stepped by
/// [`MatchSessionPlugin`](regia_framework::rules::MatchSessionPlugin)) and its view in
/// [`ArenaState`]. Joins of extra gamepad players, deaths, the end of the match and the scoreboard go through [`MatchSessionRules`] where the arena has
/// nothing of its own to add.
#[derive(Default)]
pub struct ArenaRules;

impl GameRules for ArenaRules {
    fn on_level_start(&mut self, ctx: &mut RulesCtx) {
        let config = ctx
            .world
            .get_resource::<ArenaConfig>()
            .cloned()
            .unwrap_or_default();
        let slots = u8::try_from(1 + config.bot_count).unwrap_or(u8::MAX);
        let tick_hz = fixed_hz(ctx.world);
        // A team mode (`--mode ctf|koth|tdm`) resolves its rule set through the registry; the
        // free-for-all (and a rule set that does not resolve) keeps the arena's own session.
        let team_mode = ctx
            .world
            .get_resource::<crate::modes::ArenaMatchMode>()
            .filter(|mode| mode.mode.is_team())
            .cloned();
        let mut mode_status = crate::modes::ArenaModeStatus {
            mode: crate::modes::ArenaMode::Ffa.word().to_string(),
            ..Default::default()
        };
        let mut rule_set_phases = PhaseSettings::default();
        let team_session =
            team_mode
                .as_ref()
                .and_then(|mode| match crate::modes::resolve_mode(ctx.world, mode) {
                    Ok(resolved) => {
                        mode_status = crate::modes::ArenaModeStatus {
                            mode: mode.mode.word().to_string(),
                            ruleset: resolved.id.clone(),
                            rules_hash: resolved.content_hash(),
                            error: String::new(),
                        };
                        rule_set_phases = resolved.settings.flow.phases;
                        Some(crate::modes::team_session(
                            &resolved, &config, mode, slots, tick_hz,
                        ))
                    }
                    Err(error) => {
                        tracing::error!(
                            target: "arena",
                            "rule set `{}`: {error}; playing the free-for-all",
                            mode.ruleset_id()
                        );
                        mode_status.error = error;
                        None
                    }
                });
        let teams = team_session.is_some();
        if !teams {
            ctx.world.remove_resource::<crate::modes::ArenaMatchMode>();
        }
        let session = team_session.unwrap_or_else(|| arena_session(&config, slots, tick_hz));
        let phase = Phase::from_core(session.flow.phases.phase);
        ctx.world.insert_resource(MatchRules { session });
        ctx.world.insert_resource(ArenaPhaseSync {
            rule_set: rule_set_phases,
            config: config.phase_settings(tick_hz),
        });
        if teams {
            crate::modes::spawn_objective_markers(ctx.world);
        }
        // The match state is a singleton component so `play.expect` field atoms can read it.
        ctx.world.spawn((
            regia_scene::components::Name::new("match"),
            ArenaState {
                phase,
                scores: vec![0; usize::from(slots)],
                elapsed: 0.0,
                phase_elapsed: 0.0,
                match_secs: config.match_secs,
                ticks: 0,
            },
            mode_status,
        ));

        // Spawn the local player (if any) and the bots at distinct spawn points. In a team mode
        // slot `s` sits on team `1 + s % 2` and starts at its team's `s / 2`-th start.
        let points = spawn_points(ctx.world);
        let start_of = |slot: u8, spawn_index: usize| {
            if teams {
                let seat = crate::modes::seat_of_slot(slot);
                let starts = crate::modes::team_start_positions(seat.team);
                point_at(&starts, usize::from(seat.slot))
            } else {
                point_at(&points, spawn_index)
            }
        };
        let mut spawn_index = 0usize;
        if config.local_player {
            let pawn = spawn_pawn(ctx.world, 0, false, start_of(0, spawn_index));
            spawn_index += 1;
            spawn_player(ctx.world, pawn);
            if teams {
                crate::modes::seat_pawn(ctx.world, pawn, 0);
            }
        }
        for slot in 1..=config.bot_count {
            let slot = u8::try_from(slot).unwrap_or(u8::MAX);
            let pawn = spawn_pawn(ctx.world, slot, true, start_of(slot, spawn_index));
            spawn_index += 1;
            spawn_bot(ctx.world, pawn, slot);
            if teams {
                crate::modes::seat_pawn(ctx.world, pawn, slot);
            }
        }
    }

    fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest> {
        // The arena auto-spawns its own pawns in `on_level_start`; a later gamepad join spawns an
        // extra pawn at the start the session's spawn policy picks (ADR-8647; the first start in
        // spawn order with no policy set). Every arena team is full, so it plays unseated.
        let pos = regia_framework::spawn_select::choose_spawn(ctx.world, player)
            .map(|pick| pick.position)
            .or_else(|| spawn_points(ctx.world).first().copied())
            .unwrap_or(Vec3::ZERO);
        Some(SpawnRequest {
            transform: Transform::from_translation(pos),
            prefab: None,
            team: None,
        })
    }

    fn on_player_left(&mut self, ctx: &mut RulesCtx, player: Entity) {
        MatchSessionRules.on_player_left(ctx, player);
    }

    fn on_character_died(&mut self, ctx: &mut RulesCtx, character: Entity) {
        // The ledger first (a death the damage step did not credit), then the respawn.
        MatchSessionRules.on_character_died(ctx, character);
        // A team mode keeps the pawn down for a moment so the session sees the death (a carried
        // flag drops where its carrier fell); `crate::modes::tick_respawns` brings it back.
        if crate::modes::team_mode(ctx.world).is_some() {
            crate::modes::start_respawn_timer(ctx.world, character);
            return;
        }
        // Respawn the dead pawn at a fresh spawn point with full health (a frag arena loop).
        let point = {
            let points = spawn_points(ctx.world);
            let index = {
                let mut rng = ctx.world.remove_resource::<MatchRng>().unwrap_or_default();
                let index = rng.rng.range(0..points.len().max(1));
                ctx.world.insert_resource(rng);
                index
            };
            points.get(index).copied().unwrap_or(Vec3::ZERO)
        };
        if let Some(mut transform) = ctx.world.get_mut::<Transform>(character) {
            transform.translation = Vec3::new(point.x, crate::pawn::PAWN_EYE_HEIGHT, point.z);
        }
        if let Some(mut health) = ctx.world.get_mut::<Health>(character) {
            health.current = health.max;
        }
        // A respawned pawn can be hurt again (the damage step ignores entities marked `Died`).
        if let Ok(mut entity) = ctx.world.get_entity_mut(character) {
            entity.remove::<Died>();
        }
        // A fresh life starts with a full magazine.
        crate::loadout::refill(ctx.world, character, None);
    }

    fn tick(&mut self, ctx: &mut RulesCtx) {
        // `tick_match` runs in `FixedUpdate` before the session steps: re-apply the phase
        // settings only when the config's own settings changed (a rule set's phases are merged
        // back in and never clobbered), and report the arena's score limit as a game condition.
        let Some(config) = ctx.world.get_resource::<ArenaConfig>().cloned() else {
            return;
        };
        let tick_hz = fixed_hz(ctx.world);
        let settings = config.phase_settings(tick_hz);
        // `None`: nothing to sync (no config change since the last apply, or no arena-built
        // session). Comparing the config's settings — not the session's — is what keeps a rule
        // set's restored timer safe from this tick.
        let sync = ctx
            .world
            .get_resource::<ArenaPhaseSync>()
            .filter(|sync| sync.config != settings)
            .map(|sync| crate::modes::merged_phase_settings(sync.rule_set, &config, tick_hz));
        let mode = crate::modes::team_mode(ctx.world);
        let leader = arena_state(ctx.world)
            .filter(|_| mode.is_none())
            .and_then(|state| {
                (state.leading_score() >= config.score_limit as i32)
                    .then(|| state.leader())
                    .flatten()
            });
        let Some(mut rules) = ctx.world.get_resource_mut::<MatchRules>() else {
            return;
        };
        let session = &mut rules.session;
        let mut applied = false;
        if let Some(settings) = sync {
            match session.set_phases(settings) {
                Ok(()) => applied = true,
                Err(error) => {
                    tracing::warn!(target: "arena", "the arena config's phase settings cannot run: {error}")
                }
            }
        }
        if let Some(slot) = leader
            && session.flow.phases.phase.is_play()
            && !session.board.match_over
        {
            session.report_condition(slot_team(slot), EndReason::Ended);
        }
        // Team deathmatch: the first team to the kill limit. CTF and KOTH end on the board's own
        // capture or hill limit.
        if mode == Some(crate::modes::ArenaMode::Tdm) {
            crate::modes::report_tdm_limit(session, config.score_limit);
        }
        // `rules` is not used after this point, so its world borrow has ended and the sync
        // record can be taken mutably.
        if applied && let Some(mut sync) = ctx.world.get_resource_mut::<ArenaPhaseSync>() {
            sync.config = settings;
        }
    }

    fn is_match_over(&self, world: &World) -> bool {
        MatchSessionRules.is_match_over(world)
    }

    fn scoreboard(&mut self, ctx: &mut RulesCtx) -> Option<ScoreSheet> {
        MatchSessionRules.scoreboard(ctx)
    }
}

/// `Startup`: builds the arena world (floor, walls, spawn points, props, camera, HUD) and starts
/// the match by invoking [`GameRules::on_level_start`].
pub fn spawn_world(world: &mut World) {
    let half = ARENA_HALF;
    // Floor: a static box whose top surface is y = 0, with a visible mesh.
    crate::meshes::spawn_static_visible(
        world,
        "floor",
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(half + 1.0, 0.5, half + 1.0),
        [0.30, 0.34, 0.38, 1.0],
    );
    // Four perimeter walls (collider + mesh). North/south are long in X; east/west in Z.
    let wall = [0.62, 0.58, 0.52, 1.0];
    crate::meshes::spawn_static_visible(
        world,
        "wall_n",
        Vec3::new(0.0, 1.5, -half),
        Vec3::new(half + 1.0, 1.5, 0.5),
        wall,
    );
    crate::meshes::spawn_static_visible(
        world,
        "wall_s",
        Vec3::new(0.0, 1.5, half),
        Vec3::new(half + 1.0, 1.5, 0.5),
        wall,
    );
    crate::meshes::spawn_static_visible(
        world,
        "wall_e",
        Vec3::new(half, 1.5, 0.0),
        Vec3::new(0.5, 1.5, half + 1.0),
        wall,
    );
    crate::meshes::spawn_static_visible(
        world,
        "wall_w",
        Vec3::new(-half, 1.5, 0.0),
        Vec3::new(0.5, 1.5, half + 1.0),
        wall,
    );

    // A sun so the arena renders lit (physically bright, like the other samples).
    let dir = Vec3::new(-0.577, -0.577, -0.577).normalize_or_zero();
    let light_transform = Transform {
        translation: Vec3::ZERO,
        rotation: regia_core::math::Quat::from_rotation_arc(-Vec3::Z, dir),
        scale: Vec3::ONE,
    };
    world.spawn((
        regia_scene::components::Name::new("sun"),
        light_transform,
        GlobalTransform(light_transform.compute_affine()),
        regia_render::DirectionalLight {
            direction: [-0.577, -0.577, -0.577],
            color: [1.0, 1.0, 1.0],
            illuminance: 20_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));

    // Spawn points: spread around the arena, or two team columns in a team mode.
    if crate::modes::team_mode(world).is_some() {
        crate::modes::spawn_team_world(world);
    } else {
        for (i, point) in default_spawn_points().iter().enumerate() {
            world.spawn((
                regia_scene::components::Name::new(format!("spawn_{i}")),
                Transform::from_translation(*point),
                PlayerStart,
            ));
        }
    }

    // Dynamic props (crates) and a jointed prop — see `crate::props`.
    crate::props::spawn_props(world);

    // Start the match through the rules object.
    let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
    {
        let mut ctx = RulesCtx::new(world);
        rules.rules.on_level_start(&mut ctx);
    }
    world.insert_resource(rules);

    // Ammunition, team tags and the weapon abilities on every pawn (ADR-2134).
    crate::loadout::equip_all(world);

    // Navmesh pathing (specs/22 § T2.2): behind the flag, build the mesh from the static
    // colliders, register the bot leaf tasks, and attach nav + AI components to every bot.
    // Otherwise the default brain is the state tree with resupply stations (ADR-2134).
    let config = world.get_resource::<ArenaConfig>().cloned().unwrap_or_default();
    let mut status = crate::config::ArenaBrainStatus {
        errors: world
            .get_resource::<crate::loadout::LoadoutDiagnostics>()
            .map(|d| d.errors.clone())
            .unwrap_or_default(),
        ..Default::default()
    };
    if crate::modes::team_mode(world).is_some() {
        // Team modes: the match core's mode-bot orders drive the bots (`crate::modes`).
        status.brain = "mode".into();
    } else if config.use_navmesh {
        setup_navmesh_bots(world);
        status.brain = "navmesh".into();
    } else if config.brain == crate::config::BotBrainKind::StateTree {
        status.stations = crate::brain::spawn_stations(world).len() as u32;
        if let Err(err) = crate::brain::register_station_query(world) {
            tracing::error!(target: "arena", "station query: {err}");
            status.errors.push(format!("ai/resupply.spatialquery.ron: {err}"));
        }
        match crate::brain::setup_state_tree_bots(world) {
            Ok(trees) => {
                status.brain = "state_tree".into();
                status.trees = trees as u32;
            }
            Err(err) => {
                // The match still plays: the steering loop takes over.
                tracing::error!(target: "arena", "bot state tree: {err}; falling back to steering");
                status.errors.push(format!("{}: {err}", crate::brain::BOT_TREE));
                status.brain = "steering".into();
                if let Some(mut config) = world.get_resource_mut::<ArenaConfig>() {
                    config.brain = crate::config::BotBrainKind::Steering;
                }
            }
        }
    } else {
        status.brain = "steering".into();
    }
    let match_entity = {
        let mut query = world.query_filtered::<Entity, With<ArenaState>>();
        query.iter(world).next()
    };
    if let Some(entity) = match_entity {
        world.entity_mut(entity).insert(status);
    }
}

/// Builds the navmesh, registers the bot behaviour-tree tasks, and attaches the nav/AI components
/// (a [`regia_nav::NavAgent`], its state, a blackboard and a compiled behaviour tree) to each bot.
fn setup_navmesh_bots(world: &mut World) {
    crate::navmesh::build_navmesh(world);
    crate::bot_bt::register_bot_tasks(world);
    let def = crate::bot_bt::build_bot_tree();

    let bots: Vec<Entity> = {
        let mut query = world.query::<(Entity, &Pawn)>();
        query
            .iter(world)
            .filter(|(_, pawn)| pawn.is_bot)
            .map(|(entity, _)| entity)
            .collect()
    };
    for bot in bots {
        let tree = crate::bot_bt::compile_bot_tree(world, &def);
        world.entity_mut(bot).insert((
            regia_nav::NavAgent {
                radius: crate::pawn::PAWN_RADIUS,
                max_speed: 3.5,
                ..Default::default()
            },
            regia_nav::NavAgentState::default(),
            regia_ai::Blackboard::default(),
            tree,
        ));
    }
}

/// The fixed steps per second of this app (60 without a fixed clock).
fn fixed_hz(world: &World) -> f32 {
    world
        .get_resource::<FixedTime>()
        .map(|fixed| fixed.step().as_secs_f32())
        .filter(|step| *step > 0.0)
        .map_or(60.0, |step| 1.0 / step)
}

/// `FixedUpdate`: reports the match's game conditions via [`GameRules::tick`] (the session steps
/// after it, in [`regia_framework::rules::MatchRulesSet`]).
pub fn tick_match(world: &mut World) {
    let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
    {
        let mut ctx = RulesCtx::new(world);
        rules.rules.tick(&mut ctx);
    }
    world.insert_resource(rules);
}

/// `FixedUpdate`: reads [`DiedEvent`]s, credits a kill the damage step did not (a death raised
/// directly, with a seated killer), and respawns the victim via [`GameRules::on_character_died`].
pub fn resolve_deaths(world: &mut World) {
    // Collect deaths first (the message borrow must end before mutating the world). Each death is
    // read once: a fresh cursor would also re-read last frame's deaths (see `weapons::drain`).
    let deaths: Vec<DiedEvent> = crate::weapons::drain::<DiedEvent>(world);

    for death in deaths {
        let victim = seat_of(world, death.entity);
        let killer = death.killer.and_then(|killer| seat_of(world, killer));
        let credited = world.get::<DeathCredited>(death.entity).is_some();
        if !credited
            && let (Some(victim), Some(killer)) = (victim, killer)
            && victim != killer
            && let Some(mut rules) = world.get_resource_mut::<MatchRules>()
        {
            rules.session.credit_kill(killer, victim);
            world.entity_mut(death.entity).insert(DeathCredited);
        }

        // Ledger and respawn.
        let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
        {
            let mut ctx = RulesCtx::new(world);
            rules.rules.on_character_died(&mut ctx, death.entity);
        }
        world.insert_resource(rules);
    }
}

/// The ledger seat of a pawn: its [`TeamId`] and [`LedgerSlot`], on a fighting team.
fn seat_of(world: &World, entity: Entity) -> Option<SeatId> {
    let team = world.get::<TeamId>(entity)?;
    let slot = world.get::<LedgerSlot>(entity)?;
    (team.0 != 0).then(|| SeatId::new(team.0, slot.0))
}

/// `FixedUpdate`, after the session's step: copies the phase, the kills per slot and the clocks
/// from the session into [`ArenaState`].
pub fn mirror_match_state(world: &mut World) {
    let delta = world
        .get_resource::<FixedTime>()
        .map_or(0.0, |fixed| fixed.step().as_secs_f32());
    let match_secs = world
        .get_resource::<ArenaConfig>()
        .map(|config| config.match_secs);
    let Some((phase, step, kills)) = world.get_resource::<MatchRules>().map(|rules| {
        let session = &rules.session;
        let kills: Vec<(SeatId, u32)> = session
            .ledgers
            .rows
            .iter()
            .map(|row| (SeatId::new(row.team, row.slot), row.kills))
            .collect();
        (
            Phase::from_core(session.flow.phases.phase),
            session.flow.step,
            kills,
        )
    }) else {
        return;
    };
    let teams = crate::modes::team_mode(world).is_some();
    let Some(mut state) = arena_state_mut(world) else {
        return;
    };
    state.elapsed += delta;
    state.phase_elapsed += delta;
    if let Some(secs) = match_secs {
        state.match_secs = secs;
    }
    state.ticks = step;
    for (seat, kills) in kills {
        // Free-for-all: slot `s` is team `s + 1`; team modes: `crate::modes::seat_of_slot`.
        let slot = if teams {
            crate::modes::slot_of_seat(seat).map(usize::from)
        } else {
            usize::from(seat.team).checked_sub(1)
        };
        let Some(slot) = slot else {
            continue;
        };
        if let Some(score) = state.scores.get_mut(slot) {
            *score = i32::try_from(kills).unwrap_or(i32::MAX);
        }
    }
    if phase != state.phase {
        state.phase = phase;
        state.phase_elapsed = 0.0;
    }
}

/// Reads the singleton [`ArenaState`] component (the `"match"` entity).
///
/// Bot tasks call this every fixed step: it matches the archetypes that hold the component instead of
/// visiting every entity (that scan was 3.5 % of the main thread on the TEST Pixel 9 Pro, ADR-4933).
pub fn arena_state(world: &World) -> Option<&ArenaState> {
    let mut query = world.try_query::<&ArenaState>()?;
    query.iter(world).next()
}

/// Mutably reads the singleton [`ArenaState`] component.
pub fn arena_state_mut(world: &mut World) -> Option<Mut<'_, ArenaState>> {
    let mut query = world.query::<&mut ArenaState>();
    query.iter_mut(world).next()
}

/// All [`PlayerStart`] positions, in entity order.
pub fn spawn_points(world: &mut World) -> Vec<Vec3> {
    let mut query = world.query_filtered::<&Transform, With<PlayerStart>>();
    query.iter(world).map(|t| t.translation).collect()
}

/// The `i`-th spawn point (wrapping), or the origin when none exist.
fn point_at(points: &[Vec3], i: usize) -> Vec3 {
    points.get(i % points.len().max(1)).copied().unwrap_or(Vec3::ZERO)
}

/// The default spawn points (used when no level-authored [`PlayerStart`]s exist).
pub fn default_spawn_points() -> Vec<Vec3> {
    let half = ARENA_HALF - 2.0;
    vec![
        Vec3::new(-half, 0.0, -half),
        Vec3::new(half, 0.0, -half),
        Vec3::new(-half, 0.0, half),
        Vec3::new(half, 0.0, half),
        Vec3::new(0.0, 0.0, -half),
        Vec3::new(0.0, 0.0, half),
        Vec3::new(-half, 0.0, 0.0),
        Vec3::new(half, 0.0, 0.0),
        Vec3::new(0.0, 0.0, 0.0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::damage::{DamageEvent, DamagePlugin};
    use regia_framework::rules::objective::{RulesEvent, ScoreReason};
    use regia_framework::rules::{MatchEvent, MatchRulesSet, MatchSessionPlugin, RulesPlugin};

    /// A headless arena app with physics, rules, damage, one bot, no local player.
    fn arena_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((
            regia_physics::PhysicsPlugin,
            RulesPlugin,
            DamagePlugin,
            crate::bots::BotsPlugin,
            crate::weapons::WeaponsPlugin,
            crate::props::PropsPlugin,
        ));
        app.insert_resource(ArenaConfig {
            bot_count: 1,
            local_player: false,
            score_limit: 2,
            warmup_secs: 0.0,
            match_secs: 60.0,
            use_navmesh: false,
            brain: crate::config::BotBrainKind::Steering,
        });
        app.insert_resource(MatchRng::new(seed));
        app.insert_resource(GameRulesResource {
            rules: Box::new(ArenaRules),
        });
        app.add_plugins(MatchSessionPlugin);
        app.init_resource::<MatchEventLog>();
        app.add_systems(Startup, spawn_world);
        app.add_systems(
            FixedUpdate,
            (tick_match, resolve_deaths)
                .chain()
                .after(regia_framework::damage::DamageSet)
                .before(MatchRulesSet),
        );
        app.add_systems(
            FixedUpdate,
            (mirror_match_state, log_match_events).after(MatchRulesSet),
        );
        enable_fixed_step(&mut app, 60.0);
        app
    }

    /// Every [`MatchEvent`] the session sent, in order.
    #[derive(Resource, Default)]
    struct MatchEventLog(Vec<RulesEvent>);

    fn log_match_events(mut reader: MessageReader<MatchEvent>, mut log: ResMut<MatchEventLog>) {
        log.0.extend(reader.read().map(|event| event.0.clone()));
    }

    fn run_seconds(app: &mut App, seconds: f32) {
        for _ in 0..(seconds * 60.0) as usize {
            app.update();
        }
    }

    #[test]
    fn warmup_transitions_to_round_and_scores_start_at_zero() {
        let mut app = arena_app(1);
        app.update();
        // Warm-up is 0 s: the first tick transitions straight into the round.
        let state = arena_state(app.world()).expect("match state").clone();
        assert_eq!(state.phase, Phase::Round);
        assert_eq!(state.scores.len(), 2, "one bot + empty local slot");
        assert!(state.scores.iter().all(|&s| s == 0));
    }

    #[test]
    fn score_limit_ends_the_match() {
        let mut app = arena_app(2);
        app.update();
        // Force a score of 2 (the limit) on the bot slot, in the round phase.
        {
            let mut state = arena_state_mut(app.world_mut()).expect("match state");
            state.phase = Phase::Round;
            state.scores[1] = 2;
        }
        run_seconds(&mut app, 0.2);
        let state = arena_state(app.world()).expect("match state").clone();
        assert_eq!(state.phase, Phase::MatchOver, "score limit ends the round");
    }

    #[test]
    fn the_match_timer_reaches_match_over() {
        let mut app = arena_app(5);
        app.update();
        // Shorten the round so the timer, not the score limit, ends the match.
        app.world_mut().resource_mut::<ArenaConfig>().match_secs = 0.1;
        run_seconds(&mut app, 0.2);
        let state = arena_state(app.world()).expect("match state").clone();
        assert_eq!(state.phase, Phase::MatchOver, "the timer ends the round");
        assert!(state.leading_score() < app.world().resource::<ArenaConfig>().score_limit as i32);
    }

    #[test]
    fn a_died_event_scores_the_killer() {
        let mut app = arena_app(3);
        app.update();
        // Spawn a fresh victim (slot 0) and kill it by the existing bot (slot 1).
        let victim = spawn_pawn(app.world_mut(), 0, false, Vec3::new(2.0, 0.0, 2.0));
        let killer = bot_entity(&mut app);
        app.update();
        app.world_mut().write_message(DiedEvent {
            entity: victim,
            killer: Some(killer),
        });
        app.update();
        let state = arena_state(app.world()).expect("match state").clone();
        assert_eq!(state.score(1), 1, "the killer's slot is credited");
        assert_eq!(state.score(0), 0);
    }

    /// The first bot pawn entity (spawned by `on_level_start`).
    fn bot_entity(app: &mut App) -> Entity {
        let mut pawns = app.world_mut().query_filtered::<Entity, With<Pawn>>();
        pawns
            .iter(app.world())
            .find(|e| app.world().get::<Pawn>(*e).is_some_and(|p| p.is_bot))
            .expect("a bot exists")
    }

    #[test]
    fn resolve_deaths_reads_messages_once() {
        let mut app = arena_app(4);
        app.update();
        // Spawn the victim off every `PlayerStart` so a no-op respawn (which would leave it at the
        // death position) is detectable.
        let death_pos = Vec3::new(5.0, 0.0, 5.0);
        let victim = spawn_pawn(app.world_mut(), 0, false, death_pos);
        // `spawn_pawn` starts the pawn at full health; drop it to zero so the test proves
        // `on_character_died` actually restores health (not merely that spawn filled it).
        app.world_mut().get_mut::<Health>(victim).expect("health").current = 0.0;
        app.update();
        app.world_mut().write_message(DiedEvent {
            entity: victim,
            killer: None,
        });
        app.update();
        // The victim respawned: health restored from 0, moved off the death position, and placed on
        // a `PlayerStart` at eye height.
        let health = app.world().get::<Health>(victim).expect("health");
        assert!(
            health.current > 0.0 && !health.is_dead(),
            "health restored from 0, got {health:?}"
        );
        assert_eq!(health.current, health.max, "health restored to full");
        let translation = app.world().get::<Transform>(victim).expect("victim").translation;
        assert_eq!(translation.y, crate::pawn::PAWN_EYE_HEIGHT);
        let spawns = spawn_points(app.world_mut());
        let on_spawn = spawns
            .iter()
            .any(|p| (p.x - translation.x).abs() < 1.0e-3 && (p.z - translation.z).abs() < 1.0e-3);
        assert!(
            on_spawn,
            "respawned at a PlayerStart, got {translation:?} (spawns {spawns:?})"
        );
        assert!(
            (translation.x - death_pos.x).abs() > 1.0e-3 || (translation.z - death_pos.z).abs() > 1.0e-3,
            "respawned off the death position {death_pos:?}, got {translation:?}"
        );
    }

    /// GR-1.3: the arena's match is the framework's hosted session. Bot 1 frags the local slot and
    /// bot 2 through the damage step; the second kill reaches the score limit (2), the arena
    /// reports it to the session, and the session ends the match: `MatchSessionRules` says so,
    /// the ledger and the scoreboard have the kills, and `ArenaState` shows the session's result.
    #[test]
    fn arena_sample_match_ends_through_match_session_rules() {
        let mut app = arena_app(6);
        app.world_mut().resource_mut::<ArenaConfig>().bot_count = 2;
        app.update();
        let victim = spawn_pawn(app.world_mut(), 0, false, Vec3::new(2.0, 0.0, 2.0));
        let (killer, other) = {
            let bots = app.world_mut().query::<(Entity, &Pawn)>();
            let world = app.world();
            let find = |slot: u8| {
                bots.iter_manual(world)
                    .find(|(_, pawn)| pawn.is_bot && pawn.slot == slot)
                    .map(|(entity, _)| entity)
                    .expect("bot")
            };
            (find(1), find(2))
        };
        let seats = app.world().resource::<MatchRules>().session.flow.seated.clone();
        assert_eq!(
            seats,
            vec![SeatId::new(1, 0), SeatId::new(2, 0), SeatId::new(3, 0)],
            "one seat per slot"
        );
        assert!(!MatchSessionRules.is_match_over(app.world()));
        for target in [victim, other] {
            app.world_mut().write_message(DamageEvent {
                target,
                amount: 1_000.0,
                source: Some(killer),
                position: None,
            });
            app.update();
        }
        run_seconds(&mut app, 0.1);

        assert!(
            app.world()
                .resource::<GameRulesResource>()
                .rules
                .is_match_over(app.world()),
            "the arena's rules read the end from the session"
        );
        assert!(MatchSessionRules.is_match_over(app.world()));
        let session = &app.world().resource::<MatchRules>().session;
        assert_eq!(session.flow.phases.phase, MatchPhase::PostMatch);
        let killer_row = session.ledgers.row(SeatId::new(2, 0)).expect("bot 1 row");
        assert_eq!(killer_row.kills, 2);
        assert_eq!(
            session.ledgers.row(SeatId::new(1, 0)).map(|row| row.deaths),
            Some(1)
        );
        assert_eq!(
            session.ledgers.row(SeatId::new(3, 0)).map(|row| row.deaths),
            Some(1)
        );
        let log = &app.world().resource::<MatchEventLog>().0;
        let kills = log
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RulesEvent::PlayerScored {
                        team: 2,
                        reason: ScoreReason::Kill,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(kills, 2, "both kills reached the session's message stream");
        assert!(log.iter().any(|event| matches!(
            event,
            RulesEvent::PhaseChanged {
                phase: MatchPhase::PostMatch,
                ..
            }
        )));
        let state = arena_state(app.world()).expect("match state").clone();
        assert_eq!(state.phase, Phase::MatchOver);
        assert_eq!(state.score(1), 2);
        let sheet = {
            let mut rules = app
                .world_mut()
                .remove_resource::<GameRulesResource>()
                .expect("rules");
            let sheet = rules.rules.scoreboard(&mut RulesCtx::new(app.world_mut()));
            app.world_mut().insert_resource(rules);
            sheet.expect("the arena has a scoreboard")
        };
        assert_eq!(sheet.rows.len(), 3);
        // Score-sheet row ids are the ledger rows' ids (ADR-8657): the fragger is team 2 slot 0.
        let fragger_id = app
            .world()
            .resource::<MatchRules>()
            .session
            .ledgers
            .row(SeatId::new(2, 0))
            .expect("the fragger's row")
            .id;
        assert_eq!(sheet.rows[0].team, Some(2), "the fragger leads the sheet");
        assert_eq!(sheet.rows[0].id, fragger_id, "the fragger leads the sheet");
        // The victims respawned and can be hurt again.
        assert!(app.world().get::<Died>(victim).is_none());
    }

    #[test]
    fn spawn_points_are_read_from_player_starts() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.world_mut()
            .spawn((Transform::from_translation(Vec3::new(1.0, 0.0, 2.0)), PlayerStart));
        app.world_mut()
            .spawn((Transform::from_translation(Vec3::new(3.0, 0.0, 4.0)), PlayerStart));
        let points = spawn_points(app.world_mut());
        assert_eq!(points, vec![Vec3::new(1.0, 0.0, 2.0), Vec3::new(3.0, 0.0, 4.0)]);
    }
}
