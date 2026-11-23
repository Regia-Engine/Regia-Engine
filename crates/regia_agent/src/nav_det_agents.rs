// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the deterministic agents (plan row S-03, ADR-8405): `nav.det_agents`.
//!
//! The agents live in the host's lockstep sim; the host publishes a report into
//! [`regia_nav::det_view::DetAgentsView`] each tick and this method reads the last one. Without the
//! resource, or before the first report, it answers `nav_no_det_agents`. See
//! book/agents/det-grid.md.

use regia_app::prelude::*;
use regia_nav::det_view::DetAgentsView;
use serde_json::{Value, json};

use crate::nav::nav_error;
use crate::protocol::RpcError;

fn no_det_agents() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_det_agents",
        "no deterministic agents report in this app: the host inserts regia_nav::det_view::DetAgentsView and calls \
         publish(tick, &agents) once per tick",
    )
}

/// `nav.det_agents`: agent count, queue length, the counters (stuck and give-up among them),
/// every routed agent's waypoints, and the crowd flows (groups, lane mode, choke count), from the
/// host's last published report.
pub fn det_agents(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(view) = world.get_resource::<DetAgentsView>() else {
        return Err(no_det_agents());
    };
    let Some(report) = view.report.as_ref() else {
        return Err(no_det_agents());
    };
    let mut stats = serde_json::to_value(&report.stats).map_err(|e| RpcError::internal(e.to_string()))?;
    // The stuck log can hold 2000 rows: report its length, not the rows.
    if let Some(map) = stats.as_object_mut() {
        map.remove("stuck_log");
        map.insert("stuck_log_len".into(), json!(report.stats.stuck_log.len()));
    }
    let crowd = serde_json::to_value(&report.crowd).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(json!({
        "tick": view.tick,
        "agents": report.agents,
        "queue": report.queue,
        "disabled": report.disabled,
        "grid": report.grid,
        "grid_error": report.grid_error,
        "stats": stats,
        "routes": report.routes,
        "crowd": crowd,
    }))
}

#[cfg(test)]
mod tests {
    use regia_nav::det::agent::{DetAgentWorld, DetAgents, DetBody};
    use regia_nav::det::grid::{Profile, Stamp};

    use super::*;

    /// Two movers on open ground, one routed round a building.
    struct Two;

    impl DetAgentWorld for Two {
        fn ids(&self) -> Vec<u32> {
            vec![1, 2, 9]
        }
        fn body(&self, id: u32) -> Option<DetBody> {
            let pos = match id {
                1 => [-150.0, 0.0, 0.0],
                2 => [-150.0, 0.0, 30.0],
                9 => [0.0, 0.0, 0.0],
                _ => return None,
            };
            Some(DetBody {
                pos,
                radius: if id == 9 { 40.0 } else { 3.9 },
                profile: (id != 9).then_some(Profile::HOVER),
                team: 1,
                vel: [0.0, 0.0],
                yaw: 0.0,
                cruise_m_s: 20.0,
                soft: false,
            })
        }
        fn obstacle(&self, id: u32) -> Option<Stamp> {
            (id == 9).then(|| Stamp::from_world(0.0, 0.0, 40.0))
        }
    }

    fn reason(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("reason")?.as_str()
    }

    #[test]
    fn det_agents_reports_through_the_dispatcher() {
        let mut world = World::new();
        let missing = crate::dispatch::call_in_process(&mut world, "nav.det_agents", json!({}))
            .expect("answers now")
            .expect_err("no view");
        assert_eq!(reason(&missing), Some("nav_no_det_agents"));
        world.insert_resource(DetAgentsView::default());
        let empty = crate::dispatch::call_in_process(&mut world, "nav.det_agents", json!({}))
            .expect("answers now")
            .expect_err("no report yet");
        assert_eq!(reason(&empty), Some("nav_no_det_agents"));

        let mut agents = DetAgents::default();
        let host = Two;
        for tick in 1..=3 {
            agents.service(tick, &host);
            for id in [1, 2] {
                agents.steer(id, [150.0, 0.0, 0.0], 8.0, tick, &host);
            }
        }
        world.resource_mut::<DetAgentsView>().publish(3, &agents);
        let out = crate::dispatch::call_in_process(&mut world, "nav.det_agents", json!({}))
            .expect("answers now")
            .expect("a report");
        assert_eq!(out["tick"], 3);
        assert_eq!(out["agents"], 2);
        assert_eq!(out["queue"], 0);
        assert_eq!(out["disabled"], false);
        assert_eq!(out["grid"]["width"], 256);
        assert!(out["stats"]["requests"].as_u64().is_some_and(|n| n >= 2));
        assert_eq!(out["stats"]["stuck_events"], 0);
        assert_eq!(out["stats"]["gave_up"], 0);
        assert_eq!(out["stats"]["stuck_log_len"], 0);
        assert!(out["stats"].get("stuck_log").is_none());
        let routes = out["routes"].as_array().expect("routes");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["id"], 1);
        assert!(routes[0]["waypoints"].as_array().is_some_and(|w| w.len() >= 2));
        assert!(routes[0]["next"].as_u64().is_some_and(|n| n >= 1));
        assert_eq!(out["crowd"]["groups"], 0);
        assert_eq!(out["crowd"]["chokes"], 0);
        assert_eq!(out["crowd"]["lane_groups"], 0);
        assert_eq!(out["crowd"]["flows"].as_array().map(|rows| rows.len()), Some(0));
    }

    /// One registered choke files four members; `nav.det_agents` reports the flow.
    #[test]
    fn det_crowd_reports_groups_lanes_and_chokes() {
        let mut world = World::new();
        world.insert_resource(DetAgentsView::default());
        let mut agents = DetAgents::default();
        agents.register_choke([0.0, 0.0]);
        let members = [
            (1u32, [-40.0, 0.0]),
            (2, [-40.0, 12.0]),
            (3, [-52.0, 0.0]),
            (4, [-52.0, 12.0]),
        ];
        agents.crowd_refresh(7, &members, [40.0, 0.0], 1);
        assert!(agents.crowd.lane_mode(7), "the choke files the group");
        world.resource_mut::<DetAgentsView>().publish(1, &agents);
        let out = crate::dispatch::call_in_process(&mut world, "nav.det_agents", json!({}))
            .expect("answers now")
            .expect("a report");
        assert_eq!(out["tick"], 1);
        assert_eq!(out["crowd"]["groups"], 1);
        assert_eq!(out["crowd"]["chokes"], 1);
        assert_eq!(out["crowd"]["lane_groups"], 1);
        let flows = out["crowd"]["flows"].as_array().expect("flows");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0]["group"], 7);
        assert_eq!(flows[0]["members"], 4);
        assert_eq!(flows[0]["lane_mode"], true);
    }
}
