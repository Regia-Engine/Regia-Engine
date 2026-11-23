// SPDX-License-Identifier: MIT-0
//! Team modes through the rule-set registry: `arena --mode ctf|koth|tdm` (GR-8.3,
//! `book/tutorials/capture-the-flag.md`).
//!
//! The default arena is a free-for-all frag match (`crate::match_flow`). With an
//! [`ArenaMatchMode`] resource naming a team mode, the arena instead:
//!
//! 1. resolves the shipped rule set of that id (`tdm`, `ctf` or `koth`) through the app's
//!    [`RuleSetRegistry`] and builds a two-team [`MatchSession`] from it ([`team_session`]);
//! 2. places two teams of [`PlayerStart`]s, each with a [`SpawnPoint`] for its side
//!    ([`team_start_positions`]), and seats slot `s` on team `1 + s % 2`, ledger slot `s / 2`
//!    ([`seat_of_slot`]);
//! 3. places the flags (CTF) or the hill (KOTH) on the session's board and shows each with an
//!    [`ObjectiveEntity`] the hosted session moves;
//! 4. drives the bots with the match core's mode-bot orders ([`drive_mode_bots`]: a goal point and
//!    a focus enemy per bot, from `regia_framework::rules::bots`);
//! 5. keeps a dead pawn down for [`RESPAWN_DELAY_TICKS`] before it respawns at its team's start
//!    ([`tick_respawns`]), so the session sees the death and a carried flag drops where its
//!    carrier fell.
//!
//! Everything the match decides (pickups, returns, captures, hill points, the end) is the core's
//! arithmetic in the hosted session; this module only feeds it entities and moves the bots.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::damage::{Died, Health};
use regia_framework::player::{PlayerStart, SpawnPoint};
use regia_framework::rules::bots::{AiLevel, BotHooks, CtfRole, contacts_of};
use regia_framework::rules::match_flow::SeatId;
use regia_framework::rules::mode::MatchMode;
use regia_framework::rules::objective::{EndReason, HillVolume};
use regia_framework::rules::phases::PhaseSettings;
use regia_framework::rules::ruleset::ResolvedRules;
use regia_framework::rules::session::MatchSession;
use regia_framework::rules::teams::{SeatRequest, TeamDef};
use regia_framework::rules::{LedgerSlot, MatchRules, ObjectiveEntity, TeamId, match_contestants};
use regia_framework::ruleset_file::RuleSetRegistry;
use regia_scene::Transform;

use crate::config::{ArenaConfig, MatchRng};
use crate::match_flow::{ARENA_HALF, Phase, arena_state};
use crate::pawn::{BotBrain, Pawn};
use crate::weapons::{FireRequest, Weapon};

/// The arena's match mode (`--mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArenaMode {
    /// Every pawn for itself, first to the kill limit (the arena's own rules, no rule set).
    #[default]
    Ffa,
    /// Team deathmatch: two teams, first team to the kill limit (rule set `tdm`).
    Tdm,
    /// Capture the flag: two teams, two flags (rule set `ctf`).
    Ctf,
    /// King of the hill: two teams, one hill in the middle (rule set `koth`).
    Koth,
}

impl ArenaMode {
    /// Every mode, in `--mode` order.
    pub const ALL: [Self; 4] = [Self::Ffa, Self::Tdm, Self::Ctf, Self::Koth];

    /// The `--mode` word and, for a team mode, the shipped rule-set id.
    pub fn word(self) -> &'static str {
        match self {
            Self::Ffa => "ffa",
            Self::Tdm => "tdm",
            Self::Ctf => "ctf",
            Self::Koth => "koth",
        }
    }

    /// The mode a `--mode` word names.
    ///
    /// # Errors
    ///
    /// A message naming the accepted words.
    pub fn parse(word: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.word() == word)
            .ok_or_else(|| format!("--mode takes `ffa`, `tdm`, `ctf` or `koth`, got `{word}`"))
    }

    /// Two teams, seats, team starts and mode bots (everything but the free-for-all).
    pub fn is_team(self) -> bool {
        self != Self::Ffa
    }
}

/// The arena's mode as a resource. Absent, or [`ArenaMode::Ffa`], the arena plays its own
/// free-for-all exactly as before.
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct ArenaMatchMode {
    /// The mode.
    pub mode: ArenaMode,
    /// The rule-set id to resolve; empty is the mode's own word (`ctf` for [`ArenaMode::Ctf`]).
    /// A project rule set that extends a shipped one (`ctf_long`) goes here.
    pub ruleset: String,
    /// Playlist-row options for the rule set (checked against its option schema).
    pub options: BTreeMap<String, String>,
    /// Overrides the rule set's capture limit (CTF) or hill score limit (KOTH); `None` keeps it.
    /// TDM always uses `ArenaConfig::score_limit` as the team kill limit.
    pub limit: Option<u32>,
}

impl ArenaMatchMode {
    /// A team mode with its shipped rule set and no overrides.
    pub fn new(mode: ArenaMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// The rule-set id this mode resolves.
    pub fn ruleset_id(&self) -> &str {
        if self.ruleset.is_empty() {
            self.mode.word()
        } else {
            &self.ruleset
        }
    }
}

/// Which mode and rule set the match runs: a component on the `"match"` entity (next to
/// `ArenaState`), so `play.expect` and `scene.get_entity` can read it.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct ArenaModeStatus {
    /// `ffa`, `tdm`, `ctf` or `koth`.
    pub mode: String,
    /// The resolved rule-set id (empty in the free-for-all).
    pub ruleset: String,
    /// The resolved rules' content hash (`ResolvedRules::content_hash`; 0 in the free-for-all).
    pub rules_hash: u64,
    /// Why the asked rule set did not resolve (`<code>: <message>`); the arena then plays the
    /// free-for-all.
    pub error: String,
}

/// The team mode in force, if any (`None` in the free-for-all).
pub fn team_mode(world: &World) -> Option<ArenaMode> {
    world
        .get_resource::<ArenaMatchMode>()
        .map(|m| m.mode)
        .filter(|m| m.is_team())
}

/// The two teams' colors (red, blue), RGBA.
pub const TEAM_COLORS: [[u8; 4]; 2] = [[220, 60, 50, 255], [60, 110, 230, 255]];

/// The board id of team `t`'s flag is `FLAG_ID_BASE + t`.
pub const FLAG_ID_BASE: u32 = 900;

/// The board id of the hill.
pub const HILL_ID: u32 = 950;

/// How far from the center line (x = 0) each team's flag stands, meters.
pub const FLAG_OFFSET_X: f32 = 9.0;

/// Half the side of the hill box, meters.
pub const HILL_HALF: f32 = 3.0;

/// Fixed steps a dead pawn stays down before it respawns (1.5 s at 60 Hz).
pub const RESPAWN_DELAY_TICKS: u32 = 90;

/// Mode bots run at this speed, meters per second.
pub const MODE_BOT_SPEED: f32 = 4.0;

/// Mode bots fire at their focus enemy only inside this range, meters (short, so an attacker can
/// reach a guarded flag).
pub const MODE_FIRE_RANGE: f32 = 6.0;

/// Seconds between a mode bot's rifle shots.
pub const MODE_FIRE_COOLDOWN: f32 = 0.6;

/// The chance that a mode bot's shot is aimed true (the rest pass a meter wide), drawn from
/// `MatchRng`. Without misses the two teams of a mirrored arena trade every fight evenly and a
/// capture-the-flag match never resolves.
pub const MODE_HIT_CHANCE: f32 = 0.5;

/// The seat of player slot `slot` in a team mode: team `1 + slot % 2`, ledger slot `slot / 2`.
pub fn seat_of_slot(slot: u8) -> SeatId {
    SeatId::new(1 + slot % 2, slot / 2)
}

/// The player slot of `seat` in a team mode (the inverse of [`seat_of_slot`]), or `None` for a
/// team other than 1 and 2.
pub fn slot_of_seat(seat: SeatId) -> Option<u8> {
    match seat.team {
        1 | 2 => seat
            .slot
            .checked_mul(2)
            .and_then(|s| s.checked_add(seat.team - 1)),
        _ => None,
    }
}

/// The [`PlayerStart`] positions of team `team` (1 west, 2 east): four starts in a column behind
/// the team's flag.
pub fn team_start_positions(team: u8) -> [Vec3; 4] {
    let x = if team == 1 { -1.0 } else { 1.0 } * (ARENA_HALF - 1.5);
    [-6.0, -2.0, 2.0, 6.0].map(|z| Vec3::new(x, 0.0, z))
}

/// Where team `team`'s flag stands.
pub fn flag_base(team: u8) -> [f32; 3] {
    let x = if team == 1 { -FLAG_OFFSET_X } else { FLAG_OFFSET_X };
    [x, 0.0, 0.0]
}

/// The arena's own rule sets, embedded: `(path, text)`. [`load_arena_rule_sets`] adds them to the
/// app's registry next to the shipped defaults, so `--ruleset ctf_quick` resolves.
pub const ARENA_RULE_SETS: [(&str, &str); 1] = [(
    "assets/rules/ctf_quick.rules.ron",
    include_str!("../assets/rules/ctf_quick.rules.ron"),
)];

/// Load [`ARENA_RULE_SETS`] into `registry`. A file that does not load is logged and kept in the
/// registry's error list (the agent's `rules.ruleset_list` shows it); the others still load.
/// Returns how many loaded.
pub fn load_arena_rule_sets(registry: &mut RuleSetRegistry) -> usize {
    let mut loaded = 0;
    for (path, text) in ARENA_RULE_SETS {
        match registry.load_text(path, text) {
            Ok(_) => loaded += 1,
            Err(error) => tracing::error!(target: "arena", "{path}: {}: {}", error.code, error.message),
        }
    }
    loaded
}

/// Resolve the mode's rule set through the app's [`RuleSetRegistry`] (the shipped defaults when
/// the app has none).
///
/// # Errors
///
/// The registry's error code and message (`ruleset_unknown`, an option out of range, ...).
pub fn resolve_mode(world: &World, mode: &ArenaMatchMode) -> Result<ResolvedRules, String> {
    let resolved = match world.get_resource::<RuleSetRegistry>() {
        Some(registry) => registry.resolve(mode.ruleset_id(), &mode.options),
        None => RuleSetRegistry::with_defaults().resolve(mode.ruleset_id(), &mode.options),
    };
    resolved.map_err(|error| format!("{}: {}", error.code, error.message))
}

/// The phase settings a session of this arena runs: `rule_set`'s fields where the rule set sets
/// them, and the arena config's warm-up and timer ([`ArenaConfig::phase_settings`]) where it
/// does not. The rule set's phases win (the rules-guide review's finding A); the arena defaults
/// fill only the fields the rule set leaves unset.
///
/// "Set" follows the rule-set patch semantics (`ruleset_file::PhasePatch`): an `Option` field
/// holds `Some` when a rule set in the `extends` chain set it; `None` falls back to the arena
/// default. `countdown_ticks` and `on_time_up` are not `Option`, so the rule set's value is
/// kept as is — the arena config sets neither, so its defaults there (no countdown, end on
/// time up) are exactly what an unset rule set leaves.
pub fn merged_phase_settings(rule_set: PhaseSettings, config: &ArenaConfig, tick_hz: f32) -> PhaseSettings {
    let arena = config.phase_settings(tick_hz);
    PhaseSettings {
        warmup_ticks: rule_set.warmup_ticks.or(arena.warmup_ticks),
        warmup_min_seats: rule_set.warmup_min_seats.or(arena.warmup_min_seats),
        countdown_ticks: rule_set.countdown_ticks,
        time_limit_ticks: rule_set.time_limit_ticks.or(arena.time_limit_ticks),
        on_time_up: rule_set.on_time_up,
        rounds: rule_set.rounds.or(arena.rounds),
        post_match_ticks: rule_set.post_match_ticks.or(arena.post_match_ticks),
        forfeit_after_ticks: rule_set.forfeit_after_ticks.or(arena.forfeit_after_ticks),
    }
}

/// A two-team session of `slots` players from `resolved`: the rule set's settings with its
/// phase fields kept where it sets them and the arena's warm-up and timer filling the rest
/// ([`merged_phase_settings`]), two teams (red and blue) with room for every slot, the mode's
/// objectives on the board, started, every slot seated by [`seat_of_slot`].
pub fn team_session(
    resolved: &ResolvedRules,
    config: &ArenaConfig,
    mode: &ArenaMatchMode,
    slots: u8,
    tick_hz: f32,
) -> MatchSession {
    let per_team = slots.div_ceil(2).max(1);
    let mut settings = resolved.settings.clone();
    settings.flow.max_teams = 2;
    settings.flow.seats_per_team = per_team;
    let rule_set_phases = settings.flow.phases;
    settings.flow.phases = merged_phase_settings(rule_set_phases, config, tick_hz);
    settings.objectives.max_teams = 2;
    if let Some(limit) = mode.limit {
        settings.objectives.capture_limit = limit;
        settings.objectives.hill_score_limit = limit;
    }
    settings.teams.defs = vec![
        TeamDef::unnamed(1, TEAM_COLORS[0], per_team),
        TeamDef::unnamed(2, TEAM_COLORS[1], per_team),
    ];
    let mut session = MatchSession::new(settings);
    resolved.apply_to(&mut session);
    // Launching resets the board, so the objectives go on after it.
    session.start_now();
    match session.board.mode {
        MatchMode::CaptureTheFlag => {
            for team in [1u8, 2] {
                if let Err(error) =
                    session
                        .board
                        .spawn_flag(FLAG_ID_BASE + u32::from(team), team, flag_base(team))
                {
                    tracing::warn!(target: "arena", "team {team} has no flag: {error}");
                }
            }
        }
        MatchMode::KingOfTheHill => {
            let volume = HillVolume {
                min: [-HILL_HALF, -1.0, -HILL_HALF],
                max: [HILL_HALF, 4.0, HILL_HALF],
            };
            if let Err(error) = session.board.install_hill_volume(HILL_ID, volume) {
                tracing::warn!(target: "arena", "the hill is missing: {error}");
            }
        }
        _ => {}
    }
    for slot in 0..slots {
        let seat = seat_of_slot(slot);
        let request = SeatRequest {
            team: Some(seat.team),
            slot: Some(seat.slot),
            ..SeatRequest::default()
        };
        if let Err(error) = session.assign_team(&request, &[]) {
            tracing::warn!(target: "arena", "slot {slot} has no seat: {error}");
        }
    }
    session
}

/// `Startup` (team modes): the team starts and the objective markers. Called by
/// `crate::match_flow::spawn_world` instead of the free-for-all spawn points.
pub fn spawn_team_world(world: &mut World) {
    for team in [1u8, 2] {
        for (i, point) in team_start_positions(team).iter().enumerate() {
            world.spawn((
                regia_scene::components::Name::new(format!("start_{team}_{i}")),
                Transform::from_translation(*point),
                PlayerStart,
                SpawnPoint::for_team(team),
            ));
        }
    }
}

/// Spawns the marker entity of every objective on the session's board (a flag pole per team, a
/// pad for the hill), each tagged [`ObjectiveEntity`] so the hosted session moves it.
pub fn spawn_objective_markers(world: &mut World) {
    let objectives: Vec<(u32, u8, bool, [f32; 3])> = world
        .get_resource::<MatchRules>()
        .map(|rules| {
            rules
                .session
                .board
                .objectives
                .iter()
                .map(|o| (o.entity_id, o.team_id, o.is_flag(), o.position))
                .collect()
        })
        .unwrap_or_default();
    for (id, team, is_flag, position) in objectives {
        let (name, half, color) = if is_flag {
            let c = TEAM_COLORS[usize::from(team.saturating_sub(1)).min(1)];
            let color = [c[0], c[1], c[2]].map(|v| f32::from(v) / 255.0);
            (
                format!("flag_{team}"),
                Vec3::new(0.15, 1.2, 0.15),
                [color[0], color[1], color[2], 1.0],
            )
        } else {
            (
                "hill".to_string(),
                Vec3::new(HILL_HALF, 0.02, HILL_HALF),
                [0.9, 0.8, 0.2, 1.0],
            )
        };
        let marker = crate::meshes::spawn_marker(
            world,
            &name,
            Vec3::new(position[0], position[1], position[2]),
            half,
            color,
        );
        world.entity_mut(marker).insert(ObjectiveEntity(id));
    }
}

/// Moves a freshly spawned pawn of `slot` onto its team-mode seat.
pub fn seat_pawn(world: &mut World, pawn: Entity, slot: u8) {
    let seat = seat_of_slot(slot);
    world
        .entity_mut(pawn)
        .insert((TeamId(seat.team), LedgerSlot(seat.slot)));
}

/// A dead pawn waiting to respawn (team modes).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RespawnTimer {
    /// Fixed steps left.
    pub ticks_left: u32,
}

/// Team modes: keep `pawn` down for [`RESPAWN_DELAY_TICKS`] (health stays at zero, so the next
/// session step drops a carried flag where it fell).
pub fn start_respawn_timer(world: &mut World, pawn: Entity) {
    if let Some(mut health) = world.get_mut::<Health>(pawn) {
        health.current = 0.0;
    }
    if let Ok(mut entity) = world.get_entity_mut(pawn) {
        entity.insert(RespawnTimer {
            ticks_left: RESPAWN_DELAY_TICKS,
        });
    }
}

/// `FixedUpdate` (team modes): counts the respawn timers down and respawns each pawn whose timer
/// ran out at the start the session's spawn policy picks for its team (`choose_spawn`), with full
/// health and a full magazine.
pub fn tick_respawns(world: &mut World) {
    if team_mode(world).is_none() {
        return;
    }
    let mut ready = Vec::new();
    {
        let mut query = world.query::<(Entity, &mut RespawnTimer)>();
        for (entity, mut timer) in query.iter_mut(world) {
            timer.ticks_left = timer.ticks_left.saturating_sub(1);
            if timer.ticks_left == 0 {
                ready.push(entity);
            }
        }
    }
    ready.sort_by_key(|e| e.index().index());
    for pawn in ready {
        let point = regia_framework::spawn_select::choose_spawn(world, pawn).map(|pick| pick.position);
        if let Some(point) = point
            && let Some(mut transform) = world.get_mut::<Transform>(pawn)
        {
            transform.translation = Vec3::new(point.x, crate::pawn::PAWN_EYE_HEIGHT, point.z);
        }
        if let Some(mut health) = world.get_mut::<Health>(pawn) {
            health.current = health.max;
        }
        if let Ok(mut entity) = world.get_entity_mut(pawn) {
            entity.remove::<(Died, RespawnTimer)>();
        }
        crate::loadout::refill(world, pawn, None);
    }
}

/// Team deathmatch: the kills of each team (index 0 is team 1), from the session's ledger.
pub fn team_kills(session: &MatchSession) -> [u32; 2] {
    let mut kills = [0u32; 2];
    for row in &session.ledgers.rows {
        if let Some(total) = usize::from(row.team)
            .checked_sub(1)
            .and_then(|i| kills.get_mut(i))
        {
            *total = total.saturating_add(row.kills);
        }
    }
    kills
}

/// Team deathmatch: report the first team to `limit` kills to the session (a game condition, like
/// the free-for-all's kill limit). Ties go to team 1, the earlier seat.
pub fn report_tdm_limit(session: &mut MatchSession, limit: u32) {
    if !session.flow.phases.phase.is_play() || session.board.match_over {
        return;
    }
    let kills = team_kills(session);
    if let Some(index) = kills.iter().position(|&k| k >= limit.max(1)) {
        session.report_condition(index as u8 + 1, EndReason::Ended);
    }
}

/// `FixedUpdate` ([`crate::ArenaSimSet::Bots`], team modes): every living bot gets the match core's
/// mode-bot order (the attacker runs for the enemy flag and home with it, the defender holds or
/// returns its own; KOTH bots contest the hill; TDM bots hunt), walks toward the order's goal and
/// fires a rifle at the order's focus enemy inside [`MODE_FIRE_RANGE`]. A CTF attacker that does
/// not carry a flag runs for it without stopping to shoot. Each shot draws from `MatchRng` and is
/// aimed true with [`MODE_HIT_CHANCE`].
///
/// Orders come from `BotHooks::orders` over the hosted session's board and the adapter's
/// contestant list, so they are the same on every run with the same inputs.
#[tracing::instrument(skip_all, target = "arena", name = "mode_bots_drive")]
pub fn drive_mode_bots(world: &mut World) {
    if team_mode(world).is_none() {
        return;
    }
    if arena_state(world).is_some_and(|s| s.phase != Phase::Round) {
        return;
    }
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32().max(1.0e-4));
    let units: Vec<_> = match_contestants(world)
        .into_iter()
        .map(|(unit, _)| unit)
        .collect();
    // Health fraction and entity of every contestant id (entity index).
    let mut pawns: Vec<(u32, Entity, f32, bool)> = {
        let mut query = world.query::<(Entity, &Pawn, &Health)>();
        query
            .iter(world)
            .map(|(entity, pawn, health)| {
                let pct = if health.max > 0.0 {
                    health.current / health.max
                } else {
                    0.0
                };
                (entity.index().index(), entity, pct, pawn.is_bot)
            })
            .collect()
    };
    pawns.sort_by_key(|(id, ..)| *id);
    let (orders, carriers) = {
        let Some(rules) = world.get_resource::<MatchRules>() else {
            return;
        };
        let board = &rules.session.board;
        let mut carriers: Vec<u32> = board.objectives.iter().filter_map(|o| o.carrier_id).collect();
        carriers.sort_unstable();
        let hooks = BotHooks {
            level: AiLevel::Medium,
            world_seed: 0,
            free_for_all: false,
        };
        let bodies = hooks.bodies(board, &units, |unit| {
            pawns
                .binary_search_by_key(&unit.id, |(id, ..)| *id)
                .ok()
                .and_then(|i| pawns.get(i))
                .map_or(1.0, |(_, _, pct, _)| *pct)
        });
        let orders = hooks.orders(
            board,
            rules.session.flow.match_tick,
            &bodies,
            &contacts_of(&bodies),
            &[],
        );
        (orders, carriers)
    };
    let position_of = |world: &World, id: u32| -> Option<Vec3> {
        let index = pawns.binary_search_by_key(&id, |(i, ..)| *i).ok()?;
        let entity = pawns.get(index)?.1;
        world.get::<Transform>(entity).map(|t| t.translation)
    };
    for (id, order) in orders {
        let Ok(index) = pawns.binary_search_by_key(&id, |(i, ..)| *i) else {
            continue;
        };
        let Some(&(_, entity, _, is_bot)) = pawns.get(index) else {
            continue;
        };
        if !is_bot || world.get::<RespawnTimer>(entity).is_some() {
            continue;
        }
        let target = order.focus_enemy.and_then(|enemy| position_of(world, enemy));
        let Some(mut transform) = world.get_mut::<Transform>(entity) else {
            continue;
        };
        let here = transform.translation;
        let goal = Vec3::new(order.goal[0], here.y, order.goal[2]);
        let to_goal = goal - here;
        let step = MODE_BOT_SPEED * dt;
        let next = if to_goal.length_squared() <= step * step {
            goal
        } else {
            here + to_goal.normalize_or_zero() * step
        };
        transform.translation = crate::bots::clamp_to_arena(next);
        let eye = transform.translation;
        let Some(mut brain) = world.get_mut::<BotBrain>(entity) else {
            continue;
        };
        brain.cooldown = (brain.cooldown - dt).max(0.0);
        let Some(target) = target else {
            continue;
        };
        let runner = order.ctf == Some(CtfRole::Attack) && carriers.binary_search(&id).is_err();
        let aim = Vec3::new(target.x, eye.y, target.z) - eye;
        if runner || aim.length() > MODE_FIRE_RANGE || brain.cooldown > 0.0 {
            continue;
        }
        brain.cooldown = MODE_FIRE_COOLDOWN;
        brain.shots = brain.shots.saturating_add(1);
        let hit = world
            .get_resource_mut::<MatchRng>()
            .is_none_or(|mut rng| rng.rng.f32() < MODE_HIT_CHANCE);
        // A miss passes a meter to the side of the target.
        let wide = Vec3::new(-aim.z, 0.0, aim.x).normalize_or_zero();
        let point = if hit { aim } else { aim + wide };
        world.write_message(FireRequest {
            shooter: entity,
            position: eye,
            direction: point.normalize_or_zero(),
            weapon: Weapon::Rifle,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_words_round_trip_and_unknown_words_are_refused() {
        for mode in ArenaMode::ALL {
            assert_eq!(ArenaMode::parse(mode.word()), Ok(mode));
        }
        assert!(ArenaMode::parse("dom").is_err());
        assert!(!ArenaMode::Ffa.is_team());
        assert!(ArenaMode::Ctf.is_team());
    }

    #[test]
    fn slots_alternate_teams_and_map_back() {
        assert_eq!(seat_of_slot(0), SeatId::new(1, 0));
        assert_eq!(seat_of_slot(1), SeatId::new(2, 0));
        assert_eq!(seat_of_slot(2), SeatId::new(1, 1));
        assert_eq!(seat_of_slot(3), SeatId::new(2, 1));
        for slot in 0..16u8 {
            assert_eq!(slot_of_seat(seat_of_slot(slot)), Some(slot));
        }
        assert_eq!(slot_of_seat(SeatId::new(3, 0)), None);
    }

    #[test]
    fn a_ctf_session_from_the_shipped_rule_set_has_two_flags_and_four_seats() {
        let world = World::new();
        let mode = ArenaMatchMode::new(ArenaMode::Ctf);
        let resolved = resolve_mode(&world, &mode).expect("the shipped ctf rule set resolves");
        assert_eq!(resolved.mode, MatchMode::CaptureTheFlag);
        let session = team_session(&resolved, &ArenaConfig::default(), &mode, 4, 60.0);
        assert_eq!(session.board.mode, MatchMode::CaptureTheFlag);
        let flags: Vec<(u32, u8)> = session
            .board
            .objectives
            .iter()
            .filter(|o| o.is_flag())
            .map(|o| (o.entity_id, o.team_id))
            .collect();
        assert_eq!(flags, vec![(901, 1), (902, 2)]);
        assert_eq!(
            session.flow.seated,
            vec![
                SeatId::new(1, 0),
                SeatId::new(1, 1),
                SeatId::new(2, 0),
                SeatId::new(2, 1)
            ]
        );
        assert_eq!(
            session.settings().objectives.capture_limit,
            resolved.settings.objectives.capture_limit,
            "no override keeps the rule set's limit"
        );
    }

    #[test]
    fn a_koth_session_has_the_hill_and_the_limit_override() {
        let world = World::new();
        let mode = ArenaMatchMode {
            limit: Some(7),
            ..ArenaMatchMode::new(ArenaMode::Koth)
        };
        let resolved = resolve_mode(&world, &mode).expect("koth resolves");
        let session = team_session(&resolved, &ArenaConfig::default(), &mode, 4, 60.0);
        assert!(session.board.objective(HILL_ID).is_some_and(|o| o.is_hill()));
        assert_eq!(session.settings().objectives.hill_score_limit, 7);
    }

    #[test]
    fn the_arena_rule_set_extends_ctf_with_one_capture() {
        let mut registry = RuleSetRegistry::with_defaults();
        assert_eq!(load_arena_rule_sets(&mut registry), 1);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let mut world = World::new();
        world.insert_resource(registry);
        let mode = ArenaMatchMode {
            ruleset: "ctf_quick".into(),
            ..ArenaMatchMode::new(ArenaMode::Ctf)
        };
        let resolved = resolve_mode(&world, &mode).expect("ctf_quick resolves");
        assert_eq!(resolved.chain, ["ctf", "ctf_quick"]);
        assert_eq!(resolved.mode, MatchMode::CaptureTheFlag);
        assert_eq!(resolved.settings.objectives.capture_limit, 1);
        assert_eq!(resolved.settings.objectives.flag_return_ticks, Some(600));
    }

    #[test]
    fn an_unknown_rule_set_is_an_error_with_its_code() {
        let world = World::new();
        let mode = ArenaMatchMode {
            ruleset: "no_such_mode".into(),
            ..ArenaMatchMode::new(ArenaMode::Ctf)
        };
        let error = resolve_mode(&world, &mode).expect_err("unknown id");
        assert!(error.starts_with("ruleset_"), "{error}");
    }

    #[test]
    fn tdm_reports_the_first_team_to_the_kill_limit() {
        let world = World::new();
        let mode = ArenaMatchMode::new(ArenaMode::Tdm);
        let resolved = resolve_mode(&world, &mode).expect("tdm resolves");
        let config = ArenaConfig {
            warmup_secs: 0.0,
            ..ArenaConfig::default()
        };
        let mut session = team_session(&resolved, &config, &mode, 4, 60.0);
        let _ = session.step(&[]);
        session.credit_kill(SeatId::new(2, 0), SeatId::new(1, 0));
        session.credit_kill(SeatId::new(2, 1), SeatId::new(1, 1));
        assert_eq!(team_kills(&session), [0, 2]);
        report_tdm_limit(&mut session, 3);
        assert!(!session.board.match_over);
        report_tdm_limit(&mut session, 2);
        let step = session.step(&[]);
        assert!(session.board.match_over);
        assert!(
            step.events.iter().any(|e| matches!(
                e,
                regia_framework::rules::objective::RulesEvent::MatchOver { team: 2, .. }
            )),
            "{:?}",
            step.events
        );
    }

    /// The guide's `ctf_long` (`book/user-guide/game-modes.md`, "Making a mode without code")
    /// resolves on rules-ron v1 with a 36_000-tick limit and a 3_600-tick overtime. The tutorial
    /// says to add that file and run `--ruleset ctf_long`. The session [`team_session`] builds
    /// must keep those phase settings (today it copies [`ArenaConfig::phase_settings`] over them).
    #[test]
    fn guide_ctf_long_keeps_its_timer_and_overtime() {
        use regia_framework::rules::phases::TimeUp;

        // The ron fence in the guide, unchanged.
        const CTF_LONG: &str = r#"(
    format_version: 1,
    id: "ctf_long",
    name_key: "mode.ctf_long",
    extends: "ctf",
    session: (
        flow: (start_delay_ticks: 300),
        phases: (time_limit_ticks: 36000, on_time_up: (kind: "overtime", ticks: 3600)),
        objectives: (capture_limit: 5, flag_return_ticks: 1800),
        respawn: (default: (kind: "delay", ticks: 180)),
    ),
    options: [
        (key: "sky", kind: enum, default: "day", choices: ["day", "dusk", "night"]),
    ],
)"#;
        let mut registry = RuleSetRegistry::with_defaults();
        registry
            .load_text("assets/rules/ctf_long.rules.ron", CTF_LONG)
            .expect("the guide's ctf_long loads on rules-ron v1");
        let mut world = World::new();
        world.insert_resource(registry);
        let mode = ArenaMatchMode {
            ruleset: "ctf_long".into(),
            ..ArenaMatchMode::new(ArenaMode::Ctf)
        };
        let resolved = resolve_mode(&world, &mode).expect("ctf_long resolves");
        assert_eq!(resolved.chain, ["ctf", "ctf_long"]);
        assert_eq!(resolved.settings.objectives.capture_limit, 5);
        assert_eq!(resolved.settings.flow.start_delay_ticks, 300);
        assert_eq!(resolved.settings.flow.phases.time_limit_ticks, Some(36_000));
        assert_eq!(
            resolved.settings.flow.phases.on_time_up,
            TimeUp::Overtime { ticks: Some(3_600) },
            "the guide example parses its overtime"
        );
        let session = team_session(&resolved, &ArenaConfig::default(), &mode, 4, 60.0);
        let phases = session.settings().flow.phases;
        assert_eq!(
            session.settings().objectives.capture_limit,
            5,
            "objective fields of ctf_long are kept"
        );
        assert_eq!(
            phases.time_limit_ticks,
            Some(36_000),
            "team_session replaced ctf_long's 36_000-tick timer with {:?}",
            phases.time_limit_ticks
        );
        assert_eq!(
            phases.on_time_up,
            TimeUp::Overtime { ticks: Some(3_600) },
            "team_session replaced ctf_long's overtime with {:?}",
            phases.on_time_up
        );
    }
}
