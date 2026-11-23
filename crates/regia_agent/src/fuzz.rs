// SPDX-License-Identifier: MPL-2.0
//! `fuzz.targets` and `fuzz.run` (ADR-1917, `book/agents/fuzzing.md`): list the fuzz-target registry
//! and fuzz one target, optionally with a project file as the fixture, inside a running dev build.

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::fuzz_targets::{self, FuzzTarget, FuzzTargetExtensions};
use crate::protocol::{self, RpcError};

/// Most iterations one `fuzz.run` may ask for (the call blocks the frame it runs in).
const MAX_ITERATIONS: u64 = 5000;
/// Iterations when the request names none.
const DEFAULT_ITERATIONS: u64 = 200;
/// Largest project file `fuzz.run` reads as a fixture.
const MAX_FIXTURE_BYTES: u64 = 1 << 20;

/// Whether this build unwinds on panic (a caught panic is then a finding, not the end of the process).
const UNWINDS: bool = cfg!(panic = "unwind");

fn target_json(t: &FuzzTarget, source: &str) -> Value {
    json!({
        "name": t.name,
        "format": t.format,
        "format_version": t.format_version,
        "input": t.input_kind(),
        "round_trip": t.round_trip,
        "source": source,
    })
}

/// `fuzz.targets`: every registered target, the built-in registry first, then the targets crates above
/// this one added ([`FuzzTargetExtensions`]; `source` is `"builtin"` or `"extension"`).
pub(crate) fn targets(world: &World) -> Result<Value, RpcError> {
    let mut list: Vec<Value> = fuzz_targets::targets()
        .iter()
        .map(|t| target_json(t, "builtin"))
        .collect();
    if let Some(extensions) = world.get_resource::<FuzzTargetExtensions>() {
        list.extend(extensions.targets().iter().map(|t| target_json(t, "extension")));
    }
    let count = list.len();
    Ok(json!({ "targets": list, "count": count, "available": UNWINDS }))
}

/// Reads a project-relative fixture (`regia_core::project_path` rules), refusing big files.
fn read_fixture(world: &World, path: &str) -> Result<Vec<u8>, RpcError> {
    // ADR-8798: the shared root (`AgentSettings::project_dir` counts when no asset server is set).
    let root = crate::req_path::project_root(world);
    let not_found = || {
        RpcError::not_found(
            format!("{path}: cannot be read"),
            "check the path with assets.list",
        )
    };
    let resolved = crate::req_path::project_file_in(&root, "path", path)?;
    let len = std::fs::metadata(&resolved).map_err(|_| not_found())?.len();
    if len > MAX_FIXTURE_BYTES {
        return Err(RpcError::invalid_params(format!(
            "{path} is {len} bytes; a fuzz fixture may be at most {MAX_FIXTURE_BYTES}"
        )));
    }
    std::fs::read(&resolved).map_err(|_| not_found())
}

/// `fuzz.run`: mutate one target's fixture and report what loaded, what was rejected and what broke.
pub(crate) fn run(world: &World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`target` (string) is required"))?;
    let extension = world
        .get_resource::<FuzzTargetExtensions>()
        .and_then(|extensions| extensions.target(name).copied());
    let target = fuzz_targets::target(name).copied().or(extension).ok_or_else(|| {
        RpcError::not_found(
            format!("no fuzz target {name}"),
            "list the targets with fuzz.targets",
        )
    })?;
    let seed = match params.get("seed") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`seed` must be an unsigned 64-bit integer"))?,
    };
    let iterations = match params.get("iterations") {
        None | Some(Value::Null) => DEFAULT_ITERATIONS,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_ITERATIONS).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`iterations` must be an integer in 1..={MAX_ITERATIONS}"))
            })?,
    };
    let fixture = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) => Some(read_fixture(world, path)?),
        Some(_) => return Err(RpcError::invalid_params("`path` must be a string")),
    };
    if !UNWINDS {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0016",
            "fuzz.run needs a build that unwinds on panic; this build aborts",
            "run fuzz.run in a dev build (panic = \"unwind\")",
        ));
    }
    let iterations = u32::try_from(iterations).unwrap_or(u32::MAX);
    let report =
        fuzz_targets::run(&target, fixture.as_deref(), seed, iterations).map_err(RpcError::invalid_params)?;
    let first_failure = report.first_failure.as_ref().map_or(Value::Null, |f| {
        json!({
            "iteration": f.iteration,
            "kind": f.kind.as_str(),
            "message": f.message,
            "input_len": f.input_len,
        })
    });
    Ok(json!({
        "target": target.name,
        "seed": seed,
        "iterations": iterations,
        "accepted": report.accepted,
        "rejected": report.rejected,
        "panicked": report.panicked,
        "round_trip_failures": report.round_trip_failures,
        "empty_errors": report.empty_errors,
        "bugs": report.bugs,
        "clean": report.is_clean(),
        "first_failure": first_failure,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_lists_the_registry() {
        let out = targets(&World::new()).expect("targets");
        assert_eq!(out["count"], fuzz_targets::targets().len());
        assert_eq!(out["available"], UNWINDS);
        assert_eq!(out["targets"][0]["source"], "builtin");
    }

    /// An extension target is listed after the built-ins and runs like one; a built-in name is refused.
    #[test]
    fn extension_targets_are_listed_and_run() {
        fn load(s: &str) -> Result<String, String> {
            if s.contains('x') {
                Err("has an x".into())
            } else {
                Ok(s.to_string())
            }
        }
        let added = FuzzTarget {
            name: "test.extension",
            format: None,
            format_version: 0,
            round_trip: true,
            input: fuzz_targets::FuzzInput::Text {
                fixture: || "abc".into(),
                load,
            },
        };
        let mut extensions = FuzzTargetExtensions::default();
        extensions.add(added).expect("a new name");
        extensions.add(added).expect("the same target twice is a no-op");
        let builtin = FuzzTarget {
            name: fuzz_targets::targets()[0].name,
            ..added
        };
        assert!(extensions.add(builtin).is_err());
        let mut world = World::new();
        world.insert_resource(extensions);
        let out = targets(&world).expect("targets");
        assert_eq!(out["count"], fuzz_targets::targets().len() + 1);
        let last = out["targets"]
            .as_array()
            .and_then(|l| l.last())
            .cloned()
            .unwrap_or_default();
        assert_eq!(last["name"], "test.extension");
        assert_eq!(last["source"], "extension");
        let run = run(
            &world,
            &json!({ "target": "test.extension", "seed": 2, "iterations": 30 }),
        )
        .expect("run");
        assert_eq!(run["target"], "test.extension");
        assert_eq!(run["clean"], true, "{run}");
    }

    #[test]
    fn run_validates_its_parameters() {
        let world = World::new();
        let code = |r: Result<Value, RpcError>| {
            r.expect_err("must fail")
                .data
                .and_then(|d| d["code"].as_str().map(str::to_string))
        };
        assert_eq!(code(run(&world, &json!({}))).as_deref(), Some("F0002"));
        assert_eq!(
            code(run(&world, &json!({ "target": "nope" }))).as_deref(),
            Some("F0003")
        );
        let name = fuzz_targets::targets()[0].name;
        for bad in [json!(0), json!(5001), json!(-1), json!(1.5), json!("9")] {
            assert_eq!(
                code(run(&world, &json!({ "target": name, "iterations": bad }))).as_deref(),
                Some("F0002")
            );
        }
        assert_eq!(
            code(run(&world, &json!({ "target": name, "seed": -3 }))).as_deref(),
            Some("F0002")
        );
        for path in ["../Cargo.toml", "/etc/passwd", "", "a/../../b"] {
            assert_eq!(
                code(run(&world, &json!({ "target": name, "path": path }))).as_deref(),
                Some("F0002"),
                "{path}"
            );
        }
    }

    #[test]
    fn run_reports_counts_that_add_up() {
        let world = World::new();
        let name = fuzz_targets::targets()[0].name;
        let out = run(&world, &json!({ "target": name, "seed": 3, "iterations": 40 })).expect("run");
        let sum: u64 = ["accepted", "rejected", "panicked", "empty_errors", "bugs"]
            .iter()
            .map(|k| out[*k].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(sum, 40);
        assert_eq!(out["clean"], true, "{out}");
    }

    // Spec 97 § 3 `path` rules, through the agent method (ADR-5660 audit).

    fn project_world(root: &std::path::Path) -> World {
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        let mut world = World::new();
        world.insert_resource(server);
        world
    }

    fn error_code(r: Result<Value, RpcError>) -> Option<String> {
        r.expect_err("must fail")
            .data
            .and_then(|d| d["code"].as_str().map(str::to_string))
    }

    fn text_target() -> &'static FuzzTarget {
        fuzz_targets::targets()
            .iter()
            .find(|t| matches!(t.input, fuzz_targets::FuzzInput::Text { .. }))
            .expect("the registry has a text target")
    }

    /// A project file holding the built-in fixture gives the same counts as the built-in fixture: the run is a
    /// function of (target, fixture bytes, seed, iterations) only (spec 97 § 4).
    #[test]
    fn a_project_fixture_equal_to_the_builtin_one_gives_the_same_report() {
        let root = regia_test_utils::unique_temp_dir("fuzz-run-fixture");
        let target = text_target();
        let fuzz_targets::FuzzInput::Text { fixture, .. } = target.input else {
            unreachable!("text_target returns a text target")
        };
        regia_test_utils::write_atomic(&root.join("fixture.ron"), fixture().as_bytes()).expect("write");
        let world = project_world(&root);
        let builtin = run(
            &world,
            &json!({ "target": target.name, "seed": 11, "iterations": 60 }),
        )
        .expect("builtin run");
        let from_file = run(
            &world,
            &json!({ "target": target.name, "seed": 11, "iterations": 60, "path": "fixture.ron" }),
        )
        .expect("file run");
        assert_eq!(builtin, from_file);
        let again = run(
            &world,
            &json!({ "target": target.name, "seed": 11, "iterations": 60, "path": "fixture.ron" }),
        )
        .expect("second file run");
        assert_eq!(from_file, again);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `path` may name a file of at most 1 MiB; one byte more is `F0002`, and the size check runs before the read.
    #[test]
    fn a_fixture_over_one_mebibyte_is_refused() {
        let root = regia_test_utils::unique_temp_dir("fuzz-run-big");
        let limit = usize::try_from(MAX_FIXTURE_BYTES).expect("1 MiB fits");
        regia_test_utils::write_atomic(&root.join("at_limit.ron"), &vec![b'a'; limit]).expect("write");
        regia_test_utils::write_atomic(&root.join("over_limit.ron"), &vec![b'a'; limit + 1]).expect("write");
        let world = project_world(&root);
        let name = text_target().name;
        let at_limit = run(
            &world,
            &json!({ "target": name, "iterations": 1, "path": "at_limit.ron" }),
        )
        .expect("exactly 1 MiB is allowed");
        assert_eq!(at_limit["iterations"], 1);
        assert_eq!(
            error_code(run(
                &world,
                &json!({ "target": name, "iterations": 1, "path": "over_limit.ron" })
            ))
            .as_deref(),
            Some("F0002")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing fixture is `F0003` (not found); a non-string `path` and a non-UTF-8 fixture for a text target are
    /// `F0002`.
    #[test]
    fn missing_mistyped_and_non_utf8_fixtures_are_refused() {
        let root = regia_test_utils::unique_temp_dir("fuzz-run-bad-fixture");
        regia_test_utils::write_atomic(&root.join("latin1.ron"), &[0x28, 0xE9, 0xFF, 0x29]).expect("write");
        let world = project_world(&root);
        let name = text_target().name;
        assert_eq!(
            error_code(run(&world, &json!({ "target": name, "path": "absent.ron" }))).as_deref(),
            Some("F0003")
        );
        assert_eq!(
            error_code(run(&world, &json!({ "target": name, "path": 7 }))).as_deref(),
            Some("F0002")
        );
        assert_eq!(
            error_code(run(&world, &json!({ "target": name, "path": "latin1.ron" }))).as_deref(),
            Some("F0002")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A link inside the project that leads out of it is refused like `..` (`F0002`), before anything is read.
    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_project_is_refused() {
        let root = regia_test_utils::unique_temp_dir("fuzz-run-link");
        let outside = regia_test_utils::unique_temp_dir("fuzz-run-link-outside");
        regia_test_utils::write_atomic(&outside.join("secret.ron"), b"()").expect("write");
        std::os::unix::fs::symlink(outside.join("secret.ron"), root.join("link.ron")).expect("symlink");
        let world = project_world(&root);
        let name = text_target().name;
        assert_eq!(
            error_code(run(&world, &json!({ "target": name, "path": "link.ron" }))).as_deref(),
            Some("F0002")
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    fn text_extension(
        name: &'static str,
        round_trip: bool,
        load: fn(&str) -> Result<String, String>,
    ) -> FuzzTarget {
        FuzzTarget {
            name,
            format: None,
            format_version: 0,
            round_trip,
            input: fuzz_targets::FuzzInput::Text {
                fixture: || "(a: 1, b: [2, 3])".into(),
                load,
            },
        }
    }

    /// Spec 97 § 2.2 / § 3: `fuzz.run` counts each failure kind and names the first by its wire name. (`xtask`'s
    /// `check_one` has its own test; this is the registry's `run` behind the agent method.)
    #[test]
    fn each_failure_kind_is_counted_and_named() {
        fn empty(_: &str) -> Result<String, String> {
            Err(String::new())
        }
        fn bug(_: &str) -> Result<String, String> {
            Err(format!(
                "{}generated code failed validation",
                fuzz_targets::BUG_PREFIX
            ))
        }
        // Appending a token keeps the re-encoding from parsing to the same RON value.
        fn unstable(s: &str) -> Result<String, String> {
            Ok(format!("{s}x"))
        }
        fn panics(_: &str) -> Result<String, String> {
            panic!("loader panicked on purpose")
        }
        let mut extensions = FuzzTargetExtensions::default();
        for target in [
            text_extension("test.empty", true, empty),
            text_extension("test.bug", true, bug),
            text_extension("test.unstable", true, unstable),
            text_extension("test.panics", true, panics),
        ] {
            extensions.add(target).expect("new name");
        }
        let mut world = World::new();
        world.insert_resource(extensions);
        for (name, counter, kind) in [
            ("test.empty", "empty_errors", "empty_error"),
            ("test.bug", "bugs", "bug"),
            ("test.unstable", "round_trip_failures", "round_trip"),
            ("test.panics", "panicked", "panic"),
        ] {
            let out = run(&world, &json!({ "target": name, "seed": 4, "iterations": 6 })).expect("run");
            assert_eq!(out[counter], 6, "{name}: {out}");
            assert_eq!(out["clean"], false, "{name}");
            assert_eq!(out["first_failure"]["kind"], kind, "{name}");
            assert_eq!(out["first_failure"]["iteration"], 0, "{name}");
        }
        let out = run(&world, &json!({ "target": "test.bug", "iterations": 1 })).expect("run");
        assert!(
            out["first_failure"]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with(fuzz_targets::BUG_PREFIX)),
            "{out}"
        );
    }

    /// A target whose format has no writer sets `round_trip: false`; its output is never re-loaded (spec 97 § 2.2).
    #[test]
    fn a_target_without_a_writer_skips_the_round_trip() {
        fn unstable(s: &str) -> Result<String, String> {
            Ok(format!("{s} "))
        }
        let report =
            fuzz_targets::run(&text_extension("test.no_writer", false, unstable), None, 9, 12).expect("run");
        assert_eq!(report.accepted, 12);
        assert!(report.is_clean(), "{report:?}");
    }

    /// Re-encoded text that differs only in map order passes (spec 97 § 1 item 2); a different value does not.
    #[test]
    fn round_trip_text_accepts_map_order_and_refuses_new_values() {
        fn swaps(s: &str) -> Result<String, String> {
            Ok(if s.starts_with("{\"a\"") {
                "{\"b\": 2, \"a\": 1}".to_string()
            } else {
                "{\"a\": 1, \"b\": 2}".to_string()
            })
        }
        fn grows(s: &str) -> Result<String, String> {
            Ok(format!("[{s}]"))
        }
        assert_eq!(
            fuzz_targets::check_round_trip_text(swaps, "{\"b\": 2, \"a\": 1}"),
            Ok(())
        );
        assert!(fuzz_targets::check_round_trip_text(grows, "1").is_err());
        fn bytes_grow(b: &[u8]) -> Result<Vec<u8>, String> {
            Ok([b, &[0]].concat())
        }
        fn bytes_same(b: &[u8]) -> Result<Vec<u8>, String> {
            Ok(b.to_vec())
        }
        assert!(fuzz_targets::check_round_trip_bytes(bytes_grow, b"ab").is_err());
        assert_eq!(fuzz_targets::check_round_trip_bytes(bytes_same, b"ab"), Ok(()));
    }

    /// Spec 97 § 2.1: a text run feeds `mutate_text` on even iterations and `mutate_text_values` on odd ones, drawing both
    /// from one seeded stream; a bytes run feeds `mutate_bytes` only.
    #[test]
    fn text_runs_alternate_syntax_and_value_mutations_on_one_stream() {
        use regia_core::fuzz::{FuzzRng, mutate_bytes, mutate_text, mutate_text_values};
        static SEEN_TEXT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        static SEEN_BYTES: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());
        fn record_text(s: &str) -> Result<String, String> {
            SEEN_TEXT.lock().map_err(|e| e.to_string())?.push(s.to_string());
            Err("recorded".into())
        }
        fn record_bytes(b: &[u8]) -> Result<Vec<u8>, String> {
            SEEN_BYTES.lock().map_err(|e| e.to_string())?.push(b.to_vec());
            Err("recorded".into())
        }
        const BASE: &str = "Point(x: 1, y: [2.5, -3], name: \"a\", on: true)";
        let report = fuzz_targets::run(
            &text_extension("test.record", true, record_text),
            Some(BASE.as_bytes()),
            5,
            10,
        )
        .expect("run");
        assert_eq!(report.rejected, 10);
        let mut rng = FuzzRng::new(5);
        let expected: Vec<String> = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    mutate_text(BASE, &mut rng)
                } else {
                    mutate_text_values(BASE, &mut rng)
                }
            })
            .collect();
        assert_eq!(*SEEN_TEXT.lock().expect("lock"), expected);

        let bytes_target = FuzzTarget {
            input: fuzz_targets::FuzzInput::Bytes {
                fixture: || b"RGPK\x01\x00\x00\x00payload".to_vec(),
                load: record_bytes,
            },
            ..text_extension("test.record_bytes", true, record_text)
        };
        let report = fuzz_targets::run(&bytes_target, None, 6, 10).expect("run");
        assert_eq!(report.rejected, 10);
        let mut rng = FuzzRng::new(6);
        let expected: Vec<Vec<u8>> = (0..10)
            .map(|_| mutate_bytes(b"RGPK\x01\x00\x00\x00payload", &mut rng))
            .collect();
        assert_eq!(*SEEN_BYTES.lock().expect("lock"), expected);
    }
}
