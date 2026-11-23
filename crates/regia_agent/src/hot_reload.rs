// SPDX-License-Identifier: MPL-2.0
//! `agent.hot_reload` (`docs/specs/23-agent-native.md` § 3.6, ADR-5707): asks the running
//! editor to rebuild the game crate and swap the new code in (the editor's Compile action,
//! `regia_editor::hot_reload`, driven from the protocol).
//!
//! The runtime has no game crate to reload, so the method answers `editor_not_running` unless
//! the host installed a [`HotReloadHook`] (the editor does, in `regia_editor::agent_ext::install`).
//! The hook runs in the editor's world: it checks the editor's compile state, asks the editor
//! loop for a build (its `CompileRequest`) and starts a `hot:<n>` job the loop finishes when the
//! host's build and swap complete ([`HotReloadJobPending`], serviced by the editor's
//! `code_reload::service_hot_reload_job`). The result carries the reload's structured outcome:
//! `accepted`, `phase`, `generation`, `detail`, `toast`, `elapsed_ms`, `types`, `instances`,
//! `scripts_rebuilt` and `scripts_dropped` (game script slots rebuilt from the new library or
//! dropped because their type is gone, ADR-7206).
//!
//! Rebuilding compiles and runs project code with the app's privileges, so the method needs the
//! `build` permission (specs/23 § 5). While a reload job runs, a second call is refused with
//! `hot_reload_busy` (the running job is polled with `agent.job_status`).

use std::time::Instant;

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::jobs::{JobHandle, JobRegistry, reason_error};
use crate::protocol::{RpcError, UNSUPPORTED};

/// The host's handler: runs a checked hot-reload request (`dry_run` reports without building).
pub type HotReloadHandler = fn(&mut World, bool) -> Result<Value, RpcError>;

/// Installed by an editor host so `agent.hot_reload` reaches it.
#[derive(Resource, Clone, Copy)]
pub struct HotReloadHook(pub HotReloadHandler);

impl std::fmt::Debug for HotReloadHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HotReloadHook(..)")
    }
}

/// A reload job the editor loop has not finished yet: the job handle it reports through, the
/// host's report serial at request time (a finish is recognised by a changed serial) and when
/// the request was made (the progress message reports the elapsed wait).
#[derive(Resource, Debug)]
pub struct HotReloadJobPending {
    /// The job's worker side.
    pub handle: JobHandle,
    /// The host's `report_serial` when the request was made.
    pub report_serial: u64,
    /// When the request started (progress messages report the wait).
    pub started: Instant,
}

/// `agent.hot_reload`: checks the host and the busy rule, then defers to the hook.
pub(crate) fn hot_reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let Some(HotReloadHook(handler)) = world.get_resource::<HotReloadHook>().copied() else {
        return Err(reason_error(
            UNSUPPORTED,
            "F0002",
            "editor_not_running",
            "this app is not the editor; connect to a running editor (regia mcp attaches to the editor's endpoint)",
        ));
    };
    refuse_if_pending(world)?;
    let mut answer = handler(world, dry_run)?;
    if dry_run && let Some(object) = answer.as_object_mut() {
        object.insert("dry_run".into(), json!(true));
    }
    Ok(answer)
}

/// The busy refusal both the built-in method and the editor hook use.
pub(crate) fn busy_error(running: &str) -> RpcError {
    reason_error(
        crate::protocol::INVALID_PARAMS,
        "F0002",
        "hot_reload_busy",
        format!("{running} is still running; poll it with agent.job_status or stop it with agent.job_cancel"),
    )
}

/// The busy rule for `agent.hot_reload`: a `hot:` job still running refuses the call (used by
/// the built-in method before the hook and by the editor hook on its own pending job).
///
/// # Errors
/// `hot_reload_busy` naming the running job.
pub fn refuse_if_pending(world: &mut World) -> Result<(), RpcError> {
    let running = world
        .get_resource::<JobRegistry>()
        .and_then(|registry| registry.running("hot"));
    if let Some(running) = running {
        Err(busy_error(&running))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// Every reason the code methods can answer (their contract errors, job failures included)
    /// is in the book page's reason table (review rk9 finding 4: `hot_reload_no_report`).
    #[test]
    fn every_code_method_reason_is_in_the_book_page() {
        let page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../book/agents/code.md"))
            .expect("book/agents/code.md");
        for method in ["agent.build_game", "agent.hot_reload"] {
            let contract = crate::contract::contract(method).expect("contract");
            for reason in contract.errors.iter().filter(|e| !e.starts_with('F')) {
                assert!(
                    page.contains(&format!("| `{reason}` |")),
                    "{method}: `{reason}` has no row in book/agents/code.md's reason table"
                );
            }
        }
        assert!(
            crate::contract::contract("agent.hot_reload")
                .expect("contract")
                .errors
                .contains(&"hot_reload_no_report"),
            "the job failure hot_reload_no_report is listed with the method's errors"
        );
    }
}
