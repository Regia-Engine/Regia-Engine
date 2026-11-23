// SPDX-License-Identifier: MPL-2.0
//! The two-account acceptance harness (spec 24 § 4, ADR-4896) without Steam: the host and guest
//! scripts run against two users on one `MockSteamNet` in fixed poll steps, and the verdict over
//! their two logs is checked for every way a session can fail to prove anything: absent setup,
//! a missing host or guest, wrong content, and an incomplete result. No SDK, no wall time.

use std::sync::Arc;

use regia_net::DatagramSocket;
use regia_steam::real_run::{
    EndpointStatus, RealRunEndpoint, RealRunNotRun, RealRunSetup, RealRunVerdict, account_tag,
    verify_real_run, verify_real_run_files,
};
use regia_steam::{AppId, MockSteam, MockSteamNet, SteamBackend, SteamId, SteamSocket, steam_peer_addr};

const HOST: SteamId = SteamId(76_561_190_000_000_101);
const GUEST: SteamId = SteamId(76_561_190_000_000_102);
const COMMIT: &str = "a181e5bab";
/// The poll budget of one mock run; the full script needs far fewer.
const MAX_POLLS: u32 = 400;

fn user(net: &MockSteamNet, id: SteamId, name: &str) -> Arc<MockSteam> {
    let mock = MockSteam::shared();
    mock.join_net(net, id, name);
    mock.set_app_id(Some(AppId(480)));
    mock
}

fn setup(role: &str, lobby: Option<&str>, app: Option<&str>, commit: &str) -> RealRunSetup {
    RealRunSetup::from_values(Some(role), lobby, app, Some(commit)).expect("valid setup")
}

/// A mock session: the host creates its lobby, then the guest starts with the printed id.
struct Session {
    net: MockSteamNet,
    host_backend: Arc<MockSteam>,
    guest_backend: Arc<MockSteam>,
    host: RealRunEndpoint,
    guest: Option<RealRunEndpoint>,
}

impl Session {
    fn new(guest_app: Option<&str>, guest_commit: &str) -> Self {
        let net = MockSteamNet::new();
        let host_backend = user(&net, HOST, "Ann");
        let guest_backend = user(&net, GUEST, "Bob");
        let mut host = RealRunEndpoint::start(setup("host", None, None, COMMIT), host_backend.as_ref())
            .expect("host starts");
        let mut lobby = None;
        for _ in 0..MAX_POLLS {
            host.poll(host_backend.as_ref());
            lobby = host.lobby();
            if lobby.is_some() {
                break;
            }
        }
        let lobby = lobby.expect("the host prints a lobby id").0.to_string();
        let guest = RealRunEndpoint::start(
            setup("guest", Some(&lobby), guest_app, guest_commit),
            guest_backend.as_ref(),
        )
        .expect("guest starts");
        Self {
            net,
            host_backend,
            guest_backend,
            host,
            guest: Some(guest),
        }
    }

    /// Polls both endpoints in lockstep until neither is running; returns the polls used.
    fn run(&mut self) -> u32 {
        for n in 0..MAX_POLLS {
            let _ = self.host_backend.pump();
            let _ = self.guest_backend.pump();
            let h = self.host.poll(self.host_backend.as_ref());
            let g = self
                .guest
                .as_mut()
                .map_or(EndpointStatus::Complete, |g| g.poll(self.guest_backend.as_ref()));
            if h != EndpointStatus::Running && g != EndpointStatus::Running {
                return n;
            }
        }
        MAX_POLLS
    }

    fn guest(&self) -> &RealRunEndpoint {
        self.guest.as_ref().expect("guest")
    }

    fn logs(&self) -> (String, String) {
        (self.host.log().text(), self.guest().log().text())
    }
}

fn full_run() -> (String, String) {
    let mut s = Session::new(None, COMMIT);
    let polls = s.run();
    assert!(polls < MAX_POLLS, "the run finishes in the poll budget");
    assert_eq!(
        s.host.status(),
        &EndpointStatus::Complete,
        "{}",
        s.host.log().text()
    );
    assert_eq!(
        s.guest().status(),
        &EndpointStatus::Complete,
        "{}",
        s.guest().log().text()
    );
    s.logs()
}

#[test]
fn a_full_mock_session_passes_with_reconnect_and_cloud() {
    let (host, guest) = full_run();
    let verdict = verify_real_run(Some(&host), Some(&guest));
    assert_eq!(verdict, RealRunVerdict::Pass, "host:\n{host}\nguest:\n{guest}");
    assert!(verdict.is_pass());
    assert_eq!(verdict.to_string(), "PASS");
    // The logs carry account tags and the app id, never the raw Steam ids.
    for log in [&host, &guest] {
        assert!(
            !log.contains(&HOST.0.to_string()) && !log.contains(&GUEST.0.to_string()),
            "{log}"
        );
        assert!(
            log.contains("app_id=480") && log.contains(&format!("commit={COMMIT}")),
            "{log}"
        );
    }
    assert!(host.contains(&format!("me={}", account_tag(HOST))), "{host}");
    assert!(
        guest.contains(&format!("server={}", account_tag(HOST))),
        "{guest}"
    );
    // Reconnect: the host saw the guest leave and join again, and a second echo after it.
    assert!(host.contains("member_left") && host.contains("join=2"), "{host}");
    assert!(
        guest.contains("rejoin") && guest.contains("payload=pong.2."),
        "{guest}"
    );
    assert!(host.ends_with("result status=complete\n") && guest.ends_with("result status=complete\n"));
}

#[test]
fn absent_setup_is_not_run_never_pass() {
    let not_run = |role: Option<&str>, lobby: Option<&str>, app: Option<&str>, commit: Option<&str>| {
        RealRunSetup::from_values(role, lobby, app, commit).expect_err("not a valid setup")
    };
    let cases: [(RealRunNotRun, &str); 8] = [
        (not_run(None, None, None, Some(COMMIT)), "setup_absent"),
        (not_run(Some("  "), None, None, Some(COMMIT)), "setup_absent"),
        (not_run(Some("hots"), None, None, Some(COMMIT)), "bad_role"),
        (
            not_run(Some("guest"), None, None, Some(COMMIT)),
            "missing_lobby_id",
        ),
        (
            not_run(Some("guest"), Some("lobby-7"), None, Some(COMMIT)),
            "bad_lobby_id",
        ),
        (not_run(Some("host"), None, None, None), "missing_commit"),
        (
            not_run(Some("host"), None, None, Some("abc def")),
            "missing_commit",
        ),
        (
            not_run(Some("host"), None, Some("spacewar"), Some(COMMIT)),
            "bad_app_id",
        ),
    ];
    for (err, code) in cases {
        assert_eq!(err.code, code, "{err}");
        assert!(err.to_string().starts_with("NOT RUN ("), "{err}");
    }
    let verdict = verify_real_run(None, None);
    assert_eq!(verdict, RealRunVerdict::NotRun { code: "setup_absent" });
    assert!(!verdict.is_pass());
    assert_eq!(verdict.to_string(), "NOT RUN (setup_absent)");
    // Defaults: app 480; the host needs no lobby id.
    let host = setup("host", Some("ignored"), None, COMMIT);
    assert_eq!((host.app_id, host.lobby), (AppId(480), None));
}

#[test]
fn a_missing_host_or_guest_log_is_not_run() {
    let (host, guest) = full_run();
    for (verdict, code) in [
        (verify_real_run(None, Some(&guest)), "missing_host_log"),
        (verify_real_run(Some(&host), None), "missing_guest_log"),
    ] {
        assert_eq!(verdict, RealRunVerdict::NotRun { code });
        assert!(!verdict.is_pass());
    }
    // From files: one written atomically, the other never written.
    let dir = regia_test_utils::unique_temp_dir("steam_real_run");
    let (host_path, guest_path) = (dir.join("host.log"), dir.join("guest.log"));
    let mut s = Session::new(None, COMMIT);
    s.run();
    s.host.log().write_to(&host_path).expect("write host log");
    assert!(
        !dir.join("host.log.tmp").exists(),
        "the temporary file was renamed"
    );
    let verdict = verify_real_run_files(&host_path, &guest_path).expect("read");
    assert_eq!(
        verdict,
        RealRunVerdict::NotRun {
            code: "missing_guest_log"
        }
    );
    s.guest().log().write_to(&guest_path).expect("write guest log");
    assert_eq!(
        verify_real_run_files(&host_path, &guest_path).expect("read"),
        RealRunVerdict::Pass
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_guest_that_never_joins_times_the_host_out() {
    let net = MockSteamNet::new();
    let backend = user(&net, HOST, "Ann");
    let mut host =
        RealRunEndpoint::start(setup("host", None, None, COMMIT), backend.as_ref()).expect("host starts");
    for _ in 0..MAX_POLLS {
        assert_eq!(host.poll(backend.as_ref()), EndpointStatus::Running);
    }
    host.time_out();
    assert_eq!(host.status(), &EndpointStatus::Failed("timeout"));
    let log = host.log().text();
    assert!(
        log.ends_with("result status=failed code=timeout waiting=guest_done\n"),
        "{log}"
    );
    let verdict = verify_real_run(Some(&log), None);
    assert_eq!(
        verdict,
        RealRunVerdict::NotRun {
            code: "missing_guest_log"
        }
    );
}

#[test]
fn a_guest_on_another_build_fails_the_content_check() {
    let mut s = Session::new(None, "0000000ff");
    s.run();
    assert_eq!(s.guest().status(), &EndpointStatus::Failed("content_mismatch"));
    let (host, guest) = s.logs();
    assert!(guest.contains("content_checked host_commit=a181e5bab"), "{guest}");
    let verdict = verify_real_run(Some(&host), Some(&guest));
    assert_eq!(verdict.code(), "commit_mismatch", "{verdict}");
    // Another app id: the guest refuses the lobby, the verdict names the app id.
    let mut s = Session::new(Some("481"), COMMIT);
    s.run();
    assert_eq!(s.guest().status(), &EndpointStatus::Failed("content_mismatch"));
    let (host, guest) = s.logs();
    assert_eq!(
        verify_real_run(Some(&host), Some(&guest)).code(),
        "app_id_mismatch"
    );
}

#[test]
fn a_stray_datagram_fails_the_host_with_wrong_payload() {
    let mut s = Session::new(None, COMMIT);
    let stranger = user(&s.net, SteamId(76_561_190_000_000_199), "Eve");
    let mut socket = SteamSocket::open(stranger.p2p().expect("p2p"), 0).expect("socket");
    socket.send_to(b"hello", steam_peer_addr(HOST, 0)).expect("send");
    s.run();
    assert_eq!(s.host.status(), &EndpointStatus::Failed("wrong_payload"));
    let (host, guest) = s.logs();
    assert!(host.contains("payload=hello"), "{host}");
    let verdict = verify_real_run(Some(&host), Some(&guest));
    assert_eq!(verdict.code(), "wrong_payload", "{verdict}");
}

#[test]
fn edited_logs_are_wrong_content() {
    let (host, guest) = full_run();
    let check = |host: &str, guest: &str, code: &str| {
        let verdict = verify_real_run(Some(host), Some(guest));
        assert_eq!(verdict.code(), code, "{verdict}");
        assert!(verdict.to_string().starts_with("FAIL ("), "{verdict}");
    };
    // The guest's echo carries another round.
    let lobby = host
        .lines()
        .find_map(|l| l.strip_prefix("lobby_created lobby="))
        .expect("lobby line")
        .to_owned();
    check(
        &host,
        &guest.replacen(&format!("pong.1.{lobby}"), &format!("pong.9.{lobby}"), 1),
        "wrong_payload",
    );
    // Both logs from one account.
    let (host_tag, guest_tag) = (account_tag(HOST), account_tag(GUEST));
    check(
        &host,
        &guest.replacen(&format!("me={guest_tag}"), &format!("me={host_tag}"), 1),
        "same_account",
    );
    // The host saw someone else join.
    check(
        &host.replace(&format!("member={guest_tag}"), "member=acct-00000000000000ee"),
        &guest,
        "peer_mismatch",
    );
    // The cloud file read back differently.
    let read = guest
        .lines()
        .find(|l| l.starts_with("cloud_round_trip"))
        .and_then(|l| l.split(' ').find(|kv| kv.starts_with("read=")))
        .expect("read field")
        .to_owned();
    check(
        &host,
        &guest.replace(&read, "read=00000000000000aa"),
        "cloud_mismatch",
    );
    // Another lobby, another app id, logs swapped, a garbled line, no header.
    check(
        &host,
        &guest.replace(&format!("lobby={lobby} "), "lobby=999999 "),
        "lobby_mismatch",
    );
    check(
        &host,
        &guest.replace("app_id=480", "app_id=481"),
        "app_id_mismatch",
    );
    check(&guest, &host, "wrong_role");
    check(&host, &format!("{guest}garbled line\n"), "log_unreadable");
    check(
        &host,
        guest.trim_start_matches("regia_steam_real_run version=1\n"),
        "log_unreadable",
    );
}

#[test]
fn a_cut_short_log_is_incomplete() {
    let (host, guest) = full_run();
    // The guest stopped after the first echo: no reconnect, no cloud, no result.
    let cut: String = guest
        .lines()
        .take_while(|l| !l.starts_with("rejoin"))
        .map(|l| format!("{l}\n"))
        .collect();
    match verify_real_run(Some(&host), Some(&cut)) {
        RealRunVerdict::Incomplete { code, missing } => {
            assert_eq!(code, "incomplete_guest");
            for step in [
                "rejoin",
                "lobby_entered round=2",
                "datagram_in round=2",
                "cloud_round_trip",
            ] {
                assert!(missing.iter().any(|m| m == step), "{missing:?}");
            }
            assert_eq!(missing.last().map(String::as_str), Some("result status=complete"));
        }
        other => panic!("expected incomplete, got {other}"),
    }
    // The host's process died before its result line.
    let no_result = host.replace("result status=complete\n", "");
    let verdict = verify_real_run(Some(&no_result), Some(&guest));
    assert_eq!(
        verdict,
        RealRunVerdict::Incomplete {
            code: "incomplete_host",
            missing: vec!["result status=complete".into()],
        }
    );
    assert!(verdict.to_string().starts_with("INCOMPLETE (incomplete_host)"));
    assert!(!verdict.is_pass());
}

#[test]
fn an_unavailable_backend_cannot_start_an_endpoint() {
    let backend = MockSteam::shared_unavailable();
    let err =
        RealRunEndpoint::start(setup("host", None, None, COMMIT), backend.as_ref()).expect_err("not ready");
    assert_eq!(err, "steam_not_ready");
}
