// SPDX-License-Identifier: MPL-2.0
//! Damage helper (`docs/specs/14-scene-and-framework.md` § B6): a small, sample-level
//! convenience for health, damage and death.
//!
//! [`Health`] is a reflected component a damageable entity carries. [`DamageEvent`] is a message
//! any system (or a script via its context) writes to hurt something; the [`DamagePlugin`] applies
//! the queued events in [`FixedUpdate`] (so damage is a fixed-step effect, deterministic per
//! replay), clips health at zero, and raises a [`DiedEvent`] the first time an entity's health
//! reaches zero, tagging it with the [`Died`] marker. While the marker is present the system ignores
//! damage events on that entity, heals included, until the game respawns it.
//!
//! Nothing here depends on a particular combat model: it only moves health numbers. The game
//! decides what produces a [`DamageEvent`], what `Died` means (respawn, gibs, game over) and how
//! much health an entity starts with.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::system::SystemParam;

/// Health of a damageable entity (§ B6 `Health`).
///
/// `current` is clamped to `0..=max` by the damage system; `max` is the authored ceiling. An
/// entity at zero health is dead ([`Died`] is added and [`DiedEvent`] raised once).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct Health {
    /// Current hit points (≥ 0).
    pub current: f32,
    /// Maximum hit points.
    pub max: f32,
}

impl Default for Health {
    fn default() -> Self {
        Self::new(100.0)
    }
}

impl Health {
    /// A full-health entity with `max` hit points. A NaN or negative `max` is stored as 0, so
    /// `current` stays ≥ 0 (a dead entity, not one whose health is NaN or negative). Positive
    /// infinity stays infinity.
    pub fn new(max: f32) -> Self {
        let max = if max.is_nan() { 0.0 } else { max.max(0.0) };
        Self { current: max, max }
    }

    /// The fraction of health remaining, `1.0` full → `0.0` dead.
    pub fn fraction(&self) -> f32 {
        if self.max <= 0.0 {
            0.0
        } else {
            (self.current / self.max).clamp(0.0, 1.0)
        }
    }

    /// Whether the entity has no health left.
    pub fn is_dead(&self) -> bool {
        self.current <= 0.0
    }
}

/// A request to hurt an entity (§ B6 `DamageEvent`). Written as a message and applied by the
/// damage system.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct DamageEvent {
    /// The entity to damage (must have a [`Health`] component).
    pub target: Entity,
    /// How many hit points to subtract. A negative amount heals; `current` is clamped to
    /// `0..=max`. An event on an entity that already has [`Died`] is ignored, heals included
    /// (entities with `HealthFromAttributes` follow the bridge's rules instead).
    pub amount: f32,
    /// Who dealt the damage (for kill attribution); `None` for environmental damage.
    pub source: Option<Entity>,
    /// Where the hit landed, in world space (impact VFX, decals); `None` when unknown.
    pub position: Option<Vec3>,
}

/// Marker: the entity has run out of health (§ B6 `Died`).
///
/// Added by the damage system when `current` reaches zero and never removed (a dead entity stays
/// dead until the game respawns or despawns it). While it is present the damage system ignores
/// [`DamageEvent`]s on the entity, heals included; a respawn restores [`Health`] and removes it.
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct Died;

/// Raised (as a message) once, the first time an entity's health reaches zero.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct DiedEvent {
    /// The entity that died.
    pub entity: Entity,
    /// Who dealt the killing blow, if known.
    pub killer: Option<Entity>,
}

/// How damage between two entities on the same non-zero [`crate::rules::TeamId`] is applied.
///
/// Consulted only when the target and the source are different entities and both carry a team.
/// Team `0` (neutral) and a missing team are not teammates, so damage is unchanged. A missing
/// resource is [`FriendlyFire::On`], which is today's behavior.
///
/// Reflected (ADR-8657), so hot reload keeps it and a world snapshot can include it with
/// [`crate::state_hash::insert_resource_text`] and `"FriendlyFire"`.
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Resource, Default, Debug, Clone, PartialEq)]
pub enum FriendlyFire {
    /// Full damage.
    #[default]
    On,
    /// The hit is ignored.
    Off,
    /// Multiply the amount by this factor. A non-finite factor is ignored.
    Scaled(f32),
    /// Leave the target unhurt and apply `amount * factor` to the source, once.
    ///
    /// A non-finite factor is ignored. The reflected hit is not tested as friendly fire again.
    /// If it kills the source, [`DiedEvent::killer`] is the original target.
    Reflect(f32),
}

/// Whether a [`DamageEvent`] whose source is the target itself applies.
///
/// A missing resource means enabled, which is today's behavior. Self-damage is not friendly fire.
/// Reflected like [`FriendlyFire`] (ADR-8657).
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Resource, Default, Debug, Clone, PartialEq)]
pub struct SelfDamage(pub bool);

impl Default for SelfDamage {
    fn default() -> Self {
        Self(true)
    }
}

/// Spawn protection settings (ADR-8647): how long a fresh spawn ignores damage, and whether its
/// first shot ends that early. The game inserts [`SpawnProtected`] with
/// [`SpawnProtection::protect`] when it spawns a pawn; nothing is protected by default.
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct SpawnProtection {
    /// Fixed steps the pawn ignores damage for. `0` protects nothing.
    pub ticks: u32,
    /// The pawn's first shot ends the protection: a [`ShotFired`] from it, or a [`DamageEvent`]
    /// it is the source of (that hit still lands).
    pub ends_on_fire: bool,
}

impl Default for SpawnProtection {
    fn default() -> Self {
        Self {
            ticks: 120,
            ends_on_fire: true,
        }
    }
}

impl SpawnProtection {
    /// The component to insert on a pawn that just spawned.
    pub fn protected(&self) -> SpawnProtected {
        SpawnProtected {
            ticks_left: self.ticks,
            ends_on_fire: self.ends_on_fire,
        }
    }

    /// Insert [`SpawnProtection::protected`] on `entity` (no-op for a missing entity or `ticks == 0`).
    pub fn protect(&self, world: &mut World, entity: Entity) {
        if self.ticks == 0 {
            return;
        }
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(self.protected());
        }
    }
}

/// A pawn that ignores damage (heals still apply) while `ticks_left > 0` (ADR-8647). The damage step
/// counts it down once per fixed step and removes it at zero.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct SpawnProtected {
    /// Fixed steps of protection left.
    pub ticks_left: u32,
    /// The pawn's first shot ends the protection.
    pub ends_on_fire: bool,
}

impl SpawnProtected {
    /// Whether damage is ignored now.
    pub fn active(&self) -> bool {
        self.ticks_left > 0
    }
}

/// A weapon fired (ADR-8647). Weapons write it so a [`SpawnProtected`] shooter with
/// `ends_on_fire` loses its protection on the step it fires, even when the shot hits nothing.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotFired {
    /// The entity that fired.
    pub shooter: Entity,
}

/// Applies queued [`DamageEvent`]s and raises [`DiedEvent`]s (§ B6).
///
/// Runs in [`FixedUpdate`] before scripts so damage and death are fixed-step effects, and so a
/// script reading `DiedEvent` through a `MessageReader` (it is an ECS message, not a reflected
/// `ScriptMessages` envelope, so `Script::on_message` does not receive it) sees the final health for the step.
pub struct DamagePlugin;

impl Plugin for DamagePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Health>()
            .register_type::<Died>()
            .register_type::<FriendlyFire>()
            .register_type::<SelfDamage>()
            .register_type::<SpawnProtected>()
            .register_type::<SpawnProtection>()
            .add_message::<ShotFired>()
            .init_resource::<FriendlyFire>()
            .init_resource::<SelfDamage>()
            .add_message::<DamageEvent>()
            .add_message::<DiedEvent>()
            .add_systems(
                FixedUpdate,
                (end_protection_on_shots, apply_damage, tick_spawn_protection)
                    .chain()
                    .in_set(DamageSet)
                    .before(crate::script::ScriptsSet::FixedUpdate),
            );
    }
}

/// The `FixedUpdate` damage step ([`DamageEvent`] → [`Health`] → [`DiedEvent`]). Games order
/// the systems that raise damage before it and the ones that react to deaths after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct DamageSet;

/// Team, ledger seat and friendly-fire inputs of one damage step.
#[derive(SystemParam)]
struct DamageContext<'w, 's> {
    teams: Query<'w, 's, &'static crate::rules::TeamId>,
    slots: Query<'w, 's, &'static crate::rules::LedgerSlot>,
    friendly: Option<Res<'w, FriendlyFire>>,
    self_damage: Option<Res<'w, SelfDamage>>,
    rules: Option<ResMut<'w, crate::rules::MatchRules>>,
    protected: Query<'w, 's, &'static mut SpawnProtected>,
}

/// `FixedUpdate`, before the damage step: a [`ShotFired`] ends its shooter's protection when it has
/// `ends_on_fire`.
fn end_protection_on_shots(mut shots: MessageReader<ShotFired>, mut protected: Query<&mut SpawnProtected>) {
    for shot in shots.read() {
        if let Ok(mut p) = protected.get_mut(shot.shooter)
            && p.ends_on_fire
        {
            p.ticks_left = 0;
        }
    }
}

/// `FixedUpdate`, after the damage step: counts protection down and removes it at zero.
fn tick_spawn_protection(mut commands: Commands, mut protected: Query<(Entity, &mut SpawnProtected)>) {
    for (entity, mut p) in &mut protected {
        p.ticks_left = p.ticks_left.saturating_sub(1);
        if p.ticks_left == 0 {
            commands.entity(entity).remove::<SpawnProtected>();
        }
    }
}

/// `FixedUpdate`: drains [`DamageEvent`]s, mutates [`Health`], and raises [`DiedEvent`] once per
/// entity on the step it dies.
#[tracing::instrument(skip_all, target = "regia_framework", name = "damage_apply")]
fn apply_damage(
    mut commands: Commands,
    mut events: MessageReader<DamageEvent>,
    mut deaths: MessageWriter<DiedEvent>,
    mut query: Query<(Entity, &mut Health, Has<Died>), Without<crate::abilities::HealthFromAttributes>>,
    mut context: DamageContext,
    mut killed: Local<Vec<Entity>>,
) {
    let policy = context.friendly.as_deref().copied().unwrap_or_default();
    let self_enabled = context.self_damage.as_deref().copied().unwrap_or_default().0;
    killed.clear();
    for event in events.read() {
        let mut target = event.target;
        let mut amount = event.amount;
        let mut killer = event.source;
        // A protected pawn's first shot ends its protection (the shot itself still lands).
        if let Some(source) = event.source
            && source != event.target
            && let Ok(mut p) = context.protected.get_mut(source)
            && p.ends_on_fire
        {
            p.ticks_left = 0;
        }
        if let Some(source) = event.source {
            if source == event.target {
                if !self_enabled {
                    continue;
                }
            } else if let (Ok(source_team), Ok(target_team)) =
                (context.teams.get(source), context.teams.get(event.target))
                && source_team.0 != 0
                && source_team.0 == target_team.0
            {
                match policy {
                    FriendlyFire::On => {}
                    FriendlyFire::Off => continue,
                    FriendlyFire::Scaled(factor) => {
                        if !factor.is_finite() {
                            continue;
                        }
                        amount *= factor;
                    }
                    FriendlyFire::Reflect(factor) => {
                        if !factor.is_finite() {
                            continue;
                        }
                        target = source;
                        amount *= factor;
                        killer = Some(event.target);
                    }
                }
            }
        }
        let Ok((entity, mut health, corpse)) = query.get_mut(target) else {
            // No health to damage: not a damageable entity. Skip silently — a stray damage event
            // on a wall or prop is normal and must never panic.
            continue;
        };
        // A dead entity stays dead until the game respawns it (restores health, removes `Died`):
        // a heal does not revive it, so a later hit cannot raise a second death. `Died` inserted
        // earlier in this step is still a deferred command, hence `killed`.
        if corpse || killed.contains(&entity) {
            continue;
        }
        // Spawn protection ignores hits; heals still apply.
        if amount > 0.0 && context.protected.get(entity).is_ok_and(|p| p.active()) {
            continue;
        }
        // A NaN amount (a script's `0.0 / 0.0` falloff) is no damage, and neither is an infinite one
        // on infinite health (`inf - inf`): either made `current` NaN for good, so the entity could
        // never die (audit-gameplay High, ADR-1809). An infinite amount otherwise clamps: a kill
        // volume's infinite hit kills and an infinite heal fills (ADR-1910 (1)).
        let next = health.current - amount;
        if next.is_nan() {
            tracing::warn!(target: "regia_framework", amount, "damage_nonfinite: ignored a DamageEvent that would make Health NaN on {:?}", target);
            continue;
        }
        let was_dead = health.is_dead();
        health.current = next.clamp(0.0, health.max.max(0.0));
        let died = !was_dead && health.is_dead();
        if died {
            commands.entity(entity).insert(Died);
            killed.push(entity);
            deaths.write(DiedEvent { entity, killer });
        }
        if amount > 0.0 && amount.is_finite() {
            let victim_seat = ledger_seat(&context.teams, &context.slots, entity);
            let attacker_seat =
                killer.and_then(|attacker| ledger_seat(&context.teams, &context.slots, attacker));
            if let (Some(rules), Some(victim_seat), Some(attacker_seat)) =
                (context.rules.as_deref_mut(), victim_seat, attacker_seat)
            {
                rules.session.note_hit(victim_seat, attacker_seat);
                if died {
                    rules.session.credit_kill(attacker_seat, victim_seat);
                    // `MatchSessionRules::on_character_died` must not record this death again. A
                    // self-kill credits nothing here, so it records that death there.
                    if attacker_seat != victim_seat {
                        commands.entity(entity).insert(crate::rules::DeathCredited);
                    }
                }
            }
        }
        // A seated pawn's death books its respawn or elimination (ADR-8647).
        if died
            && let (Some(rules), Some(seat)) = (
                context.rules.as_deref_mut(),
                ledger_seat(&context.teams, &context.slots, entity),
            )
        {
            rules.session.note_death(seat);
        }
    }
}

/// Team and slot, when both components are present and the team is not neutral.
fn ledger_seat(
    teams: &Query<&crate::rules::TeamId>,
    slots: &Query<&crate::rules::LedgerSlot>,
    entity: Entity,
) -> Option<regia_rules::match_flow::SeatId> {
    let team = teams.get(entity).ok()?;
    let slot = slots.get(entity).ok()?;
    if team.0 == 0 {
        return None;
    }
    Some(regia_rules::match_flow::SeatId::new(team.0, slot.0))
}

/// Convenience: writes a [`DamageEvent`] for `target` directly to the world (used by scripts and
/// systems that have `&mut World`). No-ops harmlessly on entities without health.
pub fn damage(
    world: &mut World,
    target: Entity,
    amount: f32,
    source: Option<Entity>,
    position: Option<Vec3>,
) {
    world.write_message(DamageEvent {
        target,
        amount,
        source,
        position,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::message::{MessageCursor, Messages};

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, DamagePlugin, crate::script::ScriptPlugin));
        enable_fixed_step(&mut app, 60.0);
        app
    }

    /// Reads all queued [`DiedEvent`]s with a fresh cursor.
    fn died_events(app: &App) -> Vec<DiedEvent> {
        let mut cursor = MessageCursor::default();
        cursor
            .read(app.world().resource::<Messages<DiedEvent>>())
            .cloned()
            .collect()
    }

    #[test]
    fn health_defaults_to_full() {
        let h = Health::default();
        assert_eq!(h.current, 100.0);
        assert_eq!(h.max, 100.0);
        assert_eq!(h.fraction(), 1.0);
        assert!(!h.is_dead());
    }

    #[test]
    fn damage_event_reduces_health_and_clamps_at_zero() {
        let mut app = app();
        let target = app.world_mut().spawn(Health::new(100.0)).id();

        app.world_mut().write_message(DamageEvent {
            target,
            amount: 30.0,
            source: None,
            position: None,
        });
        app.update();
        let health = app.world().get::<Health>(target).expect("health").to_owned();
        assert_eq!(health.current, 70.0);
        assert!(!health.is_dead());

        // Overkill clamps at zero, never negative.
        app.world_mut().write_message(DamageEvent {
            target,
            amount: 10_000.0,
            source: None,
            position: None,
        });
        app.update();
        let health = app.world().get::<Health>(target).expect("health").to_owned();
        assert_eq!(health.current, 0.0);
        assert!(health.is_dead());
    }

    /// audit-gameplay (High): one NaN amount made `current` NaN, `is_dead()` false forever and
    /// every later hit NaN too.
    #[test]
    fn a_nan_damage_amount_is_ignored_and_later_damage_still_kills() {
        let mut app = app();
        let target = app.world_mut().spawn(Health::new(10.0)).id();
        app.world_mut().write_message(DamageEvent {
            target,
            amount: f32::NAN,
            source: None,
            position: None,
        });
        app.update();
        let health = *app.world().get::<Health>(target).expect("health");
        assert_eq!(health.current, 10.0);
        // Infinite amounts clamp: an infinite heal fills, an infinite hit kills.
        for (amount, current) in [(f32::NEG_INFINITY, 10.0), (f32::INFINITY, 0.0)] {
            app.world_mut().write_message(DamageEvent {
                target,
                amount,
                source: None,
                position: None,
            });
            app.update();
            assert_eq!(
                app.world().get::<Health>(target).expect("health").current,
                current
            );
        }
        assert!(app.world().get::<Died>(target).is_some());
        let nan = Health::new(f32::NAN);
        assert_eq!((nan.current, nan.max), (0.0, 0.0));
        assert!(nan.is_dead());
        let negative = Health::new(-10.0);
        assert_eq!((negative.current, negative.max), (0.0, 0.0));
        assert!(negative.is_dead());
    }

    #[test]
    fn dying_raises_died_event_exactly_once_and_tags_the_entity() {
        let mut app = app();
        let killer = app.world_mut().spawn(()).id();
        let target = app.world_mut().spawn(Health::new(10.0)).id();

        app.world_mut().write_message(DamageEvent {
            target,
            amount: 50.0,
            source: Some(killer),
            position: None,
        });
        app.update();

        assert!(app.world().get::<Died>(target).is_some(), "died marker added");
        let deaths = died_events(&app);
        assert_eq!(deaths.len(), 1, "one death event on the killing step");
        assert_eq!(deaths[0].entity, target);
        assert_eq!(deaths[0].killer, Some(killer));

        // A second damage event does not raise a second death.
        app.world_mut().write_message(DamageEvent {
            target,
            amount: 5.0,
            source: None,
            position: None,
        });
        app.update();
        let deaths = died_events(&app);
        assert_eq!(deaths.len(), 1, "no double death event");
    }

    /// A heal on a dead entity does not bring it back, so a later hit raises no second death
    /// (audit framework5 #11); the game respawns by restoring health and removing `Died`.
    #[test]
    fn healing_a_corpse_does_not_emit_a_second_death() {
        let mut app = app();
        // Every death so far, read after each update with one cursor (the buffers keep two).
        let mut cursor = MessageCursor::<DiedEvent>::default();
        let mut deaths: Vec<Entity> = Vec::new();
        let mut step = |app: &mut App, hits: &[(Entity, f32)], deaths: &mut Vec<Entity>| {
            for &(target, amount) in hits {
                app.world_mut().write_message(DamageEvent {
                    target,
                    amount,
                    source: None,
                    position: None,
                });
            }
            app.update();
            deaths.extend(
                cursor
                    .read(app.world().resource::<Messages<DiedEvent>>())
                    .map(|d| d.entity),
            );
        };
        let target = app
            .world_mut()
            .spawn(Health {
                current: 10.0,
                max: 100.0,
            })
            .id();
        step(&mut app, &[(target, 100.0)], &mut deaths);
        assert_eq!(deaths, [target]);
        step(&mut app, &[(target, -50.0)], &mut deaths);
        assert_eq!(
            app.world().get::<Health>(target).expect("health").current,
            0.0,
            "a corpse is not healed"
        );
        assert!(app.world().get::<Died>(target).is_some());
        step(&mut app, &[(target, 50.0)], &mut deaths);
        assert_eq!(deaths, [target], "no second death");

        // The kill and the heal in one step: the heal still finds a corpse.
        let other = app.world_mut().spawn(Health::new(10.0)).id();
        step(
            &mut app,
            &[(other, 20.0), (other, -50.0), (other, 20.0)],
            &mut deaths,
        );
        assert_eq!(deaths, [target, other]);
        assert_eq!(app.world().get::<Health>(other).expect("health").current, 0.0);

        // A respawn (health restored, marker removed) can die again.
        app.world_mut()
            .entity_mut(target)
            .insert(Health::new(10.0))
            .remove::<Died>();
        step(&mut app, &[(target, 20.0)], &mut deaths);
        assert_eq!(deaths, [target, other, target]);
    }

    #[test]
    fn a_non_finite_amount_is_ignored_and_does_not_poison_health() {
        let mut app = app();
        let target = app.world_mut().spawn(Health::new(100.0)).id();
        let endless = app.world_mut().spawn(Health::new(f32::INFINITY)).id();
        // NaN on any health, and an infinite hit on infinite health (`inf - inf`).
        for (target, amount) in [(target, f32::NAN), (endless, f32::NAN), (endless, f32::INFINITY)] {
            app.world_mut().write_message(DamageEvent {
                target,
                amount,
                source: None,
                position: None,
            });
            app.update();
        }
        assert_eq!(app.world().get::<Health>(target).map(|h| h.current), Some(100.0));
        assert_eq!(
            app.world().get::<Health>(endless).map(|h| h.current),
            Some(f32::INFINITY)
        );
        app.world_mut().write_message(DamageEvent {
            target,
            amount: 100.0,
            source: None,
            position: None,
        });
        app.update();
        assert!(
            app.world().get::<Died>(target).is_some(),
            "later damage still kills"
        );
    }

    #[test]
    fn damage_to_an_entity_without_health_is_ignored() {
        let mut app = app();
        let wall = app.world_mut().spawn(()).id();
        app.world_mut().write_message(DamageEvent {
            target: wall,
            amount: 100.0,
            source: None,
            position: None,
        });
        app.update(); // must not panic
        let deaths = died_events(&app);
        assert!(deaths.is_empty());
    }

    #[test]
    fn helper_writes_a_damage_event() {
        let mut app = app();
        let target = app.world_mut().spawn(Health::new(50.0)).id();
        damage(app.world_mut(), target, 10.0, None, None);
        app.update();
        assert_eq!(app.world().get::<Health>(target).expect("health").current, 40.0);
    }

    fn pawn(app: &mut App, team: u8, health: f32) -> Entity {
        app.world_mut()
            .spawn((Health::new(health), crate::rules::TeamId(team)))
            .id()
    }

    #[test]
    fn friendly_fire_off_ignores_teammate_damage_and_scaled_halves_it() {
        let mut app = app();
        let source = pawn(&mut app, 1, 100.0);
        let mate = pawn(&mut app, 1, 100.0);
        let other = pawn(&mut app, 2, 100.0);
        app.world_mut().insert_resource(FriendlyFire::Off);
        app.world_mut().write_message(DamageEvent {
            target: mate,
            amount: 40.0,
            source: Some(source),
            position: None,
        });
        app.update();
        assert_eq!(app.world().get::<Health>(mate).expect("health").current, 100.0);
        app.world_mut().write_message(DamageEvent {
            target: other,
            amount: 40.0,
            source: Some(source),
            position: None,
        });
        app.update();
        assert_eq!(
            app.world().get::<Health>(other).expect("health").current,
            60.0,
            "another team is not friendly fire"
        );

        app.world_mut().insert_resource(FriendlyFire::Scaled(0.5));
        app.world_mut().write_message(DamageEvent {
            target: mate,
            amount: 40.0,
            source: Some(source),
            position: None,
        });
        app.update();
        assert_eq!(app.world().get::<Health>(mate).expect("health").current, 80.0);

        app.world_mut().insert_resource(FriendlyFire::Reflect(0.5));
        app.world_mut().write_message(DamageEvent {
            target: mate,
            amount: 40.0,
            source: Some(source),
            position: None,
        });
        app.update();
        assert_eq!(app.world().get::<Health>(mate).expect("health").current, 80.0);
        assert_eq!(app.world().get::<Health>(source).expect("health").current, 80.0);

        app.world_mut().insert_resource(SelfDamage(false));
        app.world_mut().write_message(DamageEvent {
            target: source,
            amount: 10.0,
            source: Some(source),
            position: None,
        });
        app.update();
        assert_eq!(
            app.world().get::<Health>(source).expect("health").current,
            80.0,
            "self-damage is a separate switch"
        );
    }

    #[test]
    fn damage_with_ledger_slots_credits_the_match_session() {
        use regia_rules::match_flow::SeatId;

        let mut app = app();
        app.insert_resource(crate::rules::MatchRules::default());
        let killer = app
            .world_mut()
            .spawn((crate::rules::TeamId(1), crate::rules::LedgerSlot(0)))
            .id();
        let victim = app
            .world_mut()
            .spawn((
                Health::new(10.0),
                crate::rules::TeamId(2),
                crate::rules::LedgerSlot(1),
            ))
            .id();
        app.world_mut().write_message(DamageEvent {
            target: victim,
            amount: 4.0,
            source: Some(killer),
            position: None,
        });
        app.update();
        app.world_mut().write_message(DamageEvent {
            target: victim,
            amount: 10.0,
            source: Some(killer),
            position: None,
        });
        app.update();
        let rules = app.world().resource::<crate::rules::MatchRules>();
        let killer_row = rules.session.ledgers.row(SeatId::new(1, 0)).expect("killer");
        let victim_row = rules.session.ledgers.row(SeatId::new(2, 1)).expect("victim");
        assert_eq!(killer_row.kills, 1);
        assert_eq!(victim_row.deaths, 1);
        assert!(app.world().get::<Died>(victim).is_some());
    }

    /// After a team change and `retag_moved_seats`, damage credits the moved row and opens no
    /// second row on the seat the player left (ADR-8657).
    #[test]
    fn damage_after_a_seat_move_credits_the_moved_row() {
        use regia_rules::match_flow::{FlowInput, SeatId};
        use regia_rules::teams::{SeatRequest, TeamSwitch};

        let mut app = app();
        let mut rules = crate::rules::MatchRules::default();
        rules.session.teams.switch = TeamSwitch::BetweenMatches;
        for seat in [(1, 0), (2, 0)] {
            rules
                .session
                .apply(&FlowInput::Seat {
                    team: seat.0,
                    slot: seat.1,
                })
                .expect("seat");
        }
        rules.session.apply(&FlowInput::OpenBetween).expect("between");
        app.insert_resource(rules);
        let killer = app
            .world_mut()
            .spawn((crate::rules::TeamId(1), crate::rules::LedgerSlot(0)))
            .id();
        let victim = app
            .world_mut()
            .spawn((
                Health::new(10.0),
                crate::rules::TeamId(2),
                crate::rules::LedgerSlot(0),
            ))
            .id();
        let moved = app
            .world_mut()
            .resource_mut::<crate::rules::MatchRules>()
            .session
            .assign_team(
                &SeatRequest {
                    from: Some(SeatId::new(1, 0)),
                    team: Some(3),
                    slot: Some(0),
                    unit: None,
                },
                &[],
            )
            .expect("move");
        assert_eq!(moved, vec![SeatId::new(3, 0)]);
        assert_eq!(crate::rules::retag_moved_seats(app.world_mut()), 1);
        assert_eq!(
            app.world().get::<crate::rules::TeamId>(killer).copied(),
            Some(crate::rules::TeamId(3))
        );
        app.world_mut().write_message(DamageEvent {
            target: victim,
            amount: 10.0,
            source: Some(killer),
            position: None,
        });
        app.update();
        let rules = app.world().resource::<crate::rules::MatchRules>();
        assert_eq!(
            rules
                .session
                .ledgers
                .row(SeatId::new(3, 0))
                .expect("moved row")
                .kills,
            1
        );
        assert!(
            rules.session.ledgers.row(SeatId::new(1, 0)).is_none(),
            "a hit after the move opened a second row on the seat the player left"
        );
        assert!(rules.session.seat_moves.is_empty(), "retag drains the moves");
    }

    /// A swap in one assign call retags both sides.
    #[test]
    fn retag_after_a_swap_moves_both_entities() {
        use regia_rules::match_flow::SeatId;
        use regia_rules::teams::SeatMove;

        let mut app = app();
        let mut rules = crate::rules::MatchRules::default();
        let a = SeatId::new(1, 0);
        let b = SeatId::new(2, 0);
        rules.session.seat_moves = vec![
            SeatMove {
                from: a,
                to: b,
                batch: 0,
            },
            SeatMove {
                from: b,
                to: a,
                batch: 0,
            },
            SeatMove {
                from: b,
                to: SeatId::new(3, 1),
                batch: 1,
            },
        ];
        app.insert_resource(rules);
        let first = app
            .world_mut()
            .spawn((crate::rules::TeamId(1), crate::rules::LedgerSlot(0)))
            .id();
        let second = app
            .world_mut()
            .spawn((crate::rules::TeamId(2), crate::rules::LedgerSlot(0)))
            .id();
        assert_eq!(crate::rules::retag_moved_seats(app.world_mut()), 2);
        let seat = |entity| {
            (
                app.world().get::<crate::rules::TeamId>(entity).map(|team| team.0),
                app.world()
                    .get::<crate::rules::LedgerSlot>(entity)
                    .map(|slot| slot.0),
            )
        };
        assert_eq!(
            seat(first),
            (Some(3), Some(1)),
            "swapped to 2/0, then moved on to 3/1"
        );
        assert_eq!(seat(second), (Some(1), Some(0)));
    }

    /// `MatchRules`, `FriendlyFire` and `SelfDamage` are reflected resources: a world snapshot
    /// can include them (ADR-8657).
    #[test]
    fn match_rules_and_damage_policy_join_a_world_snapshot() {
        use regia_rules::match_flow::{FlowInput, SeatId};

        let mut app = app();
        app.add_plugins(crate::rules::MatchRulesPlugin);
        app.world_mut().insert_resource(FriendlyFire::Off);
        app.world_mut().insert_resource(SelfDamage(false));
        {
            let mut rules = app.world_mut().resource_mut::<crate::rules::MatchRules>();
            rules
                .session
                .apply(&FlowInput::Seat { team: 1, slot: 0 })
                .expect("seat");
            rules.session.credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
        }
        let mut snapshot = crate::state_hash::world_snapshot(app.world());
        for name in ["MatchRules", "FriendlyFire", "SelfDamage"] {
            assert!(
                crate::state_hash::insert_resource_text(app.world(), &mut snapshot, name),
                "{name} is not a reflected resource"
            );
        }
        assert_eq!(snapshot["$resource/FriendlyFire"], "Off");
        assert_eq!(snapshot["$resource/SelfDamage"], "(false)");
        assert!(snapshot["$resource/MatchRules"].contains("kills: 1"));
        let before = snapshot["$resource/MatchRules"].clone();
        app.world_mut()
            .resource_mut::<crate::rules::MatchRules>()
            .session
            .credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
        let mut after = crate::state_hash::world_snapshot(app.world());
        assert!(crate::state_hash::insert_resource_text(
            app.world(),
            &mut after,
            "MatchRules"
        ));
        assert_ne!(
            after["$resource/MatchRules"], before,
            "a ledger change moves the snapshot"
        );
    }

    /// ADR-8647: a protected pawn ignores hits (heals still apply) until its protection runs out,
    /// and its first shot ends the protection early.
    #[test]
    fn spawn_protection_ignores_damage_until_expiry_or_first_shot() {
        let mut app = app();
        let settings = SpawnProtection {
            ticks: 3,
            ends_on_fire: true,
        };
        let pawn = app.world_mut().spawn(Health::new(100.0)).id();
        let other = app.world_mut().spawn(Health::new(100.0)).id();
        settings.protect(app.world_mut(), pawn);
        let hit = |app: &mut App, target: Entity, amount: f32, source: Option<Entity>| {
            app.world_mut().write_message(DamageEvent {
                target,
                amount,
                source,
                position: None,
            });
            app.update();
            app.world().get::<Health>(target).expect("health").current
        };
        // Three protected steps: hits are ignored, a heal is not (health is full, so it stays 100).
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 100.0);
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 100.0);
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 100.0);
        assert!(
            app.world().get::<SpawnProtected>(pawn).is_none(),
            "expired after 3 steps"
        );
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 90.0);

        // First shot: a hit the pawn deals lands and ends its protection on that step.
        settings.protect(app.world_mut(), pawn);
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 90.0);
        assert_eq!(hit(&mut app, other, 5.0, Some(pawn)), 95.0);
        assert!(app.world().get::<SpawnProtected>(pawn).is_none());
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 80.0);

        // A ShotFired that hits nothing ends it too.
        settings.protect(app.world_mut(), pawn);
        app.world_mut().write_message(ShotFired { shooter: pawn });
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 70.0);

        // Without ends_on_fire the shot keeps the protection.
        SpawnProtection {
            ticks: 5,
            ends_on_fire: false,
        }
        .protect(app.world_mut(), pawn);
        assert_eq!(hit(&mut app, other, 5.0, Some(pawn)), 90.0);
        assert_eq!(hit(&mut app, pawn, 10.0, Some(other)), 70.0);
    }

    /// ADR-8647: the death of a seated pawn reaches the session's respawn book; with one life the
    /// seat is eliminated.
    #[test]
    fn a_seated_death_books_the_session_respawn() {
        use regia_rules::match_flow::SeatId;
        use regia_rules::session::{MatchSession, SessionSettings};
        use regia_rules::spawn::{RespawnPolicy, RespawnRules};

        let mut app = app();
        app.insert_resource(crate::rules::MatchRules {
            session: MatchSession::new(SessionSettings {
                respawn: RespawnRules {
                    default: RespawnPolicy::Lives { lives: 1 },
                    ..RespawnRules::default()
                },
                ..SessionSettings::default()
            }),
        });
        let victim = app
            .world_mut()
            .spawn((
                Health::new(10.0),
                crate::rules::TeamId(2),
                crate::rules::LedgerSlot(1),
            ))
            .id();
        app.world_mut().write_message(DamageEvent {
            target: victim,
            amount: 50.0,
            source: None,
            position: None,
        });
        app.update();
        let rules = app.world().resource::<crate::rules::MatchRules>();
        assert!(rules.session.respawns.is_eliminated(SeatId::new(2, 1)));
    }
}
