// SPDX-License-Identifier: MPL-2.0
//! The optional bridge from `Attributes` to the baseline `Health` (§ 3.5).

use regia_app::prelude::*;
use regia_ecs::ecs::message::{MessageCursor, Messages};

use super::effects::{Edit, edit_base};
use super::modifiers::{Magnitude, ModOp, Modifier};
use super::{AbilitySet, AttrId, Attributes};
use crate::damage::{DamageEvent, Died, DiedEvent, Health};
use regia_scene::components::EntityGuid;

/// Marker: this entity's `Health` mirrors its `health`/`health_max` attributes, so the baseline
/// damage system skips it.
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct HealthFromAttributes;

/// Bridge bookkeeping: whether the entity was alive at the last notify and the last attacker.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct HealthBridgeState {
    /// `health` was above 0 at the last notify.
    pub alive: bool,
    /// The source of the last damage.
    pub killer: Option<Entity>,
}

/// Turns `DamageEvent`s on bridged entities into instant `health` edits and mirrors the
/// attributes into `Health` (add after `AbilityPlugin` and `DamagePlugin`).
///
/// A heal (a negative amount) raises `health` after it has reached zero and does not remove
/// [`Died`]. [`HealthBridgeState::alive`] follows `current > 0`.
pub struct HealthBridge;

const HEALTH: AttrId = AttrId::of("health");
const HEALTH_MAX: AttrId = AttrId::of("health_max");

impl Plugin for HealthBridge {
    fn build(&self, app: &mut App) {
        app.register_type::<HealthFromAttributes>().add_systems(
            FixedUpdate,
            (
                (mark_bridged, restore_bridge_state, bridge_damage)
                    .chain()
                    .in_set(AbilitySet::Requests),
                bridge_notify.in_set(AbilitySet::Notify),
            ),
        );
    }
}

/// Entities with `Attributes` and `Health` that are not bridged yet.
type Unbridged<'w, 's> =
    Query<'w, 's, (Entity, &'static Attributes), (With<Health>, Without<HealthFromAttributes>)>;

fn mark_bridged(mut commands: Commands, query: Unbridged) {
    for (e, attrs) in &query {
        if let (Some(h), Some(_)) = (attrs.get(HEALTH), attrs.get(HEALTH_MAX)) {
            commands.entity(e).insert((
                HealthFromAttributes,
                HealthBridgeState {
                    alive: h > 0.0,
                    killer: None,
                },
            ));
        }
    }
}

/// Bridged entities that lost their bookkeeping: a loaded save restores the reflected
/// [`HealthFromAttributes`] marker but not [`HealthBridgeState`] (it holds an `Entity` and is not
/// saved). Without it the baseline damage system skips the entity and the notify never sees it,
/// so it would stop mirroring health and never die (audit-gameplay H4, ADR-1809).
type LostState<'w, 's> =
    Query<'w, 's, (Entity, &'static Attributes), (With<HealthFromAttributes>, Without<HealthBridgeState>)>;

fn restore_bridge_state(mut commands: Commands, query: LostState) {
    for (e, attrs) in &query {
        // Alive unless `health` is already at 0, so a loaded corpse does not die twice.
        let alive = attrs.get(HEALTH).is_none_or(|h| h > 0.0);
        commands
            .entity(e)
            .insert(HealthBridgeState { alive, killer: None });
    }
}

fn bridge_damage(world: &mut World, mut cursor: Local<MessageCursor<DamageEvent>>) {
    let events: Vec<DamageEvent> = world
        .get_resource::<Messages<DamageEvent>>()
        .map(|m| cursor.read(m).copied().collect())
        .unwrap_or_default();
    for event in events {
        if world.get::<HealthFromAttributes>(event.target).is_none() || !event.amount.is_finite() {
            continue;
        }
        let edit = [Modifier {
            attribute: "health".to_owned(),
            op: ModOp::AddPre,
            amount: Magnitude::Const(-event.amount),
            per_stack: false,
            when: None,
        }];
        let _ = edit_base(
            world,
            Edit {
                target: event.target,
                source: event.source,
                effect: None,
                mods: &edit,
                amounts: &[-event.amount],
                stacks: 1,
                dry: false,
            },
        );
        if let Some(mut state) = world.get_mut::<HealthBridgeState>(event.target) {
            state.killer = event.source;
        }
    }
}

/// Bridged entities whose deaths this step notify about, with the guid for the § 7 order.
type Bridged<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Attributes,
        &'static mut Health,
        &'static mut HealthBridgeState,
        Option<&'static EntityGuid>,
    ),
    With<HealthFromAttributes>,
>;

fn bridge_notify(mut commands: Commands, mut deaths: MessageWriter<DiedEvent>, mut query: Bridged) {
    // Mirror the attributes into `Health` per entity (order-free), then emit the deaths of this
    // step in the § 7 entity order (ascending `EntityGuid`, then bits) so simultaneous deaths are
    // deterministic across spawn order and seeks (audit G finding 6, ADR-4756).
    let mut died: Vec<(super::OrderKey, Entity, Option<Entity>)> = Vec::new();
    for (e, attrs, mut health, mut state, guid) in &mut query {
        let (Some(current), Some(max)) = (attrs.get(HEALTH), attrs.get(HEALTH_MAX)) else {
            continue;
        };
        health.current = current;
        health.max = max;
        let alive = current > 0.0;
        if state.alive && !alive {
            died.push((super::order_key(e, guid), e, state.killer));
        }
        state.alive = alive;
    }
    died.sort_by_key(|entry| entry.0);
    for (_, e, killer) in died {
        commands.entity(e).insert(Died);
        deaths.write(DiedEvent { entity: e, killer });
    }
}
