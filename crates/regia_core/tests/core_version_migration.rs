// SPDX-License-Identifier: MPL-2.0
//! Public-API checks for `regia_core::version`: the migration stepping loop and the RON
//! version peek. Complements the unit tests in `src/version.rs` with error-shape, ordering and
//! malformed-input cases.

use regia_core::version::{VersionMigrationError, VersionStep, migrate, peek_ron_version};

#[derive(Debug, Default)]
struct Doc {
    log: Vec<&'static str>,
}

fn a(doc: &mut Doc) -> Result<(), String> {
    doc.log.push("a");
    Ok(())
}

fn b(doc: &mut Doc) -> Result<(), String> {
    doc.log.push("b");
    Ok(())
}

fn c(doc: &mut Doc) -> Result<(), String> {
    doc.log.push("c");
    Ok(())
}

fn boom(_: &mut Doc) -> Result<(), String> {
    Err("bad body".to_owned())
}

fn chain() -> [VersionStep<Doc>; 3] {
    [
        VersionStep {
            from: 1,
            to: 2,
            migrate: a,
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: b,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: c,
        },
    ]
}

#[test]
fn steps_run_in_version_order_regardless_of_registration_order() {
    let mut steps = chain();
    steps.reverse();
    let mut doc = Doc::default();
    assert_eq!(migrate(&mut doc, 1, &steps, 4), Ok(4));
    assert_eq!(doc.log, ["a", "b", "c"]);
}

#[test]
fn migrating_part_way_stops_at_the_target() {
    let mut doc = Doc::default();
    assert_eq!(migrate(&mut doc, 1, &chain(), 3), Ok(3));
    assert_eq!(doc.log, ["a", "b"], "the v3 -> v4 step must not run");
}

#[test]
fn a_version_at_or_above_the_target_is_returned_untouched() {
    let mut doc = Doc::default();
    assert_eq!(migrate(&mut doc, 4, &chain(), 4), Ok(4));
    assert_eq!(migrate(&mut doc, 9, &chain(), 4), Ok(9));
    assert!(
        doc.log.is_empty(),
        "no step runs when nothing is older than the target"
    );
}

#[test]
fn a_gap_in_the_middle_names_the_missing_version() {
    let steps = [
        VersionStep {
            from: 1,
            to: 2,
            migrate: a,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: c,
        },
    ];
    let mut doc = Doc::default();
    let err = migrate(&mut doc, 1, &steps, 4).expect_err("gap");
    assert_eq!(err.from, 2, "the chain stalled at v2");
    assert_eq!(err.to, 4);
    assert!(err.reason.contains("v2"), "{err}");
    assert_eq!(doc.log, ["a"], "steps before the gap already ran");
}

#[test]
fn an_empty_chain_below_the_target_is_a_gap_error() {
    let mut doc = Doc::default();
    let err = migrate(&mut doc, 0, &[], 1).expect_err("no steps");
    assert_eq!((err.from, err.to), (0, 1));
}

#[test]
fn a_failing_step_reports_its_own_versions_and_message() {
    let steps = [
        VersionStep {
            from: 1,
            to: 2,
            migrate: a,
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: boom,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: c,
        },
    ];
    let mut doc = Doc::default();
    let err = migrate(&mut doc, 1, &steps, 4).expect_err("step 2 fails");
    assert_eq!(
        err,
        VersionMigrationError {
            from: 2,
            to: 3,
            reason: "bad body".to_owned(),
        }
    );
    assert_eq!(doc.log, ["a"], "later steps do not run after a failure");
}

#[test]
fn error_display_names_both_versions_and_the_reason() {
    let err = VersionMigrationError {
        from: 2,
        to: 5,
        reason: "oops".to_owned(),
    };
    assert_eq!(err.to_string(), "migration v2 -> v5 failed: oops");
    let boxed: Box<dyn std::error::Error> = Box::new(err);
    assert!(boxed.to_string().contains("oops"));
}

#[test]
fn a_backward_step_is_rejected_without_running() {
    let steps = [VersionStep {
        from: 2,
        to: 1,
        migrate: a,
    }];
    let mut doc = Doc::default();
    let err = migrate(&mut doc, 2, &steps, 3).expect_err("backward step");
    assert!(err.reason.contains("does not advance"), "{err}");
    assert!(doc.log.is_empty());
}

#[test]
fn peek_reads_format_version_alone_and_named_structs() {
    assert_eq!(peek_ron_version("(format_version: 7)"), Ok(7));
    assert_eq!(peek_ron_version("(version: 0)"), Ok(0));
    assert_eq!(peek_ron_version("Thing(version: 3, extra: [1, 2, 3])"), Ok(3));
}

#[test]
fn peek_rejects_a_document_that_is_not_a_map() {
    let err = peek_ron_version("[1, 2, 3]").expect_err("list");
    assert!(err.contains("not a map"), "{err}");
    assert!(peek_ron_version("42").is_err());
    assert!(peek_ron_version("\"version\"").is_err());
}

#[test]
fn peek_reports_a_ron_syntax_error_instead_of_panicking() {
    assert!(peek_ron_version("(version: ").is_err());
    assert!(peek_ron_version("").is_err());
}

#[test]
fn peek_rejects_a_negative_version() {
    assert!(peek_ron_version("(version: -1)").is_err());
}

#[test]
fn peek_ignores_a_nested_version_field() {
    // Only the top level is inspected: a `version` inside a child must not be mistaken for the
    // file's version.
    assert!(peek_ron_version("(name: \"x\", inner: (version: 9))").is_err());
}
