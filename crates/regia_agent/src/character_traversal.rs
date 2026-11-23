// SPDX-License-Identifier: MPL-2.0
//! Traversal moves on the agent protocol (`docs/specs/traversal.md` § 9,
//! book/agents/character-traversal.md, ADR-2971).
//!
//! - `physics.character_traversal { entity? }` reports every character with `TraversalSettings`
//!   (entity order): the running move, mode, swim phase and depth, curve progress, ladder and the last
//!   refusal.
//! - `physics.character_traversal_probe { entity, position?, forward? }` tells what this capsule could
//!   use now: a ledge, a ladder, the water. It changes nothing.

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::character::CharacterMovement;
use regia_framework::traversal::{
    CharacterTraversal, SwimPhase, TraversalKind, TraversalSettings, probe_traversal,
};
use serde_json::{Value, json};

use crate::anim_montage::{invalid, missing};
use crate::character_physics::mode_name;
use crate::protocol::RpcError;

fn vec3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// The optional `name` parameter: three finite numbers.
fn vec3_param(params: &Value, name: &str) -> Result<Option<Vec3>, RpcError> {
    let bad = || {
        invalid(
            "traversal_bad_param",
            format!("`{name}` must be three finite numbers"),
        )
    };
    let value = match params.get(name) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let items = value.as_array().ok_or_else(bad)?;
    let [x, y, z] = items.as_slice() else {
        return Err(bad());
    };
    let read = |v: &Value| v.as_f64().map(|f| f as f32).filter(|f| f.is_finite());
    match (read(x), read(y), read(z)) {
        (Some(x), Some(y), Some(z)) => Ok(Some(Vec3::new(x, y, z))),
        _ => Err(bad()),
    }
}

fn row(entity: Entity, movement: &CharacterMovement, at: Vec3, t: &CharacterTraversal) -> Value {
    json!({
        "entity": entity.to_bits(),
        "kind": t.kind.name(),
        "mode": mode_name(movement.mode),
        "phase": match t.phase {
            SwimPhase::Surface => "surface",
            SwimPhase::Underwater => "underwater",
        },
        "depth": t.depth,
        "progress": t.progress(),
        "ladder_id": matches!(t.kind, TraversalKind::Ladder | TraversalKind::LadderEnter).then_some(t.ladder.id),
        "last_refusal": t.last_refusal.map(|r| r.code()),
        "position": vec3(at),
    })
}

/// `physics.character_traversal`: every character with traversal moves (or the one named by
/// `entity`), in entity order.
pub fn character_traversal(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity") {
        None | Some(Value::Null) => None,
        Some(_) => Some(crate::scene::entity_from(params)?),
    };
    if let Some(entity) = only {
        require_traversal(world, entity)?;
    }
    let mut q = world.query_filtered::<(
        Entity,
        &CharacterMovement,
        &regia_scene::Transform,
        Option<&CharacterTraversal>,
    ), With<TraversalSettings>>();
    let idle = CharacterTraversal::default();
    let mut rows: Vec<((u32, u64), Value)> = q
        .iter(world)
        .filter(|(e, ..)| only.is_none_or(|o| o == *e))
        .map(|(entity, movement, transform, traversal)| {
            (
                (entity.index().index(), entity.to_bits()),
                row(
                    entity,
                    movement,
                    transform.translation,
                    traversal.unwrap_or(&idle),
                ),
            )
        })
        .collect();
    rows.sort_by_key(|(order, _)| *order);
    let count = rows.len();
    Ok(json!({
        "characters": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        "count": count,
    }))
}

/// `character_not_found` or `traversal_not_enabled` for an entity that cannot answer.
fn require_traversal(world: &World, entity: Entity) -> Result<(), RpcError> {
    if world.get::<CharacterMovement>(entity).is_none() {
        return Err(missing(
            "character_not_found",
            format!("entity {} is not a character", entity.to_bits()),
            "physics.characters lists the characters",
        ));
    }
    if world.get::<TraversalSettings>(entity).is_none() {
        return Err(missing(
            "traversal_not_enabled",
            format!("entity {} has no TraversalSettings", entity.to_bits()),
            "add the TraversalSettings component to give a character the traversal moves",
        ));
    }
    Ok(())
}

/// `physics.character_traversal_probe`: what the capsule could use, at its place or at `position`,
/// looking along its facing or `forward`.
pub fn character_traversal_probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let position = vec3_param(params, "position")?;
    let forward = vec3_param(params, "forward")?;
    if forward.is_some_and(|f| f.with_y(0.0).length_squared() < 1.0e-6) {
        return Err(invalid(
            "traversal_bad_param",
            "`forward` must have a horizontal length",
        ));
    }
    require_traversal(world, entity)?;
    let Some(probe) = probe_traversal(world, entity, position, forward) else {
        return Err(missing(
            "traversal_not_enabled",
            format!("entity {} cannot be probed", entity.to_bits()),
            "the entity needs CharacterMovement, Transform and TraversalSettings",
        ));
    };
    let ledge = match &probe.ledge {
        Ok(found) => json!({
            "available": true,
            "kind": found.kind.name(),
            "height": found.height,
            "edge": vec3(found.edge),
            "stand": vec3(found.stand),
            "normal": vec3(found.normal),
            "reason": Value::Null,
        }),
        Err(reason) => json!({ "available": false, "reason": reason.code() }),
    };
    let ladder = match &probe.ladder {
        Ok(found) => json!({
            "available": true,
            "user_id": found.user_id,
            "entity": found.entity.to_bits(),
            "attach": found.attach,
            "reason": Value::Null,
        }),
        Err(reason) => json!({ "available": false, "reason": reason.code() }),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "ledge": ledge,
        "ladder": ladder,
        "water": {
            "available": probe.water_surface.is_some(),
            "surface": probe.water_surface,
            "chest_under": probe.chest_under,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::character::{Character, CharacterInput, CharacterState, MovementMode};
    use regia_framework::traversal::{FlatWater, TraversalRefusal, TraversalWater};
    use regia_scene::Transform;

    fn character(world: &mut World, with_settings: bool) -> Entity {
        let e = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
                Character,
                CharacterMovement::default(),
                CharacterInput::default(),
                CharacterState::default(),
            ))
            .id();
        if with_settings {
            world.entity_mut(e).insert(TraversalSettings::default());
        }
        e
    }

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("error_code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn the_state_lists_only_characters_with_settings_and_shows_the_last_refusal() {
        let mut world = World::new();
        let plain = character(&mut world, false);
        let e = character(&mut world, true);
        if let Some(mut t) = world.get_mut::<CharacterTraversal>(e) {
            t.last_refusal = Some(TraversalRefusal::LedgeTooThin);
        }
        let out = character_traversal(&mut world, &json!({})).expect("reads");
        assert_eq!(out["count"], 1);
        let row = &out["characters"][0];
        assert_eq!(row["entity"], e.to_bits());
        assert_eq!(row["kind"], "none");
        assert_eq!(row["mode"], "walking");
        assert_eq!(row["last_refusal"], "ledge_too_thin");
        assert_eq!(row["ladder_id"], Value::Null);
        let one = character_traversal(&mut world, &json!({ "entity": e.to_bits() })).expect("one");
        assert_eq!(one["count"], 1);
        let err = character_traversal(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("off");
        assert_eq!(code(&err), "traversal_not_enabled");
    }

    #[test]
    fn the_mode_names_cover_the_reserved_ids() {
        assert_eq!(mode_name(MovementMode::CLIMBING), "climbing");
        assert_eq!(mode_name(MovementMode::TRAVERSING), "traversing");
        assert_eq!(mode_name(MovementMode::Custom(7)), "custom:7");
    }

    #[test]
    fn the_probe_reads_water_and_refuses_bad_params_and_entities() {
        let mut world = World::new();
        let e = character(&mut world, true);
        world.insert_resource(TraversalWater(Box::new(FlatWater {
            height: 5.0,
            min: [0.0, 0.0],
            max: [4.0, 4.0],
        })));
        let bits = e.to_bits();
        let out = character_traversal_probe(&mut world, &json!({ "entity": bits })).expect("probe");
        assert_eq!(out["water"]["available"], true);
        assert_eq!(out["water"]["surface"], 5.0);
        assert_eq!(out["water"]["chest_under"], true);
        // No physics world: the ledge and ladder say why.
        assert_eq!(out["ledge"]["reason"], "traversal_no_physics");
        assert_eq!(out["ladder"]["reason"], "traversal_no_physics");
        let dry = character_traversal_probe(
            &mut world,
            &json!({ "entity": bits, "position": [9.0, 0.0, 9.0], "forward": [1.0, 0.0, 0.0] }),
        )
        .expect("probe");
        assert_eq!(dry["water"]["available"], false);
        assert_eq!(dry["water"]["surface"], Value::Null);

        for params in [
            json!({ "entity": bits, "position": [1.0, 2.0] }),
            json!({ "entity": bits, "position": "here" }),
            json!({ "entity": bits, "position": [1.0, "x", 3.0] }),
            json!({ "entity": bits, "forward": [0.0, 1.0, 0.0] }),
        ] {
            let err = character_traversal_probe(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), "traversal_bad_param", "{params}");
        }
        let plain = character(&mut world, false);
        let err =
            character_traversal_probe(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("off");
        assert_eq!(code(&err), "traversal_not_enabled");
        let nothing = world.spawn_empty().id();
        let err =
            character_traversal_probe(&mut world, &json!({ "entity": nothing.to_bits() })).expect_err("none");
        assert_eq!(code(&err), "character_not_found");
    }
}
