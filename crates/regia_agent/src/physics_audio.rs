// SPDX-License-Identifier: MPL-2.0
//! Contact sounds on the agent protocol (`book/agents/physics-audio.md`, ADR-3270): the impact
//! bank and the live pair voices (`audio.impacts`), an offline probe of the curves
//! (`audio.impact_probe`) and replacing the bank (`audio.impacts_set`).

use regia_app::prelude::*;
use regia_audio::{Audio, ImpactBank, ImpactBankDef, ImpactContact, ImpactPlay, SoundAssets};
use regia_core::math::Vec3;
use regia_framework::physics_audio::{PairVoice, PhysicsAudioState};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

fn require_audio(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<Audio>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "no audio in this app",
            "add regia_audio::AudioPlugin (regia::DefaultPlugins includes it)",
        ));
    }
    Ok(())
}

fn impacts_error(code: &'static str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, code, message, hint)
}

fn obj(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    match params {
        Value::Null => Ok(EMPTY.get_or_init(Map::new)),
        Value::Object(map) => Ok(map),
        _ => Err(RpcError::invalid_params("params must be an object")),
    }
}

fn num(p: &Map<String, Value>, field: &str, range: (f32, f32)) -> Result<Option<f32>, RpcError> {
    let Some(value) = p.get(field) else {
        return Ok(None);
    };
    let v = value.as_f64().map(|v| v as f32).filter(|v| v.is_finite());
    match v {
        Some(v) if (range.0..=range.1).contains(&v) => Ok(Some(v)),
        _ => Err(impacts_error(
            "impacts_bad_param",
            format!("`{field}` must be a number in {}..={}", range.0, range.1),
            "see book/agents/physics-audio.md for the ranges",
        )),
    }
}

fn text<'a>(p: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>, RpcError> {
    match p.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(impacts_error(
            "impacts_bad_param",
            format!("`{field}` must be a string"),
            "see book/agents/physics-audio.md",
        )),
    }
}

fn bank_error(error: &regia_audio::ImpactBankError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        error.code(),
        error.to_string(),
        "see docs/formats/impactbank.md for the fields and ranges",
    )
}

fn play_json(play: &ImpactPlay) -> Value {
    json!({
        "kind": play.kind,
        "body": play.body,
        "materials": [play.materials.0, play.materials.1],
        "source": play.source.as_str(),
        "variant": play.variant,
        "sound": play.sound,
        "curve_db": play.curve_db,
        "volume_db": play.volume_db,
        "pitch": play.pitch,
        "impulse_ns": play.impulse_ns,
        "normal_speed_m_s": play.normal_speed_m_s,
        "position": [play.position.x, play.position.y, play.position.z],
        "started": play.started,
    })
}

fn bank_json(bank: &ImpactBank, sounds: Option<&SoundAssets>) -> Value {
    let def = bank.def();
    let missing = |list: &[String]| -> Vec<String> {
        list.iter()
            .filter(|s| sounds.is_none_or(|a| a.by_name(s).is_none()))
            .cloned()
            .collect()
    };
    let set_json = |set: &regia_audio::ImpactSet| {
        let mut all = set.impacts.clone();
        all.extend(set.breaks.iter().cloned());
        all.extend(set.scrape.iter().cloned());
        all.extend(set.roll.iter().cloned());
        json!({
            "impacts": set.impacts,
            "scrape": set.scrape,
            "roll": set.roll,
            "breaks": set.breaks,
            "volume_db": set.volume_db,
            "pitch": set.pitch,
            "missing_sounds": missing(&all),
        })
    };
    let materials: Map<String, Value> = def
        .materials
        .iter()
        .map(|(name, set)| (name.clone(), set_json(set)))
        .collect();
    let pairs: Vec<Value> = def
        .pairs
        .iter()
        .map(|p| json!({ "a": p.a, "b": p.b, "set": set_json(&p.set) }))
        .collect();
    json!({
        "format_version": def.format_version,
        "materials": materials,
        "pairs": pairs,
        "default_material": def.default_material,
        "destruction_materials": def.destruction_materials,
        "curves": serde_json::to_value(&def.curves).unwrap_or(Value::Null),
        "budget": serde_json::to_value(&def.budget).unwrap_or(Value::Null),
        "bus": def.bus,
        "max_distance_m": def.max_distance_m,
        "played": bank.played(),
        "breaks_played": bank.breaks_played(),
        "recent": bank.recent().map(play_json).collect::<Vec<_>>(),
    })
}

fn pair_json(pair: &PairVoice) -> Value {
    json!({
        "a": pair.a.to_bits(),
        "b": pair.b.to_bits(),
        "kind": pair.kind.as_str(),
        "level_db": pair.level_db,
        "gain": pair.gain,
        "pitch": pair.pitch,
        "last_active_step": pair.last_active_step,
        "position": [pair.position.x, pair.position.y, pair.position.z],
        "sound": pair.sound,
        "voice": pair.voice.is_some(),
        "voiced": pair.voiced,
    })
}

fn state_json(state: &PhysicsAudioState) -> Value {
    let s = &state.stats;
    json!({
        "step": state.step(),
        "contacts": s.contacts,
        "unsampled": s.unsampled,
        "impacts": s.impacts,
        "dropped_by_budget": s.dropped_by_budget,
        "dropped_last_step": s.dropped_last_step,
        "max_impacts_in_a_step": s.max_impacts_in_a_step,
        "cooled_down": s.cooled_down,
        "loops_started": s.loops_started,
        "loops_stopped": s.loops_stopped,
        "loops_over_budget": s.loops_over_budget,
        "breaks": s.breaks,
        "pairs": state.pairs().iter().map(pair_json).collect::<Vec<_>>(),
    })
}

/// `audio.impacts`: `{ bank, state }` (`bank` null until `audio.impacts_set`; `state` null without
/// `PhysicsAudioPlugin`).
pub(crate) fn impacts(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let bank = world
        .get_resource::<ImpactBank>()
        .map(|b| bank_json(b, world.get_resource::<SoundAssets>()));
    let state = world.get_resource::<PhysicsAudioState>().map(state_json);
    Ok(json!({ "bank": bank, "state": state }))
}

/// `audio.impact_probe { a, b, impulse_ns?, normal_speed_m_s?, slip_m_s?, rolling_speed_m_s?,
/// normal_force_n?, seed?, ron? }`: what the bank (or the given `.impactbank.ron` text) would do
/// for one contact, on a copy (nothing plays and the live bank's history does not move).
pub(crate) fn impact_probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let p = obj(params)?;
    let mut bank = match text(p, "ron")? {
        Some(ron) => ImpactBank::from_ron(ron).map_err(|e| bank_error(&e))?,
        None => world.get_resource::<ImpactBank>().cloned().ok_or_else(|| {
            impacts_error(
                "impacts_no_bank",
                "no ImpactBank in this app and no `ron` given",
                "set one with audio.impacts_set { ron } or pass `ron` (the text of a .impactbank.ron)",
            )
        })?,
    };
    let a = text(p, "a")?.unwrap_or("");
    let b = text(p, "b")?.unwrap_or("");
    let impulse = num(p, "impulse_ns", (0.0, 1.0e6))?.unwrap_or(0.0);
    let speed = num(p, "normal_speed_m_s", (0.0, 1.0e4))?.unwrap_or(0.0);
    let slip = num(p, "slip_m_s", (0.0, 1.0e4))?.unwrap_or(0.0);
    let rolling = num(p, "rolling_speed_m_s", (0.0, 1.0e4))?.unwrap_or(0.0);
    let force = num(p, "normal_force_n", (0.0, 1.0e9))?.unwrap_or(0.0);
    if let Some(seed) = p.get("seed") {
        let seed = seed.as_u64().ok_or_else(|| {
            impacts_error(
                "impacts_bad_param",
                "`seed` must be a non-negative integer",
                "see book/agents/physics-audio.md",
            )
        })?;
        bank.reseed(seed);
    }
    let Some((_, source, _)) = bank.resolve(a, b) else {
        return Err(impacts_error(
            "impacts_no_set",
            format!("no set for `{a}` + `{b}` and the bank has no default material set"),
            "add a set for one of the materials or for default_material",
        ));
    };
    let source = source.as_str();
    let impact = bank
        .plan_impact(&ImpactContact {
            body: 0,
            material_a: a,
            material_b: b,
            impulse_ns: impulse,
            normal_speed_m_s: speed,
            position: Vec3::ZERO,
        })
        .map(|play| play_json(&play));
    let contact_loop = bank.contact_loop(slip, rolling, force).map(|level| {
        json!({
            "kind": level.kind.as_str(),
            "level_db": level.db,
            "pitch": level.pitch,
            "sound": bank.loop_sound(a, b, level.kind).map(|(sound, _, _)| sound),
        })
    });
    Ok(json!({
        "source": source,
        "impact_db": bank.impact_db(impulse, speed),
        "impact_pitch": bank.impact_pitch(speed),
        "impact": impact,
        "loop": contact_loop,
    }))
}

/// `audio.impacts_set { enabled?, ron?, seed? }` → `{ before, after }`: replaces the bank with
/// the given `.impactbank.ron` text (validated), reseeds it, or removes it.
pub(crate) fn impacts_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let p = obj(params)?;
    let before = world
        .get_resource::<ImpactBank>()
        .map(|b| bank_json(b, world.get_resource::<SoundAssets>()));
    let enabled = crate::params::opt_bool(p, "enabled").map_err(|_| {
        impacts_error(
            "impacts_bad_param",
            "`enabled` must be a boolean",
            "see book/agents/physics-audio.md",
        )
    })?;
    // Everything supplied is validated before anything is applied, so a malformed `ron` is
    // refused even next to `enabled: false`.
    let def = match text(p, "ron")? {
        Some(ron) => Some(ImpactBankDef::from_ron(ron).map_err(|e| bank_error(&e))?),
        None => None,
    };
    let seed = match p.get("seed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            impacts_error(
                "impacts_bad_param",
                "`seed` must be a non-negative integer",
                "see book/agents/physics-audio.md",
            )
        })?),
    };
    if enabled == Some(false) {
        world.remove_resource::<ImpactBank>();
        return Ok(json!({ "before": before, "after": null }));
    }
    // A seed alone cannot make a bank (ADR-5597): it used to install an empty default bank and
    // report success.
    if def.is_none() && world.get_resource::<ImpactBank>().is_none() {
        return Err(impacts_error(
            "impacts_no_bank",
            "no ImpactBank in this app and no `ron` given",
            "pass `ron` (the text of a .impactbank.ron)",
        ));
    }
    let mut bank = world.remove_resource::<ImpactBank>().unwrap_or_default();
    if let Some(def) = def {
        bank.set_def(def);
    }
    if let Some(seed) = seed {
        bank.reseed(seed);
    }
    world.insert_resource(bank);
    let after = world
        .get_resource::<ImpactBank>()
        .map(|b| bank_json(b, world.get_resource::<SoundAssets>()));
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings};

    fn world() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        app
    }

    const BANK: &str = r#"(format_version: 1, materials: {
        "wood": (impacts: ["wood_1", "wood_2", "wood_3"], scrape: Some("wood_scrape"), roll: Some("wood_roll")),
        "default": (impacts: ["thud_1", "thud_2"]),
    })"#;

    #[test]
    fn set_probe_and_report() {
        let mut app = world();
        let w = app.world_mut();
        let none = impacts(w).expect("report");
        assert!(none["bank"].is_null());
        let set = impacts_set(w, &json!({ "ron": BANK, "seed": 9 })).expect("set");
        assert!(set["before"].is_null());
        assert_eq!(set["after"]["materials"]["wood"]["impacts"][0], "wood_1");
        let probe = impact_probe(
            w,
            &json!({ "a": "wood", "b": "stone", "impulse_ns": 10.0, "normal_speed_m_s": 5.0 }),
        )
        .expect("probe");
        assert_eq!(probe["source"], "first");
        assert_eq!(probe["impact_db"], 0.0);
        assert!(
            probe["impact"]["sound"]
                .as_str()
                .is_some_and(|s| s.starts_with("wood_"))
        );
        let rolling = impact_probe(
            w,
            &json!({ "a": "wood", "b": "stone", "slip_m_s": 0.0, "rolling_speed_m_s": 2.0 }),
        )
        .expect("probe");
        assert_eq!(rolling["loop"]["kind"], "roll");
        assert_eq!(rolling["loop"]["sound"], "wood_roll");
        assert!(rolling["impact"].is_null(), "no impulse, no impact");
        // The probe works on a copy: the live bank played nothing.
        assert_eq!(impacts(w).expect("report")["bank"]["played"], 0);
    }

    #[test]
    fn errors_have_stable_codes() {
        let mut app = world();
        let w = app.world_mut();
        let code = |r: Result<Value, RpcError>| {
            let error = r.expect_err("an error");
            error
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(code(impact_probe(w, &json!({ "a": "wood" }))), "impacts_no_bank");
        assert_eq!(code(impacts_set(w, &json!({}))), "impacts_no_bank");
        // corr-agent (ADR-5597): a seed alone installed an empty default bank.
        assert_eq!(code(impacts_set(w, &json!({ "seed": 5 }))), "impacts_no_bank");
        assert!(
            !w.contains_resource::<ImpactBank>(),
            "a refused seed installs nothing"
        );
        assert_eq!(code(impacts_set(w, &json!({ "ron": "((" }))), "impactbank_parse");
        assert_eq!(
            code(impacts_set(w, &json!({ "ron": "(format_version: 7)" }))),
            "impactbank_version"
        );
        impacts_set(w, &json!({ "ron": "(format_version: 1)" })).expect("an empty bank");
        for enabled in [json!(0), json!("false"), json!([]), json!({})] {
            assert_eq!(
                code(impacts_set(w, &json!({ "enabled": enabled }))),
                "impacts_bad_param"
            );
            assert!(w.contains_resource::<ImpactBank>());
        }
        assert_eq!(
            code(impact_probe(w, &json!({ "a": "x", "b": "y" }))),
            "impacts_no_set"
        );
        assert_eq!(
            code(impact_probe(w, &json!({ "ron": BANK, "impulse_ns": -1.0 }))),
            "impacts_bad_param"
        );
        let removed = impacts_set(w, &json!({ "enabled": false })).expect("removed");
        assert!(removed["after"].is_null());
    }

    #[test]
    fn seed_without_a_bank_is_refused_and_params_validate_before_removal() {
        let mut app = world();
        let w = app.world_mut();
        let code = |r: Result<Value, RpcError>| {
            let error = r.expect_err("an error");
            error
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        // A seed-only call with no bank would silently install an empty default bank.
        assert_eq!(code(impacts_set(w, &json!({ "seed": 3 }))), "impacts_no_bank");
        assert!(w.get_resource::<ImpactBank>().is_none());
        // A malformed `ron` is refused even next to `enabled: false` (no silent removal).
        assert_eq!(
            code(impacts_set(w, &json!({ "enabled": false, "ron": "((" }))),
            "impactbank_parse"
        );
        assert!(w.get_resource::<ImpactBank>().is_none());
        // Once a bank exists, a seed-only call reseeds it.
        impacts_set(w, &json!({ "ron": BANK })).expect("set");
        let reseeded = impacts_set(w, &json!({ "seed": 3 })).expect("reseed");
        assert_eq!(reseeded["after"]["materials"]["wood"]["impacts"][0], "wood_1");
    }
}
