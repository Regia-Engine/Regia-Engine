// SPDX-License-Identifier: MPL-2.0
//! `regia package --server` (`docs/specs/76-dedicated-server-package.md` § 3, ADR-1886): the game's
//! `server` feature, the filtered content, `net-manifest.ron`, the default `server.toml`, an empty
//! `bans.ron` and `README-server.txt`, staged as
//!
//! ```text
//! <out>/<slug>-server/
//!     <bin>-server(.exe)     the binary
//!     content/base.fpak      server content
//!     content/net-manifest.ron
//!     assets/                raw gameplay data (levels, prefabs, tables), read through `assets://`
//!     server.toml  bans.ron  README-server.txt  tier.ron
//!     regia-lobby-hub(.exe)  hub.toml   the lobby hub (NL-06, ADR-8891)
//!     deploy/regia-server.service
//!     deploy/regia-hub.service
//! ```
//!
//! Every unit under `deploy/` starts a binary the package ships (KI-NT-043, ADR-8891). Regia has
//! no standalone relay binary yet, so no relay unit is written.
//!
//! and archived as `<slug>-server-linux.tar.gz` or `<slug>-server-windows.zip`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use regia_net::server_host::config::DEFAULT_SERVER_TOML;
use regia_net::session::BanList;

use crate::commands::build_profile::{self, ResolvedProfile};
use crate::commands::package::{
    self, Format, PackageArgs, Platform, Profile, SymbolFormat, cargo_workspace_root, default_out_dir,
};
use crate::commands::ship_check;
use crate::manifest::Manifest;
use crate::process;

/// Crates a server build never links (spec 76 § 2.1): the graph and the binary are both checked.
pub const FORBIDDEN_CRATES: &[&str] = &[
    "regia_render",
    "regia_audio",
    "regia_ui",
    "regia_vfx",
    "regia_2d",
    "winit",
    "wgpu",
    "kira",
];

/// The forbidden crates in the output of `cargo tree --prefix none`.
#[must_use]
pub fn forbidden_in_tree(tree: &str) -> Vec<String> {
    let names: BTreeSet<&str> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    FORBIDDEN_CRATES
        .iter()
        .filter(|c| names.contains(**c))
        .map(|c| (*c).to_string())
        .collect()
}

/// Strings that mean a forbidden crate is linked: its mangled symbols, which a stripped binary
/// lacks, and its panic locations (`crates/<name>/src/...`, `.../<name>-<version>/src/...`), which
/// survive stripping. Not `<name>::`: `regia_scene` names client component types in string literals.
fn forbidden_markers() -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    for name in FORBIDDEN_CRATES {
        out.push((format!("_ZN{}{name}", name.len()), "a client crate is linked"));
        out.push((format!("{name}/src"), "a client crate is linked"));
        out.push((format!("/{name}-"), "a client crate is linked"));
    }
    out
}

/// The binary checks of spec 76 § 3: no forbidden crate in the symbol table or panic locations.
///
/// # Errors
///
/// The binary cannot be read, or it holds a marker.
pub fn check_binary_links_no_client(binary: &Path) -> Result<()> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let found = ship_check::find_forbidden(&bytes, &forbidden_markers());
    if !found.is_empty() {
        bail!(
            "the server binary {} links client code:\n  {}",
            binary.display(),
            found.join("\n  ")
        );
    }
    Ok(())
}

/// `cargo tree` for the server build of `package` on `target`: fails when a forbidden crate is in
/// the graph (the game kept `regia`'s default features, or a dependency pulls a client crate).
///
/// # Errors
///
/// `cargo tree` fails, or the graph holds a forbidden crate.
fn check_graph(
    workspace: &Path,
    package: &str,
    resolved: &ResolvedProfile,
    target: Option<&str>,
) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.args(["tree", "-e", "normal", "--prefix", "none", "-p", package]);
    if let Some(triple) = target {
        cmd.args(["--target", triple]);
    }
    cmd.args(build_profile::cargo_extra_args(resolved))
        .current_dir(workspace);
    let (tree, _) = process::output(&mut cmd, "Install Rust from https://rustup.rs")?;
    let found = forbidden_in_tree(&tree);
    if !found.is_empty() {
        bail!(
            "the server build of `{package}` depends on {}: a server build has no window, render or \
             audio crate. Give the game `regia = {{ default-features = false }}` with \
             `[features] default = [\"regia/client\", ...]` and `server = [\"regia/server\"]`, and put \
             client-only dependencies behind the default feature",
            found.join(", ")
        );
    }
    Ok(())
}

/// Fails unless the game's manifest declares the `server` feature (spec 76 § 2).
///
/// # Errors
///
/// No `[features] server` entry.
pub fn check_server_feature(project_dir: &Path, package: &str) -> Result<()> {
    for path in [
        project_dir.join("Cargo.toml"),
        project_dir.join("launcher").join("Cargo.toml"),
        project_dir.join("server").join("Cargo.toml"),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        let name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str);
        if name != Some(package) {
            continue;
        }
        if doc.get("features").and_then(|f| f.get("server")).is_some() {
            return Ok(());
        }
        bail!(
            "{} has no `server` feature: add `[features] server = [\"regia/server\"]` (spec 76 § 2)",
            path.display()
        );
    }
    Ok(())
}

/// The default `server.toml` with the project's name and rotation in place of the examples.
#[must_use]
pub fn server_toml_text(project_name: &str, levels: &[String]) -> String {
    let quoted = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let rotation = levels
        .iter()
        .map(|l| format!("\"{}\"", quoted(l)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = String::with_capacity(DEFAULT_SERVER_TOML.len());
    for line in DEFAULT_SERVER_TOML.lines() {
        if line.starts_with("name = ") {
            let comment = line.split_once('#').map_or("", |(_, c)| c);
            out.push_str(&format!(
                "name = \"{} Server\"           #{comment}\n",
                quoted(project_name)
            ));
        } else if line.starts_with("rotation = ") && !levels.is_empty() {
            let comment = line.split_once('#').map_or("", |(_, c)| c);
            out.push_str(&format!("rotation = [{rotation}]  #{comment}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The levels a server rotates by default: the profile's entry levels, else every kept level.
fn default_rotation(resolved: &ResolvedProfile, kept_raw: &BTreeSet<String>) -> Vec<String> {
    if let Some(spec) = &resolved.spec
        && !spec.entry_levels.is_empty()
    {
        return spec
            .entry_levels
            .iter()
            .map(|level| {
                let level = level.strip_prefix("assets://").unwrap_or(level);
                level.strip_prefix("assets/").unwrap_or(level).to_owned()
            })
            .collect();
    }
    kept_raw
        .iter()
        .filter(|p| p.ends_with(".level.ron"))
        .cloned()
        .collect()
}

/// The README section the server package appends. One heading, so a second stage does not repeat
/// it.
const LOBBY_HUB_README: &str = "
Lobby hub
================================================================

This package also contains regia-lobby-hub and hub.toml.

    ./regia-lobby-hub
    ./regia-lobby-hub --config hub.toml --port 7810 --bind 0.0.0.0 --key-file PATH

hub.toml sets the listen port, the server key path, the caps and the per-address and
per-identity rate limits. A bad value prints one line per field (`field: expected ...`) and
exits 2. The key file is created on first run and is refused when an ancestor directory holds
Regia.toml, so the key is never placed inside a project.

GET /health on status.bind:status.port returns JSON (sessions, lobbies, state_bytes, draining);
503 while draining. SIGTERM or SIGINT drains and exits 0; a second one exits at once. SIGHUP
does not reload hub.toml: restart the process to change it.

The agent method lobby.hub_status reads the same report the process publishes.
";

/// Copies `binary` into `staging` under its own file name, writes `hub.toml` atomically, and
/// appends the lobby-hub section of `README-server.txt` when that section is not already there.
///
/// # Errors
///
/// The binary could not be copied, or a staged file could not be written.
pub fn stage_lobby_hub_into(staging: &Path, binary: &Path) -> Result<()> {
    let name = binary
        .file_name()
        .context("the lobby hub binary has no file name")?;
    std::fs::copy(binary, staging.join(name))
        .with_context(|| format!("copying {} into the server package", binary.display()))?;
    let toml_tmp = staging.join(".hub.toml.tmp");
    std::fs::write(&toml_tmp, regia_net::lobby::hub::DEFAULT_HUB_TOML).context("writing hub.toml")?;
    std::fs::rename(&toml_tmp, staging.join("hub.toml")).context("renaming hub.toml")?;
    let readme_path = staging.join("README-server.txt");
    let mut text = if readme_path.is_file() {
        std::fs::read_to_string(&readme_path).context("reading README-server.txt")?
    } else {
        String::new()
    };
    if !text.contains("Lobby hub") {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(LOBBY_HUB_README);
        std::fs::write(&readme_path, text).context("writing README-server.txt")?;
    }
    Ok(())
}

/// File name of the lobby hub binary in a Linux (`windows == false`) or Windows server package.
#[must_use]
pub(crate) fn lobby_hub_file_name(windows: bool) -> &'static str {
    if windows {
        "regia-lobby-hub.exe"
    } else {
        "regia-lobby-hub"
    }
}

/// The `regia-lobby-hub` binary for the package's platform, next to this `regia` binary
/// (`cargo install` and `cargo build -p regia_cli` put it there).
///
/// # Errors
///
/// The binary is not there. `regia package --server` does not build it.
fn lobby_hub_binary_beside_regia(windows: bool) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("the running regia binary")?;
    let dir = exe.parent().context("regia has no directory")?;
    let name = lobby_hub_file_name(windows);
    let path = dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        bail!(
            "{name} was not found next to this regia binary ({path}); a server package ships the \
             lobby hub. Build it for the package's platform (`cargo build -p regia_cli --bin \
             regia-lobby-hub`, with `--target` for a cross package) and place it beside regia",
            path = path.display()
        )
    }
}

/// `README-server.txt`: how to run the server (spec 76 § 4-§ 8).
#[must_use]
pub fn readme_text(project_name: &str, binary: &str) -> String {
    format!(
        "{project_name} dedicated server
================================================================

Run it:

    ./{binary}

It reads server.toml next to the binary. Everything below is in that file; the environment
(REGIA_SERVER_PORT, REGIA_SERVER_MAX_PLAYERS, REGIA_SERVER_LEVEL, REGIA_SERVER_PASSWORD) overrides
it, and the command line overrides both:

    --config PATH  --port N  --bind ADDR  --max-players N  --level PATH
    --name TEXT  --log-dir DIR  --console

After an engine update, ./{binary} --upgrade-config rewrites an older server.toml to the current
format (comments kept) and exits.

Configuration: server.toml
    [server] name, bind, port (UDP), max_players, password, tick_hz, motd, idle_tick_hz
    [levels] rotation, start_index, match_end_delay_s
    [net] client_rate_bytes, server_rate_bytes, send_every_ticks (absent: config/net.ron decides)
    [admin] token_file
    [status] enabled, bind, port
    [logs] dir, keep_files
A bad value prints one line per field and exits with code 2. Unknown keys warn and are ignored.

Stopping and reloading
    Linux: SIGTERM or SIGINT drains (players get `server_shutting_down`, up to 5 s) and exits 0; a
    second one exits at once. SIGHUP reloads name, motd, max_players, password, the rotation, the
    [net] rates and bans.ron; other fields need a restart.
    Windows: Ctrl+C, Ctrl+Break and closing the console drain; reload is the console command.

Console (a terminal, or --console): status, players, kick, ban, unban, bans, level, next, say,
reload, quit, serverkey, joincode, help. `help` lists their arguments.

Remote admin: a token in the [admin] token_file lets a connected client send console commands. The
file must be readable by its owner only (mode 0600 on Linux: no group or other access). It needs
the encrypted transport, so it is off on plain UDP.

Health endpoint: with [status] enabled = true, GET /health on status.bind:status.port returns JSON
with the state, tick, players, level, tick time and overruns; 503 while draining. Read-only.

Logs: under [logs] dir. One line per join, leave, kick, ban, travel and reload, and a stats line
per minute. A crash writes a report there too.

Exit codes
    0   normal shutdown
    2   configuration or command-line error
    3   a port could not be bound
    4   content error (pak, manifest or level)
    70  crash

Running under systemd (restarting is the supervisor's job):

    [Unit]
    Description={project_name} server
    After=network-online.target

    [Service]
    WorkingDirectory=/opt/{binary}
    ExecStart=/opt/{binary}/{binary}
    Restart=on-failure
    RestartSec=5
    KillSignal=SIGTERM
    TimeoutStopSec=10

    [Install]
    WantedBy=multi-user.target

The package also writes those units under deploy/ (regia-server.service and regia-hub.service,
which starts the packaged regia-lobby-hub with hub.toml) and tier.ron, the low residency cap.
Both units start binaries in this package. UDP ports above 1024 need no extra capability.
"
    )
}

/// Settings the two unit files are filled from. Every field is a setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnitSettings {
    /// Account the process runs as.
    pub user: String,
    /// Group the process runs as.
    pub group: String,
    /// Install prefix. The working directory and the binary paths sit under it.
    pub prefix: String,
    /// File name of the packaged dedicated-server binary.
    pub server_binary: String,
    /// File name of the packaged lobby hub binary (`regia-lobby-hub`).
    pub hub_binary: String,
    /// Dedicated-server UDP port (`REGIA_SERVER_PORT`).
    pub server_port: u16,
    /// Hub key path (`--key-file`). A path setting, not a key. The hub's port, caps and limits
    /// are in the packaged `hub.toml`.
    pub hub_key: String,
}

impl UnitSettings {
    /// Defaults: user and group `regia`, prefix `/opt/regia`, server port 7777, the Linux hub
    /// binary and the hub key at `/opt/regia/hub.key`.
    #[must_use]
    pub(crate) fn for_server_binary(binary: &str) -> Self {
        let prefix = "/opt/regia";
        Self {
            user: "regia".to_owned(),
            group: "regia".to_owned(),
            prefix: prefix.to_owned(),
            server_binary: binary.to_owned(),
            hub_binary: lobby_hub_file_name(false).to_owned(),
            server_port: 7777,
            hub_key: format!("{prefix}/hub.key"),
        }
    }
}

/// The unit files: `(file name, unit text)`. Each one's ExecStart starts a packaged binary
/// (KI-NT-043, ADR-8891).
#[must_use]
pub(crate) fn systemd_units(settings: &UnitSettings) -> [(&'static str, String); 2] {
    let server = unit_text(
        "Regia dedicated server",
        &settings.user,
        &settings.group,
        &settings.prefix,
        Some(&format!("REGIA_SERVER_PORT={}", settings.server_port)),
        &format!("{}/{}", settings.prefix, settings.server_binary),
        "Generated by regia package --server. The ExecStart path is the packaged binary.",
    );
    let hub = unit_text(
        "Regia lobby hub",
        &settings.user,
        &settings.group,
        &settings.prefix,
        None,
        &format!(
            "{prefix}/{bin} --config {prefix}/hub.toml --key-file {key}",
            prefix = settings.prefix,
            bin = settings.hub_binary,
            key = settings.hub_key
        ),
        "Generated by regia package --server. The ExecStart path is the packaged regia-lobby-hub; \
         its port, caps and limits are in hub.toml. The key file is created on first run.",
    );
    [("regia-server.service", server), ("regia-hub.service", hub)]
}

fn unit_text(
    description: &str,
    user: &str,
    group: &str,
    prefix: &str,
    environment: Option<&str>,
    exec_start: &str,
    header: &str,
) -> String {
    let environment = environment.map_or_else(String::new, |env| format!("Environment={env}\n"));
    format!(
        "\
# {header}
# UDP ports above 1024 need no extra capability.
[Unit]
Description={description}
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={user}
Group={group}
WorkingDirectory={prefix}
{environment}ExecStart={exec_start}
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
KillSignal=SIGTERM
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
"
    )
}

/// Write `deploy/*.service` under `staging`.
///
/// # Errors
///
/// A directory or a file could not be written.
pub(crate) fn write_deploy_units(staging: &Path, settings: &UnitSettings) -> Result<()> {
    let deploy = staging.join("deploy");
    std::fs::create_dir_all(&deploy)?;
    for (name, text) in systemd_units(settings) {
        std::fs::write(deploy.join(name), text)?;
    }
    Ok(())
}

fn write_tier_ron(staging: &Path) -> Result<()> {
    let text = regia_framework::install_tier::InstallTier::low()
        .to_ron()
        .map_err(|err| anyhow!("tier.ron: {err}"))?;
    std::fs::write(staging.join("tier.ron"), text)?;
    Ok(())
}

/// Builds the package (spec 76 § 3). `resolved.server` is set.
///
/// # Errors
///
/// The build, a check or a copy failed.
pub fn package(
    project_dir: &Path,
    manifest: &Manifest,
    args: &PackageArgs,
    resolved: &ResolvedProfile,
) -> Result<PathBuf> {
    let platform = args.platform;
    let platform_name = package::platform_dir_name(platform);
    let windows = platform == Platform::Windows;
    let fmt = args
        .format
        .unwrap_or(if windows { Format::Zip } else { Format::Tar });
    match (windows, fmt) {
        (_, Format::Dir) | (false, Format::Tar) | (true, Format::Zip) => {}
        _ => bail!("--server packages are `dir`, `tar` (Linux) or `zip` (Windows)"),
    }
    let slug = format!("{}-server", package::staging_slug(manifest));
    // A split project's `server/` crate (ADR-2440), else the root or `launcher/` package.
    let target_pkg = crate::commands::server::server_cargo_target(project_dir)?;
    let cargo_pkg = target_pkg.package.as_str();
    check_server_feature(project_dir, cargo_pkg)?;
    let workspace = cargo_workspace_root(project_dir)?;

    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| default_out_dir(project_dir, &format!("server-{platform_name}")));
    let staging = out_dir.join(&slug);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(staging.join("content"))?;

    // The build, on the floor glibc of a Linux server (spec 21) or cargo-xwin for Windows.
    let glibc = if windows {
        None
    } else {
        package::linux_glibc_floor(args)?
    };
    let linux_target = glibc.map(package::linux_glibc_target);
    let target = if windows {
        package::cross_target(Platform::Windows).map(str::to_owned)
    } else {
        linux_target
    };
    let target = target.as_deref();
    let allow_agent = resolved.allow_agent();
    // An agent server pulls the client crates in: a playtest profile says it wants that (ADR-1886).
    if allow_agent {
        tracing::warn!("profile allows the agent server: this server build links the client crates");
    } else {
        check_graph(
            &workspace,
            cargo_pkg,
            resolved,
            target.map(package::cargo_target_dir_name),
        )?;
    }
    package::build_cargo(project_dir, cargo_pkg, resolved, target)?;
    let built = package::find_binary(project_dir, &target_pkg.bin, args.profile, windows, target)?;
    let bin_name = format!("{}-server{}", target_pkg.bin, if windows { ".exe" } else { "" });
    let staged_bin = staging.join(&bin_name);
    std::fs::copy(&built, &staged_bin)?;
    package::keep_shipping_symbols(
        &built,
        &staged_bin,
        &out_dir,
        args.profile,
        if windows {
            SymbolFormat::Pdb
        } else {
            SymbolFormat::Dwarf
        },
    )?;
    if windows {
        if args.profile == Profile::Shipping {
            ship_check::check_no_build_machine_strings(&staged_bin, &workspace)?;
            ship_check::check_binary_features(&staged_bin, allow_agent, true)?;
        }
    } else {
        package::check_staged_linux_binary(&staged_bin, project_dir, args.profile, glibc, resolved)?;
    }
    if !allow_agent {
        check_binary_links_no_client(&staged_bin)?;
    }

    let content = build_profile::stage_server_content(
        project_dir,
        platform_name,
        resolved,
        &staging,
        &manifest.project.version,
    )?;
    if !staging.join("content/net-manifest.ron").is_file() {
        bail!("the server package has no content/net-manifest.ron");
    }

    let rotation = default_rotation(resolved, &content.kept.raw_set());
    if rotation.is_empty() {
        bail!(
            "the server package has no level: add a `.level.ron` under assets/, or `entry_levels` to the \
             `target = \"server\"` profile"
        );
    }
    std::fs::write(
        staging.join("server.toml"),
        server_toml_text(&manifest.project.name, &rotation),
    )?;
    let bans = BanList::default()
        .to_ron()
        .map_err(|e| anyhow::anyhow!("bans.ron: {e}"))?;
    std::fs::write(staging.join("bans.ron"), bans)?;
    std::fs::write(
        staging.join("README-server.txt"),
        readme_text(&manifest.project.name, &bin_name),
    )?;
    // The lobby hub ships with every server package, and its unit starts it (KI-NT-043).
    let hub = lobby_hub_binary_beside_regia(windows)?;
    stage_lobby_hub_into(&staging, &hub)?;
    if let Some(floor) = glibc {
        // The hub is the build beside `regia`, not a build at the package's floor: say so when it
        // needs a newer glibc than the game binary may (ADR-8891).
        if let Err(err) = ship_check::check_linux_binary(&staging.join(lobby_hub_file_name(false)), floor) {
            tracing::warn!("regia-lobby-hub: {err:#}");
        }
    }
    let mut units = UnitSettings::for_server_binary(&bin_name);
    units.hub_binary = lobby_hub_file_name(windows).to_owned();
    write_deploy_units(&staging, &units)?;
    write_tier_ron(&staging)?;
    package::copy_notices(
        &staging,
        project_dir,
        &package::sbom_triples(target),
        args.no_engine_credit,
    )?;
    let mut extra = serde_json::Map::new();
    extra.insert("target".into(), "server".into());
    if !windows {
        extra.insert(
            "glibc_floor".into(),
            glibc.map_or(serde_json::Value::from("host"), |v| v.to_string().into()),
        );
    }
    package::write_build_info(&staging, project_dir, manifest, extra)?;

    package::check_package_hygiene(project_dir, &staging, args.profile)?;

    if fmt == Format::Dir {
        package::final_artifact_hook(project_dir, &staging, args.profile, &[])?;
        outln!("{}", staging.display());
        return Ok(staging);
    }
    let archive = if windows {
        let zip = out_dir.join(format!("{slug}-windows.zip"));
        package::write_zip(&zip, &out_dir, &staging, &slug)?;
        zip
    } else {
        let tarball = out_dir.join(format!("{slug}-linux.tar.gz"));
        package::write_tarball(&tarball, &out_dir, &slug)?;
        tarball
    };
    package::final_artifact_hook(project_dir, &archive, args.profile, &[format!("{slug}/")])?;
    outln!("{}", archive.display());
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::server_host::config::{CliOverrides, resolve_config};

    #[test]
    fn the_graph_check_names_forbidden_crates_only() {
        let clean = "regia v0.2.2 (/x)\nregia_net v0.2.2 (/x)\nregia_render_helpers v1.0.0\n";
        assert!(forbidden_in_tree(clean).is_empty());
        let dirty = "regia v0.2.2 (/x)\nregia_render v0.2.2 (/x) (*)\nwgpu v30.0.0\nwinit v0.30.13\n";
        assert_eq!(forbidden_in_tree(dirty), ["regia_render", "winit", "wgpu"]);
    }

    #[test]
    fn the_generated_server_toml_parses_and_keeps_the_rotation() {
        let dir = regia_test_utils::unique_temp_dir("server-toml");
        std::fs::create_dir_all(&dir).expect("dir");
        let text = server_toml_text(
            "My \"Game\"",
            &["levels/a.level.ron".to_string(), "levels/b.level.ron".to_string()],
        );
        std::fs::write(dir.join("server.toml"), &text).expect("write");
        let resolved = resolve_config(&dir, &CliOverrides::default(), &|_| None).expect("valid");
        assert_eq!(resolved.settings.name, "My \"Game\" Server");
        assert_eq!(
            resolved.settings.rotation,
            ["levels/a.level.ron", "levels/b.level.ron"]
        );
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rotation_is_named_as_the_server_looks_levels_up() {
        let mut profile = ResolvedProfile {
            name: "server".into(),
            cargo: Profile::Shipping,
            spec: None,
            server: true,
        };
        let kept: BTreeSet<String> = ["levels/a.level.ron", "data/t.ron"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(default_rotation(&profile, &kept), ["levels/a.level.ron"]);
        profile.spec = Some(crate::manifest::BuildProfile {
            entry_levels: vec![
                "assets/levels/b.level.ron".into(),
                "assets://levels/c.level.ron".into(),
            ],
            ..crate::manifest::BuildProfile::default()
        });
        assert_eq!(
            default_rotation(&profile, &kept),
            ["levels/b.level.ron", "levels/c.level.ron"]
        );
    }

    #[test]
    fn a_game_without_the_server_feature_is_told_how_to_add_it() {
        let dir = regia_test_utils::unique_temp_dir("server-feature");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        let err = check_server_feature(&dir, "demo").expect_err("refused");
        assert!(err.to_string().contains("regia/server"), "{err}");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[features]\nserver = [\"regia/server\"]\n",
        )
        .expect("write");
        check_server_feature(&dir, "demo").expect("declared");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2440: a split project (the `net-game` template) packages its `server/` crate, whose
    /// `server` feature is a marker the server build's `--features server` names.
    #[test]
    fn a_split_project_packages_its_server_crate() {
        let dir = regia_test_utils::unique_temp_dir("server-split-package");
        std::fs::create_dir_all(dir.join("server")).expect("dir");
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"server\"]\n").expect("write");
        let template = regia_templates::template_files("net-game")
            .and_then(|files| files.iter().find(|(p, _)| *p == "server/Cargo.toml"))
            .map(|(_, text)| {
                text.replace("{{crate_name}}", "arena")
                    .replace("{{engine}}", "../engine")
            })
            .expect("the template's server manifest");
        std::fs::write(dir.join("server/Cargo.toml"), &template).expect("write");
        let target = crate::commands::server::server_cargo_target(&dir).expect("target");
        assert_eq!(
            (target.package.as_str(), target.bin.as_str()),
            ("arena_server", "arena_server")
        );
        check_server_feature(&dir, "arena_server").expect("the template declares the marker");
        std::fs::write(
            dir.join("server/Cargo.toml"),
            "[package]\nname = \"arena_server\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        assert!(check_server_feature(&dir, "arena_server").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binary_with_a_client_type_path_fails() {
        let dir = regia_test_utils::unique_temp_dir("server-bin");
        std::fs::create_dir_all(&dir).expect("dir");
        let bin = dir.join("server");
        // A string literal that names a client type is not a link: `regia_scene` has some.
        std::fs::write(&bin, b"\0\0regia_net::session\0regia_audio::AudioEmitter\0").expect("write");
        check_binary_links_no_client(&bin).expect("clean");
        for linked in [
            &b"\0\0crates/regia_render/src/lib.rs\0"[..],
            b"\0\0_ZN4wgpu6device\0",
            b"\0\0/cargo/registry/src/x/winit-0.30.13/src/window.rs\0",
        ] {
            std::fs::write(&bin, linked).expect("write");
            assert!(check_binary_links_no_client(&bin).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_readme_lists_the_exit_codes_and_a_supervisor_example() {
        let text = readme_text("Arena", "arena-server");
        for needle in [
            "./arena-server",
            "Restart=on-failure",
            "SIGHUP",
            "70  crash",
            "/health",
            "mode 0600",
            "serverkey, joincode",
        ] {
            assert!(text.contains(needle), "{needle}");
        }
        assert!(text.contains("deploy/"));
        assert!(text.contains("tier.ron"));
    }

    #[test]
    fn server_package_writes_systemd_units() {
        let settings = UnitSettings::for_server_binary("arena-server");
        let units = systemd_units(&settings);
        assert_eq!(
            [units[0].0, units[1].0],
            ["regia-server.service", "regia-hub.service"]
        );
        for (name, text) in &units {
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("ironreach"), "{name}");
            for needle in [
                "After=network-online.target",
                "Wants=network-online.target",
                "Type=simple",
                "User=regia",
                "Group=regia",
                "WorkingDirectory=/opt/regia",
                "Restart=on-failure",
                "RestartSec=5",
                "NoNewPrivileges=true",
                "WantedBy=multi-user.target",
                "UDP ports above 1024",
            ] {
                assert!(text.contains(needle), "{name} missing {needle}");
            }
        }
        let server = &units[0].1;
        assert!(server.contains("ExecStart=/opt/regia/arena-server"));
        assert!(server.contains("REGIA_SERVER_PORT=7777"));
        assert!(units[1].1.contains(
            "ExecStart=/opt/regia/regia-lobby-hub --config /opt/regia/hub.toml --key-file /opt/regia/hub.key"
        ));
        assert!(units[1].1.contains("hub.toml"));
        assert!(
            !units[1].1.contains("Environment="),
            "the hub reads hub.toml, not the environment"
        );

        let dir = regia_test_utils::unique_temp_dir("server-units");
        std::fs::create_dir_all(&dir).expect("dir");
        write_deploy_units(&dir, &settings).expect("units");
        write_tier_ron(&dir).expect("tier");
        for name in ["regia-server.service", "regia-hub.service"] {
            assert!(dir.join("deploy").join(name).is_file(), "{name}");
        }
        assert!(
            !dir.join("deploy/regia-relay.service").exists(),
            "no relay unit: Regia ships no relay binary"
        );
        let tier = regia_framework::install_tier::read_tier(&dir.join("tier.ron")).expect("tier");
        assert_eq!(tier.tier, "low");
        assert_eq!(tier.texture_max_px, 1024);
        assert_eq!(tier.preset_max, "Medium");
    }

    /// The server package stages the hub binary, a default `hub.toml` and one README section.
    #[test]
    fn server_package_with_lobby_hub_stages_the_binary() {
        let dir = regia_test_utils::unique_temp_dir("lobby-hub-stage");
        let staging = dir.join("game-server");
        std::fs::create_dir_all(&staging).expect("staging");
        std::fs::write(
            staging.join("README-server.txt"),
            readme_text("Arena", "arena-server"),
        )
        .expect("readme");
        let stub = dir.join("regia-lobby-hub");
        std::fs::write(&stub, b"stub-binary").expect("stub");
        stage_lobby_hub_into(&staging, &stub).expect("stage");
        assert_eq!(
            std::fs::read(staging.join("regia-lobby-hub")).expect("binary"),
            b"stub-binary"
        );
        let toml = std::fs::read_to_string(staging.join("hub.toml")).expect("hub.toml");
        assert!(toml.contains("format_version = 1"), "{toml}");
        assert!(toml.contains("port = 7810"), "{toml}");
        let readme = std::fs::read_to_string(staging.join("README-server.txt")).expect("readme");
        assert!(readme.contains("Lobby hub"), "{readme}");
        assert!(readme.contains("hub.toml"), "{readme}");
        stage_lobby_hub_into(&staging, &stub).expect("stage again");
        let again = std::fs::read_to_string(staging.join("README-server.txt")).expect("readme");
        assert_eq!(again.matches("Lobby hub").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KI-NT-043 (ADR-8891): every unit under `deploy/` starts a binary that is in the package,
    /// and the hub unit's arguments parse with `regia-lobby-hub`'s own command line. It failed on
    /// the merge base: the relay unit started `/opt/regia/relay` and the hub unit
    /// `/opt/regia/hub --listen ...`, neither of which the package ships.
    #[test]
    fn every_deploy_unit_starts_a_binary_the_package_ships() {
        use clap::Parser as _;
        let dir = regia_test_utils::unique_temp_dir("server-units-shipped");
        let staging = dir.join("arena-server");
        std::fs::create_dir_all(&staging).expect("staging");
        std::fs::write(staging.join("arena-server"), b"server").expect("server");
        let stub = dir.join(lobby_hub_file_name(false));
        std::fs::write(&stub, b"hub").expect("stub");
        stage_lobby_hub_into(&staging, &stub).expect("stage");
        let settings = UnitSettings::for_server_binary("arena-server");
        write_deploy_units(&staging, &settings).expect("units");
        let mut units = 0;
        for entry in std::fs::read_dir(staging.join("deploy")).expect("deploy") {
            let path = entry.expect("entry").path();
            let text = std::fs::read_to_string(&path).expect("unit");
            let exec = text
                .lines()
                .find_map(|line| line.strip_prefix("ExecStart="))
                .expect("an ExecStart line");
            let argv: Vec<&str> = exec.split_whitespace().collect();
            let installed = argv[0]
                .strip_prefix(&format!("{}/", settings.prefix))
                .expect("under the install prefix");
            assert!(
                staging.join(installed).is_file(),
                "{} starts {installed}, which the package does not ship",
                path.display()
            );
            if installed == lobby_hub_file_name(false) {
                let args = regia_cli::lobby_hub_args::LobbyHubArgs::try_parse_from(argv.iter().copied())
                    .unwrap_or_else(|e| panic!("regia-lobby-hub refuses {exec}: {e}"));
                assert_eq!(args.config, std::path::Path::new("/opt/regia/hub.toml"));
                let config = args.config.file_name().expect("file name");
                assert!(staging.join(config).is_file(), "hub.toml is staged");
                assert_eq!(
                    args.key_file.as_deref(),
                    Some(std::path::Path::new("/opt/regia/hub.key"))
                );
                assert_eq!(args.port, None, "the port comes from hub.toml");
            }
            units += 1;
        }
        assert_eq!(units, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
