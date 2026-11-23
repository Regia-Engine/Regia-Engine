// SPDX-License-Identifier: MPL-2.0
//! `rules.ruleset_*`: the rule sets an app has loaded (`.rules.ron` v1, ADR-8694,
//! `book/agents/match-rules.md` § Rule sets), and the `ruleset` parameter of `rules.match_setup`.

use std::collections::BTreeMap;

use regia_app::prelude::World;
use regia_framework::rules::ruleset::{ResolvedRules, RuleSetError};
use regia_framework::ruleset_file::{RuleSetRegistry, parse_rules_ron};
use serde_json::{Value, json};

use crate::params::check_known_and_types;
use crate::protocol::{self, RpcError};

/// A rule-set error as an agent error: the code is `data.code` and `data.reason`, the field path is
/// `data.path`.
pub(crate) fn rpc(error: &RuleSetError) -> RpcError {
    let mut out = RpcError::new(
        protocol::INVALID_PARAMS,
        &error.code,
        error.to_string(),
        "book/agents/match-rules.md (Rule sets) lists the codes; rules.ruleset_list shows what is loaded",
    );
    if let Some(Value::Object(data)) = out.data.as_mut() {
        data.insert("reason".into(), json!(error.code));
        data.insert("path".into(), json!(error.path));
    }
    out
}

fn registry(world: &World) -> RuleSetRegistry {
    world
        .get_resource::<RuleSetRegistry>()
        .cloned()
        .unwrap_or_default()
}

/// `options`: an object of string values (the playlist row's shape). Numbers are refused so a float
/// is never read through JSON's `f64`.
pub(crate) fn row_options(params: &Value) -> Result<BTreeMap<String, String>, RpcError> {
    let Some(value) = params.get("options").filter(|v| !v.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let Value::Object(map) = value else {
        return Err(RpcError::wrong_type("options", "an object of strings"));
    };
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let Value::String(text) = value else {
            return Err(RpcError::invalid_params(format!(
                "`options.{key}` must be a string (write numbers as text: \"3\", \"0.25\")"
            )));
        };
        out.insert(key.clone(), text.clone());
    }
    Ok(out)
}

/// The resolved rules as JSON (the content hash as 16 hex digits).
pub(crate) fn resolved_json(rules: &ResolvedRules) -> Result<Value, RpcError> {
    let ser = |value: Result<Value, serde_json::Error>| {
        value.map_err(|e| RpcError::internal(format!("serializing the rule set: {e}")))
    };
    Ok(json!({
        "id": rules.id,
        "name_key": rules.name_key,
        "chain": rules.chain,
        "mode": rules.mode.as_lobby_str(),
        "settings": ser(serde_json::to_value(&rules.settings))?,
        "zones": ser(serde_json::to_value(&rules.zones))?,
        "win_conditions": ser(serde_json::to_value(&rules.win_conditions))?,
        "schema": ser(serde_json::to_value(rules.schema.values().collect::<Vec<_>>()))?,
        "options": ser(serde_json::to_value(&rules.options))?,
        "content_hash": format!("{:016x}", rules.content_hash()),
    }))
}

/// `rules.ruleset_list`: every loaded rule set in id order with its skeleton, parent, source and
/// content hash (resolved without row options), plus the files that did not load.
pub fn ruleset_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.ruleset_list", params, &[])?;
    let registry = registry(world);
    let rows: Vec<Value> = registry
        .library()
        .iter()
        .map(|set| {
            let source = registry.source(&set.id);
            let resolved = registry.resolve(&set.id, &BTreeMap::new());
            json!({
                "id": set.id,
                "name_key": set.name_key,
                "skeleton": resolved.as_ref().ok().map(|r| r.mode.as_lobby_str()),
                "extends": set.extends,
                "builtin": source.is_some_and(|s| s.builtin),
                "path": source.map(|s| s.path.clone()),
                "options": set.options.iter().map(|o| o.key.clone()).collect::<Vec<_>>(),
                "content_hash": resolved.as_ref().ok().map(|r| format!("{:016x}", r.content_hash())),
                "error": resolved.err().map(|e| json!({ "code": e.code, "path": e.path, "message": e.message })),
            })
        })
        .collect();
    let errors: Vec<Value> = registry
        .errors()
        .iter()
        .map(|(file, e)| json!({ "file": file, "code": e.code, "path": e.path, "message": e.message }))
        .collect();
    Ok(json!({ "rulesets": rows, "errors": errors }))
}

/// `rules.ruleset_get { id, options? }`: the rule set as written and what it resolves to with
/// `options` (a playlist row's string options). A bare skeleton id answers its Rust defaults with
/// `ruleset: null`.
pub fn ruleset_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.ruleset_get", params, &[])?;
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`id` (a rule-set id) is required"))?;
    let options = row_options(params)?;
    let registry = registry(world);
    let resolved = registry.resolve(id, &options).map_err(|e| rpc(&e))?;
    let set = match registry.library().get(id) {
        Some(set) => {
            serde_json::to_value(set).map_err(|e| RpcError::internal(format!("serializing: {e}")))?
        }
        None => Value::Null,
    };
    Ok(json!({ "ruleset": set, "resolved": resolved_json(&resolved)? }))
}

/// `rules.ruleset_validate { text, options?, load? }`: parse and check a `.rules.ron` text and
/// resolve it against the loaded rule sets (its parent must be loaded). `load: true` also adds it
/// to the app's registry (replacing a rule set of the same id).
pub fn ruleset_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.ruleset_validate", params, &[])?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`text` (the .rules.ron text) is required"))?;
    let options = row_options(params)?;
    let load = crate::params::opt_bool(params, "load")?.unwrap_or(false);
    let set = parse_rules_ron(text).map_err(|e| rpc(&e))?;
    let mut library = registry(world).library().clone();
    library.replace(set.clone()).map_err(|e| rpc(&e))?;
    let resolved = library.resolve(&set.id, &options).map_err(|e| rpc(&e))?;
    if load {
        world
            .get_resource_or_insert_with(RuleSetRegistry::default)
            .replace_text(&format!("agent:{}", set.id), text)
            .map_err(|e| rpc(&e))?;
    }
    Ok(json!({ "ok": true, "id": set.id, "loaded": load, "resolved": resolved_json(&resolved)? }))
}

/// The `ruleset` parameter of `rules.match_setup`: resolve it with `options` (and the skeleton of
/// each rule set the playlist rows name). `None` when the call names no rule set.
pub(crate) fn setup_rules(
    world: &World,
    params: &Value,
    rows: &[regia_framework::rules::playlist::PlaylistEntry],
) -> Result<
    (
        Option<ResolvedRules>,
        BTreeMap<String, regia_framework::rules::mode::MatchMode>,
    ),
    RpcError,
> {
    let registry = registry(world);
    let modes = registry.library().rule_modes(rows).map_err(|e| rpc(&e))?;
    let Some(id) = params.get("ruleset").filter(|v| !v.is_null()) else {
        if params.get("options").is_some_and(|v| !v.is_null()) {
            return Err(RpcError::invalid_params("`options` needs `ruleset`"));
        }
        return Ok((None, modes));
    };
    let id = id
        .as_str()
        .ok_or_else(|| RpcError::wrong_type("ruleset", "a string"))?;
    for key in ["settings", "phases"] {
        if params.get(key).is_some_and(|v| !v.is_null()) {
            return Err(RpcError::invalid_params(format!(
                "`ruleset` and `{key}` exclude each other (extend the rule set instead)"
            )));
        }
    }
    let options = row_options(params)?;
    let resolved = registry.resolve(id, &options).map_err(|e| rpc(&e))?;
    Ok((Some(resolved), modes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    fn path(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("path")?.as_str()
    }

    const LONG: &str = r#"(format_version: 1, id: "ctf_long", extends: "ctf",
        session: (objectives: (capture_limit: 5)),
        options: [(key: "sky", kind: enum, default: "day", choices: ["day", "night"])])"#;

    #[test]
    fn ruleset_methods_list_get_validate_and_set_up_a_match() {
        let mut world = World::new();
        let list = ruleset_list(&mut world, &json!({})).expect("list");
        let ids: Vec<&str> = list["rulesets"]
            .as_array()
            .expect("rulesets")
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, ["ctf", "dom", "ffa", "koth", "tdm"]);
        assert!(list["errors"].as_array().is_some_and(Vec::is_empty));

        let ctf = ruleset_get(&mut world, &json!({ "id": "ctf" })).expect("get");
        assert_eq!(ctf["resolved"]["mode"], json!("ctf"));
        assert_eq!(
            ctf["resolved"]["content_hash"],
            list["rulesets"][0]["content_hash"]
        );
        let bare = ruleset_get(&mut world, &json!({ "id": "custom" })).expect("bare skeleton");
        assert!(bare["ruleset"].is_null());

        // Validate without loading, then with.
        let ok = ruleset_validate(
            &mut world,
            &json!({ "text": LONG, "options": { "sky": "night" } }),
        )
        .expect("ok");
        assert_eq!(
            ok["resolved"]["settings"]["objectives"]["capture_limit"],
            json!(5)
        );
        assert_eq!(ok["resolved"]["options"]["sky"], json!({ "enum": "night" }));
        let err = ruleset_get(&mut world, &json!({ "id": "ctf_long" })).expect_err("not loaded");
        assert_eq!(code(&err), Some("ruleset_unknown"));
        ruleset_validate(&mut world, &json!({ "text": LONG, "load": true })).expect("load");
        // A second load of the same id replaces the first.
        ruleset_validate(&mut world, &json!({ "text": LONG, "load": true })).expect("reload");

        // Bad options and bad text name their code and path.
        let err = ruleset_get(
            &mut world,
            &json!({ "id": "ctf_long", "options": { "sky": "dusk" } }),
        )
        .expect_err("choice");
        assert_eq!(
            (code(&err), path(&err)),
            (Some("ruleset_option_kind"), Some("options.sky"))
        );
        crate::contract::assert_listed("rules.ruleset_get", "ruleset_option_kind");
        let err = ruleset_validate(
            &mut world,
            &json!({ "text": "(format_version: 1, id: \"x\", skeleton: \"ctf\", sesion: ())" }),
        )
        .expect_err("typo");
        assert_eq!(code(&err), Some("ruleset_parse"));
        crate::contract::assert_listed("rules.ruleset_validate", "ruleset_parse");
        let err =
            ruleset_get(&mut world, &json!({ "id": "ctf", "options": { "n": 3 } })).expect_err("a number");
        assert_eq!(code(&err), Some("F0002"));

        // match_setup { ruleset, options } and a row naming the rule set.
        let out = crate::match_rules::match_setup(
            &mut world,
            &json!({
                "ruleset": "ctf_long",
                "options": { "sky": "night" },
                "playlist": [{ "map_id": "harbor", "mode": "ctf_long" }],
                "start": true,
            }),
        )
        .expect("setup");
        assert_eq!(
            out["mode"],
            json!("ctf"),
            "the row launched on the rule set's skeleton"
        );
        assert_eq!(out["board"]["settings"]["capture_limit"], json!(5));
        assert_eq!(out["ruleset"]["chain"], json!(["ctf", "ctf_long"]));
        assert_eq!(out["rule_modes"], json!({ "ctf_long": "capture_the_flag" }));
        let err = crate::match_rules::match_setup(&mut world, &json!({ "ruleset": "ctf", "settings": {} }))
            .expect_err("exclusive");
        assert_eq!(code(&err), Some("F0002"));
        let err = crate::match_rules::match_setup(
            &mut world,
            &json!({ "ruleset": "ctf_long", "options": { "wind": "1" } }),
        )
        .expect_err("unknown option");
        assert_eq!(
            (code(&err), path(&err)),
            (Some("ruleset_option_unknown"), Some("options.wind"))
        );
        crate::contract::assert_listed("rules.match_setup", "ruleset_option_unknown");
        // Without a ruleset the setup answer keeps its shape.
        let plain = crate::match_rules::match_setup(&mut world, &json!({})).expect("plain");
        assert!(plain.get("ruleset").is_none() && plain.get("rule_modes").is_none());
    }

    /// The genre sections (ADR-8748) reach the agent: `rules.ruleset_validate` shows them resolved,
    /// `rules.match_setup { ruleset }` starts a session with them, and a bad section names its
    /// indexed path.
    #[test]
    fn genre_sections_reach_the_agent_and_name_their_path() {
        use regia_framework::rules::MatchRules;
        let mut world = World::new();
        let br = r#"(format_version: 1, id: "br", skeleton: "custom",
            session: (flow: (max_teams: 4, seats_per_team: 1), objectives: (max_teams: 4),
                respawn: (default: (kind: "never")),
                zone: (radius: 100.0, phases: [(wait_ticks: 10, shrink_ticks: 100, end_radius: 10.0, damage_per_tick: 3)], seed: 42),
                elimination: (last_standing: true),
                victory: (conditions: [(kind: "surrender")], surrender: (threshold_percent: 50))))"#;
        let ok = ruleset_validate(&mut world, &json!({ "text": br, "load": true })).expect("ok");
        let settings = &ok["resolved"]["settings"];
        assert_eq!(settings["zone"]["seed"], json!(42));
        assert_eq!(settings["zone"]["phases"][0]["damage_per_tick"], json!(3));
        assert_eq!(settings["elimination"]["last_standing"], json!(true));
        assert_eq!(
            settings["victory"]["conditions"],
            json!([{ "kind": "surrender" }])
        );
        assert_eq!(settings["victory"]["surrender"]["threshold_percent"], json!(50));
        let bare = ruleset_get(&mut world, &json!({ "id": "custom" })).expect("bare");
        assert_ne!(
            ok["resolved"]["content_hash"], bare["resolved"]["content_hash"],
            "the sections are in the rules hash"
        );

        crate::match_rules::match_setup(&mut world, &json!({ "ruleset": "br", "start": true }))
            .expect("setup");
        let rules = world.get_resource::<MatchRules>().expect("session");
        assert_eq!(rules.session.zone.as_ref().map(|z| z.seed), Some(42));
        assert!(rules.session.elimination.last_standing);
        assert!(rules.session.victory.has("surrender"));

        let race = "(format_version: 1, id: \"r\", skeleton: \"custom\", session: (race: (checkpoints: [\
            (id: 1, volume: (min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0)), next: [2]), \
            (id: 2, volume: (min: (5.0, 0.0, 0.0), max: (6.0, 1.0, 1.0)), next: [3])])))";
        let err = ruleset_validate(&mut world, &json!({ "text": race })).expect_err("next");
        assert_eq!(
            (code(&err), path(&err)),
            (
                Some("ruleset_invalid"),
                Some("r: session.race.checkpoints[1].next[0]")
            )
        );
        crate::contract::assert_listed("rules.ruleset_validate", "ruleset_invalid");
        let typo = "(format_version: 1, id: \"w\", skeleton: \"custom\", session: (waves: (waves: [(groups: [(archetype: \"a\", cnt: 1)])])))";
        let err = ruleset_validate(&mut world, &json!({ "text": typo })).expect_err("typo");
        assert_eq!(code(&err), Some("ruleset_parse"));
        assert!(err.message.contains("cnt"), "{}", err.message);
    }
}
