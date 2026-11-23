// SPDX-License-Identifier: MPL-2.0
//! The editor's agent method table, as the checked-in snapshot
//! `docs/api-snapshots/editor-agent-methods.txt` records it (ADR-7307).
//!
//! `cargo xtask editor-agent-coverage` reads that snapshot (it cannot link the editor) and holds
//! every method to the runtime's coverage rules: a `book/COVERAGE.md` row whose page names the
//! method and each parameter, an `agent.guide` topic that names it, and a test literal that calls
//! it. This test keeps the snapshot equal to what the editor really registers.
//!
//! Refresh: `REGIA_UPDATE_EDITOR_METHODS=1 cargo test -p regia_editor --test editor_method_snapshot`
//! (or `cargo xtask editor-agent-coverage --update`).

use std::path::PathBuf;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;

const SNAPSHOT: &str = "docs/api-snapshots/editor-agent-methods.txt";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// The snapshot text for the methods the editor registers.
fn render() -> String {
    let mut world = World::new();
    regia_editor::agent_ext::install(&mut world);
    // `editor.headless_status`, `editor.open_level`, `agent.shutdown`, `gen.*` (ADR-2412).
    regia_editor::headless::install(&mut world);
    let extension = world.resource::<AgentExtension>();
    let builtin = regia_agent::builtin_extension_methods();
    let mut names: Vec<&str> = extension
        .method_names()
        .into_iter()
        .filter(|name| !builtin.contains(name))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut out = String::from(
        "# Editor agent methods (ADR-7307): one line per method the editor registers on AgentExtension,\n\
         # `name(param: kind, optional?: kind)`. Read by `cargo xtask editor-agent-coverage`.\n\
         # Refresh: REGIA_UPDATE_EDITOR_METHODS=1 cargo test -p regia_editor --test editor_method_snapshot\n",
    );
    for name in names {
        let params = extension
            .method_schema(name)
            .map(|schema| {
                schema
                    .params
                    .iter()
                    .map(|p| format!("{}{}: {}", p.name, if p.required { "" } else { "?" }, p.kind))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        out.push_str(&format!("{name}({params})\n"));
    }
    out
}

#[test]
fn the_editor_method_snapshot_is_current() {
    let path = workspace_root().join(SNAPSHOT);
    let want = render();
    if std::env::var_os("REGIA_UPDATE_EDITOR_METHODS").is_some() {
        let tmp = path.with_extension("txt.tmp");
        std::fs::write(&tmp, &want).expect("write the snapshot");
        std::fs::rename(&tmp, &path).expect("replace the snapshot");
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    if have != want {
        let have_lines: std::collections::BTreeSet<&str> = have.lines().collect();
        let want_lines: std::collections::BTreeSet<&str> = want.lines().collect();
        let added: Vec<&&str> = want_lines.difference(&have_lines).collect();
        let removed: Vec<&&str> = have_lines.difference(&want_lines).collect();
        panic!(
            "{SNAPSHOT} is stale; refresh it with REGIA_UPDATE_EDITOR_METHODS=1 cargo test -p regia_editor --test editor_method_snapshot\nnew or changed: {added:#?}\ngone or changed: {removed:#?}"
        );
    }
}
