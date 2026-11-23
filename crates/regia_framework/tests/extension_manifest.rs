// SPDX-License-Identifier: MPL-2.0
//! Spec 57 § 8 tests 1-6: manifest parse and resolver. Pure text in, diagnostics out: no GPU,
//! no file system, no clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use regia_framework::extension::manifest::Phase;
use regia_framework::extension::{
    DiscoveredManifest, ExtensionManifest, ExtensionState, ResolveInput, ResolvedSet, Severity, resolve,
};
use semver::Version;

fn man(text: &str) -> DiscoveredManifest {
    let manifest = ExtensionManifest::parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    DiscoveredManifest {
        path: format!("ext/{}", manifest.id),
        hash: format!("h-{}", manifest.id),
        manifest,
    }
}

/// A manifest with `body` after the id and version lines.
fn m(id: &str, version: &str, body: &str) -> DiscoveredManifest {
    man(&format!(
        "format_version = 1\nid = \"{id}\"\nversion = \"{version}\"\n{body}"
    ))
}

struct Run<'a> {
    found: &'a [DiscoveredManifest],
    enabled: &'a [&'a str],
    disabled: &'a [&'a str],
    platform: &'a str,
    profile: &'a str,
    engine: &'a str,
}

fn run(r: Run<'_>) -> ResolvedSet {
    let enabled: Vec<String> = r.enabled.iter().map(|s| (*s).to_owned()).collect();
    let disabled: Vec<String> = r.disabled.iter().map(|s| (*s).to_owned()).collect();
    let grants = BTreeMap::new();
    let engine = Version::parse(r.engine).unwrap();
    let reserved = vec!["scene".to_owned(), "plugin".to_owned()];
    resolve(&ResolveInput {
        discovered: r.found,
        enabled: &enabled,
        disabled: &disabled,
        grants: &grants,
        engine_version: &engine,
        platform: r.platform,
        profile: r.profile,
        dynamic_code: r.platform != "android",
        reserved_ids: &reserved,
    })
}

fn simple<'a>(found: &'a [DiscoveredManifest], enabled: &'a [&'a str]) -> Run<'a> {
    Run {
        found,
        enabled,
        disabled: &[],
        platform: "linux",
        profile: "dev",
        engine: "0.4.1",
    }
}

fn ids(set: &ResolvedSet) -> Vec<&str> {
    set.order.iter().map(|e| e.id.as_str()).collect()
}

#[test]
fn spec_example_manifest_parses() {
    let text = r#"
format_version = 1
id        = "dialogue"
name      = "Dialogue Trees"
version   = "1.4.2"
publisher = "Acme Tools"
licence   = "MIT OR Apache-2.0"
description = "Branching conversation assets and a runtime player."
auto_enable = false
provides  = ["dialogue"]
replaces  = []
conflicts = ["old_dialogue"]
platforms = ["linux", "windows", "android"]

[engine]
min_version = "0.4.0"
max_version = "0.4.99"

[[dependency]]
id       = "ui_extras"
version  = ">=2.1, <3"
optional = false
platforms = ["linux", "windows"]

[[code]]
crate    = "dialogue_runtime"
audience = ["game", "server"]
phase    = "default"
library  = "bin/libdialogue_runtime.so"

[[code]]
crate    = "dialogue_editor"
audience = ["editor"]

capabilities = ["systems", "asset_loader", "agent_method", "editor_panel", "content_mount", "feature_pack"]

[[content]]
root = "content"

[[tool]]
name = "dialogue-lint"
command = "bin/dialogue-lint"
capabilities = []
timeout_seconds = 30
"#;
    let mf = ExtensionManifest::parse(text).unwrap();
    assert_eq!(mf.id, "dialogue");
    assert_eq!(mf.version, Version::new(1, 4, 2));
    assert_eq!(mf.code.len(), 2);
    assert_eq!(mf.dependencies[0].id, "ui_extras");
    assert!(mf.capabilities.contains(&"editor_panel".to_owned()));
    assert_eq!(mf.kind(), "source");
    assert_eq!(mf.tools[0].timeout_seconds, 30);
    assert!(mf.warnings.is_empty());
}

#[test]
fn minimal_unknown_key_and_errors() {
    let mf = ExtensionManifest::parse("format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\n").unwrap();
    assert_eq!(
        (mf.name.as_str(), mf.kind(), mf.phase()),
        ("cheats", "content", Phase::Default)
    );

    let w = ExtensionManifest::parse(
        "format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\nfuture_key = 3\n",
    )
    .unwrap();
    assert_eq!(w.warnings, vec!["unknown_field: future_key".to_owned()]);

    let code = |t: &str| ExtensionManifest::parse(t).unwrap_err().code();
    // Unknown key in [[code]] is an error.
    assert_eq!(
        code(
            "format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\n[[code]]\ncrate = \"x\"\nbogus = 1\n"
        ),
        "manifest_invalid"
    );
    for bad_id in [
        "A",
        "1abc",
        "a",
        "has-dash",
        "waytoolongwaytoolongwaytoolongwaytoolong",
    ] {
        assert_eq!(
            code(&format!(
                "format_version = 1\nid = \"{bad_id}\"\nversion = \"0.1.0\"\n"
            )),
            "manifest_invalid",
            "{bad_id}"
        );
    }
    assert_eq!(
        code("format_version = 1\nid = \"cheats\"\nversion = \"nope\"\n"),
        "manifest_invalid"
    );
    assert_eq!(code("not toml ["), "manifest_invalid");
    assert_eq!(code("id = \"cheats\"\nversion = \"0.1.0\"\n"), "manifest_invalid");
    // Paths may not escape.
    // Windows reads `.. `, `...` and `. .` as the parent (dcc audit H1); UNC, NUL and device
    // names are refused too (ADR-1816).
    for p in [
        "../x",
        "/abs",
        "a/../../b",
        "c:/x",
        ".. ",
        "...",
        "a/. ./b",
        "...\\\\x",
        "//server/share",
        "CON",
        "a/nul.dll",
    ] {
        assert_eq!(
            code(&format!(
                "format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\n[[content]]\nroot = \"{p}\"\n"
            )),
            "path_escape",
            "{p}"
        );
    }
    // Plain relative paths and the directory itself stay fine.
    for p in [".", "content", "content/sub", "./content", "lib/a..b.so"] {
        assert!(
            ExtensionManifest::parse(&format!(
                "format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\n[[content]]\nroot = \"{p}\"\n"
            ))
            .is_ok(),
            "{p}"
        );
    }
    // A newer format is refused before anything else is read, even when the rest is garbage.
    assert_eq!(code("format_version = 2\nid = 5\n"), "manifest_too_new");
}

/// Spec 57 § 4.1: an unknown `[[dependency]]` key is an error (like `[[code]]`), every manifest
/// path (`code.library`, `tool.command`, not only `content.root`) is checked for `path_escape`, and
/// an audience outside `game`/`editor`/`server`/`tool` (§ 2.3) is refused.
#[test]
fn dependency_keys_audiences_and_every_path_are_checked() {
    let head = "format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\n";
    let code = |body: &str| {
        ExtensionManifest::parse(&format!("{head}{body}"))
            .unwrap_err()
            .code()
    };
    assert_eq!(
        code("[[dependency]]\nid = \"ui_extras\"\nbogus = true\n"),
        "manifest_invalid"
    );
    assert_eq!(
        code("[[code]]\ncrate = \"x\"\naudience = [\"client\"]\n"),
        "manifest_invalid"
    );
    for p in ["../lib.so", "/abs/lib.so", "a/../../lib.so"] {
        assert_eq!(
            code(&format!("[[code]]\ncrate = \"x\"\nlibrary = \"{p}\"\n")),
            "path_escape",
            "code.library {p}"
        );
        assert_eq!(
            code(&format!("[[tool]]\nname = \"lint\"\ncommand = \"{p}\"\n")),
            "path_escape",
            "tool.command {p}"
        );
    }
    // The same entries with in-tree paths parse.
    let ok = ExtensionManifest::parse(&format!(
        "{head}[[dependency]]\nid = \"ui_extras\"\nversion = \">=2.1, <3\"\n\
         [[code]]\ncrate = \"x\"\naudience = [\"game\", \"server\"]\nlibrary = \"bin/libx.so\"\n\
         [[tool]]\nname = \"lint\"\ncommand = \"bin/lint\"\n"
    ))
    .unwrap();
    assert_eq!(ok.dependencies[0].id, "ui_extras");
    assert_eq!(ok.code[0].library.as_deref(), Some("bin/libx.so"));
    assert_eq!(ok.tools[0].command, "bin/lint");
}

#[test]
fn diamond_orders_dependencies_first_with_id_ties() {
    // app -> (b, c) -> d
    let found = [
        m(
            "app",
            "1.0.0",
            "[[dependency]]\nid = \"cc\"\n[[dependency]]\nid = \"bb\"\n",
        ),
        m("bb", "1.0.0", "[[dependency]]\nid = \"dd\"\n"),
        m("cc", "1.0.0", "[[dependency]]\nid = \"dd\"\n"),
        m("dd", "1.0.0", ""),
        m("unused", "1.0.0", ""),
    ];
    let set = run(simple(&found, &["app"]));
    assert_eq!(ids(&set), vec!["dd", "bb", "cc", "app"]);
    assert_eq!(set.states["unused"], ExtensionState::Available);
    assert!(!set.has_errors());
}

#[test]
fn hash_is_stable_over_shuffled_discovery() {
    let base = [
        m(
            "app",
            "1.0.0",
            "[[dependency]]\nid = \"bb\"\n[[dependency]]\nid = \"cc\"\n",
        ),
        m("bb", "1.0.0", "[[dependency]]\nid = \"dd\"\n"),
        m("cc", "1.0.0", "[[dependency]]\nid = \"dd\"\n"),
        m("dd", "1.0.0", ""),
        m("ee", "2.0.0", "auto_enable = true\n"),
    ];
    let want = run(simple(&base, &["app"]));
    let mut state = 12345u64;
    for _ in 0..100 {
        let mut shuffled = base.to_vec();
        for i in (1..shuffled.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            shuffled.swap(i, (state >> 33) as usize % (i + 1));
        }
        let got = run(simple(&shuffled, &["app"]));
        assert_eq!(got.resolved_hash, want.resolved_hash);
        assert_eq!(got.order, want.order);
    }
    assert!(
        ids(&want).contains(&"ee"),
        "auto_enable roots load without being named"
    );
}

/// The resolver's closure and order are checked against a deliberately small reference model:
/// repeatedly add dependencies to the live set, then verify every edge points forward.
#[test]
fn bounded_dag_permutations_match_closure_and_partial_order_oracle() {
    // Reverse lexical id order so the oracle catches a resolver that ignores edge indegrees.
    let names = ["dd", "cc", "bb", "aa"];
    let mut edges = Vec::new();
    for dependent in 1..names.len() {
        for dependency in 0..dependent {
            edges.push((dependent, dependency));
        }
    }

    for graph in 0_u32..(1 << edges.len()) {
        let mut bodies = vec![String::new(); names.len()];
        let mut oracle_edges = vec![Vec::new(); names.len()];
        for (bit, &(dependent, dependency)) in edges.iter().enumerate() {
            if graph & (1 << bit) == 0 {
                continue;
            }
            let optional = bit % 2 == 1;
            bodies[dependent].push_str(&format!(
                "[[dependency]]\nid = \"{}\"\noptional = {optional}\n",
                names[dependency]
            ));
            oracle_edges[dependent].push(dependency);
        }
        let manifests: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, id)| m(id, "1.0.0", &bodies[index]))
            .collect();

        let mut expected = BTreeSet::from([names[3]]);
        loop {
            let before = expected.len();
            for (dependent, dependencies) in oracle_edges.iter().enumerate() {
                if expected.contains(names[dependent]) {
                    expected.extend(dependencies.iter().map(|index| names[*index]));
                }
            }
            if before == expected.len() {
                break;
            }
        }
        let expected: Vec<_> = expected.into_iter().collect();

        let mut reversed = manifests.clone();
        reversed.reverse();
        let mut shuffled_declarations = bodies.clone();
        for body in &mut shuffled_declarations {
            let mut declarations: Vec<_> = body
                .split("[[dependency]]\n")
                .filter(|part| !part.is_empty())
                .collect();
            declarations.reverse();
            *body = declarations
                .into_iter()
                .map(|declaration| format!("[[dependency]]\n{declaration}"))
                .collect::<Vec<_>>()
                .join("");
        }
        let shuffled_declarations: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, id)| m(id, "1.0.0", &shuffled_declarations[index]))
            .collect();
        let forward = run(simple(&manifests, &["aa", "aa"]));
        let backward = run(simple(&reversed, &["aa", "aa"]));
        let declaration_permuted = run(simple(&shuffled_declarations, &["aa", "aa"]));
        assert_eq!(
            ids(&forward).iter().copied().collect::<BTreeSet<_>>(),
            expected.into_iter().collect()
        );
        assert_eq!(forward.order, backward.order, "graph mask {graph:#08b}");
        assert_eq!(
            forward.order, declaration_permuted.order,
            "dependency declaration permutation for graph mask {graph:#08b}"
        );
        assert_eq!(forward.states, backward.states, "graph mask {graph:#08b}");
        assert_eq!(
            forward.diagnostics, backward.diagnostics,
            "graph mask {graph:#08b}"
        );
        assert_eq!(
            forward.diagnostics, declaration_permuted.diagnostics,
            "dependency declaration diagnostics for graph mask {graph:#08b}"
        );
        assert_eq!(
            forward.resolved_hash, backward.resolved_hash,
            "graph mask {graph:#08b}"
        );
        assert_eq!(
            forward.resolved_hash, declaration_permuted.resolved_hash,
            "dependency declaration hash for graph mask {graph:#08b}"
        );

        let positions: BTreeMap<_, _> = ids(&forward)
            .into_iter()
            .enumerate()
            .map(|(position, id)| (id, position))
            .collect();
        for (dependent, dependencies) in oracle_edges.iter().enumerate() {
            if !positions.contains_key(names[dependent]) {
                continue;
            }
            for dependency in dependencies {
                assert!(
                    positions[names[*dependency]] < positions[names[dependent]],
                    "dependency {} must precede {} in graph mask {graph:#08b}",
                    names[*dependency],
                    names[dependent]
                );
            }
        }
    }
}

#[test]
fn diagnostic_identities_and_failed_then_valid_runs_are_order_independent() {
    let cycle = [
        m(
            "aa",
            "1.0.0",
            "[[dependency]]\nid = \"cc\"\n[[dependency]]\nid = \"bb\"\n",
        ),
        m("bb", "1.0.0", "[[dependency]]\nid = \"aa\"\n"),
        m("cc", "1.0.0", "[[dependency]]\nid = \"aa\"\n"),
        m("good", "1.0.0", ""),
    ];
    let mut reversed = cycle.clone();
    reversed.reverse();
    let a = run(simple(&cycle, &["aa", "good", "aa"]));
    let b = run(simple(&reversed, &["good", "aa", "aa"]));
    assert_eq!(a.diagnostics, b.diagnostics);
    assert_eq!(a.states, b.states);
    assert_eq!(a.order, b.order);
    assert_eq!(a.resolved_hash, b.resolved_hash);
    assert_eq!(ids(&a), vec!["good"]);

    let manifests = [
        m("dep", "1.0.0", ""),
        m("app", "1.0.0", "[[dependency]]\nid = \"dep\"\n"),
    ];
    let failed = run(Run {
        disabled: &["dep"],
        ..simple(&manifests, &["app"])
    });
    assert_eq!(failed.states["app"], ExtensionState::Failed);
    let recovered = run(simple(&manifests, &["app"]));
    assert!(!recovered.has_errors());
    assert_eq!(ids(&recovered), vec!["dep", "app"]);
}

#[test]
fn disabled_filtered_optional_and_conflict_cases_survive_input_permutations() {
    let manifests = [
        m("aa", "1.0.0", "conflicts = [\"bb\"]\n"),
        m("bb", "1.0.0", ""),
        m(
            "cc",
            "1.0.0",
            "[[dependency]]\nid = \"missing\"\noptional = true\n[[dependency]]\nid = \"platform_only\"\noptional = true\nplatforms = [\"windows\"]\n",
        ),
        m("platform_only", "1.0.0", "platforms = [\"windows\"]\n"),
        m("auto", "1.0.0", "auto_enable = true\n"),
    ];
    let mut reversed = manifests.clone();
    reversed.reverse();
    let x = run(Run {
        disabled: &["auto"],
        ..simple(&manifests, &["aa", "bb", "cc", "platform_only", "bb"])
    });
    let y = run(Run {
        disabled: &["auto"],
        ..simple(&reversed, &["platform_only", "cc", "bb", "aa", "bb"])
    });
    assert_eq!(x.order, y.order);
    assert_eq!(x.states, y.states);
    assert_eq!(x.diagnostics, y.diagnostics);
    assert_eq!(x.resolved_hash, y.resolved_hash);
    assert_eq!(x.states["bb"], ExtensionState::Failed);
    assert_eq!(x.states["platform_only"], ExtensionState::Filtered);
    assert_eq!(x.states["auto"], ExtensionState::Disabled);
    assert!(
        x.diagnostics
            .iter()
            .any(|d| d.code == "optional_dependency_missing")
    );
    assert!(x.diagnostics.iter().any(|d| d.code == "conflict"));
}

#[test]
fn phases_group_the_order() {
    let found = [
        m(
            "zeta",
            "1.0.0",
            "[[code]]\ncrate = \"zeta\"\nphase = \"foundation\"\n",
        ),
        m(
            "alpha",
            "1.0.0",
            "[[code]]\ncrate = \"alpha\"\nphase = \"late\"\n",
        ),
        m("mid", "1.0.0", "[[code]]\ncrate = \"mid\"\n"),
    ];
    let set = run(simple(&found, &["alpha", "mid", "zeta"]));
    assert_eq!(ids(&set), vec!["zeta", "mid", "alpha"]);
}

fn only_code(set: &ResolvedSet, id: &str) -> Vec<&'static str> {
    set.diagnostics
        .iter()
        .filter(|d| d.id == id && d.severity == Severity::Error)
        .map(|d| d.code)
        .collect()
}

#[test]
fn resolver_errors_one_each() {
    // missing_dependency
    let f = [m("aa", "1.0.0", "[[dependency]]\nid = \"ghost\"\n")];
    let s = run(simple(&f, &["aa"]));
    assert_eq!(only_code(&s, "aa"), vec!["missing_dependency"]);
    assert_eq!(s.states["aa"], ExtensionState::Failed);

    // version_unsatisfied and its range edges.
    for (have, req, ok) in [
        ("2.1.0", ">=2.1, <3", true),
        ("3.0.0", ">=2.1, <3", false),
        ("2.0.9", ">=2.1, <3", false),
        ("2.1.0-rc.1", ">=2.1, <3", false),
    ] {
        let f = [
            m(
                "aa",
                "1.0.0",
                &format!("[[dependency]]\nid = \"bb\"\nversion = \"{req}\"\n"),
            ),
            m("bb", have, ""),
        ];
        let s = run(simple(&f, &["aa"]));
        assert_eq!(s.has_errors(), !ok, "{have} against {req}");
        if !ok {
            assert_eq!(only_code(&s, "aa"), vec!["version_unsatisfied"]);
        }
    }
    // A range that names a pre-release matches that pre-release.
    let f = [
        m(
            "aa",
            "1.0.0",
            "[[dependency]]\nid = \"bb\"\nversion = \">=2.0.0-rc.1\"\n",
        ),
        m("bb", "2.0.0-rc.2", ""),
    ];
    assert!(!run(simple(&f, &["aa"])).has_errors());

    // engine_incompatible: min and max inclusive.
    let f = [m(
        "aa",
        "1.0.0",
        "[engine]\nmin_version = \"0.4.0\"\nmax_version = \"0.4.99\"\n",
    )];
    for (engine, ok) in [
        ("0.3.9", false),
        ("0.4.0", true),
        ("0.4.99", true),
        ("0.5.0", false),
    ] {
        let s = run(Run {
            engine,
            ..simple(&f, &["aa"])
        });
        assert_eq!(!s.has_errors(), ok, "engine {engine}");
        if !ok {
            assert_eq!(only_code(&s, "aa"), vec!["engine_incompatible"]);
        }
    }

    // dependency_cycle lists the path.
    let f = [
        m("aa", "1.0.0", "[[dependency]]\nid = \"bb\"\n"),
        m("bb", "1.0.0", "[[dependency]]\nid = \"cc\"\n"),
        m("cc", "1.0.0", "[[dependency]]\nid = \"aa\"\n"),
    ];
    let s = run(simple(&f, &["aa"]));
    let d = s
        .diagnostics
        .iter()
        .find(|d| d.code == "dependency_cycle")
        .expect("cycle reported");
    assert!(d.message.contains("aa -> bb -> cc -> aa"), "{}", d.message);
    assert_eq!(s.states["aa"], ExtensionState::Failed);
    assert!(s.order.is_empty());

    // conflict, from either side.
    let f = [m("aa", "1.0.0", "conflicts = [\"bb\"]\n"), m("bb", "1.0.0", "")];
    for enabled in [["aa", "bb"], ["bb", "aa"]] {
        let s = run(simple(&f, &enabled));
        assert!(s.diagnostics.iter().any(|d| d.code == "conflict"));
        assert_eq!(ids(&s), vec!["aa"], "the later id loses");
        assert_eq!(s.states["bb"], ExtensionState::Failed);
    }

    // provides_clash, and resolved by `replaces`.
    let f = [
        m("aa", "1.0.0", "provides = [\"dialogue\"]\n"),
        m("bb", "1.0.0", "provides = [\"dialogue\"]\n"),
    ];
    assert!(
        run(simple(&f, &["aa", "bb"]))
            .diagnostics
            .iter()
            .any(|d| d.code == "provides_clash")
    );
    let f = [
        m("aa", "1.0.0", "provides = [\"dialogue\"]\n"),
        m("bb", "1.0.0", "provides = [\"dialogue\"]\nreplaces = [\"aa\"]\n"),
    ];
    assert!(!run(simple(&f, &["aa", "bb"])).has_errors());

    // phase_inversion: a dependency in a later phase than its dependent.
    let f = [
        m(
            "aa",
            "1.0.0",
            "[[dependency]]\nid = \"bb\"\n[[code]]\ncrate = \"aa\"\nphase = \"early\"\n",
        ),
        m("bb", "1.0.0", "[[code]]\ncrate = \"bb\"\nphase = \"late\"\n"),
    ];
    assert_eq!(
        only_code(&run(simple(&f, &["aa"])), "aa"),
        vec!["phase_inversion"]
    );

    // unknown_capability / capability_not_granted; systems and settings are implicit.
    let f = [
        m(
            "aa",
            "1.0.0",
            "capabilities = [\"systems\", \"settings\", \"teleport\"]\n",
        ),
        m("bb", "1.0.0", "capabilities = [\"systems\", \"network\"]\n"),
    ];
    let s = run(simple(&f, &["aa", "bb"]));
    assert_eq!(only_code(&s, "aa"), vec!["unknown_capability"]);
    assert_eq!(only_code(&s, "bb"), vec!["capability_not_granted"]);

    // reserved_id.
    let f = [m("scene", "1.0.0", "")];
    assert_eq!(
        only_code(&run(simple(&f, &["scene"])), "scene"),
        vec!["reserved_id"]
    );

    // duplicate_id: a warning when nobody asked for it, a stop when an enabled root is ambiguous.
    let mut dup = m("aa", "1.0.0", "");
    dup.path = "elsewhere/a".into();
    let f = [m("aa", "1.0.0", ""), dup];
    assert!(!run(simple(&f, &[])).has_errors());
    let s = run(simple(&f, &["aa"]));
    assert!(s.has_errors() && s.order.is_empty());
    assert_eq!(s.codes_for("aa"), vec!["duplicate_id"]);
    // Plugin audit: `auto_enable` on the later copy asks for the id too; the idle first copy used to
    // win with a warning, so the engine extension silently never loaded.
    let mut auto = m("aa", "1.0.0", "auto_enable = true\n");
    auto.path = "engine/extensions/aa".into();
    let f = [m("aa", "1.0.0", ""), auto];
    let s = run(simple(&f, &[]));
    assert!(s.has_errors() && s.order.is_empty());
    assert_eq!(s.codes_for("aa"), vec!["duplicate_id"]);
}

#[test]
fn granted_capabilities_pass() {
    let f = [m("bb", "1.0.0", "capabilities = [\"network\"]\n")];
    let enabled = vec!["bb".to_owned()];
    let mut grants = BTreeMap::new();
    grants.insert("bb".to_owned(), vec!["network".to_owned()]);
    let engine = Version::new(0, 4, 1);
    let s = resolve(&ResolveInput {
        discovered: &f,
        enabled: &enabled,
        disabled: &[],
        grants: &grants,
        engine_version: &engine,
        platform: "linux",
        profile: "dev",
        dynamic_code: true,
        reserved_ids: &[],
    });
    assert!(!s.has_errors());
}

#[test]
fn partial_failure_and_disabled_dependencies() {
    // A failed root does not stop an unrelated one.
    let f = [
        m("bad", "1.0.0", "[[dependency]]\nid = \"ghost\"\n"),
        m("good", "1.0.0", ""),
        m("dep", "1.0.0", ""),
        m("needs_dep", "1.0.0", "[[dependency]]\nid = \"dep\"\n"),
    ];
    let s = run(simple(&f, &["bad", "good", "needs_dep"]));
    assert_eq!(ids(&s), vec!["dep", "good", "needs_dep"]);
    assert_eq!(s.states["bad"], ExtensionState::Failed);

    // Disabling a dependency fails its dependents with dependency_disabled; the rest continue.
    let s = run(Run {
        disabled: &["dep"],
        ..simple(&f, &["bad", "good", "needs_dep"])
    });
    assert_eq!(s.states["dep"], ExtensionState::Disabled);
    assert_eq!(s.states["needs_dep"], ExtensionState::Failed);
    assert_eq!(only_code(&s, "needs_dep"), vec!["dependency_disabled"]);
    assert_eq!(ids(&s), vec!["good"]);

    // disabled beats auto_enable.
    let f = [m("auto", "1.0.0", "auto_enable = true\n")];
    let s = run(Run {
        disabled: &["auto"],
        ..simple(&f, &[])
    });
    assert_eq!(s.states["auto"], ExtensionState::Disabled);
    assert!(s.order.is_empty());
}

#[test]
fn optional_dependencies() {
    let f = [m(
        "aa",
        "1.0.0",
        "[[dependency]]\nid = \"opt\"\noptional = true\n",
    )];
    let s = run(simple(&f, &["aa"]));
    assert!(!s.has_errors());
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.code == "optional_dependency_missing" && d.severity == Severity::Info)
    );

    let f = [
        m("aa", "1.0.0", "[[dependency]]\nid = \"opt\"\noptional = true\n"),
        m("opt", "1.0.0", ""),
    ];
    assert_eq!(
        ids(&run(simple(&f, &["aa"]))),
        vec!["opt", "aa"],
        "present: ordered first"
    );

    // A platform filter removes the edge: opt is not loaded for android.
    let f = [
        m(
            "aa",
            "1.0.0",
            "[[dependency]]\nid = \"opt\"\noptional = true\nplatforms = [\"linux\"]\n",
        ),
        m("opt", "1.0.0", ""),
    ];
    assert_eq!(
        ids(&run(Run {
            platform: "android",
            ..simple(&f, &["aa"])
        })),
        vec!["aa"]
    );
    assert_eq!(ids(&run(simple(&f, &["aa"]))), vec!["opt", "aa"]);
}

#[test]
fn platform_and_profile_filters() {
    let f = [
        m("everywhere", "1.0.0", ""),
        m("desktop", "1.0.0", "platforms = [\"linux\", \"windows\"]\n"),
        m("nothing", "1.0.0", "platforms = []\n"),
        m("devonly", "1.0.0", "profiles = [\"dev\"]\n"),
    ];
    let all = ["everywhere", "desktop", "nothing", "devonly"];
    let linux = run(simple(&f, &all));
    assert_eq!(ids(&linux), vec!["desktop", "devonly", "everywhere"]);
    assert_eq!(
        linux.states["nothing"],
        ExtensionState::Filtered,
        "an empty list means none"
    );
    let android = run(Run {
        platform: "android",
        profile: "shipping",
        ..simple(&f, &all)
    });
    assert_eq!(ids(&android), vec!["everywhere"]);
    assert_ne!(linux.resolved_hash, android.resolved_hash);
    assert_eq!(android.states["desktop"], ExtensionState::Filtered);
}

#[test]
fn prebuilt_only_code_is_not_supported_without_dynamic_code() {
    let f = [m("dyn", "1.0.0", "[[code]]\nlibrary = \"bin/libdyn.so\"\n")];
    assert_eq!(f[0].manifest.kind(), "dynamic");
    assert!(!run(simple(&f, &["dyn"])).has_errors());
    let s = run(Run {
        platform: "android",
        ..simple(&f, &["dyn"])
    });
    assert_eq!(only_code(&s, "dyn"), vec!["not_supported"]);
}

/// A capability only a `[[tool]]` lists shows up for review but gates that tool, not the extension
/// (spec § 2.7): an optional helper that opens sockets must not stop the extension from loading.
#[test]
fn tool_only_capabilities_do_not_block_loading() {
    let f = [m(
        "bb",
        "1.0.0",
        "capabilities = [\"systems\"]\n\n[[tool]]\nname = \"sync\"\ncommand = \"bin/sync\"\ncapabilities = [\"network\"]\n",
    )];
    assert_eq!(f[0].manifest.capabilities, ["network", "process", "systems"]);
    assert_eq!(f[0].manifest.load_capabilities, ["systems"]);
    let s = run(simple(&f, &["bb"]));
    assert!(!s.has_errors(), "{:?}", s.diagnostics);
    assert_eq!(s.order.len(), 1);
    // An unknown capability on a tool is still loud: the manifest was written for a newer engine.
    let f = [m(
        "cc",
        "1.0.0",
        "[[tool]]\nname = \"x\"\ncommand = \"bin/x\"\ncapabilities = [\"teleport\"]\n",
    )];
    assert_eq!(
        only_code(&run(simple(&f, &["cc"])), "cc"),
        vec!["unknown_capability"]
    );
}
