// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask api-snapshot`: the scan's view of a small fixture crate, what counts as breaking,
//! and the check and update paths on a fixture workspace.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regia_test_utils::unique_temp_dir;
use xtask::api_snapshot::scan::{ApiLine, scan_crate, scan_crate_with};
use xtask::api_snapshot::{
    ApiDiff, Args, Policy, SCOPED_SURFACES, STABLE_CRATES, deprecation_findings, diff, parse, read_policy,
    render, report, rotate, run,
};

const LIB: &str = r#"
//! Fixture.
mod private;
pub mod public;
mod hidden_parent;
pub mod prelude {
    pub use crate::private::{Reexported, helper as renamed};
    pub use crate::public::*;
}
pub use private::Reexported;
pub use regia_core::Outside;
pub use serde::*;

/// Doc comments are not API.
#[derive(Clone, Debug)]
pub struct Literal {
    pub a: u32,
    pub b: Vec<String>,
}

pub struct Sealed {
    pub open: f32,
    closed: f32,
}

#[non_exhaustive]
pub struct Grows {
    pub one: u8,
}

pub struct Pair(pub u32, u32);

pub struct Unit;

pub enum Closed {
    A,
    B(u32),
    C { x: u8 },
}

#[non_exhaustive]
pub enum Open {
    A,
}

pub trait Shape<T>: Send {
    type Out;
    const SIDES: u32;
    fn area(&self, scale: T) -> f32;
    fn name(&self) -> String {
        String::new()
    }
}

impl Sealed {
    /// Makes one.
    pub fn new(open: f32) -> Self {
        Self { open, closed: 0.0 }
    }
    pub const MAX: f32 = 1.0;
    fn internal(&self) {}
    pub(crate) fn crate_only(&self) {}
}

impl<T: Copy> Shape<T> for Sealed {
    type Out = u32;
    const SIDES: u32 = 4;
    fn area(&self, _: T) -> f32 {
        0.0
    }
}

impl std::fmt::Display for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "x")
    }
}

#[cfg(target_os = "android")]
pub fn only_android(window: *mut u8) -> Option<&'static str> {
    None
}

#[cfg(not(target_os = "android"))]
pub fn only_android(window: *mut u8) -> Option<&'static str> {
    Some("x")
}

pub fn generic<'a, T: Into<String> + ?Sized>(value: &'a T, count: usize) -> Result<(u32, f32), String>
where
    T: Send,
{
    Err(String::new())
}

pub const LIMIT: usize = 4;
pub static NAME: &str = "x";
pub type Alias<T> = Vec<T>;

#[macro_export]
macro_rules! exported {
    () => {};
}

macro_rules! local {
    () => {};
}

#[doc(hidden)]
pub fn hidden() {}

pub(crate) fn crate_private() {}

fn private_fn() {}

#[cfg(test)]
pub mod tests {
    pub fn not_api() {}
}
"#;

const PRIVATE: &str = "pub struct Reexported;\npub fn helper(n: u32) -> u32 { n }\npub struct Unreachable;\n";
const PUBLIC: &str = "pub mod nested;\npub fn listed() {}\nmod inner { pub fn sealed_away() {} }\n";
const NESTED: &str = "pub fn deep() {}\n";

/// A crate tree under `root/crates/<name>` from `(path, text)` pairs.
fn write_crate(root: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let src = root.join("crates").join(name).join("src");
    for (path, text) in files {
        let file = src.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(file, text).expect("write");
    }
    src.join("lib.rs")
}

fn fixture(root: &Path, lib: &str) -> PathBuf {
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.9.1\"\n",
    )
    .expect("manifest");
    write_crate(
        root,
        "regia_app",
        &[
            ("lib.rs", lib),
            ("private.rs", PRIVATE),
            ("public.rs", PUBLIC),
            ("public/nested.rs", NESTED),
            ("hidden_parent/mod.rs", "pub fn never() {}\n"),
        ],
    )
}

fn scan(lib: &str) -> Vec<ApiLine> {
    let root = unique_temp_dir("api-scan");
    scan_crate(&fixture(&root, lib)).expect("scan")
}

fn signature<'a>(lines: &'a [ApiLine], key: &str) -> Vec<&'a str> {
    lines
        .iter()
        .filter(|l| l.key == key)
        .map(|l| l.signature.as_str())
        .collect()
}

fn keys(lines: &[ApiLine]) -> Vec<&str> {
    lines.iter().map(|l| l.key.as_str()).collect()
}

/// The fixture after `edit`, compared with the fixture.
fn after(edit: impl Fn(&str) -> String) -> ApiDiff {
    let edited = edit(LIB);
    assert_ne!(edited, LIB, "the edit must change the source");
    diff(&scan(LIB), &scan(&edited))
}

#[test]
fn only_what_a_user_can_name_is_listed() {
    let lines = scan(LIB);
    let keys = keys(&lines);
    for present in [
        "Literal",
        "Sealed",
        "Sealed.open",
        "Sealed::new",
        "Sealed::MAX",
        "Reexported",
        "prelude::Reexported",
        "prelude::renamed",
        "prelude::listed",
        "prelude::nested",
        "prelude::nested::deep",
        "public",
        "public::listed",
        "public::nested::deep",
        "exported!",
        "LIMIT",
        "NAME",
        "Alias",
    ] {
        assert!(keys.contains(&present), "{present} missing from {keys:?}");
    }
    for absent in [
        "hidden",
        "crate_private",
        "private_fn",
        "tests::not_api",
        "Unreachable",
        "private::Unreachable",
        "Sealed.closed",
        "Sealed::internal",
        "Sealed::crate_only",
        "public::inner",
        "never",
        "local!",
    ] {
        assert!(!keys.contains(&absent), "{absent} is not public");
    }
}

#[test]
fn signatures_are_normalized() {
    let lines = scan(LIB);
    assert_eq!(
        signature(&lines, "Literal"),
        ["pub struct Literal { pub a: u32, pub b: Vec<String> }"]
    );
    assert_eq!(signature(&lines, "Sealed"), ["pub struct Sealed { .. }"]);
    assert_eq!(signature(&lines, "Sealed.open"), ["pub open: f32"]);
    assert_eq!(
        signature(&lines, "Grows"),
        ["#[non_exhaustive] pub struct Grows { .. }"]
    );
    assert_eq!(signature(&lines, "Pair"), ["pub struct Pair(pub u32, _);"]);
    assert_eq!(signature(&lines, "Unit"), ["pub struct Unit;"]);
    assert_eq!(
        signature(&lines, "Closed"),
        ["pub enum Closed { A, B(u32), C { x: u8 } }"]
    );
    assert_eq!(
        signature(&lines, "Open"),
        ["#[non_exhaustive] pub enum Open { .. }"]
    );
    assert_eq!(signature(&lines, "Open::A"), ["A"]);
    assert_eq!(
        signature(&lines, "Shape"),
        ["pub trait Shape<T>: Send { type Out; const SIDES: u32; fn area(&self, _: T) -> f32; .. }"]
    );
    assert_eq!(
        signature(&lines, "Shape::name"),
        ["fn name(&self) -> String { .. }"]
    );
    assert_eq!(signature(&lines, "Sealed::new"), ["pub fn new(_: f32) -> Self"]);
    assert_eq!(signature(&lines, "Sealed::MAX"), ["pub const MAX: f32"]);
    assert_eq!(
        signature(&lines, "generic"),
        [
            "pub fn generic<'a, T: Into<String> + ?Sized>(_: &'a T, _: usize) -> Result<(u32, f32), String> where T: Send"
        ]
    );
    assert_eq!(signature(&lines, "LIMIT"), ["pub const LIMIT: usize"]);
    assert_eq!(signature(&lines, "NAME"), ["pub static NAME: &str"]);
    assert_eq!(signature(&lines, "Alias"), ["pub type Alias<T> = Vec<T>"]);
    assert_eq!(signature(&lines, "Outside"), ["pub use regia_core::Outside"]);
    assert_eq!(signature(&lines, "*"), ["pub use serde::*"]);
    assert_eq!(
        signature(&lines, "prelude::renamed"),
        ["pub fn helper(_: u32) -> u32"],
        "its only public path holds the definition"
    );
    assert_eq!(signature(&lines, "prelude::Reexported"), ["pub use Reexported"]);
    assert_eq!(signature(&lines, "Literal impl Clone"), ["derive(Clone)"]);
    assert_eq!(
        signature(&lines, "Sealed impl Display"),
        ["impl Display for Sealed"]
    );
    assert_eq!(
        signature(&lines, "Sealed impl Shape"),
        ["impl<T: Copy> Shape<T> for Sealed"]
    );
}

/// A derive behind `#[cfg_attr(..)]` (regia_net's `ecs` feature, ADR-8032) is an impl a user can
/// name under that condition: it is listed with the condition, not dropped.
#[test]
fn a_cfg_attr_derive_is_listed_with_its_condition() {
    let lib = format!(
        "{LIB}\n#[derive(Debug)]\n#[cfg_attr(feature = \"ecs\", derive(Resource, Clone), reflect(Resource))]\npub struct Gated;\n"
    );
    let lines = scan(&lib);
    assert_eq!(signature(&lines, "Gated impl Debug"), ["derive(Debug)"]);
    assert_eq!(
        signature(&lines, "Gated impl Resource"),
        ["#[cfg(feature = \"ecs\")] derive(Resource)"]
    );
    assert_eq!(
        signature(&lines, "Gated impl Clone"),
        ["#[cfg(feature = \"ecs\")] derive(Clone)"]
    );
    assert!(
        !keys(&lines).iter().any(|k| k.contains("reflect")),
        "a non-derive cfg_attr is not an impl: {:?}",
        keys(&lines)
    );
    // Moving a derive behind a condition changes its line (hosts without the feature lose it).
    let moved = diff(
        &scan(&format!("{LIB}\n#[derive(Clone)]\npub struct Gated;\n")),
        &scan(&format!(
            "{LIB}\n#[cfg_attr(feature = \"ecs\", derive(Clone))]\npub struct Gated;\n"
        )),
    );
    assert!(moved.breaking());
    assert_eq!(moved.changed.len(), 1);
    assert_eq!(moved.changed[0].key, "Gated impl Clone");
    assert_eq!(moved.changed[0].now, ["#[cfg(feature = \"ecs\")] derive(Clone)"]);
}

#[test]
fn a_cfg_item_is_listed_once_per_condition_on_every_host() {
    let lines = scan(LIB);
    let mut found = signature(&lines, "only_android");
    found.sort_unstable();
    assert_eq!(
        found,
        [
            "#[cfg(not(target_os = \"android\"))] pub fn only_android(_: *mut u8) -> Option<&'static str>",
            "#[cfg(target_os = \"android\")] pub fn only_android(_: *mut u8) -> Option<&'static str>",
        ]
    );
}

#[test]
fn comments_bodies_and_argument_names_change_nothing() {
    let quiet = after(|s| {
        s.replace("/// Makes one.", "/// Makes a new one.")
            .replace("Self { open, closed: 0.0 }", "Self { open, closed: 1.0 }")
            .replace("value: &'a T, count: usize", "input: &'a T, n: usize")
            .replace(
                "pub fn only_android(window: *mut u8) -> Option<&'static str> {\n    None",
                "pub fn only_android(\n    window: *mut u8,\n) -> Option<&'static str> {\n    None",
            )
            .replace(
                "pub type Alias<T> = Vec<T>;",
                "pub type Alias<\n    T,\n> = Vec<T>;",
            )
    });
    assert!(quiet.is_empty(), "{quiet:?}");
}

#[test]
fn removing_or_changing_an_item_is_breaking() {
    let removed = after(|s| s.replace("pub const LIMIT: usize = 4;\n", ""));
    assert!(removed.breaking());
    assert_eq!(removed.removed.len(), 1);
    assert_eq!(removed.removed[0].key, "LIMIT");

    let changed = after(|s| s.replace("pub const LIMIT: usize = 4;", "pub const LIMIT: u32 = 4;"));
    assert!(changed.breaking());
    assert_eq!(changed.changed.len(), 1);
    assert_eq!(changed.changed[0].key, "LIMIT");
    assert_eq!(changed.changed[0].was, ["pub const LIMIT: usize"]);
    assert_eq!(changed.changed[0].now, ["pub const LIMIT: u32"]);

    let text = report("regia_app", &changed);
    assert!(text.contains("regia_app: changed LIMIT"), "{text}");
    assert!(text.contains("was: pub const LIMIT: usize"), "{text}");
    assert!(text.contains("now: pub const LIMIT: u32"), "{text}");

    let moved = after(|s| {
        s.replace(
            "impl Sealed {\n    /// Makes one.\n    pub fn new",
            "impl Sealed {\n    fn new",
        )
    });
    assert!(moved.removed.iter().any(|l| l.key == "Sealed::new"), "{moved:?}");
}

#[test]
fn a_struct_literal_or_a_match_breaks_on_a_new_member() {
    let field = after(|s| s.replace("pub a: u32,", "pub a: u32,\n    pub c: u8,"));
    assert!(field.breaking(), "a new field breaks struct literals: {field:?}");
    assert_eq!(field.changed[0].key, "Literal");

    let variant = after(|s| s.replace("    A,\n    B(u32),", "    A,\n    New,\n    B(u32),"));
    assert!(
        variant.breaking(),
        "a new variant breaks exhaustive matches: {variant:?}"
    );
    assert_eq!(variant.changed[0].key, "Closed");

    let required = after(|s| s.replace("    type Out;\n", "    type Out;\n    fn more(&self);\n"));
    assert!(
        required.breaking(),
        "a new required method breaks impls: {required:?}"
    );
    assert_eq!(required.changed[0].key, "Shape");
}

#[test]
fn additive_changes_are_not_breaking() {
    type Edit = fn(&str) -> String;
    let cases: [(&str, Edit); 5] = [
        ("Grows.two", |s| {
            s.replace("pub one: u8,", "pub one: u8,\n    pub two: u8,")
        }),
        ("Open::B", |s| {
            s.replace("pub enum Open {\n    A,", "pub enum Open {\n    A,\n    B,")
        }),
        ("Shape::extra", |s| {
            s.replace(
                "    type Out;\n",
                "    type Out;\n    fn extra(&self) -> u8 { 0 }\n",
            )
        }),
        ("Sealed::more", |s| {
            s.replace(
                "    fn internal(&self) {}",
                "    pub fn more(&self) {}\n    fn internal(&self) {}",
            )
        }),
        ("brand_new", |s| format!("{s}\npub fn brand_new() {{}}\n")),
    ];
    for (key, edit) in cases {
        let d = after(edit);
        assert!(!d.breaking(), "{key}: {d:?}");
        assert!(d.added.iter().any(|l| l.key == key), "{key}: {d:?}");
    }
    let private_field = after(|s| s.replace("    closed: f32,", "    closed: f32,\n    more: f32,"));
    assert!(
        private_field.is_empty(),
        "a private field is not API: {private_field:?}"
    );
}

#[test]
fn a_struct_that_gains_a_private_field_stops_being_a_literal() {
    let d = after(|s| s.replace("pub a: u32,", "pub a: u32,\n    private: u8,"));
    assert!(d.breaking(), "struct literals stop compiling: {d:?}");
}

#[test]
fn snapshot_text_round_trips_and_a_hand_edit_is_caught() {
    let lines = scan(LIB);
    let text = render(&lines);
    assert!(text.starts_with("# Generated"), "{text}");
    assert_eq!(parse(&text).expect("parses"), lines);
    let err = parse("# header\nno tab here\n").expect_err("corrupt");
    assert!(err.to_string().contains("line 2"), "{err}");
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(sorted, lines, "lines are sorted");
}

#[test]
fn a_use_cycle_ends() {
    let cycle = "mod a;\nmod b;\npub use a::*;\npub use b::Thing;\npub mod c { pub use super::c::*; }\n";
    let root = unique_temp_dir("api-cycle");
    let lib = write_crate(
        &root,
        "regia_app",
        &[
            ("lib.rs", cycle),
            (
                "a.rs",
                "pub use crate::b::*;\npub use crate::a::Loop;\npub fn thing() {}\n",
            ),
            ("b.rs", "pub use crate::a::*;\npub struct Thing;\n"),
        ],
    );
    let lines = scan_crate(&lib).expect("scan");
    assert!(lines.iter().any(|l| l.key == "Thing"), "{lines:?}");
    assert!(lines.iter().any(|l| l.key == "thing"), "{lines:?}");
}

#[test]
fn a_path_attribute_is_relative_to_the_file_that_holds_it() {
    let root = unique_temp_dir("api-path");
    let lib = write_crate(
        &root,
        "regia_app",
        &[
            ("lib.rs", "pub mod outer;\n"),
            (
                "outer.rs",
                "#[path = \"side.rs\"]\npub mod side;\npub mod inline {\n    #[path = \"deep.rs\"]\n    pub mod deep;\n}\n",
            ),
            ("side.rs", "pub mod beside;\npub fn from_side() {}\n"),
            ("beside.rs", "pub fn from_beside() {}\n"),
            ("outer/inline/deep.rs", "pub fn from_deep() {}\n"),
        ],
    );
    let lines = scan_crate(&lib).expect("scan");
    let keys = keys(&lines);
    assert!(keys.contains(&"outer::side::from_side"), "{keys:?}");
    assert!(keys.contains(&"outer::side::beside::from_beside"), "{keys:?}");
    assert!(keys.contains(&"outer::inline::deep::from_deep"), "{keys:?}");
}

#[test]
fn a_missing_module_file_is_an_error() {
    let root = unique_temp_dir("api-missing");
    let lib = write_crate(&root, "regia_app", &[("lib.rs", "pub mod gone;\n")]);
    let err = scan_crate(&lib).expect_err("no file");
    assert!(format!("{err:#}").contains("gone"), "{err:#}");
}

fn args(update: bool, breaking: Option<&str>) -> Args {
    Args {
        update,
        breaking: breaking.map(str::to_owned),
        only: Some("regia_app".to_owned()),
        rotate: None,
    }
}

fn is_success(code: ExitCode) -> bool {
    code == ExitCode::SUCCESS
}

#[test]
fn the_check_and_the_update_guard_a_workspace() {
    let root = unique_temp_dir("api-run");
    let lib = fixture(&root, LIB);
    assert!(
        !is_success(run(&root, &args(false, None)).expect("runs")),
        "a stable crate without a snapshot fails"
    );
    assert!(
        is_success(run(&root, &args(true, None)).expect("runs")),
        "first write"
    );
    let snapshot = root.join("docs/api-snapshots/regia_app.txt");
    let first = std::fs::read_to_string(&snapshot).expect("written");
    assert!(is_success(run(&root, &args(false, None)).expect("runs")), "clean");

    // An addition passes and leaves the snapshot for the release lane.
    std::fs::write(&lib, format!("{LIB}\npub fn added() {{}}\n")).expect("edit");
    assert!(is_success(run(&root, &args(false, None)).expect("runs")));
    assert_eq!(std::fs::read_to_string(&snapshot).expect("read"), first);

    // A removal fails, the update refuses without a reason and writes nothing.
    std::fs::write(&lib, LIB.replace("pub const LIMIT: usize = 4;\n", "")).expect("edit");
    assert!(!is_success(run(&root, &args(false, None)).expect("runs")));
    assert!(!is_success(run(&root, &args(true, None)).expect("runs")));
    assert_eq!(std::fs::read_to_string(&snapshot).expect("read"), first);
    assert!(!root.join("docs/api-snapshots/BREAKING.md").exists());

    // With a reason it writes the snapshot and logs the change.
    assert!(is_success(
        run(&root, &args(true, Some("LIMIT moved to the settings"))).expect("runs")
    ));
    assert!(
        is_success(run(&root, &args(false, None)).expect("runs")),
        "clean again"
    );
    let log = std::fs::read_to_string(root.join("docs/api-snapshots/BREAKING.md")).expect("log");
    assert!(
        log.contains("| 0.9.1 | regia_app | removed | `LIMIT` | LIMIT moved to the settings |"),
        "{log}"
    );

    let wrong = Args {
        only: Some("regia_cli".to_owned()),
        ..Args::default()
    };
    assert!(run(&root, &wrong).is_err(), "not a stable crate");
}

#[test]
fn a_corrupt_snapshot_fails_instead_of_reading_as_empty() {
    let root = unique_temp_dir("api-corrupt");
    fixture(&root, LIB);
    std::fs::create_dir_all(root.join("docs/api-snapshots")).expect("mkdir");
    std::fs::write(root.join("docs/api-snapshots/regia_app.txt"), "<<<<<<< HEAD\n").expect("write");
    let err = run(&root, &args(false, None)).expect_err("corrupt");
    assert!(format!("{err:#}").contains("no tab"), "{err:#}");
}

#[test]
fn every_reviewed_crate_is_a_stable_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");
    let review = std::fs::read_to_string(root.join("docs/api-review.md")).expect("review");
    for heading in review.lines().filter_map(|l| l.strip_prefix("## ")) {
        if let Some(name) = heading
            .split_whitespace()
            .next()
            .filter(|n| n.starts_with("regia_"))
        {
            assert!(
                STABLE_CRATES.contains(&name),
                "{name} is reviewed but not snapshotted"
            );
        }
    }
    assert!(STABLE_CRATES.contains(&"regia"), "the umbrella crate");
    let mut sorted = STABLE_CRATES.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, STABLE_CRATES);
    for name in STABLE_CRATES {
        assert!(
            root.join("crates").join(name).join("src/lib.rs").is_file(),
            "{name}"
        );
    }
}

// ---- deprecation awareness (ADR-2527) ----

const DEPRECATED_LIB: &str = r#"
#[deprecated(since = "0.9.0", note = "use `fresh`")]
pub fn old_fn() {}
pub fn fresh() {}
pub struct Holder { pub keep: u8 }
impl Holder {
    #[deprecated = "gone soon"]
    pub fn legacy(&self) {}
}
#[non_exhaustive]
pub enum Kind {
    A,
    #[deprecated(since = "0.9.1", note = "use A")]
    Old,
}
"#;

fn scan_text(lib: &str) -> Vec<ApiLine> {
    let root = unique_temp_dir("api-dep");
    let file = write_crate(&root, "regia_app", &[("lib.rs", lib)]);
    scan_crate(&file).expect("scan")
}

#[test]
fn deprecations_are_recorded_per_item() {
    let lines = scan_text(DEPRECATED_LIB);
    assert_eq!(
        signature(&lines, "old_fn deprecated"),
        ["since 0.9.0: use `fresh`"]
    );
    assert_eq!(
        signature(&lines, "Holder::legacy deprecated"),
        ["since ?: gone soon"]
    );
    assert_eq!(signature(&lines, "Kind::Old deprecated"), ["since 0.9.1: use A"]);
    assert!(signature(&lines, "fresh deprecated").is_empty());
}

#[test]
fn deprecated_then_removed_is_a_retirement() {
    let old = scan_text(DEPRECATED_LIB);
    let new = scan_text(&DEPRECATED_LIB.replace(
        "#[deprecated(since = \"0.9.0\", note = \"use `fresh`\")]\npub fn old_fn() {}\n",
        "",
    ));
    let d = diff(&old, &new);
    assert!(!d.breaking(), "{d:?}");
    assert!(d.removed_without_deprecation().is_empty());
    assert_eq!(d.retired.len(), 1);
    assert_eq!(d.retired[0].key, "old_fn");
    assert!(!d.is_empty(), "the snapshot still has to be rewritten");
}

#[test]
fn removed_directly_is_a_finding() {
    let old = scan_text(DEPRECATED_LIB);
    let new = scan_text(&DEPRECATED_LIB.replace("pub fn fresh() {}\n", ""));
    let d = diff(&old, &new);
    assert!(d.breaking());
    assert_eq!(d.removed_without_deprecation(), ["fresh"]);
    let warn = deprecation_findings("regia_app", &d, Policy { freeze: false });
    assert_eq!(warn, "warning: regia_app: removed_without_deprecation fresh\n");
    let error = deprecation_findings("regia_app", &d, Policy { freeze: true });
    assert_eq!(error, "error: regia_app: removed_without_deprecation fresh\n");
}

#[test]
fn a_removed_type_names_only_itself() {
    let old = scan_text(DEPRECATED_LIB);
    let new = scan_text(&DEPRECATED_LIB.replace("pub struct Holder { pub keep: u8 }\nimpl Holder {\n    #[deprecated = \"gone soon\"]\n    pub fn legacy(&self) {}\n}\n", ""));
    let d = diff(&old, &new);
    assert_eq!(d.removed_without_deprecation(), ["Holder"]);
}

#[test]
fn deprecating_an_item_is_not_breaking_and_updates_the_snapshot() {
    let plain = scan_text("pub fn fresh() {}\npub fn soon() {}\n");
    let marked =
        scan_text("pub fn fresh() {}\n#[deprecated(since = \"1\", note = \"n\")]\npub fn soon() {}\n");
    let d = diff(&plain, &marked);
    assert!(!d.breaking());
    assert!(d.deprecations_changed);
    assert!(!d.is_empty());
    let back = diff(&marked, &plain);
    assert!(!back.breaking(), "un-deprecating breaks nobody");
}

fn write_policy(root: &Path, text: &str) {
    let dir = root.join("docs/api-snapshots");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("policy.toml"), text).expect("policy");
}

#[test]
fn the_freeze_switch_turns_the_finding_into_an_error() {
    let root = unique_temp_dir("api-freeze");
    let lib = fixture(&root, DEPRECATED_LIB);
    assert_eq!(
        read_policy(&root).expect("policy"),
        Policy { freeze: false },
        "missing file"
    );
    assert!(is_success(run(&root, &args(true, None)).expect("runs")));

    // Deprecated, then removed: passes even when frozen.
    write_policy(&root, "freeze = true\n");
    assert!(read_policy(&root).expect("policy").freeze);
    std::fs::write(
        &lib,
        DEPRECATED_LIB.replace(
            "#[deprecated(since = \"0.9.0\", note = \"use `fresh`\")]\npub fn old_fn() {}\n",
            "",
        ),
    )
    .expect("edit");
    assert!(
        is_success(run(&root, &args(false, None)).expect("runs")),
        "retirement"
    );
    assert!(
        is_success(run(&root, &args(true, None)).expect("runs")),
        "no reason needed"
    );
    assert!(is_success(run(&root, &args(false, None)).expect("runs")), "clean");

    // Removed directly: before the freeze --breaking accepts it, after it nothing does.
    std::fs::write(
        &lib,
        DEPRECATED_LIB.replace("pub fn fresh() {}\n", "").replace(
            "#[deprecated(since = \"0.9.0\", note = \"use `fresh`\")]\npub fn old_fn() {}\n",
            "",
        ),
    )
    .expect("edit");
    assert!(!is_success(run(&root, &args(false, None)).expect("runs")));
    assert!(
        !is_success(run(&root, &args(true, Some("cut it"))).expect("runs")),
        "frozen: a reason does not help"
    );
    write_policy(&root, "freeze = false\n");
    assert!(
        is_success(run(&root, &args(true, Some("cut it"))).expect("runs")),
        "pre-freeze a deliberate break is accepted"
    );
    assert!(is_success(run(&root, &args(false, None)).expect("runs")));

    write_policy(&root, "freeze = \"yes\"\n");
    assert!(read_policy(&root).is_err(), "freeze must be a boolean");
}

// ---- module-scoped surfaces ----

#[test]
fn a_module_scoped_surface_lists_only_that_module() {
    assert!(SCOPED_SURFACES.contains(&("regia_editor", "extension")));
    let root = unique_temp_dir("api-scoped");
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.9.1\"\n",
    )
    .expect("manifest");
    write_crate(
        &root,
        "regia_editor",
        &[
            ("lib.rs", "pub mod extension;\npub mod other;\npub fn top() {}\n"),
            (
                "extension.rs",
                "pub struct Tool;\nimpl Tool { pub fn run(&self) {} }\npub fn register() {}\n",
            ),
            ("other.rs", "pub fn private_to_us() {}\n"),
        ],
    );
    let scoped = Args {
        update: true,
        only: Some("regia_editor.extension".to_owned()),
        ..Args::default()
    };
    assert!(is_success(run(&root, &scoped).expect("runs")));
    let text =
        std::fs::read_to_string(root.join("docs/api-snapshots/regia_editor.extension.txt")).expect("written");
    let lines = parse(&text).expect("parse");
    let names = keys(&lines);
    assert!(
        names.contains(&"extension::Tool") && names.contains(&"extension::Tool::run"),
        "{names:?}"
    );
    assert!(names.contains(&"extension::register"), "{names:?}");
    assert!(
        !names.iter().any(|k| k.contains("other") || *k == "top"),
        "{names:?}"
    );
    // The plain crate name is not a selector for the scoped surface.
    let whole = Args {
        only: Some("regia_editor".to_owned()),
        ..Args::default()
    };
    assert!(run(&root, &whole).is_err());
    // Unrelated changes outside the module do not move the snapshot.
    std::fs::write(
        root.join("crates/regia_editor/src/other.rs"),
        "pub fn changed(a: u8) {}\n",
    )
    .expect("edit");
    let check = Args {
        update: false,
        ..scoped
    };
    assert!(is_success(run(&root, &check).expect("runs")));
}

// ---- BREAKING.md rotation ----

const LOG: &str = "# Accepted breaking API changes\n\nText.\n\n| Version | Crate | Kind | Item | Why |\n|---|---|---|---|---|\n| 0.8.0 | a | removed | `x` | old |\n| 0.9.0 | a | changed | `y` | newer |\n| 0.8.0 | b | changed | `z` | old again |\n";

fn log_tree(log: &str) -> PathBuf {
    let root = unique_temp_dir("api-rotate");
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.9.1\"\n",
    )
    .expect("manifest");
    std::fs::create_dir_all(root.join("docs/api-snapshots")).expect("mkdir");
    std::fs::write(root.join("docs/api-snapshots/BREAKING.md"), log).expect("log");
    root
}

#[test]
fn rotation_moves_the_rows_of_a_released_version() {
    let root = log_tree(LOG);
    assert_eq!(rotate(&root, "0.8.0").expect("rotates"), 2);
    let kept = std::fs::read_to_string(root.join("docs/api-snapshots/BREAKING.md")).expect("log");
    assert!(kept.contains("| 0.9.0 | a | changed | `y` | newer |"));
    assert!(!kept.contains("0.8.0"), "{kept}");
    assert!(
        kept.contains("|---|---|---|---|---|") && kept.contains("Text."),
        "header stays: {kept}"
    );
    let moved = std::fs::read_to_string(root.join("docs/api-snapshots/breaking/0.8.0.md")).expect("archive");
    assert!(moved.contains("| 0.8.0 | a | removed | `x` | old |"));
    assert!(moved.contains("| 0.8.0 | b | changed | `z` | old again |"));
    assert!(!moved.contains("0.9.0"));
    // The rotation goes through `run` too and a second one finds nothing.
    let again = Args {
        rotate: Some("0.8.0".to_owned()),
        ..Args::default()
    };
    assert!(run(&root, &again).is_err(), "nothing left to rotate");
}

#[test]
fn rotation_refuses_an_unreleased_or_unreadable_log() {
    let root = log_tree(LOG);
    let err = rotate(&root, "0.9.1").expect_err("current version");
    assert!(format!("{err:#}").contains("unreleased"), "{err:#}");
    let broken = log_tree(&format!("{LOG}| next | a | removed | `q` | ambiguous |\n"));
    let err = rotate(&broken, "0.8.0").expect_err("ambiguous row");
    assert!(format!("{err:#}").contains("nothing rotated"), "{err:#}");
    let after = std::fs::read_to_string(broken.join("docs/api-snapshots/BREAKING.md")).expect("log");
    assert!(after.contains("| 0.8.0 | a | removed"), "untouched");
    assert!(!broken.join("docs/api-snapshots/breaking").exists());
    assert!(rotate(&root, "../x").is_err(), "not a version");
}

#[path = "api_snapshot/export_fixtures.rs"]
mod api_snapshot_export_fixtures;

const DET_GRID: &str = "pub struct Cell { pub x: u32 }\nimpl Cell {\n    pub fn at(x: u32) -> Self { Self { x } }\n}\n\
                        pub(crate) fn inner() {}\n";

/// ADR-8877: a module that moved into its own workspace crate and is re-exported whole under its old
/// name (`regia_nav::det` -> `pub use regia_nav_det as det;`, listed in `MOVED_MODULES`) keeps the
/// same snapshot lines, so the move is no API break. `crate::` inside the sibling names the sibling;
/// an outside crate re-exported whole (`bevy_ecs`) and a sibling not listed as moved (the `regia`
/// facade's re-exports) stay one line as written.
#[test]
fn a_crate_reexported_whole_lists_like_the_module_it_replaced() {
    let as_module = unique_temp_dir("api-scan-graft-mod");
    let before = write_crate(
        &as_module,
        "host",
        &[
            ("lib.rs", "pub mod det;\npub use bevy_ecs as ecs;\n"),
            ("det.rs", "pub mod grid;\npub use self::grid::Cell as Alias;\n"),
            ("det/grid.rs", DET_GRID),
        ],
    );
    let as_crate = unique_temp_dir("api-scan-graft-crate");
    write_crate(
        &as_crate,
        "host_det",
        &[
            ("lib.rs", "pub mod grid;\npub use crate::grid::Cell as Alias;\n"),
            ("grid.rs", DET_GRID),
        ],
    );
    let after = write_crate(
        &as_crate,
        "host",
        &[(
            "lib.rs",
            "/// The moved module.\npub use host_det as det;\npub use bevy_ecs as ecs;\n",
        )],
    );
    let before = scan_crate(&before).expect("scan the module form");
    let unlisted = scan_crate(&after).expect("scan the crate form, sibling not listed as moved");
    assert_eq!(keys(&unlisted), ["det", "ecs"]);
    assert_eq!(signature(&unlisted, "det"), ["pub use host_det"]);
    let after = scan_crate_with(&after, &["host_det"]).expect("scan the crate form");
    for key in [
        "det",
        "det::grid",
        "det::grid::Cell",
        "det::Alias",
        "det::Alias::at",
    ] {
        assert!(
            keys(&after).contains(&key),
            "{key} missing from {:?}",
            keys(&after)
        );
    }
    assert!(!keys(&after).contains(&"det::grid::inner"), "{:?}", keys(&after));
    assert_eq!(signature(&after, "ecs"), ["pub use bevy_ecs"]);
    assert_eq!(before, after);
}
