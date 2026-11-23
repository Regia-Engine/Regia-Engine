// SPDX-License-Identifier: MPL-2.0
//! Game rules (`docs/specs/14-scene-and-framework.md` § B3): per-level/project rules object
//! that governs spawning, win/lose conditions, and match flow.
//!
//! A [`GameRules`] trait object resource is chosen by the level settings (when the scene
//! system loads a level). The default, [`DefaultRules`], provides free-for-all spawning at
//! [`PlayerStart`]s with no match limits — the minimum playable game.
//!
//! # Custom game rules
//!
//! Implement [`GameRules`] for a type and register it by name in [`GameRulesRegistry`]; at level
//! load the name in `LevelSettings.game_rules` picks the rules (an unknown name warns and falls back
//! to [`DefaultRules`]). Because rules need to survive hot reload, their state lives
//! in reflected resources and components, not in the trait object itself.
//!
//! # Match modes and match flow
//!
//! The submodules re-export the ECS-free, lockstep-safe `regia_rules` core (ADR-8239): [`mode`] (the mode
//! selector), [`objective`] (objective entities, the board of scores and events), [`ctf`] and
//! [`koth`] (the capture-the-flag and king-of-the-hill skeletons), [`match_flow`] (pause, ready-up
//! on an agreed step, observers, join requests), [`playlist`] and [`session`] (all of them stepped
//! together). They use only `+ - *` and comparisons on plain data and are scanned by
//! `cargo xtask det-scan`, so a lockstep game's sim can step them on every peer. [`MatchRules`] holds
//! a [`session::MatchSession`] as a resource for the agent protocol's `rules.match_*` methods
//! (`book/agents/match-rules.md`). [`bots`] hooks the deterministic mode bots of `regia_detsim::ai` to
//! the board (plan row S-13, ADR-8554; `book/agents/det-ai-squads.md`).

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;
use regia_scene::level::LevelSettings;

use crate::player::PlayerStart;

pub use regia_rules::{
    RulesError, bots, ctf, end_stats, koth, map_vote, match_flow, mode, objective, phases, playlist, proof,
    race, ruleset, series, session, spawn, teams, vision, waves, win_condition, zones,
};
// Shrinking zones, elimination placements and strategy victory conditions (GR-11.3, GR-11.4).
pub use regia_rules::{elimination, victory, zone};

mod session_adapter;

pub use session_adapter::{
    DeathCredited, MatchEvent, MatchEventUnits, MatchRulesSet, MatchSessionAdapter, MatchSessionPlugin,
    MatchSessionRules, ObjectiveEntity, claim_pawn_seat, match_contestants, match_session_score_sheet,
    record_death, seat_player,
};

// Replicated match state for client/server games (feature `net`).
#[cfg(feature = "net")]
pub mod match_replication;
#[cfg(feature = "net")]
pub use match_replication::{
    MatchEventMsg, MatchPhaseView, MatchStateCounters, MatchStateEntity, MatchStateReplicationPlugin,
    MatchStateSettings, ObjectiveViews, PlayerScoreView, ReplicatedMatchRules, TeamScoresView,
};

mod match_events;

pub use match_events::{MATCH_EVENT_KINDS, MatchEventFields, MatchEventListener, score_reason_str};

/// The hosted match session as a resource: the state the agent protocol's `rules.match_*` methods
/// read and drive (`book/agents/match-rules.md`). A lockstep game may keep its session here or in
/// its own sim state; nothing steps it on its own unless [`MatchSessionPlugin`] is installed
/// (a server-authoritative game: one step per fixed tick from ECS).
///
/// Reflected as one opaque value (serde and `Debug`), so hot reload keeps it and
/// [`crate::state_hash::insert_resource_text`] with `"MatchRules"` adds it to a world snapshot
/// (ADR-8657). [`MatchRulesPlugin`] registers it.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(opaque)]
#[reflect(Resource, Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchRules {
    /// The session.
    pub session: session::MatchSession,
}

/// Options of the playlist row the dedicated host last applied
/// (`book/user-guide/game-modes.md`). Rules read this when a row names limits or similar.
#[derive(Resource, Debug, Clone, Default)]
pub struct MatchOptions {
    /// Key/value options from the row, in key order.
    pub options: std::collections::BTreeMap<String, String>,
}

/// The mode id of the playlist row the dedicated host last applied. Empty means the level's own
/// `LevelSettings.game_rules`.
#[derive(Resource, Debug, Clone, Default)]
pub struct RotationModeOverride {
    /// Mode id, or empty for the level's own rules.
    pub mode: String,
}

/// Inserts a default [`MatchRules`] resource when the app has none. Adds no system: the host steps
/// the session from its own lockstep tick, or adds [`MatchSessionPlugin`] to step it from ECS.
pub struct MatchRulesPlugin;

impl Plugin for MatchRulesPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<TeamId>()
            .register_type::<LedgerSlot>()
            .register_type::<MatchRules>()
            .register_type::<DeathCredited>()
            .register_type::<crate::damage::FriendlyFire>()
            .register_type::<crate::damage::SelfDamage>()
            .init_resource::<MatchRules>()
            .init_resource::<crate::ruleset_file::RuleSetRegistry>();
        // The replicated views' named winner is server state of the match: registered with the
        // rules, so a world snapshot of any app with them keeps it (match-state replication
        // review finding A).
        #[cfg(feature = "net")]
        app.register_type::<match_replication::MatchOverSeen>();
    }
}

/// The seat `seat` ends on after `moves` (oldest first), or `None` when no move touches it.
///
/// Batches apply in order; the moves of one batch apply at once (a swap), so each batch is one
/// lookup of the seat held after the earlier batches.
pub(crate) fn moved_seat(moves: &[teams::SeatMove], seat: match_flow::SeatId) -> Option<match_flow::SeatId> {
    let mut current = seat;
    let mut moved = false;
    let mut start = 0;
    while start < moves.len() {
        let batch = moves[start].batch;
        let end = moves[start..]
            .iter()
            .position(|step| step.batch != batch)
            .map_or(moves.len(), |offset| start + offset);
        if let Some(step) = moves[start..end].iter().find(|step| step.from == current) {
            current = step.to;
            moved = true;
        }
        start = end;
    }
    moved.then_some(current)
}

/// Retag entities after seat moves: every entity whose [`TeamId`] and [`LedgerSlot`] name a
/// `from` seat of [`session::MatchSession::seat_moves`] gets the matching `to` seat. The list is
/// drained. Returns how many entities were retagged.
///
/// Batches apply in order and the moves of one batch at once, so a swap retags both sides. Call this in the step that called
/// [`session::MatchSession::assign_team`] or stepped a launch, before damage runs: damage credits
/// the seat these components name, so a stale tag would credit the coordinate the player left
/// (ADR-8657). The agent's `rules.match_assign` and `rules.match_step` call it. Entities are
/// visited in the world's entity order.
pub fn retag_moved_seats(world: &mut World) -> usize {
    let moves = match world.get_resource_mut::<MatchRules>() {
        Some(mut rules) => std::mem::take(&mut rules.session.seat_moves),
        None => return 0,
    };
    if moves.is_empty() {
        return 0;
    }
    let final_seat = |seat: match_flow::SeatId| moved_seat(&moves, seat);
    let mut query = world.query::<(&mut TeamId, &mut LedgerSlot)>();
    let mut retagged = 0;
    for (mut team, mut slot) in query.iter_mut(world) {
        if team.0 == 0 {
            continue;
        }
        if let Some(to) = final_seat(match_flow::SeatId::new(team.0, slot.0)) {
            team.0 = to.team;
            slot.0 = to.slot;
            retagged += 1;
        }
    }
    retagged
}

/// Rules team of an entity. `0` is neutral and is never a teammate.
///
/// The presentation component [`crate::sim_bridge::Team`] is a separate `u32` used for team
/// colour. A presentation value `n` corresponds to this id when `n` fits in a `u8`. Nothing
/// copies one into the other.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[reflect(Component, Default)]
pub struct TeamId(pub u8);

/// Slot on [`TeamId`]'s team. Damage credits the match ledger only when both are present.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[reflect(Component)]
pub struct LedgerSlot(pub u8);

/// The context a [`GameRules`] method receives: the world, and nothing else. Messages and random
/// numbers come through the [`World`] (its message resources and `regia_core`'s RNG). Built like
/// [`ScriptCtx`](crate::script::ScriptCtx) so a single [`World`] reference is the authority.
pub struct RulesCtx<'w> {
    /// The ECS world.
    pub world: &'w mut World,
}

impl<'w> RulesCtx<'w> {
    /// Wraps the world.
    pub fn new(world: &'w mut World) -> Self {
        Self { world }
    }
}

/// Per-level/project rules object (§ B3).
///
/// The trait is object-safe because the concrete implementation is stored as a `Box<dyn GameRules>`
/// inside a resource. State that must survive hot reload lives in reflected resources and
/// components, not here.
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_core::math::Vec3;
/// use regia_framework::prelude::*;
///
/// // The default rules spawn a player at the first `PlayerStart` in spawn order.
/// let mut app = App::new();
/// app.add_plugins(MinimalPlugins);
/// app.world_mut().spawn((
///     regia_scene::Transform {
///         translation: Vec3::new(1.0, 2.0, 3.0),
///         ..Default::default()
///     },
///     PlayerStart,
/// ));
/// let player = app.world_mut().spawn_empty().id();
///
/// let mut rules = DefaultRules;
/// let mut ctx = RulesCtx::new(app.world_mut());
/// let req = rules
///     .on_player_joined(&mut ctx, player)
///     .expect("default rules always return a spawn request");
/// assert_eq!(req.transform.translation, Vec3::new(1.0, 2.0, 3.0));
/// assert!(req.prefab.is_none());
/// ```
pub trait GameRules: Send + Sync + 'static {
    /// Called once when the level starts (after all entities are spawned from the level file).
    /// Game-polled: the game calls this from its own systems; `RulesPlugin` does not.
    fn on_level_start(&mut self, _ctx: &mut RulesCtx) {}

    /// A participant joined (local, remote or connectionless server bot). The rules decide what to spawn: a spawn
    /// point, a prefab (character class), and an optional team. Return `None` to accept the player
    /// but spawn nothing (e.g. a spectator).
    ///
    /// Local players: called by the `PlayerManager` join flow with the `Player` entity. Remote
    /// players (feature `net`, spec 77 § 9): called on the server once per player when it first
    /// reaches `InGame`, with its `NetPlayer` entity; the spawned character is owned by the
    /// player's peer (see the `net_session_bridge` module).
    fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest>;

    /// A participant left. Local players and server bots: called by `PlayerManager::remove_player`. Remote players
    /// (feature `net`): called on the server once when the player leaves the session, before its
    /// `NetPlayer` entity is despawned and before the peer's owned entities are handed on, and only
    /// for a player whose `on_player_joined` ran. The character spawned from this player's
    /// `on_player_joined` request carries `regia_net::session::NetDespawnWithOwner` and is
    /// despawned by that hand-off (KI-GP-033); remove the marker here to keep the character.
    fn on_player_left(&mut self, _ctx: &mut RulesCtx, _player: Entity) {}

    /// A player needs a spawn point (respawn, initial join). The rules may override the default
    /// logic that picks the first `PlayerStart` in spawn order ([`default_spawn_point`]).
    /// Game-polled: neither `RulesPlugin` nor `DefaultRules::on_player_joined` calls it; a rules
    /// type that only
    /// overrides this still gets the default spawn on local join unless its own
    /// `on_player_joined` calls it.
    fn choose_spawn(&mut self, ctx: &mut RulesCtx, player: Entity) -> Vec3 {
        default_spawn_point(ctx.world, player)
    }

    /// A [`Character`](crate::character::Character) (player or AI) died. Game-polled: the game calls
    /// this from its own `DiedEvent` handling; `RulesPlugin` does not.
    fn on_character_died(&mut self, _ctx: &mut RulesCtx, _character: Entity) {}

    /// For match-clock and global logic (e.g. "match ends when timer reaches zero"). Game-polled:
    /// the game calls this once per frame from its own system; `RulesPlugin` does not.
    fn tick(&mut self, _ctx: &mut RulesCtx) {}

    /// Whether the match is over. The game polls this (the framework does not drive it
    /// from `RulesPlugin`) and may then show a scoreboard / restart.
    fn is_match_over(&self, _world: &World) -> bool {
        false
    }

    /// The match's standings, for a scoreboard. The default has none. With the `client` feature,
    /// [`crate::game_ui::GameUiBridgePlugin`] calls this each frame while a `regia_ui` scoreboard
    /// showing the rules is on screen, and a dedicated server may call it for a query answer. The
    /// rules read whatever components hold the scores: on a client, the replicated ones.
    fn scoreboard(&mut self, _ctx: &mut RulesCtx) -> Option<ScoreSheet> {
        None
    }

    /// The concrete rules type's path, for diagnostics and the agent protocol's `rules.get`
    /// (`book/agents/runtime-gameplay.md`). Defaults to [`std::any::type_name`] of the
    /// implementing type; override it when the type name is not what operators should see.
    fn type_path(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// One cell of a [`ScoreSheet`].
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreCell {
    /// A number: sorts numerically, and team totals add it up.
    Int(i64),
    /// Text (a ping label, a class name).
    Text(String),
}

/// One column of a [`ScoreSheet`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreSheetColumn {
    /// Stable key.
    pub key: String,
    /// Heading (`"loc:<key>"` for a localization key); empty uses the key.
    pub title: String,
    /// Width in UI px; `0` uses the widget's default.
    pub width: f32,
}

impl ScoreSheetColumn {
    /// A column keyed `key`, headed `title`.
    #[must_use]
    pub fn new(key: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            width: 0.0,
        }
    }
}

/// One team of a [`ScoreSheet`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreSheetTeam {
    /// Team index, as in [`SpawnRequest::team`] and [`ScoreSheetRow::team`].
    pub id: u8,
    /// Heading.
    pub name: String,
    /// Linear RGBA tint of the heading.
    pub color: Option<[f32; 4]>,
}

/// One player of a [`ScoreSheet`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreSheetRow {
    /// Stable id: it breaks sort ties, so every client shows the same order.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Team index.
    pub team: Option<u8>,
    /// This machine's player.
    pub local: bool,
    /// One cell per column.
    pub values: Vec<ScoreCell>,
}

/// The match's standings ([`GameRules::scoreboard`]). Plain data: it builds on a dedicated server,
/// which has no UI.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreSheet {
    /// Columns.
    pub columns: Vec<ScoreSheetColumn>,
    /// Players, in any order.
    pub rows: Vec<ScoreSheetRow>,
    /// Teams; a team without an entry gets a default heading.
    pub teams: Vec<ScoreSheetTeam>,
}

/// A score-sheet row id: the ledger row's own id (handed out once per session, never `0`, kept
/// across a team change, new after a leave and a rejoin). A row decoded from a session saved
/// before row ids existed has id `0`; it maps to `2^63 | team << 8 | slot`, outside the range the
/// counter reaches (ADR-8657).
fn score_row_id(row: &teams::PlayerLedger) -> u64 {
    if row.id != 0 {
        row.id
    } else {
        (1 << 63) | (u64::from(row.team) << 8) | u64::from(row.slot)
    }
}

/// Standings built from the session's team definitions and per-seat ledgers.
///
/// Rows are ordered by score (highest first), then kills, then team, then slot. Colors are the
/// definition's bytes scaled by `1/255`. An empty name key stays empty.
#[must_use]
pub fn score_sheet(session: &session::MatchSession) -> ScoreSheet {
    let mut defs = session.teams.defs.clone();
    defs.sort_by_key(|def| def.id);
    let mut seen = Vec::new();
    let mut teams = Vec::new();
    for def in defs {
        if seen.contains(&def.id) {
            continue;
        }
        seen.push(def.id);
        teams.push(ScoreSheetTeam {
            id: def.id,
            name: def.name_key.clone(),
            color: Some([
                f32::from(def.color[0]) / 255.0,
                f32::from(def.color[1]) / 255.0,
                f32::from(def.color[2]) / 255.0,
                f32::from(def.color[3]) / 255.0,
            ]),
        });
    }
    let mut rows_src = session.ledgers.rows.clone();
    rows_src.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(right.kills.cmp(&left.kills))
            .then(left.team.cmp(&right.team))
            .then(left.slot.cmp(&right.slot))
    });
    let rows = rows_src
        .iter()
        .map(|row| ScoreSheetRow {
            id: score_row_id(row),
            name: match_flow::seat_label(row.seat()),
            team: Some(row.team),
            local: false,
            values: vec![
                ScoreCell::Int(row.score),
                ScoreCell::Int(i64::from(row.kills)),
                ScoreCell::Int(i64::from(row.deaths)),
                ScoreCell::Int(i64::from(row.assists)),
                ScoreCell::Int(i64::from(row.objective_points)),
            ],
        })
        .collect();
    ScoreSheet {
        columns: vec![
            ScoreSheetColumn::new("score", "Score"),
            ScoreSheetColumn::new("kills", "Kills"),
            ScoreSheetColumn::new("deaths", "Deaths"),
            ScoreSheetColumn::new("assists", "Assists"),
            ScoreSheetColumn::new("objective", "Objective"),
        ],
        rows,
        teams,
    }
}

/// What the rules want spawned for a player.
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// Where to place the character.
    pub transform: Transform,
    /// Optional project-relative `.prefab.ron` path or registered `asset:<guid>` reference.
    /// Local and remote joins instantiate it through the scene's prefab library and possess
    /// its authored root, retaining its components. The request overrides its transform and
    /// team (`team: None` removes any authored team). `None`, a missing prefab or a failed
    /// instantiation spawns the default character (failures log a warning). Nested prefabs
    /// must already be in the library.
    pub prefab: Option<String>,
    /// Team index (0-based); `None` means free-for-all. Local join stores it as a
    /// [`Team`](crate::sim_bridge::Team) component on the pawn.
    pub team: Option<u8>,
}

/// Free-for-all rules: players appear at [`PlayerStart`]s, no match limits, no score.
///
/// The default when `LevelSettings.game_rules` is absent or unresolvable. Games that want
/// nothing more than "walk around and test things" never need to implement [`GameRules`].
#[derive(Default)]
pub struct DefaultRules;

impl GameRules for DefaultRules {
    fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest> {
        let pos = default_spawn_point(ctx.world, player);
        Some(SpawnRequest {
            transform: Transform {
                translation: pos,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            },
            prefab: None,
            team: None,
        })
    }

    /// The session's spawn policy over [`PlayerStart`]s and their `SpawnPoint` data
    /// ([`crate::spawn_select::choose_spawn`], ADR-8647). With no spawn data and no session policy it
    /// is the first start in spawn order; with no eligible start, [`default_spawn_point`].
    fn choose_spawn(&mut self, ctx: &mut RulesCtx, player: Entity) -> Vec3 {
        match crate::spawn_select::choose_spawn(ctx.world, player) {
            Some(pick) => pick.position,
            None => default_spawn_point(ctx.world, player),
        }
    }
}

/// Finds a spawn point for `player`: the first [`PlayerStart`] in spawn order, else origin.
/// "First" is the arrival order of `PlayerStart` when the world tracks it
/// (`regia_ecs::spawn_order::SpawnOrder<PlayerStart>`, which
/// [`PlayerPlugin`](crate::player::PlayerPlugin) installs), else the start with the lowest
/// translation (x, then y, then z; [`crate::spawn_select::StartKey`], ADR-8704); never an entity
/// index, which recycling reuses, and never the order the starts' archetypes happen to be stored in.
///
/// [`PlayerStart`] entities are defined in `crate::player`. This function is public so rules
/// implementations can call it or build their own logic.
///
/// A start carrying [`crate::player::PlayerStartDisabled`] is skipped. When every start is
/// disabled, the first of them is used anyway (a spawn at the world origin would be worse).
pub fn default_spawn_point(world: &mut World, _player: Entity) -> Vec3 {
    let mut query = world
        .query_filtered::<(Entity, &Transform, Has<crate::player::PlayerStartDisabled>), With<PlayerStart>>();
    let world: &World = world;
    let order = world.get_resource::<regia_ecs::spawn_order::SpawnOrder<PlayerStart>>();
    // Enabled starts sort before disabled ones, then the spawn order decides (ADR-8731: the
    // logic-nodes skip over ADR-8704's start key).
    query
        .iter(world)
        .min_by_key(|(entity, start, disabled)| {
            (
                *disabled,
                crate::spawn_select::start_key(order, *entity, start.translation),
            )
        })
        .map_or(Vec3::ZERO, |(_, start, _)| start.translation)
}

/// A [`GameRules`] trait object stored as a resource.
///
/// Insert this before the level loads (or during `GamePlugin::register`); the level system picks
/// it up and calls its methods. The default is [`DefaultRules`].
#[derive(Resource)]
pub struct GameRulesResource {
    /// The boxed rules object.
    pub rules: Box<dyn GameRules>,
}

impl Default for GameRulesResource {
    fn default() -> Self {
        Self {
            rules: Box::new(DefaultRules),
        }
    }
}

impl GameRulesResource {
    /// Replaces the current rules with `new_rules`.
    pub fn set(&mut self, new_rules: Box<dyn GameRules>) {
        self.rules = new_rules;
    }
}

/// Plugin that initialises the default rules resource.
///
/// Games that install custom rules replace the resource after registration (or insert their own
/// before this plugin runs). With the `net` feature it also adds `NetSessionBridgePlugin` (in the
/// `net_session_bridge` module), so a session's remote players reach
/// [`GameRules::on_player_joined`] and [`GameRules::on_player_left`]. The plugin reference stays
/// plain text because the module only exists with the `net` feature; an intra-doc link would
/// break `cargo doc` on the default feature set.
pub struct RulesPlugin;

impl Plugin for RulesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GameRulesResource>()
            .init_resource::<GameRulesRegistry>()
            .add_systems(OnLevelLoaded, apply_level_rules);
        let mut registry = app.world_mut().resource_mut::<GameRulesRegistry>();
        registry.register_default::<DefaultRules>();
        #[cfg(feature = "net")]
        if !app.has_plugin(std::any::type_name::<
            crate::net_session_bridge::NetSessionBridgePlugin,
        >()) {
            app.add_plugins(crate::net_session_bridge::NetSessionBridgePlugin);
        }
    }
}

/// Constructs a [`GameRules`] instance by its registered name ([`GameRulesRegistry`]).
pub type RulesCtor = fn() -> Box<dyn GameRules>;

/// Name → constructor registry the level loader resolves `LevelSettings.game_rules` against
/// (§ B3).
///
/// Games register their rules during `GamePlugin::register` (or plugin `build`) so levels can
/// select them by type path:
///
/// ```
/// use regia_framework::prelude::*;
/// use regia_app::prelude::*;
/// # #[derive(Default)]
/// # struct ArenaRules;
/// # impl GameRules for ArenaRules {
/// #     fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> { None }
/// # }
/// fn register(app: &mut App) {
///     app.world_mut()
///         .resource_mut::<GameRulesRegistry>()
///         .register::<ArenaRules>("my_game::ArenaRules");
/// }
/// ```
#[derive(Resource, Default)]
pub struct GameRulesRegistry {
    ctors: std::collections::HashMap<String, RulesCtor>,
}

impl GameRulesRegistry {
    /// Registers `name` → a constructor for `R`. Registering a name again replaces it (the last
    /// registration wins, so a game can override an engine name).
    pub fn register<R: GameRules + Default>(&mut self, name: &str) {
        self.ctors.insert(name.to_string(), || Box::new(R::default()));
    }

    /// Registers `R` under its full type path *and* its bare type name (so a level can ask for
    /// `my_game::ArenaRules` or just `ArenaRules`). This is how the engine's [`DefaultRules`]
    /// entries are registered.
    pub fn register_default<R: GameRules + Default>(&mut self) {
        let full = std::any::type_name::<R>();
        self.register::<R>(full);
        if let Some(short) = full.rsplit("::").next() {
            self.register::<R>(short);
        }
    }

    /// The names that resolve, sorted (agent/UI listing).
    #[must_use]
    pub fn registered_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.ctors.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    fn get(&self, name: &str) -> Option<RulesCtor> {
        self.ctors.get(name).copied()
    }
}

/// Resolves [`LevelSettings::game_rules`] when a level finishes loading (`OnLevelLoaded`,
/// § B3): the name is looked up in [`GameRulesRegistry`] and a fresh instance replaces
/// [`GameRulesResource`].
///
/// - No `game_rules` in the level → whatever rules are already installed stay (a game's own
///   plugin-installed rules are the default then; the resource is created if absent).
/// - A name that resolves → the rules are replaced by a fresh instance of that type.
/// - An unknown name → `warn` and fall back to [`DefaultRules`], so a typo'd level still plays.
///
/// A non-empty [`RotationModeOverride`] wins over `LevelSettings.game_rules`, so a playlist row's
/// mode survives a level file that names its own rules.
pub(crate) fn apply_level_rules(world: &mut World) {
    let Some(name) = rotation_mode_name(world) else {
        world.init_resource::<GameRulesResource>();
        return;
    };
    let ctor = match world.resource::<GameRulesRegistry>().get(&name) {
        Some(ctor) => Some(ctor),
        None => {
            tracing::warn!(
                target: "regia_framework",
                "level requests unknown game rules `{name}`; falling back to DefaultRules \
                 (register the name in GameRulesRegistry during GamePlugin::register)"
            );
            world
                .resource::<GameRulesRegistry>()
                .get("regia_framework::rules::DefaultRules")
        }
    };
    let Some(ctor) = ctor else {
        // The registry was replaced wholesale and no longer knows DefaultRules.
        tracing::warn!(
            target: "regia_framework",
            "game rules registry has no DefaultRules entry; inserting the default resource"
        );
        world.init_resource::<GameRulesResource>();
        return;
    };
    if let Some(mut rules) = world.get_resource_mut::<GameRulesResource>() {
        rules.rules = ctor();
    } else {
        world.insert_resource(GameRulesResource { rules: ctor() });
    }
}

/// The mode a level should run: a playlist row's mode when the host set one, otherwise the
/// level file's `game_rules`.
fn rotation_mode_name(world: &World) -> Option<String> {
    if let Some(over) = world.get_resource::<RotationModeOverride>()
        && !over.mode.is_empty()
    {
        return Some(over.mode.clone());
    }
    world
        .get_resource::<LevelSettings>()
        .and_then(|settings| settings.game_rules.clone())
}

/// The leaderboard sink for race laps (ADR-8695): post the `local` seat's new best laps among
/// `events` (a step's [`regia_rules::objective::RulesEvent`]s) to the online board `board`, one
/// upload per `racer_lap { best: true }`, the score being the lap in match ticks. Declare the board
/// with an ascending sort so a shorter lap ranks higher. Returns how many uploads were queued.
pub fn post_best_laps(
    leaderboard: &mut crate::leaderboard::Leaderboard,
    board: &str,
    local: regia_rules::match_flow::SeatId,
    events: &[regia_rules::objective::RulesEvent],
) -> Result<u32, crate::leaderboard::LeaderboardError> {
    let mut posted = 0u32;
    for event in events {
        if let regia_rules::objective::RulesEvent::RacerLap {
            team,
            slot,
            lap_ticks,
            best: true,
            ..
        } = event
            && regia_rules::match_flow::SeatId::new(*team, *slot) == local
        {
            leaderboard.submit(board, i32::try_from(*lap_ticks).unwrap_or(i32::MAX), &[])?;
            posted = posted.saturating_add(1);
        }
    }
    Ok(posted)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    #[test]
    fn best_laps_of_the_local_seat_reach_the_leaderboard_sink() {
        use regia_rules::match_flow::SeatId;
        use regia_rules::objective::RulesEvent;

        let mut board =
            crate::leaderboard::Leaderboard::new(Arc::new(crate::leaderboard::UnavailableLeaderboards));
        let lap = |slot: u8, lap_ticks: u64, best: bool| RulesEvent::RacerLap {
            tick: 100,
            team: 1,
            slot,
            lap: 1,
            lap_ticks,
            best,
        };
        let events = [lap(0, 900, true), lap(1, 800, true), lap(0, 950, false)];
        let posted = post_best_laps(&mut board, "lap_times", SeatId::new(1, 0), &events).expect("post");
        assert_eq!(posted, 1, "only the local seat's best lap");
        assert_eq!(board.pending(), 1);
    }

    #[test]
    fn default_rules_spawns_at_player_start() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.world_mut().spawn((
            Transform {
                translation: Vec3::new(1.0, 2.0, 3.0),
                ..Default::default()
            },
            PlayerStart,
        ));
        let player = app.world_mut().spawn_empty().id();
        app.insert_resource(GameRulesResource::default());

        let req = {
            let mut rules = app
                .world_mut()
                .remove_resource::<GameRulesResource>()
                .expect("GameRulesResource inserted above");
            let mut ctx = RulesCtx::new(app.world_mut());
            rules.rules.on_player_joined(&mut ctx, player)
        };
        let req = req.expect("default rules always returns a spawn request");
        assert_eq!(req.transform.translation, Vec3::new(1.0, 2.0, 3.0));
        assert!(req.prefab.is_none());
        assert!(req.team.is_none());
    }

    /// "First" is entity order, not archetype order: a start that moves to another archetype
    /// (a `Name` added later) is still the first (audit framework5 #12).
    #[test]
    fn default_spawn_point_is_the_first_start_by_entity_not_by_archetype() {
        let mut app = App::new();
        let start = |at: Vec3| {
            (
                Transform {
                    translation: at,
                    ..Default::default()
                },
                PlayerStart,
            )
        };
        let a = app.world_mut().spawn(start(Vec3::new(1.0, 2.0, 3.0))).id();
        app.world_mut().spawn(start(Vec3::new(9.0, 0.0, 0.0)));
        app.world_mut()
            .entity_mut(a)
            .insert(regia_scene::components::Name::new("a"));
        let player = app.world_mut().spawn_empty().id();
        assert_eq!(
            default_spawn_point(app.world_mut(), player),
            Vec3::new(1.0, 2.0, 3.0)
        );
    }

    /// Spec 14 B3: the default spawn ignores tags: a tagged later start does not win over the
    /// first start in entity order.
    #[test]
    fn default_spawn_point_ignores_tags() {
        let mut app = App::new();
        app.world_mut().spawn((
            Transform {
                translation: Vec3::new(1.0, 0.0, 0.0),
                ..Default::default()
            },
            PlayerStart,
        ));
        app.world_mut().spawn((
            Transform {
                translation: Vec3::new(2.0, 0.0, 0.0),
                ..Default::default()
            },
            PlayerStart,
            regia_scene::components::Tags::from_iter_str(["red", "player"]),
        ));
        let player = app.world_mut().spawn_empty().id();
        assert_eq!(
            default_spawn_point(app.world_mut(), player),
            Vec3::new(1.0, 0.0, 0.0)
        );
    }

    #[test]
    fn default_spawn_point_falls_back_to_origin() {
        let mut app = App::new();
        let player = Entity::PLACEHOLDER;
        let pos = default_spawn_point(app.world_mut(), player);
        assert_eq!(pos, Vec3::ZERO);
    }

    #[test]
    fn choose_spawn_defaults_to_spawn_point() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.world_mut().spawn((
            Transform {
                translation: Vec3::new(10.0, 0.0, -5.0),
                ..Default::default()
            },
            PlayerStart,
        ));
        let player = Entity::PLACEHOLDER;

        let mut rules = DefaultRules;
        let mut ctx = RulesCtx::new(app.world_mut());
        let pos = rules.choose_spawn(&mut ctx, player);
        assert_eq!(pos, Vec3::new(10.0, 0.0, -5.0));
    }

    #[test]
    fn default_rules_match_is_never_over() {
        let app = App::new();
        let rules = DefaultRules;
        assert!(!rules.is_match_over(app.world()));
    }

    // ---- LevelSettings.game_rules resolution (§ B3) ----

    /// Rules distinguishable from the default through a sentinel: `is_match_over` is
    /// unconditionally true (DefaultRules answer false).
    #[derive(Default)]
    struct TimedRules;

    impl GameRules for TimedRules {
        fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
            None
        }
        fn is_match_over(&self, _world: &World) -> bool {
            true
        }
    }

    fn rules_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(RulesPlugin);
        app.world_mut()
            .resource_mut::<GameRulesRegistry>()
            .register::<TimedRules>("my_game::TimedRules");
        app
    }

    fn load_level_with_rules(app: &mut App, game_rules: Option<&str>) {
        app.world_mut().insert_resource(LevelSettings {
            game_rules: game_rules.map(str::to_string),
            ..LevelSettings::default()
        });
        app.world_mut().run_schedule(OnLevelLoaded);
    }

    #[test]
    fn level_rules_are_resolved_by_registered_name() {
        let mut app = rules_app();
        load_level_with_rules(&mut app, Some("my_game::TimedRules"));
        assert!(
            app.world()
                .resource::<GameRulesResource>()
                .rules
                .is_match_over(app.world()),
            "the TimedRules instance is active, not DefaultRules"
        );
        let names = app.world().resource::<GameRulesRegistry>().registered_names();
        assert!(names.contains(&"my_game::TimedRules"), "{names:?}");
        assert!(names.contains(&"DefaultRules"), "built-ins registered: {names:?}");
    }

    #[test]
    fn unknown_level_rules_fall_back_to_default_with_a_warning() {
        let mut app = rules_app();
        // Capture the `tracing` output of the load so the fallback's `warn!` is asserted, not
        // just the resulting resource (a silent fallback would pass the resource check).
        let sink = SharedSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer({
                let sink = sink.clone();
                move || sink.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            load_level_with_rules(&mut app, Some("my_game::MissingRules"));
        });
        assert!(
            !app.world()
                .resource::<GameRulesResource>()
                .rules
                .is_match_over(app.world()),
            "an unknown name must fall back to DefaultRules (which never end the match)"
        );
        let text = String::from_utf8(sink.0.lock().expect("sink lock").clone()).expect("utf-8");
        assert!(
            text.contains("unknown game rules") && text.contains("DefaultRules"),
            "the fallback must warn about the unknown name; captured: {text:?}"
        );
    }

    /// A `std::io::Write` that appends to a shared byte buffer (captures `tracing` output for
    /// a scoped `with_default` assertion).
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn rules_have_no_scoreboard_by_default() {
        let mut app = App::new();
        let mut rules = DefaultRules;
        assert_eq!(rules.scoreboard(&mut RulesCtx::new(app.world_mut())), None);
    }

    #[test]
    fn a_level_without_rules_keeps_the_installed_rules() {
        let mut app = rules_app();
        app.world_mut().insert_resource(GameRulesResource {
            rules: Box::new(TimedRules),
        });
        load_level_with_rules(&mut app, None);
        assert!(
            app.world()
                .resource::<GameRulesResource>()
                .rules
                .is_match_over(app.world()),
            "game-installed rules survive a level that does not name any"
        );
    }

    #[test]
    fn score_sheet_sorts_by_score_then_kills_then_seat() {
        use regia_rules::match_flow::SeatId;
        use regia_rules::teams::{PlayerLedger, TeamDef};

        let mut session = session::MatchSession::default();
        session.teams.defs = vec![
            TeamDef::unnamed(2, [64, 112, 196, 255], 16),
            TeamDef::unnamed(1, [196, 64, 64, 255], 16),
        ];
        // Row ids are the ledger rows' own ids (ADR-8657); each row gets a distinct one here so
        // the order below names the seats.
        let mut row = |team: u8, slot: u8, score: i64, kills: u32, id: u64| {
            let mut ledger = PlayerLedger::new(SeatId::new(team, slot));
            ledger.score = score;
            ledger.kills = kills;
            ledger.id = id;
            session.ledgers.rows.push(ledger);
        };
        row(1, 1, 5, 1, 11);
        row(2, 0, 5, 3, 20);
        row(1, 0, 5, 3, 10);
        row(1, 2, 1, 0, 12);
        let sheet = score_sheet(&session);
        assert_eq!(
            sheet.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![10, 20, 11, 12],
            "seat 1/0, then 2/0, then 1/1, then 1/2"
        );
        // A row decoded from a session saved before ids existed (id 0) keeps a seat-derived id in
        // its own range.
        let mut legacy = session::MatchSession::default();
        legacy.ledgers.rows.push(PlayerLedger::new(SeatId::new(1, 2)));
        assert_eq!(score_sheet(&legacy).rows[0].id, (1 << 63) | (1 << 8) | 2);
        assert_eq!(sheet.teams[0].id, 1);
        assert_eq!(sheet.teams[0].name, "");
        assert_eq!(
            sheet.teams[0].color,
            Some([
                f32::from(196u8) / 255.0,
                f32::from(64u8) / 255.0,
                f32::from(64u8) / 255.0,
                1.0,
            ])
        );
        assert!(
            sheet
                .rows
                .iter()
                .all(|row| matches!(row.values.first(), Some(ScoreCell::Int(_))))
        );
        assert_eq!(sheet.columns.len(), 5);
    }

    /// Review proof (rules-spawn-selection-review). An eliminated seat and a remembered spawn
    /// use live on [`MatchRules`]. A snapshot restore has to see both. `insert_resource_text`
    /// returns false while `MatchRules` is not a reflected resource.
    #[test]
    fn proof_review_respawn_book_and_spawn_state_join_a_world_snapshot() {
        use crate::state_hash::{insert_resource_text, world_snapshot};

        let mut app = App::new();
        app.add_plugins(MatchRulesPlugin);
        {
            let mut rules = app.world_mut().resource_mut::<MatchRules>();
            rules.session.respawn.default = spawn::RespawnPolicy::Never;
            rules
                .session
                .apply(&match_flow::FlowInput::Seat { team: 1, slot: 0 })
                .expect("seat");
            let seat = match_flow::SeatId::new(1, 0);
            rules.session.note_death(seat);
            rules
                .session
                .spawn
                .uses
                .push(spawn::SpawnUse { point: 7, tick: 3 });
            assert!(rules.session.respawns.is_eliminated(seat));
        }
        let world = app.world();
        let mut snapshot = world_snapshot(world);
        assert!(
            insert_resource_text(world, &mut snapshot, "MatchRules"),
            "MatchRules is not reflected: seat (1, 0) eliminated and spawn use of point 7 drop out of a snapshot"
        );
        let text = &snapshot["$resource/MatchRules"];
        assert!(
            text.contains("point: 7") || text.contains("point:7"),
            "snapshot text dropped the spawn use: {text}"
        );
    }

    /// Proof: a score-sheet row id is not reused after that seat leaves and someone sits there
    /// again, and a legal seat is not id 0. Fails at the reviewed head: the id is
    /// `(team << 8) | slot`, so team 1 slot 0 is 256 both times. Commit b2b9437af only moved the
    /// `| 0` out of this file's other test.
    #[test]
    fn proof_score_sheet_seat_id_is_not_reused_after_leave_and_rejoin() {
        use regia_rules::match_flow::FlowInput;

        let mut session = session::MatchSession::default();
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let first = score_sheet(&session).rows[0].id;
        assert_ne!(first, 0, "a seated row must not use id 0");
        session
            .apply(&FlowInput::Leave { team: 1, slot: 0 })
            .expect("leave");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("rejoin");
        let second = score_sheet(&session).rows[0].id;
        assert_ne!(
            first, second,
            "the next occupant of team 1 slot 0 reused score-sheet id {first}"
        );
    }

    /// A session with every kind of respawn row and moved spawn state: seat (1, 0) eliminated,
    /// seat (2, 0) waiting on a delay, seat (3, 0) holding a lives count, a recent spawn use, a
    /// round-robin cursor and a generator that has drawn once.
    fn session_with_respawn_and_spawn_state() -> MatchRules {
        use match_flow::{FlowInput, SeatId};
        use spawn::{RespawnPolicy, RespawnRules, SpawnKey, SpawnUse, TeamRespawn};

        let mut rules = MatchRules::default();
        rules.session.respawn = RespawnRules {
            default: RespawnPolicy::Never,
            teams: vec![
                TeamRespawn {
                    team: 2,
                    policy: RespawnPolicy::Delay { ticks: 40 },
                },
                TeamRespawn {
                    team: 3,
                    policy: RespawnPolicy::Lives { lives: 3 },
                },
            ],
            ..RespawnRules::default()
        };
        for team in 1..=3 {
            rules
                .session
                .apply(&FlowInput::Seat { team, slot: 0 })
                .expect("seat");
            rules.session.note_death(SeatId::new(team, 0));
        }
        rules.session.spawn.uses.push(SpawnUse { point: 7, tick: 3 });
        rules.session.spawn.last = Some(SpawnKey {
            priority: 0,
            order: 2,
            id: 7,
        });
        rules.session.spawn.rng.next_u64();
        // Undrained events are a per-step queue serde skips; the host drains them every step.
        rules.session.drain_events();
        rules
    }

    fn snapshot_with_rules(rules: MatchRules) -> crate::state_hash::StateSnapshot {
        use crate::state_hash::{insert_resource_text, world_snapshot};

        let mut app = App::new();
        app.add_plugins(MatchRulesPlugin);
        app.insert_resource(rules);
        let mut snapshot = world_snapshot(app.world());
        assert!(insert_resource_text(app.world(), &mut snapshot, "MatchRules"));
        snapshot
    }

    /// ADR-8704 (spawn-selection review hazard 3): `world_snapshot` plus
    /// `insert_resource_text("MatchRules")` covers the respawn book and the spawn picker's
    /// state. Equal sessions give equal snapshots; changing only the pending row, the lives, the
    /// eliminated list, the recent uses, the cursor or the generator changes the snapshot.
    #[test]
    fn a_world_snapshot_with_match_rules_covers_the_respawn_book_and_spawn_state() {
        use crate::state_hash::{differing_keys, snapshot_hash};

        let base = session_with_respawn_and_spawn_state();
        let reference = snapshot_with_rules(base.clone());
        assert_eq!(
            snapshot_hash(&reference),
            snapshot_hash(&snapshot_with_rules(base.clone())),
            "the same session must give the same snapshot"
        );
        let text = &reference["$resource/MatchRules"];
        for needle in ["respawns", "pending", "lives", "eliminated", "spawn", "rng"] {
            assert!(text.contains(needle), "snapshot text lacks `{needle}`: {text}");
        }
        /// One change to the session and what it names.
        type Variant = (&'static str, fn(&mut MatchRules));
        let variants: [Variant; 6] = [
            ("pending due", |r| r.session.respawns.pending[0].due += 1),
            ("lives", |r| r.session.respawns.lives[0].lives += 1),
            ("eliminated", |r| r.session.respawns.eliminated.clear()),
            ("recent uses", |r| r.session.spawn.uses[0].tick += 1),
            ("cursor", |r| r.session.spawn.last = None),
            ("generator", |r| {
                r.session.spawn.rng.next_u64();
            }),
        ];
        for (what, change) in variants {
            let mut changed = base.clone();
            change(&mut changed);
            let other = snapshot_with_rules(changed);
            assert_eq!(
                differing_keys(&reference, &other, 8),
                vec!["$resource/MatchRules".to_string()],
                "changing only the {what} must move the MatchRules entry and nothing else"
            );
        }
    }

    /// ADR-8704: the reflected `MatchRules` (its `ReflectSerialize`/`ReflectDeserialize` path, in
    /// JSON) round-trips the respawn book and the spawn state exactly.
    #[test]
    fn reflected_match_rules_round_trip_the_respawn_book_and_spawn_state() {
        use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
        use regia_ecs::reflect::{AppTypeRegistry, FromReflect};
        use serde::de::DeserializeSeed;

        let mut app = App::new();
        app.add_plugins(MatchRulesPlugin);
        let rules = session_with_respawn_and_spawn_state();
        assert!(
            rules
                .session
                .respawns
                .is_eliminated(match_flow::SeatId::new(1, 0))
        );
        assert_eq!(
            rules.session.respawns.due_of(match_flow::SeatId::new(2, 0)),
            Some(40)
        );
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        // JSON, the session's agent-protocol encoding: the session's internally tagged enums
        // (`#[serde(tag = ..)]`) do not deserialize from RON or postcard (no `deserialize_any`).
        let text = serde_json::to_string(&TypedReflectSerializer::new(&rules, &registry)).expect("serialize");
        let registration = registry
            .get(std::any::TypeId::of::<MatchRules>())
            .expect("MatchRulesPlugin registers MatchRules");
        let mut de = serde_json::Deserializer::from_str(&text);
        let value = TypedReflectDeserializer::new(registration, &registry)
            .deserialize(&mut de)
            .expect("deserialize");
        let back = MatchRules::from_reflect(value.as_partial_reflect()).expect("from_reflect");
        assert_eq!(back.session.respawns, rules.session.respawns);
        assert_eq!(back.session.spawn, rules.session.spawn);
        assert_eq!(back, rules);
    }
}
