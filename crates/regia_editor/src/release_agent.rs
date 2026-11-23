// SPDX-License-Identifier: MPL-2.0
//! Tool-side `release.*` agent methods: `release.verify` and `release.diff` (ADR-8557,
//! `book/agents/release-verify-diff.md`), `release.sign_status` (ADR-8687) and
//! `release.upload_dry_run` (ADR-8846) (`book/agents/release-sign.md`). They are host methods the
//! editor registers on the world's `AgentExtension` (`crate::agent_ext::install`), like
//! `asset.export_glb`: each reads a release directory through the release-packaging pipeline
//! crate (`regia_release_chunks`), and a runtime crate must not link the pipeline layer
//! (`docs/02-architecture.md` § 1, ADR-10356). Their contracts are host-side data in the runtime
//! crate (`regia_agent::contract::RELEASE_HOST_CONTRACTS`); `release_contracts_match_the_schemas`
//! keeps the two equal.
//!
//! Every call is read-only. A missing parameter is `F0002`. An empty path is refused before any
//! directory is opened, so the working directory is never scanned in its place. A directory that
//! can be read answers `Ok` even when the report's `ok` field is false.

use std::path::Path;

use regia_agent::extension::AgentHandler;
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Map, Value, json};

const fn p(name: &'static str, kind: &'static str, required: bool, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const VERIFY_PARAMS: &[ParamSchema] = &[
    p("dir", "string", true, "Release directory."),
    p(
        "deep",
        "boolean",
        false,
        "Decompress chunks and check members, member hashes, and content_key.",
    ),
    p(
        "tiers",
        "array",
        false,
        "Tier names to check. Empty checks every tier.",
    ),
    p(
        "require_signature",
        "boolean",
        false,
        "When true, a manifest with no detached signature is a problem.",
    ),
    p(
        "trust_keys",
        "array",
        false,
        "Public keys (ed25519: plus 64 hex digits) allowed to sign. Empty accepts the key in the signature.",
    ),
];

const DIFF_PARAMS: &[ParamSchema] = &[
    p("old", "string", true, "Older release directory or manifest file."),
    p("new", "string", true, "Newer release directory or manifest file."),
    p("platform", "string", false, "Keep manifests for this platform."),
    p(
        "tier",
        "string",
        false,
        "Keep manifests for this tier. An empty string is a real tier.",
    ),
];

const SIGN_STATUS_PARAMS: &[ParamSchema] = &[p("dir", "string", true, "Release directory.")];

const UPLOAD_DRY_RUN_PARAMS: &[ParamSchema] = &[
    p("dir", "string", true, "Release directory."),
    p(
        "config",
        "string",
        true,
        "Path of the upload_cli = 1 TOML config.",
    ),
    p(
        "vars",
        "object",
        false,
        "Overrides of the config's vars, key to string; each key must be defined in the config.",
    ),
];

/// The tool-side release methods and their handlers, registered by `agent_ext::install`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, AgentHandler)> {
    vec![
        (
            MethodSchema {
                name: "release.verify",
                tool: "release_verify",
                description: "Check a release directory against its manifests. Shallow checks sizes and SHA-256. deep also decompresses every chunk and checks members.",
                params: VERIFY_PARAMS,
                result: "{ ok, release_dir, deep, manifests, skipped, problems, problem_count }",
                mutating: false,
                permission: Permission::Read,
            },
            verify,
        ),
        (
            MethodSchema {
                name: "release.diff",
                tool: "release_diff",
                description: "Compare two release directories or manifest files: chunks added, removed and changed, files moved once, and bytes to download.",
                params: DIFF_PARAMS,
                result: "{ pairs, tiers }",
                mutating: false,
                permission: Permission::Read,
            },
            diff,
        ),
        (
            MethodSchema {
                name: "release.sign_status",
                tool: "release_sign_status",
                description: "Read detached Ed25519 signatures next to the release manifests. A readable directory answers ok true only when every manifest signature verifies.",
                params: SIGN_STATUS_PARAMS,
                result: "{ ok, release_dir, manifests, problems, problem_count }",
                mutating: false,
                permission: Permission::Read,
            },
            sign_status,
        ),
        (
            MethodSchema {
                name: "release.upload_dry_run",
                tool: "release_upload_dry_run",
                description: "Plan a hosting-CLI upload of a release directory without starting anything: the command lines in run order and whether each credential variable is set (values are never returned).",
                params: UPLOAD_DRY_RUN_PARAMS,
                result: "{ ok, dir, files, commands, secret_env }",
                mutating: false,
                permission: Permission::Read,
            },
            upload_dry_run,
        ),
    ]
}

fn verify(_world: &mut regia_ecs::prelude::World, params: &Value) -> Result<Value, RpcError> {
    let dir = required_str(params, "dir")?;
    if dir.is_empty() || !Path::new(dir).is_dir() {
        return Err(coded(
            "release_dir",
            format!("release directory {dir} does not exist"),
            "pass a directory that holds the release manifests",
        ));
    }
    let deep = opt_bool(params, "deep")?.unwrap_or(false);
    let tiers = string_list(params, "tiers")?;
    let require_signature = opt_bool(params, "require_signature")?.unwrap_or(false);
    let trusted_keys = string_list(params, "trust_keys")?;
    let report = regia_release_chunks::verify_release(
        Path::new(dir),
        &regia_release_chunks::VerifyOptions {
            deep,
            tiers,
            require_signature,
            trusted_keys,
        },
    );
    Ok(report.to_value())
}

fn diff(_world: &mut regia_ecs::prelude::World, params: &Value) -> Result<Value, RpcError> {
    let old = required_str(params, "old")?;
    let new = required_str(params, "new")?;
    if old.is_empty() || new.is_empty() {
        return Err(coded(
            "release_input",
            "old and new must be non-empty paths",
            "pass two release directories or manifest files",
        ));
    }
    let opts = regia_release_chunks::DiffOptions {
        platform: opt_str(params, "platform")?.map(str::to_string),
        tier: opt_str(params, "tier")?.map(str::to_string),
    };
    match regia_release_chunks::diff_release(Path::new(old), Path::new(new), &opts) {
        Ok(report) => Ok(report.to_value()),
        Err(err) => Err(coded(
            err.code,
            err.message,
            "pass two readable release directories or manifest files",
        )),
    }
}

fn sign_status(_world: &mut regia_ecs::prelude::World, params: &Value) -> Result<Value, RpcError> {
    let dir = required_str(params, "dir")?;
    if dir.is_empty() || !Path::new(dir).is_dir() {
        return Err(coded(
            "release_dir",
            format!("release directory {dir} does not exist"),
            "pass a directory that holds the release manifests",
        ));
    }
    Ok(regia_release_chunks::sign_status(Path::new(dir)).to_value())
}

/// `release.upload_dry_run { dir, config, vars? }`: the plan of `regia release upload <dir> --backend
/// cli:<config> --dry-run` for an agent. Nothing is started and nothing is written. Credential values
/// are never returned: a set value is only used to refuse a config that holds it (`upload_cli_secret`).
///
/// # Errors
///
/// `F0002` when `dir` or `config` is absent or a param has the wrong type. `release_dir` when
/// `dir` is empty or not a directory. `upload_cli_config`, `upload_cli_secret`, `unsafe_path` or
/// `io_error` from the config and the plan.
fn upload_dry_run(_world: &mut regia_ecs::prelude::World, params: &Value) -> Result<Value, RpcError> {
    upload_dry_run_with(params, &|name| std::env::var(name).ok())
}

/// [`upload_dry_run`] over a credential lookup: `lookup` answers the value of one environment
/// variable, as the hosting CLI child would read it. Tests inject their own.
fn upload_dry_run_with(params: &Value, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Value, RpcError> {
    let dir = required_str(params, "dir")?;
    let config_path = required_str(params, "config")?;
    let mut overrides = Vec::new();
    if let Some(map) = opt_object(params, "vars")? {
        for (key, value) in map {
            let Some(text) = value.as_str() else {
                return Err(RpcError::wrong_type(&format!("vars.{key}"), "string"));
            };
            overrides.push((key.clone(), text.to_string()));
        }
    }
    if dir.is_empty() || !Path::new(dir).is_dir() {
        return Err(coded(
            "release_dir",
            format!("release directory {dir} does not exist"),
            "pass a directory that holds the release files",
        ));
    }
    let chunk_error = |err: regia_release_chunks::ChunkError| {
        RpcError::new(
            INVALID_PARAMS,
            err.code,
            err.message,
            "see book/agents/release-sign.md",
        )
    };
    let config = regia_release_chunks::UploadCliConfig::load(Path::new(config_path))
        .and_then(|config| config.with_vars(&overrides))
        .map_err(chunk_error)?;
    let opts = regia_release_chunks::UploadPlanOptions {
        dry_run: true,
        ..regia_release_chunks::UploadPlanOptions::default()
    };
    let plan =
        regia_release_chunks::cli_dry_run(Path::new(dir), &config, &opts, lookup).map_err(chunk_error)?;
    Ok(json!({
        "ok": true,
        "dir": dir,
        "files": plan.files,
        "commands": plan.commands,
        "secret_env": plan
            .secret_env
            .iter()
            .map(|(name, set)| json!({ "name": name, "set": set }))
            .collect::<Vec<_>>(),
    }))
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    match opt_str(params, key)? {
        Some(text) => Ok(text),
        None => Err(RpcError::invalid_params(format!("`{key}` is required"))),
    }
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(RpcError::wrong_type(key, "string")),
    }
}

fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(RpcError::wrong_type(key, "boolean")),
    }
}

fn opt_array<'a>(params: &'a Value, key: &str) -> Result<Option<&'a Vec<Value>>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => Ok(Some(values)),
        Some(_) => Err(RpcError::wrong_type(key, "array")),
    }
}

fn opt_object<'a>(params: &'a Value, key: &str) -> Result<Option<&'a Map<String, Value>>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(RpcError::wrong_type(key, "object")),
    }
}

fn string_list(params: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    let Some(values) = opt_array(params, key)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let Some(text) = value.as_str() else {
            return Err(RpcError::invalid_params(format!(
                "`{key}` must be an array of strings"
            )));
        };
        out.push(text.to_string());
    }
    Ok(out)
}

fn coded(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_agent::protocol::{Request, Response};
    use regia_agent::server::Inbound;
    use regia_app::prelude::*;
    use serde_json::json;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        regia_test_utils::unique_temp_dir(&format!("release_agent_{tag}"))
    }

    /// The editor host: the runtime agent plugin and the editor's methods, so a call passes the
    /// permission check, the parameter gate and the registered handler.
    struct Host {
        app: App,
        inbox: crossbeam_channel::Sender<Inbound>,
    }

    impl Host {
        fn new() -> Self {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .add_plugins(regia_agent::AgentPlugin);
            let mut permissions = regia_agent::AgentPermissions::default();
            permissions.config.rate_limit_per_second = u32::MAX;
            permissions.config.heavy_rate_limit_per_second = u32::MAX;
            app.insert_resource(permissions);
            crate::agent_ext::install(app.world_mut());
            let (inbox, requests) = crossbeam_channel::unbounded();
            app.insert_resource(regia_agent::AgentInbox(requests));
            Self { app, inbox }
        }

        fn call(&mut self, method: &str, params: Value) -> Response {
            let (reply, response) = crossbeam_channel::bounded(1);
            self.inbox
                .send(Inbound {
                    request: Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: method.into(),
                        params,
                    },
                    reply,
                })
                .expect("queue request");
            self.app.update();
            response.try_recv().expect("the dispatcher answered")
        }
    }

    #[test]
    fn an_empty_dir_is_release_dir_and_does_not_scan() {
        let mut world = World::new();
        let err = verify(&mut world, &json!({ "dir": "" })).expect_err("empty");
        assert_eq!(code(&err), "release_dir");
        let err = verify(&mut world, &json!({})).expect_err("missing");
        assert_eq!(code(&err), "F0002");
    }

    #[test]
    fn sign_status_empty_dir_is_release_dir_and_does_not_scan() {
        let mut world = World::new();
        let err = sign_status(&mut world, &json!({ "dir": "" })).expect_err("empty");
        assert_eq!(code(&err), "release_dir");
        let err = sign_status(&mut world, &json!({})).expect_err("missing");
        assert_eq!(code(&err), "F0002");
    }

    /// End to end through the editor host: permission, parameter gate and the handler.
    #[test]
    fn release_verify_and_diff_through_the_protocol_dispatcher() {
        let mut host = Host::new();

        let missing = host.call("release.verify", json!({ "dir": "no-such-release-dir" }));
        assert_eq!(code(&missing.error.expect("missing dir")), "release_dir");

        let empty = host.call("release.verify", json!({}));
        assert_eq!(code(&empty.error.expect("no dir")), "F0002");
        let empty_diff = host.call("release.diff", json!({}));
        assert_eq!(code(&empty_diff.error.expect("no paths")), "F0002");

        let dir = scratch("verify_e2e");
        let checked = host.call(
            "release.verify",
            json!({ "dir": dir.display().to_string(), "deep": false }),
        );
        assert!(checked.error.is_none(), "{:?}", checked.error);
        let result = checked.result.expect("report");
        assert_eq!(result["ok"], false, "{result}");
        assert!(result["problem_count"].as_u64().unwrap_or(0) > 0);

        let bad_diff = host.call(
            "release.diff",
            json!({ "old": "no-such-old", "new": "no-such-new" }),
        );
        assert_eq!(code(&bad_diff.error.expect("unreadable")), "release_input");

        let body = r#"{"format":1,"platform":"linux","tier":"low","version":"1.0.0","chunks":[{"name":"core","asset":"linux-core.tar.zst","size":1,"unpacked":1,"files":["bin/app"]}],"links":[]}"#;
        let old = dir.join("old.json");
        let new = dir.join("new.json");
        std::fs::write(&old, body).expect("old");
        std::fs::write(&new, body).expect("new");
        let compared = host.call(
            "release.diff",
            json!({
                "old": old.display().to_string(),
                "new": new.display().to_string(),
            }),
        );
        assert!(compared.error.is_none(), "{:?}", compared.error);
        let result = compared.result.expect("diff");
        assert!(
            result.get("pairs").and_then(Value::as_array).is_some(),
            "{result}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End to end through the editor host: a throwaway key, then signature status.
    #[test]
    fn release_sign_status_through_the_protocol_dispatcher() {
        let mut host = Host::new();

        let missing = host.call("release.sign_status", json!({ "dir": "no-such-release-dir" }));
        assert_eq!(code(&missing.error.expect("missing dir")), "release_dir");
        let empty = host.call("release.sign_status", json!({}));
        assert_eq!(code(&empty.error.expect("no dir")), "F0002");

        let unsigned = scratch("sign_unsigned");
        std::fs::write(
            unsigned.join("manifest-demo.json"),
            br#"{"format":1,"platform":"demo","version":"1.0.0","chunks":[],"links":[]}"#,
        )
        .expect("manifest");
        let open = host.call(
            "release.sign_status",
            json!({ "dir": unsigned.display().to_string() }),
        );
        assert!(open.error.is_none(), "{:?}", open.error);
        let result = open.result.expect("status");
        assert_eq!(result["ok"], false, "{result}");
        let problems = result["problems"].as_array().expect("problems");
        assert!(
            problems
                .iter()
                .any(|problem| problem["check"] == "manifest_signature"),
            "{result}"
        );

        let required = host.call(
            "release.verify",
            json!({
                "dir": unsigned.display().to_string(),
                "require_signature": true,
            }),
        );
        assert!(required.error.is_none(), "{:?}", required.error);
        let verified = required.result.expect("verify");
        assert_eq!(verified["ok"], false, "{verified}");

        let dir = scratch("sign_e2e");
        let keys = scratch("sign_key");
        let key = keys.join("release.key");
        std::fs::write(
            &key,
            regia_release_chunks::generate_throwaway_signing_key().expect("throwaway key"),
        )
        .expect("key file");
        std::fs::write(
            dir.join("manifest-demo.json"),
            br#"{"format":1,"platform":"demo","version":"1.0.0","chunks":[],"links":[]}"#,
        )
        .expect("manifest");
        regia_release_chunks::sign_release(&dir, &key, "2026-11-18", None).expect("sign");
        let signed = host.call("release.sign_status", json!({ "dir": dir.display().to_string() }));
        assert!(signed.error.is_none(), "{:?}", signed.error);
        let result = signed.result.expect("signed");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["manifests"][0]["verified"], true, "{result}");
        assert_eq!(result["manifests"][0]["signed"], true, "{result}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&keys);
    }

    const TOKEN: &str = "agent-test-token-77";

    fn release() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = scratch("dir");
        std::fs::write(dir.join("core.tar.zst"), b"chunk").expect("chunk");
        std::fs::write(dir.join("manifest-demo.json"), b"{}\n").expect("manifest");
        let config = scratch("cfg").join("host.toml");
        std::fs::write(
            &config,
            "upload_cli = 1\nprogram = \"hostcli-never-started\"\nput = [\"push\", \"{file}\", \"{channel}\"]\n\
             secret_env = [\"REGIA_AGENT_TEST_TOKEN\"]\n[vars]\nchannel = \"beta\"\n",
        )
        .expect("config");
        (dir, config)
    }

    #[test]
    fn the_dry_run_lists_commands_and_secret_state_and_starts_nothing() {
        let (dir, config) = release();
        let params = json!({ "dir": dir, "config": config, "vars": { "channel": "linux" } });
        let lookup = |name: &str| (name == "REGIA_AGENT_TEST_TOKEN").then(|| TOKEN.to_string());
        let out = upload_dry_run_with(&params, &lookup).expect("dry run");
        assert_eq!(out["files"], json!(["core.tar.zst", "manifest-demo.json"]));
        let core = dir.join("core.tar.zst").display().to_string();
        assert_eq!(
            out["commands"][0],
            json!(["hostcli-never-started", "push", core, "linux"])
        );
        assert_eq!(
            out["secret_env"],
            json!([{ "name": "REGIA_AGENT_TEST_TOKEN", "set": true }])
        );
        assert!(!out.to_string().contains(TOKEN));
        let out = upload_dry_run_with(&params, &|_| None).expect("unset");
        assert_eq!(out["secret_env"][0]["set"], json!(false));
    }

    #[test]
    fn bad_params_have_stable_codes() {
        let (dir, config) = release();
        let lookup = |name: &str| (name == "REGIA_AGENT_TEST_TOKEN").then(|| TOKEN.to_string());
        let err = upload_dry_run_with(&json!({ "config": config }), &lookup).expect_err("no dir");
        assert_eq!(code(&err), "F0002");
        let err = upload_dry_run_with(&json!({ "dir": dir }), &lookup).expect_err("no config");
        assert_eq!(code(&err), "F0002");
        let err = upload_dry_run_with(&json!({ "dir": "", "config": config }), &lookup).expect_err("empty");
        assert_eq!(code(&err), "release_dir");
        let err = upload_dry_run_with(&json!({ "dir": dir, "config": config, "vars": [1] }), &lookup)
            .expect_err("vars array");
        assert_eq!(code(&err), "F0002");
        let err = upload_dry_run_with(
            &json!({ "dir": dir, "config": config, "vars": { "nope": "x" } }),
            &lookup,
        )
        .expect_err("undefined var");
        assert_eq!(code(&err), "upload_cli_config");
        let err = upload_dry_run_with(
            &json!({ "dir": dir, "config": config, "vars": { "channel": TOKEN } }),
            &lookup,
        )
        .expect_err("credential in a var");
        assert_eq!(code(&err), "upload_cli_secret");
        assert!(!format!("{err:?}").contains(TOKEN));
        let missing = dir.join("missing.toml");
        let err =
            upload_dry_run_with(&json!({ "dir": dir, "config": missing }), &lookup).expect_err("no file");
        assert_eq!(code(&err), "io_error");
    }

    /// End to end through the editor host: the method is registered, Read, and answers the plan.
    #[test]
    fn release_upload_dry_run_through_the_protocol_dispatcher() {
        let mut host = Host::new();

        let (dir, config) = release();
        let answer = host.call(
            "release.upload_dry_run",
            json!({ "dir": dir.display().to_string(), "config": config.display().to_string() }),
        );
        assert!(answer.error.is_none(), "{:?}", answer.error);
        let result = answer.result.expect("plan");
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["files"], json!(["core.tar.zst", "manifest-demo.json"]));
        assert_eq!(result["commands"].as_array().map(Vec::len), Some(2));
        let missing = host.call(
            "release.upload_dry_run",
            json!({ "dir": "no-such-release-dir", "config": "x.toml" }),
        );
        assert_eq!(code(&missing.error.expect("missing dir")), "release_dir");
    }

    /// ADR-10356: the host-side contracts in the runtime crate name exactly the methods this
    /// module registers, their examples fit the schemas, and they list the codes the tests above
    /// see the handlers answer.
    #[test]
    fn release_contracts_match_the_schemas() {
        let methods = methods();
        let mut registered: Vec<&str> = methods.iter().map(|(schema, _)| schema.name).collect();
        registered.sort_unstable();
        let mut contracted: Vec<&str> = regia_agent::contract::RELEASE_HOST_CONTRACTS
            .iter()
            .map(|contract| contract.method)
            .collect();
        contracted.sort_unstable();
        assert_eq!(
            registered, contracted,
            "one host contract per registered release method"
        );
        for (schema, _) in &methods {
            assert_eq!(schema.permission, Permission::Read, "{}", schema.name);
            assert!(!schema.mutating, "{}", schema.name);
            let contract = regia_agent::contract::contract(schema.name)
                .unwrap_or_else(|| panic!("{} has no contract", schema.name));
            let example: Map<String, Value> = serde_json::from_str(contract.example)
                .unwrap_or_else(|error| panic!("{}: example is not a JSON object: {error}", schema.name));
            for key in example.keys() {
                assert!(
                    schema.params.iter().any(|param| param.name == key),
                    "{}: the example names undeclared `{key}`",
                    schema.name
                );
            }
            for param in schema.params.iter().filter(|param| param.required) {
                assert!(
                    example.contains_key(param.name),
                    "{}: the example lacks `{}`",
                    schema.name,
                    param.name
                );
            }
        }
        for (method, listed) in [
            ("release.verify", "release_dir"),
            ("release.diff", "release_input"),
            ("release.sign_status", "release_dir"),
            ("release.upload_dry_run", "release_dir"),
        ] {
            let contract = regia_agent::contract::contract(method).expect(method);
            assert!(
                contract.errors.contains(&listed),
                "{method} answers `{listed}`, but its contract lists only {:?}",
                contract.errors
            );
        }
    }
}
