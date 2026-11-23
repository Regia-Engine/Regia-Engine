// SPDX-License-Identifier: MPL-2.0
//! ADR-2665 (4): kick and ban for peers on a replication game's own port (`HostedLink`). The
//! peers join an app's `NetLink` over the in-memory `LoopbackNet` (no sockets, fixed steps);
//! bans go to the hosted session's ban list, persist in its `bans.ron`, and the link refuses the
//! banned address until the entry is removed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::model::NetMode;
use regia_net::server_host::{
    HostedLink, LinkBanMirror, ban_link_peer, enforce_link_bans, kick_link_peer, link_hosts_game_port,
    unban_link_entry,
};
use regia_net::session::bans::{BanKind, BanList, BanStore};
use regia_net::session::lifecycle::HostOptions;
use regia_net::socket::LoopbackNet;
use regia_net::{NetGame, NetPlugin, NetSession};

struct Wire;
impl NetGame for Wire {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.link_bans";
    const PROTOCOL_VERSION: u32 = 1;
}

fn manual() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 16_000,
        },
        ..LinkOptions::default()
    }
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("address")
}

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-net-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn link_app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

/// The game's server app as the host leaves it: a `HostedLink`, the link hosting the game port,
/// and a local hosted session whose ban list is `bans.ron` in `dir`.
fn server(net: &LoopbackNet, dir: &std::path::Path) -> App {
    let mut app = link_app(NetMode::DedicatedServer);
    app.insert_resource(HostedLink::new::<Wire>(manual()));
    app.world_mut()
        .resource_mut::<NetLink>()
        .host::<Wire>(
            Box::new(net.bind(addr("10.0.0.1:7000")).expect("bind")),
            &manual(),
        )
        .expect("host");
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    session
        .host_local(
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host the session");
    *session.server_mut().expect("server").bans_mut() = BanStore::open(&dir.join("bans.ron"));
    app.insert_resource(session);
    app
}

fn client(net: &LoopbackNet, from: &str) -> App {
    let mut app = link_app(NetMode::Client);
    app.world_mut()
        .resource_mut::<NetLink>()
        .connect::<Wire>(
            Box::new(net.bind(addr(from)).expect("bind")),
            addr("10.0.0.1:7000"),
            &manual(),
        )
        .expect("connect");
    app
}

fn pump(server: &mut App, clients: &mut [&mut App], steps: usize) {
    for _ in 0..steps {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
    }
}

fn peers(server: &App) -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = server
        .world()
        .resource::<NetLink>()
        .peer_addrs()
        .into_iter()
        .map(|(p, a)| (p.0, a.to_string()))
        .collect();
    v.sort();
    v
}

fn peer_of(server: &App, from: &str) -> String {
    peers(server)
        .into_iter()
        .find(|(_, a)| a == from)
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| panic!("{from} is not a peer: {:?}", peers(server)))
}

/// The reason the client's link ended with, if it did.
fn ended(app: &mut App) -> Option<String> {
    app.world_mut()
        .resource_mut::<NetLink>()
        .take_events()
        .into_iter()
        .find_map(|e| match e {
            LinkEvent::Ended { reason } => Some(reason),
            _ => None,
        })
}

#[test]
fn link_peers_are_kicked_banned_persisted_and_unbanned() {
    let dir = temp_dir("link-bans");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    assert!(link_hosts_game_port(sv.world()));
    let mut a = client(&net, "10.0.0.2:5000");
    let mut b = client(&net, "10.0.0.3:5000");
    pump(&mut sv, &mut [&mut a, &mut b], 30);
    assert_eq!(peers(&sv).len(), 2, "{:?}", peers(&sv));

    // Kick: the peer leaves and hears why; nothing is banned.
    let pa = peer_of(&sv, "10.0.0.2:5000");
    kick_link_peer(sv.world_mut(), &pa, "afk").expect("kick");
    let mut why = None;
    for _ in 0..20 {
        pump(&mut sv, &mut [&mut a, &mut b], 1);
        why = why.or_else(|| ended(&mut a));
    }
    assert_eq!(why.as_deref(), Some("kicked"));
    assert_eq!(peers(&sv).len(), 1);
    assert_eq!(
        kick_link_peer(sv.world_mut(), "999", "x").map_err(|e| e.code),
        Err("player_unknown")
    );

    // Ban (as `peer.local`, as server.status writes ids): an address entry in the session's list,
    // saved to bans.ron, and the peer removed with `banned`.
    let pb = peer_of(&sv, "10.0.0.3:5000");
    let id = ban_link_peer(sv.world_mut(), &format!("{pb}.0"), Some(600), "spam").expect("ban");
    let mut why = None;
    for _ in 0..20 {
        pump(&mut sv, &mut [&mut b], 1);
        why = why.or_else(|| ended(&mut b));
    }
    assert_eq!(why.as_deref(), Some("banned"));
    assert!(peers(&sv).is_empty());
    let saved = BanList::from_ron(&std::fs::read_to_string(dir.join("bans.ron")).expect("bans.ron written"))
        .expect("parses");
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved.entries[0].id, id);
    assert_eq!(saved.entries[0].kind, BanKind::Address);
    assert_eq!(saved.entries[0].value, "10.0.0.3");
    assert_eq!(saved.entries[0].reason, "spam");
    assert!(saved.entries[0].until.is_some());

    // The banned address cannot join again; another address can.
    let mut b2 = client(&net, "10.0.0.3:5001");
    let mut a2 = client(&net, "10.0.0.2:5001");
    let mut refused = None;
    for _ in 0..40 {
        pump(&mut sv, &mut [&mut b2, &mut a2], 1);
        refused = refused.or_else(|| ended(&mut b2));
    }
    assert!(
        refused.as_deref().is_some_and(|r| r.contains("banned")),
        "{refused:?}"
    );
    assert_eq!(
        peers(&sv).iter().map(|(_, a)| a.as_str()).collect::<Vec<_>>(),
        ["10.0.0.2:5001"]
    );

    // Unban: the entry leaves bans.ron and the address may join again.
    unban_link_entry(sv.world_mut(), id.0).expect("unban");
    assert_eq!(
        unban_link_entry(sv.world_mut(), id.0).map_err(|e| e.code),
        Err("ban_entry_unknown")
    );
    let saved =
        BanList::from_ron(&std::fs::read_to_string(dir.join("bans.ron")).expect("bans.ron")).expect("parses");
    assert!(saved.entries.is_empty());
    assert!(sv.world().resource::<LinkBanMirror>().refused.is_empty());
    let mut b3 = client(&net, "10.0.0.3:5002");
    pump(&mut sv, &mut [&mut b3, &mut a2], 40);
    assert_eq!(ended(&mut b3), None);
    assert_eq!(peers(&sv).len(), 2, "{:?}", peers(&sv));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_range_entry_in_the_list_removes_matching_link_peers_each_tick() {
    let dir = temp_dir("link-ban-range");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut a = client(&net, "10.0.0.2:5000");
    let mut c = client(&net, "192.168.7.9:5000");
    pump(&mut sv, &mut [&mut a, &mut c], 30);
    assert_eq!(peers(&sv).len(), 2);
    assert_eq!(enforce_link_bans(sv.world_mut()), 0, "no entry: nobody removed");

    // A range edited into the list (bans.ron + reload, or another tool) matches peer a only.
    let id = sv
        .world_mut()
        .resource_mut::<NetSession>()
        .ban(BanKind::Address, "10.0.0.0/24", None, "range")
        .expect("range ban");
    assert_eq!(enforce_link_bans(sv.world_mut()), 1);
    let mut why = None;
    for _ in 0..20 {
        pump(&mut sv, &mut [&mut a, &mut c], 1);
        why = why.or_else(|| ended(&mut a));
    }
    assert_eq!(why.as_deref(), Some("banned"));
    assert_eq!(peers(&sv).len(), 1);
    assert_eq!(enforce_link_bans(sv.world_mut()), 0, "applied once");
    // The removed address stays refused at the handshake until the entry goes.
    let mirror = sv.world().resource::<LinkBanMirror>().refused.clone();
    assert_eq!(mirror.len(), 1);
    assert_eq!(mirror[0].1, id);
    unban_link_entry(sv.world_mut(), id.0).expect("unban");
    let mut a2 = client(&net, "10.0.0.2:5001");
    pump(&mut sv, &mut [&mut a2, &mut c], 40);
    assert_eq!(peers(&sv).len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-4936: a `bans.ron` entry spelled `addr/128` refuses exactly that address on the game port,
/// not its whole IPv6 /64; the other addresses of the block stay open, and lifting the entry
/// reopens the address.
#[test]
fn a_128_entry_refuses_one_address_of_a_block() {
    let dir = temp_dir("link-ban-128");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut a = client(&net, "[2001:db8:1:2::dead:1]:5000");
    let mut b = client(&net, "[2001:db8:1:2::dead:2]:5000");
    pump(&mut sv, &mut [&mut a, &mut b], 30);
    assert_eq!(peers(&sv).len(), 2);

    // One address of the /64, the way an operator edits the list.
    let id = sv
        .world_mut()
        .resource_mut::<NetSession>()
        .ban(BanKind::Address, "2001:db8:1:2::dead:1/128", None, "one bot")
        .expect("ban");
    assert_eq!(enforce_link_bans(sv.world_mut()), 1);
    let mut why = None;
    for _ in 0..20 {
        pump(&mut sv, &mut [&mut a, &mut b], 1);
        why = why.or_else(|| ended(&mut a));
    }
    assert_eq!(why.as_deref(), Some("banned"));
    assert_eq!(peers(&sv).len(), 1, "the neighbor in the /64 stays");

    // The neighbor rejoins from a new port; the refused address does not.
    let mut b2 = client(&net, "[2001:db8:1:2::dead:2]:5001");
    let mut a2 = client(&net, "[2001:db8:1:2::dead:1]:5001");
    let mut refused = None;
    for _ in 0..40 {
        pump(&mut sv, &mut [&mut b2, &mut a2], 1);
        refused = refused.or_else(|| ended(&mut a2));
    }
    assert!(
        refused.as_deref().is_some_and(|r| r.contains("banned")),
        "{refused:?}"
    );
    assert_eq!(peers(&sv).len(), 2);

    // Unban lifts the exact refusal: the address joins again.
    unban_link_entry(sv.world_mut(), id.0).expect("unban");
    let mut a3 = client(&net, "[2001:db8:1:2::dead:1]:5002");
    pump(&mut sv, &mut [&mut a3], 40);
    assert_eq!(ended(&mut a3), None);
    assert_eq!(peers(&sv).len(), 3, "{:?}", peers(&sv));
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-4936: an admin ban of an IPv6 game-port peer persists as an explicit `/64` block ban, so
/// the session port and the game port read the entry the same way.
#[test]
fn an_admin_ban_of_an_ipv6_peer_writes_a_block_ban() {
    let dir = temp_dir("link-ban-v6-admin");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut a = client(&net, "[2001:db8:1:2::dead:1]:5000");
    pump(&mut sv, &mut [&mut a], 30);
    let pa = peer_of(&sv, "[2001:db8:1:2::dead:1]:5000");
    let id = ban_link_peer(sv.world_mut(), &format!("{pa}.0"), None, "spam").expect("ban");
    let saved = BanList::from_ron(&std::fs::read_to_string(dir.join("bans.ron")).expect("bans.ron written"))
        .expect("parses");
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved.entries[0].id, id);
    assert_eq!(saved.entries[0].value, "2001:db8:1:2::dead:1/64");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_saved_ban_outlives_the_host_process() {
    let dir = temp_dir("link-ban-restart");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut b = client(&net, "10.0.0.3:5000");
    pump(&mut sv, &mut [&mut b], 30);
    let pb = peer_of(&sv, "10.0.0.3:5000");
    let id = ban_link_peer(sv.world_mut(), &pb, None, "spam").expect("ban");
    drop(sv);

    // A new process: the link's refusals are gone, and the list comes back from bans.ron.
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    assert!(
        !sv.world().contains_resource::<LinkBanMirror>(),
        "a fresh link refuses nobody yet"
    );
    let mut b2 = client(&net, "10.0.0.3:5000");
    let mut a = client(&net, "10.0.0.2:5000");
    pump(&mut sv, &mut [&mut b2, &mut a], 30);
    assert_eq!(peers(&sv).len(), 2, "{:?}", peers(&sv));

    // The host's next tick removes the banned address only, and refuses it from then on.
    assert_eq!(enforce_link_bans(sv.world_mut()), 1);
    let mut why = None;
    for _ in 0..20 {
        pump(&mut sv, &mut [&mut b2, &mut a], 1);
        why = why.or_else(|| ended(&mut b2));
    }
    assert_eq!(why.as_deref(), Some("banned"));
    assert_eq!(
        peers(&sv)
            .iter()
            .map(|(_, addr)| addr.as_str())
            .collect::<Vec<_>>(),
        ["10.0.0.2:5000"]
    );
    let mut b3 = client(&net, "10.0.0.3:5001");
    let mut refused = None;
    for _ in 0..40 {
        pump(&mut sv, &mut [&mut b3, &mut a], 1);
        refused = refused.or_else(|| ended(&mut b3));
    }
    assert!(
        refused.as_deref().is_some_and(|r| r.contains("banned")),
        "{refused:?}"
    );

    // The entry from the earlier process is the one an unban removes.
    unban_link_entry(sv.world_mut(), id.0).expect("unban");
    let mut b4 = client(&net, "10.0.0.3:5002");
    pump(&mut sv, &mut [&mut b4, &mut a], 40);
    assert_eq!(ended(&mut b4), None);
    assert_eq!(peers(&sv).len(), 2, "{:?}", peers(&sv));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether a new client from `from` is admitted, or ended with a reason.
fn try_join(net: &LoopbackNet, sv: &mut App, from: &str) -> Result<(), String> {
    let mut c = client(net, from);
    let mut why = None;
    for _ in 0..40 {
        pump(sv, &mut [&mut c], 1);
        why = why.or_else(|| ended(&mut c));
    }
    match why {
        Some(reason) => Err(reason),
        None => {
            assert!(peers(sv).iter().any(|(_, a)| a == from), "{:?}", peers(sv));
            Ok(())
        }
    }
}

#[test]
fn an_expired_entry_does_not_keep_an_address_refused_after_the_unban() {
    let dir = temp_dir("link-ban-expired");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut b = client(&net, "10.0.0.3:5000");
    pump(&mut sv, &mut [&mut b], 30);

    // An earlier temporary ban of this address, expired by now: the mirror still has its row.
    let old = sv
        .world_mut()
        .resource_mut::<NetSession>()
        .ban(BanKind::Address, "10.0.0.3", Some(1), "long ago")
        .expect("old ban");
    let ip = "10.0.0.3".parse().expect("ip");
    sv.world_mut()
        .get_resource_or_insert_with(LinkBanMirror::default)
        .refused
        .push((ip, old));

    // The address comes back and is banned for good; the unban lifts that, and the expired row
    // has no say in it.
    let pb = peer_of(&sv, "10.0.0.3:5000");
    let id = ban_link_peer(sv.world_mut(), &pb, None, "spam").expect("ban");
    assert!(try_join(&net, &mut sv, "10.0.0.3:5001").is_err(), "banned");
    unban_link_entry(sv.world_mut(), id.0).expect("unban");
    assert!(sv.world().resource::<LinkBanMirror>().refused.is_empty());
    assert_eq!(try_join(&net, &mut sv, "10.0.0.3:5002"), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_entry_deleted_from_the_file_lifts_its_refusal_at_the_reload() {
    let dir = temp_dir("link-ban-reload");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut b = client(&net, "10.0.0.3:5000");
    pump(&mut sv, &mut [&mut b], 30);
    let pb = peer_of(&sv, "10.0.0.3:5000");
    let id = ban_link_peer(sv.world_mut(), &pb, None, "spam").expect("ban");
    assert!(try_join(&net, &mut sv, "10.0.0.3:5001").is_err(), "banned");
    let _ = enforce_link_bans(sv.world_mut());

    // The operator deletes the row from bans.ron and reloads.
    let path = dir.join("bans.ron");
    let mut list = BanList::from_ron(&std::fs::read_to_string(&path).expect("bans.ron")).expect("parses");
    list.entries.clear();
    std::fs::write(&path, list.to_ron().expect("ron")).expect("write");
    sv.world_mut()
        .resource_mut::<NetSession>()
        .server_mut()
        .expect("server")
        .bans_mut()
        .reload()
        .expect("reload");

    // The next host tick sees it: nothing left to unban, and the address may join.
    let _ = enforce_link_bans(sv.world_mut());
    assert!(sv.world().resource::<LinkBanMirror>().refused.is_empty());
    assert_eq!(
        unban_link_entry(sv.world_mut(), id.0).map_err(|e| e.code),
        Err("ban_entry_unknown")
    );
    assert_eq!(try_join(&net, &mut sv, "10.0.0.3:5002"), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refusal_another_entry_covers_moves_to_it_at_the_unban() {
    let dir = temp_dir("link-ban-covered");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut b = client(&net, "10.0.0.3:5000");
    pump(&mut sv, &mut [&mut b], 30);
    let pb = peer_of(&sv, "10.0.0.3:5000");
    let exact = ban_link_peer(sv.world_mut(), &pb, None, "spam").expect("ban");
    let range = sv
        .world_mut()
        .resource_mut::<NetSession>()
        .ban(BanKind::Address, "10.0.0.0/24", None, "range")
        .expect("range");

    // The exact entry goes, the range still bans the address.
    unban_link_entry(sv.world_mut(), exact.0).expect("unban");
    let ip = "10.0.0.3".parse().expect("ip");
    assert_eq!(sv.world().resource::<LinkBanMirror>().refused, [(ip, range)]);
    assert!(
        try_join(&net, &mut sv, "10.0.0.3:5001").is_err(),
        "the range covers it"
    );

    unban_link_entry(sv.world_mut(), range.0).expect("unban");
    assert_eq!(try_join(&net, &mut sv, "10.0.0.3:5002"), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// corr-low (corr-net LOW): a block refusal whose entry goes while a `/128` entry still bans the
/// one address moves to that entry as an exact refusal. The move used to add the exact refusal
/// and keep the block one, so the whole /64 stayed refused although only one address of it was
/// still banned.
#[test]
fn a_block_refusal_that_moves_to_a_128_entry_reopens_the_rest_of_the_block() {
    let dir = temp_dir("link-ban-block-to-128");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let mut a = client(&net, "[2001:db8:1:2::dead:1]:5000");
    pump(&mut sv, &mut [&mut a], 30);
    let pa = peer_of(&sv, "[2001:db8:1:2::dead:1]:5000");
    // An admin ban of an IPv6 peer is a /64 block ban.
    let block = ban_link_peer(sv.world_mut(), &pa, None, "spam").expect("ban");
    assert!(
        try_join(&net, &mut sv, "[2001:db8:1:2::dead:2]:5000").is_err(),
        "the block is refused"
    );
    // The operator narrows it to the one address, then lifts the block entry.
    let exact = sv
        .world_mut()
        .resource_mut::<NetSession>()
        .ban(BanKind::Address, "2001:db8:1:2::dead:1/128", None, "one bot")
        .expect("ban");
    unban_link_entry(sv.world_mut(), block.0).expect("unban");
    let ip = "2001:db8:1:2::dead:1".parse().expect("ip");
    assert_eq!(sv.world().resource::<LinkBanMirror>().refused, [(ip, exact)]);
    assert_eq!(
        try_join(&net, &mut sv, "[2001:db8:1:2::dead:2]:5001"),
        Ok(()),
        "a neighbor in the /64 no longer matches any entry"
    );
    assert!(
        try_join(&net, &mut sv, "[2001:db8:1:2::dead:1]:5001").is_err(),
        "the /128 entry still refuses its address"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_hosted_link_the_link_functions_do_not_apply() {
    let net = LoopbackNet::new();
    let mut app = link_app(NetMode::DedicatedServer);
    app.world_mut()
        .resource_mut::<NetLink>()
        .host::<Wire>(
            Box::new(net.bind(addr("10.0.0.1:7001")).expect("bind")),
            &manual(),
        )
        .expect("host");
    assert!(
        !link_hosts_game_port(app.world()),
        "a link without HostedLink is the game's own business"
    );
    assert_eq!(enforce_link_bans(app.world_mut()), 0);
    assert_eq!(
        ban_link_peer(app.world_mut(), "1", None, "x").map_err(|e| e.code),
        Err("player_unknown")
    );
}

/// fixrev6 (review sprint-rev-claude-13-k finding 5, ADR-5973): the ban list's `until` is on the
/// session clock (`NetSession::now_unix_s`), the clock the session's own join check reads, so a
/// game-port ban issued for 60 s ends after 60 s of that clock wherever the system clock stands.
/// Here the session clock runs an hour ahead of the wall (an NTP step, a replay base): the link
/// refuses the address at 59 s and lets it in at 61 s, although the wall barely moved.
#[test]
fn a_game_port_ban_ends_on_the_session_clock() {
    let dir = temp_dir("link-ban-clock");
    let net = LoopbackNet::new();
    let mut sv = server(&net, &dir);
    let base = regia_net::session::unix_now_s() + 3600;
    sv.world_mut().resource_mut::<NetSession>().set_unix_base_s(base);
    let mut b = client(&net, "10.0.0.3:5000");
    pump(&mut sv, &mut [&mut b], 30);
    let pb = peer_of(&sv, "10.0.0.3:5000");
    let banned_at = sv.world().resource::<NetSession>().now_unix_s();
    let id = ban_link_peer(sv.world_mut(), &pb, Some(60), "spam").expect("ban");

    sv.world_mut()
        .resource_mut::<NetSession>()
        .advance_manual_clock(59_000);
    let _ = enforce_link_bans(sv.world_mut());
    assert!(
        try_join(&net, &mut sv, "10.0.0.3:5001").is_err_and(|r| r.contains("banned")),
        "59 s of the session clock: still banned"
    );

    sv.world_mut()
        .resource_mut::<NetSession>()
        .advance_manual_clock(2_000);
    let _ = enforce_link_bans(sv.world_mut());
    assert_eq!(
        try_join(&net, &mut sv, "10.0.0.3:5002"),
        Ok(()),
        "61 s of the session clock: the 60 s ban is over"
    );
    assert!(
        sv.world().resource::<LinkBanMirror>().refused.is_empty(),
        "the expired entry's refusal is lifted"
    );
    let saved =
        BanList::from_ron(&std::fs::read_to_string(dir.join("bans.ron")).expect("bans.ron")).expect("parses");
    assert_eq!(
        saved.entries.iter().find(|e| e.id == id).and_then(|e| e.until),
        Some(banned_at + 60),
        "bans.ron holds the expiry on the session clock"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
