// SPDX-License-Identifier: MPL-2.0
//! Players, controllers and the player manager (`docs/specs/14-scene-and-framework.md` § B3).
//!
//! A [`Player`] is an entity representing a human participant — local (same machine, split-screen)
//! or remote (Tier 2). The [`PlayerManager`] resource handles local join (press a button on a new
//! gamepad), enforces the maximum-local-players project setting, and computes the split-screen
//! viewport layout ([`SplitScreenLayout`]), which the client build writes onto each camera tagged
//! with a [`SplitScreenCamera`] (ADR-7880).
//!
//! # Controllers
//!
//! A [`PlayerController`] drives a character from a player's input: it owns the input context
//! stack, can [`PlayerController::possess`] / [`PlayerController::unpossess`] an entity, and holds
//! the camera view target. An [`AiController`] is the possession target for script-driven bots and
//! connectionless server bots (`net_bots`, with behaviour trees in `regia_ai`).
//!
//! # Spawning
//!
//! [`PlayerStart`] is a marker entity placed in levels; the default
//! [`GameRules`](crate::rules::GameRules) spawn logic picks the first one in entity order.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_platform::GamepadButton;

/// How the player joined.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default)]
pub enum PlayerKind {
    /// On this device (keyboard/mouse, gamepad, touch).
    #[default]
    Local,
    /// Over the network (Tier 2).
    Remote,
}

/// A human participant.
///
/// Every player is an entity with this component. The [`PlayerManager`] resource is the authority
/// on active players; query `Player` to get the per-entity data.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct Player {
    /// Unique index across all local+remote players (starts at 0, assigned by [`PlayerManager`]).
    pub index: u8,
    /// Local or remote.
    pub kind: PlayerKind,
    /// The [`PlayerController`] entity for this player.
    pub controller: Entity,
    /// The gamepad device id this player joined with, if any (drives the per-player device
    /// assignment in `regia_input`).
    pub device: Option<u64>,
}

impl Default for Player {
    fn default() -> Self {
        Self {
            index: 0,
            kind: PlayerKind::Local,
            controller: Entity::PLACEHOLDER,
            device: None,
        }
    }
}

impl Player {
    /// A player with the given index and kind. The controller field must be wired afterwards.
    pub fn new(index: u8, kind: PlayerKind) -> Self {
        Self {
            index,
            kind,
            controller: Entity::PLACEHOLDER,
            device: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Player manager
// ---------------------------------------------------------------------------

/// Maximum local players on one machine (project setting, defaults to 4).
///
/// [`PlayerPlugin`] copies this onto [`PlayerManager::max_local`] when the setting is first
/// seen and is not the default 4, or whenever the setting value changes after that. A manager
/// whose cap was written directly stays at that cap while this resource remains 4, so a lower
/// manager cap is not raised by the default. Raising only this resource above the manager's
/// current cap takes effect when the value changes (the first observation of a non-default
/// value counts as that change).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxLocalPlayers(pub u8);

/// The last [`MaxLocalPlayers`] value applied onto [`PlayerManager::max_local`].
#[derive(Resource, Debug, Default)]
struct AppliedMaxLocal(Option<u8>);

impl Default for MaxLocalPlayers {
    fn default() -> Self {
        Self(4)
    }
}

/// The split-screen layout for the current players. Layout data is produced by
/// [`PlayerManager::compute_viewports`] in `PostUpdate` and stored here. In the client build the
/// player plugin then writes each player's rect onto the `regia_render::Camera::viewport` of every
/// camera tagged with that player's [`SplitScreenCamera`] (ADR-7880); an untagged camera is left
/// to the game.
#[derive(Resource, Debug, Clone, Default)]
pub struct SplitScreenLayout {
    /// Per-player viewport normalised coordinates: `(x, y, w, h)` in [0, 1].
    pub viewports: BTreeMap<u8, (f32, f32, f32, f32)>,
}

impl SplitScreenLayout {
    /// The viewport of `player` as `regia_render::Camera::viewport` takes it (`[x, y, w, h]`):
    /// the player's rect, or the full target when the layout has none for it (a player who left,
    /// or a camera tagged before its player joined).
    #[must_use]
    pub fn camera_viewport(&self, player: u8) -> [f32; 4] {
        let (x, y, w, h) = self
            .viewports
            .get(&player)
            .copied()
            .unwrap_or((0.0, 0.0, 1.0, 1.0));
        [x, y, w, h]
    }
}

/// Marks a camera as the view of local player `player` (ADR-7880): in the client build the player
/// plugin writes that player's [`SplitScreenLayout`] rect onto the camera's
/// `regia_render::Camera::viewport` in `PostUpdate` whenever the two differ (a join, a leave). The
/// camera's draw order, target and everything else stay the game's.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct SplitScreenCamera {
    /// The [`Player::index`] this camera renders for.
    pub player: u8,
}

/// The player authority: join/leave, split-screen layout, max-local cap (§ B3).
///
/// Insert manually or through [`PlayerPlugin`]. The manager watches for a join button on
/// gamepads that no player owns yet and spawns a [`Player`] + [`PlayerController`] when one is
/// pressed; the device is then assigned to that player, so a second press on the same pad never
/// spawns a second player.
#[derive(Resource, Debug)]
pub struct PlayerManager {
    /// Active player indices → player entities.
    players: BTreeMap<u8, Entity>,
    /// Where the search for the next free player index starts (indices are reused only after
    /// the search wraps past 255, so a long-running server never runs out).
    next_index: u8,
    /// Maximum allowed local players.
    pub max_local: u8,
    /// Split-screen layout (updated each `PostUpdate` by [`Self::compute_viewports`]).
    pub layout: SplitScreenLayout,
    /// Gamepad device ids → the player index that joined with them.
    assigned_devices: BTreeMap<u64, u8>,
    /// Indices of local participants only; server bots do not consume the local split-screen
    /// cap. Kept here, not read back from the `Player` component, so a despawned or edited player
    /// entity cannot leak a local slot.
    local_indices: BTreeSet<u8>,
    /// One camera director per local player (spec 14 § B5, ADR-7203), created when the player
    /// joins and dropped when they leave.
    #[cfg(feature = "client")]
    pub(crate) directors: BTreeMap<u8, crate::camera::CameraDirector>,
}

impl PlayerManager {
    /// Creates a fresh manager with space for `max_local` players.
    pub fn new(max_local: u8) -> Self {
        Self {
            players: BTreeMap::new(),
            next_index: 0,
            max_local,
            layout: SplitScreenLayout::default(),
            assigned_devices: BTreeMap::new(),
            local_indices: BTreeSet::new(),
            #[cfg(feature = "client")]
            directors: BTreeMap::new(),
        }
    }

    #[cfg(feature = "client")]
    /// Local player `index`'s camera director: its blends move that player's
    /// [`PlayerCamera`](crate::camera::blend::PlayerCamera) cameras. `None` for a remote player or
    /// an index no local player holds.
    #[must_use]
    pub fn camera_director(&self, index: u8) -> Option<&crate::camera::CameraDirector> {
        self.directors.get(&index)
    }

    #[cfg(feature = "client")]
    /// Every local player's camera director, by player index (ascending).
    pub fn camera_directors(&self) -> impl Iterator<Item = (u8, &crate::camera::CameraDirector)> + '_ {
        self.directors.iter().map(|(index, director)| (*index, director))
    }

    #[cfg(feature = "client")]
    /// Mutable [`Self::camera_director`] (start a blend with
    /// [`CameraDirector::blend_to`](crate::camera::CameraDirector::blend_to)).
    pub fn camera_director_mut(&mut self, index: u8) -> Option<&mut crate::camera::CameraDirector> {
        self.directors.get_mut(&index)
    }

    /// How many players are currently active.
    pub fn count(&self) -> usize {
        self.players.len()
    }

    /// The entity for player `index`, if they are active.
    pub fn player_entity(&self, index: u8) -> Option<Entity> {
        self.players.get(&index).copied()
    }

    /// Every active (index, entity) pair.
    pub fn iter(&self) -> impl Iterator<Item = (u8, Entity)> + '_ {
        self.players.iter().map(|(k, v)| (*k, *v))
    }

    /// Whether another local player can join.
    pub fn can_join(&self) -> bool {
        self.local_indices.len() < self.max_local as usize
    }

    /// Whether `device` is already bound to a player.
    pub fn is_device_assigned(&self, device: u64) -> bool {
        self.assigned_devices.contains_key(&device)
    }

    /// The player index that owns `device`, if any.
    pub fn device_owner(&self, device: u64) -> Option<u8> {
        self.assigned_devices.get(&device).copied()
    }

    /// Binds `device` to `index`. Used by the join system; harmless if already bound to the same
    /// player.
    pub fn assign_device(&mut self, index: u8, device: u64) {
        self.assigned_devices.insert(device, index);
    }

    /// Spawns a player + controller, returns the new entities. Callers (the join system) must wire
    /// the controller to the player afterwards.
    ///
    /// Returns `None` when a [`PlayerKind::Local`] player would exceed [`Self::max_local`] (other
    /// kinds take no local seat), or when every player index is in use.
    pub fn spawn_player(&mut self, world: &mut World, kind: PlayerKind) -> Option<(Entity, Entity)> {
        self.spawn_player_inner(world, kind, true)
    }

    fn spawn_player_inner(
        &mut self,
        world: &mut World,
        kind: PlayerKind,
        enforce_local_cap: bool,
    ) -> Option<(Entity, Entity)> {
        if enforce_local_cap && kind == PlayerKind::Local && !self.can_join() {
            return None;
        }
        let index = self.free_index()?;
        self.next_index = index.wrapping_add(1);

        let controller = world
            .spawn((PlayerController::new(index), regia_scene::Transform::default()))
            .id();

        let player_entity = world
            .spawn((Player {
                index,
                kind,
                controller,
                device: None,
            },))
            .id();

        self.players.insert(index, player_entity);
        if kind == PlayerKind::Local {
            self.local_indices.insert(index);
            #[cfg(feature = "client")]
            self.directors
                .insert(index, crate::camera::CameraDirector::default());
        }
        Some((player_entity, controller))
    }

    /// The first index at or after `next_index` (wrapping) that no active player holds.
    fn free_index(&self) -> Option<u8> {
        (0..=u8::MAX)
            .map(|offset| self.next_index.wrapping_add(offset))
            .find(|index| !self.players.contains_key(index))
    }

    /// Static helper: spawns a player through the [`PlayerManager`] resource without requiring a
    /// separate borrow. Used by the local-join system where the manager is already borrowed.
    pub fn spawn_player_static(world: &mut World, kind: PlayerKind) -> Option<(Entity, Entity)> {
        apply_max_local_setting(world);
        let mut pm = world.remove_resource::<PlayerManager>()?;
        let result = pm.spawn_player(world, kind);
        world.insert_resource(pm);
        result
    }

    /// Spawns a server bot participant without consuming a local split-screen slot.
    ///
    /// The caller adds `net_bots::NetBot` to the player and its possession target. Bots use the
    /// remote player kind so [`PlayerKind`] remains exhaustive-compatible with existing games.
    pub fn spawn_bot_static(world: &mut World) -> Option<(Entity, Entity)> {
        let mut pm = world.remove_resource::<PlayerManager>()?;
        let result = pm.spawn_player_inner(world, PlayerKind::Remote, false);
        world.insert_resource(pm);
        result
    }

    /// Makes an existing entity a remote [`Player`] (spec 70 § 4): the session's `NetPlayer`
    /// entity of a joining peer. Takes the next free index (no local split-screen seat), inserts
    /// [`Player`] with [`PlayerKind::Remote`] on `entity` and spawns its [`PlayerController`].
    /// Returns the controller; `None` without a [`PlayerManager`], when `entity` is gone or
    /// already a player, or when every index is in use.
    pub fn adopt_remote_static(world: &mut World, entity: Entity) -> Option<Entity> {
        if world.get_entity(entity).is_err() || world.get::<Player>(entity).is_some() {
            return None;
        }
        let mut pm = world.remove_resource::<PlayerManager>()?;
        let index = pm.free_index();
        let controller = index.map(|index| {
            pm.next_index = index.wrapping_add(1);
            let controller = world
                .spawn((PlayerController::new(index), regia_scene::Transform::default()))
                .id();
            if let Ok(mut e) = world.get_entity_mut(entity) {
                e.insert(Player {
                    index,
                    kind: PlayerKind::Remote,
                    controller,
                    device: None,
                });
            }
            pm.players.insert(index, entity);
            controller
        });
        world.insert_resource(pm);
        controller
    }

    /// Frees a remote player's slot (spec 70 § 4): drops its index from the manager and despawns
    /// its controller. The player entity itself and the game rules are the session's business (it
    /// calls `GameRules::on_player_left` and despawns the `NetPlayer` entity), so neither is
    /// touched here. Returns whether `entity` was an active player.
    pub fn release_remote_static(world: &mut World, entity: Entity) -> bool {
        let Some(player) = world.get::<Player>(entity).cloned() else {
            return false;
        };
        let Some(mut pm) = world.remove_resource::<PlayerManager>() else {
            return false;
        };
        let known = pm.players.get(&player.index) == Some(&entity);
        if known {
            pm.players.remove(&player.index);
            pm.assigned_devices.retain(|_, owner| *owner != player.index);
            pm.local_indices.remove(&player.index);
        }
        world.insert_resource(pm);
        if known && world.get_entity(player.controller).is_ok() {
            world.despawn(player.controller);
        }
        known
    }

    /// Static helper: removes a player through the [`PlayerManager`] resource without requiring a
    /// separate borrow.
    pub fn remove_player_static(world: &mut World, index: u8) -> bool {
        let Some(mut pm) = world.remove_resource::<PlayerManager>() else {
            return false;
        };
        let result = pm.remove_player(world, index);
        world.insert_resource(pm);
        result
    }

    /// Removes a player and their controller, freeing their device and notifying
    /// [`GameRules::on_player_left`](crate::rules::GameRules::on_player_left). Returns whether
    /// the player existed.
    pub fn remove_player(&mut self, world: &mut World, index: u8) -> bool {
        let Some(player_entity) = self.players.remove(&index) else {
            return false;
        };
        self.assigned_devices.retain(|_, owner| *owner != index);
        self.local_indices.remove(&index);
        #[cfg(feature = "client")]
        self.directors.remove(&index);

        // Notify the game rules (the resource may be absent in bare tests).
        if world.get_resource::<crate::rules::GameRulesResource>().is_some() {
            let mut rules = world
                .remove_resource::<crate::rules::GameRulesResource>()
                .unwrap_or_default();
            let mut ctx = crate::rules::RulesCtx::new(world);
            rules.rules.on_player_left(&mut ctx, player_entity);
            world.insert_resource(rules);
        }

        if let Some(player) = world.get::<Player>(player_entity) {
            let controller = player.controller;
            world.despawn(controller);
        }
        world.despawn(player_entity);
        true
    }

    /// Computes split-screen viewports for `n` active players. Writes the result into
    /// [`PlayerManager::layout`].
    ///
    /// Layouts:
    /// - 1 player: full screen (0, 0, 1, 1)
    /// - 2 players: horizontal split (left/right halves)
    /// - 3 players: one full-width top half, two bottom quarters
    /// - 4 players: four equal quarters
    /// - >4: grid (incomplete last row is left-aligned)
    pub fn compute_viewports(&mut self) {
        let indices: Vec<u8> = self.players.keys().copied().collect();
        self.compute_viewports_for(&indices);
    }

    fn compute_viewports_for(&mut self, indices: &[u8]) {
        let n = indices.len();
        if n == 0 {
            self.layout.viewports.clear();
            return;
        }
        // Rects per slot, in join order; slots are then assigned to the real player indices so a
        // survivor of a leave (index 1 of {1}) still gets a rect under its own key.
        let slots: Vec<(f32, f32, f32, f32)> = match n {
            1 => vec![(0.0, 0.0, 1.0, 1.0)],
            2 => vec![(0.0, 0.0, 0.5, 1.0), (0.5, 0.0, 0.5, 1.0)],
            3 => vec![(0.0, 0.0, 1.0, 0.5), (0.0, 0.5, 0.5, 0.5), (0.5, 0.5, 0.5, 0.5)],
            4 => vec![
                (0.0, 0.0, 0.5, 0.5),
                (0.5, 0.0, 0.5, 0.5),
                (0.0, 0.5, 0.5, 0.5),
                (0.5, 0.5, 0.5, 0.5),
            ],
            _ => {
                // Grid: ceil(sqrt(n)) columns, fill left-to-right, top-to-bottom.
                let cols = (n as f32).sqrt().ceil() as u32;
                let col_w = 1.0 / cols as f32;
                let rows = (n as u32).div_ceil(cols);
                let row_h = 1.0 / rows as f32;
                (0..n as u32)
                    .map(|i| {
                        let col = i % cols;
                        let row = i / cols;
                        (col as f32 * col_w, row as f32 * row_h, col_w, row_h)
                    })
                    .collect()
            }
        };
        let viewports: BTreeMap<_, _> = indices.iter().copied().zip(slots).collect();
        self.layout.viewports = viewports;
    }
}

impl Default for PlayerManager {
    fn default() -> Self {
        Self::new(4)
    }
}

// ---------------------------------------------------------------------------
// Player controller
// ---------------------------------------------------------------------------

/// The default input context name for gameplay — a player controller's implicit bottom stack entry.
pub const GAMEPLAY_CONTEXT: &str = "Gameplay";

/// Controller driven by player input (§ B3).
///
/// Owns the input context stack for its player, can possess/unpossess a
/// [`Character`](crate::character::Character) entity,
/// and holds the camera view target. Spawn one per player.
///
/// # Input context
///
/// The controller pushes/pops context names (strings matching contexts in
/// `regia_input::InputConfig`). The topmost context is the active one. The default bottom
/// entry is [`GAMEPLAY_CONTEXT`].
///
/// # Example
///
/// ```
/// use regia_app::prelude::Entity;
/// use regia_framework::player::PlayerController;
///
/// let mut pc = PlayerController::new(0);
/// assert_eq!(pc.player_index, 0);
/// assert_eq!(pc.active_context(), "Gameplay");
///
/// pc.push_context("UI");
/// assert_eq!(pc.active_context(), "UI");
///
/// let pawn = Entity::PLACEHOLDER;
/// pc.possess(pawn);
/// assert_eq!(pc.possessed, pawn);
/// assert_eq!(pc.view_target, pawn);
/// ```
#[derive(Component, Reflect, Debug, Clone)]
#[reflect(Component, Default)]
pub struct PlayerController {
    /// Which local player this controller belongs to.
    pub player_index: u8,
    /// The entity this controller currently possesses (a [`Character`](crate::character::Character)
    /// or other pawn), or [`Entity::PLACEHOLDER`] when unpossessed.
    pub possessed: Entity,
    /// The camera view target — the entity the camera should follow (often the possessed entity,
    /// but can be different for cinematic cameras).
    pub view_target: Entity,
    /// The input context stack as string names; the last element is the active context.
    pub context_stack: Vec<String>,
}

impl Default for PlayerController {
    fn default() -> Self {
        Self {
            player_index: 0,
            possessed: Entity::PLACEHOLDER,
            view_target: Entity::PLACEHOLDER,
            context_stack: vec![GAMEPLAY_CONTEXT.to_string()],
        }
    }
}

impl PlayerController {
    /// A fresh controller for local player `index`, with the default gameplay context.
    pub fn new(player_index: u8) -> Self {
        Self {
            player_index,
            possessed: Entity::PLACEHOLDER,
            view_target: Entity::PLACEHOLDER,
            context_stack: vec![GAMEPLAY_CONTEXT.to_string()],
        }
    }

    /// The currently active input context (top of stack).
    pub fn active_context(&self) -> &str {
        self.context_stack
            .last()
            .map(String::as_str)
            .unwrap_or(GAMEPLAY_CONTEXT)
    }

    /// Pushes an input context name onto the stack.
    pub fn push_context(&mut self, context: &str) {
        self.context_stack.push(context.to_string());
    }

    /// Pops the topmost context; never removes the last one (the implicit gameplay context).
    pub fn pop_context(&mut self) {
        if self.context_stack.len() > 1 {
            self.context_stack.pop();
        }
    }

    /// Possesses `entity` (the [`Character`](crate::character::Character) the controller will
    /// drive). The previous possessed
    /// entity, if any, is unpossessed first.
    pub fn possess(&mut self, entity: Entity) {
        self.possessed = entity;
        self.view_target = entity;
    }

    /// Unpossesses the current entity, if any.
    pub fn unpossess(&mut self) {
        self.possessed = Entity::PLACEHOLDER;
    }
}

// ---------------------------------------------------------------------------
// AI controller
// ---------------------------------------------------------------------------

/// An AI-driven controller: scripts or `net_bots` drive the possessed
/// [`Character`](crate::character::Character).
///
/// This marker is also the hook for [`GameRules::on_character_died`](crate::rules::GameRules::on_character_died)
/// to know it's a bot. `regia_ai::net_bots` supplies behaviour trees, perception and navigation for
/// connectionless server bots.
///
/// Note: `Default` on `AiController` yields placeholder entities for `possessed` and `view_target`;
/// use [`AiController::new`] for a concrete assignment.
#[derive(Component, Reflect, Debug, Clone)]
#[reflect(Component, Default)]
pub struct AiController {
    /// The entity this AI possesses.
    pub possessed: Entity,
    /// The view target for networked relevance or cinematic tracking.
    pub view_target: Entity,
}

impl Default for AiController {
    fn default() -> Self {
        Self {
            possessed: Entity::PLACEHOLDER,
            view_target: Entity::PLACEHOLDER,
        }
    }
}

impl AiController {
    /// A controller possessing `entity`.
    pub fn new(possessed: Entity) -> Self {
        Self {
            possessed,
            view_target: possessed,
        }
    }
}

// ---------------------------------------------------------------------------
// Player start
// ---------------------------------------------------------------------------

/// A spawn-point marker placed in levels. The default spawn logic picks the first [`PlayerStart`]
/// in entity order. A [`SpawnPoint`] beside it adds a team side, tags, a priority and an enabled
/// flag, which the session's spawn policy reads ([`crate::spawn_select::choose_spawn`], ADR-8647).
///
/// Place one or more in every level; the transform is the spawn location. Optional [`Name`] (e.g.
/// "Red Team North") helps the editor.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct PlayerStart;

/// Selection data of a [`PlayerStart`] (ADR-8647): which side may use it, tags a spawn ask can
/// require, a priority (higher is walked first) and whether it is enabled.
///
/// A start without one behaves as `SpawnPoint::default()`: side 0 (every team), no tags, priority
/// 0, enabled. The rules read it through [`crate::spawn_select::choose_spawn`]; the agent protocol's
/// `rules.spawn_preview` lists it (`book/agents/runtime-gameplay.md`).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct SpawnPoint {
    /// The side that spawns here; `0` serves every team. After a side swap a team spawns on the
    /// side it plays on now.
    pub team: u8,
    /// Tags a spawn ask can require (`"attack"`, `"vehicle"`).
    pub tags: Vec<String>,
    /// Higher priority points are walked first (and add to a scored policy's total).
    pub priority: i16,
    /// A disabled point is never picked.
    pub enabled: bool,
}

impl Default for SpawnPoint {
    fn default() -> Self {
        Self {
            team: 0,
            tags: Vec::new(),
            priority: 0,
            enabled: true,
        }
    }
}

impl SpawnPoint {
    /// An enabled point for `team`'s side with no tags.
    pub fn for_team(team: u8) -> Self {
        Self {
            team,
            ..Self::default()
        }
    }
}

/// Takes a [`PlayerStart`] out of spawn selection while present (a logic graph's
/// `rules::SetSpawnEnabled` adds and removes it). `crate::rules::default_spawn_point` skips a
/// disabled start; when every start is disabled it falls back to all of them rather than the world
/// origin. Saved with the level like any reflected component.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct PlayerStartDisabled;

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Registers the player types and runs per-frame systems: local-join detection, viewport layout.
///
/// It also installs `regia_ecs::spawn_order::SpawnOrder<PlayerStart>` tracking, so spawn
/// selection walks [`PlayerStart`]s in arrival order (ADR-8704). Add it before anything spawns a
/// `PlayerStart`: tracking cannot start once one exists, and the starts then fall back to position
/// order ([`crate::spawn_select::StartKey`]).
pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Player>()
            .register_type::<PlayerKind>()
            .register_type::<PlayerController>()
            .register_type::<AiController>()
            .register_type::<PlayerStart>()
            .register_type::<SpawnPoint>()
            .register_type::<crate::spawn_select::SpawnShape>()
            .register_type::<PlayerStartDisabled>()
            .register_type::<SplitScreenCamera>()
            .init_resource::<MaxLocalPlayers>()
            .init_resource::<PlayerManager>()
            .add_systems(Update, (detect_local_join, adopt_streaming_sources))
            .add_systems(PostUpdate, update_viewports);
        // Spawn selection walks starts in arrival order (ADR-8704). The hooks can only go on
        // before the first `PlayerStart` exists; when they cannot, starts fall back to position
        // order (`crate::spawn_select::StartKey`), which is the same on every peer, so a late add
        // (the editor spawned starts before a game registered this plugin) only warns.
        if let Err(err) = regia_ecs::spawn_order::try_track_spawn_order::<PlayerStart>(app.world_mut()) {
            tracing::warn!(
                target: "regia_framework",
                ?err,
                "PlayerStart arrival order is not tracked; spawn selection uses position order"
            );
        }
        #[cfg(feature = "client")]
        app.add_systems(PostUpdate, apply_split_screen_viewports.after(update_viewports));
    }
}

/// `PostUpdate`, after the layout: writes each tagged camera's player rect onto its viewport when
/// they differ (ADR-7880), so a camera is marked changed only on a join or a leave.
#[cfg(feature = "client")]
fn apply_split_screen_viewports(
    manager: Option<Res<PlayerManager>>,
    mut cameras: Query<(&SplitScreenCamera, &mut regia_render::Camera)>,
) {
    let Some(manager) = manager else {
        return;
    };
    for (tag, mut camera) in &mut cameras {
        let viewport = manager.layout.camera_viewport(tag.player);
        if camera.viewport != viewport {
            camera.viewport = viewport;
        }
    }
}

/// In a streamed level every possessed pawn pulls the world in around it (spec 78 § 7.1).
fn adopt_streaming_sources(
    settings: Option<Res<regia_scene::streaming::LevelStreaming>>,
    controllers: Query<&PlayerController>,
    sources: Query<(), With<regia_scene::streaming::StreamingSource>>,
    mut commands: Commands,
) {
    if !settings.is_some_and(|s| s.enabled) {
        return;
    }
    for controller in &controllers {
        let pawn = controller.possessed;
        if pawn != Entity::PLACEHOLDER && !sources.contains(pawn) {
            commands
                .entity(pawn)
                .try_insert(regia_scene::streaming::StreamingSource {
                    kind: regia_scene::streaming::SourceKind::Player,
                    ..Default::default()
                });
        }
    }
}

/// Copies [`MaxLocalPlayers`] onto the manager. See that resource for when a direct
/// `max_local` write is left alone.
fn apply_max_local_setting(world: &mut World) {
    let Some(setting) = world.get_resource::<MaxLocalPlayers>().map(|max| max.0) else {
        return;
    };
    let manager_cap = world.get_resource::<PlayerManager>().map(|pm| pm.max_local);
    let seen = world
        .get_resource::<AppliedMaxLocal>()
        .and_then(|applied| applied.0);
    let apply = match seen {
        Some(previous) => previous != setting,
        None => {
            setting != MaxLocalPlayers::default().0
                || manager_cap.is_none_or(|cap| cap == MaxLocalPlayers::default().0)
        }
    };
    if apply && let Some(mut manager) = world.get_resource_mut::<PlayerManager>() {
        manager.max_local = setting;
    }
    world.insert_resource(AppliedMaxLocal(Some(setting)));
}

/// Watches for a join-button press on a gamepad no player owns yet and spawns a new local
/// player ([`Player`], [`PlayerController`], a character at the rules' spawn point, possession).
#[tracing::instrument(skip_all, target = "regia_framework", name = "local_join")]
fn detect_local_join(world: &mut World) {
    apply_max_local_setting(world);
    // Only run if we can accept more players.
    if !world
        .get_resource::<PlayerManager>()
        .is_some_and(|pm| pm.can_join())
    {
        return;
    }
    let Some(gamepads) = world.get_resource::<regia_input::Gamepads>() else {
        return;
    };

    // The first unassigned gamepad whose join button (South / Start) was just pressed joins.
    let join_device = {
        let pm = world.resource::<PlayerManager>();
        gamepads.pads().find_map(|(device, pad)| {
            let join_pressed =
                pad.just_pressed(GamepadButton::South) || pad.just_pressed(GamepadButton::Start);
            (join_pressed && !pm.is_device_assigned(device)).then_some(device)
        })
    };
    let Some(device) = join_device else {
        return;
    };

    // Spawn the player + controller (the cap may have changed under us — bail out gracefully).
    let Some((player_entity, controller)) = PlayerManager::spawn_player_static(world, PlayerKind::Local)
    else {
        return;
    };

    // Record the pad as this player's device, both here and in the input system's per-player
    // device assignment (specs/11 § 7).
    if let Some(player) = world.get::<Player>(player_entity) {
        let index = player.index;
        let mut pm = world.remove_resource::<PlayerManager>().unwrap_or_default();
        pm.assign_device(index, device);
        world.insert_resource(pm);
        if let Some(mut player) = world.get_mut::<Player>(player_entity) {
            player.device = Some(device);
        }
        if let Some(mut assignments) = world.get_resource_mut::<regia_input::PlayerInputs>() {
            assignments
                .players
                .insert(index, regia_input::DeviceAssignment::Gamepad(device));
        }
    }

    tracing::info!(
        target: "regia_framework",
        "local player joined on gamepad {device}: entity {}",
        player_entity.to_bits()
    );

    // Notify the game rules and spawn + possess what they ask for.
    let mut rules = world
        .remove_resource::<crate::rules::GameRulesResource>()
        .unwrap_or_default();
    let spawn = {
        let mut ctx = crate::rules::RulesCtx::new(world);
        rules.rules.on_player_joined(&mut ctx, player_entity)
    };
    world.insert_resource(rules);

    if let Some(req) = spawn {
        let character = spawn_pawn(world, &req);
        // `MatchSessionRules` seated this player: the pawn wears that seat (ADR-8723).
        crate::rules::claim_pawn_seat(world, player_entity, character);
        if let Some(mut pc) = world.get_mut::<PlayerController>(controller) {
            pc.possess(character);
        }
    }
}

/// Spawns the requested prefab's authored root, or the default character if it cannot load.
/// The request overrides the character's transform and optional team; authored character
/// components are retained. Local and remote joins use this same path.
pub(crate) fn spawn_pawn(world: &mut World, req: &crate::rules::SpawnRequest) -> Entity {
    let _span = tracing::info_span!(target: "regia_framework", "spawn_character").entered();
    let prefab = req.prefab.as_ref().and_then(|prefab| {
        match spawn_character_prefab(world, prefab) {
            Ok(entity) => Some(entity),
            Err(error) => {
                tracing::warn!(
                    target: "regia_framework",
                    %error,
                    "spawn request names prefab `{prefab}`; spawning the default character because the prefab could not be instantiated"
                );
                None
            }
        }
    });
    let entity = prefab.unwrap_or_else(|| world.spawn_empty().id());
    let mut pawn = world.entity_mut(entity);
    pawn.insert(req.transform);
    // Keep a prefab's loadout and movement settings; only supply absent character components.
    if !pawn.contains::<crate::character::Character>() {
        pawn.insert(crate::character::Character);
    }
    if !pawn.contains::<crate::character::CharacterMovement>() {
        pawn.insert(crate::character::CharacterMovement::default());
    }
    if !pawn.contains::<crate::character::CharacterInput>() {
        pawn.insert(crate::character::CharacterInput::default());
    }
    if let Some(team) = req.team {
        pawn.insert(crate::sim_bridge::Team(u32::from(team)));
    } else {
        pawn.remove::<crate::sim_bridge::Team>();
    }
    entity
}

/// Uses the same baked templates and instantiation as levels. The wrapper remains at identity:
/// controllers and movement address the authored root underneath it, not an empty wrapper.
fn spawn_character_prefab(world: &mut World, reference: &str) -> Result<Entity, String> {
    use regia_core::id::Guid;
    use regia_scene::components::EntityGuid;
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef};
    use regia_scene::prefab::{PrefabLibrary, instantiate_prefab};
    use regia_scene::spawn::SpawnOptions;

    let asset = character_prefab_asset(world, reference)?;
    let library = world.get_resource::<PrefabLibrary>().ok_or("no prefab library")?;
    let resolved = library.resolve(asset).map_err(|error| error.to_string())?;
    let local_root = resolved
        .template
        .entities
        .first()
        .ok_or("prefab has no root")?
        .local_id;
    if world
        .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
        .is_none()
    {
        return Err("no scene type registry".into());
    }
    let wrapper = world
        .spawn((EntityGuid(Guid::new_random()), regia_scene::Transform::default()))
        .id();
    let result = instantiate_prefab(
        world,
        &PrefabInstanceData::plain(SceneAssetRef(asset)),
        wrapper,
        &SpawnOptions::default(),
        0,
    )
    .map_err(|error| error.to_string())
    .and_then(|instance| {
        instance
            .locals
            .get(&local_root)
            .copied()
            .ok_or_else(|| "prefab root was removed".into())
    });
    match result {
        Ok(root) => {
            world.entity_mut(root).insert(PrefabPawnWrapper(wrapper));
        }
        Err(_) => {
            regia_scene::hierarchy::despawn_tree(world, wrapper);
        }
    }
    result
}

/// On a prefab pawn's authored root: the identity wrapper `spawn_character_prefab` hung it under,
/// which carries the `PrefabInstance`. Despawning the root (a death, a respawn, a leave the game
/// handles) despawns the wrapper's tree too, so each cleaned-up prefab join leaves no empty
/// instance behind for a scene save to keep (ADR-7979).
#[derive(Component, Debug, Clone, Copy)]
#[component(on_despawn = prefab_pawn_despawned)]
struct PrefabPawnWrapper(Entity);

/// Despawns the wrapper of a despawned prefab pawn after the despawn in progress (a no-op when the
/// wrapper's own tree is what is being despawned).
fn prefab_pawn_despawned(
    mut world: regia_ecs::ecs::world::DeferredWorld<'_>,
    context: regia_ecs::ecs::lifecycle::HookContext,
) {
    let Some(&PrefabPawnWrapper(wrapper)) = world.get::<PrefabPawnWrapper>(context.entity) else {
        return;
    };
    world.commands().queue(move |world: &mut World| {
        regia_scene::hierarchy::despawn_tree(world, wrapper);
    });
}

/// Resolves registered asset ids, or bakes a project-relative source prefab once into the library.
fn character_prefab_asset(world: &mut World, reference: &str) -> Result<regia_core::id::Guid, String> {
    use regia_asset::{AssetId, AssetServer};
    use regia_scene::format::{SceneAssetRef, SceneFile};
    use regia_scene::prefab::{PrefabLibrary, PrefabTemplate};

    if let Ok(asset) = reference.parse::<SceneAssetRef>() {
        return Ok(asset.guid());
    }
    let server = world.get_resource::<AssetServer>();
    let root = server.map_or_else(regia_asset::detect_project_root, |server| {
        server.project_root().to_path_buf()
    });
    let path = root.join(reference);
    let asset = if let Some(id) = server.and_then(|server| server.id_for_path(reference)) {
        id.0
    } else {
        match std::fs::read(regia_asset::meta_path_for(&path)) {
            Ok(bytes) => {
                regia_asset::MetaSidecar::from_ron(&bytes)
                    .map_err(|error| error.to_string())?
                    .id
                    .0
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                AssetId::sub_asset(AssetId::NIL, &path.to_string_lossy()).0
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    if world
        .get_resource::<PrefabLibrary>()
        .is_some_and(|library| library.contains(asset))
    {
        return Ok(asset);
    }
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file = SceneFile::from_ron_str(&text).map_err(|error| error.to_string())?;
    let registry = world
        .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
        .ok_or("no scene type registry")?;
    let template =
        PrefabTemplate::from_scene_file(asset, &file, &registry.read()).map_err(|error| error.to_string())?;
    world.init_resource::<PrefabLibrary>();
    world.resource_mut::<PrefabLibrary>().insert(template);
    Ok(asset)
}

/// Recomputes viewport layout in `PostUpdate` when the player count changes.
fn update_viewports(world: &mut World) {
    let locals: Vec<u8> = world
        .get_resource::<PlayerManager>()
        .map(|pm| {
            pm.players
                .keys()
                .filter(|index| {
                    pm.player_entity(**index).is_some_and(|entity| {
                        world
                            .get::<Player>(entity)
                            .is_some_and(|player| player.kind == PlayerKind::Local)
                    })
                })
                .copied()
                .collect()
        })
        .unwrap_or_default();
    // Recompute when the local set differs from the laid-out set. Connectionless server bots
    // possess pawns but have no local viewport.
    let stale = world.get_resource::<PlayerManager>().is_some_and(|pm| {
        pm.layout.viewports.len() != locals.len() || !pm.layout.viewports.keys().eq(locals.iter())
    });
    if stale {
        world
            .resource_mut::<PlayerManager>()
            .compute_viewports_for(&locals);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_indices_are_reused_after_255_joins_and_leaves() {
        // A long-running server adds and removes bots all the time; the u8 index must not run out.
        let mut world = World::new();
        world.insert_resource(PlayerManager::new(4));
        let mut seen_255 = false;
        for round in 0..600 {
            let (player, _) = PlayerManager::spawn_bot_static(&mut world)
                .unwrap_or_else(|| panic!("bot {round} got no player slot"));
            let index = world.get::<Player>(player).map(|p| p.index).expect("player");
            seen_255 |= index == u8::MAX;
            assert!(PlayerManager::remove_player_static(&mut world, index));
        }
        assert!(seen_255, "the search wraps through every index");
        assert_eq!(world.resource::<PlayerManager>().count(), 0);
        assert!(PlayerManager::spawn_player_static(&mut world, PlayerKind::Local).is_some());
    }

    #[test]
    fn a_wrapped_index_search_skips_indices_still_in_use() {
        let mut world = World::new();
        world.insert_resource(PlayerManager::new(4));
        let (keep, _) = PlayerManager::spawn_bot_static(&mut world).expect("bot 0");
        let keep_index = world.get::<Player>(keep).map(|p| p.index).expect("player");
        assert_eq!(keep_index, 0);
        for _ in 0..300 {
            let (player, _) = PlayerManager::spawn_bot_static(&mut world).expect("bot");
            let index = world.get::<Player>(player).map(|p| p.index).expect("player");
            assert_ne!(index, keep_index, "index 0 is still active");
            assert!(PlayerManager::remove_player_static(&mut world, index));
        }
        assert_eq!(world.resource::<PlayerManager>().player_entity(0), Some(keep));
    }

    #[test]
    fn a_remote_player_takes_no_local_seat() {
        let mut world = World::new();
        world.insert_resource(PlayerManager::new(1));
        assert!(PlayerManager::spawn_player_static(&mut world, PlayerKind::Local).is_some());
        assert!(PlayerManager::spawn_player_static(&mut world, PlayerKind::Local).is_none());
        assert!(
            PlayerManager::spawn_player_static(&mut world, PlayerKind::Remote).is_some(),
            "max_local caps local seats only"
        );
        assert_eq!(world.resource::<PlayerManager>().count(), 2);
    }

    #[test]
    fn a_despawned_local_player_entity_does_not_leak_its_slot() {
        let mut world = World::new();
        world.insert_resource(PlayerManager::new(1));
        let (player, _) = PlayerManager::spawn_player_static(&mut world, PlayerKind::Local).expect("p0");
        world.despawn(player);
        assert!(PlayerManager::remove_player_static(&mut world, 0));
        assert!(
            world.resource::<PlayerManager>().can_join(),
            "the local seat is free again"
        );
    }

    fn prefab_app() -> (App, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir("player-spawn-prefab");
        std::fs::create_dir_all(root.join("actors")).expect("fixture directory");
        let path = root.join("actors/hero.prefab.ron");
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, include_str!("../tests/fixtures/spawn_character.prefab.ron"))
            .expect("fixture write");
        std::fs::rename(tmp, path).expect("fixture publish");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin)
            .register_type::<crate::character::CharacterInput>()
            .register_type::<crate::sim_bridge::Team>()
            .insert_resource(
                regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                    project_root: root.clone(),
                    platform: "linux".into(),
                    content_dir: None,
                })
                .expect("asset server"),
            );
        (app, root)
    }

    #[test]
    fn a_spawn_request_instantiates_the_named_prefab() {
        use regia_core::math::Vec3;
        use regia_scene::{
            Transform,
            components::Name,
            prefab::{PrefabInstance, PrefabLink},
        };

        let (mut app, root) = prefab_app();
        let req = crate::rules::SpawnRequest {
            prefab: Some("actors/hero.prefab.ron".into()),
            transform: Transform::from_translation(Vec3::new(3.0, 4.0, 5.0)),
            team: Some(2),
        };
        let world = app.world_mut();
        let pawn = spawn_pawn(world, &req);
        assert_eq!(
            world.get::<Name>(pawn).map(Name::as_str),
            Some("FixtureCharacter")
        );
        assert_eq!(world.get::<Transform>(pawn), Some(&req.transform));
        assert_eq!(
            world.get::<crate::sim_bridge::Team>(pawn),
            Some(&crate::sim_bridge::Team(2))
        );
        assert!(
            world
                .get::<crate::character::CharacterInput>(pawn)
                .expect("authored input")
                .crouch
        );
        assert!(world.get::<PrefabLink>(pawn).is_some());
        assert_eq!(world.query::<&PrefabInstance>().iter(world).count(), 1);
        let accessory = world
            .query::<(Entity, &Name)>()
            .iter(world)
            .find_map(|(entity, name)| (name.as_str() == "FixtureAccessory").then_some(entity))
            .expect("prefab child spawned");
        assert_eq!(world.get::<ChildOf>(accessory).map(ChildOf::parent), Some(pawn));
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn a_registered_spawn_prefab_can_be_reused_without_source_files() {
        use regia_scene::{
            components::{EntityGuid, Name},
            prefab::PrefabInstance,
        };
        let (mut app, root) = prefab_app();
        let world = app.world_mut();
        let first = spawn_pawn(
            world,
            &crate::rules::SpawnRequest {
                prefab: Some("actors/hero.prefab.ron".into()),
                ..Default::default()
            },
        );
        let asset = world
            .query::<&PrefabInstance>()
            .single(world)
            .expect("instance")
            .prefab_guid();
        std::fs::remove_dir_all(root).expect("remove source fixture");
        let second = spawn_pawn(
            world,
            &crate::rules::SpawnRequest {
                prefab: Some(format!("asset:{asset}")),
                ..Default::default()
            },
        );
        assert_eq!(
            world.get::<Name>(second).map(Name::as_str),
            Some("FixtureCharacter")
        );
        assert_ne!(world.get::<EntityGuid>(first), world.get::<EntityGuid>(second));
        assert!(
            world.get::<crate::sim_bridge::Team>(first).is_none(),
            "None is free-for-all"
        );
        assert!(
            world.get::<crate::sim_bridge::Team>(second).is_none(),
            "None overrides authored team"
        );
        assert_eq!(world.query::<&PrefabInstance>().iter(world).count(), 2);
    }

    #[test]
    fn a_spawn_prefab_keeps_authored_character_movement() {
        use regia_core::id::Guid;
        use regia_scene::{
            format::SceneFile,
            prefab::{BakedValue, PrefabLibrary, PrefabTemplate},
        };
        let (mut app, root) = prefab_app();
        app.register_type::<crate::character::CharacterMovement>();
        let asset = Guid::from_u128(0xa901);
        let file = SceneFile::from_ron_str(include_str!("../tests/fixtures/spawn_character.prefab.ron"))
            .expect("fixture scene");
        let registry = app.world().resource::<regia_ecs::reflect::AppTypeRegistry>();
        let mut template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        let movement = crate::character::CharacterMovement {
            max_walk_speed: 12.0,
            max_jumps: 3,
            ..Default::default()
        };
        template.entities[0].components.push((
            "regia_framework::character::CharacterMovement".into(),
            BakedValue::Reflected(Box::new(movement.clone())),
        ));
        let world = app.world_mut();
        world.resource_mut::<PrefabLibrary>().insert(template);
        let pawn = spawn_pawn(
            world,
            &crate::rules::SpawnRequest {
                prefab: Some(format!("asset:{asset}")),
                ..Default::default()
            },
        );
        assert_eq!(
            world.get::<crate::character::CharacterMovement>(pawn),
            Some(&movement)
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn an_unloaded_nested_spawn_prefab_leaves_no_partial_instance() {
        use regia_core::id::Guid;
        use regia_scene::{
            format::{PrefabInstanceData, SceneAssetRef, SceneFile},
            prefab::{PendingPrefab, PrefabLibrary, PrefabTemplate},
        };
        let (mut app, root) = prefab_app();
        let asset = Guid::from_u128(0xa901);
        let mut file = SceneFile::from_ron_str(include_str!("../tests/fixtures/spawn_character.prefab.ron"))
            .expect("fixture scene");
        file.entities[0].prefab_instance =
            Some(PrefabInstanceData::plain(SceneAssetRef(Guid::from_u128(0xa902))));
        let registry = app.world().resource::<regia_ecs::reflect::AppTypeRegistry>();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        let world = app.world_mut();
        world.resource_mut::<PrefabLibrary>().insert(template);
        // The app already owns bookkeeping entities; spawning must preserve them.
        let mut expected: std::collections::BTreeSet<Entity> = world.query::<Entity>().iter(world).collect();
        let pawn = spawn_pawn(
            world,
            &crate::rules::SpawnRequest {
                prefab: Some(format!("asset:{asset}")),
                ..Default::default()
            },
        );
        assert!(world.get::<crate::character::Character>(pawn).is_some());
        expected.insert(pawn);
        assert_eq!(
            world
                .query::<Entity>()
                .iter(world)
                .collect::<std::collections::BTreeSet<_>>(),
            expected,
            "only the fallback pawn is added to the existing app entities"
        );
        assert_eq!(world.query::<&PendingPrefab>().iter(world).count(), 0);
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn a_malformed_spawn_prefab_falls_back_without_leaking_entities() {
        let (mut app, root) = prefab_app();
        regia_test_utils::write_atomic(&root.join("actors/hero.prefab.ron"), b"not a scene")
            .expect("malformed fixture");
        let world = app.world_mut();
        // The app already owns bookkeeping entities; spawning must preserve them.
        let mut expected: std::collections::BTreeSet<Entity> = world.query::<Entity>().iter(world).collect();
        let pawn = spawn_pawn(
            world,
            &crate::rules::SpawnRequest {
                prefab: Some("actors/hero.prefab.ron".into()),
                ..Default::default()
            },
        );
        assert!(world.get::<crate::character::Character>(pawn).is_some());
        expected.insert(pawn);
        assert_eq!(
            world
                .query::<Entity>()
                .iter(world)
                .collect::<std::collections::BTreeSet<_>>(),
            expected,
            "only the fallback pawn is added to the existing app entities"
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn a_missing_spawn_prefab_falls_back_without_pending_entities() {
        use regia_scene::{Transform, prefab::PendingPrefab};
        let (mut app, root) = prefab_app();
        let req = crate::rules::SpawnRequest {
            prefab: Some("actors/missing.prefab.ron".into()),
            transform: Transform::from_translation(regia_core::math::Vec3::X),
            team: Some(7),
        };
        let world = app.world_mut();
        // The app already owns bookkeeping entities; spawning must preserve them.
        let mut expected: std::collections::BTreeSet<Entity> = world.query::<Entity>().iter(world).collect();
        let pawn = spawn_pawn(world, &req);
        assert!(world.get::<crate::character::Character>(pawn).is_some());
        assert_eq!(world.get::<Transform>(pawn), Some(&req.transform));
        assert_eq!(
            world.get::<crate::sim_bridge::Team>(pawn),
            Some(&crate::sim_bridge::Team(7))
        );
        assert_eq!(world.query::<&PendingPrefab>().iter(world).count(), 0);
        expected.insert(pawn);
        assert_eq!(
            world
                .query::<Entity>()
                .iter(world)
                .collect::<std::collections::BTreeSet<_>>(),
            expected,
            "only the fallback pawn is added to the existing app entities"
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn a_spawn_request_team_lands_on_the_pawn() {
        let mut world = World::new();
        let req = crate::rules::SpawnRequest {
            team: Some(3),
            ..Default::default()
        };
        let pawn = spawn_pawn(&mut world, &req);
        assert_eq!(
            world.get::<crate::sim_bridge::Team>(pawn),
            Some(&crate::sim_bridge::Team(3))
        );
        let free = spawn_pawn(&mut world, &crate::rules::SpawnRequest::default());
        assert!(world.get::<crate::sim_bridge::Team>(free).is_none());
        assert!(world.get::<crate::character::Character>(free).is_some());
    }

    #[test]
    fn a_possessed_pawn_becomes_a_streaming_source_only_in_a_streamed_level() {
        use regia_scene::streaming::{LevelStreaming, SourceKind, StreamingSource};
        let mut app = App::new();
        app.add_systems(Update, adopt_streaming_sources);
        let pawn = app.world_mut().spawn_empty().id();
        let mut controller = PlayerController::default();
        controller.possess(pawn);
        app.world_mut().spawn(controller);
        app.update();
        assert!(app.world().get::<StreamingSource>(pawn).is_none());
        app.insert_resource(LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        });
        app.update();
        let source = app.world().get::<StreamingSource>(pawn).expect("a source");
        assert_eq!(source.kind, SourceKind::Player);
    }
    use std::time::Duration;

    use regia_platform::{ButtonState, GamepadButtonChanged, GamepadConnected};

    /// A joinable app: input plugin (gamepad messages), rules, players, one spawn point.
    fn join_app(max_local: u8) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_input::InputPlugin::default(),
            crate::rules::RulesPlugin,
            PlayerPlugin,
        ))
        .insert_resource(PlayerManager::new(max_local))
        .insert_resource(MaxLocalPlayers(max_local))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.world_mut().spawn((
            regia_scene::Transform {
                translation: regia_core::math::Vec3::new(1.0, 2.0, 3.0),
                ..Default::default()
            },
            PlayerStart,
        ));
        app
    }

    /// Connects a pad and presses South on it (a full press edge).
    fn press_south(app: &mut App, device: u64) {
        app.world_mut().write_message(GamepadConnected {
            device,
            name: "Test Pad".into(),
        });
        app.world_mut().write_message(GamepadButtonChanged {
            device,
            button: GamepadButton::South,
            state: ButtonState::Pressed,
            value: 1.0,
        });
    }

    /// Releases South on a device (so a later press is a fresh edge).
    fn release_south(app: &mut App, device: u64) {
        app.world_mut().write_message(GamepadButtonChanged {
            device,
            button: GamepadButton::South,
            state: ButtonState::Released,
            value: 0.0,
        });
    }

    #[test]
    fn player_manager_spawn_and_remove() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(PlayerManager::new(4));

        // Use spawn_player_static to avoid double borrow.
        let (player_entity, controller) =
            PlayerManager::spawn_player_static(app.world_mut(), PlayerKind::Local).expect("can join");

        assert!(app.world().get_entity(player_entity).is_ok());
        assert!(app.world().get_entity(controller).is_ok());
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1);

        // Remove
        assert!(PlayerManager::remove_player_static(app.world_mut(), 0));
        assert_eq!(app.world().resource::<PlayerManager>().count(), 0);
        assert!(app.world().get_entity(player_entity).is_err());
        assert!(app.world().get_entity(controller).is_err());
    }

    #[test]
    fn max_local_enforced() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(PlayerManager::new(1));

        let (_p0, _c0) =
            PlayerManager::spawn_player_static(app.world_mut(), PlayerKind::Local).expect("first join");
        let second = PlayerManager::spawn_player_static(app.world_mut(), PlayerKind::Local);
        assert!(second.is_none(), "cap enforced");
    }

    /// A join-button press on a new gamepad creates the player, the controller, a character at
    /// the rules' spawn point, and the possession + device assignment (the full § B3 join path).
    #[test]
    fn local_join_on_a_new_gamepad_button() {
        let mut app = join_app(4);
        press_south(&mut app, 7);
        app.update();

        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(pm.count(), 1, "one player joined");
        assert_eq!(pm.device_owner(7), Some(0), "the pad belongs to player 0");

        let player_entity = pm.player_entity(0).expect("player entity");
        let player = app
            .world()
            .get::<Player>(player_entity)
            .cloned()
            .expect("Player component");
        assert_eq!(player.kind, PlayerKind::Local);
        assert_eq!(player.device, Some(7));

        // The controller possesses a character spawned at the PlayerStart.
        let controller = app
            .world()
            .get::<PlayerController>(player.controller)
            .cloned()
            .expect("controller spawned");
        let possessed = app
            .world()
            .get::<regia_scene::Transform>(controller.possessed)
            .cloned()
            .expect("possessed character");
        assert_eq!(
            possessed.translation,
            regia_core::math::Vec3::new(1.0, 2.0, 3.0),
            "spawned at the PlayerStart (default rules)"
        );
        assert_eq!(controller.view_target, controller.possessed);

        // The input system's per-player device assignment follows.
        let assignments = app.world().resource::<regia_input::PlayerInputs>();
        assert_eq!(
            assignments.assignment(0),
            regia_input::DeviceAssignment::Gamepad(7)
        );
    }

    /// A second press on an already-assigned pad never spawns a second player; a *new* pad does.
    #[test]
    fn second_press_on_the_same_pad_spawns_nothing() {
        let mut app = join_app(4);
        press_south(&mut app, 7);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1);

        // Full press cycle on the same pad: still one player.
        release_south(&mut app, 7);
        app.update();
        press_south(&mut app, 7);
        app.update();
        assert_eq!(
            app.world().resource::<PlayerManager>().count(),
            1,
            "the same pad cannot join twice"
        );

        // A different pad joins as player 1.
        press_south(&mut app, 9);
        app.update();
        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(pm.count(), 2, "a new pad joins");
        assert_eq!(pm.device_owner(9), Some(1));
    }

    /// The max-local cap stops joins even from brand-new pads.
    #[test]
    fn server_bots_do_not_consume_local_viewports() {
        let mut app = join_app(4);
        press_south(&mut app, 7);
        app.update();
        for _ in 0..3 {
            PlayerManager::spawn_bot_static(app.world_mut()).expect("bot");
        }
        app.update();
        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(pm.count(), 4);
        assert_eq!(pm.layout.viewports.len(), 1);
        assert_eq!(pm.layout.viewports[&0], (0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn max_local_cap_blocks_new_pads() {
        let mut app = join_app(1);
        press_south(&mut app, 7);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1);

        press_south(&mut app, 9);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1, "cap enforced");
    }

    /// A `MaxLocalPlayers` resource of 1 stops a second pad even when the manager was created at 4.
    #[test]
    fn max_local_players_resource_lowers_the_manager_cap() {
        let mut app = join_app(4);
        app.insert_resource(MaxLocalPlayers(1));
        press_south(&mut app, 7);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1);
        assert_eq!(app.world().resource::<PlayerManager>().max_local, 1);
        press_south(&mut app, 9);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1, "resource cap");
    }

    /// Spec 14 B3: a manager cap written directly is left alone while `MaxLocalPlayers` stays at
    /// its default 4 (the default never raises a lower cap the game set on the manager).
    #[test]
    fn a_direct_manager_cap_survives_the_default_setting() {
        let mut app = join_app(4);
        app.insert_resource(PlayerManager::new(1))
            .insert_resource(MaxLocalPlayers::default());
        press_south(&mut app, 7);
        app.update();
        press_south(&mut app, 9);
        app.update();
        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(
            pm.max_local, 1,
            "the default setting did not overwrite the manager's cap"
        );
        assert_eq!(pm.count(), 1, "the manager's own cap of 1 holds");
    }

    /// Raising `MaxLocalPlayers` after a join lets the next pad in.
    #[test]
    fn raising_max_local_players_allows_another_pad() {
        let mut app = join_app(1);
        press_south(&mut app, 7);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1);
        app.insert_resource(MaxLocalPlayers(2));
        press_south(&mut app, 9);
        app.update();
        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(pm.max_local, 2);
        assert_eq!(pm.count(), 2, "the raised setting admits the second pad");
    }

    /// Leaving frees the pad: after removal, pressing the same pad joins a new player, and the
    /// rules see the player leave.
    #[test]
    fn leaving_frees_the_pad_and_notifies_rules() {
        let left_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let mut app = join_app(4);
        app.insert_resource(crate::rules::GameRulesResource {
            rules: Box::new(RecordingRules {
                left: left_log.clone(),
            }),
        });
        press_south(&mut app, 7);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 1);

        assert!(PlayerManager::remove_player_static(app.world_mut(), 0));
        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(pm.count(), 0);
        assert_eq!(pm.device_owner(7), None, "the pad is free again");
        let left = left_log.lock().expect("unpoisoned").clone();
        assert_eq!(left, vec![0], "on_player_left saw the player index");

        // The freed pad joins again as a new player.
        release_south(&mut app, 7);
        app.update();
        press_south(&mut app, 7);
        app.update();
        let pm = app.world().resource::<PlayerManager>();
        assert_eq!(pm.count(), 1, "the freed pad joins again");
        assert_eq!(pm.device_owner(7), Some(1), "as the next index");
    }

    /// Rules that record `on_player_left` into a shared log (the join path is covered by the
    /// spawn-choice test).
    struct RecordingRules {
        left: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl crate::rules::GameRules for RecordingRules {
        fn on_player_joined(
            &mut self,
            ctx: &mut crate::rules::RulesCtx,
            _player: Entity,
        ) -> Option<crate::rules::SpawnRequest> {
            let pos = crate::rules::default_spawn_point(ctx.world, Entity::PLACEHOLDER);
            Some(crate::rules::SpawnRequest {
                transform: regia_scene::Transform {
                    translation: pos,
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        fn on_player_left(&mut self, ctx: &mut crate::rules::RulesCtx, player: Entity) {
            let index = ctx
                .world
                .get::<Player>(player)
                .map(|p| p.index)
                .unwrap_or(u8::MAX);
            self.left.lock().expect("unpoisoned").push(index);
        }
    }

    #[test]
    fn viewport_layouts() {
        let mut pm = PlayerManager::new(4);

        // 0 players
        pm.compute_viewports();
        assert!(pm.layout.viewports.is_empty());

        // 1 player: full screen
        pm.players.insert(0, Entity::PLACEHOLDER);
        pm.compute_viewports();
        assert_eq!(pm.layout.viewports[&0], (0.0, 0.0, 1.0, 1.0));

        // 2 players: halves
        pm.players.insert(1, Entity::PLACEHOLDER);
        pm.compute_viewports();
        assert_eq!(pm.layout.viewports[&0], (0.0, 0.0, 0.5, 1.0));
        assert_eq!(pm.layout.viewports[&1], (0.5, 0.0, 0.5, 1.0));

        // 3 players
        pm.players.insert(2, Entity::PLACEHOLDER);
        pm.compute_viewports();
        assert_eq!(pm.layout.viewports[&0], (0.0, 0.0, 1.0, 0.5));
        assert_eq!(pm.layout.viewports[&1], (0.0, 0.5, 0.5, 0.5));
        assert_eq!(pm.layout.viewports[&2], (0.5, 0.5, 0.5, 0.5));

        // 4 players: quarters
        pm.players.insert(3, Entity::PLACEHOLDER);
        pm.compute_viewports();
        for i in 0..4 {
            let vp = pm.layout.viewports[&i];
            assert!(vp.0 + vp.2 <= 1.01, "player {i} viewport within bounds");
            assert!(vp.1 + vp.3 <= 1.01, "player {i} viewport within bounds");
        }
    }

    #[test]
    fn viewports_follow_real_indices_after_a_leave() {
        let mut pm = PlayerManager::new(4);
        pm.players.insert(1, Entity::PLACEHOLDER);
        pm.compute_viewports();
        assert_eq!(pm.layout.viewports.len(), 1);
        assert_eq!(pm.layout.viewports[&1], (0.0, 0.0, 1.0, 1.0));

        pm.players.insert(3, Entity::PLACEHOLDER);
        pm.compute_viewports();
        assert_eq!(pm.layout.viewports[&1], (0.0, 0.0, 0.5, 1.0));
        assert_eq!(pm.layout.viewports[&3], (0.5, 0.0, 0.5, 1.0));
        assert!(!pm.layout.viewports.contains_key(&0));
    }

    #[test]
    fn player_controller_own_management() {
        let mut pc = PlayerController::new(0);
        assert_eq!(pc.player_index, 0);
        assert_eq!(pc.possessed, Entity::PLACEHOLDER);

        pc.push_context("UI");
        assert_eq!(pc.active_context(), "UI");
        pc.pop_context();
        assert_eq!(pc.active_context(), GAMEPLAY_CONTEXT);
        // Never pop the last one.
        pc.pop_context();
        assert_eq!(pc.active_context(), GAMEPLAY_CONTEXT);

        let e = Entity::try_from_bits(42).expect("42 is a valid Entity");
        pc.possess(e);
        assert_eq!(pc.possessed, e);
        assert_eq!(pc.view_target, e);

        pc.unpossess();
        assert_eq!(pc.possessed, Entity::PLACEHOLDER);
    }

    #[test]
    fn player_start_is_a_marker() {
        let start = PlayerStart;
        let _ = format!("{:?}", start); // Debug
        let _ = start.clone(); // Clone
        let _ = PlayerStart; // unit struct
    }

    #[test]
    fn ai_controller_holds_possession_target() {
        let target = Entity::try_from_bits(100).expect("100 is a valid Entity");
        let ai = AiController::new(target);
        assert_eq!(ai.possessed, target);
        assert_eq!(ai.view_target, target);
    }

    #[test]
    fn camera_viewport_is_the_rect_or_the_full_target() {
        let mut pm = PlayerManager::new(4);
        pm.players.insert(0, Entity::PLACEHOLDER);
        pm.players.insert(1, Entity::PLACEHOLDER);
        pm.compute_viewports();
        assert_eq!(pm.layout.camera_viewport(0), [0.0, 0.0, 0.5, 1.0]);
        assert_eq!(pm.layout.camera_viewport(1), [0.5, 0.0, 0.5, 1.0]);
        assert_eq!(
            pm.layout.camera_viewport(7),
            [0.0, 0.0, 1.0, 1.0],
            "no rect: full target"
        );
    }

    /// ADR-7880: two and four local players: each tagged camera reads back its player's rect, a
    /// leave widens the survivors' cameras, and an untagged camera is never touched.
    #[cfg(feature = "client")]
    #[test]
    fn split_screen_rects_reach_the_tagged_cameras() {
        let mut app = join_app(4);
        let camera_for = |app: &mut App, player: u8| {
            app.world_mut()
                .spawn((regia_render::Camera::default(), SplitScreenCamera { player }))
                .id()
        };
        let cameras: Vec<Entity> = (0..4).map(|p| camera_for(&mut app, p)).collect();
        let untagged = app
            .world_mut()
            .spawn(regia_render::Camera {
                viewport: [0.25, 0.25, 0.1, 0.1],
                ..Default::default()
            })
            .id();
        let viewport = |app: &App, camera: Entity| {
            app.world()
                .get::<regia_render::Camera>(camera)
                .expect("camera")
                .viewport
        };
        // Two players: halves.
        press_south(&mut app, 1);
        app.update();
        press_south(&mut app, 2);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 2);
        assert_eq!(viewport(&app, cameras[0]), [0.0, 0.0, 0.5, 1.0]);
        assert_eq!(viewport(&app, cameras[1]), [0.5, 0.0, 0.5, 1.0]);
        assert_eq!(viewport(&app, cameras[2]), [0.0, 0.0, 1.0, 1.0], "not joined yet");
        // Four players: quarters.
        press_south(&mut app, 3);
        app.update();
        press_south(&mut app, 4);
        app.update();
        assert_eq!(app.world().resource::<PlayerManager>().count(), 4);
        let quarters = [
            [0.0, 0.0, 0.5, 0.5],
            [0.5, 0.0, 0.5, 0.5],
            [0.0, 0.5, 0.5, 0.5],
            [0.5, 0.5, 0.5, 0.5],
        ];
        for (player, camera) in cameras.iter().enumerate() {
            assert_eq!(viewport(&app, *camera), quarters[player], "player {player}");
        }
        assert_eq!(
            viewport(&app, untagged),
            [0.25, 0.25, 0.1, 0.1],
            "untagged camera untouched"
        );
        // A leave: three players, the layout and the cameras follow on the same frame.
        assert!(PlayerManager::remove_player_static(app.world_mut(), 3));
        app.update();
        assert_eq!(viewport(&app, cameras[0]), [0.0, 0.0, 1.0, 0.5]);
        assert_eq!(viewport(&app, cameras[1]), [0.0, 0.5, 0.5, 0.5]);
        assert_eq!(viewport(&app, cameras[2]), [0.5, 0.5, 0.5, 0.5]);
        assert_eq!(
            viewport(&app, cameras[3]),
            [0.0, 0.0, 1.0, 1.0],
            "the player left"
        );
    }

    /// The viewport write happens only on a difference: a steady layout leaves the cameras
    /// unchanged for change detection.
    #[cfg(feature = "client")]
    #[test]
    fn a_steady_layout_does_not_mark_cameras_changed() {
        #[derive(Resource, Default)]
        struct Changes(u32);
        fn count(cameras: Query<(), Changed<regia_render::Camera>>, mut changes: ResMut<Changes>) {
            changes.0 += cameras.iter().count() as u32;
        }
        let mut app = join_app(4);
        app.init_resource::<Changes>().add_systems(Last, count);
        app.world_mut()
            .spawn((regia_render::Camera::default(), SplitScreenCamera { player: 0 }));
        press_south(&mut app, 1);
        app.update();
        press_south(&mut app, 2);
        app.update();
        let after_joins = app.world().resource::<Changes>().0;
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<Changes>().0,
            after_joins,
            "no write while steady"
        );
    }
}
