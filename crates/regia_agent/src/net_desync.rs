// SPDX-License-Identifier: MPL-2.0
//! Lockstep desync diagnosis on the agent protocol (spec 120 § 7.6 and § 13.3, ADR-8455;
//! `book/agents/net-desync.md`): `net.desync` reads each published host's or seat's newest
//! verdict, its probe rows, the bundle it wrote and the offline replay's verdict;
//! `net.desync_inject` (development builds only) queues a one-subsystem flip at a chosen tick for
//! the game's step to apply.
//!
//! Codes: `desync_unknown_endpoint` (no published lockstep host or seat has that label),
//! `desync_inject_disabled` (`net.desync_inject` in a release build), `desync_inject_invalid`
//! (a missing or empty `subsystem`, a missing `tick`, a zero `mask`), `desync_no_endpoint`
//! (nothing published to inject into).

use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::desync::DesyncInject;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn endpoint_param(params: &Value) -> Result<Option<&str>, RpcError> {
    match params.get("endpoint") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(RpcError::invalid_params("`endpoint` must be a non-empty string")),
    }
}

/// Every label `net.desync` and `net.desync_inject` know: published lockstep statuses and
/// published diagnoses.
fn labels(state: Option<&NetAgentState>) -> Vec<String> {
    let mut out: Vec<String> = state
        .map(|s| s.lockstep().keys().chain(s.desync().keys()).cloned().collect())
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

fn unknown(label: &str, state: Option<&NetAgentState>) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "desync_unknown_endpoint",
        format!("no lockstep host or seat named `{label}`"),
        format!(
            "published: {:?} (net.desync without `endpoint` lists them)",
            labels(state)
        ),
    )
}

/// `net.desync`: per published lockstep host or seat, the probe report count, the newest verdict
/// (first diverged tick and subsystem), the newest probe's rows by voter, the bundle written and
/// the offline replay's verdict, plus injections waiting.
pub(crate) fn desync(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let state = world.get_resource::<NetAgentState>();
    let known = labels(state);
    if let Some(label) = target
        && !known.iter().any(|k| k == label)
    {
        return Err(unknown(label, state));
    }
    let endpoints: Vec<Value> = known
        .iter()
        .filter(|label| target.is_none_or(|t| t == label.as_str()))
        .map(|label| {
            let status = state
                .and_then(|s| s.desync().get(label))
                .cloned()
                .unwrap_or_default();
            let pending = state.map_or(&[][..], |s| s.pending_desync_injects(label));
            json!({
                "label": label,
                "reports": status.reports,
                "last": status.last,
                "rows": status.rows,
                "bundle": status.bundle,
                "replay": status.replay,
                "pending_injections": pending,
            })
        })
        .collect();
    Ok(json!({ "endpoints": endpoints }))
}

/// `net.desync_inject` (development builds): queues a flip of `subsystem` at `tick` for one
/// lockstep host or seat, or for all; the game takes it with `NetAgentState::take_desync_inject`
/// and applies it in its step.
pub(crate) fn desync_inject(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    desync_inject_gated(world, params, cfg!(debug_assertions))
}

fn desync_inject_gated(world: &mut World, params: &Value, allowed: bool) -> Result<Value, RpcError> {
    if !allowed {
        return Err(RpcError::new(
            UNSUPPORTED,
            "desync_inject_disabled",
            "net.desync_inject is available in development and test builds only",
            "run a development build; a shipped match never plants a divergence",
        ));
    }
    let invalid = |message: &str| {
        RpcError::new(
            INVALID_PARAMS,
            "desync_inject_invalid",
            message,
            "pass { subsystem: \"<name>\", tick: <n>, mask?: <non-zero u64>, endpoint?: \"<label>\" }",
        )
    };
    let target = endpoint_param(params)?;
    let subsystem = match params.get("subsystem") {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= regia_net::desync::MAX_SUBSYSTEM_NAME_BYTES => {
            s.clone()
        }
        _ => {
            return Err(invalid(
                "`subsystem` (a subsystem name, 1 to 64 bytes) is required",
            ));
        }
    };
    let tick = params
        .get("tick")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("`tick` (the tick whose step flips the subsystem) is required"))?;
    let mut inject = DesyncInject::new(subsystem, tick);
    match params.get("mask") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_u64() {
            Some(mask) if mask != 0 => inject.mask = mask,
            _ => return Err(invalid("`mask` must be a non-zero unsigned integer")),
        },
    }
    let known = labels(world.get_resource::<NetAgentState>());
    if let Some(label) = target
        && !known.iter().any(|k| k == label)
    {
        return Err(unknown(label, world.get_resource::<NetAgentState>()));
    }
    if known.is_empty() {
        return Err(RpcError::new(
            NOT_FOUND,
            "desync_no_endpoint",
            "no lockstep host or seat is published",
            "the game publishes them with regia_net::lockstep::net::publish_host / publish_seat",
        ));
    }
    let queued = world
        .get_resource_or_insert_with(NetAgentState::default)
        .request_desync_inject(target, inject.clone());
    Ok(json!({ "queued_for": queued, "inject": inject }))
}

#[cfg(test)]
mod tests {
    use regia_net::desync::sample::{SampleGame, SampleSim};
    use regia_net::desync::{DesyncBundle, DesyncStatus, ReplayRegistry};
    use regia_net::lockstep::{HostRole, LockstepConfig, LockstepHost, LockstepSeat, SeatId};

    use super::*;

    fn code(err: &RpcError) -> String {
        format!("{err:?}")
    }

    /// A listen host (seat 0) and seats 1 and 2 stepping the sample over direct delivery; each
    /// seat's simulation takes the injections queued for its label (`seat1`, `seat2`). Self-check
    /// is off: the sample seats do not replay.
    fn run_match(world: &mut World, ticks: u64) -> LockstepHost<SampleGame> {
        let config = LockstepConfig {
            self_check: false,
            ..LockstepConfig::default()
        };
        let mut host = LockstepHost::<SampleGame>::new(config.clone(), HostRole::Listen(SeatId(0)));
        host.add_seat(SeatId(0));
        let mut seats: Vec<(LockstepSeat<SampleGame>, SampleSim)> = (1..=2u16)
            .map(|id| {
                host.add_seat(SeatId(id));
                (
                    LockstepSeat::new(config.clone(), SeatId(id)),
                    SampleSim::default(),
                )
            })
            .collect();
        host.start(0);
        let mut host_sim = SampleSim::default();
        let input = regia_net::desync::sample::sample_input;
        let mut now = 0u64;
        for (seat, _) in &mut seats {
            let id = seat.seat().0;
            let _ = seat.submit_input(&input(id, 0));
        }
        let _ = host.local_input(&input(0, 0), now);
        while host.tick() < ticks && now < ticks * 40 {
            for (seat, _) in &mut seats {
                let id = seat.seat();
                for out in seat.outgoing(now) {
                    let _ = host.receive(id, out.lane, &out.bytes, now);
                }
            }
            while let Some(frame) = host.poll_frame(now) {
                let inputs = frame.inputs.iter().map(|(s, i)| (s.0, i.as_ref().copied()));
                host_sim.step_inputs(frame.tick, inputs);
                host.record_state(frame.tick, &host_sim);
                let _ = host.local_input(&input(0, frame.tick + 1), now);
            }
            while host.poll_event().is_some() {}
            for (to, out) in host.outgoing(now) {
                if let Some((seat, _)) = seats.iter_mut().find(|(s, _)| s.seat() == to) {
                    let _ = seat.receive(out.lane, &out.bytes, now);
                }
            }
            for (seat, sim) in &mut seats {
                let label = format!("seat{}", seat.seat().0);
                while let Some(frame) = seat.next_frame() {
                    if let Some(inject) = world.resource_mut::<NetAgentState>().take_desync_inject(&label) {
                        sim.inject = Some(inject);
                    }
                    let inputs = frame.inputs.iter().map(|(s, i)| (s.0, i.as_ref().copied()));
                    sim.step_inputs(frame.tick, inputs);
                    seat.record_state(frame.tick, sim);
                    let _ = seat.submit_input(&input(seat.seat().0, frame.tick + 1));
                }
                while seat.poll_event().is_some() {}
            }
            let mut state = world.resource_mut::<NetAgentState>();
            regia_net::lockstep::net::publish_host(&mut host, "host", now, &mut state);
            for (seat, _) in &seats {
                regia_net::lockstep::net::publish_seat(seat, &format!("seat{}", seat.seat().0), &mut state);
            }
            now += 16;
        }
        host
    }

    /// `net.desync_inject` queues a flip for the seat, the seat's step applies it, the host's
    /// probe names it, and `net.desync` shows the verdict, the rows, the bundle written and the
    /// offline replay's verdict, which agrees with the live one.
    #[test]
    fn net_desync_inject_is_named_by_the_probe_and_net_desync_reports_it() {
        let mut world = World::new();
        world.insert_resource(NetAgentState::default());
        // Nothing published yet.
        let err = desync_inject(&mut world, &json!({"subsystem": "economy", "tick": 40})).expect_err("none");
        assert!(code(&err).contains("desync_no_endpoint"), "{err:?}");
        world.resource_mut::<NetAgentState>().publish_lockstep(
            "seat1",
            LockstepSeat::<SampleGame>::new(LockstepConfig::default(), SeatId(1)).status(),
        );
        let queued = desync_inject(
            &mut world,
            &json!({"endpoint": "seat1", "subsystem": "economy", "tick": 40}),
        )
        .expect("queued");
        assert_eq!(queued["queued_for"], json!(["seat1"]));
        let before = desync(&mut world, &json!({"endpoint": "seat1"})).expect("net.desync");
        assert_eq!(before["endpoints"][0]["pending_injections"][0]["tick"], json!(40));

        // Checkpoint 60 is the verdict; stop before the unrepaired seat (the sample seats do not
        // take a transfer) can be judged again.
        let host = run_match(&mut world, 100);
        let (reports, last) = host.desync_reports();
        assert_eq!(reports, 1);
        let last = last.expect("a report").clone();
        assert_eq!(
            (last.first_diverged_tick, last.subsystem.as_deref()),
            (Some(40), Some("economy"))
        );

        // The host application writes the bundle (from the match start) and replays it.
        let bundle =
            DesyncBundle::from_host(&host, regia_net::desync::BundleSnapshot::start()).expect("bundle");
        let verdict = ReplayRegistry::with_builtin().replay(&bundle).expect("replay");
        assert_eq!(verdict.matches_live, Some(true), "{verdict:?}");
        let mut status =
            DesyncStatus::of_host(&host).with_bundle(std::path::Path::new("saved/desync/x.desync.ron"));
        status.replay = Some(verdict);
        world
            .resource_mut::<NetAgentState>()
            .publish_desync("host", status);

        let out = desync(&mut world, &json!({"endpoint": "host"})).expect("net.desync host");
        let row = &out["endpoints"][0];
        assert_eq!(row["reports"], json!(1));
        assert_eq!(row["last"]["subsystem"], json!("economy"));
        assert_eq!(row["last"]["first_diverged_tick"], json!(40));
        assert_eq!(row["bundle"], json!("saved/desync/x.desync.ron"));
        assert_eq!(row["replay"]["first_diverging_tick"], json!(40));
        assert_eq!(row["replay"]["matches_live"], json!(true));
        let voters: Vec<&Value> = row["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| &r["voter"])
            .collect();
        assert_eq!(voters.len(), 2, "the reference and the diverged seat: {row}");
        // Taken: nothing waits any more.
        let seat = desync(&mut world, &json!({"endpoint": "seat1"})).expect("seat");
        assert_eq!(seat["endpoints"][0]["pending_injections"], json!([]));
        let all = desync(&mut world, &json!({})).expect("all");
        assert_eq!(
            all["endpoints"].as_array().map(Vec::len),
            Some(3),
            "host, seat1, seat2"
        );
    }

    /// Every error code of both methods, including the release-build refusal.
    #[test]
    fn net_desync_error_codes() {
        let mut world = World::new();
        world.insert_resource(NetAgentState::default());
        world.resource_mut::<NetAgentState>().publish_lockstep(
            "host",
            LockstepSeat::<SampleGame>::new(LockstepConfig::default(), SeatId(1)).status(),
        );
        let err = desync(&mut world, &json!({"endpoint": "nope"})).expect_err("unknown");
        assert!(code(&err).contains("desync_unknown_endpoint"), "{err:?}");
        let err = desync_inject(
            &mut world,
            &json!({"endpoint": "nope", "subsystem": "a", "tick": 1}),
        )
        .expect_err("unknown");
        assert!(code(&err).contains("desync_unknown_endpoint"), "{err:?}");
        for bad in [
            json!({"tick": 1}),
            json!({"subsystem": "", "tick": 1}),
            json!({"subsystem": "a"}),
            json!({"subsystem": "a", "tick": 1, "mask": 0}),
            json!({"subsystem": "a", "tick": -1}),
        ] {
            let err = desync_inject(&mut world, &bad).expect_err("invalid");
            assert!(code(&err).contains("desync_inject_invalid"), "{bad}: {err:?}");
        }
        let err = desync_inject_gated(&mut world, &json!({"subsystem": "a", "tick": 1}), false)
            .expect_err("release");
        assert!(code(&err).contains("desync_inject_disabled"), "{err:?}");
        assert!(
            world
                .resource::<NetAgentState>()
                .pending_desync_injects("host")
                .is_empty()
        );
        let ok = desync_inject(&mut world, &json!({"subsystem": "a", "tick": 1, "mask": 4})).expect("all");
        assert_eq!(ok["inject"]["mask"], json!(4));
        assert_eq!(ok["queued_for"], json!(["host"]));
    }
}
