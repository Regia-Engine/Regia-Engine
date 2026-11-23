// SPDX-License-Identifier: MPL-2.0
//! The template-match leg of the owner's two-account Steam run (N08, spec 24 § 4, ADR-5375).
//!
//! Section 2 of `book/user-guide/steam-real-run.md` proves lobbies, Steam sockets and the cloud
//! with a scripted echo ([`crate::real_run`]). This leg plays the `net_game` template over the same
//! two accounts: the host's agent creates the lobby (`lobby.create`), the guest's agent joins it
//! (`lobby.join`), the host launches the lobby to its own Steam id and serves the game over
//! [`crate::SteamSocket`], the guest's client follows the launch, its agent reads `net.status`
//! `connected`, and a scripted bot plays a fixed number of ticks. Each endpoint writes its own
//! evidence log, judged here by [`verify_net_leg`]:
//!
//! - the guest connected (`net_status state=connected`) to the host's account (the tag comes
//!   from the link's remote Steam peer), and its link was still connected when it reported;
//! - the server received at least [`MIN_SERVER_DATAGRAMS`] datagrams (the match was played);
//! - every client's `netstats` line has `corrections` at most [`MAX_CORRECTIONS`], `resyncs` at
//!   most [`MAX_RESYNCS`] and no `verify_failures` (the template's windowed acceptance bounds,
//!   `book/agents/net-match.md`);
//! - both logs name the same app id, commit and lobby, and two different accounts.
//!
//! The driver lives with the template (`samples/net_game`, feature `steam`), because it needs the
//! game; this module holds only what judges the logs, so the gate tests it without the game.
//! The log format is the one of [`crate::real_run`] with its own header, [`NET_LOG_HEADER`].

use std::path::Path;

use crate::real_run::{
    self, Endpoint, LOG_HEADER, RealRunLog, RealRunNotRun, RealRunSetup, RealRunVerdict, parse_log_with,
};

/// `host` or `guest` for the template-match leg; unset means the leg was not set up (`NOT RUN`).
pub const NET_LEG_ENV: &str = "REGIA_STEAM_NET_LEG";
/// Where the leg's endpoint writes its evidence log (default `regia-steam-net-leg-<role>.log`).
pub const NET_LOG_ENV: &str = "REGIA_STEAM_NET_LOG";
/// The first line of every net-leg evidence log.
pub const NET_LOG_HEADER: &str = "regia_steam_net_leg version=1";
/// At most this many prediction corrections per client (the windowed acceptance bound).
pub const MAX_CORRECTIONS: u64 = 20;
/// At most this many prediction resyncs per client.
pub const MAX_RESYNCS: u64 = 2;
/// The server must have received at least this many datagrams from the match: the guest's bot
/// plays 600 fixed steps (`net_game::steam_leg::LEG_TICKS`) and sends one input datagram or more
/// per step, so half of that proves the match was played on the link, not only joined.
pub const MIN_SERVER_DATAGRAMS: u64 = 300;

/// Reads the leg's settings from raw values: `role` is `REGIA_STEAM_NET_LEG`, the others as in
/// [`RealRunSetup::from_values`] (`REGIA_STEAM_LOBBY_ID`, `STEAM_APP_ID`, `REGIA_STEAM_RUN_COMMIT`).
///
/// # Errors
/// [`RealRunNotRun`] with the codes of [`RealRunSetup::from_values`]; the messages name
/// `REGIA_STEAM_NET_LEG`.
pub fn net_leg_setup(
    role: Option<&str>,
    lobby: Option<&str>,
    app: Option<&str>,
    commit: Option<&str>,
) -> Result<RealRunSetup, RealRunNotRun> {
    RealRunSetup::from_values(role, lobby, app, commit).map_err(|mut not_run| {
        not_run.detail = not_run.detail.replace(real_run::ROLE_ENV, NET_LEG_ENV);
        if not_run.code == "setup_absent" {
            not_run.detail =
                format!("{NET_LEG_ENV} is not set; the template-match leg needs an owner session");
        }
        not_run
    })
}

/// [`net_leg_setup`] from the process environment.
///
/// # Errors
/// As [`net_leg_setup`].
pub fn net_leg_setup_from_env() -> Result<RealRunSetup, RealRunNotRun> {
    let var = |name: &str| regia_core::brand::env_var(name).ok();
    net_leg_setup(
        var(NET_LEG_ENV).as_deref(),
        var(real_run::LOBBY_ENV).as_deref(),
        var("STEAM_APP_ID").as_deref(),
        var(real_run::COMMIT_ENV).as_deref(),
    )
}

/// An empty net-leg evidence log (its first line is [`NET_LOG_HEADER`]).
pub fn net_leg_log() -> RealRunLog {
    RealRunLog::with_header(NET_LOG_HEADER)
}

/// A number field, or `None` when absent or not a number.
fn number(record: &std::collections::BTreeMap<String, String>, key: &str) -> Option<u64> {
    record.get(key).and_then(|v| v.parse().ok())
}

/// Judges the host's and the guest's net-leg logs (`None` = the file does not exist).
///
/// Codes: `NOT RUN` (`setup_absent`, `missing_host_log`, `missing_guest_log`); `FAIL`
/// (`log_unreadable`, `wrong_role`, `app_id_mismatch`, `commit_mismatch`, `same_account`,
/// `content_mismatch`, `endpoint_failed`, `lobby_mismatch`, `peer_mismatch`,
/// `net_not_connected`, `net_disconnected`, `net_corrections`, `net_resyncs`,
/// `net_verify_failed`, `net_too_short`); `INCOMPLETE`
/// (`incomplete_host`, `incomplete_guest`). The order is the one of
/// [`crate::real_run::verify_real_run`]: absent, unreadable, contradictions, then missing steps.
pub fn verify_net_leg(host: Option<&str>, guest: Option<&str>) -> RealRunVerdict {
    let (host, guest) = match (host, guest) {
        (None, None) => return RealRunVerdict::NotRun { code: "setup_absent" },
        (None, Some(_)) => {
            return RealRunVerdict::NotRun {
                code: "missing_host_log",
            };
        }
        (Some(_), None) => {
            return RealRunVerdict::NotRun {
                code: "missing_guest_log",
            };
        }
        (Some(h), Some(g)) => (h, g),
    };
    let fail = |code, detail: String| RealRunVerdict::Fail { code, detail };
    let (Some(host), Some(guest)) = (
        parse_log_with(host, NET_LOG_HEADER),
        parse_log_with(guest, NET_LOG_HEADER),
    ) else {
        return fail(
            "log_unreadable",
            "a net-leg log lacks the header or has a malformed line".into(),
        );
    };
    let (h, g) = (Endpoint { records: host }, Endpoint { records: guest });
    if h.field("endpoint", "role") != Some("host") || g.field("endpoint", "role") != Some("guest") {
        return fail(
            "wrong_role",
            "the first log must be the host's, the second the guest's".into(),
        );
    }
    let same = |key: &str| h.field("endpoint", key) == g.field("endpoint", key);
    if !same("app_id") {
        return fail(
            "app_id_mismatch",
            format!(
                "{:?} vs {:?}",
                h.field("endpoint", "app_id"),
                g.field("endpoint", "app_id")
            ),
        );
    }
    if !same("commit") {
        return fail("commit_mismatch", "the machines ran different builds".into());
    }
    let (host_me, guest_me) = (h.field("endpoint", "me"), g.field("endpoint", "me"));
    if host_me.is_none() || host_me == guest_me {
        return fail(
            "same_account",
            "both endpoints report the same Steam account".into(),
        );
    }
    let failed = |e: &Endpoint| {
        e.all("result")
            .find(|r| r.get("status").map(String::as_str) == Some("failed"))
            .map(|r| r.get("code").map_or("unknown", String::as_str).to_owned())
    };
    // A guest that refused the lobby's content makes the host fail too (it never gets a peer):
    // the content mismatch is the finding, so it is reported before any other endpoint failure.
    // (Another build is already `commit_mismatch` above, from the two logs' own commits; this
    // catches a lobby that advertised content the host's log does not name.)
    for (who, e) in [("host", &h), ("guest", &g)] {
        if failed(e).as_deref() == Some("content_mismatch") {
            return fail(
                "content_mismatch",
                format!("{who}: the lobby advertised another build"),
            );
        }
    }
    for (who, e) in [("host", &h), ("guest", &g)] {
        if let Some(code) = failed(e) {
            return fail("endpoint_failed", format!("{who} failed with `{code}`"));
        }
    }
    let created = h
        .all("agent")
        .find(|r| r.get("method").map(String::as_str) == Some("lobby.create"))
        .and_then(|r| r.get("lobby"));
    if let Some(lobby) = created {
        let joined_ok = g
            .all("agent")
            .filter(|r| r.get("method").map(String::as_str) == Some("lobby.join"))
            .chain(g.all("lobby_entered"))
            .all(|r| r.get("lobby") == Some(lobby));
        if !joined_ok {
            return fail("lobby_mismatch", "the guest joined another lobby".into());
        }
    }
    let peer_ok = |e: &Endpoint, name: &str, key: &str, want: Option<&str>| {
        e.all(name).all(|r| r.get(key).map(String::as_str) == want)
    };
    if !peer_ok(&h, "member_joined", "member", guest_me)
        || !peer_ok(&h, "launch", "server", host_me)
        || !peer_ok(&g, "lobby_entered", "owner", host_me)
        || !peer_ok(&g, "net_status", "server", host_me)
    {
        return fail(
            "peer_mismatch",
            "a record names an account other than the other endpoint".into(),
        );
    }
    if let Some(r) = g
        .all("net_status")
        .find(|r| r.get("state").map(String::as_str) != Some("connected"))
    {
        return fail(
            "net_not_connected",
            format!("the guest's link reported {:?}, not connected", r.get("state")),
        );
    }
    for (who, e) in [("host", &h), ("guest", &g)] {
        for r in e
            .all("netstats")
            .filter(|r| r.get("role").map(String::as_str) == Some("client"))
        {
            let client = r.get("who").map_or(who, String::as_str);
            // The counters only mean something for a link that was still up at the report.
            if r.get("state").map(String::as_str) != Some("connected") {
                return fail(
                    "net_disconnected",
                    format!(
                        "{client}: the link reported {:?} at the end of the match",
                        r.get("state")
                    ),
                );
            }
            let (Some(corrections), Some(resyncs), Some(verify)) = (
                number(r, "corrections"),
                number(r, "resyncs"),
                number(r, "verify_failures"),
            ) else {
                return fail(
                    "log_unreadable",
                    format!("{client}: a client netstats line lacks its prediction counters"),
                );
            };
            if corrections > MAX_CORRECTIONS {
                return fail(
                    "net_corrections",
                    format!("{client}: {corrections} corrections (at most {MAX_CORRECTIONS})"),
                );
            }
            if resyncs > MAX_RESYNCS {
                return fail(
                    "net_resyncs",
                    format!("{client}: {resyncs} resyncs (at most {MAX_RESYNCS})"),
                );
            }
            if verify > 0 {
                return fail(
                    "net_verify_failed",
                    format!("{client}: {verify} prediction verify failures"),
                );
            }
        }
    }
    if let Some(r) = h
        .all("netstats")
        .find(|r| r.get("role").map(String::as_str) == Some("server"))
    {
        let datagrams = number(r, "datagrams_in").unwrap_or(0);
        if datagrams < MIN_SERVER_DATAGRAMS {
            return fail(
                "net_too_short",
                format!("the server received {datagrams} datagrams (at least {MIN_SERVER_DATAGRAMS})"),
            );
        }
    }
    let agent = |e: &Endpoint, method: &str| {
        e.all("agent")
            .any(|r| r.get("method").map(String::as_str) == Some(method) && r.contains_key("lobby"))
    };
    let netstats = |e: &Endpoint, role: &str| {
        e.all("netstats")
            .any(|r| r.get("role").map(String::as_str) == Some(role))
    };
    let host_steps: [(&str, bool); 5] = [
        ("agent method=lobby.create", agent(&h, "lobby.create")),
        ("member_joined", h.first("member_joined").is_some()),
        ("launch", h.first("launch").is_some()),
        (
            "net_status endpoint=server peers>=1",
            h.all("net_status").any(|r| {
                r.get("endpoint").map(String::as_str) == Some("server")
                    && number(r, "peers").is_some_and(|n| n >= 1)
            }),
        ),
        ("netstats role=server", netstats(&h, "server")),
    ];
    let guest_steps: [(&str, bool); 5] = [
        ("agent method=lobby.join", agent(&g, "lobby.join")),
        ("lobby_entered", g.first("lobby_entered").is_some()),
        ("content_checked", g.first("content_checked").is_some()),
        ("net_status state=connected", g.first("net_status").is_some()),
        ("netstats role=client", netstats(&g, "client")),
    ];
    let complete = |e: &Endpoint| {
        e.all("result")
            .any(|r| r.get("status").map(String::as_str) == Some("complete"))
    };
    for (code, steps, e) in [
        ("incomplete_host", &host_steps, &h),
        ("incomplete_guest", &guest_steps, &g),
    ] {
        let mut missing: Vec<String> = steps
            .iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| (*name).to_owned())
            .collect();
        if !complete(e) {
            missing.push("result status=complete".into());
        }
        if !missing.is_empty() {
            return RealRunVerdict::Incomplete { code, missing };
        }
    }
    RealRunVerdict::Pass
}

/// Judges two log files of either leg: section 2's echo logs ([`crate::real_run::LOG_HEADER`])
/// through [`crate::real_run::verify_real_run`], net-leg logs ([`NET_LOG_HEADER`]) through
/// [`verify_net_leg`]. A file that does not exist counts as absent; one log of each leg is
/// `FAIL (log_unreadable)`.
///
/// # Errors
/// An IO error other than "not found".
pub fn verify_leg_files(host: &Path, guest: &Path) -> std::io::Result<RealRunVerdict> {
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    };
    let (host, guest) = (read(host)?, read(guest)?);
    Ok(verify_leg(host.as_deref(), guest.as_deref()))
}

/// The first non-empty line of a log.
fn header(text: Option<&str>) -> Option<&str> {
    text.and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
}

/// [`verify_leg_files`] on text already read (`None` = absent).
pub fn verify_leg(host: Option<&str>, guest: Option<&str>) -> RealRunVerdict {
    let is_net = |text: Option<&str>| header(text) == Some(NET_LOG_HEADER);
    let is_echo = |text: Option<&str>| header(text) == Some(LOG_HEADER);
    match (is_net(host), is_net(guest)) {
        (true, true) => verify_net_leg(host, guest),
        // One log present and of the net leg: judge it as that leg (reports the missing one).
        (true, false) if guest.is_none() => verify_net_leg(host, guest),
        (false, true) if host.is_none() => verify_net_leg(host, guest),
        (true, false) | (false, true) if is_echo(host) || is_echo(guest) => RealRunVerdict::Fail {
            code: "log_unreadable",
            detail: "one log is from the echo run, the other from the template-match leg".into(),
        },
        _ => real_run::verify_real_run(host, guest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_net_leg_setup_names_its_own_variable() {
        let err = net_leg_setup(None, None, None, Some("abc")).expect_err("absent");
        assert_eq!(err.code, "setup_absent");
        assert!(err.detail.contains(NET_LEG_ENV), "{err}");
        let err = net_leg_setup(Some("server"), None, None, Some("abc")).expect_err("bad role");
        assert_eq!(err.code, "bad_role");
        assert!(
            err.detail.contains(NET_LEG_ENV) && !err.detail.contains(real_run::ROLE_ENV),
            "{err}"
        );
        let setup = net_leg_setup(Some("guest"), Some("9"), Some("480"), Some("abc")).expect("guest");
        assert_eq!(setup.lobby, Some(regia_net::lobby::LobbyId(9)));
    }

    #[test]
    fn mixed_legs_are_unreadable_not_judged_as_either() {
        let echo = format!("{LOG_HEADER}\nendpoint role=host\n");
        let net = format!("{NET_LOG_HEADER}\nendpoint role=guest\n");
        assert_eq!(verify_leg(Some(&echo), Some(&net)).code(), "log_unreadable");
        assert_eq!(verify_leg(Some(&net), None).code(), "missing_guest_log");
        assert_eq!(verify_leg(None, None).code(), "setup_absent");
    }
}
