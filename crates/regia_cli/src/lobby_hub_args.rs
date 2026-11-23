// SPDX-License-Identifier: MPL-2.0
//! The command line of `regia-lobby-hub` (NL-06, ADR-8648). It lives in the library so the server
//! package's unit test parses the `regia-hub.service` ExecStart with the same parser the binary
//! runs (KI-NT-043, ADR-8891).

use std::path::PathBuf;

use clap::Parser;

/// `regia-lobby-hub [--config PATH] [--bind ADDR] [--port N] [--key-file PATH]`.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "regia-lobby-hub", about = "Self-hosted lobby hub")]
pub struct LobbyHubArgs {
    /// `hub.toml` to load. A missing file uses the built-in defaults.
    #[arg(long, default_value = "hub.toml")]
    pub config: PathBuf,
    /// UDP listen address. Overrides `bind` in the file.
    #[arg(long)]
    pub bind: Option<String>,
    /// UDP listen port. Overrides `port` in the file.
    #[arg(long)]
    pub port: Option<u16>,
    /// Server key file. Created on first run. Overrides `key_path`. Refused inside a project.
    #[arg(long)]
    pub key_file: Option<PathBuf>,
}
