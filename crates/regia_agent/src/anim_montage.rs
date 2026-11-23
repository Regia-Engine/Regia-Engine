// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for montages, sync groups and motion warping (spec 17 § 4b,
//! ADR-1465, `book/agents/montages.md`): `anim.montage_state` reports a rig's slots, montage
//! library, playing instances, queue, warp targets and recent events; `anim.montage_play`,
//! `anim.montage_stop` and `anim.montage_jump` drive playback; `anim.montage_set_warp_target`
//! sets or clears a warp target; `anim.sync_state` shows each sync group's leader.
//!
//! Errors carry a stable lowercase `error_code` in `data` next to the `F0xxx` code (listed in
//! the book page). Handlers are plain functions over `&mut World`.

use regia_animation::montage::SectionNext;
use regia_animation::montage_player::MontageRecord;
use regia_animation::warp::yaw_of;
use regia_animation::{
    AnimatorSource, BranchAction, ClipLibrary, Montage, MontageEventKind, MontageId, MontagePlayer,
    MontageStatus, PlayOptions, WarpTarget,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn with_error_code(mut err: RpcError, error_code: &str) -> RpcError {
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("error_code".into(), json!(error_code));
    }
    err
}

pub(crate) fn invalid(error_code: &str, message: impl Into<String>) -> RpcError {
    with_error_code(RpcError::invalid_params(message), error_code)
}

pub(crate) fn missing(error_code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    with_error_code(RpcError::not_found(message, hint), error_code)
}

/// The rig entity named by `params.entity`; it must have an `Animator`.
pub(crate) fn rig_of(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(missing(
            "entity_missing",
            format!("entity {} does not exist", entity.to_bits()),
            "list animated rigs with anim.list",
        ));
    }
    if world.get::<regia_animation::Animator>(entity).is_none() {
        return Err(missing(
            "no_animator",
            format!("entity {} has no Animator", entity.to_bits()),
            "list animated rigs with anim.list",
        ));
    }
    Ok(entity)
}

fn player_of(world: &World, entity: Entity) -> Result<&MontagePlayer, RpcError> {
    world.get::<MontagePlayer>(entity).ok_or_else(|| {
        missing(
            "no_montage_player",
            format!("entity {} has no MontagePlayer", entity.to_bits()),
            "play a montage with anim.montage_play (it adds a player), or add MontagePlayer in code",
        )
    })
}

pub(crate) fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn status_json(status: &MontageStatus) -> Value {
    json!({
        "id": status.id.0,
        "montage": status.montage,
        "section": status.section,
        "time": status.time,
        "length": status.length,
        "weight": status.weight,
        "rate": status.rate,
        "phase": status.phase.as_str(),
        "interrupted": status.interrupted,
        "reverse": status.reverse,
    })
}

fn record_json(record: &MontageRecord) -> Value {
    let mut value = json!({
        "id": record.id.0,
        "montage": record.montage,
        "kind": record.kind.as_str(),
    });
    let extra = match &record.kind {
        MontageEventKind::Section { from, to } => json!({ "from": from, "to": to }),
        MontageEventKind::Notify { name, payload } | MontageEventKind::WindowBegin { name, payload } => {
            json!({ "name": name, "payload": payload })
        }
        MontageEventKind::WindowEnd { name } | MontageEventKind::BranchPoint { name } => {
            json!({ "name": name })
        }
        MontageEventKind::BlendingOut { interrupted } | MontageEventKind::Ended { interrupted } => {
            json!({ "interrupted": interrupted })
        }
        MontageEventKind::Started => json!({}),
    };
    if let (Some(object), Value::Object(extra)) = (value.as_object_mut(), extra) {
        object.extend(extra);
    }
    value
}

fn montage_json(montage: &Montage) -> Value {
    json!({
        "name": montage.name,
        "rate": montage.rate,
        "slots": montage.tracks.iter().map(|t| t.slot.clone()).collect::<Vec<_>>(),
        "sections": (0..montage.section_count()).map(|i| json!({
            "name": montage.section_name(i),
            "start": montage.section_start(i),
            "next": match montage.section_next(i) {
                SectionNext::Continue => json!("continue"),
                SectionNext::Stop => json!("stop"),
                SectionNext::Jump(to) => json!({ "jump": to }),
            },
        })).collect::<Vec<_>>(),
        "notifies": montage.notifies.iter().map(|n| json!({
            "name": n.name,
            "time": n.time,
            "duration": n.duration,
            "branch": n.branch,
        })).collect::<Vec<_>>(),
        "warps": montage.warps.iter().map(|w| json!({
            "target": w.target,
            "start": w.start,
            "end": w.end,
            "translation": w.translation,
            "vertical": w.vertical,
            "rotation": w.rotation,
            "mode": format!("{:?}", w.mode),
            "rotation_kind": format!("{:?}", w.rotation_kind),
            "rotation_fraction": w.rotation_fraction,
            "anchor": format!("{:?}", w.anchor),
        })).collect::<Vec<_>>(),
    })
}

/// `anim.montage_state { entity }`.
pub fn montage_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let Some(player) = world.get::<MontagePlayer>(entity) else {
        return Ok(json!({ "entity": entity.to_bits(), "player": false }));
    };
    let slots: Vec<Value> = player
        .slots
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "group": s.group,
                "joints": s.mask.as_ref().map(|m| m.joints.clone()),
            })
        })
        .collect();
    let warp_targets: Vec<Value> = player
        .warp_targets()
        .iter()
        .map(|(name, t)| {
            json!({
                "name": name,
                "position": vec3_json(t.position),
                "facing_yaw": t.facing.map(yaw_of),
            })
        })
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "player": true,
        "slots": slots,
        "library": player.library.iter().map(|m| montage_json(m)).collect::<Vec<_>>(),
        "instances": player.instances().iter().map(status_json).collect::<Vec<_>>(),
        "queued": player.queued().iter().map(|id| id.0).collect::<Vec<_>>(),
        "warp_targets": warp_targets,
        "events": player.history().map(record_json).collect::<Vec<_>>(),
        // Load warnings of the library (spec 34 M1, ADR-7216): `montage_unknown_slot` per track.
        "load_issues": player.load_issues().iter().map(|i| json!({
            "code": i.code, "track": i.track, "detail": i.detail,
        })).collect::<Vec<_>>(),
    }))
}

/// `anim.montage_commands { entity, record? }`: the montage command hook (spec 34 § 9,
/// ADR-7216). Reads (and with `record` turns on or off) command recording, the player's command
/// clock, the received commands waiting for it, the last recorded commands, and the network counts
/// (`net`: sent, received, dropped, last applied stamp; ADR-9370).
pub fn montage_commands(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let record = match params.get("record") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(invalid("bad_bool", "`record` must be a boolean")),
    };
    let Some(mut player) = world.get_mut::<MontagePlayer>(entity) else {
        return Err(missing(
            "no_montage_player",
            format!("entity {} has no MontagePlayer", entity.to_bits()),
            "play a montage with anim.montage_play first",
        ));
    };
    if let Some(on) = record
        && player.is_recording() != on
    {
        player.set_recording(on);
    }
    let recent: Vec<Value> = player
        .recent_commands()
        .map(|c| {
            json!({
                "at": c.at,
                "command": c.command.as_str(),
                "detail": serde_json::to_value(&c.command).unwrap_or(Value::Null),
            })
        })
        .collect();
    // Network counts of the command stream (`regia::montage_net`, ADR-9370): zeros when the rig
    // is not networked.
    let net = player.net_state();
    Ok(json!({
        "entity": entity.to_bits(),
        "recording": player.is_recording(),
        "clock": player.clock(),
        "waiting": player.waiting_commands(),
        "recent": recent,
        "net": {
            "sent": net.sent,
            "received": net.received,
            "dropped": net.dropped,
            "last_applied": net.last_applied.map(|(tick, seq)| json!({ "tick": tick, "seq": seq })),
        },
    }))
}

pub(crate) fn finite_param(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_f64()
                .map(|n| n as f32)
                .filter(|n| n.is_finite())
                .ok_or_else(|| invalid("bad_number", format!("`{name}` must be a finite number")))?;
            Ok(Some(n))
        }
    }
}

pub(crate) fn string_param<'v>(params: &'v Value, name: &str) -> Result<Option<&'v str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(invalid("bad_string", format!("`{name}` must be a string"))),
    }
}

fn id_param(params: &Value) -> Result<Option<MontageId>, RpcError> {
    match params.get("id") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|id| Some(MontageId(id)))
            .ok_or_else(|| invalid("bad_id", "`id` must be a montage id from anim.montage_play")),
    }
}

/// Whether every clip of `montage` is in the player's or the animator's library.
fn missing_clip(
    montage: &Montage,
    player: Option<&MontagePlayer>,
    graph: Option<&ClipLibrary>,
) -> Option<String> {
    montage
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .map(|s| s.clip.as_str())
        .find(|clip| {
            player.is_none_or(|p| p.clips.get(clip).is_none()) && graph.is_none_or(|g| g.get(clip).is_none())
        })
        .map(str::to_string)
}

/// `anim.montage_play { entity, montage? | ron?, rate?, section?, start_time?, queue? }`.
pub fn montage_play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let name = string_param(params, "montage")?;
    let ron = string_param(params, "ron")?;
    let rate = finite_param(params, "rate")?.unwrap_or(1.0);
    if rate < 0.0 {
        return Err(invalid(
            "bad_rate",
            "`rate` must be >= 0 (play backward with `reverse: true`)",
        ));
    }
    let duration = finite_param(params, "duration")?;
    if duration.is_some_and(|d| d <= 0.0) {
        return Err(invalid(
            "bad_rate",
            "`duration` (time-stretch, wall seconds) must be > 0",
        ));
    }
    let reverse = match params.get("reverse") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("bad_bool", "`reverse` must be a boolean")),
    };
    let start_time = finite_param(params, "start_time")?.unwrap_or(0.0).max(0.0);
    let section = string_param(params, "section")?.map(str::to_string);
    let queue = match params.get("queue") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("bad_bool", "`queue` must be a boolean")),
    };
    let player = world.get::<MontagePlayer>(entity);
    let montage = match (name, ron) {
        (_, Some(text)) => Montage::from_ron_str(text)
            .map_err(|e| invalid("montage_invalid", format!("montage RON: {e}")))?,
        (Some(name), None) => player
            .and_then(|p| p.montage(name))
            .map(|m| (**m).clone())
            .ok_or_else(|| {
                missing(
                    "montage_unknown",
                    format!("the rig has no montage named {name:?}"),
                    "list the library with anim.montage_state, or pass `ron`",
                )
            })?,
        (None, None) => {
            return Err(invalid(
                "montage_required",
                "pass `montage` (a library name) or `ron`",
            ));
        }
    };
    if let Some(section) = &section
        && montage.section_index(section).is_none()
    {
        return Err(missing(
            "section_unknown",
            format!("montage {:?} has no section {section:?}", montage.name),
            "list sections with anim.montage_state",
        ));
    }
    let graph = world
        .get::<regia_animation::Animator>(entity)
        .and_then(|a| match &a.source {
            AnimatorSource::Graph { clips, .. } => Some(clips.as_ref()),
            AnimatorSource::SingleClip { .. } => None,
        });
    if let Some(clip) = missing_clip(&montage, player, graph) {
        return Err(missing(
            "montage_clip_missing",
            format!(
                "montage {:?} uses clip {clip:?}, which the rig has not loaded",
                montage.name
            ),
            "add the clip to the MontagePlayer's or the animator's clip library",
        ));
    }
    // Every check passed: only now create the player, so a rejected call leaves no trace.
    let created = player.is_none();
    if created {
        world.entity_mut(entity).insert(MontagePlayer::default());
    }
    let Some(mut player) = world.get_mut::<MontagePlayer>(entity) else {
        return Err(RpcError::internal("the montage player could not be added"));
    };
    let options = PlayOptions {
        rate,
        section,
        start_time,
        reverse,
        duration,
    };
    let montage = player.add_montage(montage);
    let id = if queue {
        player.queue(montage.clone(), options)
    } else {
        player.play(montage.clone(), options)
    };
    let queued = player.queued().contains(&id);
    Ok(json!({
        "entity": entity.to_bits(),
        "id": id.0,
        "montage": montage.name,
        "queued": queued,
        "created_player": created,
    }))
}

/// `anim.montage_stop { entity, id?, blend? }`.
pub fn montage_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    player_of(world, entity)?;
    let id = id_param(params)?;
    let blend = finite_param(params, "blend")?.map(|b| b.max(0.0));
    let Some(mut player) = world.get_mut::<MontagePlayer>(entity) else {
        return Err(RpcError::internal("montage player vanished"));
    };
    if let Some(id) = id
        && player.status(id).is_none()
        && !player.queued().contains(&id)
    {
        return Err(missing(
            "montage_not_playing",
            format!("montage id {} is not playing or queued", id.0),
            "list instances with anim.montage_state",
        ));
    }
    let stopped = player.stop(id, blend);
    Ok(json!({ "entity": entity.to_bits(), "stopped": stopped }))
}

/// `anim.montage_jump { entity, section, id?, next?, branch? }`: jump now, relink where `section`
/// goes at its end (`next`: `"continue"`, `"stop"` or a section name), or arm a jump to `section`
/// at branching point `branch` (applied at its exact time, once).
pub fn montage_jump(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    player_of(world, entity)?;
    let id = id_param(params)?;
    let section = string_param(params, "section")?
        .ok_or_else(|| invalid("section_required", "`section` is required"))?
        .to_string();
    let next = string_param(params, "next")?.map(|n| match n {
        "continue" => SectionNext::Continue,
        "stop" => SectionNext::Stop,
        other => SectionNext::Jump(other.to_string()),
    });
    let branch = string_param(params, "branch")?.map(str::to_string);
    if branch.is_some() && next.is_some() {
        return Err(invalid("branch_with_next", "pass `branch` or `next`, not both"));
    }
    let Some(mut player) = world.get_mut::<MontagePlayer>(entity) else {
        return Err(RpcError::internal("montage player vanished"));
    };
    if player.instances().is_empty() {
        return Err(missing(
            "no_montage_playing",
            "no montage is playing on this rig",
            "start one with anim.montage_play",
        ));
    }
    if let Some(id) = id
        && player.status(id).is_none()
    {
        return Err(missing(
            "montage_not_playing",
            format!("montage id {} is not playing", id.0),
            "list instances with anim.montage_state",
        ));
    }
    if let Some(branch) = &branch {
        if !player.on_branch(id, branch, BranchAction::JumpTo(section.clone())) {
            return Err(missing(
                "branch_unknown",
                format!("the montage has no branching point {branch:?} or no section {section:?}"),
                "list notifies (branch: true) and sections with anim.montage_state",
            ));
        }
        let status = id
            .and_then(|id| player.status(id))
            .or_else(|| player.instances().last().cloned());
        return Ok(json!({
            "entity": entity.to_bits(),
            "armed": branch,
            "instance": status.as_ref().map(status_json),
        }));
    }
    let done = match next {
        Some(next) => player.set_next_section(id, &section, next),
        None => player.jump_to_section(id, &section),
    };
    if !done {
        return Err(missing(
            "section_unknown",
            format!("the montage has no section {section:?} (or the link target is unknown)"),
            "list sections with anim.montage_state",
        ));
    }
    let status = id
        .and_then(|id| player.status(id))
        .or_else(|| player.instances().last().cloned());
    Ok(json!({
        "entity": entity.to_bits(),
        "instance": status.as_ref().map(status_json),
    }))
}

/// `anim.montage_set_warp_target { entity, name, position | null, facing_yaw? }`.
pub fn montage_set_warp_target(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    player_of(world, entity)?;
    let name = string_param(params, "name")?
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            invalid(
                "name_required",
                "`name` (the warp window's target name) is required",
            )
        })?
        .to_string();
    let target = match params.get("position") {
        None => {
            return Err(invalid(
                "position_required",
                "`position` is [x, y, z] or null (clears)",
            ));
        }
        Some(Value::Null) => {
            // A facing with a clear has nothing to apply to: refused, not silently dropped
            // (audit-gameplay-muse L5).
            if params.get("facing_yaw").is_some_and(|v| !v.is_null()) {
                return Err(invalid(
                    "warp_target_invalid",
                    "`facing_yaw` needs a `position`; `position: null` clears the target",
                ));
            }
            None
        }
        Some(Value::Array(items)) if items.len() == 3 => {
            let c: Vec<f32> = items
                .iter()
                .filter_map(Value::as_f64)
                .map(|v| v as f32)
                .filter(|v| v.is_finite())
                .collect();
            if c.len() != 3 {
                return Err(invalid(
                    "warp_target_invalid",
                    "`position` needs three finite numbers",
                ));
            }
            let facing = finite_param(params, "facing_yaw")?.map(Quat::from_rotation_y);
            Some(WarpTarget {
                position: Vec3::new(c[0], c[1], c[2]),
                facing,
            })
        }
        Some(_) => return Err(invalid("warp_target_invalid", "`position` is [x, y, z] or null")),
    };
    let Some(mut player) = world.get_mut::<MontagePlayer>(entity) else {
        return Err(RpcError::internal("montage player vanished"));
    };
    let before = player.warp_target(&name).map(|t| vec3_json(t.position));
    match target {
        Some(target) => player.set_warp_target(name.clone(), target),
        None => {
            // Clearing a target that is not set is a typo the caller must hear about, as with
            // `anim.scripted_warp clear_target` (audit-gameplay-muse L5).
            if !player.clear_warp_target(&name) {
                return Err(missing(
                    "warp_unknown_target",
                    format!("no montage warp target named {name:?}"),
                    "list them with anim.montage_state (warp_targets)",
                ));
            }
        }
    }
    let after = player.warp_target(&name).map(|t| vec3_json(t.position));
    Ok(json!({ "entity": entity.to_bits(), "name": name, "before": before, "after": after }))
}

/// `anim.sync_state { entity }`: declared groups and markers, and each playing group's leader.
pub fn sync_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let Some(animator) = world.get::<regia_animation::Animator>(entity) else {
        return Err(RpcError::internal("animator vanished"));
    };
    let declared = animator.sync_setup().map(|setup| {
        json!({
            "groups": setup.groups.iter().map(|g| g.name.clone()).collect::<Vec<_>>(),
            "marked_clips": setup.markers.iter().map(|m| json!({
                "clip": m.clip,
                "markers": m.markers.iter().map(|k| json!({ "name": k.name, "time": k.time })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    });
    let groups: Vec<Value> = animator
        .sync_groups()
        .iter()
        .map(|g| {
            json!({
                "name": g.name,
                "leader": g.leader.as_ref().map(|l| json!({
                    "clip": l.clip,
                    "time": l.time,
                    "speed": l.speed,
                    "looping": l.looping,
                })),
            })
        })
        .collect();
    Ok(json!({ "entity": entity.to_bits(), "setup": declared, "groups": groups }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_animation::components::spawn_skeleton;
    use regia_animation::pose::translated;
    use regia_animation::{AnimationClip, AnimationPlugin, Animator, Skeleton};
    use regia_scene::Transform;

    use super::*;

    fn rig_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(50),
        ));
        let skeleton = Arc::new(
            Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![translated(Vec3::ZERO)])
                .expect("skeleton"),
        );
        let rig = app.world_mut().spawn(Transform::default()).id();
        let binding = spawn_skeleton(app.world_mut(), skeleton);
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("swing", 1.0));
        let graph = regia_animation::AnimGraph {
            parameters: Vec::new(),
            root: regia_animation::AnimNode::Clip(regia_animation::graph::ClipNode {
                clip: "swing".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        app.world_mut()
            .entity_mut(rig)
            .insert((binding, Animator::graph(graph, clips)));
        (app, rig)
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("error_code")?.as_str()
    }

    const ATTACK: &str = r#"(format_version: 1, name: "attack", tracks: [(slot: "full_body", segments: [(clip: "swing")])],
        sections: [(name: "a", start: 0.0), (name: "b", start: 0.5, next: Stop)],
        notifies: [(name: "combo", time: 0.2, branch: true)],
        warps: [(target: "enemy", start: 0.1, end: 0.4)])"#;

    #[test]
    fn an_armed_branch_jumps_at_the_branching_point() {
        let (mut app, rig) = rig_app();
        let bits = rig.to_bits();
        montage_play(app.world_mut(), &json!({ "entity": bits, "ron": ATTACK })).expect("play");
        let armed = montage_jump(
            app.world_mut(),
            &json!({ "entity": bits, "section": "b", "branch": "combo" }),
        )
        .expect("arm");
        assert_eq!(armed["armed"], json!("combo"));
        assert_eq!(armed["instance"]["section"], json!("a"), "nothing jumps yet");
        for _ in 0..8 {
            app.update();
        }
        let state = montage_state(app.world_mut(), &json!({ "entity": bits })).expect("state");
        let kinds: Vec<&str> = state["events"]
            .as_array()
            .map(|e| e.iter().filter_map(|x| x["kind"].as_str()).collect())
            .unwrap_or_default();
        let at = kinds
            .iter()
            .position(|k| *k == "branch_point")
            .expect("branch point reached");
        assert_eq!(kinds.get(at + 1), Some(&"section"), "{kinds:?}");
        let both = montage_jump(
            app.world_mut(),
            &json!({ "entity": bits, "section": "b", "branch": "combo", "next": "stop" }),
        );
        assert!(both.is_err_and(|e| code(&e) == Some("branch_with_next")));
    }

    #[test]
    fn play_jump_warp_state_and_stop_round_trip() {
        let (mut app, rig) = rig_app();
        let world = app.world_mut();
        let bits = rig.to_bits();
        let played = montage_play(world, &json!({ "entity": bits, "ron": ATTACK })).expect("play");
        assert_eq!(played["created_player"], json!(true));
        let id = played["id"].as_u64().expect("id");
        montage_set_warp_target(
            world,
            &json!({ "entity": bits, "name": "enemy", "position": [1.0, 0.0, -2.0] }),
        )
        .expect("warp");
        let jumped = montage_jump(world, &json!({ "entity": bits, "section": "b" })).expect("jump");
        assert_eq!(jumped["instance"]["section"], json!("b"));
        app.update();
        let state = montage_state(app.world_mut(), &json!({ "entity": bits })).expect("state");
        assert_eq!(state["instances"][0]["id"].as_u64(), Some(id));
        assert_eq!(state["warp_targets"][0]["name"], json!("enemy"));
        let kinds: Vec<&str> = state["events"]
            .as_array()
            .map(|e| e.iter().filter_map(|x| x["kind"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(kinds, ["started", "section"]);
        let stopped = montage_stop(app.world_mut(), &json!({ "entity": bits, "id": id })).expect("stop");
        assert_eq!(stopped["stopped"], json!(1));
        let sync = sync_state(app.world_mut(), &json!({ "entity": bits })).expect("sync");
        assert_eq!(sync["setup"], Value::Null);
    }

    #[test]
    fn errors_carry_stable_codes_and_leave_no_trace() {
        let (mut app, rig) = rig_app();
        let world = app.world_mut();
        let bits = rig.to_bits();
        let bad = montage_play(world, &json!({ "entity": bits, "ron": "(nope" })).expect_err("bad ron");
        assert_eq!(code(&bad), Some("montage_invalid"));
        let unknown =
            montage_play(world, &json!({ "entity": bits, "montage": "nothing" })).expect_err("unknown");
        assert_eq!(code(&unknown), Some("montage_unknown"));
        let clipless = ATTACK.replace("\"swing\"", "\"absent\"");
        let missing_clip =
            montage_play(world, &json!({ "entity": bits, "ron": clipless })).expect_err("clip");
        assert_eq!(code(&missing_clip), Some("montage_clip_missing"));
        assert!(
            world.get::<MontagePlayer>(rig).is_none(),
            "rejected calls add no player"
        );
        let no_player = montage_stop(world, &json!({ "entity": bits })).expect_err("no player");
        assert_eq!(code(&no_player), Some("no_montage_player"));
        montage_play(world, &json!({ "entity": bits, "ron": ATTACK })).expect("play");
        let branch = montage_jump(
            world,
            &json!({ "entity": bits, "section": "b", "branch": "nope" }),
        )
        .expect_err("branch");
        assert_eq!(code(&branch), Some("branch_unknown"));
        let section = montage_jump(world, &json!({ "entity": bits, "section": "zzz" })).expect_err("section");
        assert_eq!(code(&section), Some("section_unknown"));
        let warp = montage_set_warp_target(
            world,
            &json!({ "entity": bits, "name": "enemy", "position": [1, "x", 2] }),
        )
        .expect_err("warp");
        assert_eq!(code(&warp), Some("warp_target_invalid"));
        let rate = montage_play(
            world,
            &json!({ "entity": bits, "montage": "attack", "rate": -1.0 }),
        )
        .expect_err("rate");
        assert_eq!(code(&rate), Some("bad_rate"));
    }

    /// The spec 34 § 9 command hook and the M1 load warning through the dispatcher (ADR-7216):
    /// `anim.montage_commands { record: true }` turns recording on, a stop through the protocol
    /// is recorded on the player's clock, and `anim.montage_state` lists the library's
    /// `montage_unknown_slot` load warnings.
    #[test]
    fn montage_commands_and_load_issues_through_the_dispatcher() {
        let (mut app, rig) = rig_app();
        let bits = rig.to_bits();
        let call = |app: &mut App, method: &str, params: Value| {
            crate::dispatch::call_in_process(app.world_mut(), method, params).expect("answers at once")
        };
        let no_player =
            call(&mut app, "anim.montage_commands", json!({ "entity": bits })).expect_err("no player");
        assert_eq!(code(&no_player), Some("no_montage_player"));
        call(
            &mut app,
            "anim.montage_play",
            json!({ "entity": bits, "ron": ATTACK }),
        )
        .expect("play");
        app.update();
        app.update();
        let on = call(
            &mut app,
            "anim.montage_commands",
            json!({ "entity": bits, "record": true }),
        )
        .expect("record");
        assert_eq!(on["recording"], json!(true));
        let clock = on["clock"].as_f64().expect("clock");
        assert!(clock > 0.0, "{on}");
        call(&mut app, "anim.montage_stop", json!({ "entity": bits })).expect("stop");
        app.update();
        let read = call(&mut app, "anim.montage_commands", json!({ "entity": bits })).expect("read");
        let recent = read["recent"].as_array().expect("recent");
        assert_eq!(recent.len(), 1, "{read}");
        assert_eq!(recent[0]["command"], json!("stop"));
        assert_eq!(recent[0]["at"].as_f64(), Some(clock));
        let bad = call(
            &mut app,
            "anim.montage_commands",
            json!({ "entity": bits, "record": "yes" }),
        )
        .expect_err("bad bool");
        assert_eq!(code(&bad), Some("bad_bool"));

        let mut player = regia_animation::MontagePlayer::new(
            vec![regia_animation::SlotDef::full_body("full_body", "body")],
            ClipLibrary::new(),
        );
        player.add_montage(regia_animation::Montage::single("stray", "tail", "swing"));
        app.world_mut().entity_mut(rig).insert(player);
        let state = call(&mut app, "anim.montage_state", json!({ "entity": bits })).expect("state");
        assert_eq!(
            state["load_issues"][0]["code"],
            json!("montage_unknown_slot"),
            "{state}"
        );
        assert_eq!(state["load_issues"][0]["track"], json!(0));
    }

    /// The network counts of the command stream through the dispatcher (ADR-9370): zeros on a
    /// rig that is not networked, then what the net layer counted on the player.
    #[test]
    fn montage_commands_report_the_network_counts() {
        let (mut app, rig) = rig_app();
        let bits = rig.to_bits();
        let call_ok = |app: &mut App, method: &str, params: Value| {
            crate::dispatch::call_in_process(app.world_mut(), method, params)
                .expect("answers at once")
                .expect("succeeds")
        };
        call_ok(
            &mut app,
            "anim.montage_play",
            json!({ "entity": bits, "ron": ATTACK }),
        );
        app.update();
        let read = call_ok(&mut app, "anim.montage_commands", json!({ "entity": bits }));
        assert_eq!(
            read["net"],
            json!({ "sent": 0, "received": 0, "dropped": 0, "last_applied": null }),
            "{read}"
        );
        {
            let mut player = app
                .world_mut()
                .get_mut::<regia_animation::MontagePlayer>(rig)
                .expect("player");
            let net = player.net_state_mut();
            assert!(net.accept(7, 0));
            assert!(net.accept(9, 1));
            assert!(!net.accept(9, 1), "a duplicate");
            assert!(!net.accept(8, 0), "a late one");
            let _ = net.stamp(9);
        }
        let read = call_ok(&mut app, "anim.montage_commands", json!({ "entity": bits }));
        assert_eq!(
            read["net"],
            json!({ "sent": 1, "received": 2, "dropped": 2, "last_applied": { "tick": 9, "seq": 1 } }),
            "{read}"
        );
    }

    /// Reverse and time-stretch through the protocol (ADR-1870): `anim.montage_play { reverse,
    /// duration }`, then `anim.montage_control` turns it around and stretches the rest.
    #[test]
    fn reverse_and_duration_through_play_and_control() {
        let (mut app, rig) = rig_app();
        let bits = rig.to_bits();
        let played = montage_play(
            app.world_mut(),
            &json!({ "entity": bits, "ron": ATTACK, "reverse": true, "duration": 0.5 }),
        )
        .expect("play");
        let id = played["id"].as_u64().expect("id");
        app.update();
        app.update();
        let state = montage_state(app.world_mut(), &json!({ "entity": bits })).expect("state");
        let instance = &state["instances"][0];
        assert_eq!(instance["reverse"], json!(true));
        // 1 s of montage in 0.5 s of wall time: rate 2.
        assert!(
            (instance["rate"].as_f64().unwrap_or_default() - 2.0).abs() < 1e-4,
            "{instance}"
        );
        let turned = crate::anim_montage_edit::montage_control(
            app.world_mut(),
            &json!({ "entity": bits, "id": id, "reverse": false, "duration": 2.0 }),
        )
        .expect("control");
        assert_eq!(turned["instance"]["reverse"], json!(false));
        for (params, expected) in [
            (json!({ "entity": bits }), "montage_bad_edit"),
            (json!({ "entity": bits, "duration": 0.0 }), "bad_rate"),
            (json!({ "entity": bits, "reverse": "yes" }), "bad_bool"),
            (
                json!({ "entity": bits, "id": 999, "reverse": true }),
                "montage_not_playing",
            ),
        ] {
            let err =
                crate::anim_montage_edit::montage_control(app.world_mut(), &params).expect_err("refused");
            assert_eq!(code(&err), Some(expected), "{params}");
        }
        let bad = montage_play(
            app.world_mut(),
            &json!({ "entity": bits, "ron": ATTACK, "duration": -1.0 }),
        )
        .expect_err("duration");
        assert_eq!(code(&bad), Some("bad_rate"));
    }

    /// Clearing a warp target that is not set is refused with `warp_unknown_target` (as
    /// `anim.scripted_warp clear_target` does), so a typo is not reported as a successful clear;
    /// a `facing_yaw` next to `position: null` is refused instead of silently ignored
    /// (audit-gameplay-muse L5).
    #[test]
    fn clearing_an_unset_warp_target_is_refused() {
        let (mut app, rig) = rig_app();
        let world = app.world_mut();
        let bits = rig.to_bits();
        montage_play(world, &json!({ "entity": bits, "ron": ATTACK })).expect("play");
        montage_set_warp_target(
            world,
            &json!({ "entity": bits, "name": "enemy", "position": [1.0, 0.0, -2.0] }),
        )
        .expect("set");
        let typo = montage_set_warp_target(
            world,
            &json!({ "entity": bits, "name": "enemyy", "position": null }),
        )
        .expect_err("clearing an unset target");
        assert_eq!(code(&typo), Some("warp_unknown_target"));
        let facing = montage_set_warp_target(
            world,
            &json!({ "entity": bits, "name": "enemy", "position": null, "facing_yaw": 1.0 }),
        )
        .expect_err("facing with a clear");
        assert_eq!(code(&facing), Some("warp_target_invalid"));
        let player = world.get::<MontagePlayer>(rig).expect("player");
        assert!(player.warp_target("enemy").is_some(), "the real target stays set");
        let cleared = montage_set_warp_target(
            world,
            &json!({ "entity": bits, "name": "enemy", "position": null }),
        )
        .expect("clear");
        assert_eq!(cleared["after"], Value::Null);
    }
}
