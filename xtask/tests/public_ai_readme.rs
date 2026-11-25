// SPDX-License-Identifier: MPL-2.0
//! Review guardrails for the public AI setup docs.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// Review finding: the public setup flow offers the `quick` template, but a quick project is one
/// crate with no launcher crate. The onboarding docs must not hard-code the full-project warm
/// build for every template.
#[test]
fn onboarding_docs_do_not_assume_every_template_has_a_launcher_crate() {
    let cli = read("crates/regia_cli/src/new_project.rs");
    assert!(
        cli.contains("The Quick template is one crate with one")
            && cli.contains("no editor, launcher or Android crates"),
        "the quick template contract moved; update this review proof"
    );

    for rel in [
        "dist/public-root/AGENTS.md",
        "dist/public-skills/regia-new-project/SKILL.md",
        "book/agents/onboarding.md",
    ] {
        let text = read(rel);
        assert!(
            !text.contains("my_game_launcher") && !text.contains("<name>_launcher"),
            "{rel} offers the `quick` template through the public setup flow, but still hard-codes a launcher crate for the warm build"
        );
    }
}

/// ADR-10616 (a public user's `cargo build -p my_game_launcher` failed in a `quick` project): the
/// docs that tell an agent how to warm a new project name exactly the templates that ship a
/// `launcher/` crate, read from `crates/regia_templates/templates/<id>/launcher/Cargo.toml.tmpl`,
/// and give plain `cargo build` for the rest.
#[test]
fn warm_build_docs_name_exactly_the_templates_with_a_launcher_crate() {
    let templates = root().join("crates/regia_templates/templates");
    let mut with_launcher = Vec::new();
    let mut without = Vec::new();
    for entry in std::fs::read_dir(&templates).unwrap_or_else(|e| panic!("reading templates: {e}")) {
        let dir = entry.unwrap_or_else(|e| panic!("template entry: {e}")).path();
        if !dir.is_dir() {
            continue;
        }
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if dir.join("launcher/Cargo.toml.tmpl").is_file() {
            with_launcher.push(id);
        } else {
            without.push(id);
        }
    }
    with_launcher.sort();
    without.sort();
    assert_eq!(
        with_launcher,
        ["blank", "first_person", "third_person", "top_down"]
    );
    assert_eq!(without, ["net_game", "quick"]);
    for rel in [
        "dist/public-root/AGENTS.md",
        "dist/public-skills/regia-new-project/SKILL.md",
        "book/agents/onboarding.md",
    ] {
        let text = read(rel);
        assert!(
            text.contains("_launcher"),
            "{rel} no longer says how to warm a launcher template"
        );
        for id in &with_launcher {
            assert!(
                text.contains(&format!("`{id}`")),
                "{rel} does not name `{id}` as a launcher template"
            );
        }
        assert!(
            text.contains("`net_game`") || text.contains("`net-game`"),
            "{rel} does not say how to warm a `net_game` project (it has no launcher crate)"
        );
        assert!(
            text.contains("plain `cargo build`"),
            "{rel} does not give plain `cargo build` for the templates without a launcher"
        );
    }
}
