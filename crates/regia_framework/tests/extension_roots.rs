// SPDX-License-Identifier: MPL-2.0
//! ADR-9850 (a) and (b): the two discovery roots with the override rule, and the session record a
//! hot reload of an extension leaves for the replay and determinism tooling. Fixed app steps; no
//! GPU, no wall-clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_framework::extension::discover::MANIFEST_FILE;
use regia_framework::extension::prelude::*;
use regia_framework::extension::{ExtensionPlugin, ExtensionRegistry, ExtensionState, reload_in_world};
use regia_test_utils::unique_temp_dir;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn manifest(dir: &Path, id: &str, version: &str, body: &str) {
    write(
        &dir.join(MANIFEST_FILE),
        &format!(
            "format_version = 1\nid = \"{id}\"\nname = \"{id} {version}\"\nversion = \"{version}\"\n{body}"
        ),
    );
}

struct Dirs {
    base: PathBuf,
    project: PathBuf,
    engine: PathBuf,
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// An engine with `greeter` 1.0.0 (auto-enabled) and `engine_only`, and a project whose own
/// `greeter` 2.0.0 is enabled.
fn dirs(tag: &str) -> Dirs {
    let base = unique_temp_dir(tag);
    let project = base.join("project");
    let engine = base.join("engine");
    write(
        &project.join("Regia.toml"),
        "[project]\nname = \"roots\"\n\n[extensions]\nenabled = [\"greeter\"]\n",
    );
    manifest(&project.join("extensions/greeter"), "greeter", "2.0.0", "");
    manifest(
        &engine.join("extensions/greeter"),
        "greeter",
        "1.0.0",
        "auto_enable = true\n",
    );
    manifest(
        &engine.join("extensions/engine_only"),
        "engine_only",
        "1.0.0",
        "auto_enable = true\n",
    );
    Dirs {
        base,
        project,
        engine,
    }
}

#[test]
fn a_project_extension_overrides_the_engine_extension_of_the_same_id() {
    let d = dirs("ext-roots-override");
    let registry = ExtensionRegistry::load_project(&d.project, Some(&d.engine)).unwrap();
    let all: Vec<String> = registry
        .diagnostics()
        .iter()
        .map(|diag| format!("{} {}", diag.id, diag.code))
        .collect();
    assert!(!all.iter().any(|d| d.ends_with("duplicate_id")), "{all:?}");
    assert_eq!(registry.state("greeter"), Some(ExtensionState::Active), "{all:?}");
    let greeter = registry.discovered("greeter").unwrap();
    assert!(
        Path::new(&greeter.path).starts_with(&d.project),
        "the project copy wins: {}",
        greeter.path
    );
    assert_eq!(greeter.manifest.version.to_string(), "2.0.0");
    assert_eq!(registry.codes_for("greeter"), ["extension_overridden"]);
    // An engine extension with no project copy still loads from the engine root.
    assert_eq!(
        registry.state("engine_only"),
        Some(ExtensionState::Active),
        "{all:?}"
    );
    let engine_only = registry.discovered("engine_only").unwrap();
    assert!(Path::new(&engine_only.path).starts_with(&d.engine));
    // Project roots come first in the discovery order.
    let order: Vec<&str> = registry
        .inputs()
        .discovery
        .manifests
        .iter()
        .map(|m| m.manifest.id.as_str())
        .collect();
    assert_eq!(order, ["greeter", "engine_only"]);
}

#[test]
fn the_plugin_reads_the_engine_root_only_when_given_one() {
    let d = dirs("ext-roots-plugin");
    let without = ExtensionPlugin::for_project(d.project.clone(), Vec::new()).resolve_now();
    assert_eq!(
        without.state("engine_only"),
        None,
        "no engine root, no engine extensions"
    );
    let with = ExtensionPlugin::for_project(d.project.clone(), Vec::new())
        .with_engine(Some(d.engine.clone()))
        .resolve_now();
    assert_eq!(with.state("engine_only"), Some(ExtensionState::Active));
    assert_eq!(with.codes_for("greeter"), ["extension_overridden"]);
    // Two copies on the same side are still a duplicate.
    manifest(
        &d.project.join("extensions/nested/greeter"),
        "greeter",
        "3.0.0",
        "",
    );
    let twice = ExtensionPlugin::for_project(d.project.clone(), Vec::new())
        .with_engine(Some(d.engine.clone()))
        .resolve_now();
    assert!(twice.codes_for("greeter").contains(&"duplicate_id"));
}

/// A linked extension whose method name carries a tag, so a reload is visible.
struct Live(&'static str);

impl RegiaExtension for Live {
    fn id(&self) -> &str {
        "live_ext"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.register_agent_method(AgentMethodDesc {
            name: format!("live_ext.{}", self.0),
            summary: "Says which copy is loaded.".into(),
            access: MethodAccess::Read,
            params_schema: serde_json::json!({ "type": "object" }),
            result_schema: serde_json::json!({ "type": "object" }),
            handler: Arc::new(|_world: &mut World, _params: serde_json::Value| Ok(serde_json::json!({}))),
        })
    }
}

#[test]
fn a_hot_reload_marks_the_session_nondeterministic_for_demos_and_state_hashes() {
    use regia_framework::demo::DemoHeader;
    use regia_framework::session_determinism::{NONDETERMINISTIC_RELOAD, SessionDeterminism, session_tags};

    let base = unique_temp_dir("ext-reload-determinism");
    write(
        &base.join("Regia.toml"),
        "[extensions]\nenabled = [\"live_ext\"]\n[extensions.grants]\nlive_ext = [\"agent_method\"]\n",
    );
    manifest(
        &base.join("extensions/live_ext"),
        "live_ext",
        "1.0.0",
        "capabilities = [\"agent_method\"]\n[[code]]\ncrate = \"x\"\naudience = [\"game\"]\n",
    );
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(ExtensionPlugin::for_project(
        base.clone(),
        vec![Arc::new(Live("one"))],
    ));
    app.update();
    assert!(
        session_tags(app.world()).is_empty(),
        "a fresh session is reproducible"
    );

    reload_in_world(app.world_mut(), &Live("one"), &Live("two")).unwrap();
    app.update();
    let record = app.world().resource::<SessionDeterminism>().clone();
    assert_eq!(record.events.len(), 1);
    assert_eq!(record.events[0].code, NONDETERMINISTIC_RELOAD);
    assert_eq!(record.events[0].source, "live_ext");
    assert_eq!(record.events[0].detail, "generation 1");
    // A failed reload swaps nothing and marks nothing more.
    struct Broken;
    impl RegiaExtension for Broken {
        fn id(&self) -> &str {
            "live_ext"
        }
        fn build(&self, _cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            Err(ExtensionError::BuildFailed("fixture".into()))
        }
    }
    assert!(reload_in_world(app.world_mut(), &Live("two"), &Broken).is_err());
    assert_eq!(app.world().resource::<SessionDeterminism>().events.len(), 1);

    // The replay record: a demo of this session carries the tag, and any replay of it reports it.
    let mut header = DemoHeader::new("yard", 7, 60, 4).with_build("c0ffee");
    header.mark_nondeterministic(&session_tags(app.world()));
    header.mark_nondeterministic(&session_tags(app.world()));
    assert_eq!(header.nondeterministic(), ["nondeterministic_reload:live_ext"]);
    assert_eq!(header.build_id(), "c0ffee");
    let clean = DemoHeader::new("yard", 7, 60, 4).with_build("c0ffee");
    assert!(clean.mismatches(&clean).is_empty());
    assert_eq!(header.mismatches(&clean), ["nondeterministic"]);
    let _ = std::fs::remove_dir_all(&base);
}
