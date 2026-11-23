// SPDX-License-Identifier: MPL-2.0
#![cfg(test)]

use std::time::Duration;

use regia_framework::desync_hash::{DesyncHashPlugin, Systems};
use serde::Serialize;
use serde_json::{Value, json};

use super::*;
use crate::dispatch::{AgentInbox, RateLimit, process_requests};
use crate::protocol::{Request, Response};
use crate::safety::{AgentConfig, AgentPermissions, AgentSession, AuditLog};
use crate::server::Inbound;

#[derive(Component, Serialize)]
struct Pos([i32; 2]);

#[derive(Resource, Serialize, Default)]
struct Gold(u32);

const SYSTEMS: Systems<2> = Systems::new(["movement", "economy"]);

fn step(mut q: Query<&mut Pos>) {
    for mut pos in &mut q {
        pos.0[0] += 1;
    }
}

/// A minimal app with the dispatch resources, one moving entity and the desync hash recording in
/// `Update` (one row per `app.update()`).
fn app(with_hash: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentPermissions {
            config: AgentConfig::default(),
            source: None,
        })
        .init_resource::<AuditLog>()
        .init_resource::<AgentSession>()
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>()
        .init_resource::<Gold>();
    app.add_systems(Update, step);
    if with_hash {
        app.add_plugins(
            DesyncHashPlugin::new(SYSTEMS)
                .component::<Pos>("movement")
                .resource::<Gold>("economy")
                .in_schedule(Update)
                .after(step),
        );
    }
    app.world_mut().spawn(Pos([0, 0]));
    app
}

/// Drives one request through `process_requests` (the dispatcher) and returns the response.
fn call(app: &mut App, method: &str, params: Value) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    drop(inbox_tx);
    rx.recv_timeout(Duration::from_secs(2)).expect("answered")
}

fn ok(app: &mut App, params: Value) -> Value {
    let response = call(app, "sim.desync_rows", params.clone());
    match response.result {
        Some(result) => result,
        None => panic!("sim.desync_rows {params}: {:?}", response.error),
    }
}

#[test]
fn desync_rows_dispatch_names_the_split_system() {
    let (mut a, mut b) = (app(true), app(true));
    for tick in 1..=6 {
        if tick == 4 {
            b.world_mut().resource_mut::<Gold>().0 = 9;
        }
        a.update();
        b.update();
    }

    let rows_a = ok(&mut a, json!({}));
    assert_eq!(rows_a["systems"], json!(["movement", "economy"]));
    assert_eq!(rows_a["newest"], json!(6));
    assert_eq!(rows_a["recorded"], json!(6));
    let rows = rows_a["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0]["tick"], json!(1));
    assert_eq!(rows[0]["words"].as_array().expect("words").len(), 2);
    assert!(
        rows[0]["words"][0]
            .as_str()
            .is_some_and(|w| w.len() == 16 && w.bytes().all(|c| c.is_ascii_hexdigit())),
        "{rows_a}"
    );
    assert!(rows_a.get("report").is_none());

    // The window: `to` and `count`.
    let window = ok(&mut a, json!({ "to": 5, "count": 2 }));
    let ticks: Vec<&Value> = window["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| &r["tick"])
        .collect();
    assert_eq!(ticks, [&json!(4), &json!(5)]);

    // B against A's rows: economy split at step 4, every step before it agreed.
    let verdict = ok(
        &mut b,
        json!({ "against": rows_a["rows"], "left": "b", "right": "a" }),
    );
    let report = &verdict["report"];
    assert_eq!(report["same"], json!(false));
    assert_eq!(report["root"], json!("economy"));
    assert_eq!(report["first_tick"], json!(4));
    assert_eq!(report["last_agreed"], json!(3));
    assert_eq!(report["compared"], json!(6));
    assert_eq!(report["list"], json!("economy@4"));
    let lines = verdict["lines"].as_array().expect("lines");
    assert_eq!(
        lines[1],
        json!(
            "lockstep desync verdict split 6 left b right a root economy first 4 compared 6 last_agreed 3 systems economy@4"
        )
    );

    // A against its own rows, given as `lockstep systems row` log lines: no split.
    let as_lines: Vec<String> = rows
        .iter()
        .map(|r| {
            let words: Vec<&str> = r["words"]
                .as_array()
                .expect("words")
                .iter()
                .map(|w| w.as_str().expect("hex"))
                .collect();
            format!("[peer] lockstep systems row a {} {}", r["tick"], words.join(":"))
        })
        .collect();
    let same = ok(&mut a, json!({ "against": as_lines }));
    assert_eq!(same["report"]["same"], json!(true));
    assert_eq!(same["report"]["root"], Value::Null);
    assert_eq!(same["report"]["last_agreed"], json!(6));
}

#[test]
fn desync_rows_errors_name_their_code() {
    let mut bare = app(false);
    let missing = call(&mut bare, "sim.desync_rows", json!({}));
    let error = missing.error.expect("no plugin");
    assert_eq!(
        error.data.as_ref().and_then(|d| d["code"].as_str()),
        Some("F0003")
    );
    assert!(error.message.contains("[no_desync_hash]"), "{}", error.message);

    let mut app = app(true);
    app.update();
    for (params, code) in [
        (json!({ "to": -1 }), "[bad_to]"),
        (json!({ "to": "5" }), "[bad_to]"),
        (json!({ "count": 0 }), "[bad_count]"),
        (json!({ "count": 97 }), "[bad_count]"),
        (json!({ "split": 1.5 }), "[bad_split]"),
        (json!({ "left": "has space" }), "[bad_label]"),
        (json!({ "right": "" }), "[bad_label]"),
        (json!({ "against": {} }), "[bad_against]"),
        (json!({ "against": [{ "tick": 1 }] }), "[bad_against]"),
        (
            json!({ "against": [{ "tick": 1, "words": ["zz"] }] }),
            "[bad_against]",
        ),
        (
            json!({ "against": ["lockstep systems row a 1 0:1 extra"] }),
            "[bad_against]",
        ),
        (
            json!({ "against": vec![json!({ "tick": 1, "words": ["0"] }); 97] }),
            "[bad_against]",
        ),
        (
            json!({ "against": [{ "tick": 1, "words": ["0"] }] }),
            "[word_count]",
        ),
    ] {
        let response = call(&mut app, "sim.desync_rows", params.clone());
        let error = response.error.unwrap_or_else(|| panic!("{params} must fail"));
        assert!(
            error.message.contains(code),
            "{params}: {code} not in {}",
            error.message
        );
    }
}

#[test]
fn no_rows_before_the_first_step() {
    let mut app = app(true);
    let empty = ok(&mut app, json!({ "against": [] }));
    assert_eq!(empty["newest"], Value::Null);
    assert_eq!(empty["recorded"], json!(0));
    assert_eq!(empty["rows"], json!([]));
    assert_eq!(empty["report"]["compared"], json!(0));
    // `sim_desync_rows` is the handler the dispatcher calls.
    assert_eq!(
        sim_desync_rows(app.world_mut(), &json!({})).expect("ok")["systems"],
        json!(["movement", "economy"])
    );
}

/// An external sim (no ECS) that implements the per-system fold: `movement` holds a position and a
/// value that may become a signed zero, `economy` a gold count.
struct ExtSim {
    tick: u64,
    pos: [i64; 2],
    zero: f32,
    gold: u32,
    gold_at: Option<u64>,
    sign_at: Option<u64>,
}

impl ExtSim {
    fn new(gold_at: Option<u64>, sign_at: Option<u64>) -> Self {
        Self {
            tick: 0,
            pos: [0, 0],
            zero: 0.0,
            gold: 0,
            gold_at,
            sign_at,
        }
    }
}

impl regia_framework::sim_host::FixedStepSim for ExtSim {
    type Frame = u64;
    type Event = ();
    fn step(&mut self, _input: &regia_framework::sim_bridge::BridgeInput) {
        self.tick += 1;
        self.pos[0] += 1;
        if self.gold_at == Some(self.tick) {
            self.gold += 1;
        }
        if self.sign_at == Some(self.tick) {
            self.zero = -0.0;
        }
    }
    fn tick(&self) -> u64 {
        self.tick
    }
    fn write_frame(&self, out: &mut u64) {
        *out = self.tick;
    }
}

impl regia_framework::desync_hash::SystemFold<2> for ExtSim {
    const SYSTEMS: Systems<2> = Systems::new(["movement", "economy"]);
    fn fold_systems(&self, mix: &mut [regia_framework::desync_hash::Mix; 2]) {
        mix[0].fold(&self.pos);
        mix[0].f32(self.zero);
        mix[1].u64(u64::from(self.gold));
    }
}

/// Start `sim` behind a manual threaded host that records its desync rows (raw too), in an app
/// without the ECS desync plugin.
fn host_app(sim: ExtSim) -> (App, regia_framework::sim_host::ThreadedSimHost<ExtSim>) {
    use regia_framework::sim_host::{HostPacing, SimHostSettings, ThreadedSimHost};
    let host = ThreadedSimHost::start_with_desync::<2>(
        sim,
        SimHostSettings {
            pacing: HostPacing::Manual,
            ..SimHostSettings::default()
        },
        true,
    )
    .expect("start");
    let mut app = app(false);
    app.insert_resource(host.handle());
    (app, host)
}

/// Grant `ticks` through `sim.host_step` and wait (on the publish, no sleeps) until the sim is there.
fn step_host(app: &mut App, host: &regia_framework::sim_host::ThreadedSimHost<ExtSim>, ticks: u64) {
    let before = host.published().curr_tick;
    let step = call(app, "sim.host_step", json!({ "ticks": ticks }));
    assert!(step.error.is_none(), "{:?}", step.error);
    let mut pair = host.published();
    while pair.curr_tick < before + ticks {
        pair = host.wait_published(pair.sequence).expect("steps");
    }
}

#[test]
fn sim_desync_rows_answers_for_an_external_sim() {
    // Peer A runs clean; peer B's zero turns negative at 3 and its gold moves at 6.
    let (mut a, host_a) = host_app(ExtSim::new(None, None));
    let (mut b, host_b) = host_app(ExtSim::new(Some(6), Some(3)));
    step_host(&mut a, &host_a, 8);
    step_host(&mut b, &host_b, 8);

    let rows_a = ok(&mut a, json!({}));
    assert_eq!(rows_a["source"], json!("sim_host"));
    assert_eq!(rows_a["systems"], json!(["movement", "economy"]));
    assert_eq!(rows_a["raw"], json!(true));
    assert_eq!(rows_a["partial_rows"], json!(0));
    // The boot state (tick 0) and every step.
    assert_eq!(rows_a["newest"], json!(8));
    assert_eq!(rows_a["recorded"], json!(9));
    let rows = rows_a["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 9);
    assert_eq!(rows[0]["tick"], json!(0));
    assert_eq!(rows[8]["raw"].as_array().map(Vec::len), Some(2));
    crate::contract::assert_result_named("sim.desync_rows", &rows_a);

    // B against A: the canonical rule names economy at 6; the raw rows name the zero sign at 3.
    let verdict = ok(
        &mut b,
        json!({ "against": rows_a["rows"], "left": "b", "right": "a" }),
    );
    let report = &verdict["report"];
    assert_eq!(report["list"], json!("economy@6"));
    assert_eq!(report["raw_list"], json!("movement@3,economy@6"));
    assert_eq!(
        report["float_bits"],
        json!([{ "system": "movement", "raw_tick": 3, "canonical_tick": null }])
    );
    assert_eq!(report["partial_from"], Value::Null);
    let lines = verdict["lines"].as_array().expect("lines");
    assert_eq!(
        lines[2],
        json!("lockstep desync raw movement tick 3 b vs a: zero sign or NaN payload split (canonical never)")
    );
    crate::contract::assert_result_named("sim.desync_rows", &verdict);

    // The handle answers the same rows as the method (and an ECS plugin, when present, wins).
    let direct = host_b.handle().desync().expect("recording").rows(Some(8), 2);
    assert_eq!(direct.rows.len(), 2);
    assert_eq!(direct.rows[1].tick, 8);
    let mut both = app(true);
    both.insert_resource(host_a.handle());
    both.update();
    assert_eq!(ok(&mut both, json!({}))["source"], json!("ecs"));

    // A host started without a fold has no rows: the method still answers `no_desync_hash`.
    let plain = regia_framework::sim_host::ThreadedSimHost::start(
        ExtSim::new(None, None),
        regia_framework::sim_host::SimHostSettings::default(),
    )
    .expect("start");
    let mut bare = app(false);
    bare.insert_resource(plain.handle());
    let error = call(&mut bare, "sim.desync_rows", json!({}))
        .error
        .expect("no rows");
    assert!(error.message.contains("[no_desync_hash]"), "{}", error.message);
}

#[test]
fn against_rows_carry_raw_words_and_partial_flags() {
    let mut app = app(true);
    app.update();
    let response = call(
        &mut app,
        "sim.desync_rows",
        json!({ "against": [{ "tick": 1, "words": ["0", "0"], "raw": ["0"] }] }),
    );
    let error = response.error.expect("short raw vector");
    assert!(error.message.contains("[word_count]"), "{}", error.message);
    let response = call(
        &mut app,
        "sim.desync_rows",
        json!({ "against": [{ "tick": 1, "words": ["0", "0"], "partial": "yes" }] }),
    );
    let error = response.error.expect("partial must be a bool");
    assert!(error.message.contains("[bad_against]"), "{}", error.message);
    let fine = ok(
        &mut app,
        json!({ "against": [{ "tick": 1, "words": ["0", "0"], "raw": ["0", "0"], "partial": true }] }),
    );
    assert_eq!(fine["report"]["partial_from"], json!(1));
    assert_eq!(fine["source"], json!("ecs"));
    assert_eq!(fine["raw"], json!(false));
}

// ADR-8621: `report.same` means agreement in every view, and `report.verdict` names the view that
// disagreed.

#[test]
fn same_is_false_when_only_the_zero_sign_differs() {
    let (mut a, host_a) = host_app(ExtSim::new(None, None));
    let (mut b, host_b) = host_app(ExtSim::new(None, Some(3)));
    step_host(&mut a, &host_a, 8);
    step_host(&mut b, &host_b, 8);
    let rows_a = ok(&mut a, json!({}));
    let verdict = ok(&mut b, json!({ "against": rows_a["rows"] }));
    let report = &verdict["report"];
    assert_eq!(report["list"], json!(""), "the canonical words agree");
    assert_eq!(report["float_bits"][0]["raw_tick"], json!(3));
    assert_eq!(report["float_bits"][0]["canonical_tick"], Value::Null);
    assert_eq!(report["same"], json!(false));
    assert_eq!(report["verdict"], json!("float_bits"));
    let first = verdict["lines"][0].as_str().unwrap_or_default();
    assert!(
        first.contains("raw words split first movement at tick 3"),
        "the first line names the raw view: {first}"
    );
    crate::contract::assert_result_named("sim.desync_rows", &verdict);
}

#[test]
fn same_is_false_when_no_tick_was_compared() {
    let (mut a, host_a) = host_app(ExtSim::new(None, None));
    step_host(&mut a, &host_a, 4);
    let verdict = ok(
        &mut a,
        json!({ "against": [{ "tick": 1000, "words": ["1", "2"], "raw": ["1", "2"] }] }),
    );
    let report = &verdict["report"];
    assert_eq!(report["compared"], json!(0));
    assert_eq!(report["same"], json!(false));
    assert_eq!(report["verdict"], json!("no_overlap"));
    let first = verdict["lines"][0].as_str().unwrap_or_default();
    assert!(
        first.ends_with("not agreed: no shared tick (0 compared)"),
        "{first}"
    );
}

#[test]
fn same_is_false_and_last_agreed_stops_before_a_partial_row() {
    let (mut a, host_a) = host_app(ExtSim::new(None, None));
    step_host(&mut a, &host_a, 4);
    let rows = ok(&mut a, json!({}))["rows"].clone();
    // The peer's rows: the same words, but its tick 3 fold was partial.
    let mut theirs = rows.as_array().cloned().unwrap_or_default();
    for row in &mut theirs {
        if row["tick"] == json!(3) {
            row["partial"] = json!(true);
        }
    }
    let verdict = ok(&mut a, json!({ "against": theirs }));
    let report = &verdict["report"];
    assert_eq!(report["list"], json!(""));
    assert_eq!(report["partial_from"], json!(3));
    assert_eq!(report["last_agreed"], json!(2), "a partial step is not agreement");
    assert_eq!(report["same"], json!(false));
    assert_eq!(report["verdict"], json!("partial"));
    let first = verdict["lines"][0].as_str().unwrap_or_default();
    assert!(first.contains("not agreed: partial rows from tick 3"), "{first}");

    // The control: the same rows without the flag agree.
    let verdict = ok(&mut a, json!({ "against": rows }));
    assert_eq!(verdict["report"]["same"], json!(true));
    assert_eq!(verdict["report"]["verdict"], json!("same"));
    assert_eq!(verdict["report"]["last_agreed"], json!(4));
}

#[test]
fn a_tick_given_twice_in_against_is_refused() {
    let (mut a, host_a) = host_app(ExtSim::new(None, None));
    step_host(&mut a, &host_a, 4);
    let rows = ok(&mut a, json!({}))["rows"].clone();
    let mut twice = rows.as_array().cloned().unwrap_or_default();
    // A second tick-3 row with other words: comparing only the first would hide it.
    let mut other = twice[3].clone();
    other["words"] = json!(["0000000000000001", "0000000000000002"]);
    twice.push(other);
    let response = call(&mut a, "sim.desync_rows", json!({ "against": twice }));
    let error = response.error.expect("a repeated tick is refused");
    assert!(error.message.contains("[bad_against]"), "{}", error.message);
    assert!(error.message.contains("tick 3 twice"), "{}", error.message);
}

#[test]
fn report_unmatched_lists_rows_the_other_side_lacks() {
    let (mut a, host_a) = host_app(ExtSim::new(None, None));
    step_host(&mut a, &host_a, 4);
    let rows = ok(&mut a, json!({}))["rows"].clone();
    let mine = rows.as_array().cloned().unwrap_or_default();
    // Their rows: our last two ticks and one tick past our window (ADR-8724).
    let ticks: Vec<u64> = mine.iter().filter_map(|row| row["tick"].as_u64()).collect();
    let newest = ticks.iter().copied().max().unwrap_or_default();
    let mut theirs: Vec<Value> = mine[mine.len() - 2..].to_vec();
    let mut later = mine[mine.len() - 1].clone();
    later["tick"] = json!(newest + 5);
    theirs.push(later);
    let verdict = ok(&mut a, json!({ "against": theirs }));
    let report = &verdict["report"];
    assert_eq!(report["compared"], json!(2), "{report}");
    assert_eq!(report["verdict"], json!("same"), "{report}");
    let unmatched = report["unmatched"].as_array().expect("unmatched");
    // Our older rows first (by tick), then their row past our window.
    assert_eq!(unmatched.len(), mine.len() - 2 + 1, "{report}");
    for (i, entry) in unmatched[..mine.len() - 2].iter().enumerate() {
        assert_eq!(
            entry,
            &json!({ "side": "left", "tick": ticks[i], "index": i, "tick_shared": false })
        );
    }
    assert_eq!(
        unmatched[mine.len() - 2],
        json!({ "side": "right", "tick": newest + 5, "index": 2, "tick_shared": false })
    );
}
