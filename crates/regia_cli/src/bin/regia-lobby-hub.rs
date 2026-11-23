// SPDX-License-Identifier: MPL-2.0
//! The lobby hub process (NL-06, ADR-8648). Loads `hub.toml`, serves until SIGTERM or SIGINT, and
//! drains on the way out.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use regia_cli::lobby_hub_args::LobbyHubArgs as Args;
use regia_net::UdpTransport;
use regia_net::crypto::ServerIdentity;
use regia_net::lobby::hub::{HubFile, HubProcess, validate_key_path};
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
    let mut file = load_file(&args.config)?;
    if let Some(bind) = args.bind {
        file.bind = bind;
    }
    if let Some(port) = args.port {
        if port == 0 {
            return Err(Stop::Config("port: expected an integer 1..=65535".into()));
        }
        file.port = port;
    }
    if let Some(path) = args.key_file {
        file.key_path = path.display().to_string();
    }
    let listen = file.listen_addr().map_err(|e| Stop::Config(e.to_string()))?;
    let status = file.status_addr().map_err(|e| Stop::Config(e.to_string()))?;
    let key_path = file.resolved_key_path();
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
    let identity = ServerIdentity::load_or_create(&key_path)
        .map_err(|e| Stop::Config(format!("key_path: expected a server key file ({e})")))?;
    let socket = UdpTransport::bind(listen)
        .map_err(|e| Stop::Bind(format!("bind: expected a free UDP port ({e})")))?;
    let health = match status {
        Some(addr) => Some(start_health(addr)?),
        None => None,
    };
    let mut process = HubProcess::new(socket, identity, &file);
    let signals = HostSignals::new();
    if let Err(err) = signals.install_os_handlers() {
        eprintln!("signals: {err}");
    }
    eprintln!(
        "regia-lobby-hub listening on {listen}, key {}",
        key_path.display()
    );
    serve(&mut process, health, &signals);
    Ok(())
}

fn load_file(path: &std::path::Path) -> Result<HubFile, Stop> {
    if !path.is_file() {
        eprintln!("{} is missing; using the built-in hub.toml", path.display());
        return HubFile::from_toml(regia_net::lobby::hub::DEFAULT_HUB_TOML)
            .map_err(|errors| Stop::Config(join_errors(&errors)));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Stop::Config(format!("hub.toml: expected a readable file ({e})")))?;
    HubFile::from_toml(&text).map_err(|errors| Stop::Config(join_errors(&errors)))
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

fn serve(process: &mut HubProcess<UdpTransport>, health: Option<HealthServer>, signals: &HostSignals) {
    let start = Instant::now();
    loop {
        if signals.take_shutdown() {
            if process.is_draining() {
                break;
            }
            process.begin_drain();
            signals.mark_draining();
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
    signals.mark_stopped();
}
