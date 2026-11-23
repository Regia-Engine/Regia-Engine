// SPDX-License-Identifier: MPL-2.0
//! `agent-snapshot`: the agent protocol surface against the checked-in snapshot
//! `docs/api-snapshots/agent-protocol.txt` — the agent-protocol counterpart of `api-snapshot`
//! (ADR-3335). `api-snapshot` holds the Rust API of the stable crates; this holds what a
//! protocol client sees: one block per method — its name, permission class, params schema,
//! result schema and documented error codes — plus the built-in extension methods and the
//! universal error codes.
//!
//! Without flags the command checks: it renders the surface from the public tables the way
//! `agent_coverage` reads them ([`regia_agent::schema::METHODS`],
//! [`regia_agent::contract::CONTRACTS`] / `EXTENSION_CONTRACTS`, the
//! [`regia_agent::extension::AgentExtension`] schemas of [`regia_agent::install_builtin_extensions`]
//! and [`regia_agent::contract::UNIVERSAL_ERRORS`]) — no renderer, no app, seconds — and fails
//! when it differs from the snapshot. `--update` rewrites the snapshot and is how every schema
//! change lands (a lane that changed the agent schema runs it and commits the refresh).
//!
//! Every diff is classified. **Breaking**: a method or param removed, a param type, shape or the
//! result schema changed, a new required param, a permission, tool, `mutating` or cost class
//! changed, a limit tightened, an error code removed or renamed. **Additive**: anything else (a
//! new method, a new optional param, a new error code, a limit relaxed or removed, ...). A
//! breaking change is accepted only when a release fragment this branch touches
//! (`docs/releases/*.md`) names the method under `## Upgrade`; an additive change needs a `## New`
//! line naming it. Even fully documented, a diff fails `--check` until the snapshot is refreshed
//! with `--update` in the same branch, and `--update` itself refuses to write a snapshot whose
//! changes no touched fragment documents — so the checked-in file always matches the tables at
//! HEAD. See `book/agents/protocol.md` ("Stability").

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use regia_app::prelude::World;
use serde_json::Value;

/// Where the agent protocol snapshot lives, relative to the workspace root.
pub const SNAPSHOT_PATH: &str = "docs/api-snapshots/agent-protocol.txt";

/// The snapshot format this build writes. Bumped when the block grammar gains fields (v2 added
/// the MCP tool name, the `mutating` flag, the cost class, the dispatcher limits and the
/// structured param shapes); a file of an older format fails `--check` with "run `--update`"
/// instead of diffing field by field.
pub const SNAPSHOT_FORMAT: u32 = 2;

/// The release fragments a branch may document its changes in, relative to the workspace root.
const RELEASE_DIR: &str = "docs/releases";

/// Command-line options of `cargo xtask agent-snapshot`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// Rewrite the snapshot from the protocol tables instead of checking it.
    #[arg(long)]
    pub update: bool,
    /// Check the snapshot against the protocol tables (the default).
    #[arg(long, conflicts_with = "update")]
    pub check: bool,
    /// The ref the release-fragment diff is taken against (the merge base with it decides
    /// which fragments count as "touched by this branch").
    #[arg(long, default_value = "main")]
    pub base: String,
}

/// One parameter of a method, as the snapshot records it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParamLine {
    /// Parameter name.
    pub name: String,
    /// JSON kind (`object`, `array`, `string`, `number`, `boolean`, `any`).
    pub kind: String,
    /// Whether the call fails when the parameter is absent.
    pub required: bool,
    /// The structured JSON Schema of the param (an enum, an array of tagged items), in compact
    /// one-line form, when it has structure beyond its bare `kind`; `None` for a plain param.
    /// This is the shape `agent.schema` publishes, so a change here is what a protocol client
    /// sees (a dropped enum value, a tightened range).
    pub shape: Option<String>,
}

/// One method block of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodBlock {
    /// JSON-RPC method name, e.g. `scene.spawn`.
    pub name: String,
    /// The permission class (`read`, `edit_scene`, `play`, `assets`, `tools`, `build`, `review`).
    pub permission: String,
    /// The MCP tool name (`spawn`); empty = protocol-only, no MCP tool.
    pub tool: String,
    /// Whether the call changes engine state (rate-limited and permission-gated).
    pub mutating: bool,
    /// The rate class (`light` or `heavy`) the contract publishes; `None` when the table has no
    /// contract row for the method.
    pub cost: Option<String>,
    /// The dispatcher-enforced limits, each the compact one-line JSON `Limit::to_json` renders,
    /// sorted.
    pub limits: Vec<String>,
    /// Parameters, sorted by name.
    pub params: Vec<ParamLine>,
    /// What the successful result contains (single line).
    pub result: String,
    /// Documented error codes beyond the universal ones, sorted.
    pub errors: Vec<String>,
}

impl MethodBlock {
    /// The canonical form: params, limits and errors sorted and without repeats (a table row
    /// that lists a code twice renders it once, so the rendered snapshot parses back), one-line
    /// result.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.params.sort();
        self.params.dedup();
        self.limits.sort();
        self.limits.dedup();
        self.errors.sort();
        self.errors.dedup();
        self.result = one_line(&self.result);
        self
    }
}

/// The agent protocol surface: the universal error codes plus one block per method.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Protocol {
    /// Codes any method can answer with ([`regia_agent::contract::UNIVERSAL_ERRORS`]), sorted.
    pub universal: Vec<String>,
    /// One block per method (schema methods and built-in extension methods), sorted by name.
    pub methods: Vec<MethodBlock>,
}

impl Protocol {
    /// The canonical form: universal codes and methods sorted.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.universal.sort();
        self.universal.dedup();
        self.methods = self.methods.into_iter().map(MethodBlock::normalized).collect();
        self.methods.sort_by(|a, b| a.name.cmp(&b.name));
        self.methods.dedup_by(|next, prev| {
            if next.name == prev.name {
                // Later registrations win, as on the extension itself.
                *prev = next.clone();
                true
            } else {
                false
            }
        });
        self
    }
}

/// Collapses whitespace a table string is never supposed to carry, so a block is one line.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The snapshot format version of `text`: the `# format: N` line, or 1 when absent (files
/// written before the field existed).
fn format_version(text: &str) -> u32 {
    text.lines()
        .find_map(|line| {
            line.strip_prefix("# format: ")
                .and_then(|rest| rest.trim().parse::<u32>().ok())
        })
        .unwrap_or(1)
}

/// Renders the snapshot text: a header, the universal codes, then one blank-line-separated
/// block per method, methods sorted by name.
#[must_use]
pub fn render(protocol: &Protocol) -> String {
    let protocol = protocol.clone().normalized();
    let mut out = String::new();
    out.push_str(&format!(
        "# format: {SNAPSHOT_FORMAT}\n#\n# Generated by `cargo xtask agent-snapshot --update`; do not edit.\n\
          # The agent protocol surface: one block per method — name, permission class, MCP tool\n\
          # name, mutating flag, cost class, dispatcher limits, params schema (kinds and the\n\
          # structured JSON Schema shapes), result schema and documented error codes — plus the\n\
          # built-in extension methods and the universal error codes. `cargo xtask agent-snapshot\n\
          # --check` fails when the tables differ: a breaking change needs the method named under\n\
          # `## Upgrade` and every change a `## New` line in a docs/releases/*.md fragment this\n\
          # branch touches, and `--update` itself refuses to write an undocumented change (the\n\
          # refreshed snapshot must be committed too; book/agents/protocol.md, \"Stability\").\n"
    ));
    out.push_str(&format!("universal: {}\n", protocol.universal.join(" ")));
    for method in &protocol.methods {
        out.push('\n');
        let _ = writeln!(out, "method: {}", method.name);
        let _ = writeln!(out, "  permission: {}", method.permission);
        if !method.tool.is_empty() {
            let _ = writeln!(out, "  tool: {}", method.tool);
        }
        let _ = writeln!(out, "  mutating: {}", method.mutating);
        if let Some(cost) = &method.cost {
            let _ = writeln!(out, "  cost: {cost}");
        }
        for limit in &method.limits {
            let _ = writeln!(out, "  limit: {limit}");
        }
        for param in &method.params {
            let _ = write!(
                out,
                "  param: {} {} {}",
                param.name,
                param.kind,
                if param.required { "required" } else { "optional" }
            );
            if let Some(shape) = &param.shape {
                let _ = write!(out, " {shape}");
            }
            out.push('\n');
        }
        let _ = writeln!(out, "  result: {}", method.result);
        if method.errors.is_empty() {
            out.push_str("  errors: none\n");
        } else {
            let _ = writeln!(out, "  errors: {}", method.errors.join(" "));
        }
    }
    out
}

/// Reads the snapshot text back. Anything that is not a comment, a blank line, the `universal:`
/// line, a `method:` block or one of its indented fields is a corrupt file (a hand edit or
/// a bad merge), not an empty surface. Duplicated params or error codes, a repeated field line,
/// a missing `result:` line and a missing `universal:` line are corrupt too: a bad merge must
/// not diff away as `Clean`.
pub fn parse(text: &str) -> Result<Protocol> {
    let mut protocol = Protocol::default();
    // The open block, which of its field lines were seen, and its param names so far (an empty
    // error list renders as `errors: none`, so the line is required and distinguishes "none"
    // from "not there yet").
    let mut block: Option<(MethodBlock, Seen)> = None;
    let mut seen_universal = false;
    let finish =
        |block: &mut Option<(MethodBlock, Seen)>, protocol: &mut Protocol, line: usize| -> Result<()> {
            let Some((done, seen)) = block.take() else {
                return Ok(());
            };
            if !seen.permission {
                bail!("line {line}: method `{}` has no permission line", done.name);
            }
            if !seen.result {
                bail!("line {line}: method `{}` has no result line", done.name);
            }
            if !seen.errors {
                bail!("line {line}: method `{}` has no errors line", done.name);
            }
            protocol.methods.push(done);
            Ok(())
        };
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("universal: ") {
            if seen_universal {
                bail!("line {number}: a second universal line");
            }
            seen_universal = true;
            protocol.universal = rest.split_whitespace().map(str::to_owned).collect();
            continue;
        }
        if let Some(name) = line.strip_prefix("method: ") {
            finish(&mut block, &mut protocol, number)?;
            block = Some((
                MethodBlock {
                    name: name.trim().to_owned(),
                    permission: String::new(),
                    tool: String::new(),
                    mutating: false,
                    cost: None,
                    limits: Vec::new(),
                    params: Vec::new(),
                    result: String::new(),
                    errors: Vec::new(),
                },
                Seen::default(),
            ));
            continue;
        }
        let Some((done, seen)) = block.as_mut() else {
            bail!("line {number}: `{line}` sits outside a method block");
        };
        if let Some(permission) = line.strip_prefix("  permission: ") {
            if seen.permission {
                bail!(
                    "line {number}: method `{}` has a second permission line",
                    done.name
                );
            }
            seen.permission = true;
            done.permission = permission.trim().to_owned();
        } else if let Some(tool) = line.strip_prefix("  tool: ") {
            if seen.tool {
                bail!("line {number}: method `{}` has a second tool line", done.name);
            }
            seen.tool = true;
            done.tool = tool.trim().to_owned();
        } else if let Some(mutating) = line.strip_prefix("  mutating: ") {
            if seen.mutating {
                bail!("line {number}: method `{}` has a second mutating line", done.name);
            }
            seen.mutating = true;
            done.mutating = match mutating.trim() {
                "true" => true,
                "false" => false,
                other => bail!("line {number}: `{other}` is not true|false"),
            };
        } else if let Some(cost) = line.strip_prefix("  cost: ") {
            if seen.cost {
                bail!("line {number}: method `{}` has a second cost line", done.name);
            }
            seen.cost = true;
            done.cost = Some(cost.trim().to_owned());
        } else if let Some(limit) = line.strip_prefix("  limit: ") {
            let limit = limit.trim().to_owned();
            if done.limits.contains(&limit) {
                bail!(
                    "line {number}: method `{}` repeats the limit `{limit}`",
                    done.name
                );
            }
            if serde_json::from_str::<Value>(&limit).is_err() {
                bail!("line {number}: a limit line is the compact JSON `Limit::to_json` renders");
            }
            done.limits.push(limit);
        } else if let Some(param) = line.strip_prefix("  param: ") {
            // `name`, `kind` and `required|optional` are the first three space-separated
            // tokens; an optional structured shape is the compact JSON remainder of the line
            // (it can hold spaces inside nested description strings).
            let (name, rest) = param.split_once(' ').ok_or_else(|| {
                anyhow::anyhow!(
                    "line {number}: a param line is `param: <name> <kind> <required|optional>[ <shape-json>]`"
                )
            })?;
            let (kind, rest) = rest.split_once(' ').ok_or_else(|| {
                anyhow::anyhow!(
                    "line {number}: a param line is `param: <name> <kind> <required|optional>[ <shape-json>]`"
                )
            })?;
            let (required, shape) = match rest.split_once(' ') {
                Some((required, shape)) => (required, Some(shape)),
                None => (rest, None),
            };
            let required = match required {
                "required" => true,
                "optional" => false,
                other => bail!("line {number}: `{other}` is not required|optional"),
            };
            if done.params.iter().any(|p| p.name == name) {
                bail!("line {number}: method `{}` repeats the param `{name}`", done.name);
            }
            let shape = shape
                .map(|shape| {
                    serde_json::from_str::<Value>(shape)
                        .map(|_| shape.to_owned())
                        .map_err(|_| anyhow::anyhow!("line {number}: the param shape is not compact JSON"))
                })
                .transpose()?;
            done.params.push(ParamLine {
                name: name.to_owned(),
                kind: kind.to_owned(),
                required,
                shape,
            });
        } else if let Some(result) = line.strip_prefix("  result:") {
            if seen.result {
                bail!("line {number}: method `{}` has a second result line", done.name);
            }
            seen.result = true;
            done.result = result.trim_start().to_owned();
        } else if line == "  errors: none" {
            if seen.errors {
                bail!("line {number}: method `{}` has a second errors line", done.name);
            }
            seen.errors = true;
        } else if let Some(errors) = line.strip_prefix("  errors: ") {
            if seen.errors {
                bail!("line {number}: method `{}` has a second errors line", done.name);
            }
            seen.errors = true;
            let codes: Vec<String> = errors.split_whitespace().map(str::to_owned).collect();
            if let Some(dup) = codes.windows(2).find(|w| w[0] == w[1]) {
                bail!(
                    "line {number}: method `{}` repeats the error code `{}`",
                    done.name,
                    dup[0]
                );
            }
            done.errors = codes;
        } else {
            bail!("line {number}: `{line}` is not part of a method block");
        }
    }
    finish(&mut block, &mut protocol, text.lines().count())?;
    if !seen_universal {
        bail!("the snapshot has no universal line");
    }
    Ok(protocol.normalized())
}

/// Which field lines of the open block were seen, so a repeated line fails instead of silently
/// overwriting (a bad-merge signature).
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    permission: bool,
    tool: bool,
    mutating: bool,
    cost: bool,
    result: bool,
    errors: bool,
}

/// The contract row of `method`, or none when the table has no row (the coverage check requires
/// one; an absent row must not panic a build tool).
fn contract_row(
    contracts: &'static [regia_agent::contract::MethodContract],
    method: &str,
) -> Option<&'static regia_agent::contract::MethodContract> {
    contracts.iter().find(|contract| contract.method == method)
}

/// The compact one-line JSON of one limit, as `agent.schema` publishes it.
fn limit_line(limit: &regia_agent::contract::Limit) -> String {
    serde_json::to_string(&limit.to_json()).unwrap_or_else(|_| "\"unrenderable\"".to_owned())
}

/// The structured shape of `param` as `agent.schema` publishes it (compact one-line JSON), or
/// `None` when the param has nothing beyond its bare `kind`: the published params schema minus
/// the `description` key, compared against `{ "type": kind }`.
fn published_shape(schema: &regia_agent::schema::MethodSchema, param: &str, kind: &str) -> Option<String> {
    let published = schema.params_json_schema();
    let mut shape = published["properties"].get(param)?.clone();
    shape.as_object_mut()?.remove("description");
    let bare = serde_json::json!({ "type": kind });
    if shape == bare {
        return None;
    }
    serde_json::to_string(&shape).ok()
}

/// Builds the block of one schema row plus its contract's cost, limits and error codes.
fn block_from_schema(
    schema: &regia_agent::schema::MethodSchema,
    contract: Option<&regia_agent::contract::MethodContract>,
    shapes: &dyn Fn(&str, &str) -> Option<String>,
) -> MethodBlock {
    MethodBlock {
        name: schema.name.to_owned(),
        permission: schema.permission.name().to_owned(),
        tool: schema.tool.to_owned(),
        mutating: schema.mutating,
        cost: contract.map(|c| c.cost.name().to_owned()),
        limits: contract
            .map(|c| c.limits.iter().map(limit_line).collect())
            .unwrap_or_default(),
        params: schema
            .params
            .iter()
            .map(|param| ParamLine {
                name: param.name.to_owned(),
                kind: param.kind.to_owned(),
                required: param.required,
                shape: shapes(param.name, param.kind),
            })
            .collect(),
        result: schema.result.to_owned(),
        errors: contract
            .map(|c| c.errors.iter().map(|code| (*code).to_owned()).collect())
            .unwrap_or_default(),
    }
    .normalized()
}

/// The protocol surface of the current sources: the schema table and its contracts, the
/// built-in extension methods ([`regia_agent::install_builtin_extensions`]) and their
/// contracts, and the universal error codes. Reading tables only — no renderer, no app.
#[must_use]
pub fn current() -> Protocol {
    let mut by_name: BTreeMap<String, MethodBlock> = BTreeMap::new();
    for schema in regia_agent::schema::METHODS {
        let contract = contract_row(regia_agent::contract::CONTRACTS, schema.name);
        by_name.insert(
            schema.name.to_owned(),
            block_from_schema(schema, contract, &|param, kind| {
                published_shape(schema, param, kind)
            }),
        );
    }
    let mut world = World::new();
    regia_agent::install_builtin_extensions(&mut world);
    if let Some(extension) = world.get_resource::<regia_agent::extension::AgentExtension>() {
        for name in extension.method_names() {
            let Some(schema) = extension.method_schema(name) else {
                continue;
            };
            let contract = contract_row(regia_agent::contract::EXTENSION_CONTRACTS, name);
            by_name.insert(
                name.to_owned(),
                block_from_schema(schema, contract, &|param, kind| {
                    extension
                        .param_shape(name, param)
                        .and_then(|shape| serde_json::to_string(shape).ok())
                        .or_else(|| published_shape(schema, param, kind))
                }),
            );
        }
    }
    Protocol {
        universal: regia_agent::contract::UNIVERSAL_ERRORS
            .iter()
            .map(|code| (*code).to_owned())
            .collect(),
        methods: by_name.into_values().collect(),
    }
    .normalized()
}

/// Whether a change breaks clients built against the old surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    /// Old clients can stop compiling or start receiving errors.
    Breaking,
    /// Old clients keep working.
    Additive,
}

impl ChangeKind {
    fn label(self) -> &'static str {
        match self {
            Self::Breaking => "breaking",
            Self::Additive => "additive",
        }
    }
}

/// One difference between the snapshot and the current tables, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The method the change belongs to; a universal-code change carries the code itself, so a
    /// fragment can name it.
    pub subject: String,
    /// Breaking or additive.
    pub kind: ChangeKind,
    /// What moved, in one line.
    pub detail: String,
}

/// Classifies the difference between two surfaces. Every rendered field is compared, so an
/// empty diff means the renders are equal.
#[must_use]
pub fn diff(old: &Protocol, new: &Protocol) -> Vec<Change> {
    let (old, new) = (old.clone().normalized(), new.clone().normalized());
    let mut changes = Vec::new();
    let was: std::collections::BTreeSet<&str> = old.universal.iter().map(String::as_str).collect();
    let now: std::collections::BTreeSet<&str> = new.universal.iter().map(String::as_str).collect();
    for code in was.difference(&now) {
        changes.push(Change {
            subject: (*code).to_owned(),
            kind: ChangeKind::Breaking,
            detail: format!("universal error code `{code}` removed"),
        });
    }
    for code in now.difference(&was) {
        changes.push(Change {
            subject: (*code).to_owned(),
            kind: ChangeKind::Additive,
            detail: format!("universal error code `{code}` added"),
        });
    }
    let old_map: BTreeMap<&str, &MethodBlock> = old.methods.iter().map(|m| (m.name.as_str(), m)).collect();
    let new_map: BTreeMap<&str, &MethodBlock> = new.methods.iter().map(|m| (m.name.as_str(), m)).collect();
    for (name, was) in &old_map {
        let Some(now) = new_map.get(name) else {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Breaking,
                detail: "method removed".to_owned(),
            });
            continue;
        };
        if was.permission != now.permission {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Breaking,
                detail: format!(
                    "permission class changed: `{}` → `{}`",
                    was.permission, now.permission
                ),
            });
        }
        if was.tool != now.tool {
            let (kind, detail) = match (was.tool.is_empty(), now.tool.is_empty()) {
                (true, false) => (ChangeKind::Additive, format!("MCP tool `{}` added", now.tool)),
                (false, true) => (ChangeKind::Breaking, format!("MCP tool `{}` removed", was.tool)),
                (false, false) => (
                    ChangeKind::Breaking,
                    format!("MCP tool renamed: `{}` → `{}`", was.tool, now.tool),
                ),
                (true, true) => unreachable!("tools differ, one is non-empty"),
            };
            changes.push(Change {
                subject: (*name).to_owned(),
                kind,
                detail,
            });
        }
        if was.mutating != now.mutating {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Breaking,
                detail: format!(
                    "mutating flag changed: {} → {} (the rate-limit class moved)",
                    was.mutating, now.mutating
                ),
            });
        }
        if was.cost != now.cost {
            let (kind, detail) = match (&was.cost, &now.cost) {
                (None, Some(cost)) => (ChangeKind::Additive, format!("cost class `{cost}` published")),
                (Some(cost), None) => (
                    ChangeKind::Breaking,
                    format!("cost class `{cost}` no longer published"),
                ),
                (Some(was_cost), Some(now_cost)) => (
                    ChangeKind::Breaking,
                    format!("cost class changed: `{was_cost}` → `{now_cost}`"),
                ),
                (None, None) => unreachable!("costs differ, one is Some"),
            };
            changes.push(Change {
                subject: (*name).to_owned(),
                kind,
                detail,
            });
        }
        changes.extend(limits_changes(name, &was.limits, &now.limits));
        if was.result != now.result {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Breaking,
                detail: "result schema changed".to_owned(),
            });
        }
        let was_params: BTreeMap<&str, &ParamLine> =
            was.params.iter().map(|p| (p.name.as_str(), p)).collect();
        let now_params: BTreeMap<&str, &ParamLine> =
            now.params.iter().map(|p| (p.name.as_str(), p)).collect();
        let describe = |param: &ParamLine| {
            format!(
                "`{}`{}",
                param.kind,
                if param.required {
                    ", required"
                } else {
                    ", optional"
                }
            )
        };
        for (param, was) in &was_params {
            let Some(now) = now_params.get(param) else {
                changes.push(Change {
                    subject: (*name).to_owned(),
                    kind: ChangeKind::Breaking,
                    detail: format!("param `{param}` removed (was {})", describe(was)),
                });
                continue;
            };
            if was.kind != now.kind {
                changes.push(Change {
                    subject: (*name).to_owned(),
                    kind: ChangeKind::Breaking,
                    detail: format!("param `{param}` type changed: `{}` → `{}`", was.kind, now.kind),
                });
            }
            if was.shape != now.shape {
                let (kind, what) = match (&was.shape, &now.shape) {
                    (None, Some(_)) => (ChangeKind::Breaking, "gained a structured shape"),
                    (Some(_), None) => (ChangeKind::Additive, "structure dropped (bare type again)"),
                    (Some(_), Some(_)) => (ChangeKind::Breaking, "shape changed"),
                    (None, None) => unreachable!("shapes differ, one is Some"),
                };
                changes.push(Change {
                    subject: (*name).to_owned(),
                    kind,
                    detail: format!("param `{param}` {what}"),
                });
            }
            if was.required != now.required {
                changes.push(Change {
                    subject: (*name).to_owned(),
                    kind: if now.required {
                        ChangeKind::Breaking
                    } else {
                        ChangeKind::Additive
                    },
                    detail: if now.required {
                        format!("param `{param}` became required")
                    } else {
                        format!("param `{param}` became optional")
                    },
                });
            }
        }
        for (param, now) in &now_params {
            if !was_params.contains_key(param) {
                let kind = if now.required {
                    ChangeKind::Breaking
                } else {
                    ChangeKind::Additive
                };
                changes.push(Change {
                    subject: (*name).to_owned(),
                    kind,
                    detail: format!("param `{param}` added ({})", describe(now)),
                });
            }
        }
        let was_errors: std::collections::BTreeSet<&str> = was.errors.iter().map(String::as_str).collect();
        let now_errors: std::collections::BTreeSet<&str> = now.errors.iter().map(String::as_str).collect();
        for code in was_errors.difference(&now_errors) {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Breaking,
                detail: format!("error code `{code}` removed"),
            });
        }
        for code in now_errors.difference(&was_errors) {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Additive,
                detail: format!("error code `{code}` added"),
            });
        }
    }
    for name in new_map.keys() {
        if !old_map.contains_key(*name) {
            changes.push(Change {
                subject: (*name).to_owned(),
                kind: ChangeKind::Additive,
                detail: "new method".to_owned(),
            });
        }
    }
    changes.sort_by(|a, b| (&a.subject, a.kind, &a.detail).cmp(&(&b.subject, b.kind, &b.detail)));
    changes
}

/// The `(kind, low, high)` bounds of one compact limit JSON line: a range limits `min..=max`,
/// an items/bytes limit limits `0..=max`. Unparseable lines compare as the tightest thing
/// possible, so a corrupt old snapshot fails loudly instead of diffing away.
fn limit_bounds(line: &str) -> (&'static str, f64, f64) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return ("corrupt", f64::MAX, f64::MIN);
    };
    match value["kind"].as_str() {
        Some("range") => (
            "range",
            value["min"].as_f64().unwrap_or(f64::MAX),
            value["max"].as_f64().unwrap_or(f64::MIN),
        ),
        Some("items") => ("items", 0.0, value["max"].as_f64().unwrap_or(f64::MIN)),
        Some("bytes") => ("bytes", 0.0, value["max"].as_f64().unwrap_or(f64::MIN)),
        _ => ("corrupt", f64::MAX, f64::MIN),
    }
}

/// The param one compact limit JSON line applies to.
fn limit_param(line: &str) -> String {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| value["param"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Classifies a limits change on one method. Per param: a new or tightened limit is breaking
/// (calls the old surface accepted now fail `F0002`), a removed or relaxed limit is additive.
#[must_use]
fn limits_changes(name: &str, was: &[String], now: &[String]) -> Vec<Change> {
    let params: std::collections::BTreeSet<String> = was.iter().chain(now).map(|l| limit_param(l)).collect();
    let mut changes = Vec::new();
    for param in params {
        let was_l: Vec<&String> = was.iter().filter(|l| limit_param(l) == param).collect();
        let now_l: Vec<&String> = now.iter().filter(|l| limit_param(l) == param).collect();
        let added: Vec<&&String> = now_l.iter().filter(|l| !was_l.contains(l)).collect();
        let mut unmatched_removed: Vec<&&String> = was_l.iter().filter(|l| !now_l.contains(l)).collect();
        if added.is_empty() && unmatched_removed.is_empty() {
            continue;
        }
        let mut tightened = false;
        let mut relaxed = false;
        for a in &added {
            let (a_kind, a_low, a_high) = limit_bounds(a);
            // A replacement pairs with a removed limit of the same kind; anything left over on
            // either side is a pure addition (tightening) or a pure removal (relaxing).
            let pair = unmatched_removed
                .iter()
                .position(|r| limit_bounds(r).0 == a_kind)
                .map(|i| unmatched_removed.remove(i));
            match pair {
                None => tightened = true,
                Some(r) => {
                    let (_, r_low, r_high) = limit_bounds(r);
                    if a_low <= r_low && a_high >= r_high {
                        relaxed = true;
                    } else {
                        tightened = true;
                    }
                }
            }
        }
        relaxed |= !unmatched_removed.is_empty();
        if tightened {
            changes.push(Change {
                subject: name.to_owned(),
                kind: ChangeKind::Breaking,
                detail: format!("limits tightened on `{param}`"),
            });
        } else if relaxed {
            changes.push(Change {
                subject: name.to_owned(),
                kind: ChangeKind::Additive,
                detail: format!("limits relaxed on `{param}`"),
            });
        }
    }
    changes
}

/// The `## New` and `## Upgrade` lines of one release fragment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fragment {
    /// Where the fragment lives, for notes.
    pub source: String,
    /// Non-empty lines under the `## New` heading, list markers included.
    pub new_lines: Vec<String>,
    /// Non-empty lines under the `## Upgrade` heading.
    pub upgrade_lines: Vec<String>,
}

/// Splits a fragment into its New and Upgrade lines. Section headings are `## New`,
/// `## Changed`, `## Fixed`, `## Upgrade`, `## Known issues` (the release-fragment style of
/// `docs/releases/v0.1.24.md`); anything deeper than `## ` is content of the current section.
#[must_use]
pub fn parse_fragment(source: &str, text: &str) -> Fragment {
    let mut fragment = Fragment {
        source: source.to_owned(),
        ..Fragment::default()
    };
    let mut section = String::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            section = heading.trim().to_ascii_lowercase();
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        match section.as_str() {
            "new" => fragment.new_lines.push(line.to_owned()),
            "upgrade" => fragment.upgrade_lines.push(line.to_owned()),
            _ => {}
        }
    }
    fragment
}

/// Whether a touched fragment documents `change`: a breaking change names its subject under
/// `## Upgrade`, an additive one under `## New`. The subject must appear as a whole token —
/// bounded by backticks, whitespace or punctuation — so a line naming `assets.load_progress`
/// does not document a change about `assets.load`.
#[must_use]
pub fn documented(change: &Change, fragments: &[Fragment]) -> bool {
    fragments.iter().any(|fragment| {
        let lines = match change.kind {
            ChangeKind::Breaking => &fragment.upgrade_lines,
            ChangeKind::Additive => &fragment.new_lines,
        };
        lines.iter().any(|line| contains_token(line, &change.subject))
    })
}

/// Whether `text` contains `subject` bounded on both sides by a character that cannot extend a
/// method or code name (not `[a-z0-9_.]`), or by the start/end of the line. A backtick, a
/// space or a comma all bound; a letter, digit, `_` or `.` does not.
fn contains_token(text: &str, subject: &str) -> bool {
    let is_name_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.';
    let mut at = 0;
    while let Some(found) = text[at..].find(subject) {
        let start = at + found;
        let before_ok = text[..start].chars().next_back().is_none_or(|c| !is_name_char(c));
        let after = start + subject.len();
        let after_ok = text[after..].chars().next().is_none_or(|c| !is_name_char(c));
        if before_ok && after_ok {
            return true;
        }
        at = start + subject.len();
    }
    false
}

/// The verdict over a diff and the touched fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The snapshot matches the tables.
    Clean,
    /// Every change is documented in a touched fragment; only the snapshot refresh is missing.
    Stale,
    /// These changes have no documenting fragment line.
    Undocumented(Vec<Change>),
}

/// Decides what a diff means against the fragments the branch touches.
#[must_use]
pub fn verdict(changes: &[Change], fragments: &[Fragment]) -> Verdict {
    if changes.is_empty() {
        return Verdict::Clean;
    }
    let uncovered: Vec<Change> = changes
        .iter()
        .filter(|change| !documented(change, fragments))
        .cloned()
        .collect();
    if uncovered.is_empty() {
        Verdict::Stale
    } else {
        Verdict::Undocumented(uncovered)
    }
}

/// The fix line of a failing change: names the subject and the exact command.
#[must_use]
pub fn fix_hint(change: &Change) -> String {
    let section = match change.kind {
        ChangeKind::Breaking => "## Upgrade",
        ChangeKind::Additive => "## New",
    };
    let restore = match change.kind {
        ChangeKind::Breaking => "restore the old surface, or ",
        ChangeKind::Additive => "",
    };
    format!(
        "{restore}name `{}` under a `{section}` heading in a `{RELEASE_DIR}/*.md` fragment this \
         branch touches, then run `cargo xtask agent-snapshot --update` and commit the refreshed \
         snapshot",
        change.subject
    )
}

/// One git invocation in `root`, its stdout returned.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The release fragments this branch touches against `base`: added or modified in the merge-base
/// diff, or new in the worktree. A tree without the base ref (a source archive) has none.
fn touched_fragment_paths(root: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let candidates = [base.to_owned(), format!("origin/{base}")];
    let Some(merge_base) = candidates
        .iter()
        .find_map(|candidate| git(root, &["merge-base", "HEAD", candidate]).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    else {
        return Ok(Vec::new());
    };
    let merge_base = merge_base.to_string();
    let mut files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for listing in [
        git(root, &["diff", "--name-only", "--no-renames", &merge_base])?,
        git(
            root,
            &["diff", "--name-only", "--no-renames", "--cached", &merge_base],
        )?,
        git(root, &["ls-files", "--others", "--exclude-standard"])?,
    ] {
        files.extend(listing.lines().filter(|l| !l.is_empty()).map(str::to_owned));
    }
    Ok(files
        .into_iter()
        .filter(|path| path.starts_with(&format!("{RELEASE_DIR}/")) && path.ends_with(".md"))
        .map(|path| root.join(path))
        .collect())
}

/// Whether `base` resolves to a merge-base with HEAD: only then does the fragment diff mean
/// "touched by this branch". A tree without the base ref (a source archive) has none, and the
/// `--update` gate skips the fragment check there rather than refusing every change.
fn has_base(root: &Path, base: &str) -> bool {
    [base.to_owned(), format!("origin/{base}")]
        .iter()
        .any(|candidate| git(root, &["merge-base", "HEAD", candidate]).is_ok())
}

/// The touched fragments' New and Upgrade lines.
fn touched_fragments(root: &Path, base: &str) -> Result<Vec<Fragment>> {
    let mut fragments = Vec::new();
    for path in touched_fragment_paths(root, base)? {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        fragments.push(parse_fragment(
            &path.strip_prefix(root).unwrap_or(&path).to_string_lossy(),
            &text,
        ));
    }
    Ok(fragments)
}

/// Runs the check or the update.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let current = current();
    let path = root.join(SNAPSHOT_PATH);
    if args.update {
        // The gate: `--update` refreshes the snapshot, but it must not bless an undocumented
        // change. Diff the old file against the tables like `--check` does and refuse to write
        // when a change has no fragment line — otherwise a lane could refresh the snapshot,
        // commit it with no `docs/releases/*.md` line, and every later check would pass.
        // A missing old file (the first snapshot) and a format bump (the new fields have no
        // per-method fragment vocabulary) skip the gate; a tree without the base ref (a source
        // archive) cannot know its fragments and skips with a note.
        let old = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let old = parse(&text).with_context(|| format!("parsing {}", path.display()))?;
                Some((text, old))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        if let Some((text, old)) = &old
            && format_version(text) == SNAPSHOT_FORMAT
        {
            if has_base(root, &args.base) {
                let changes = diff(old, &current);
                if let Verdict::Undocumented(uncovered) =
                    verdict(&changes, &touched_fragments(root, &args.base)?)
                {
                    for change in &uncovered {
                        eprintln!(
                            "agent-snapshot: FAIL: {} change: `{}`: {}",
                            change.kind.label(),
                            change.subject,
                            change.detail
                        );
                        eprintln!("    fix: {}", fix_hint(change));
                    }
                    eprintln!(
                        "agent-snapshot: --update refused to write {}: {} breaking/additive \
                         change(s) without a release-fragment line; document them first, then \
                         run `cargo xtask agent-snapshot --update` again",
                        SNAPSHOT_PATH,
                        uncovered.len()
                    );
                    return Ok(ExitCode::FAILURE);
                }
            } else {
                println!(
                    "agent-snapshot: no merge-base with `{}`; skipping the fragment gate on --update",
                    args.base
                );
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, render(&current)).with_context(|| format!("writing {}", path.display()))?;
        println!(
            "agent-snapshot: {} written ({} methods, {} universal codes)",
            SNAPSHOT_PATH,
            current.methods.len(),
            current.universal.len()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "agent-snapshot: FAIL: no snapshot at {}.\n    fix: run `cargo xtask agent-snapshot --update` and commit the file",
                path.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    if format_version(&text) != SNAPSHOT_FORMAT {
        println!(
            "agent-snapshot: FAIL: {} is format v{} (this tool writes v{}).\n    fix: run `cargo \
             xtask agent-snapshot --update` and commit the refreshed file (a format bump needs no \
             per-method fragment line)",
            SNAPSHOT_PATH,
            format_version(&text),
            SNAPSHOT_FORMAT
        );
        return Ok(ExitCode::FAILURE);
    }
    let old = parse(&text).with_context(|| format!("parsing {}", path.display()))?;
    let changes = diff(&old, &current);
    match verdict(&changes, &touched_fragments(root, &args.base)?) {
        Verdict::Clean => {
            println!("agent-snapshot: ok ({} methods)", current.methods.len());
            Ok(ExitCode::SUCCESS)
        }
        Verdict::Stale => {
            println!(
                "agent-snapshot: FAIL: the surface changed ({} change(s)) and every one is named in \
                 a touched release fragment, but the snapshot is stale.\n    fix: run `cargo xtask \
                 agent-snapshot --update` and commit the refreshed {}",
                changes.len(),
                SNAPSHOT_PATH
            );
            Ok(ExitCode::FAILURE)
        }
        Verdict::Undocumented(uncovered) => {
            for change in &uncovered {
                eprintln!(
                    "agent-snapshot: FAIL: {} change: `{}`: {}",
                    change.kind.label(),
                    change.subject,
                    change.detail
                );
                eprintln!("    fix: {}", fix_hint(change));
            }
            eprintln!(
                "agent-snapshot: {} breaking/additive change(s) without a release-fragment line \
                 (book/agents/protocol.md, \"Stability\")",
                uncovered.len()
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-method surface with one universal code, for before/after diffs.
    fn base() -> Protocol {
        Protocol {
            universal: vec!["F0001".to_owned()],
            methods: vec![
                MethodBlock {
                    name: "scene.spawn".to_owned(),
                    permission: "edit_scene".to_owned(),
                    tool: "spawn".to_owned(),
                    mutating: true,
                    cost: Some("light".to_owned()),
                    limits: vec![
                        "{\"kind\":\"items\",\"max\":8,\"param\":\"tags\"}".to_owned(),
                        "{\"kind\":\"range\",\"max\":4096.0,\"min\":1.0,\"param\":\"width\"}".to_owned(),
                    ],
                    params: vec![
                        ParamLine {
                            name: "count".to_owned(),
                            kind: "number".to_owned(),
                            required: false,
                            shape: None,
                        },
                        ParamLine {
                            name: "name".to_owned(),
                            kind: "string".to_owned(),
                            required: true,
                            shape: None,
                        },
                    ],
                    result: "{ entity }".to_owned(),
                    errors: vec!["bad_spec".to_owned()],
                },
                MethodBlock {
                    name: "agent.hello".to_owned(),
                    permission: "read".to_owned(),
                    tool: "hello".to_owned(),
                    mutating: false,
                    cost: Some("light".to_owned()),
                    limits: Vec::new(),
                    params: vec![],
                    result: "{ protocol }".to_owned(),
                    errors: vec![],
                },
            ],
        }
        .normalized()
    }

    /// The `scene.spawn` block of a surface (`normalized` sorts `agent.hello` first).
    fn scene_spawn(protocol: &mut Protocol) -> &mut MethodBlock {
        protocol
            .methods
            .iter_mut()
            .find(|m| m.name == "scene.spawn")
            .expect("base() has scene.spawn")
    }

    fn fragment(new: &[&str], upgrade: &[&str]) -> Fragment {
        Fragment {
            source: "docs/releases/v-test.md".to_owned(),
            new_lines: new.iter().map(|s| (*s).to_owned()).collect(),
            upgrade_lines: upgrade.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn render_parse_round_trips() {
        let protocol = base();
        let text = render(&protocol);
        assert!(text.contains("method: agent.hello"), "{text}");
        let parsed = parse(&text).expect("parse");
        assert_eq!(parsed, protocol, "the canonical form round-trips");
        assert_eq!(render(&parsed), text, "a second render is byte-identical");
    }

    #[test]
    fn parse_rejects_garbage_and_incomplete_blocks() {
        assert!(
            parse("method: a.b\n  permission: read\n").is_err(),
            "no errors line"
        );
        assert!(parse("  permission: read\n").is_err(), "field outside a block");
        assert!(
            parse("method: a.b\n  param: x string\n").is_err(),
            "bad param line"
        );
        assert!(parse("method: a.b\n  permission: read\n  result: {}\n  errors: none\njunk\n").is_err());
        assert!(
            parse("universal: F0001\nuniversal: F0002\n").is_err(),
            "two universal lines"
        );
        assert!(
            parse("method: a.b\n  permission: read\n  errors: none\n").is_err(),
            "no result line"
        );
        assert!(
            parse("method: a.b\n  result: {}\n  errors: none\n").is_err(),
            "no permission line"
        );
        assert!(
            parse("universal: F0001\nmethod: a.b\n  permission: read\n  result: {}\n  errors: none\n")
                .is_ok(),
            "a complete minimal block parses"
        );
        assert!(
            parse("method: a.b\n  permission: read\n  result: {}\n  errors: none\n").is_err(),
            "no universal line"
        );
    }

    #[test]
    fn parse_rejects_duplicate_params_and_errors() {
        let dup_param = "universal: F0001\nmethod: a.b\n  permission: read\n  param: x string optional\n  \
                         param: x string optional\n  result: {}\n  errors: none\n";
        assert!(
            parse(dup_param).is_err(),
            "a duplicated param line hides a bad merge"
        );
        let dup_error = "universal: F0001\nmethod: a.b\n  permission: read\n  result: {}\n  errors: gone \
                         gone\n";
        assert!(
            parse(dup_error).is_err(),
            "a duplicated error code hides a bad merge"
        );
    }

    #[test]
    fn parse_rejects_repeated_field_lines() {
        let head = "universal: F0001\nmethod: a.b\n  permission: read\n";
        let tail = "  result: {}\n  errors: none\n";
        assert!(
            parse(&format!("{head}  permission: edit_scene\n{tail}")).is_err(),
            "a second permission line"
        );
        assert!(
            parse(&format!("{head}  result: {{}}\n{tail}")).is_err(),
            "a second result line"
        );
        assert!(
            parse(&format!("{head}{tail}  errors: other\n")).is_err(),
            "a second errors line"
        );
    }

    #[test]
    fn identical_surfaces_have_no_changes() {
        assert!(diff(&base(), &base()).is_empty());
        let text = render(&base());
        assert!(diff(&parse(&text).expect("parse"), &base()).is_empty());
    }

    #[test]
    fn empty_diff_iff_renders_match() {
        let mut variants = vec![base()];
        let mut moved = base();
        scene_spawn(&mut moved).permission = "play".to_owned();
        scene_spawn(&mut moved).params.push(ParamLine {
            name: "extra".to_owned(),
            kind: "boolean".to_owned(),
            required: false,
            shape: None,
        });
        scene_spawn(&mut moved).errors.push("new_code".to_owned());
        moved.methods.push(MethodBlock {
            name: "types.list".to_owned(),
            permission: "read".to_owned(),
            tool: String::new(),
            mutating: false,
            cost: Some("light".to_owned()),
            limits: Vec::new(),
            params: vec![],
            result: "{ types }".to_owned(),
            errors: vec![],
        });
        moved.universal.push("F0099".to_owned());
        variants.push(moved.clone());
        for a in &variants {
            for b in &variants {
                assert_eq!(
                    diff(a, b).is_empty(),
                    render(a) == render(b),
                    "diff and render agree in both directions"
                );
            }
        }
    }

    // -- Breaking classes ----------------------------------------------------

    #[test]
    fn removing_a_method_is_breaking() {
        let mut after = base();
        after.methods.retain(|m| m.name != "scene.spawn");
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert_eq!(changes[0].subject, "scene.spawn");
        assert!(changes[0].detail.contains("removed"), "{}", changes[0].detail);
    }

    #[test]
    fn removing_a_param_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).params.retain(|p| p.name != "count");
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert_eq!(changes[0].subject, "scene.spawn");
        assert!(changes[0].detail.contains("`count`"), "{}", changes[0].detail);
        assert!(changes[0].detail.contains("number"), "{}", changes[0].detail);
    }

    #[test]
    fn changing_a_param_type_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).params[0].kind = "string".to_owned();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("`number` → `string`"),
            "{}",
            changes[0].detail
        );
    }

    #[test]
    fn a_new_required_param_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).params.push(ParamLine {
            name: "parent".to_owned(),
            kind: "number".to_owned(),
            required: true,
            shape: None,
        });
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(changes[0].detail.contains("`parent`"), "{}", changes[0].detail);
        assert!(changes[0].detail.contains("required"), "{}", changes[0].detail);
    }

    #[test]
    fn making_an_optional_param_required_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).params[0].required = true;
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("became required"),
            "{}",
            changes[0].detail
        );
    }

    #[test]
    fn changing_the_permission_class_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).permission = "play".to_owned();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("`edit_scene` → `play`"),
            "{}",
            changes[0].detail
        );
    }

    #[test]
    fn removing_an_error_code_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).errors.clear();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert_eq!(changes[0].subject, "scene.spawn");
        assert!(changes[0].detail.contains("`bad_spec`"), "{}", changes[0].detail);
    }

    #[test]
    fn removing_a_universal_error_code_is_breaking() {
        let mut after = base();
        after.universal.clear();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert_eq!(
            changes[0].subject, "F0001",
            "the code is the fragment-match subject"
        );
    }

    #[test]
    fn changing_the_result_schema_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).result = "{ entity, components }".to_owned();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("result schema"),
            "{}",
            changes[0].detail
        );
    }

    // -- Additive classes ----------------------------------------------------

    #[test]
    fn a_new_method_is_additive() {
        let mut after = base();
        after.methods.push(MethodBlock {
            name: "types.list".to_owned(),
            permission: "read".to_owned(),
            tool: String::new(),
            mutating: false,
            cost: Some("light".to_owned()),
            limits: Vec::new(),
            params: vec![],
            result: "{ types }".to_owned(),
            errors: vec![],
        });
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive);
        assert_eq!(changes[0].subject, "types.list");
    }

    #[test]
    fn a_new_optional_param_is_additive() {
        let mut after = base();
        scene_spawn(&mut after).params.push(ParamLine {
            name: "tags".to_owned(),
            kind: "array".to_owned(),
            required: false,
            shape: None,
        });
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive);
        assert!(changes[0].detail.contains("`tags`"), "{}", changes[0].detail);
    }

    #[test]
    fn making_a_required_param_optional_is_additive() {
        let mut after = base();
        scene_spawn(&mut after).params[1].required = false;
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive);
        assert!(
            changes[0].detail.contains("became optional"),
            "{}",
            changes[0].detail
        );
    }

    #[test]
    fn a_new_error_code_is_additive() {
        let mut after = base();
        scene_spawn(&mut after).errors.push("too_many".to_owned());
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive);
        assert!(changes[0].detail.contains("`too_many`"), "{}", changes[0].detail);
    }

    #[test]
    fn a_new_universal_error_code_is_additive() {
        let mut after = base();
        after.universal.push("F0002".to_owned());
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive);
        assert_eq!(changes[0].subject, "F0002");
    }

    #[test]
    fn param_renames_report_both_sides() {
        let mut after = base();
        scene_spawn(&mut after).params[0].name = "amount".to_owned();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(
            changes
                .iter()
                .any(|c| c.kind == ChangeKind::Breaking && c.detail.contains("`count` removed"))
        );
        assert!(
            changes
                .iter()
                .any(|c| c.kind == ChangeKind::Additive && c.detail.contains("`amount` added"))
        );
        assert!(changes.iter().all(|c| c.subject == "scene.spawn"));
    }

    // -- Fragments and verdicts ----------------------------------------------

    #[test]
    fn fragment_parsing_splits_new_and_upgrade_lines() {
        let text = "# v0.1.28-x\n\n## New\n\n- `scene.spawn` gained `tags`.\n- Something else.\n\n\
                    ## Changed\n\n- not counted\n\n## Upgrade\n\n- `scene.spawn` param `count` renamed to `amount`.\n";
        let fragment = parse_fragment("docs/releases/v0.1.28-x.md", text);
        assert_eq!(fragment.new_lines.len(), 2, "{fragment:?}");
        assert_eq!(fragment.upgrade_lines.len(), 1, "{fragment:?}");
        assert!(fragment.upgrade_lines[0].contains("scene.spawn"));
    }

    #[test]
    fn fragment_matching_is_token_bounded_not_substring() {
        // `assets.load` is a strict prefix of `assets.load_progress`; a line naming the longer
        // method must not document a change about the shorter one (and verso with backticks).
        let change = Change {
            subject: "assets.load".to_owned(),
            kind: ChangeKind::Breaking,
            detail: "method removed".to_owned(),
        };
        let longer_only = fragment(&[], &["- `assets.load_progress` reports progress."]);
        assert!(
            !documented(&change, &[longer_only]),
            "a line naming `assets.load_progress` does not document `assets.load`"
        );
        let exact = fragment(&[], &["- `assets.load` now takes a progress callback."]);
        assert!(
            documented(&change, &[exact]),
            "a backtick-bounded name documents it"
        );
        let bare = fragment(&[], &["- assets.load now takes a progress callback."]);
        assert!(
            documented(&change, &[bare]),
            "a whitespace-bounded name documents it"
        );
        let mut universal_change = change.clone();
        universal_change.subject = "F0001".to_owned();
        let other_code = fragment(&[], &["- `F0001x` replaced it."]);
        assert!(
            !documented(&universal_change, &[other_code]),
            "a longer code that merely starts with the subject does not document it"
        );
    }

    #[test]
    fn breaking_needs_upgrade_and_additive_needs_new() {
        let mut after = base();
        scene_spawn(&mut after).params[0].name = "amount".to_owned();
        let changes = diff(&base(), &after);
        let upgrade_only = fragment(&[], &["- `scene.spawn`: param `count` renamed to `amount`."]);
        match verdict(&changes, &[upgrade_only]) {
            Verdict::Undocumented(left) => assert_eq!(
                left.len(),
                1,
                "only the rename's additive side is undocumented: {left:?}"
            ),
            other => panic!("an Upgrade line does not document an additive change: {other:?}"),
        }
        let both = fragment(
            &["- `scene.spawn` gained the optional `amount` param."],
            &["- `scene.spawn`: param `count` renamed to `amount`."],
        );
        assert_eq!(verdict(&changes, &[both]), Verdict::Stale);
        assert_eq!(verdict(&changes, &[]), Verdict::Undocumented(changes.clone()));
    }

    #[test]
    fn a_new_line_is_not_an_upgrade_line_and_verso() {
        let mut after = base();
        scene_spawn(&mut after).permission = "play".to_owned();
        let changes = diff(&base(), &after);
        let new_only = fragment(&["- `scene.spawn` moved to the play permission."], &[]);
        assert!(
            matches!(verdict(&changes, &[new_only]), Verdict::Undocumented(_)),
            "a breaking change named only under New is not documented"
        );
        let upgrade = fragment(&[], &["- `scene.spawn` moved to the play permission."]);
        assert_eq!(verdict(&changes, &[upgrade]), Verdict::Stale);
    }

    #[test]
    fn verdicts_cover_clean_stale_and_uncovered() {
        assert_eq!(verdict(&[], &[]), Verdict::Clean);
        let mut after = base();
        after.methods.push(MethodBlock {
            name: "types.list".to_owned(),
            permission: "read".to_owned(),
            tool: String::new(),
            mutating: false,
            cost: Some("light".to_owned()),
            limits: Vec::new(),
            params: vec![],
            result: "{ types }".to_owned(),
            errors: vec![],
        });
        let changes = diff(&base(), &after);
        assert_eq!(
            verdict(
                &changes,
                &[fragment(&["- `types.list` lists reflected types."], &[])]
            ),
            Verdict::Stale
        );
        assert!(matches!(verdict(&changes, &[]), Verdict::Undocumented(_)));
    }

    #[test]
    fn fix_hints_name_the_method_and_the_command() {
        let mut after = base();
        scene_spawn(&mut after).params[0].name = "amount".to_owned();
        let changes = diff(&base(), &after);
        for change in &changes {
            let hint = fix_hint(change);
            assert!(hint.contains(&change.subject), "{hint}");
            assert!(hint.contains("cargo xtask agent-snapshot --update"), "{hint}");
            assert!(hint.contains("docs/releases"), "{hint}");
            assert!(hint.contains(if change.kind == ChangeKind::Breaking {
                "## Upgrade"
            } else {
                "## New"
            }));
        }
    }

    // -- The wider client-visible surface (format v2) -------------------------

    #[test]
    fn render_parse_round_trips_the_v2_fields() {
        assert_eq!(format_version(&render(&base())), SNAPSHOT_FORMAT);
        let text = render(&base());
        assert!(text.contains("  tool: spawn\n"), "{text}");
        assert!(text.contains("  mutating: true\n"), "{text}");
        assert!(text.contains("  cost: light\n"), "{text}");
        assert!(text.contains("  limit: {\"kind\":\"range\""), "{text}");
        let parsed = parse(&text).expect("parse");
        assert_eq!(parsed, base(), "the v2 fields round-trip");
        assert_eq!(format_version("# old header\nuniversal: F0001\n"), 1);
    }

    #[test]
    fn removing_the_mcp_tool_is_breaking_adding_it_is_additive() {
        let mut after = base();
        scene_spawn(&mut after).tool = String::new();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("MCP tool `spawn` removed"),
            "{}",
            changes[0].detail
        );
        let mut after = base();
        scene_spawn(&mut after).tool = "spawn_entity".to_owned();
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(changes[0].detail.contains("renamed"), "{}", changes[0].detail);
        let mut after = base();
        scene_spawn(&mut after).tool = String::new();
        let hello = after
            .methods
            .iter_mut()
            .find(|m| m.name == "agent.hello")
            .expect("agent.hello");
        hello.tool = String::new();
        let mut before = base();
        before
            .methods
            .iter_mut()
            .find(|m| m.name == "agent.hello")
            .expect("agent.hello")
            .tool = String::new();
        let changes = diff(&before, &after);
        assert!(
            changes.iter().all(|c| c.kind == ChangeKind::Breaking),
            "removing a tool stays breaking: {changes:?}"
        );
        let mut before = base();
        before
            .methods
            .iter_mut()
            .find(|m| m.name == "agent.hello")
            .expect("agent.hello")
            .tool = String::new();
        let after = base();
        let changes = diff(&before, &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive, "{}", changes[0].detail);
    }

    #[test]
    fn flipping_mutating_or_changing_cost_is_breaking() {
        let mut after = base();
        scene_spawn(&mut after).mutating = false;
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(changes[0].detail.contains("mutating"), "{}", changes[0].detail);
        let mut after = base();
        scene_spawn(&mut after).cost = Some("heavy".to_owned());
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(changes[0].detail.contains("cost class"), "{}", changes[0].detail);
        let mut after = base();
        scene_spawn(&mut after).cost = None;
        let changes = diff(&base(), &after);
        assert!(
            changes.iter().all(|c| c.kind == ChangeKind::Breaking),
            "{changes:?}"
        );
        let mut before = base();
        scene_spawn(&mut before).cost = None;
        let changes = diff(&before, &base());
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive, "{}", changes[0].detail);
    }

    #[test]
    fn tightened_limits_are_breaking_relaxed_ones_additive() {
        // Tighten the width range: 1.0..=4096.0 -> 1.0..=1024.0.
        let mut after = base();
        scene_spawn(&mut after).limits = vec![
            "{\"kind\":\"items\",\"max\":8,\"param\":\"tags\"}".to_owned(),
            "{\"kind\":\"range\",\"max\":1024.0,\"min\":1.0,\"param\":\"width\"}".to_owned(),
        ];
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(changes[0].detail.contains("`width`"), "{}", changes[0].detail);
        // Add a brand-new limit on `name`: pure tightening.
        let mut after = base();
        scene_spawn(&mut after)
            .limits
            .push("{\"kind\":\"bytes\",\"max\":64,\"param\":\"name\"}".to_owned());
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(changes[0].detail.contains("`name`"), "{}", changes[0].detail);
        // Drop the tags limit and widen the width range: two independent relaxations.
        let mut after = base();
        scene_spawn(&mut after).limits =
            vec!["{\"kind\":\"range\",\"max\":8192.0,\"min\":0.5,\"param\":\"width\"}".to_owned()];
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(
            changes.iter().all(|c| c.kind == ChangeKind::Additive),
            "relaxations are additive: {changes:?}"
        );
        assert!(
            changes
                .iter()
                .any(|c| c.detail.contains("relaxed") && c.detail.contains("`width`")),
            "{}",
            changes
                .iter()
                .map(|c| c.detail.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }

    #[test]
    fn gaining_a_param_shape_is_breaking_dropping_it_additive() {
        let mut after = base();
        scene_spawn(&mut after).params[0].shape =
            Some("{\"enum\":[\"a\",\"b\"],\"type\":\"string\"}".to_owned());
        let changes = diff(&base(), &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("gained a structured shape"),
            "{}",
            changes[0].detail
        );
        let before = after.clone();
        let bare = base();
        let changes = diff(&before, &bare);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Additive, "{}", changes[0].detail);
        let mut after = before.clone();
        scene_spawn(&mut after).params[0].shape = Some("{\"enum\":[\"a\"],\"type\":\"string\"}".to_owned());
        let changes = diff(&before, &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
        assert!(
            changes[0].detail.contains("shape changed"),
            "{}",
            changes[0].detail
        );
    }

    #[test]
    fn the_real_tables_render_a_stable_snapshot() {
        let protocol = current();
        assert!(!protocol.methods.is_empty());
        assert!(!protocol.universal.is_empty());
        let names: std::collections::BTreeSet<&str> =
            protocol.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names.len(), protocol.methods.len(), "method names are unique");
        assert!(names.contains("agent.hello"), "a schema method is present");
        assert!(
            names.contains("curves.list"),
            "a built-in extension method is present"
        );
        let text = render(&protocol);
        assert_eq!(
            render(&parse(&text).expect("parse")),
            text,
            "the real surface round-trips"
        );
    }

    /// A contract row that lists an error code (or a limit, or a param) twice renders it once,
    /// so the freshly rendered snapshot parses back instead of failing as "repeats the error
    /// code" (sprint-deep-zai-agent-protocol finding 5).
    #[test]
    fn duplicated_codes_limits_and_params_render_once_and_parse_back() {
        let mut protocol = base();
        let spawn = scene_spawn(&mut protocol);
        spawn.errors.push("bad_spec".to_owned());
        spawn.errors.push("bad_spec".to_owned());
        let limit = spawn.limits[0].clone();
        spawn.limits.push(limit);
        let param = spawn.params[0].clone();
        spawn.params.push(param);
        let protocol = protocol.normalized();
        let text = render(&protocol);
        assert_eq!(
            text.matches("errors: bad_spec\n").count(),
            1,
            "the code is listed once: {text}"
        );
        assert_eq!(
            text.matches("param: count number optional\n").count(),
            1,
            "the param is listed once: {text}"
        );
        let parsed = parse(&text).expect("the rendered snapshot parses back");
        assert_eq!(parsed, base(), "duplicates collapse to the base surface");
    }
}
