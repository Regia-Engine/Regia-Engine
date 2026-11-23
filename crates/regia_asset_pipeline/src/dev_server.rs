// SPDX-License-Identifier: MPL-2.0
//! How a dev dedicated server and its clients are started (`docs/specs/76-dedicated-server-package.md`
//! § 10, `docs/specs/77-session-lifecycle.md` § 10.4, ADR-2682).
//!
//! `regia server --clients N` and the editor's play-in-editor with separate processes start their
//! processes through the same functions: [`server_command`] for the server and [`client_command`] for
//! each client. Both crates are the Tools layer and may not depend on each other, so this shared
//! piece lives in the desktop tooling library they both use. Building the binaries stays in
//! `regia server`.
//!
//! What a process receives:
//!
//! | Process | Command line | Environment |
//! |---|---|---|
//! | server | `--relay local:0`, `--level L`, `--port P`, `--agent` | `REGIA_PROJECT`, `REGIA_ASSETS`, `REGIA_SERVER_DIR` |
//! | client | none | `REGIA_PROJECT`, `REGIA_ASSETS`, `REGIA_CONNECT`, `REGIA_CLIENT_NAME`, `REGIA_CLIENT_OFFSCREEN`, `REGIA_NET_MANIFEST`, `REGIA_AUDIO`, and `REGIA_JOIN_CODE` + `REGIA_RELAY` when the server printed a join code |
//!
//! Play-in-editor adds [`PieExtras`] to every process: `REGIA_PIE_LEVEL`, `REGIA_PIE_NET_MODE`,
//! `REGIA_NET_CONDITIONER`, `REGIA_NET_CONDITIONER_SEED`, and for a windowed client
//! `REGIA_CLIENT_TITLE` instead of `REGIA_CLIENT_OFFSCREEN`.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

/// The server directory the dev server reads (`server.toml`, `content/`, `logs/`).
pub const SERVER_DIR_ENV: &str = "REGIA_SERVER_DIR";
/// The server a dev client joins, `address:port`.
pub const CONNECT_ENV: &str = "REGIA_CONNECT";
/// `1`: the client runs without a window.
pub const OFFSCREEN_ENV: &str = "REGIA_CLIENT_OFFSCREEN";
/// The player's name.
pub const NAME_ENV: &str = "REGIA_CLIENT_NAME";
/// The title of a windowed dev client's window.
pub const TITLE_ENV: &str = "REGIA_CLIENT_TITLE";
/// The join code the server printed.
pub const JOIN_CODE_ENV: &str = "REGIA_JOIN_CODE";
/// The relay that resolves [`JOIN_CODE_ENV`].
pub const RELAY_ENV: &str = "REGIA_RELAY";
/// The server's content manifest.
pub const MANIFEST_ENV: &str = "REGIA_NET_MANIFEST";
/// The level a play-in-editor session plays (absolute path of the play copy).
pub const PIE_LEVEL_ENV: &str = "REGIA_PIE_LEVEL";
/// The play-in-editor net mode: `listen` or `dedicated`.
pub const PIE_MODE_ENV: &str = "REGIA_PIE_NET_MODE";
/// The conditioner preset of a play-in-editor session (`off`, `lan`, `good`, `average`, `bad`,
/// `terrible`; spec 77 § 10.1).
pub const CONDITIONER_ENV: &str = "REGIA_NET_CONDITIONER";
/// The conditioner's seed.
pub const CONDITIONER_SEED_ENV: &str = "REGIA_NET_CONDITIONER_SEED";

/// One dev server process.
#[derive(Debug, Clone)]
pub struct ServerSpec<'a> {
    /// The server binary.
    pub binary: &'a Path,
    /// The project directory (the process's working directory).
    pub project: &'a Path,
    /// The server directory ([`SERVER_DIR_ENV`]).
    pub server_dir: &'a Path,
    /// `--level`, as the rotation names it (relative to `assets/`).
    pub level: Option<&'a str>,
    /// `--port`.
    pub port: Option<u16>,
    /// `--agent`.
    pub agent: bool,
}

/// One dev client process.
#[derive(Debug, Clone)]
pub struct ClientSpec<'a> {
    /// The client binary.
    pub binary: &'a Path,
    /// The project directory (the process's working directory).
    pub project: &'a Path,
    /// The server's `address:port`.
    pub addr: &'a str,
    /// The join code and relay the server printed, when it did.
    pub join: Option<&'a (String, String)>,
    /// The client's index: its player is named `client<index>`.
    pub index: u32,
    /// The server's content manifest.
    pub manifest: &'a Path,
}

/// What play-in-editor adds to every process it starts (ADR-2682).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PieExtras {
    /// The play copy of the open level ([`PIE_LEVEL_ENV`]).
    pub level: Option<String>,
    /// `listen` or `dedicated` ([`PIE_MODE_ENV`]).
    pub mode: String,
    /// The conditioner preset ([`CONDITIONER_ENV`]); empty = `off`.
    pub conditioner: String,
    /// The conditioner seed ([`CONDITIONER_SEED_ENV`]).
    pub seed: u64,
}

impl PieExtras {
    /// Adds the variables to `cmd`.
    pub fn apply(&self, cmd: &mut Command) {
        if let Some(level) = &self.level {
            cmd.env(PIE_LEVEL_ENV, level);
        }
        let conditioner = if self.conditioner.is_empty() {
            "off"
        } else {
            self.conditioner.as_str()
        };
        cmd.env(PIE_MODE_ENV, &self.mode)
            .env(CONDITIONER_ENV, conditioner)
            .env(CONDITIONER_SEED_ENV, self.seed.to_string());
    }
}

/// The server's own arguments: always a relay inside the dev server, so it prints a join code.
#[must_use]
pub fn server_args(level: Option<&str>, port: Option<u16>, agent: bool) -> Vec<String> {
    let mut out = vec!["--relay".to_string(), "local:0".to_string()];
    if let Some(level) = level {
        out.extend(["--level".to_string(), level.to_string()]);
    }
    if let Some(port) = port {
        out.extend(["--port".to_string(), port.to_string()]);
    }
    if agent {
        out.push("--agent".to_string());
    }
    out
}

/// The server process of `spec`: its arguments, working directory and environment.
#[must_use]
pub fn server_command(spec: &ServerSpec<'_>) -> Command {
    let mut cmd = Command::new(spec.binary);
    cmd.args(server_args(spec.level, spec.port, spec.agent))
        .current_dir(spec.project)
        .env("REGIA_PROJECT", spec.project)
        .env("REGIA_ASSETS", spec.project.join("assets"))
        .env(SERVER_DIR_ENV, spec.server_dir);
    cmd
}

/// A client process: offscreen, audio off, joined to `addr` (or by the join code and relay), with
/// the server's manifest.
#[must_use]
pub fn client_command(spec: &ClientSpec<'_>) -> Command {
    let mut cmd = Command::new(spec.binary);
    if let Some((code, relay)) = spec.join {
        cmd.env(JOIN_CODE_ENV, code).env(RELAY_ENV, relay);
    }
    cmd.current_dir(spec.project)
        .env("REGIA_PROJECT", spec.project)
        .env("REGIA_ASSETS", spec.project.join("assets"))
        .env(CONNECT_ENV, spec.addr)
        .env(NAME_ENV, format!("client{}", spec.index))
        .env(OFFSCREEN_ENV, "1")
        .env(MANIFEST_ENV, spec.manifest)
        .env("REGIA_AUDIO", "off");
    cmd
}

/// Makes a [`client_command`] windowed: no [`OFFSCREEN_ENV`], and the window titled `title`.
pub fn make_windowed(cmd: &mut Command, title: &str) {
    cmd.env_remove(OFFSCREEN_ENV).env(TITLE_ENV, title);
}

/// The `join code <code> relay <addr>` line the server host prints (ADR-2440).
#[must_use]
pub fn parse_join_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("join code ")?;
    let (code, relay) = rest.split_once(" relay ")?;
    Some((code.trim().to_owned(), relay.trim().to_owned()))
}

/// The game port of the `listening <addr> key <key>` line the server host prints.
#[must_use]
pub fn parse_listening_line(line: &str) -> Option<SocketAddr> {
    let rest = line.trim().strip_prefix("listening ")?;
    let addr = rest.split_whitespace().next()?;
    addr.parse().ok()
}

/// The address a client on this machine dials for a server bound to `bound`: an unspecified bind
/// (`0.0.0.0`, `::`) is reached on loopback.
#[must_use]
pub fn loopback_dial(bound: SocketAddr) -> SocketAddr {
    if bound.ip().is_unspecified() {
        let ip = if bound.is_ipv4() {
            std::net::IpAddr::from([127, 0, 0, 1])
        } else {
            std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST)
        };
        SocketAddr::new(ip, bound.port())
    } else {
        bound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
    }

    fn removed(cmd: &Command, key: &str) -> bool {
        cmd.get_envs().any(|(k, v)| k == key && v.is_none())
    }

    #[test]
    fn server_flags_and_environment() {
        assert_eq!(server_args(None, None, false), ["--relay", "local:0"]);
        let cmd = server_command(&ServerSpec {
            binary: Path::new("/b/server"),
            project: Path::new("/p"),
            server_dir: Path::new("/p/build/pie/1"),
            level: Some(".pie/play.level.ron"),
            port: Some(40123),
            agent: true,
        });
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            [
                "--relay",
                "local:0",
                "--level",
                ".pie/play.level.ron",
                "--port",
                "40123",
                "--agent"
            ]
        );
        assert_eq!(env_of(&cmd, SERVER_DIR_ENV).as_deref(), Some("/p/build/pie/1"));
        assert_eq!(env_of(&cmd, "REGIA_PROJECT").as_deref(), Some("/p"));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/p")));
    }

    #[test]
    fn a_client_is_offscreen_unless_made_windowed() {
        let join = ("ABCDEF".to_owned(), "127.0.0.1:40000".to_owned());
        let spec = ClientSpec {
            binary: Path::new("/b/client"),
            project: Path::new("/p"),
            addr: "127.0.0.1:7000",
            join: Some(&join),
            index: 1,
            manifest: Path::new("/d/content/net-manifest.ron"),
        };
        let cmd = client_command(&spec);
        assert_eq!(env_of(&cmd, CONNECT_ENV).as_deref(), Some("127.0.0.1:7000"));
        assert_eq!(env_of(&cmd, NAME_ENV).as_deref(), Some("client1"));
        assert_eq!(env_of(&cmd, OFFSCREEN_ENV).as_deref(), Some("1"));
        assert_eq!(env_of(&cmd, JOIN_CODE_ENV).as_deref(), Some("ABCDEF"));
        assert_eq!(env_of(&cmd, RELAY_ENV).as_deref(), Some("127.0.0.1:40000"));
        let mut windowed = client_command(&spec);
        make_windowed(&mut windowed, "PIE client 1");
        assert!(removed(&windowed, OFFSCREEN_ENV), "no offscreen flag");
        assert_eq!(env_of(&windowed, TITLE_ENV).as_deref(), Some("PIE client 1"));
    }

    #[test]
    fn pie_extras_reach_the_environment() {
        let mut cmd = Command::new("/b/x");
        PieExtras {
            level: Some("/p/assets/.pie/play.level.ron".into()),
            mode: "dedicated".into(),
            conditioner: String::new(),
            seed: 9,
        }
        .apply(&mut cmd);
        assert_eq!(
            env_of(&cmd, PIE_LEVEL_ENV).as_deref(),
            Some("/p/assets/.pie/play.level.ron")
        );
        assert_eq!(env_of(&cmd, PIE_MODE_ENV).as_deref(), Some("dedicated"));
        assert_eq!(env_of(&cmd, CONDITIONER_ENV).as_deref(), Some("off"));
        assert_eq!(env_of(&cmd, CONDITIONER_SEED_ENV).as_deref(), Some("9"));
    }

    #[test]
    fn host_lines_are_read_back() {
        assert_eq!(
            parse_join_line("join code ABCDEF relay 127.0.0.1:40000\n"),
            Some(("ABCDEF".to_owned(), "127.0.0.1:40000".to_owned()))
        );
        assert_eq!(parse_join_line("join code ABCDEF"), None);
        assert_eq!(
            parse_listening_line("listening 0.0.0.0:7777 key abc"),
            Some("0.0.0.0:7777".parse().expect("addr"))
        );
        assert_eq!(parse_listening_line("relay 127.0.0.1:1"), None);
        assert_eq!(
            loopback_dial("0.0.0.0:7777".parse().expect("addr")),
            "127.0.0.1:7777".parse().expect("addr")
        );
        assert_eq!(
            loopback_dial("127.0.0.5:9".parse().expect("addr")),
            "127.0.0.5:9".parse().expect("addr")
        );
    }
}
