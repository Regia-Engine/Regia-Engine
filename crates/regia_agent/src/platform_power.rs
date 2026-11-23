// SPDX-License-Identifier: MPL-2.0
//! `platform.power_state`: the host power supply (ADR-3997, book/agents/platform-power.md).
//!
//! An omitted `dir` reads the host. A string `dir` reads that sysfs-shaped directory and is
//! how tests avoid the real `/sys` tree. The coverage harness supplies a temporary
//! directory for calls that would otherwise read the host.

use regia_ecs::prelude::World;
use regia_platform::{PowerSource, PowerState};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `platform.power_state`. A `dir` must lie under the allowed roots (the project, the temp
/// directory or an `output_roots` entry, `req_path::allowed_path`): it is a
/// test hook, and a read-only client must not probe any host path with it, or name a network share
/// that Windows would connect to (ADR-5869).
pub(crate) fn power_state(world: &World, params: &Value) -> Result<Value, RpcError> {
    let state = match params.get("dir") {
        None | Some(Value::Null) => regia_platform::power_state(),
        // An empty name and one holding NUL open nothing anywhere: they keep their documented
        // `power_unavailable` answer.
        Some(Value::String(dir)) if dir.is_empty() || dir.contains('\0') => {
            regia_platform::read_power_supply_dir(std::path::Path::new(dir))
        }
        Some(Value::String(dir)) => {
            let dir = crate::req_path::allowed_path(world, "dir", dir)?;
            regia_platform::read_power_supply_dir(&dir)
        }
        Some(_) => return Err(RpcError::wrong_type("dir", "a string")),
    };
    match state {
        Ok(state) => Ok(state_json(state)),
        Err(err) => Err(RpcError::new(
            protocol::UNSUPPORTED,
            "power_unavailable",
            err.to_string(),
            "omit dir to read the host, or pass a directory shaped like sysfs class power_supply",
        )),
    }
}

fn state_json(state: PowerState) -> Value {
    json!({
        "source": source_name(state.source),
        "percent": state.percent,
        "low_power": state.low_power,
    })
}

fn source_name(source: PowerSource) -> &'static str {
    match source {
        PowerSource::Battery => "battery",
        PowerSource::Ac => "ac",
        PowerSource::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::{unique_temp_dir, write_atomic};

    fn supply(kind: &str, capacity: Option<&[u8]>, status: Option<&[u8]>) -> std::path::PathBuf {
        let root = unique_temp_dir("power-agent");
        let dir = root.join("supply");
        std::fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("create: {err}"));
        write_atomic(&dir.join("type"), kind.as_bytes()).unwrap_or_else(|err| panic!("type: {err}"));
        if let Some(bytes) = capacity {
            write_atomic(&dir.join("capacity"), bytes).unwrap_or_else(|err| panic!("capacity: {err}"));
        }
        if let Some(bytes) = status {
            write_atomic(&dir.join("status"), bytes).unwrap_or_else(|err| panic!("status: {err}"));
        }
        dir
    }

    fn invoke(params: Value) -> Result<Value, RpcError> {
        let mut world = regia_app::prelude::World::new();
        crate::dispatch::call_in_process(&mut world, "platform.power_state", params)
            .unwrap_or_else(|| panic!("platform.power_state must answer synchronously"))
    }

    fn call(dir: &std::path::Path) -> Result<Value, RpcError> {
        invoke(json!({ "dir": dir.to_string_lossy() }))
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn platform_power_state_reads_a_discharging_battery() {
        let dir = supply("Battery", Some(b"15\n"), Some(b"Discharging\n"));
        let value = call(&dir).unwrap_or_else(|err| panic!("platform.power_state: {err:?}"));
        assert_eq!(value["source"], "battery");
        assert_eq!(value["percent"], 15);
        assert_eq!(value["low_power"].as_bool(), Some(true));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap_or(&dir));
    }

    #[test]
    fn platform_power_state_mains_is_ac() {
        let dir = supply("Mains", None, Some(b"Charging\n"));
        let value = call(&dir).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(value["source"], "ac");
        assert!(value["percent"].is_null());
        assert_eq!(value["low_power"].as_bool(), Some(false));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap_or(&dir));
    }

    #[test]
    fn platform_power_state_capacity_150_is_null() {
        let dir = supply("Battery", Some(b"150\n"), Some(b"Discharging\n"));
        let value = call(&dir).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(value["source"], "battery");
        assert!(value["percent"].is_null());
        assert_eq!(value["low_power"].as_bool(), Some(false));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap_or(&dir));
    }

    #[test]
    fn platform_power_state_missing_dir_is_power_unavailable() {
        let root = unique_temp_dir("power-agent-missing");
        let missing = root.join("never-created");
        let Err(err) = call(&missing) else {
            panic!("platform.power_state should refuse a missing directory");
        };
        assert_eq!(err.code, protocol::UNSUPPORTED);
        assert_eq!(code(&err), Some("power_unavailable"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// sec-review-assets-agent (ADR-5869): `dir` (Read permission, so a read-only token too) was
    /// opened as given: any host directory answered whether it exists, a FIFO planted in a shared
    /// folder blocked the world thread, and a UNC name connected to the share it names on Windows.
    #[test]
    fn platform_power_state_refuses_a_dir_outside_the_allowed_roots() {
        for dir in [r"\\attacker.example\share", "../../../../../../../../../../etc"] {
            let Err(err) = invoke(json!({ "dir": dir })) else {
                panic!("{dir} must be refused");
            };
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{dir}: {err:?}");
            assert_eq!(
                err.data
                    .as_ref()
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str),
                Some("path_outside_project"),
                "{dir}: {err:?}"
            );
        }
    }

    #[test]
    fn platform_power_state_rejects_a_non_string_dir() {
        let Err(err) = invoke(json!({ "dir": 15 })) else {
            panic!("a non-string dir is F0002");
        };
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(code(&err), Some("F0002"));
    }

    #[test]
    fn platform_power_state_enforces_the_documented_directory_limit() {
        let Err(err) = invoke(json!({ "dir": "a".repeat(4097) })) else {
            panic!("a directory over 4096 bytes must be rejected");
        };
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(code(&err), Some("F0002"));
    }

    #[test]
    fn platform_power_state_empty_dir_is_power_unavailable() {
        let Err(err) = invoke(json!({ "dir": "" })) else {
            panic!("an empty dir is power_unavailable");
        };
        assert_eq!(code(&err), Some("power_unavailable"));
    }

    #[test]
    fn platform_power_state_nul_dir_is_power_unavailable() {
        let Err(err) = invoke(json!({ "dir": "a\u{0}b" })) else {
            panic!("a path containing NUL is power_unavailable");
        };
        assert_eq!(code(&err), Some("power_unavailable"));
    }
}
