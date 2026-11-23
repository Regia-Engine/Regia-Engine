// SPDX-License-Identifier: MPL-2.0
//! Threat-model finding 26 (ADR-3285): `audio.loopgen_render` was a `read` method that writes a WAV
//! into `out_dir`, and `profiler.set_capture` changed state under `read`, so a `read_only = true`
//! client could do both. The first test is the threat-model reproducer, un-ignored; the audit runs
//! the rule of `book/agents/protocol.md` ("Permission classes") over the whole schema.

use regia_agent::AgentPermissions;
use regia_agent::schema::{METHODS, MethodSchema, ParamSchema, Permission, method};

/// Finding 26: a `read_only = true` client reached `audio.loopgen_render`, which writes a file.
#[test]
fn finding_26_a_read_method_does_not_write_a_file() {
    let schema = method("audio.loopgen_render").expect("the method exists");
    let writes = schema.params.iter().any(|p| p.name == "out_dir");
    assert!(
        !(schema.permission == Permission::Read && writes),
        "a read-only client can write through this method"
    );
}

/// Parameter names that name where output goes.
fn is_output_param(name: &str) -> bool {
    matches!(
        name,
        "out" | "output" | "dest" | "destination" | "save_to" | "write_to" | "write" | "save"
    ) || name.starts_with("out_")
        || name.starts_with("output_")
        || name.starts_with("dest_")
        || name.ends_with("_out")
}

/// Verbs that, as a boolean switch, change state.
const STATE_VERBS: &[&str] = &[
    "set", "start", "stop", "enable", "disable", "attach", "detach", "clear", "reset", "toggle", "write",
    "save", "record", "capture", "apply", "commit", "delete", "remove", "create", "spawn", "install", "log",
    "persist",
];

/// Verbs that, as a word of a method's name, change state: [`STATE_VERBS`] but `capture`, which
/// in a method name reads an image back (`seeing.capture_sequence`, `xr.capture_eye`), and `log`,
/// which there names what is read (`logs.read`, an editor's `log_links`).
fn is_state_verb_in_name(word: &str) -> bool {
    !matches!(word, "capture" | "log") && STATE_VERBS.contains(&word)
}

/// Whether `text` holds `word` as a whole word (ASCII letters around it do not count).
fn has_word(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.match_indices(word).any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + word.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphabetic()) && !after.is_some_and(|c| c.is_ascii_alphabetic())
    })
}

/// Why `schema`, a `read` method, breaks the rule; empty when it keeps it. The rule
/// (`book/agents/protocol.md`): a `read` method takes no output path, writes no file and changes no
/// state another call can observe (no switch, no log it starts, no recorder it attaches).
fn read_rule_breaks(schema: &MethodSchema) -> Vec<String> {
    let mut breaks = Vec::new();
    if schema.permission != Permission::Read {
        return breaks;
    }
    if schema.mutating {
        breaks.push("mutating".to_string());
    }
    let verb_name = schema
        .name
        .split_once('.')
        .map_or(schema.name, |(_, rest)| rest)
        .split('_')
        .find(|word| {
            // Here `spawn` names the data being probed, not an action on the world.
            // ADR-4760: the handler borrows &World and simulates a scratch instance;
            // only this noun is excepted. Every parameter and mutating check still runs.
            // ADR-5957: `record` names the take recorder whose state `sequence.record_state` reads;
            // the handler borrows &World and takes no parameter. Only this noun is excepted.
            is_state_verb_in_name(word)
                && !(schema.name == "vfx.spawn_probe" && *word == "spawn")
                && !(schema.name == "sequence.record_state" && *word == "record")
        });
    if let Some(verb) = verb_name {
        breaks.push(format!("its name says `{verb}`"));
    }
    for param in schema.params {
        if is_output_param(param.name) {
            breaks.push(format!("output parameter `{}`", param.name));
        }
        if param.kind == "boolean" && STATE_VERBS.contains(&param.name) {
            breaks.push(format!("switch `{}`", param.name));
        }
        if ["write", "writes", "written", "writing"]
            .iter()
            .any(|w| has_word(param.description, w))
        {
            breaks.push(format!("`{}` writes", param.name));
        }
        if matches!(param.name, "action" | "mode" | "op" | "command")
            && [
                "start", "stop", "attach", "detach", "enable", "disable", "clear", "reset",
            ]
            .iter()
            .any(|w| has_word(param.description, w))
        {
            breaks.push(format!("`{}` starts or stops something", param.name));
        }
    }
    breaks
}

/// ADR-8795: `read` methods the word detectors flag although they write nothing and change no state.
/// Each entry is one exact break of [`read_rule_breaks`] on one method, with why it is a false
/// match; every other check on the method still runs, and an entry that no longer matches a break
/// fails `every_read_allowance_still_matches_a_break`.
const READ_ALLOWANCES: &[(&str, &str, &str)] = &[
    (
        "logs.perf_gate",
        "`update_baseline` writes",
        "the description says \"Nothing is written\": the agent path answers the new baseline in \
         `baseline_json` and opens no file (only the `regia perf-gate` CLI writes it)",
    ),
    (
        "detsim.ground_vehicle_simulate",
        "`params` writes",
        "\"written over the model's DEFAULT\" is the in-memory overlay of the given fields; the method \
         is stateless and borrows nothing it changes",
    ),
    (
        "detsim.ground_vehicle_simulate",
        "`state` writes",
        "\"written over rest at the origin\" is the in-memory overlay of the start state; the method is \
         stateless",
    ),
    (
        "content.install_tier",
        "its name says `install`",
        "`install_tier` names the built-in install-tier table it reads (residency caps, ADR-8036); the \
         handler takes no world and installs nothing",
    ),
    (
        "rules.spawn_preview",
        "its name says `spawn`",
        "`spawn_preview` names the spawn pick it previews (ADR-8647): it lists the player starts and \
         the pick a spawn would choose now, and the session's spawn state does not move (integ-v07b, \
         ADR-10076)",
    ),
];

/// The breaks of `schema` that [`READ_ALLOWANCES`] does not excuse.
fn unexcused_breaks(schema: &MethodSchema) -> Vec<String> {
    read_rule_breaks(schema)
        .into_iter()
        .filter(|b| {
            !READ_ALLOWANCES
                .iter()
                .any(|(name, allowed, _)| *name == schema.name && allowed == b)
        })
        .collect()
}

/// The audit: every method of the schema that takes an output path, writes files or changes state
/// is not `read`.
#[test]
fn no_read_method_takes_an_output_path_writes_files_or_changes_state() {
    let offenders: Vec<String> = METHODS
        .iter()
        .filter_map(|schema| {
            let breaks = unexcused_breaks(schema);
            (!breaks.is_empty()).then(|| format!("{}: {}", schema.name, breaks.join(", ")))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "read methods that write or change state (give them the class of what they do; book/agents/protocol.md):\n{}",
        offenders.join("\n")
    );
}

/// Each allowance names a real `read`, non-mutating method and a break the detectors still find
/// there, so a fixed description or a reclassified method has to leave the list.
#[test]
fn every_read_allowance_still_matches_a_break() {
    for (name, allowed, why) in READ_ALLOWANCES {
        assert!(!why.is_empty(), "{name}: give the reason");
        let schema = method(name).unwrap_or_else(|| panic!("{name}: no such method"));
        assert_eq!(
            schema.permission,
            Permission::Read,
            "{name}: not read, drop the allowance"
        );
        assert!(!schema.mutating, "{name}: mutating, drop the allowance");
        let breaks = read_rule_breaks(schema);
        assert!(
            breaks.iter().any(|b| b == allowed),
            "{name}: `{allowed}` is no longer found ({breaks:?}); drop the allowance"
        );
    }
}

fn synthetic(name: &'static str, params: &'static [ParamSchema], mutating: bool) -> MethodSchema {
    MethodSchema {
        name,
        tool: "",
        description: "A synthetic method.",
        params,
        result: "{}",
        mutating,
        permission: Permission::Read,
    }
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

/// The audit's detectors catch each shape the misclassified methods had, and leave a plain query
/// (a `start` point, a `path` to read) alone.
#[test]
fn the_audit_catches_each_misclassified_shape() {
    const OUT_DIR: &[ParamSchema] = &[param("out_dir", "string", "Where the WAV goes.")];
    const WAV: &[ParamSchema] = &[param("wav", "boolean", "Also write a 16-bit WAV.")];
    const DETACH: &[ParamSchema] = &[param("detach", "boolean", "Stop recording this entity.")];
    const ACTION: &[ParamSchema] = &[param("action", "string", "\"start\", \"stop\" or \"get\".")];
    const QUERY: &[ParamSchema] = &[
        param("start", "array", "Start point [x, y, z]."),
        param("path", "string", "Project-relative file to read."),
        param("timeout_ms", "number", "Answer deadline."),
    ];
    for (schema, expected) in [
        (synthetic("audio.render", OUT_DIR, false), "output parameter"),
        (synthetic("audio.render", WAV, false), "writes"),
        (synthetic("ai.live", DETACH, false), "switch"),
        (synthetic("render.curve_log", ACTION, false), "starts or stops"),
        (
            synthetic("profiler.set_capture", &[], false),
            "its name says `set`",
        ),
        (synthetic("render.query", &[], true), "mutating"),
    ] {
        let breaks = read_rule_breaks(&schema);
        assert!(
            breaks.iter().any(|b| b.contains(expected)),
            "{}: {breaks:?}",
            schema.name
        );
    }
    assert!(read_rule_breaks(&synthetic("nav.find_path", QUERY, false)).is_empty());
    assert!(read_rule_breaks(&synthetic("render.settings", &[], false)).is_empty());
}

/// A scratch spawn probe is a computed value (spec 23 § 5), while a real spawn or
/// any output/state switch on the probe still requires a stronger permission.
#[test]
fn the_spawn_probe_noun_does_not_hide_mutations() {
    const OUT: &[ParamSchema] = &[param("out_dir", "string", "Output directory.")];
    const RESET: &[ParamSchema] = &[param("reset", "boolean", "Reset the simulation.")];
    assert!(read_rule_breaks(&synthetic("vfx.spawn_probe", &[], false)).is_empty());
    for schema in [
        synthetic("vfx.spawn", &[], false),
        synthetic("vfx.spawn_probe_spawn", &[], false),
        synthetic("vfx.spawn_probe", OUT, false),
        synthetic("vfx.spawn_probe", RESET, false),
        synthetic("vfx.spawn_probe", &[], true),
    ] {
        assert!(!read_rule_breaks(&schema).is_empty(), "{}", schema.name);
    }
    let probe = method("vfx.spawn_probe").expect("probe exists");
    assert_eq!(probe.permission, Permission::Read);
    assert!(!probe.mutating);
}

/// ADR-5957: the take recorder's state is a computed value (spec 23 § 5), while a real record
/// verb, an output path or a state switch on the same method still breaks the read rule.
#[test]
fn the_record_state_noun_does_not_hide_mutations() {
    const OUT: &[ParamSchema] = &[param("out_dir", "string", "Output directory.")];
    const SAVE: &[ParamSchema] = &[param("save", "boolean", "Save the take.")];
    const SEQUENCE: &[ParamSchema] = &[param(
        "sequence",
        "string",
        "The .seq.ron the take is written next to.",
    )];
    assert!(read_rule_breaks(&synthetic("sequence.record_state", &[], false)).is_empty());
    for schema in [
        synthetic("sequence.record", &[], false),
        synthetic("sequence.record_start", &[], false),
        synthetic("sequence.record_state_record", &[], false),
        synthetic("render.record_state", &[], false),
        synthetic("sequence.record_state", OUT, false),
        synthetic("sequence.record_state", SAVE, false),
        synthetic("sequence.record_state", SEQUENCE, false),
        synthetic("sequence.record_state", &[], true),
    ] {
        assert!(!read_rule_breaks(&schema).is_empty(), "{}", schema.name);
    }
    let state = method("sequence.record_state").expect("record_state exists");
    assert_eq!(state.permission, Permission::Read);
    assert!(!state.mutating);
    assert!(state.params.is_empty(), "record_state takes no path");
    for writer in ["sequence.record_start", "sequence.record_stop"] {
        let schema = method(writer).expect("the recorder method exists");
        assert_ne!(schema.permission, Permission::Read, "{writer}");
        assert!(schema.mutating, "{writer}");
    }
}

/// The methods this finding reclassified, with the class each now needs (ADR-3285): file writers
/// and recorders that a polling client drives take `play` (no provenance record per poll), switches
/// beside other `render.set_*`-style toggles and the project-map writer take `edit_scene`.
const RECLASSIFIED: &[(&str, Permission)] = &[
    ("audio.loopgen_render", Permission::Play),
    ("profiler.set_capture", Permission::EditScene),
    ("project.describe", Permission::EditScene),
    ("render.exposure_log", Permission::Play),
    ("pacing.trace", Permission::Play),
    ("procgen.set_debug", Permission::EditScene),
    ("ai.bt_live", Permission::Play),
    ("voice_chat.stats", Permission::Play),
    ("viewer.render_still", Permission::Play),
    ("looks.render_board", Permission::Play),
];

#[test]
fn the_reclassified_methods_are_mutating_and_need_their_class() {
    for (name, permission) in RECLASSIFIED {
        let schema = method(name).expect("the method exists");
        assert_eq!(schema.permission, *permission, "{name}");
        assert!(
            schema.mutating,
            "{name} changes state, so it is rate-limited as mutating"
        );
    }
}

/// A `read_only = true` client is refused every reclassified method, and the default profile still
/// allows each one.
#[test]
fn a_read_only_client_is_refused_the_reclassified_methods() {
    let root = regia_test_utils::unique_temp_dir("security-wave2-read-only");
    let state = root.join(regia_core::brand::PROJECT_STATE_DIR);
    std::fs::create_dir_all(&state).expect("state folder");
    std::fs::write(state.join("agent.toml"), "read_only = true\n").expect("agent.toml");
    let read_only = AgentPermissions::load(&root);
    let default = AgentPermissions::default();
    for (name, _) in RECLASSIFIED {
        let schema = method(name).expect("the method exists");
        assert!(!read_only.allows(schema.permission), "{name} under read_only");
        assert!(
            default.allows(schema.permission),
            "{name} under the default profile"
        );
    }
    assert!(read_only.allows(Permission::Read));
    let _ = std::fs::remove_dir_all(&root);
}

/// sec-review-assets-agent (ADR-5869): the terrain savers and the height exporter write project
/// files but were declared `mutating: false`, so they skipped the mutating-call budget and
/// `agent.schema` told clients they change nothing.
#[test]
fn the_terrain_file_writers_are_mutating() {
    for name in [
        "terrain.save_holes",
        "terrain.save_paint",
        "terrain.export_heights",
        "terrain.save_scatter_paint",
        "terrain.save_grass_density",
    ] {
        let schema = method(name).expect("the method exists");
        assert!(schema.mutating, "{name} writes a project file");
        assert_eq!(schema.permission, Permission::Assets, "{name}");
    }
}
