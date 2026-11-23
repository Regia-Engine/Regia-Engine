// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 5.1 / § 5.2 open item (wire 6, ADR-10052 (3)): the ECS runtime's `ReliableUnordered`
//! messages ride their own frame (`RUMSG`, tag 9) on the unordered RPC stream (stream 7) instead of
//! the replication frame on stream 3, so a stalled replication stream never holds them back
//! (head-of-line wait). Over the in-memory `LoopbackHub` at fixed steps: its reliable-ordered
//! channel keeps send order (a held frame holds every later one), its reliable-unordered channel
//! does not, like the transport's streams.

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::message::{Channel, register_net_message};
use regia_net::testing::{FrameFate, LoopbackHub};
use regia_net::*;

/// The `RUMSG` frame tag (spec 120 § 5.1, ADR-10052 (3)).
const TAG_RUMSG: u8 = 9;

/// A server-to-everyone session notice on the unordered class.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Notice {
    n: u32,
}
impl NetMessage for Notice {
    const DIRECTION: Direction = Direction::Broadcast;
    const CHANNEL: Channel = Channel::ReliableUnordered;
}
impl BroadcastMessage for Notice {}

/// The same on the ordered class: the control.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Ordered {
    n: u32,
}
impl NetMessage for Ordered {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for Ordered {}

/// A client-to-server report on the unordered class.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Report {
    n: u32,
}
impl NetMessage for Report {
    const DIRECTION: Direction = Direction::ToServer;
    const CHANNEL: Channel = Channel::ReliableUnordered;
}
impl ToServerMessage for Report {}

/// The same on the ordered class.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct OrderedReport {
    n: u32,
}
impl NetMessage for OrderedReport {
    const DIRECTION: Direction = Direction::ToServer;
}
impl ToServerMessage for OrderedReport {}

/// A large unordered message: past one frame part, so it rides the ordered stream.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct BigNotice {
    #[replicate(max_len = 100000)]
    bytes: Vec<u8>,
}
impl NetMessage for BigNotice {
    const DIRECTION: Direction = Direction::Broadcast;
    const CHANNEL: Channel = Channel::ReliableUnordered;
}
impl BroadcastMessage for BigNotice {}

fn setup(world: &mut World) {
    register_net_message::<Notice>(world);
    register_net_message::<Ordered>(world);
    register_net_message::<Report>(world);
    register_net_message::<OrderedReport>(world);
    register_net_message::<BigNotice>(world);
}

/// What a process's handlers saw: `(hub step, kind, value)`.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct Seen(Vec<(&'static str, u32)>);

fn client_log(
    notice: Res<NetMessages<Notice>>,
    ordered: Res<NetMessages<Ordered>>,
    big: Res<NetMessages<BigNotice>>,
    mut seen: ResMut<Seen>,
) {
    for m in notice.iter() {
        seen.0.push(("notice", m.msg.n));
    }
    for m in ordered.iter() {
        seen.0.push(("ordered", m.msg.n));
    }
    for m in big.iter() {
        seen.0.push(("big", m.msg.bytes.len() as u32));
    }
}

fn server_log(
    report: Res<NetMessages<Report>>,
    ordered: Res<NetMessages<OrderedReport>>,
    mut seen: ResMut<Seen>,
) {
    for m in report.iter() {
        seen.0.push(("report", m.msg.n));
    }
    for m in ordered.iter() {
        seen.0.push(("ordered_report", m.msg.n));
    }
}

fn send<M: Send + 'static>(
    world: &mut World,
    f: impl FnOnce(&mut NetSender) -> M + Send + Sync + 'static,
) -> M {
    let mut f = Some(f);
    world
        .run_system_once(move |mut net: NetSender| (f.take().expect("runs once"))(&mut net))
        .expect("system runs")
}

fn hub(seed: u64) -> (LoopbackHub, usize) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    let a = hub.add_client();
    hub.clients[a].world.init_resource::<Seen>();
    hub.clients[a].gameplay.add_systems(client_log);
    hub.server.world.init_resource::<Seen>();
    hub.server.gameplay.add_systems(server_log);
    hub.run(3);
    (hub, a)
}

/// The step at which `kind` first shows in `world`'s log, running the hub at most `steps` more.
fn first_seen(hub: &mut LoopbackHub, client: Option<usize>, kind: &str, steps: u64) -> Option<u64> {
    let start = hub.step_count();
    for _ in 0..steps {
        hub.step();
        let world = match client {
            Some(i) => &hub.clients[i].world,
            None => &hub.server.world,
        };
        if world.resource::<Seen>().0.iter().any(|(k, _)| *k == kind) {
            return Some(hub.step_count() - start);
        }
    }
    None
}

/// Holds every reliable-ordered frame of the next `stall` steps until the stall ends (a lost
/// replication-stream packet whose repair takes `stall` steps: every later frame of that stream
/// waits behind it).
fn stall_ordered(stall: u64, at: u64) -> regia_net::testing::FrameFilter {
    let due = at + stall;
    Box::new(move |d, step| {
        if d.channel == Channel::ReliableOrdered && step < due {
            FrameFate::Delay(due - step)
        } else {
            FrameFate::Deliver
        }
    })
}

/// Server to client: a `ReliableUnordered` message sent while the replication stream is stalled
/// arrives within the link's delay; the ordered control sent on the same tick waits out the stall.
/// Base (wire 5): both rode the stream-3 `TICK` frame and both waited the 30 steps.
#[test]
fn an_unordered_message_does_not_wait_for_a_stalled_replication_stream() {
    let (mut hub, a) = hub(101);
    let stall = 30;
    hub.down_filter = Some(stall_ordered(stall, hub.step_count()));
    send(&mut hub.server.world, |n| {
        n.broadcast_session(Notice { n: 7 })?;
        n.broadcast_session(Ordered { n: 8 })
    })
    .expect("sent");
    let notice = first_seen(&mut hub, Some(a), "notice", 60).expect("the notice arrives");
    assert!(
        notice <= 3,
        "the unordered notice waited {notice} steps behind the stall"
    );
    let ordered = first_seen(&mut hub, Some(a), "ordered", 60).expect("the ordered control arrives");
    assert!(
        notice + ordered >= stall,
        "the ordered control rides the stalled stream (arrived after {} steps)",
        notice + ordered
    );
    hub.down_filter = None;
    hub.run(10);
    let seen = &hub.clients[a].world.resource::<Seen>().0;
    assert_eq!(
        seen.iter().filter(|(k, _)| *k == "notice").count(),
        1,
        "exactly once: {seen:?}"
    );
}

/// The frame that carries it: tag 9 on the reliable-unordered class, never inside a `TICK` frame.
#[test]
fn the_unordered_message_rides_its_own_frame_on_the_unordered_class() {
    let (mut hub, _a) = hub(102);
    let frames: std::sync::Arc<std::sync::Mutex<Vec<(Channel, u8)>>> = Default::default();
    let log = frames.clone();
    hub.down_filter = Some(Box::new(move |d, _| {
        if let Ok(mut l) = log.lock() {
            l.push((d.channel, d.bytes.first().copied().unwrap_or(0)));
        }
        FrameFate::Deliver
    }));
    send(&mut hub.server.world, |n| n.broadcast_session(Notice { n: 1 })).expect("sent");
    hub.run(2);
    let frames = frames.lock().expect("log").clone();
    assert!(
        frames.contains(&(Channel::ReliableUnordered, TAG_RUMSG)),
        "a RUMSG frame on the unordered class: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .all(|(c, tag)| (*c == Channel::ReliableUnordered) == (*tag == TAG_RUMSG)),
        "only RUMSG frames on the unordered class: {frames:?}"
    );
}

/// Client to server: the same for a client's `ReliableUnordered` message while its command
/// stream (the client's ordered `CMSG` frames) is stalled.
#[test]
fn a_client_unordered_message_does_not_wait_for_its_stalled_command_stream() {
    let (mut hub, a) = hub(103);
    let stall = 30;
    hub.up_filter = Some(stall_ordered(stall, hub.step_count()));
    send(&mut hub.clients[a].world, |n| {
        n.send_session_to_server(Report { n: 3 })?;
        n.send_session_to_server(OrderedReport { n: 4 })
    })
    .expect("sent");
    let report = first_seen(&mut hub, None, "report", 60).expect("the report arrives");
    assert!(
        report <= 3,
        "the unordered report waited {report} steps behind the stall"
    );
    let ordered = first_seen(&mut hub, None, "ordered_report", 60).expect("the control arrives");
    assert!(
        report + ordered >= stall,
        "the ordered control rides the stalled stream"
    );
    hub.up_filter = None;
    hub.run(10);
    let seen = &hub.server.world.resource::<Seen>().0;
    assert_eq!(seen.iter().filter(|(k, _)| *k == "report").count(), 1, "{seen:?}");
}

/// An unordered message too large for one frame part keeps the ordered frame (exactly once still
/// holds; only the unordered delivery is given up for it), and small ones of the same tick still
/// skip the stall.
#[test]
fn an_unordered_message_past_one_part_falls_back_to_the_ordered_frame() {
    let (mut hub, a) = hub(104);
    let stall = 30;
    hub.down_filter = Some(stall_ordered(stall, hub.step_count()));
    send(&mut hub.server.world, |n| {
        n.broadcast_session(BigNotice {
            bytes: vec![5; 40_000],
        })?;
        n.broadcast_session(Notice { n: 9 })
    })
    .expect("sent");
    let notice = first_seen(&mut hub, Some(a), "notice", 60).expect("the small notice arrives");
    assert!(notice <= 3, "the small notice waited {notice} steps");
    hub.down_filter = None;
    hub.run(40);
    let seen = &hub.clients[a].world.resource::<Seen>().0;
    assert_eq!(
        seen.iter().filter(|(k, _)| *k == "big").collect::<Vec<_>>(),
        vec![&("big", 40_000)],
        "the large one arrives once: {seen:?}"
    );
}
