// SPDX-License-Identifier: MIT-0
//! The N08 template match over Steam (ADR-5375) on the mock Steam network: both endpoints of
//! `net_game::steam_leg` run in one process on `MockSteamNet`, a fixed number of polls, a manual
//! link clock, no sleeps and no wall time. Shows the leg's three verdicts: `PASS` for a real
//! mock session, `FAIL` for wrong content (another build, or logs edited past the bounds), and
//! `NOT RUN` when there is no setup or no log.

use net_game::link_options;
use net_game::steam_leg::{LEG_SETTLE, LEG_TICKS, SteamLegEndpoint};
use regia::net::ConditionerConfig;
use regia::net::link::{LinkClock, LinkOptions};
use regia::steam::real_run::{EndpointStatus, RealRunVerdict};
use regia::steam::real_run_net::{
    MAX_CORRECTIONS, NET_LOG_HEADER, net_leg_setup, verify_leg_files, verify_net_leg,
};
use regia::steam::{MockSteam, MockSteamNet, SteamId};

const COMMIT: &str = "c0ffee1234";
const HOST_ID: SteamId = SteamId(76_561_190_000_000_021);
const GUEST_ID: SteamId = SteamId(76_561_190_000_000_022);
/// Poll budget for one session: joins and the launch take a few dozen polls, the play
/// `LEG_TICKS + LEG_SETTLE`.
const POLLS: u32 = LEG_TICKS + LEG_SETTLE + 600;

fn opts() -> LinkOptions {
    link_options(
        LinkClock::Manual {
            start_ms: 0,
            step_us: 16_667,
        },
        ConditionerConfig::default(),
    )
}

/// The outcome of one mock session.
struct Session {
    host: String,
    guest: String,
    host_status: EndpointStatus,
    guest_status: EndpointStatus,
}

/// Runs host and guest on one mock network; the guest runs build `guest_commit`.
fn session(tag: &str, guest_commit: &str) -> Session {
    let net = MockSteamNet::new();
    let host_mock = MockSteam::shared();
    host_mock.join_net(&net, HOST_ID, "Ann");
    let guest_mock = MockSteam::shared();
    guest_mock.join_net(&net, GUEST_ID, "Bob");
    let root = regia_test_utils::unique_temp_dir(tag);

    let setup = net_leg_setup(Some("host"), None, Some("480"), Some(COMMIT)).expect("host setup");
    let mut host =
        SteamLegEndpoint::start(setup, &*host_mock, root.join("host"), opts()).expect("the host starts");
    let mut lobby = None;
    for _ in 0..60 {
        host.poll();
        if let Some(id) = host.lobby() {
            lobby = Some(id);
            break;
        }
    }
    let lobby = lobby.expect("the agent's lobby.create made a lobby");
    let setup = net_leg_setup(
        Some("guest"),
        Some(&lobby.0.to_string()),
        Some("480"),
        Some(guest_commit),
    )
    .expect("guest setup");
    let mut guest =
        SteamLegEndpoint::start(setup, &*guest_mock, root.join("guest"), opts()).expect("the guest starts");
    for _ in 0..POLLS {
        let h = host.poll();
        let g = guest.poll();
        if h != EndpointStatus::Running && g != EndpointStatus::Running {
            break;
        }
    }
    // A guest that refused the build leaves the host waiting: the budget ran out for it.
    host.time_out();
    guest.time_out();
    let out = Session {
        host: host.log().text(),
        guest: guest.log().text(),
        host_status: host.status().clone(),
        guest_status: guest.status().clone(),
    };
    drop((host, guest));
    let _ = std::fs::remove_dir_all(&root);
    out
}

/// `text` with the first `key=...` of the first line starting with `record` replaced by `key=value`.
fn edit(text: &str, record: &str, key: &str, value: &str) -> String {
    let mut done = false;
    text.lines()
        .map(|line| {
            if done || !line.starts_with(&format!("{record} ")) {
                return line.to_owned();
            }
            let fields: Vec<String> = line
                .split(' ')
                .map(|kv| match kv.split_once('=') {
                    Some((k, _)) if k == key && !done => {
                        done = true;
                        format!("{key}={value}")
                    }
                    _ => kv.to_owned(),
                })
                .collect();
            fields.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// A field of the first record named `record`.
fn field<'a>(text: &'a str, record: &str, key: &str) -> Option<&'a str> {
    text.lines()
        .find(|l| l.starts_with(&format!("{record} ")))?
        .split(' ')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

#[test]
fn a_mock_template_match_over_steam_passes() {
    let s = session("steam-leg-pass", COMMIT);
    assert_eq!(s.host_status, EndpointStatus::Complete, "host log:\n{}", s.host);
    assert_eq!(
        s.guest_status,
        EndpointStatus::Complete,
        "guest log:\n{}",
        s.guest
    );
    let verdict = verify_net_leg(Some(&s.host), Some(&s.guest));
    assert_eq!(
        verdict,
        RealRunVerdict::Pass,
        "{verdict}\nhost:\n{}\nguest:\n{}",
        s.host,
        s.guest
    );

    // The evidence the leg promises, in each endpoint's own log.
    assert!(s.host.starts_with(NET_LOG_HEADER) && s.guest.starts_with(NET_LOG_HEADER));
    assert_eq!(field(&s.host, "agent", "method"), Some("lobby.create"));
    assert_eq!(field(&s.guest, "agent", "method"), Some("lobby.join"));
    assert_eq!(
        field(&s.host, "agent", "lobby"),
        field(&s.guest, "agent", "lobby")
    );
    assert_eq!(field(&s.guest, "net_status", "state"), Some("connected"));
    // The server tag comes from the link's remote Steam peer: it is the host's account.
    assert_eq!(
        field(&s.guest, "net_status", "server"),
        field(&s.host, "endpoint", "me")
    );
    assert_eq!(field(&s.guest, "netstats", "state"), Some("connected"));
    assert_eq!(field(&s.guest, "netstats", "role"), Some("client"));
    let corrections: u64 = field(&s.guest, "netstats", "corrections")
        .and_then(|v| v.parse().ok())
        .expect("corrections counted");
    assert!(corrections <= MAX_CORRECTIONS, "{corrections}");
    assert_eq!(field(&s.guest, "netstats", "verify_failures"), Some("0"));
    assert_eq!(field(&s.host, "netstats", "role"), Some("server"));
    // The guest's bot played on the link: the server saw its datagrams.
    let datagrams: u64 = field(&s.host, "netstats", "datagrams_in")
        .and_then(|v| v.parse().ok())
        .expect("server datagrams");
    assert!(
        datagrams >= u64::from(LEG_TICKS) / 2,
        "{datagrams} datagrams for {LEG_TICKS} ticks"
    );
    // Account tags only: the raw Steam ids never reach a log.
    for text in [&s.host, &s.guest] {
        assert!(!text.contains(&HOST_ID.0.to_string()) && !text.contains(&GUEST_ID.0.to_string()));
    }

    // The same logs on disk, through the file entry point the verdict example uses.
    let dir = regia_test_utils::unique_temp_dir("steam-leg-files");
    let (h, g) = (dir.join("net-host.log"), dir.join("net-guest.log"));
    std::fs::write(&h, &s.host).expect("host log");
    std::fs::write(&g, &s.guest).expect("guest log");
    assert!(verify_leg_files(&h, &g).expect("readable").is_pass());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_guest_on_another_build_fails_the_content_check() {
    let s = session("steam-leg-content", "deadbeef99");
    assert_eq!(
        s.guest_status,
        EndpointStatus::Failed("content_mismatch"),
        "{}",
        s.guest
    );
    assert!(s.guest.contains("result status=failed code=content_mismatch"));
    assert!(!s.guest.contains("net_status"), "a refused build never connects");
    // Both logs name their own build, so the verdict reports the commit mismatch first (as the
    // echo run's verdict does); with the commit edited to agree, the guest's refusal remains.
    let verdict = verify_net_leg(Some(&s.host), Some(&s.guest));
    assert_eq!(verdict.code(), "commit_mismatch", "{verdict}");
    assert!(!verdict.is_pass());
    let same_commit = edit(&s.guest, "endpoint", "commit", COMMIT);
    let verdict = verify_net_leg(Some(&s.host), Some(&same_commit));
    assert_eq!(verdict.code(), "content_mismatch", "{verdict}");
}

#[test]
fn edited_logs_are_wrong_content() {
    let s = session("steam-leg-edits", COMMIT);
    assert!(verify_net_leg(Some(&s.host), Some(&s.guest)).is_pass());
    let fails = |host: &str, guest: &str| verify_net_leg(Some(host), Some(guest)).code();
    let g = &s.guest;
    let h = &s.host;
    assert_eq!(
        fails(h, &edit(g, "netstats", "corrections", "21")),
        "net_corrections"
    );
    assert_eq!(fails(h, &edit(g, "netstats", "resyncs", "3")), "net_resyncs");
    assert_eq!(
        fails(h, &edit(g, "netstats", "verify_failures", "1")),
        "net_verify_failed"
    );
    assert_eq!(
        fails(h, &edit(g, "net_status", "state", "connecting")),
        "net_not_connected"
    );
    assert_eq!(
        fails(h, &edit(g, "netstats", "state", "disconnected")),
        "net_disconnected"
    );
    assert_eq!(
        fails(&edit(h, "netstats", "datagrams_in", "12"), g),
        "net_too_short"
    );
    assert_eq!(
        fails(h, &edit(g, "lobby_entered", "lobby", "424242")),
        "lobby_mismatch"
    );
    assert_eq!(fails(h, &edit(g, "agent", "lobby", "424242")), "lobby_mismatch");
    assert_eq!(
        fails(h, &edit(g, "endpoint", "commit", "0ther")),
        "commit_mismatch"
    );
    assert_eq!(fails(h, &edit(g, "endpoint", "app_id", "481")), "app_id_mismatch");
    let host_me = field(h, "endpoint", "me").expect("host tag");
    assert_eq!(fails(h, &edit(g, "endpoint", "me", host_me)), "same_account");
    assert_eq!(
        fails(h, &edit(g, "net_status", "server", "acct-0000000000000001")),
        "peer_mismatch"
    );
    assert_eq!(
        fails(&edit(h, "member_joined", "member", "acct-0000000000000001"), g),
        "peer_mismatch"
    );
    assert_eq!(fails(g, h), "wrong_role");
    assert_eq!(
        fails(h, "regia_steam_net_leg version=1\nendpoint role\n"),
        "log_unreadable"
    );
    // A guest whose report is missing is incomplete, never a pass.
    let cut: String = g
        .lines()
        .filter(|l| !l.starts_with("netstats ") && !l.starts_with("result "))
        .map(|l| format!("{l}\n"))
        .collect();
    match verify_net_leg(Some(h), Some(&cut)) {
        RealRunVerdict::Incomplete { code, missing } => {
            assert_eq!(code, "incomplete_guest");
            assert!(missing.iter().any(|m| m == "netstats role=client"), "{missing:?}");
            assert!(
                missing.iter().any(|m| m == "result status=complete"),
                "{missing:?}"
            );
        }
        other => panic!("expected INCOMPLETE, got {other}"),
    }
}

#[test]
fn no_setup_is_not_run_never_pass() {
    let absent = net_leg_setup(None, None, None, None).expect_err("no role");
    assert_eq!(absent.code, "setup_absent");
    assert!(
        absent.to_string().starts_with("NOT RUN (setup_absent)"),
        "{absent}"
    );
    let guest = net_leg_setup(Some("guest"), None, None, Some(COMMIT)).expect_err("no lobby");
    assert_eq!(guest.code, "missing_lobby_id");

    let none = verify_net_leg(None, None);
    assert_eq!(none, RealRunVerdict::NotRun { code: "setup_absent" });
    assert!(!none.is_pass());
    assert_eq!(none.to_string(), "NOT RUN (setup_absent)");
    let host_only = format!("{NET_LOG_HEADER}\nendpoint role=host\n");
    assert_eq!(verify_net_leg(Some(&host_only), None).code(), "missing_guest_log");
    assert_eq!(verify_net_leg(None, Some(&host_only)).code(), "missing_host_log");

    let dir = regia_test_utils::unique_temp_dir("steam-leg-absent");
    let verdict =
        verify_leg_files(&dir.join("net-host.log"), &dir.join("net-guest.log")).expect("absent is ok");
    assert_eq!(verdict.code(), "setup_absent");
    let _ = std::fs::remove_dir_all(&dir);

    // A backend without a signed-in Steam (no lobbies, no Steam id) cannot start an endpoint.
    let setup = net_leg_setup(Some("host"), None, None, Some(COMMIT)).expect("setup");
    let offline = MockSteam::shared_unavailable();
    let err = SteamLegEndpoint::start(setup, &*offline, dir.join("agent"), opts()).expect_err("no Steam");
    assert_eq!(err, "steam_not_ready");
}

#[test]
fn a_wrong_lobby_id_fails_the_join_at_once() {
    // A guest given an id no lobby has: the refused join ends the endpoint, not the poll budget.
    let net = MockSteamNet::new();
    let guest_mock = MockSteam::shared();
    guest_mock.join_net(&net, GUEST_ID, "Bob");
    let root = regia_test_utils::unique_temp_dir("steam-leg-bad-lobby");
    let setup = net_leg_setup(Some("guest"), Some("987654"), Some("480"), Some(COMMIT)).expect("setup");
    let mut guest = SteamLegEndpoint::start(setup, &*guest_mock, root.join("guest"), opts()).expect("starts");
    let mut polls = 0;
    while guest.poll() == EndpointStatus::Running && polls < 60 {
        polls += 1;
    }
    assert_eq!(
        guest.status(),
        &EndpointStatus::Failed("lobby_join_failed"),
        "{}",
        guest.log().text()
    );
    assert!(
        guest
            .log()
            .text()
            .contains("result status=failed code=lobby_join_failed")
    );
    drop(guest);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_host_that_goes_silent_mid_match_ends_the_guest_with_net_disconnected() {
    // The host process stops (no more polls, so no more datagrams) once the guest is connected:
    // the guest's link times out (10 s of silence, 600 manual-clock steps, inside the 720-step
    // play window) and the guest ends `net_disconnected` instead of reporting a played match.
    let net = MockSteamNet::new();
    let host_mock = MockSteam::shared();
    host_mock.join_net(&net, HOST_ID, "Ann");
    let guest_mock = MockSteam::shared();
    guest_mock.join_net(&net, GUEST_ID, "Bob");
    let root = regia_test_utils::unique_temp_dir("steam-leg-silent-host");
    let setup = net_leg_setup(Some("host"), None, Some("480"), Some(COMMIT)).expect("host setup");
    let mut host =
        SteamLegEndpoint::start(setup, &*host_mock, root.join("host"), opts()).expect("host starts");
    let mut lobby = None;
    for _ in 0..60 {
        host.poll();
        lobby = host.lobby();
        if lobby.is_some() {
            break;
        }
    }
    let lobby = lobby.expect("lobby");
    let setup = net_leg_setup(
        Some("guest"),
        Some(&lobby.0.to_string()),
        Some("480"),
        Some(COMMIT),
    )
    .expect("guest setup");
    let mut guest =
        SteamLegEndpoint::start(setup, &*guest_mock, root.join("guest"), opts()).expect("guest starts");
    let mut connected = false;
    for _ in 0..600 {
        host.poll();
        guest.poll();
        if guest
            .log()
            .text()
            .contains("net_status endpoint=client state=connected")
        {
            connected = true;
            break;
        }
    }
    assert!(connected, "the guest connected first:\n{}", guest.log().text());
    // From here only the guest runs.
    let mut polls = 0;
    while guest.poll() == EndpointStatus::Running && polls < LEG_TICKS + LEG_SETTLE + 60 {
        polls += 1;
    }
    assert_eq!(
        guest.status(),
        &EndpointStatus::Failed("net_disconnected"),
        "{}",
        guest.log().text()
    );
    assert!(
        polls < LEG_TICKS + LEG_SETTLE,
        "it ended before the full play window: {polls} polls"
    );
    // The verdict never reads such a pair as a pass.
    host.time_out();
    let verdict = verify_net_leg(Some(&host.log().text()), Some(&guest.log().text()));
    assert!(!verdict.is_pass(), "{verdict}");
    assert_eq!(verdict.code(), "endpoint_failed", "{verdict}");
    drop((host, guest));
    let _ = std::fs::remove_dir_all(&root);
}
