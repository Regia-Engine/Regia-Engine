// SPDX-License-Identifier: MPL-2.0
//! Compiler-grounded examples of the documented source-scan contract.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regia_test_utils::unique_temp_dir;
use xtask::api_snapshot::diff;
use xtask::api_snapshot::scan::{ApiLine, scan_crate};

use super::{keys, signature};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/api_snapshot/fixtures")
        .join(name)
        .join("lib.rs")
}

fn scan(name: &str) -> Vec<ApiLine> {
    scan_crate(&fixture(name)).expect("scan fixture")
}

#[test]
fn nested_test_only_free_items_are_excluded() {
    let lines = scan("cfg_logic");
    for line in &lines {
        assert!(
            !line.key.contains("test_")
                && !line.signature.contains("test_required")
                && !line.signature.contains("TestType"),
            "test-only API leaked: {line:?}"
        );
    }
}

#[test]
fn test_only_trait_items_are_excluded() {
    let lines = scan("cfg_logic");
    assert_eq!(signature(&lines, "Hook"), ["pub trait Hook { .. }"]);
    assert!(signature(&lines, "Hook::test_default").is_empty());
    assert!(signature(&lines, "Hook::TEST_CONST").is_empty());
}

#[test]
fn test_only_macros_are_excluded() {
    assert!(signature(&scan("cfg_logic"), "test_macro!").is_empty());
}

#[test]
fn test_only_methods_and_enum_variants_are_excluded() {
    let lines = scan("cfg_logic");
    assert!(signature(&lines, "Holder::test_method").is_empty());
    assert!(signature(&lines, "Holder::test_impl").is_empty());
    assert_eq!(signature(&lines, "Choice::Production"), ["Production"]);
    assert!(signature(&lines, "Choice::TestVariant").is_empty());
}

#[test]
fn conditional_production_apis_keep_their_predicates() {
    let lines = scan("cfg_logic");
    for (name, predicate) in [
        ("conditional_any", "any(test, feature = \"extra\")"),
        (
            "conditional_demorgan",
            "not(all(not(test), not(feature = \"extra\")))",
        ),
        ("production", "not(test)"),
        ("never", "any()"),
        ("never_literal", "false"),
        ("production_literal_true", "any(true, test)"),
        ("conditional_negated_all", "not(all(test, feature = \"extra\"))"),
        ("conditional_negated_any", "not(any(test, feature = \"extra\"))"),
        ("feature_named_test", "feature = \"test\""),
    ] {
        assert_eq!(
            signature(&lines, name),
            [format!("#[cfg({predicate})] pub fn {name}()")],
        );
    }
    assert_eq!(
        signature(&lines, "Holder::conditional_method"),
        ["#[cfg(any(test, feature = \"extra\"))] pub fn conditional_method()"]
    );
}

#[test]
fn conditional_doc_and_deprecation_do_not_hide_or_retire_unconditionally() {
    let lines = scan("attributes");
    assert!(signature(&lines, "hidden").is_empty());
    for name in [
        "sometimes_hidden",
        "sometimes_deprecated",
        "conditional_module::visible_without_extra",
    ] {
        assert!(!signature(&lines, name).is_empty(), "{name}");
        assert!(signature(&lines, &format!("{name} deprecated")).is_empty());
    }
    assert_eq!(signature(&lines, "old deprecated"), ["since 0.9.0: use fresh"]);
    let remove = |name: &str| -> Vec<ApiLine> { lines.iter().filter(|l| l.key != name).cloned().collect() };
    assert_eq!(diff(&lines, &remove("old")).retired.len(), 1);
    assert_eq!(
        diff(&lines, &remove("sometimes_deprecated")).removed_without_deprecation(),
        ["sometimes_deprecated"]
    );
}

#[test]
fn reexports_keep_conditions_on_intermediate_aliases_and_private_modules() {
    let lines = scan("exports");
    for (name, definition) in [
        ("Exported", "pub struct Original;"),
        ("ThroughModule", "pub struct ThroughModule;"),
        ("helper", "pub fn helper()"),
        ("ThroughGlobModule", "pub struct ThroughGlobModule;"),
    ] {
        assert_eq!(
            signature(&lines, name),
            [format!(
                "{}#[cfg(feature = \"extra\")] {definition}",
                if name == "helper" {
                    ""
                } else {
                    "#[cfg(any(feature = \"extra\"))] "
                }
            )],
            "{name} lost the condition on its export route"
        );
    }
}

#[test]
fn reexported_methods_keep_the_conditions_of_their_public_owner() {
    assert_eq!(
        signature(&scan("exports"), "Exported::new"),
        ["#[cfg(any(feature = \"extra\"))] #[cfg(feature = \"extra\")] pub fn new() -> Self"]
    );
}

#[test]
fn conditional_owners_keep_the_existing_deprecation_marker_text() {
    assert_eq!(
        signature(&scan("exports"), "Exported::legacy deprecated"),
        ["since 0.9.0: use new"]
    );
}

#[test]
fn one_exported_name_keeps_each_conditional_definition() {
    let lines = scan("exports");
    assert_eq!(
        signature(&lines, "Same"),
        [
            "#[cfg(feature = \"extra\")] pub struct Multi;",
            "#[cfg(not(feature = \"extra\"))] pub struct Multi;",
        ],
        "two conditions on the same public name need two definitions"
    );
}

#[test]
fn paths_globs_and_cycles_preserve_public_names_and_sorted_output() {
    let lines = scan("exports");
    let names = keys(&lines);
    for name in ["Always", "CycleA", "CycleB", "inline::deep::from_path"] {
        assert!(names.contains(&name), "{name}: {lines:?}");
    }
    for private in [
        "implementation",
        "relay",
        "facade",
        "globbed",
        "pieces",
        "cycle_a",
        "cycle_b",
    ] {
        assert!(
            !names
                .iter()
                .any(|n| *n == private || n.starts_with(&format!("{private}::")))
        );
    }
    assert!(lines.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(lines, scan("exports"), "stable scan with cycles");
    // Original is visible via a glob only with `extra`; its shorter root spelling remains an alias.
    assert_eq!(
        signature(&lines, "Original"),
        ["#[cfg(feature = \"extra\")] pub use Exported"]
    );
}

struct CompilerFixture {
    root: PathBuf,
    library: PathBuf,
}

impl CompilerFixture {
    fn new(name: &str, cfg: &[&str]) -> Self {
        let root = unique_temp_dir("api-export-compiler");
        let library = root.join("libfixture.rlib");
        let mut command = rustc();
        command.args(["--edition=2024", "--crate-name=fixture", "--crate-type=rlib"]);
        command.arg(fixture(name)).arg("-o").arg(&library);
        for condition in cfg {
            command.arg("--cfg").arg(condition);
        }
        let output = command.output().expect("run rustc on fixture");
        assert!(
            output.status.success(),
            "{name} {cfg:?}: {}",
            diagnostics(&output)
        );
        Self { root, library }
    }

    fn probe(&self, body: &str, error: Option<&str>) {
        let source = self.root.join("probe.rs");
        std::fs::write(&source, format!("fn main() {{ {body} }}\n")).expect("write probe");
        let output = rustc()
            .args([
                "--edition=2024",
                "--crate-name=probe",
                "--emit=metadata",
                "--deny=deprecated",
            ])
            .arg(&source)
            .arg("--extern")
            .arg(format!("fixture={}", self.library.display()))
            .arg("-o")
            .arg(self.root.join("probe.rmeta"))
            .output()
            .expect("compile downstream probe");
        match error {
            None => assert!(output.status.success(), "{body}: {}", diagnostics(&output)),
            Some(code) => {
                assert!(!output.status.success(), "unexpectedly compiled: {body}");
                assert!(
                    diagnostics(&output).contains(code),
                    "{body}: {}",
                    diagnostics(&output)
                );
            }
        }
    }
}

impl Drop for CompilerFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn rustc() -> Command {
    Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
}

fn diagnostics(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn rustc_checks_selected_cfg_visibility_independently_of_the_scanner() {
    for conditions in [
        vec![],
        vec!["feature=\"extra\""],
        vec!["feature=\"test\""],
        vec!["test", "feature=\"extra\""],
        vec!["test", "feature=\"other\""],
    ] {
        let library = CompilerFixture::new("cfg_logic", &conditions);
        let test = conditions.contains(&"test");
        let extra = conditions.contains(&"feature=\"extra\"");
        let other = conditions.contains(&"feature=\"other\"");
        for (path, visible) in [
            ("test_direct", test),
            ("test_singleton_all", test),
            ("test_unreachable_first", false),
            ("test_unreachable_last", false),
            ("test_literal_true_first", test),
            ("test_literal_true_last", test),
            ("test_literal_false_first", test),
            ("test_literal_false_last", test),
            ("test_literal_nested", test),
            ("test_literal_unreachable", false),
            ("production_literal_true", true),
            ("never_literal", false),
            ("test_first", test && extra),
            ("test_last", test && extra),
            ("test_nested_all", test && extra),
            ("test_any_branches", test && (extra || other)),
            ("test_double_not", test),
            ("test_demorgan", test && !extra),
            ("test_multiple_attrs", test),
            ("conditional_any", test || extra),
            ("conditional_demorgan", test || extra),
            ("production", !test),
            ("never", false),
            ("conditional_negated_all", !(test && extra)),
            ("conditional_negated_any", !(test || extra)),
            ("feature_named_test", conditions.contains(&"feature=\"test\"")),
            ("test_module::inside", test && extra),
            ("test_export", test && extra),
        ] {
            library.probe(
                &format!("fixture::{path}();"),
                (!visible).then_some(if path.contains("::") { "E0433" } else { "E0425" }),
            );
        }
        library.probe(
            "fixture::Holder::test_method();",
            (!(test && extra)).then_some("E0599"),
        );
        library.probe(
            "fixture::Holder::test_impl();",
            (!(test && extra)).then_some("E0599"),
        );
        library.probe("fixture::test_macro!();", (!(test && extra)).then_some("E0433"));
        library.probe(
            "fixture::Holder::conditional_method();",
            (!(test || extra)).then_some("E0599"),
        );
        library.probe(
            "let _ = fixture::Choice::TestVariant;",
            (!(test && extra)).then_some("E0599"),
        );
        library.probe(
            "struct Implementor; impl fixture::Hook for Implementor {}",
            (test && extra).then_some("E0046"),
        );
        library.probe(
            "struct Implementor; impl fixture::Hook for Implementor { fn test_required() {} type TestType = (); } <Implementor as fixture::Hook>::test_default(); let _ = <Implementor as fixture::Hook>::TEST_CONST;",
            (!(test && extra)).then_some("E0407"),
        );
    }
}

#[test]
fn rustc_checks_export_routes_cycles_paths_and_privacy() {
    for conditions in [vec![], vec!["feature=\"extra\""]] {
        let library = CompilerFixture::new("exports", &conditions);
        let extra = !conditions.is_empty();
        library.probe("let _ = fixture::Same; let _ = fixture::Always; let _ = fixture::CycleA; let _ = fixture::CycleB; fixture::inline::deep::from_path();", None);
        for name in ["Exported", "ThroughModule", "Original", "ThroughGlobModule"] {
            library.probe(&format!("let _ = fixture::{name};"), (!extra).then_some("E0425"));
        }
        library.probe("fixture::helper();", (!extra).then_some("E0425"));
        library.probe("let _ = fixture::Exported::new();", (!extra).then_some("E0433"));
        library.probe(
            "fixture::Exported::legacy();",
            Some(if extra { "deprecated" } else { "E0433" }),
        );
        library.probe("let _ = fixture::implementation::Original;", Some("E0603"));
        library.probe("let _ = fixture::pieces::Always;", Some("E0433"));
    }
}

#[test]
fn rustc_checks_cfg_attr_deprecation_without_a_scanner_policy_waiver() {
    for conditions in [
        vec![],
        vec!["feature=\"extra\""],
        vec!["test", "feature=\"extra\""],
    ] {
        let library = CompilerFixture::new("attributes", &conditions);
        library.probe(
            "fixture::hidden(); fixture::sometimes_hidden(); fixture::conditional_module::visible_without_extra();",
            None,
        );
        library.probe(
            "fixture::sometimes_deprecated();",
            (conditions.contains(&"feature=\"extra\"") && !conditions.contains(&"test"))
                .then_some("deprecated"),
        );
        library.probe("fixture::old();", Some("deprecated"));
    }
}

fn assert_conditional_member(key: &str, definition: &str) {
    let lines = scan("owners");
    assert_eq!(
        signature(&lines, key),
        [
            format!("#[cfg(feature = \"extra\")] {definition}"),
            format!("#[cfg(not(feature = \"extra\"))] {definition}"),
        ],
        "{key} must cover each conditional definition"
    );
    assert!(!keys(&lines).iter().any(|name| name.starts_with("implementation")));
}

#[test]
fn conditional_definitions_keep_shared_inherent_methods() {
    assert_conditional_member("ExportedOwner::shared", "pub fn shared()");
}

#[test]
fn conditional_definitions_keep_shared_trait_impls() {
    assert_conditional_member("ExportedOwner impl Marker", "impl Marker for Conditional");
}

#[test]
fn conditional_traits_keep_shared_external_type_impls() {
    assert_conditional_member("Marker impl Marker", "impl Marker for u8");
}

#[test]
fn rustc_checks_shared_impls_for_conditional_definitions() {
    for conditions in [vec![], vec!["feature=\"extra\""]] {
        let library = CompilerFixture::new("owners", &conditions);
        library.probe(
            "fn marker<T: fixture::Marker>() {} marker::<fixture::ExportedOwner>(); marker::<u8>(); fixture::ExportedOwner::shared();",
            None,
        );
        library.probe("let _ = fixture::implementation::Conditional;", Some("E0603"));
    }
}
