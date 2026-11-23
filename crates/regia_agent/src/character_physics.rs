// SPDX-License-Identifier: MPL-2.0
//! Character controller polish and ragdoll budget on the agent protocol (spec 93 § 4,
//! book/agents/character-physics.md, ADR-1693).
//!
//! - `physics.characters { entity? }` reports every entity that has both `CharacterMovement`
//!   and `Transform` (entity order): position, velocity, grounded, crouched, movement mode
//!   and the physics ground state. A movement component without a transform is omitted;
//!   naming it is `character_not_found`.
//! - `anim.ragdoll_settings { joint_limits?, max_simulated? }` reads, and with fields sets,
//!   `regia_framework::ragdoll::RagdollSettings`, and counts simulated and frozen ragdolls.

use regia_ecs::prelude::*;
use regia_framework::character::{CharacterMovement, CharacterState, MovementMode};
use regia_framework::ragdoll::{RagdollBodies, RagdollSettings};
use serde_json::{Value, json};

use crate::anim_montage::invalid;
use crate::protocol::RpcError;

fn vec3(v: regia_core::math::Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

pub(crate) fn mode_name(mode: MovementMode) -> String {
    match mode {
        MovementMode::CLIMBING => "climbing".into(),
        MovementMode::TRAVERSING => "traversing".into(),
        MovementMode::Walking => "walking".into(),
        MovementMode::Falling => "falling".into(),
        MovementMode::Flying => "flying".into(),
        MovementMode::Swimming => "swimming".into(),
        MovementMode::Custom(id) => format!("custom:{id}"),
    }
}

/// `physics.characters`: every character entity (or the one named by `entity`), in entity
/// order.
pub fn characters(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity") {
        None | Some(Value::Null) => None,
        Some(_) => Some(crate::scene::entity_from(params)?),
    };
    let mut q = world.query::<(
        Entity,
        &CharacterMovement,
        &regia_scene::Transform,
        Option<&CharacterState>,
        Option<&regia_physics::CharacterBody>,
    )>();
    let mut rows: Vec<((u32, u64), Value)> = q
        .iter(world)
        .filter(|(e, ..)| only.is_none_or(|o| o == *e))
        .map(|(entity, movement, transform, state, body)| {
            let state = state.cloned().unwrap_or_default();
            let ground = body.map(|b| b.ground);
            (
                (entity.index().index(), entity.to_bits()),
                json!({
                    "entity": entity.to_bits(),
                    "position": vec3(transform.translation),
                    "velocity": vec3(state.velocity),
                    "grounded": state.grounded,
                    "crouched": state.crouched,
                    "mode": mode_name(movement.mode),
                    "time_in_air": state.time_in_air,
                    "ground_velocity": vec3(state.ground_velocity),
                    "ground_entity": ground
                        .filter(|g| g.ground_entity != Entity::PLACEHOLDER)
                        .map(|g| g.ground_entity.to_bits()),
                    "ground_normal": ground.map(|g| vec3(g.ground_normal)),
                    "ceiling_hit": ground.is_some_and(|g| g.ceiling_hit),
                    "sliding": ground.is_some_and(|g| g.sliding),
                }),
            )
        })
        .collect();
    if let Some(entity) = only
        && rows.is_empty()
    {
        return Err(crate::anim_montage::missing(
            "character_not_found",
            format!("entity {} is not a character", entity.to_bits()),
            "physics.characters with no entity lists the characters",
        ));
    }
    rows.sort_by_key(|(order, _)| *order);
    let count = rows.len();
    Ok(json!({
        "characters": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        "count": count,
    }))
}

/// `anim.ragdoll_settings`: read and optionally set the ragdoll quality switch and budget.
/// Every field is checked before anything changes.
pub fn ragdoll_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let joint_limits = match params.get("joint_limits") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(invalid("ragdoll_bad_flag", "`joint_limits` must be a boolean")),
    };
    let max_simulated = match params.get("max_simulated") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| {
            invalid(
                "ragdoll_bad_budget",
                "`max_simulated` must be a non-negative integer",
            )
        })?),
    };
    let mut settings = world
        .get_resource::<RagdollSettings>()
        .copied()
        .unwrap_or_default();
    if let Some(b) = joint_limits {
        settings.joint_limits = b;
    }
    if let Some(n) = max_simulated {
        settings.max_simulated = n;
    }
    if joint_limits.is_some() || max_simulated.is_some() {
        world.insert_resource(settings);
    }
    let mut q = world.query::<&RagdollBodies>();
    let (mut simulated, mut frozen, mut settled) = (0usize, 0usize, 0usize);
    for bodies in q.iter(world) {
        simulated += usize::from(bodies.simulating());
        frozen += usize::from(bodies.frozen());
        settled += usize::from(bodies.settled());
    }
    Ok(json!({
        "joint_limits": settings.joint_limits,
        "max_simulated": settings.max_simulated,
        "simulated": simulated,
        "frozen": frozen,
        "settled": settled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_framework::character::{Character, CharacterInput};
    use regia_scene::Transform;

    #[test]
    fn characters_lists_state_in_entity_order() {
        let mut world = World::new();
        let a = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
                Character,
                CharacterMovement::default(),
                CharacterInput::default(),
                CharacterState {
                    crouched: true,
                    grounded: true,
                    ..CharacterState::default()
                },
            ))
            .id();
        world.spawn((Transform::default(), Character, CharacterMovement::default()));
        let out = characters(&mut world, &json!({})).expect("physics.characters");
        assert_eq!(out["count"], 2);
        assert_eq!(out["characters"][0]["entity"], a.to_bits());
        assert_eq!(out["characters"][0]["crouched"], true);
        assert_eq!(out["characters"][0]["mode"], "walking");
        assert_eq!(out["characters"][0]["position"], json!([1.0, 2.0, 3.0]));
        let one = characters(&mut world, &json!({ "entity": a.to_bits() })).expect("one");
        assert_eq!(one["count"], 1);
        let missing = world.spawn(Transform::default()).id();
        let err =
            characters(&mut world, &json!({ "entity": missing.to_bits() })).expect_err("not a character");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("error_code")),
            Some(&json!("character_not_found"))
        );
    }

    #[test]
    fn ragdoll_settings_reads_sets_and_rejects_bad_fields() {
        let mut world = World::new();
        let out = ragdoll_settings(&mut world, &json!({})).expect("read");
        assert_eq!(out["joint_limits"], true);
        assert_eq!(out["max_simulated"], 16);
        let out =
            ragdoll_settings(&mut world, &json!({ "joint_limits": false, "max_simulated": 3 })).expect("set");
        assert_eq!(out["joint_limits"], false);
        assert_eq!(out["max_simulated"], 3);
        assert_eq!(world.resource::<RagdollSettings>().max_simulated, 3);
        for (params, code) in [
            (json!({ "max_simulated": -1 }), "ragdoll_bad_budget"),
            (json!({ "max_simulated": 1.5 }), "ragdoll_bad_budget"),
            (
                json!({ "joint_limits": "yes", "max_simulated": 9 }),
                "ragdoll_bad_flag",
            ),
        ] {
            let err = ragdoll_settings(&mut world, &params).expect_err("rejected");
            assert_eq!(
                err.data.as_ref().and_then(|d| d.get("error_code")),
                Some(&json!(code))
            );
        }
        assert_eq!(
            world.resource::<RagdollSettings>().max_simulated,
            3,
            "a rejected call changes nothing"
        );
    }

    /// Spec 93 § 4: every listed `physics.characters` field (ground entity, ground normal,
    /// ground velocity, ceiling hit, sliding, velocity, grounded) and the simulated / frozen
    /// counts of `anim.ragdoll_settings`.
    #[test]
    fn characters_report_every_ground_field_and_settings_count_ragdolls() {
        let mut world = World::new();
        let platform = world.spawn_empty().id();
        let mut body = regia_physics::CharacterBody::default();
        body.ground.grounded = true;
        body.ground.ground_entity = platform;
        body.ground.ground_normal = Vec3::Y;
        body.ground.ground_velocity = Vec3::new(2.0, 0.0, 0.0);
        body.ground.ceiling_hit = true;
        body.ground.sliding = true;
        let c = world
            .spawn((
                Transform::default(),
                Character,
                CharacterMovement::default(),
                CharacterState {
                    grounded: true,
                    velocity: Vec3::new(0.5, 0.0, 0.0),
                    ground_velocity: Vec3::new(2.0, 0.0, 0.0),
                    ..CharacterState::default()
                },
                body,
            ))
            .id();
        let bare = world
            .spawn((Transform::default(), Character, CharacterMovement::default()))
            .id();
        let out = characters(&mut world, &json!({})).expect("physics.characters");
        let row = &out["characters"][0];
        assert_eq!(row["entity"], c.to_bits());
        assert_eq!(row["grounded"], true);
        assert_eq!(row["velocity"], json!([0.5, 0.0, 0.0]));
        assert_eq!(row["ground_velocity"], json!([2.0, 0.0, 0.0]));
        assert_eq!(row["ground_entity"], platform.to_bits());
        assert_eq!(row["ground_normal"], json!([0.0, 1.0, 0.0]));
        assert_eq!(row["ceiling_hit"], true);
        assert_eq!(row["sliding"], true);
        // A character with no physics body reports no ground and no contacts.
        let row = &out["characters"][1];
        assert_eq!(row["entity"], bare.to_bits());
        assert_eq!(row["ground_entity"], Value::Null);
        assert_eq!(row["ground_normal"], Value::Null);
        assert_eq!(
            (&row["ceiling_hit"], &row["sliding"]),
            (&json!(false), &json!(false))
        );

        let settings = ragdoll_settings(&mut world, &json!({})).expect("read");
        assert_eq!(
            (&settings["simulated"], &settings["frozen"]),
            (&json!(0), &json!(0)),
            "no ragdolls: nothing simulated or frozen: {settings}"
        );
    }
}
