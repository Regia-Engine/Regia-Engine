// SPDX-License-Identifier: MIT-0
//! Combat (ADR-2596): health, damage, hit reactions, death and respawn through the framework's
//! abilities, attributes and effects (`docs/specs/53-abilities-attributes-effects.md`).
//!
//! - Every fighter carries `Attributes` (`health`, `health_max` from a `.attrs.ron`), a `Health`
//!   that `HealthBridge` mirrors from them, a team tag and one attack grant ([`Combatant`]).
//! - A hit is [`deal_damage`]: the instant `combat/hit.effect.ron` with the damage as its
//!   `Damage.Amount` parameter, then `combat/stagger.effect.ron` (the hit reaction: the
//!   `Status.Staggered` tag blocks movement and the attack abilities for 0.45 s). Each hit writes
//!   a [`HitLanded`] message (HUD damage numbers, sounds, the enemy noticing its attacker).
//! - Death is the bridge's `DiedEvent`: an enemy is replaced by a limp ragdoll
//!   ([`crate::ragdoll_fx`]) and remembered as defeated in [`crate::checkpoint::CombatProgress`];
//!   a player character falls as a ragdoll, is [`Downed`] for [`RESPAWN_SECONDS`] and respawns at
//!   the last checkpoint with full health (`combat/heal_full.effect.ron`).
//!
//! The data files ship inside the binary (`include_str!`), as in `samples/arena`, so headless
//! tests and Android load the same bytes.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::abilities::{
    AbilityAssets, AbilityPlugin, ApplyEffect, AttrId, GrantId, GrantOptions, HealthBridge, Severity,
    StartContext, activate, apply_effect, check_activation, effective_has, grant_ability,
};
use regia_framework::character::{Character, CharacterState};
use regia_framework::damage::{DamagePlugin, Died, DiedEvent, Health};
use regia_framework::tags::{Tag, TagContainer, TagRegistry};
use regia_scene::Transform;

/// Team of the player characters.
pub const TEAM_PLAYER: u8 = 0;
/// Team of the enemies (also the team a player's noise is addressed to: `regia_ai::can_hear`
/// lets a listener hear events of its own team).
pub const TEAM_ENEMY: u8 = 1;

/// Seconds a downed player lies before the respawn.
pub const RESPAWN_SECONDS: f32 = 2.0;

/// Player attributes.
pub const PLAYER_ATTRS: &str = "combat/player.attrs.ron";
/// Grunt attributes.
pub const GRUNT_ATTRS: &str = "combat/grunt.attrs.ron";
/// Ranged caster attributes.
pub const RANGED_ATTRS: &str = "combat/ranged.attrs.ron";
/// The hit effect (instant, `Damage.Amount` parameter).
pub const HIT_EFFECT: &str = "combat/hit.effect.ron";
/// The hit-reaction effect.
pub const STAGGER_EFFECT: &str = "combat/stagger.effect.ron";
/// The respawn heal.
pub const HEAL_FULL_EFFECT: &str = "combat/heal_full.effect.ron";
/// The player's combo swing.
pub const SLASH_ABILITY: &str = "combat/slash.ability.ron";
/// The grunt's swipe.
pub const SWIPE_ABILITY: &str = "combat/swipe.ability.ron";
/// The caster's bolt.
pub const BOLT_ABILITY: &str = "combat/bolt.ability.ron";

/// The combat data, in load order (cooldown effects before the abilities naming them).
const COMBAT_ASSETS: &[(&str, &str)] = &[
    (PLAYER_ATTRS, include_str!("../assets/combat/player.attrs.ron")),
    (GRUNT_ATTRS, include_str!("../assets/combat/grunt.attrs.ron")),
    (RANGED_ATTRS, include_str!("../assets/combat/ranged.attrs.ron")),
    (HIT_EFFECT, include_str!("../assets/combat/hit.effect.ron")),
    (
        STAGGER_EFFECT,
        include_str!("../assets/combat/stagger.effect.ron"),
    ),
    (
        HEAL_FULL_EFFECT,
        include_str!("../assets/combat/heal_full.effect.ron"),
    ),
    (
        "combat/slash_cooldown.effect.ron",
        include_str!("../assets/combat/slash_cooldown.effect.ron"),
    ),
    (
        "combat/swipe_cooldown.effect.ron",
        include_str!("../assets/combat/swipe_cooldown.effect.ron"),
    ),
    (
        "combat/bolt_cooldown.effect.ron",
        include_str!("../assets/combat/bolt_cooldown.effect.ron"),
    ),
    (SLASH_ABILITY, include_str!("../assets/combat/slash.ability.ron")),
    (SWIPE_ABILITY, include_str!("../assets/combat/swipe.ability.ron")),
    (BOLT_ABILITY, include_str!("../assets/combat/bolt.ability.ron")),
];

/// The `health` attribute.
pub const HEALTH: AttrId = AttrId::of("health");
/// The `health_max` attribute.
pub const HEALTH_MAX: AttrId = AttrId::of("health_max");

/// A fighter: its team and the grant id of its attack ability (reflected, so `scene.get_entity`
/// shows the grant an agent passes to `abilities.check`).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct Combatant {
    /// [`TEAM_PLAYER`] or [`TEAM_ENEMY`].
    pub team: u8,
    /// Grant id of the attack ability.
    pub attack_grant: u32,
}

/// On a player character between its death and its respawn.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct Downed {
    /// Seconds left before the respawn (fixed-step time).
    pub respawn_in: f32,
}

/// Running totals for the HUD, the sounds and agent `play.expect` checks.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct CombatStats {
    /// Hits the player characters landed.
    pub player_hits: u32,
    /// Hits the enemies landed.
    pub enemy_hits: u32,
    /// Enemies killed.
    pub enemies_killed: u32,
    /// Player deaths.
    pub player_deaths: u32,
    /// Player respawns.
    pub respawns: u32,
    /// Ragdolls spawned (enemy and player deaths).
    pub ragdolls_spawned: u32,
    /// Where the last respawn put the character (capsule center).
    pub last_respawn: Vec3,
}

/// Problems loading the embedded combat data (empty when everything loaded).
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct CombatDiagnostics {
    /// One line per error: `path: code: message`.
    pub errors: Vec<String>,
}

/// One hit that took health (written by [`deal_damage`]).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct HitLanded {
    /// Who hit.
    pub attacker: Entity,
    /// Who was hit.
    pub target: Entity,
    /// Health removed.
    pub amount: f32,
    /// Where (the target's position).
    pub position: Vec3,
    /// The target's team.
    pub target_team: u8,
}

/// The fixed-step order of the sample's combat: brains and attacks before the deaths.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CombatSet {
    /// Projectiles move and hit.
    Projectiles,
    /// Deaths become ragdolls; downed players respawn.
    Deaths,
}

/// Installs abilities, the health bridge, damage, the combat data and the death/respawn systems.
pub struct CombatPlugin;

impl Plugin for CombatPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<AbilityPlugin>()) {
            app.add_plugins(AbilityPlugin::default());
        }
        if !app.has_plugin(std::any::type_name::<DamagePlugin>()) {
            app.add_plugins(DamagePlugin);
        }
        app.add_plugins(HealthBridge)
            .register_type::<Combatant>()
            .register_type::<Downed>()
            .register_type::<CombatStats>()
            .register_type::<CombatDiagnostics>()
            .register_type::<CombatSummary>()
            .init_resource::<CombatStats>()
            .add_message::<HitLanded>()
            .configure_sets(
                FixedUpdate,
                (
                    CombatSet::Projectiles.before(regia_framework::abilities::AbilitySet::Input),
                    CombatSet::Deaths.after(regia_framework::abilities::AbilitySet::Notify),
                ),
            )
            .add_systems(
                FixedUpdate,
                (handle_deaths, tick_respawns).chain().in_set(CombatSet::Deaths),
            );
        let errors = load_combat_data(app.world_mut());
        for line in &errors {
            tracing::error!(target: "third_person", "combat data: {line}");
        }
        app.insert_resource(CombatDiagnostics { errors });
    }
}

/// Loads the combat data into `AbilityAssets`, validated against the world's `TagRegistry`
/// (installed by `CarPlugin` from `assets/game.tags.ron`); returns one line per error.
pub fn load_combat_data(world: &mut World) -> Vec<String> {
    let registry = world.get_resource::<TagRegistry>().cloned();
    let mut errors = Vec::new();
    let Some(mut assets) = world.get_resource_mut::<AbilityAssets>() else {
        errors.push("AbilityAssets missing (AbilityPlugin not installed)".to_owned());
        return errors;
    };
    for (path, text) in COMBAT_ASSETS {
        for diag in assets.load_text(path, text, registry.as_ref(), None) {
            if diag.severity == Severity::Error {
                errors.push(format!("{path}: {}: {}", diag.code, diag.message));
            }
        }
    }
    errors
}

/// Adds `name` to `entity`'s tag container (creating it).
pub fn add_tag(world: &mut World, entity: Entity, name: &str) {
    let Ok(tag) = Tag::new(name) else {
        return;
    };
    let Ok(mut entity) = world.get_entity_mut(entity) else {
        return;
    };
    if let Some(mut container) = entity.get_mut::<TagContainer>() {
        container.add(tag);
    } else {
        let mut container = TagContainer::default();
        container.add(tag);
        entity.insert(container);
    }
}

/// Makes `entity` a fighter: attributes from `attrs`, a matching `Health`, the team tag and the
/// `ability` grant. Returns the grant, or `None` when the data did not load (the entity then
/// cannot fight; the HUD shows the diagnostics).
pub fn equip(world: &mut World, entity: Entity, attrs: &str, team: u8, ability: &str) -> Option<GrantId> {
    let attributes = world
        .get_resource::<AbilityAssets>()?
        .attributes(attrs)?
        .build()
        .ok()?;
    let max = attributes.get(HEALTH_MAX).unwrap_or(100.0);
    world
        .get_entity_mut(entity)
        .ok()?
        .insert((attributes, Health::new(max)));
    add_tag(
        world,
        entity,
        if team == TEAM_PLAYER {
            "Team.Player"
        } else {
            "Team.Enemy"
        },
    );
    let grant = grant_ability(world, entity, ability, GrantOptions::default()).ok()?;
    world.entity_mut(entity).insert(Combatant {
        team,
        attack_grant: grant.0,
    });
    Some(grant)
}

/// The entity's current health (`None` without attributes).
pub fn health(world: &World, entity: Entity) -> Option<f32> {
    world
        .get::<regia_framework::abilities::Attributes>(entity)
        .and_then(|a| a.get(HEALTH))
}

/// Whether `entity` is out of the fight: dead, downed or despawned.
pub fn is_down(world: &World, entity: Entity) -> bool {
    world.get_entity(entity).is_err()
        || world.get::<Died>(entity).is_some()
        || world.get::<Downed>(entity).is_some()
        || health(world, entity).is_some_and(|h| h <= 0.0)
}

/// Whether `entity` is in a hit reaction (`Status.Staggered`).
pub fn is_staggered(world: &World, entity: Entity) -> bool {
    Tag::new("Status.Staggered").is_ok_and(|tag| effective_has(world, entity, tag))
}

/// Tries to start `entity`'s attack grant (pays the cooldown). `false` when refused (on cooldown,
/// staggered, no grant).
pub fn try_attack(world: &mut World, entity: Entity) -> bool {
    let Some(grant) = world.get::<Combatant>(entity).map(|c| GrantId(c.attack_grant)) else {
        return false;
    };
    if check_activation(world, entity, grant, &StartContext::default()).is_err() {
        return false;
    }
    activate(world, entity, grant, StartContext::default()).is_ok()
}

/// Whether `entity`'s attack grant could start now.
pub fn attack_ready(world: &World, entity: Entity) -> bool {
    world.get::<Combatant>(entity).is_some_and(|c| {
        check_activation(world, entity, GrantId(c.attack_grant), &StartContext::default()).is_ok()
    })
}

/// One hit: `amount` health off `target` (the hit effect), then the hit reaction (the stagger
/// effect) and a [`HitLanded`] message. Friendly hits, hits on the dead and non-finite amounts do
/// nothing. Returns whether the hit landed.
pub fn deal_damage(world: &mut World, attacker: Entity, target: Entity, amount: f32) -> bool {
    if !amount.is_finite() || amount <= 0.0 || is_down(world, target) {
        return false;
    }
    let (Some(target_team), attacker_team) = (
        world.get::<Combatant>(target).map(|c| c.team),
        world.get::<Combatant>(attacker).map(|c| c.team),
    ) else {
        return false;
    };
    if attacker_team == Some(target_team) {
        return false;
    }
    let Ok(param) = Tag::new("Damage.Amount") else {
        return false;
    };
    let mut hit = ApplyEffect::new(HIT_EFFECT, target);
    hit.source = Some(attacker);
    hit.params.push((param, -amount));
    if apply_effect(world, hit).is_err() {
        return false;
    }
    let mut stagger = ApplyEffect::new(STAGGER_EFFECT, target);
    stagger.source = Some(attacker);
    let _ = apply_effect(world, stagger);
    let position = world
        .get::<Transform>(target)
        .map(|t| t.translation)
        .unwrap_or(Vec3::ZERO);
    if let Some(mut stats) = world.get_resource_mut::<CombatStats>() {
        if target_team == TEAM_PLAYER {
            stats.enemy_hits += 1;
        } else {
            stats.player_hits += 1;
        }
    }
    world.write_message(HitLanded {
        attacker,
        target,
        amount,
        position,
        target_team,
    });
    true
}

/// The player characters able to fight, in entity order: characters on [`TEAM_PLAYER`] that are
/// not down and not driving.
pub fn player_targets(world: &mut World) -> Vec<(Entity, Vec3)> {
    let mut query = world.query_filtered::<(Entity, &Combatant, &Transform), With<Character>>();
    let candidates: Vec<(Entity, Vec3)> = query
        .iter(world)
        .filter(|(_, c, _)| c.team == TEAM_PLAYER)
        .map(|(e, _, t)| (e, t.translation))
        .collect();
    candidates
        .into_iter()
        .filter(|(e, _)| !is_down(world, *e) && world.get::<crate::car::Driving>(*e).is_none())
        .collect()
}

/// `FixedUpdate` ([`CombatSet::Deaths`]): every `DiedEvent` this step becomes a ragdoll. An enemy
/// is despawned and remembered as defeated; a player character is [`Downed`].
#[tracing::instrument(skip_all, target = "third_person", name = "combat_deaths")]
pub fn handle_deaths(world: &mut World, mut cursor: Local<MessageCursor<DiedEvent>>) {
    let events: Vec<DiedEvent> = world
        .get_resource::<Messages<DiedEvent>>()
        .map(|m| cursor.read(m).copied().collect())
        .unwrap_or_default();
    for event in events {
        let entity = event.entity;
        let Some(transform) = world.get::<Transform>(entity).copied() else {
            continue;
        };
        let push = event
            .killer
            .and_then(|k| world.get::<Transform>(k).map(|t| t.translation))
            .map(|from| (transform.translation - from).normalize_or_zero())
            .unwrap_or(Vec3::ZERO);
        if let Some(enemy) = world.get::<crate::enemy::Enemy>(entity).cloned() {
            let color = crate::enemy::tuning(enemy.kind).color;
            crate::ragdoll_fx::spawn_ragdoll(world, transform, push, color, TEAM_ENEMY);
            crate::enemy::remove_enemy(world, entity);
            if let Some(mut progress) = world.get_resource_mut::<crate::checkpoint::CombatProgress>()
                && !progress.defeated.contains(&enemy.id)
            {
                progress.defeated.push(enemy.id);
            }
            if let Some(mut stats) = world.get_resource_mut::<CombatStats>() {
                stats.enemies_killed += 1;
                stats.ragdolls_spawned += 1;
            }
            tracing::info!(target: "third_person", "enemy {} defeated", enemy.id);
        } else if world.get::<Character>(entity).is_some() {
            crate::ragdoll_fx::spawn_ragdoll(world, transform, push, [0.85, 0.45, 0.2, 1.0], TEAM_PLAYER);
            world.entity_mut(entity).insert(Downed {
                respawn_in: RESPAWN_SECONDS,
            });
            set_rig_hidden(world, entity, true);
            if let Some(mut input) = world.get_mut::<regia_framework::character::CharacterInput>(entity) {
                *input = Default::default();
            }
            if let Some(mut stats) = world.get_resource_mut::<CombatStats>() {
                stats.player_deaths += 1;
                stats.ragdolls_spawned += 1;
            }
            tracing::info!(target: "third_person", "player character {entity:?} fell");
        }
    }
}

/// Hides (or shows) the Fox rig and the sword of `character`.
fn set_rig_hidden(world: &mut World, character: Entity, hidden: bool) {
    let visibility = if hidden {
        regia_scene::components::Visibility::Hidden
    } else {
        regia_scene::components::Visibility::Inherited
    };
    let mut visuals: Vec<Entity> = world
        .query::<(Entity, &crate::anim::RigOf)>()
        .iter(world)
        .filter(|(_, r)| r.character == character)
        .map(|(e, _)| e)
        .collect();
    visuals.extend(
        world
            .query::<(Entity, &crate::melee::Blade)>()
            .iter(world)
            .filter(|(_, b)| b.character == character)
            .map(|(e, _)| e),
    );
    for entity in visuals {
        world.entity_mut(entity).insert(visibility);
    }
}

/// `FixedUpdate`: counts down every [`Downed`] character and respawns it at the last checkpoint
/// with full health.
#[tracing::instrument(skip_all, target = "third_person", name = "combat_respawns")]
pub fn tick_respawns(world: &mut World) {
    let dt = world.resource::<Time>().delta_secs();
    let downed: Vec<(Entity, f32)> = world
        .query::<(Entity, &Downed)>()
        .iter(world)
        .map(|(e, d)| (e, d.respawn_in - dt))
        .collect();
    for (entity, left) in downed {
        if left > 0.0 {
            if let Some(mut d) = world.get_mut::<Downed>(entity) {
                d.respawn_in = left;
            }
            continue;
        }
        respawn(world, entity);
    }
}

/// Puts `character` back at the last checkpoint with full health.
pub fn respawn(world: &mut World, character: Entity) {
    let spot = crate::checkpoint::respawn_point(world);
    if let Some(mut transform) = world.get_mut::<Transform>(character) {
        transform.translation = spot;
    }
    if let Some(mut state) = world.get_mut::<CharacterState>(character) {
        state.velocity = Vec3::ZERO;
    }
    let mut heal = ApplyEffect::new(HEAL_FULL_EFFECT, character);
    heal.source = Some(character);
    let _ = apply_effect(world, heal);
    world.entity_mut(character).remove::<(Downed, Died)>();
    set_rig_hidden(world, character, false);
    if let Some(mut stats) = world.get_resource_mut::<CombatStats>() {
        stats.respawns += 1;
        stats.last_respawn = spot;
    }
    tracing::info!(target: "third_person", "respawned {character:?} at {spot}");
}

/// The demo's progress at a glance, on the `sample-state` entity (reflected: agent conditions
/// read components, so this mirrors the combat, checkpoint, sound and save resources).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct CombatSummary {
    /// Enemies killed.
    pub enemies_killed: u32,
    /// Enemies alive.
    pub enemies_alive: u32,
    /// Player deaths.
    pub player_deaths: u32,
    /// Player respawns.
    pub respawns: u32,
    /// Hits the players landed.
    pub player_hits: u32,
    /// Hits the enemies landed.
    pub enemy_hits: u32,
    /// Ragdolls spawned.
    pub ragdolls: u32,
    /// The active checkpoint.
    pub checkpoint: u32,
    /// Defeated enemy ids.
    pub defeated: Vec<u32>,
    /// Player 0's health.
    pub player_health: f32,
    /// Footsteps played.
    pub footsteps: u32,
    /// Hit sounds played.
    pub hit_sounds: u32,
    /// The ambient bed is playing.
    pub ambient_playing: bool,
    /// Saves made through the save flow.
    pub saves: u32,
    /// Loads made through the save flow.
    pub loads: u32,
    /// Loads migrated from save shape v1.
    pub migrated: u32,
}

/// `Update`: refreshes [`CombatSummary`] on the sample-state entity, and respawns the roster's
/// enemies when the defeated list shrinks (a load through any path, the agent's `save.load`
/// included, restores `CombatProgress`).
#[tracing::instrument(skip_all, target = "third_person", name = "combat_summary")]
pub fn summarize(world: &mut World, mut seen: Local<Option<Vec<u32>>>) {
    let defeated = world
        .get_resource::<crate::checkpoint::CombatProgress>()
        .map(|p| p.defeated.clone())
        .unwrap_or_default();
    if seen.as_ref() != Some(&defeated) {
        if seen.is_some() {
            crate::enemy::reconcile(world);
        }
        *seen = Some(defeated.clone());
    }
    let Some(state) = world
        .get_resource::<crate::collectible::StateHandle>()
        .and_then(|h| h.0)
    else {
        return;
    };
    let stats = world.get_resource::<CombatStats>().cloned().unwrap_or_default();
    let checkpoint = world
        .get_resource::<crate::checkpoint::CombatProgress>()
        .map_or(0, |p| p.checkpoint);
    let sound = world
        .get_resource::<crate::sound::SoundStats>()
        .cloned()
        .unwrap_or_default();
    let save = world
        .get_resource::<crate::save::SaveStatus>()
        .cloned()
        .unwrap_or_default();
    let enemies_alive = world.query::<&crate::enemy::Enemy>().iter(world).count() as u32;
    let player_health = crate::input::possessed(world, 0)
        .and_then(|c| health(world, c))
        .unwrap_or(0.0);
    let summary = CombatSummary {
        enemies_killed: stats.enemies_killed,
        enemies_alive,
        player_deaths: stats.player_deaths,
        respawns: stats.respawns,
        player_hits: stats.player_hits,
        enemy_hits: stats.enemy_hits,
        ragdolls: stats.ragdolls_spawned,
        checkpoint,
        defeated,
        player_health,
        footsteps: sound.footsteps,
        hit_sounds: sound.hits,
        ambient_playing: sound.ambient_playing,
        saves: save.saves,
        loads: save.loads,
        migrated: save.migrated,
    };
    if world.get::<CombatSummary>(state) != Some(&summary)
        && let Ok(mut entity) = world.get_entity_mut(state)
    {
        entity.insert(summary);
    }
}
