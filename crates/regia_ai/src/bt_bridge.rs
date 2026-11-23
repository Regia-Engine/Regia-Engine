// SPDX-License-Identifier: MPL-2.0
//! Bridges between a behavior tree and a state tree (ADR-4543).
//!
//! A [`crate::DefKind::StateTree`] node runs a `.statetree.ron` until it succeeds or fails.
//! A state-tree task whose path starts with `bt:` runs a `.bt.ron` the same way.
//! Bridges nest at most four deep (`BRIDGE_DEPTH`), and a path may not appear twice on that stack.

use std::cell::RefCell;

use crate::state_tree::{StateTreeError, StateTreeRunStatus};
use crate::task::{BtContext, BtStatus, BtTask};
use crate::tree::{BehaviorTree, BtTaskRegistry};

/// How many bridges may be open while one tree compiles.
pub(crate) const BRIDGE_DEPTH: usize = 4;

const BT_PREFIX: &str = "bt:";
const BT_SUFFIX: &str = ".bt.ron";

thread_local! {
    static BRIDGE_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct BridgeGuard;

impl Drop for BridgeGuard {
    fn drop(&mut self) {
        BRIDGE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// Pushes `path` for the nested compile that `guard` covers.
///
/// # Errors
///
/// `bridge_cycle` when `path` is already on the stack, `bridge_depth` past four open bridges.
pub(crate) fn enter_bridge(path: &str) -> Result<BridgeGuard, &'static str> {
    BRIDGE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.iter().any(|seen| seen == path) {
            return Err("bridge_cycle");
        }
        if stack.len() >= BRIDGE_DEPTH {
            return Err("bridge_depth");
        }
        stack.push(path.to_string());
        Ok(BridgeGuard)
    })
}

/// Compiles a `bt:<path>.bt.ron` task. `Ok(None)` when `path` is an ordinary task path.
///
/// # Errors
///
/// The prefix is present but the path, the file or the tree is not usable.
pub(crate) fn compile_bt_task(
    path: &str,
    state: &str,
    registry: &BtTaskRegistry,
    load: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Box<dyn BtTask>>, StateTreeError> {
    let Some(asset) = path.strip_prefix(BT_PREFIX) else {
        return Ok(None);
    };
    let asset = asset.trim();
    if asset.is_empty() || !asset.ends_with(BT_SUFFIX) {
        return Err(StateTreeError::BadBehaviorTree {
            state: state.to_string(),
            detail: format!("task {path:?} must be bt:<path>.bt.ron"),
        });
    }
    let _guard = enter_bridge(asset).map_err(|code| StateTreeError::BadBehaviorTree {
        state: state.to_string(),
        detail: format!("{code} at {asset}"),
    })?;
    let text = load(asset).ok_or_else(|| StateTreeError::BadBehaviorTree {
        state: state.to_string(),
        detail: format!("could not load {asset}"),
    })?;
    let def = crate::bt_edit::parse_bt_ron(&text).map_err(|detail| StateTreeError::BadBehaviorTree {
        state: state.to_string(),
        detail,
    })?;
    let tree =
        BehaviorTree::compile_with(&def, registry, load).map_err(|err| StateTreeError::BadBehaviorTree {
            state: state.to_string(),
            detail: err.to_string(),
        })?;
    Ok(Some(Box::new(BtBridgeTask { tree, latched: None })))
}

/// Maps a finished state tree onto a behavior-tree status.
#[must_use]
pub(crate) fn map_state_status(status: StateTreeRunStatus) -> BtStatus {
    match status {
        StateTreeRunStatus::Succeeded => BtStatus::Success,
        StateTreeRunStatus::Failed => BtStatus::Failure,
        StateTreeRunStatus::Running | StateTreeRunStatus::NotStarted => BtStatus::Running,
    }
}

struct BtBridgeTask {
    tree: BehaviorTree,
    latched: Option<BtStatus>,
}

impl BtTask for BtBridgeTask {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if let Some(status) = self.latched {
            return status;
        }
        let status = self.tree.tick(ctx);
        if status != BtStatus::Running {
            self.latched = Some(status);
        }
        status
    }

    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        self.latched = None;
        self.tree.reset(ctx);
    }
}
