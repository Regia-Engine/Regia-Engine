// SPDX-License-Identifier: MPL-2.0
//! `agent.params.<group>`: the parameter parsing of the agent protocol's 30 largest method groups under JSON of the
//! right shape (ADR-2084).
//!
//! A group is the method-name prefix before the first dot (`render`, `ui`, `nav`, ...); the 30 with the most built-in
//! methods get a target (ties by name). A target's fixture is one JSON object mapping every method of the group to
//! its contract's worked example (`contract::MethodContract::example`): every required parameter, each of the
//! declared kind. Value mutations keep that shape and corrupt the values (integer edges, NaN, huge strings, emptied
//! or repeated lists), so they reach the handlers' own parsing.
//!
//! `load` sends each call through the dispatcher's whole per-request path (permission, parameter gate, handler) in a
//! fresh headless app with the agent plugin, and requires every answer to be a result or an error with a protocol
//! code. A handler that could reach outside the app (files, processes, native code, the GPU, the wall clock) gets
//! the parameter gate only: see [`handler_runs`]. The canonical re-encoding is the parsed object.

use std::sync::OnceLock;

use regia_app::prelude::*;
use serde_json::{Map, Value};

use super::{BUG_PREFIX, FuzzInput, FuzzTarget};
use crate::schema::{self, MethodSchema, Permission};

/// How many groups get a target.
pub const GROUP_COUNT: usize = 30;

/// Most calls one input may hold (a mutation can repeat a list; it cannot repeat object keys past this).
const MAX_CALLS: usize = 512;

/// Methods whose handler the targets never run, and why (the same list as `tests/dispatch_fuzz.rs`).
const GATE_ONLY: &[(&str, &str)] = &[
    ("quit.request", "ends the app"),
    ("save.save", "writes a save slot"),
    (
        "audio.loopgen_render",
        "writes WAV files to a caller-chosen `out_dir`",
    ),
    ("seeing.screenshot", "`offscreen` creates a GPU device"),
    ("seeing.contact_sheet", "needs a window and a renderer"),
    ("seeing.capture_sequence", "needs a window and a renderer"),
    (
        "seeing.reference_trace",
        "path traces on a GPU device and writes `out_dir`",
    ),
    ("seeing.compare", "path traces and writes `map_png`"),
    (
        "seeing.capture_plate",
        "starts a render-out job and writes plate files to a caller-chosen `out`",
    ),
    ("quality.detect", "enumerates GPU adapters"),
    ("play.hitch_report", "measures wall-clock frame times"),
    // Fixed-cost CPU scenarios: the work does not depend on the parameters, and one call costs 20 ms to 5 s in a
    // debug build, so running it per mutated input would only slow the target down (measured on the build server, ADR-2084).
    (
        "seeing.gi_denoise_check",
        "runs a fixed GI denoise scenario (about 5 s in a debug build)",
    ),
    (
        "render.temporal_quality",
        "runs a fixed temporal-quality scenario (about 0.8 s)",
    ),
    (
        "render.reference_upscale",
        "runs a fixed reference upscale (about 0.1 s)",
    ),
    ("render.grey_card", "renders a fixed grey-card scene on the CPU"),
    ("vfx.crash_fireball", "simulates a fixed effect"),
];

/// One method group.
pub struct Group {
    /// The prefix (`"render"`).
    pub name: &'static str,
    /// `agent.params.<name>`.
    pub target: String,
    /// Its built-in methods, in schema order.
    pub methods: Vec<&'static MethodSchema>,
}

impl std::fmt::Debug for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Group")
            .field("name", &self.name)
            .field("target", &self.target)
            .field(
                "methods",
                &self.methods.iter().map(|m| m.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The method group of `method` (`"render"` for `render.bloom`).
fn group_of(method: &str) -> &str {
    method.split_once('.').map_or(method, |(group, _)| group)
}

/// The [`GROUP_COUNT`] largest groups of `schema::METHODS`, largest first, ties by name.
pub fn groups() -> &'static [Group] {
    static GROUPS: OnceLock<Vec<Group>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        let mut all: Vec<Group> = Vec::new();
        for m in schema::METHODS {
            let name = m.name.split_once('.').map_or(m.name, |(group, _)| group);
            match all.iter_mut().find(|g| g.name == name) {
                Some(g) => g.methods.push(m),
                None => all.push(Group {
                    name,
                    target: format!("agent.params.{name}"),
                    methods: vec![m],
                }),
            }
        }
        all.sort_by(|a, b| b.methods.len().cmp(&a.methods.len()).then(a.name.cmp(b.name)));
        all.truncate(GROUP_COUNT);
        all
    })
}

/// Whether a parameter names a place in the file system or on the network. ADR-8798: `directory`
/// (`photo.capture`) and the other spelled-out names count too, so the fuzz never runs a handler that
/// writes a mutated relative path into its working directory. `output` is not listed: `output_kind`
/// names no place.
fn names_a_location(param: &str) -> bool {
    let p = param.to_ascii_lowercase();
    [
        "path",
        "dir",
        "directory",
        "file",
        "filename",
        "document",
        "location",
        "url",
        "png",
        "out",
        "root",
        "folder",
        "dest",
        "destination",
    ]
    .iter()
    .any(|word| p == *word || p.starts_with(&format!("{word}_")) || p.ends_with(&format!("_{word}")))
}

/// Whether the target runs `method`'s handler, or only the dispatcher's parameter gate. A handler runs when it can
/// only change the throwaway app: read queries, scene edits and play control, none on the `GATE_ONLY` list (the
/// `tests/dispatch_fuzz.rs` skips plus fixed-cost CPU scenarios), and no parameter that names a file, directory or
/// URL (a mutated path must not write outside the app).
pub fn handler_runs(method: &MethodSchema) -> bool {
    matches!(
        method.permission,
        Permission::Read | Permission::EditScene | Permission::Play
    ) && !GATE_ONLY.iter().any(|(name, _)| *name == method.name)
        && !method.params.iter().any(|p| names_a_location(p.name))
}

/// A fresh headless app with the agent plugin (not finished, so no server thread and no endpoint file), every
/// default permission and no rate limit, and two named entities for entity-taking methods.
fn harness() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .register_type::<Name>()
        .register_type::<regia_scene::Transform>()
        .add_plugins(crate::AgentPlugin);
    app.insert_resource(crate::safety::AgentPermissions {
        config: crate::safety::AgentConfig {
            rate_limit_per_second: u32::MAX,
            heavy_rate_limit_per_second: u32::MAX,
            ..crate::safety::AgentConfig::default()
        },
        source: None,
    });
    let world = app.world_mut();
    world.spawn((Name::new("Rock"), regia_scene::Transform::default()));
    world.spawn((Name::new("Pebble"), regia_scene::Transform::default()));
    app
}

/// The fixture of group `n`: `{ method: example params }` for every method of the group.
fn fixture(n: usize) -> String {
    let Some(group) = groups().get(n) else {
        return String::from("{}");
    };
    let mut calls = Map::new();
    for m in &group.methods {
        let example =
            crate::contract::contract(m.name).map_or_else(|| Value::Object(Map::new()), |c| c.example_json());
        calls.insert(m.name.to_string(), example);
    }
    // Some examples embed RON text with a `format_version: 1` inside a JSON string. The JSON itself has no version
    // to refuse, so the colon is written as its JSON escape: the text means the same, and the generic
    // "hostile version" rejection of the fuzz harness (which rewrites the first `version:` number) does not apply.
    serde_json::to_string(&Value::Object(calls))
        .unwrap_or_default()
        .replace("version:", "version\\u003a")
}

/// Parse the calls, run each, and re-encode the parsed object.
fn load(n: usize, text: &str) -> Result<String, String> {
    let group = groups()
        .get(n)
        .ok_or_else(|| format!("{BUG_PREFIX}no method group {n}"))?;
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Value::Object(calls) = &value else {
        return Err("the calls must be one JSON object of `method: params`".into());
    };
    if calls.is_empty() {
        return Err("no call".into());
    }
    if calls.len() > MAX_CALLS {
        return Err(format!("{} calls; at most {MAX_CALLS}", calls.len()));
    }
    let mut methods = Vec::with_capacity(calls.len());
    for (name, params) in calls {
        let method = schema::method(name)
            .filter(|m| group_of(m.name) == group.name)
            .ok_or_else(|| format!("`{name}` is not a method of the `{}` group", group.name))?;
        methods.push((method, params));
    }
    let mut app = harness();
    for (method, params) in methods {
        let answer = if handler_runs(method) {
            crate::dispatch::call_in_process(app.world_mut(), method.name, params.clone())
        } else {
            Some(
                crate::contract::check_params(crate::contract::contract(method.name), params)
                    .map(|()| Value::Null),
            )
        };
        if let Some(Err(error)) = answer {
            let code = error
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !crate::contract::is_error_code(code) || error.message.is_empty() {
                return Err(format!(
                    "{BUG_PREFIX}{} answered an error without a protocol code or message: {code:?} {:?}",
                    method.name, error.message
                ));
            }
        }
    }
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

fn group_fixture<const N: usize>() -> String {
    fixture(N)
}

fn group_load<const N: usize>(text: &str) -> Result<String, String> {
    load(N, text)
}

type Fns = (fn() -> String, fn(&str) -> Result<String, String>);

macro_rules! group_fns {
    ($($n:literal)*) => {
        [$((group_fixture::<$n> as fn() -> String, group_load::<$n> as fn(&str) -> Result<String, String>)),*]
    };
}

/// One fixture and one loader per group index (fn pointers cannot capture the index).
static GROUP_FNS: [Fns; GROUP_COUNT] =
    group_fns!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29);

/// The `agent.params.*` targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    groups()
        .iter()
        .zip(GROUP_FNS.iter())
        .map(|(group, &(fixture, load))| FuzzTarget {
            name: group.target.as_str(),
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text { fixture, load },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_largest_groups_are_ranked_and_complete() {
        let groups = groups();
        assert_eq!(groups.len(), GROUP_COUNT);
        for pair in groups.windows(2) {
            assert!(
                pair[0].methods.len() > pair[1].methods.len()
                    || (pair[0].methods.len() == pair[1].methods.len() && pair[0].name < pair[1].name),
                "{} then {}",
                pair[0].name,
                pair[1].name
            );
        }
        // No group outside the list is larger than the smallest one inside it.
        let smallest = groups.last().map_or(0, |g| g.methods.len());
        let total = |name: &str| {
            schema::METHODS
                .iter()
                .filter(|m| group_of(m.name) == name)
                .count()
        };
        for m in schema::METHODS {
            let name = group_of(m.name);
            if !groups.iter().any(|g| g.name == name) {
                assert!(
                    total(name) <= smallest,
                    "{name} is larger than the smallest listed group"
                );
            }
        }
    }

    #[test]
    fn every_fixture_names_each_method_of_its_group() {
        for (n, group) in groups().iter().enumerate() {
            let calls: Value = serde_json::from_str(&fixture(n)).expect("fixture is JSON");
            assert_eq!(
                calls.as_object().map(Map::len),
                Some(group.methods.len()),
                "{}",
                group.name
            );
        }
    }

    #[test]
    fn a_call_outside_the_group_is_rejected() {
        let n = 0;
        let other = groups()[1].methods[0].name;
        let text = format!(r#"{{"{other}": {{}}}}"#);
        let err = load(n, &text).expect_err("another group's method");
        assert!(err.contains("is not a method of"), "{err}");
        assert!(load(n, "{}").is_err());
        assert!(load(n, "[]").is_err());
    }

    #[test]
    fn location_parameters_keep_a_handler_gate_only() {
        for name in [
            "path",
            "out_dir",
            "map_png",
            "file",
            "root",
            "dest_path",
            "url",
            "directory",
            "filename",
            "document",
            "location",
            "destination",
        ] {
            assert!(names_a_location(name), "{name}");
        }
        for name in ["entity", "frames", "output_kind", "fileless", "route", "outline"] {
            assert!(!names_a_location(name), "{name}");
        }
        let save = schema::method("save.save").expect("save.save");
        assert!(!handler_runs(save));
        // ADR-8798 (review nit): `photo.capture`'s `directory` joined the fuzzer's working directory.
        let photo = schema::method("photo.capture").expect("photo.capture");
        assert!(!handler_runs(photo));
    }
}
