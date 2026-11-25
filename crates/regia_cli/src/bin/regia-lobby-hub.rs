// SPDX-License-Identifier: MPL-2.0
//! The lobby hub process (NL-06, ADR-8648; the relay is NL-11, ADR-10397). Loads `hub.toml`,
//! serves until SIGTERM or SIGINT, and drains on the way out. When `relay.enabled` is set it
//! also polls a relay on `relay.port`.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use regia_cli::lobby_hub_args::LobbyHubArgs as Args;
use regia_net::UdpTransport;
use regia_net::crypto::ServerIdentity;
use regia_net::lobby::hub::{HubDocument, HubProcess, load_ban_keys, validate_key_path};
use regia_net::relay::{RelayConfig, RelayServer};
use regia_net::server_host::health::HealthServer;
use regia_net::server_host::signals::HostSignals;

enum Stop {
    Config(String),
    Bind(String),
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Stop::Config(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
        Err(Stop::Bind(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(3)
        }
    }
}

fn run(args: Args) -> Result<(), Stop> {
    let mut doc = load_document(&args.config)?;
    if let Some(bind) = args.bind {
        doc.file.bind = bind;
    }
    if let Some(port) = args.port {
        if port == 0 {
            return Err(Stop::Config("port: expected an integer 1..=65535".into()));
        }
        doc.file.port = port;
    }
    if let Some(path) = args.key_file {
        doc.file.key_path = path.display().to_string();
    }
    let listen = doc.file.listen_addr().map_err(|e| Stop::Config(e.to_string()))?;
    let status = doc.file.status_addr().map_err(|e| Stop::Config(e.to_string()))?;
    let relay_addr = doc.relay_addr().map_err(|e| Stop::Config(e.to_string()))?;
    let key_path = doc.file.resolved_key_path();
    validate_key_path(&key_path).map_err(|e| Stop::Config(e.to_string()))?;
    if let Some(parent) = key_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            Stop::Config(format!(
                "key_path: expected a directory that can be created ({e})"
            ))
        })?;
    }
    let log = open_log(Path::new(&doc.log.dir))?;
    let identity = ServerIdentity::load_or_create(&key_path)
        .map_err(|e| Stop::Config(format!("key_path: expected a server key file ({e})")))?;
    let bans = load_ban_keys(Path::new(&doc.bans.path)).map_err(|e| Stop::Config(e.to_string()))?;
    let socket = UdpTransport::bind(listen)
        .map_err(|e| Stop::Bind(format!("bind: expected a free UDP port ({e})")))?;
    let health = match status {
        Some(addr) => Some(start_health(addr)?),
        None => None,
    };
    let mut process = HubProcess::new(socket, identity, &doc.file);
    process
        .host_mut()
        .core_mut()
        .set_operator_bans(&bans)
        .map_err(|e| Stop::Config(format!("bans.path: expected at most 4096 player keys ({e})")))?;
    if let Some(addr) = relay_addr {
        let mut relay = open_relay(addr, &doc)?;
        relay.set_source_rate(doc.relay.per_address_per_sec);
        process.attach_relay(relay);
    }
    let signals = HostSignals::new();
    if let Err(err) = signals.install_os_handlers() {
        eprintln!("signals: {err}");
    }
    let relay_note = match relay_addr {
        Some(addr) => format!(", relay on {addr}"),
        None => String::new(),
    };
    eprintln!(
        "regia-lobby-hub listening on {listen}{relay_note}, key {}, {} bans",
        key_path.display(),
        bans.len()
    );
    log.note(&format!("listening on {listen}{relay_note}, bans {}", bans.len()));
    serve(&mut process, health, &signals, &log);
    Ok(())
}

fn load_document(path: &std::path::Path) -> Result<HubDocument, Stop> {
    if !path.is_file() {
        eprintln!("{} is missing; using the built-in hub.toml", path.display());
        return HubDocument::from_toml(regia_net::lobby::hub::DEFAULT_HUB_TOML)
            .map_err(|errors| Stop::Config(join_errors(&errors)));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Stop::Config(format!("hub.toml: expected a readable file ({e})")))?;
    HubDocument::from_toml(&text).map_err(|errors| Stop::Config(join_errors(&errors)))
}

fn open_relay(addr: SocketAddr, doc: &HubDocument) -> Result<RelayServer<UdpTransport>, Stop> {
    let config = RelayConfig {
        allocation_ports: doc.relay.allocation_cap,
        ..RelayConfig::default()
    };
    let mut seed_bytes = [0u8; 8];
    // The seed only starts the join-code counter. The codes themselves come from the relay secret.
    seed_bytes[..2].copy_from_slice(&doc.file.port.to_le_bytes());
    RelayServer::bind(addr, config, u64::from_le_bytes(seed_bytes))
        .map_err(|e| Stop::Bind(format!("relay.bind: expected a free UDP port ({e})")))
}

/// A best-effort append log. Creating the directory is a configuration error. A later write
/// that fails is ignored: the hub keeps serving.
struct HubLog {
    file: Option<std::fs::File>,
}

fn open_log(dir: &Path) -> Result<HubLog, Stop> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Stop::Config(format!("log.dir: expected a directory that can be created ({e})")))?;
    let path = dir.join("hub.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| Stop::Config(format!("log.dir: expected a writable hub.log ({e})")))?;
    Ok(HubLog { file: Some(file) })
}

impl HubLog {
    fn note(&self, line: &str) {
        let Some(mut file) = self.file.as_ref().and_then(|f| f.try_clone().ok()) else {
            return;
        };
        let _ = writeln!(file, "{line}");
    }
}

fn join_errors(errors: &[regia_net::lobby::hub::HubFieldError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn start_health(addr: SocketAddr) -> Result<HealthServer, Stop> {
    HealthServer::start(addr)
        .map_err(|e| Stop::Bind(format!("status.bind: expected a free TCP port for /health ({e})")))
}

fn serve(
    process: &mut HubProcess<UdpTransport>,
    health: Option<HealthServer>,
    signals: &HostSignals,
    log: &HubLog,
) {
    let start = Instant::now();
    loop {
        if signals.take_shutdown() {
            if process.note_shutdown() {
                break;
            }
            signals.mark_draining();
            log.note("draining");
        }
        let now_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        process.update(now_ms);
        if let Some(endpoint) = &health {
            endpoint.publish(process.health_snapshot());
        }
        if process.finished_draining() {
            break;
        }
        let _ = signals.take_reload();
        std::thread::sleep(Duration::from_millis(2));
    }
    log.note("stopped");
    signals.mark_stopped();
}
