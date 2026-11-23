// SPDX-License-Identifier: MPL-2.0
//! Tests of the asset and graph diff (P-182): the RON tree, the structural diff, the graph
//! adapters, revisions from a temp git repo and the agent methods.

use std::path::Path;
use std::process::Command;

use serde_json::json;

use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const MAT_OLD: &str = r#"// fixture material graph
(
    format_version: 3,
    name: "fixture",
    nodes: {
        "base": Float(0.5),
        "tint": Vec3((1.0, 0.5, 0.25)),
        "spare": Float(2.0),
        "mix": Mul("tint", "base"),
    },
    output: (base_color: Some("mix")),
)
"#;

/// One node changed (`base`), one removed (`spare`), one added (`gloss`); no wire moves.
const MAT_NEW: &str = r#"(
    format_version: 3,
    name: "fixture",
    nodes: {
        "base": Float(0.8),
        "tint": Vec3((1.0, 0.5, 0.25)),
        "mix": Mul("tint", "base"),
        "gloss": Float(0.3),
    },
    output: (base_color: Some("mix")),
)
"#;

const TREE_OLD: &str = r#"#![enable(implicit_some)]
(
    name: "door",
    physics: (mass: 10.0, friction: (static: 0.6, dynamic: 0.4)),
    tags: ["a", "b"],
    mode: Swing(axis: Y),
)
"#;

/// The nested field `physics.friction.dynamic` changed; the rest is written differently but means
/// the same (whitespace, a comment, `10` for `10.0`, field order).
const TREE_NEW: &str = r#"#![enable(implicit_some)]
(
    // reformatted
    physics: (friction: (static: 0.6, dynamic: 0.45), mass: 10),
    name: "door",
    tags: ["a", "b"],
    mode: Swing(axis: Y),
)
"#;

const ANIM_OLD: &str = r#"(
    format_version: 2,
    parameters: [("Go", Bool(false))],
    root: StateMachine((
        initial: "A",
        states: [
            ("A", Clip((clip: "A", looping: true, speed: 1.0, sync_group: None))),
            ("B", Clip((clip: "B", looping: true, speed: 1.0, sync_group: None))),
        ],
        transitions: [],
    )),
)
"#;

/// State A's clip plays faster, state B is gone, state C is new.
const ANIM_NEW: &str = r#"(
    format_version: 2,
    parameters: [("Go", Bool(false))],
    root: StateMachine((
        initial: "A",
        states: [
            ("A", Clip((clip: "A", looping: true, speed: 2.0, sync_group: None))),
            ("C", Clip((clip: "C", looping: true, speed: 1.0, sync_group: None))),
        ],
        transitions: [],
    )),
)
"#;

const LOGIC_OLD: &str = r#"LogicGraphDef(
    format_version: 1,
    description: "door",
    seed: 7,
    locals: [],
    rules: [
        (name: "open_door", event: EntityEvent("trigger_enter"), priority: 10, condition: Key("near"), actions: []),
        (name: "raise_alarm", event: EntityEvent("trigger_enter"), priority: 20, condition: Key("locked"), actions: []),
    ],
)
"#;

/// `open_door` changed priority, `raise_alarm` is gone, `patrol` is new.
const LOGIC_NEW: &str = r#"LogicGraphDef(
    format_version: 1,
    description: "door",
    seed: 7,
    locals: [],
    rules: [
        (name: "open_door", event: EntityEvent("trigger_enter"), priority: 15, condition: Key("near"), actions: []),
        (name: "patrol", event: Timer(every_ticks: 600, jitter_ticks: 60), priority: 1, condition: Key("idle"), actions: []),
    ],
)
"#;

fn graph(body: &DiffBody) -> &GraphDiff {
    match body {
        DiffBody::Graph(g) => g,
        DiffBody::Tree { .. } => panic!("expected a graph diff"),
    }
}

/// `(status, id)` of every node that is not unchanged, sorted.
fn node_changes(g: &GraphDiff) -> Vec<(ChangeKind, String)> {
    let mut out: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.status != ChangeKind::Unchanged)
        .map(|n| (n.status, n.id().to_string()))
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

// ---------------------------------------------------------------------------
// RON tree
// ---------------------------------------------------------------------------

#[test]
fn parse_keeps_variant_and_struct_names() {
    let v = parse_ron(r##"Mul("a", "b")"##).expect("parse");
    assert_eq!(v.name(), Some("Mul"));
    let v = parse_ron(r##"(clip: r#"x "y""#, n: -1.5e-3, c: '\'', b: true, u: (), list: [], m: {})"##)
        .expect("parse");
    assert_eq!(v.field("clip"), Some(&RonValue::Str("x \"y\"".into())));
    assert_eq!(v.field("n"), Some(&RonValue::Scalar("-1.5e-3".into())));
    assert_eq!(v.field("b"), Some(&RonValue::Scalar("true".into())));
    let nested = parse_ron("/* a /* nested */ comment */ Some(Vec3((1, 2, 3)))").expect("parse");
    assert_eq!(nested.name(), Some("Some"));
}

#[test]
fn compact_ron_reads_back_into_the_typed_node() {
    let v = parse_ron(r#"Swizzle(input: "op", channels: "x")"#).expect("parse");
    let node: regia_matgraph::Node = ron::from_str(&v.to_ron()).expect("typed");
    assert_eq!(node.inputs(), vec!["op"]);
}

#[test]
fn parse_errors_name_the_line() {
    let err = parse_ron("(\n  a: 1,\n  b: ]\n)").expect_err("bad");
    assert!(err.contains("line 3"), "{err}");
    let deep = format!("{}1{}", "[".repeat(MAX_DEPTH + 5), "]".repeat(MAX_DEPTH + 5));
    assert!(parse_ron(&deep).expect_err("deep").contains("too deep"));
}

/// A file cut anywhere, or garbage, is an error and never a panic (the editor reads user files).
#[test]
fn truncated_and_garbage_text_never_panics() {
    let samples = [
        MAT_OLD,
        TREE_OLD,
        ANIM_OLD,
        LOGIC_OLD,
        "(s: \"é\\u{e9}\\x41\", c: 'é', r: r#\"ü\"#)",
    ];
    for text in samples {
        for (cut, _) in text.char_indices() {
            let _ = parse_ron(&text[..cut]);
        }
        assert!(parse_ron(text).is_ok(), "{text}");
    }
    for junk in [
        "#![",
        "\"\\x4",
        "\"\\u{zz}\"",
        "\"\\u12\"",
        "'",
        "(a: é)",
        "r#",
        "/* open",
        "[1,,]",
        "{1 1}",
    ] {
        assert!(parse_ron(junk).is_err(), "{junk:?}");
    }
}

// ---------------------------------------------------------------------------
// Structural diff
// ---------------------------------------------------------------------------

/// Acceptance: a RON document with one changed nested field gives exactly that one path.
#[test]
fn nested_field_change_is_one_path() {
    let body = diff_texts(AssetKind::Ron, TREE_OLD, None, TREE_NEW, None).expect("diff");
    let changes = body.fields();
    assert_eq!(changes.len(), 1, "{changes:#?}");
    assert_eq!(changes[0].path, "physics.friction.dynamic");
    assert_eq!(changes[0].kind, ChangeKind::Changed);
    assert_eq!(
        changes[0].old.as_ref().map(RonValue::to_ron).as_deref(),
        Some("0.4")
    );
    assert_eq!(
        changes[0].new.as_ref().map(RonValue::to_ron).as_deref(),
        Some("0.45")
    );
}

#[test]
fn identical_documents_have_no_changes() {
    assert!(
        diff_texts(AssetKind::Ron, TREE_OLD, None, TREE_OLD, None)
            .expect("diff")
            .fields()
            .is_empty()
    );
}

#[test]
fn sequence_insert_is_one_added_item() {
    let a = parse_ron(r#"["a", "b", "c"]"#).expect("a");
    let b = parse_ron(r#"["z", "a", "b", "c"]"#).expect("b");
    let changes = diff_values(&a, &b);
    assert_eq!(changes.len(), 1, "{changes:#?}");
    assert_eq!(
        (changes[0].path.as_str(), changes[0].kind),
        ("[0]", ChangeKind::Added)
    );
    let c = parse_ron(r#"["a", "c"]"#).expect("c");
    let changes = diff_values(&a, &c);
    assert_eq!(changes.len(), 1, "{changes:#?}");
    assert_eq!(
        (changes[0].path.as_str(), changes[0].kind),
        ("[1]", ChangeKind::Removed)
    );
}

#[test]
fn struct_fields_and_map_keys_add_and_remove() {
    let a = parse_ron(r#"(a: 1, m: {"x": 1, "y": 2})"#).expect("a");
    let b = parse_ron(r#"(b: 1, m: {"x": 1, "z": 2})"#).expect("b");
    let mut got: Vec<_> = diff_values(&a, &b)
        .into_iter()
        .map(|c| (c.path, c.kind))
        .collect();
    got.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        got,
        vec![
            ("a".to_string(), ChangeKind::Removed),
            ("b".to_string(), ChangeKind::Added),
            ("m[\"y\"]".to_string(), ChangeKind::Removed),
            ("m[\"z\"]".to_string(), ChangeKind::Added),
        ]
    );
}

#[test]
fn variant_change_is_one_change_at_its_path() {
    let a = parse_ron("(mode: Swing(axis: Y))").expect("a");
    let b = parse_ron("(mode: Slide(axis: Y))").expect("b");
    let changes = diff_values(&a, &b);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "mode");
}

// ---------------------------------------------------------------------------
// Graph diffs
// ---------------------------------------------------------------------------

/// Acceptance: a material graph with one added, one removed and one changed node gives exactly
/// those three (and no wire or document changes).
#[test]
fn material_graph_added_removed_changed() {
    let body = diff_texts(AssetKind::MaterialGraph, MAT_OLD, None, MAT_NEW, None).expect("diff");
    let g = graph(&body);
    assert_eq!(
        node_changes(g),
        vec![
            (ChangeKind::Changed, "base".to_string()),
            (ChangeKind::Added, "gloss".to_string()),
            (ChangeKind::Removed, "spare".to_string()),
        ]
    );
    let base = g.nodes.iter().find(|n| n.id() == "base").expect("base");
    assert_eq!(base.fields.len(), 1, "{:#?}", base.fields);
    assert!(
        g.wires.iter().all(|w| w.status == ChangeKind::Unchanged),
        "{:#?}",
        g.wires
    );
    // tint → mix, base → mix and mix → output.base_color.
    assert_eq!(g.wires.len(), 3);
    assert!(g.document.is_empty(), "{:#?}", g.document);
    assert_eq!(body.entries().len(), 3);
}

#[test]
fn material_rewire_is_a_changed_node_and_wire_changes() {
    let new = MAT_OLD.replace(r#""mix": Mul("tint", "base")"#, r#""mix": Mul("tint", "spare")"#);
    let body = diff_texts(AssetKind::MaterialGraph, MAT_OLD, None, &new, None).expect("diff");
    let g = graph(&body);
    assert_eq!(node_changes(g), vec![(ChangeKind::Changed, "mix".to_string())]);
    let wires: Vec<_> = g
        .wires
        .iter()
        .filter(|w| w.status != ChangeKind::Unchanged)
        .map(|w| (w.status, w.wire.from.as_str(), w.wire.slot.as_str()))
        .collect();
    assert_eq!(
        wires,
        vec![
            (ChangeKind::Added, "spare", "in1"),
            (ChangeKind::Removed, "base", "in1")
        ]
    );
}

/// A node renamed in place (same kind, same authored position) matches its old self: one changed
/// node, not a removed and an added one; the wires through it follow the rename.
#[test]
fn renamed_node_matches_by_kind_and_position() {
    let old_layout = r#"(format_version: 2, positions: {"base": (10.0, 20.0), "spare": (100.0, 40.0)})"#;
    let new = MAT_OLD
        .replace(r#""base": Float(0.5)"#, r#""base_value": Float(0.5)"#)
        .replace(r#"Mul("tint", "base")"#, r#"Mul("tint", "base_value")"#);
    let new_layout =
        r#"(format_version: 2, positions: {"base_value": (10.0, 20.5), "spare": (300.0, 40.0)})"#;
    let body = diff_texts(
        AssetKind::MaterialGraph,
        MAT_OLD,
        Some(old_layout),
        &new,
        Some(new_layout),
    )
    .expect("diff");
    let g = graph(&body);
    let changed = node_changes(g);
    assert_eq!(
        changed,
        vec![
            (ChangeKind::Changed, "base_value".to_string()),
            (ChangeKind::Changed, "mix".to_string()),
        ]
    );
    let renamed = g.nodes.iter().find(|n| n.id() == "base_value").expect("renamed");
    assert!(renamed.renamed() && renamed.fields.is_empty());
    let spare = g.nodes.iter().find(|n| n.id() == "spare").expect("spare");
    assert_eq!(
        spare.status,
        ChangeKind::Unchanged,
        "a move alone is not a change"
    );
    assert!(spare.moved);
    assert!(
        g.wires.iter().all(|w| w.status == ChangeKind::Unchanged),
        "{:#?}",
        g.wires
    );
}

/// Acceptance (animation graphs): the state whose clip changed is changed, the state that went
/// away is removed, the new one is added; the machine itself changed (its state list).
#[test]
fn anim_graph_added_removed_changed() {
    let body = diff_texts(AssetKind::AnimGraph, ANIM_OLD, None, ANIM_NEW, None).expect("diff");
    let g = graph(&body);
    let changes = node_changes(g);
    let of = |status: ChangeKind| -> Vec<&str> {
        changes
            .iter()
            .filter(|(s, _)| *s == status)
            .map(|(_, id)| id.as_str())
            .collect()
    };
    assert_eq!(of(ChangeKind::Added).len(), 1, "{changes:?}");
    assert!(of(ChangeKind::Added)[0].contains('C'), "{changes:?}");
    assert_eq!(of(ChangeKind::Removed).len(), 1, "{changes:?}");
    assert!(of(ChangeKind::Removed)[0].contains('B'), "{changes:?}");
    let changed_clip = g
        .nodes
        .iter()
        .find(|n| n.status == ChangeKind::Changed && n.kind == "clip")
        .expect("state A's clip changed");
    assert!(changed_clip.id().contains('A'));
    assert_eq!(changed_clip.fields.len(), 1);
    assert!(
        changed_clip.fields[0].path.ends_with("speed"),
        "{:?}",
        changed_clip.fields
    );
    assert_eq!(
        of(ChangeKind::Changed).len(),
        2,
        "the clip and the machine: {changes:?}"
    );
    assert!(g.document.is_empty(), "{:#?}", g.document);
}

#[test]
fn anim_graph_same_text_is_unchanged() {
    let body = diff_texts(AssetKind::AnimGraph, ANIM_OLD, None, ANIM_OLD, None).expect("diff");
    assert!(body.entries().is_empty());
}

#[test]
fn logic_graph_rules_are_nodes() {
    let body = diff_texts(AssetKind::LogicGraph, LOGIC_OLD, None, LOGIC_NEW, None).expect("diff");
    let g = graph(&body);
    assert_eq!(
        node_changes(g),
        vec![
            (ChangeKind::Changed, "open_door".to_string()),
            (ChangeKind::Added, "patrol".to_string()),
            (ChangeKind::Removed, "raise_alarm".to_string()),
        ]
    );
    let open = g.nodes.iter().find(|n| n.id() == "open_door").expect("open_door");
    assert_eq!(open.fields[0].path, "priority");
    assert_eq!(
        g.nodes
            .iter()
            .find(|n| n.id() == "patrol")
            .map(|n| n.kind.as_str()),
        Some("Timer")
    );
}

#[test]
fn bad_graph_text_is_parse_failed() {
    let err = diff_texts(AssetKind::MaterialGraph, "(nodes: {", None, MAT_NEW, None).expect_err("bad");
    assert_eq!(err.code, "parse_failed");
    let err = diff_texts(AssetKind::AnimGraph, "(format_version: 2)", None, ANIM_OLD, None).expect_err("bad");
    assert_eq!(err.code, "parse_failed");
}

#[test]
fn kinds_by_file_name() {
    assert_eq!(
        AssetKind::of_path("a/x.matgraph.ron"),
        Some(AssetKind::MaterialGraph)
    );
    assert_eq!(
        AssetKind::of_path("x.matfunc.ron"),
        Some(AssetKind::MaterialFunction)
    );
    assert_eq!(AssetKind::of_path("x.animgraph.ron"), Some(AssetKind::AnimGraph));
    assert_eq!(AssetKind::of_path("x.logic.ron"), Some(AssetKind::LogicGraph));
    assert_eq!(AssetKind::of_path("x.matgraph.layout.ron"), Some(AssetKind::Ron));
    assert_eq!(AssetKind::of_path("x.prefab.ron"), Some(AssetKind::Ron));
    assert_eq!(AssetKind::of_path("x.png"), None);
    assert_eq!(layout_sidecar("a/x.animgraph.ron"), "a/x.animgraph.layout.ron");
}

#[test]
fn revision_names_are_checked_before_git_sees_them() {
    for ok in [
        "HEAD",
        "HEAD~2",
        "HEAD^",
        "main",
        "feature/x",
        "v1.2",
        "@{1}",
        "1a2b3c4",
    ] {
        assert!(valid_revision(ok), "{ok}");
    }
    for bad in [
        "",
        "--output=/tmp/x",
        "-n1",
        "a..b",
        "HEAD:path",
        "a b",
        "a\nb",
        "$(x)",
        "a;b",
    ] {
        assert!(!valid_revision(bad), "{bad:?}");
    }
}

// ---------------------------------------------------------------------------
// Revisions (temp git repo)
// ---------------------------------------------------------------------------

/// A [`Git`] that cannot find a repository above `dir` (so a temp dir never sees the repo the
/// tests run in).
fn confined(dir: &Path) -> Git {
    Git {
        ceiling: dir.parent().map(Path::to_path_buf),
        ..Git::default()
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Regia Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, text).expect("write");
}

/// A repo with `assets/m.matgraph.ron` at MAT_OLD (commit 1) and the working copy at MAT_NEW.
fn repo_with_material(tag: &str) -> regia_test_utils::TempDir {
    let dir = regia_test_utils::TempDir::new(tag);
    git(&dir, &["init", "-q"]);
    write(&dir.join("Regia.toml"), "[project]\nname = \"t\"\n");
    write(&dir.join("assets/m.matgraph.ron"), MAT_OLD);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    write(&dir.join("assets/m.matgraph.ron"), MAT_NEW);
    dir
}

/// Acceptance: a temp git repo provides the revision (HEAD against the working copy).
#[test]
fn diff_against_head_from_a_temp_repo() {
    let dir = repo_with_material("asset-diff-head");
    let diff = diff_asset_with(&confined(&dir), &dir, "assets/m.matgraph.ron", None, None).expect("diff");
    assert_eq!(diff.kind, AssetKind::MaterialGraph);
    assert_eq!(diff.old.rev.as_deref(), Some("HEAD"));
    assert_eq!(diff.old.hash.as_ref().map(String::len), Some(40));
    assert_eq!(diff.new.rev, None);
    assert_eq!(node_changes(graph(&diff.body)).len(), 3);
    // An absolute path inside the project names the same file.
    let abs = dir.join("assets/m.matgraph.ron");
    let again = diff_asset_with(&confined(&dir), &dir, &abs.to_string_lossy(), None, None).expect("abs");
    assert_eq!(again.path, "assets/m.matgraph.ron");
}

#[test]
fn revision_picker_lists_history_and_diffs_two_revisions() {
    let dir = repo_with_material("asset-diff-picker");
    git(&dir, &["commit", "-q", "-am", "second"]);
    let g = confined(&dir);
    let list = asset_revisions_with(&g, &dir, "assets/m.matgraph.ron", 10).expect("revisions");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].subject, "second");
    assert_eq!(list[1].subject, "first");
    assert_eq!(list[1].path, "assets/m.matgraph.ron");
    assert!(list[0].date.len() == 10 && list[0].author == "Regia Test");
    let diff = diff_asset_with(
        &g,
        &dir,
        "assets/m.matgraph.ron",
        Some(&list[1].hash),
        Some("HEAD"),
    )
    .expect("diff");
    assert_eq!(node_changes(graph(&diff.body)).len(), 3);
    assert_eq!(diff.new.rev.as_deref(), Some("HEAD"));
    let same = diff_asset_with(&g, &dir, "assets/m.matgraph.ron", Some("HEAD"), None).expect("same");
    assert!(same.body.entries().is_empty());
}

/// A project below the work tree's top, and a file renamed since the revision: the old side
/// reads the file under its old name.
#[test]
fn renamed_file_in_a_nested_project_follows_history() {
    let dir = regia_test_utils::TempDir::new("asset-diff-rename");
    git(&dir, &["init", "-q"]);
    let root = dir.join("game");
    write(&root.join("Regia.toml"), "[project]\nname = \"t\"\n");
    write(&root.join("assets/old.ron"), TREE_OLD);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    let first = git(&dir, &["rev-parse", "HEAD"]);
    git(&dir, &["mv", "game/assets/old.ron", "game/assets/new.ron"]);
    git(&dir, &["commit", "-q", "-m", "rename"]);
    write(&root.join("assets/new.ron"), TREE_NEW);
    let g = confined(&dir);
    let list = asset_revisions_with(&g, &root, "assets/new.ron", 10).expect("revisions");
    assert_eq!(list.len(), 2, "{list:#?}");
    assert_eq!(list[1].path, "assets/old.ron");
    let diff = diff_asset_with(&g, &root, "assets/new.ron", Some(&first), None).expect("diff");
    assert_eq!(diff.old.path, "assets/old.ron");
    assert_eq!(diff.body.fields().len(), 1);
    assert_eq!(project_root_of(&root.join("assets/new.ron")), Some(root.clone()));
}

#[test]
fn refusals_have_codes() {
    let dir = repo_with_material("asset-diff-refuse");
    let g = confined(&dir);
    let code = |r: Result<AssetDiff, DiffError>| r.map(|_| ()).expect_err("refused").code;
    assert_eq!(
        code(diff_asset_with(&g, &dir, "../outside.ron", None, None)),
        "path_outside_project"
    );
    assert_eq!(
        code(diff_asset_with(&g, &dir, "/etc/hostname.ron", None, None)),
        "path_outside_project"
    );
    assert_eq!(code(diff_asset_with(&g, &dir, "", None, None)), "bad_path");
    assert_eq!(
        code(diff_asset_with(
            &g,
            &dir,
            "assets/m.matgraph.ron",
            Some("--output=x"),
            None
        )),
        "bad_revision"
    );
    assert_eq!(
        code(diff_asset_with(
            &g,
            &dir,
            "assets/m.matgraph.ron",
            Some("no-such-branch"),
            None
        )),
        "bad_revision"
    );
    write(&dir.join("assets/new.ron"), "()");
    assert_eq!(
        code(diff_asset_with(&g, &dir, "assets/new.ron", None, None)),
        "not_in_revision"
    );
    assert_eq!(
        code(diff_asset_with(&g, &dir, "assets/x.png", None, None)),
        "unsupported_asset"
    );
    std::fs::remove_file(dir.join("assets/m.matgraph.ron")).expect("remove");
    assert_eq!(
        code(diff_asset_with(&g, &dir, "assets/m.matgraph.ron", None, None)),
        "read_failed"
    );
}

/// A project outside any work tree (or without a git program) shows "No revision control".
#[test]
fn no_git_project_is_no_revision_control() {
    let dir = regia_test_utils::TempDir::new("asset-diff-nogit");
    write(&dir.join("assets/a.ron"), "(a: 1)");
    let err = diff_asset_with(&confined(&dir), &dir, "assets/a.ron", None, None).expect_err("no git");
    assert_eq!(err.code, "no_revision_control");
    assert!(err.message.starts_with("No revision control"), "{}", err.message);
    let err = asset_revisions_with(&confined(&dir), &dir, "assets/a.ron", 5).expect_err("no git");
    assert_eq!(err.code, "no_revision_control");
    let missing = Git {
        program: "regia-no-such-git-program".into(),
        ceiling: None,
    };
    let err = diff_asset_with(&missing, &dir, "assets/a.ron", None, None).expect_err("no program");
    assert_eq!(err.code, "no_revision_control");
}

#[test]
fn error_codes_are_protocol_safe() {
    for code in ERROR_CODES {
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{code}"
        );
    }
}

// ---------------------------------------------------------------------------
// Agent methods (dispatcher)
// ---------------------------------------------------------------------------

fn agent_world(root: Option<&Path>) -> World {
    let mut world = World::new();
    crate::agent_ext::install(&mut world);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: root.map(Path::to_path_buf),
    });
    world
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    let (_, handler) = world
        .resource::<regia_agent::extension::AgentExtension>()
        .method(name)
        .unwrap_or_else(|| panic!("{name} is not registered"));
    handler(world, &params)
}

fn reason(err: &RpcError) -> Option<&str> {
    err.data.as_ref().and_then(|d| d["reason"].as_str())
}

/// `editor.asset_diff` and `editor.asset_revisions` through the installed dispatcher. The real
/// `git` (no ceiling) runs here, so the temp repo must be a repo of its own: it is.
#[test]
fn agent_asset_diff_returns_the_change_list() {
    let dir = repo_with_material("asset-diff-agent");
    let mut world = agent_world(Some(&dir));
    let out = call(
        &mut world,
        "editor.asset_diff",
        json!({ "path": "assets/m.matgraph.ron" }),
    )
    .expect("diff");
    assert_eq!(out["kind"], "material_graph");
    assert_eq!(out["rev_a"], "HEAD");
    assert_eq!(out["rev_b"], Value::Null);
    assert_eq!(out["counts"], json!({ "added": 1, "removed": 1, "changed": 1 }));
    let changes = out["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), 3);
    assert!(changes.iter().all(|c| c["target"] == "node"));
    let base = changes.iter().find(|c| c["id"] == "base").expect("base");
    assert_eq!(base["status"], "changed");
    // The field paths are inside the node: `Float(0.5)` → `Float(0.8)` is item 0.
    assert_eq!(base["fields"][0]["path"], "0");
    assert_eq!(base["fields"][0]["old"], "0.5");
    assert_eq!(base["fields"][0]["new"], "0.8");

    let revs = call(
        &mut world,
        "editor.asset_revisions",
        json!({ "path": "assets/m.matgraph.ron" }),
    )
    .expect("revs");
    assert_eq!(revs["revisions"].as_array().map(Vec::len), Some(1));
    let hash = revs["revisions"][0]["hash"].as_str().expect("hash").to_string();
    let out = call(
        &mut world,
        "editor.asset_diff",
        json!({ "path": "assets/m.matgraph.ron", "rev_a": hash, "rev_b": "HEAD" }),
    )
    .expect("diff");
    assert_eq!(out["changes"].as_array().map(Vec::len), Some(0));

    let err = call(&mut world, "editor.asset_diff", json!({ "path": "../x.ron" })).expect_err("outside");
    assert_eq!(reason(&err), Some("path_outside_project"));
    let err = call(&mut world, "editor.asset_diff", json!({})).expect_err("no path");
    assert_eq!(reason(&err), Some("bad_path"));
    let err = call(
        &mut world,
        "editor.asset_diff",
        json!({ "path": "assets/m.matgraph.ron", "rev_a": "-p" }),
    )
    .expect_err("option");
    assert_eq!(reason(&err), Some("bad_revision"));
    let mut none = agent_world(None);
    let err = call(&mut none, "editor.asset_diff", json!({ "path": "a.ron" })).expect_err("no project");
    assert_eq!(reason(&err), Some("no_project"));
}
