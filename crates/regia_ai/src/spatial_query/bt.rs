// SPDX-License-Identifier: MPL-2.0
//! The `RunSpatialQuery` behavior-tree leaf (`docs/specs/33-spatial-queries.md` § 4.1).

use std::collections::BTreeMap;

use super::{ParamValue, Pick, QueryStatus, RequestId, SpatialQueryManager, SpatialQueryRequest};
use crate::blackboard::{BbValue, Blackboard};
use crate::task::{BtContext, BtStatus, BtTask};
use crate::tree::BtTaskRegistry;

/// A `RunSpatialQuery` parameter: a literal or a key read from the agent's blackboard when the
/// request is submitted.
#[derive(Debug, Clone, PartialEq)]
pub enum BtParam {
    /// A literal.
    Literal(ParamValue),
    /// A blackboard key (bool, int, float or string values; others are skipped).
    Key(String),
}

/// A leaf that runs a spatial query and writes the winner to the blackboard.
///
/// First tick: submits the request and returns `Running`. Later ticks: `Running` while it processes;
/// on `success` with an item it writes the winner into `key` (a vec3 for point items; an entity for
/// entity items, plus `<key>_position` as a vec3) and returns `Success`, and removes a
/// `<key>_error` left by an earlier failure (and a `<key>_position` left by an earlier entity
/// winner when the new winner is a point). Any other status or an empty result returns `Failure`,
/// leaves `key` as it was and writes the reason into `<key>_error` (a string): the run's message,
/// `no item survived the filters` for a `Pick::All` success with zero survivors, or the status
/// name. Aborting the node (`reset`) aborts the request.
///
/// A tree names a configured leaf by the path it was registered under
/// ([`RunSpatialQuery::register`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunSpatialQuery {
    /// The template name or path.
    pub template: String,
    /// The blackboard key for the winner.
    pub key: String,
    /// The pick (`None`: the asset's default).
    pub pick: Option<Pick>,
    /// Parameters by name.
    pub params: BTreeMap<String, BtParam>,
    request: Option<RequestId>,
}

impl RunSpatialQuery {
    /// A leaf running `template` and writing the winner to `key`.
    pub fn new(template: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            key: key.into(),
            ..Self::default()
        }
    }

    /// Adds a parameter.
    pub fn with_param(mut self, name: impl Into<String>, value: BtParam) -> Self {
        self.params.insert(name.into(), value);
        self
    }

    /// Sets the pick.
    pub fn with_pick(mut self, pick: Pick) -> Self {
        self.pick = Some(pick);
        self
    }

    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }

    /// The request in flight, if any.
    pub fn request(&self) -> Option<RequestId> {
        self.request
    }

    fn params(&self, ctx: &BtContext<'_>) -> BTreeMap<String, ParamValue> {
        let bb = ctx.blackboard();
        self.params
            .iter()
            .filter_map(|(name, param)| {
                let value = match param {
                    BtParam::Literal(v) => Some(v.clone()),
                    BtParam::Key(key) => match bb.and_then(|bb| bb.get(key)) {
                        Some(BbValue::Bool(b)) => Some(ParamValue::Bool(*b)),
                        Some(BbValue::Int(i)) => Some(ParamValue::Int(*i)),
                        Some(BbValue::Float(f)) => Some(ParamValue::Float(*f)),
                        Some(BbValue::String(s)) => Some(ParamValue::Name(s.clone())),
                        _ => None,
                    },
                };
                value.map(|v| (name.clone(), v))
            })
            .collect()
    }
}

impl BtTask for RunSpatialQuery {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let Some(id) = self.request else {
            let params = self.params(ctx);
            let owner = ctx.entity;
            let Some(mut manager) = ctx.world.get_resource_mut::<SpatialQueryManager>() else {
                return BtStatus::Failure;
            };
            let mut req = SpatialQueryRequest::new(self.template.clone(), owner);
            req.params = params;
            req.pick = self.pick;
            self.request = Some(manager.submit(req));
            return BtStatus::Running;
        };
        let Some(active) = ctx
            .world
            .get_resource::<SpatialQueryManager>()
            .map(|manager| manager.is_active(id))
        else {
            self.request = None;
            return BtStatus::Failure;
        };
        if active {
            return BtStatus::Running;
        }
        // Without a blackboard the result has nowhere to go: keep the request (do not poll, so a
        // finished success survives) and fail this tick.
        if ctx.world.get::<Blackboard>(ctx.entity).is_none() {
            return BtStatus::Failure;
        }
        self.request = None;
        let Some(result) = ctx
            .world
            .get_resource_mut::<SpatialQueryManager>()
            .and_then(|mut manager| manager.poll(id))
        else {
            return BtStatus::Failure;
        };
        let best = if result.status == QueryStatus::Success {
            result.best().cloned()
        } else {
            None
        };
        let Some(best) = best else {
            // Say why on the blackboard: a missing parameter and a real miss look alike otherwise.
            // A `Pick::All` run with zero survivors is a success with no message of its own
            // (deep-zai lower note): name the empty survivor set, never the status "success".
            let message = result.message.clone().unwrap_or_else(|| {
                if result.status == QueryStatus::Success {
                    "no item survived the filters".to_string()
                } else {
                    result.status.name().to_string()
                }
            });
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert(format!("{}_error", self.key), message);
            }
            return BtStatus::Failure;
        };
        let Some(mut bb) = ctx.blackboard_mut() else {
            return BtStatus::Failure;
        };
        // Point queries never carry entities (entity children are flattened), so an entity marks
        // an entity item.
        // A success replaces every key a previous run of this leaf wrote: a stale failure reason or
        // an entity winner's position must not outlive it.
        bb.remove(&format!("{}_error", self.key));
        match best.entity {
            Some(entity) => {
                bb.insert(self.key.clone(), entity);
                bb.insert(format!("{}_position", self.key), best.position);
            }
            None => {
                bb.insert(self.key.clone(), best.position);
                bb.remove(&format!("{}_position", self.key));
            }
        }
        BtStatus::Success
    }

    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        if let Some(id) = self.request.take()
            && let Some(mut manager) = ctx.world.get_resource_mut::<SpatialQueryManager>()
        {
            manager.abort(id);
            let _ = manager.poll(id);
        }
    }
}
