// SPDX-License-Identifier: MPL-2.0
//! Read-only source-lint policy and rule catalog (ADR-8248).

use regia_agent::extension::AgentHandler;
use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::gltf_material_lint::importer_capability;
use regia_asset_pipeline::source_lint::{self, SOURCE_LINT_REPORT_VERSION};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

const HINT: &str = "see book/agents/content-lint.md";

/// Schemas and handlers installed by the editor.
#[must_use]
pub fn methods() -> [(MethodSchema, AgentHandler); 2] {
    const POLICY_PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "path",
            kind: "string",
            required: false,
            description: "Optional project-relative asset path. Omitted means project and profile defaults, with no asset overlay. The file need not exist.",
        },
        ParamSchema {
            name: "target",
            kind: "string",
            required: false,
            description: "Named profile. Omitted means default. default is always valid; any other name must be a profile key.",
        },
    ];
    [
        (
            MethodSchema {
                name: "asset.content_lint_rules",
                tool: "asset_content_lint_rules",
                description: "Read-only catalog of stable source-lint rule ids. Takes no parameters. Errors carry data.reason: content_lint_params.",
                params: &[],
                result: "{ok, format_version, rules: [{id, producer, summary, unit}], importer: [{name, status}]}",
                mutating: false,
                permission: Permission::Read,
            },
            rules,
        ),
        (
            MethodSchema {
                name: "asset.content_lint_policy",
                tool: "asset_content_lint_policy",
                description: "Read-only effective source-lint settings after engine defaults, project defaults, the named target profile and an optional asset overlay. A missing config/source_lint.ron is the engine default. Errors carry data.reason: content_lint_params, content_lint_policy, content_lint_target, content_lint_io, path_outside_project.",
                params: POLICY_PARAMS,
                result: "{ok, format_version, target, path?, precedence, geometry, materials, provenance, findings, work?}",
                mutating: false,
                permission: Permission::Read,
            },
            policy,
        ),
    ]
}

fn error(reason: &str, message: impl Into<String>) -> RpcError {
    let mut rpc = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = rpc.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    rpc
}

fn rules(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fields = params
        .as_object()
        .ok_or_else(|| error("content_lint_params", "expected an object"))?;
    if !fields.is_empty() {
        return Err(error(
            "content_lint_params",
            "asset.content_lint_rules takes no parameters",
        ));
    }
    let catalog: Vec<Value> = source_lint::rules()
        .iter()
        .map(|rule| {
            json!({
                "id": rule.id,
                "producer": rule.producer,
                "summary": rule.summary,
                "unit": rule.unit,
            })
        })
        .collect();
    let importer: Vec<Value> = importer_capability()
        .into_iter()
        .map(|row| json!({"name": row.name, "status": row.status}))
        .collect();
    Ok(json!({
        "ok": true,
        "format_version": SOURCE_LINT_REPORT_VERSION,
        "rules": catalog,
        "importer": importer,
    }))
}

fn policy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::content_lint_jobs::policy_answer(world, params)
}
