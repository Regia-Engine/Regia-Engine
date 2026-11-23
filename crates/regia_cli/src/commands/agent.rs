// SPDX-License-Identifier: MPL-2.0
//! `regia agent call <method> [PARAMS_JSON] [--project DIR] [--out FILE.png]`
//! (`docs/specs/23-agent-native.md` § 8 M2).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_agent::client::Client;
use serde_json::Value;

/// Arguments of `regia agent call`.
#[derive(Debug, Clone, Args)]
pub struct AgentCallArgs {
    /// Protocol method, e.g. `agent.hello`.
    pub method: String,
    /// JSON parameters (default `{}`).
    #[arg(default_value = "{}")]
    pub params_json: String,
    /// Project directory (default current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Write a screenshot PNG to this file instead of printing base64.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// Runs `regia agent call`.
pub fn call(args: &AgentCallArgs) -> Result<()> {
    let params: Value = serde_json::from_str(&args.params_json)
        .with_context(|| format!("invalid PARAMS_JSON: {}", args.params_json))?;
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    let mut client =
        Client::connect_project(&project_dir).context("cannot connect to a running Regia Engine app")?;
    let result = client.call(&args.method, params).context("method call failed")?;
    if let Some(out_path) = &args.out {
        let b64 = match result.get("png_base64").and_then(Value::as_str) {
            Some(b64) => b64,
            None => {
                bail!(
                    "method {} did not return png_base64; use --out only with seeing.screenshot",
                    args.method
                )
            }
        };
        let png_bytes = decode_base64(b64).map_err(|e| anyhow::anyhow!("cannot decode base64 png: {e}"))?;
        std::fs::write(out_path, &png_bytes)
            .with_context(|| format!("cannot write {}", out_path.display()))?;
        let width = result.get("width").and_then(Value::as_u64).unwrap_or(0);
        let height = result.get("height").and_then(Value::as_u64).unwrap_or(0);
        println!("{width}x{height} {}", out_path.display());
    } else {
        let formatted = serde_json::to_string_pretty(&result).context("cannot serialize result")?;
        println!("{formatted}");
    }
    Ok(())
}

/// Decodes a `png_base64` field of a screenshot result.
///
/// # Errors
///
/// Invalid base64.
pub(crate) fn decode_base64_png(b64: &str) -> Result<Vec<u8>> {
    decode_base64(b64).map_err(|e| anyhow::anyhow!("cannot decode base64 png: {e}"))
}

use regia_cli::session::decode_base64;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: AgentCallArgs,
    }

    #[test]
    fn agent_call_parsing() {
        let args = Wrapper::try_parse_from([
            "agent-call",
            "agent.hello",
            r#"{"name":"test"}"#,
            "--project",
            "/tmp/p",
        ])
        .expect("parses")
        .args;
        assert_eq!(args.method, "agent.hello");
        assert_eq!(args.params_json, r#"{"name":"test"}"#);
        assert_eq!(args.project, PathBuf::from("/tmp/p"));
        assert_eq!(args.out, None);
    }

    #[test]
    fn agent_call_defaults() {
        let args = Wrapper::try_parse_from(["agent-call", "logs.read"])
            .expect("parses")
            .args;
        assert_eq!(args.method, "logs.read");
        assert_eq!(args.params_json, "{}");
        assert_eq!(args.project, PathBuf::from("."));
    }

    #[test]
    fn agent_call_with_out() {
        let args = Wrapper::try_parse_from(["agent-call", "seeing.screenshot", "--out", "shot.png"])
            .expect("parses")
            .args;
        assert_eq!(args.out, Some(PathBuf::from("shot.png")));
    }

    #[test]
    fn agent_call_bad_json_is_error() {
        let args = AgentCallArgs {
            method: "x".into(),
            params_json: "not json".into(),
            project: PathBuf::from("."),
            out: None,
        };
        let err = call(&args).expect_err("CLI call should fail");
        assert!(format!("{err}").contains("invalid PARAMS_JSON"));
    }

    #[test]
    fn agent_call_no_running_app_is_error() {
        let dir = std::env::temp_dir().join(format!("regia-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let args = AgentCallArgs {
            method: "agent.hello".into(),
            params_json: "{}".into(),
            project: dir.clone(),
            out: None,
        };
        let err = call(&args).expect_err("CLI call should fail");
        let message = format!("{err:#}");
        assert!(message.contains("no running Regia Engine app found"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_base64_rfc4648_vectors() {
        for (input, expected) in [
            ("", &b""[..]),
            ("Zg==", b"f"),
            ("Zm8=", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg==", b"foob"),
            ("Zm9vYmE=", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
        ] {
            assert_eq!(decode_base64(input).expect("decodes"), expected, "input: {input}");
        }
    }

    #[test]
    fn decode_base64_rejects_malformed_input() {
        for input in ["A", "Zm9vY", "!", "Zm=v", "Zm9v!!!!"] {
            assert!(decode_base64(input).is_err(), "input: {input}");
        }
    }
}
