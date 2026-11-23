// SPDX-License-Identifier: MPL-2.0
//! `package.parity` through the real dispatcher (ADR-8428).

use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::prelude::*;
use serde_json::{Value, json};

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(AgentPermissions {
        config: AgentConfig {
            read_only: true,
            ..Default::default()
        },
        source: None,
    });
    world
}

fn call(world: &mut World, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    match call_local(world, "package.parity", params, None) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("package.parity must be immediate"),
    }
}

fn code(err: &regia_agent::protocol::RpcError) -> Option<&str> {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

#[test]
fn empty_params_are_parity_bad_params() {
    let err = call(&mut world(), json!({})).expect_err("missing paths");
    assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    assert_eq!(code(&err), Some("parity_bad_params"));
}

#[test]
fn dry_run_returns_the_plan() {
    let result = call(&mut world(), json!({"dry_run": true})).expect_err_as_ok();
    assert_eq!(result["ok"], true);
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["method"], "package.parity");
}

trait ExpectOk {
    fn expect_err_as_ok(self) -> Value;
}

impl ExpectOk for Result<Value, regia_agent::protocol::RpcError> {
    fn expect_err_as_ok(self) -> Value {
        match self {
            Ok(value) => value,
            Err(err) => panic!("rpc error: {err:?}"),
        }
    }
}

#[test]
fn a_string_dry_run_is_a_type_error() {
    let err = call(&mut world(), json!({"dry_run": "yes"})).expect_err("type");
    assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    assert_eq!(code(&err), Some("F0002"));
}

#[test]
fn missing_trace_and_bad_root_are_named() {
    let tmp = regia_test_utils::TempDir::new("agent-parity-bad");
    let err = call(
        &mut world(),
        json!({
            "dev_trace": tmp.join("missing.log").display().to_string(),
            "dev_root": tmp.path().display().to_string(),
            "package_trace": tmp.join("also-missing.log").display().to_string(),
            "package_root": tmp.path().display().to_string(),
        }),
    )
    .expect_err("trace");
    assert_eq!(code(&err), Some("parity_trace_missing"));
    let dir = tmp.join("not-a-dir");
    std::fs::write(&dir, b"x").unwrap_or_else(|err| panic!("{err}"));
    let trace = tmp.join("dev.log");
    std::fs::write(&trace, b"").unwrap_or_else(|err| panic!("{err}"));
    let err = call(
        &mut world(),
        json!({
            "dev_trace": trace.display().to_string(),
            "dev_root": dir.display().to_string(),
            "package_trace": trace.display().to_string(),
            "package_root": tmp.path().display().to_string(),
        }),
    )
    .expect_err("dir");
    assert_eq!(code(&err), Some("parity_not_a_directory"));
}

#[test]
fn file_missing_from_package_is_in_the_result() {
    let tmp = regia_test_utils::TempDir::new("agent-parity-miss");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    std::fs::create_dir_all(dev.join("assets")).unwrap_or_else(|err| panic!("{err}"));
    std::fs::create_dir_all(&pkg).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dev.join("assets/needed.txt"), b"needed").unwrap_or_else(|err| panic!("{err}"));
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    std::fs::write(
        &dev_trace,
        format!("ok open {}/assets/needed.txt\n", dev.display()),
    )
    .unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(&pkg_trace, b"").unwrap_or_else(|err| panic!("{err}"));
    let result = call(
        &mut world(),
        json!({
            "dev_trace": dev_trace.display().to_string(),
            "dev_root": dev.display().to_string(),
            "package_trace": pkg_trace.display().to_string(),
            "package_root": pkg.display().to_string(),
        }),
    )
    .expect_err_as_ok();
    assert_eq!(result["ok"], false);
    assert_eq!(result["exit_code"], 1);
    assert_eq!(result["missing"], json!(["assets/needed.txt"]));
}
