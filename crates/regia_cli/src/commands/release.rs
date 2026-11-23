// SPDX-License-Identifier: MPL-2.0
//! `regia release verify|diff|upload-plan|sign|upload` (ADR-8557, ADR-8687, ADR-8846).
//!
//! `verify` checks a release directory against its manifests and, when a `.sig` is present,
//! its Ed25519 signature. `diff` compares two releases. `upload-plan` prints an ordered plan
//! and does not contact a host. `sign` writes a detached signature. `upload` copies the plan
//! through a filesystem backend or runs a configured hosting CLI (ADR-8846). `--json` documents use schema 1. Error codes are listed in
//! `book/agents/release-verify-diff.md` and `book/agents/release-sign.md`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use regia_release_chunks::{
    BackendSpec, ChunkError, CliBackend, DiffOptions, FsBackend, UploadCliConfig, UploadPlanOptions,
    VerifyOptions, cli_dry_run, diff_release, format_markdown, format_text, parse_backend, resolve_secrets,
    sign_release, upload_plan_with, upload_with_backend, verify_release,
};
use serde_json::{Value, json};

/// Version of the `--json` documents this command prints.
pub const JSON_SCHEMA: u32 = 1;

/// `regia release` subcommands.
#[derive(Debug, Subcommand)]
#[command(
    after_long_help = "usage: regia release <COMMAND>\n\nExamples:\n  regia release verify DIR\n  regia release verify DIR --deep --require-signature --json\n  regia release diff OLD NEW\n  regia release upload-plan DIR\n  regia release sign DIR --key KEY --date YYYY-MM-DD\n  regia release upload DIR --backend fs:DEST\n  regia release upload DIR --backend cli:host.toml --dry-run\n\nExit codes:\n  verify exits 0 when the directory is clean and 1 when a check fails\n  diff exits 0 after it prints, including when the two releases differ\n  diff and upload-plan exit 2 when an input cannot be read\n  sign and upload exit 0 after they finish and 2 when an input cannot be used\n"
)]
pub enum ReleaseCommand {
    /// Check manifests against the files. `--deep` also checks archive members.
    Verify(VerifyArgs),
    /// Compare two release directories or manifest files.
    Diff(DiffArgs),
    /// Print an ordered upload plan as JSON. Chunks come first and manifests last.
    UploadPlan(UploadArgs),
    /// Sign each release manifest with an Ed25519 key. The date is an argument.
    Sign(SignArgs),
    /// Upload the plan into `fs:<dir>`, or through the hosting CLI of `cli:<config.toml>`.
    Upload(ReleaseUploadArgs),
}

/// `regia release verify`.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Release directory.
    pub dir: PathBuf,
    /// Decompress every chunk and check members, member hashes, and `content_key`.
    #[arg(long, default_value_t = false)]
    pub deep: bool,
    /// Tiers to check. Repeat the flag, or separate names with commas. Empty checks every tier.
    #[arg(long, value_delimiter = ',')]
    pub tiers: Vec<String>,
    /// Refuse a manifest that has no detached signature.
    #[arg(long, default_value_t = false)]
    pub require_signature: bool,
    /// Public key (`ed25519:` plus 64 hex digits) allowed to sign. Repeat the flag.
    /// Empty accepts the key embedded in the signature.
    #[arg(long = "trust-key")]
    pub trust_key: Vec<String>,
    /// Print the JSON document on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia release diff`.
#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Older release directory or manifest file.
    pub old: PathBuf,
    /// Newer release directory or manifest file.
    pub new: PathBuf,
    /// Keep manifests for this platform.
    #[arg(long)]
    pub platform: Option<String>,
    /// Keep manifests for this tier. An empty string is a real tier.
    #[arg(long)]
    pub tier: Option<String>,
    /// Print the JSON document on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Print a markdown table. Ignored when `--json` is set.
    #[arg(long, default_value_t = false)]
    pub markdown: bool,
}

/// `regia release upload-plan`.
#[derive(Debug, Args)]
pub struct UploadArgs {
    /// Release directory.
    pub dir: PathBuf,
    /// Record `dry_run` on the plan. This command never contacts a host.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Attempts recorded for a later uploader.
    #[arg(long, default_value_t = 3)]
    pub retries: u32,
}

/// `regia release sign`.
#[derive(Debug, Args)]
pub struct SignArgs {
    /// Release directory.
    pub dir: PathBuf,
    /// Private key file written by `regia assets keygen`. It must sit outside the project.
    #[arg(long)]
    pub key: PathBuf,
    /// Signature date, `YYYY-MM-DD`. The command does not read a clock.
    #[arg(long)]
    pub date: String,
    /// Project directory the key must stay out of.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print the JSON document on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia release upload`.
#[derive(Debug, Args)]
pub struct ReleaseUploadArgs {
    /// Release directory.
    pub dir: PathBuf,
    /// Backend: `fs:<directory>` copies into a local directory; `cli:<config.toml>` runs the
    /// hosting CLI that config names (ADR-8846).
    #[arg(long)]
    pub backend: String,
    /// Print the plan and do not write or run anything. With `cli:` it prints each command
    /// line and whether each credential variable is set.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Attempts per file for a `cli:` backend (at least 1). `fs:` does not retry.
    #[arg(long, default_value_t = 3)]
    pub retries: u32,
    /// Override a `vars` value of a `cli:` config, `key=value`. Repeat the flag.
    #[arg(long = "var")]
    pub var: Vec<String>,
    /// Print the JSON document on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Run one `regia release` subcommand.
///
/// Verify exits 0 when the report is clean and 1 when it names problems, including a missing
/// directory. Diff exits 0 after printing, even when the releases differ. Diff, upload-plan,
/// sign and upload exit 2 when an input cannot be used.
pub fn run(command: &ReleaseCommand) -> ExitCode {
    match command {
        ReleaseCommand::Verify(args) => run_verify(args),
        ReleaseCommand::Diff(args) => run_diff(args),
        ReleaseCommand::UploadPlan(args) => run_upload_plan(args),
        ReleaseCommand::Sign(args) => run_sign(args),
        ReleaseCommand::Upload(args) => run_upload(args),
    }
}

fn run_verify(args: &VerifyArgs) -> ExitCode {
    let report = verify_release(
        &args.dir,
        &VerifyOptions {
            deep: args.deep,
            tiers: args.tiers.clone(),
            require_signature: args.require_signature,
            trusted_keys: args.trust_key.clone(),
        },
    );
    let ok = report.ok();
    if args.json {
        print_json(&envelope("release verify", report.to_value(), ok));
    } else {
        println!("{}", report.render_human());
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

fn run_diff(args: &DiffArgs) -> ExitCode {
    let opts = DiffOptions {
        platform: args.platform.clone(),
        tier: args.tier.clone(),
    };
    match diff_release(&args.old, &args.new, &opts) {
        Ok(report) => {
            if args.json {
                print_json(&envelope("release diff", report.to_value(), true));
            } else if args.markdown {
                println!("{}", format_markdown(&report));
            } else {
                println!("{}", format_text(&report));
            }
            ExitCode::SUCCESS
        }
        Err(err) => fail("release diff", args.json, &err),
    }
}

fn run_upload_plan(args: &UploadArgs) -> ExitCode {
    let opts = UploadPlanOptions {
        dry_run: args.dry_run,
        retries: args.retries,
        ..UploadPlanOptions::default()
    };
    match upload_plan_with(&args.dir, &opts) {
        Ok(plan) => {
            print_json(&envelope("release upload-plan", plan.to_value(), true));
            ExitCode::SUCCESS
        }
        Err(err) => fail("release upload-plan", true, &err),
    }
}

fn run_sign(args: &SignArgs) -> ExitCode {
    if let Err(err) = super::assets_trust::refuse_key_in_project(&args.key, &args.project) {
        let message = err.to_string();
        let code = if message.starts_with("content_key_in_project") {
            "content_key_in_project"
        } else {
            "io_error"
        };
        return fail_text("release sign", args.json, code, &message);
    }
    match sign_release(&args.dir, &args.key, &args.date, Some(&args.project)) {
        Ok(report) => {
            let body = json!({ "files": report.files });
            if args.json {
                print_json(&envelope("release sign", body, true));
            } else {
                for file in &report.files {
                    println!("signed {file}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => fail("release sign", args.json, &err),
    }
}

fn run_upload(args: &ReleaseUploadArgs) -> ExitCode {
    match upload_document(args, &|name| std::env::var(name).ok()) {
        Ok((body, lines)) => {
            if args.json {
                print_json(&envelope("release upload", body, true));
            } else {
                for line in lines {
                    println!("{line}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => fail("release upload", args.json, &err),
    }
}

/// The `release upload` JSON body and its text lines. `lookup` reads environment variables
/// (credentials of a `cli:` backend, ADR-8846). Every line and field is already redacted.
fn upload_document(
    args: &ReleaseUploadArgs,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(Value, Vec<String>), ChunkError> {
    let opts = UploadPlanOptions {
        dry_run: args.dry_run,
        retries: args.retries,
        ..UploadPlanOptions::default()
    };
    match parse_backend(&args.backend)? {
        BackendSpec::Fs(dest) => {
            let mut backend = FsBackend::create(&dest)?;
            let receipt = upload_with_backend(&args.dir, &mut backend, &opts)?;
            let mut lines = Vec::new();
            if args.dry_run {
                lines.extend(receipt.skipped.iter().map(|name| format!("would put {name}")));
            } else {
                lines.extend(receipt.puts.iter().map(|name| format!("put {name}")));
                lines.extend(receipt.skipped.iter().map(|name| format!("skip {name}")));
                if receipt.published {
                    lines.push("published".to_string());
                }
            }
            let body = json!({
                "puts": receipt.puts,
                "skipped": receipt.skipped,
                "published": receipt.published,
                "backend": args.backend,
            });
            Ok((body, lines))
        }
        BackendSpec::Cli(config_path) => {
            let overrides = parse_vars(&args.var)?;
            let config = UploadCliConfig::load(&config_path)?.with_vars(&overrides)?;
            if args.dry_run {
                return cli_dry_run_document(args, &config, &opts, lookup);
            }
            let secrets = resolve_secrets(&config, lookup)?;
            let mut backend = CliBackend::new(config, &args.dir, secrets)?.with_attempts(args.retries);
            let receipt = upload_with_backend(&args.dir, &mut backend, &opts)?;
            let calls = backend.calls();
            let mut lines: Vec<String> = calls
                .iter()
                .map(|call| {
                    let status = call
                        .status
                        .map_or_else(|| "none".to_string(), |code| code.to_string());
                    format!(
                        "ran {} (status {status}, attempt {})",
                        display_argv(&call.argv),
                        call.attempt
                    )
                })
                .collect();
            lines.extend(receipt.skipped.iter().map(|name| format!("skip {name}")));
            if receipt.published {
                lines.push("published".to_string());
            }
            let body = json!({
                "puts": receipt.puts,
                "skipped": receipt.skipped,
                "published": receipt.published,
                "backend": args.backend,
                "calls": calls.iter().map(|call| json!({
                    "argv": call.argv,
                    "status": call.status,
                    "attempt": call.attempt,
                    "stdout": call.stdout,
                    "stderr": call.stderr,
                })).collect::<Vec<_>>(),
            });
            Ok((body, lines))
        }
    }
}

/// The plan of a `cli:` upload: the command lines it would run and whether each credential
/// variable is set. Starts nothing.
fn cli_dry_run_document(
    args: &ReleaseUploadArgs,
    config: &UploadCliConfig,
    opts: &UploadPlanOptions,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(Value, Vec<String>), ChunkError> {
    let plan = cli_dry_run(&args.dir, config, opts, lookup)?;
    let mut lines: Vec<String> = plan
        .commands
        .iter()
        .map(|argv| format!("would run {}", display_argv(argv)))
        .collect();
    for (name, set) in &plan.secret_env {
        lines.push(format!("secret {name} {}", if *set { "set" } else { "unset" }));
    }
    let body = json!({
        "puts": Vec::<String>::new(),
        "skipped": plan.files,
        "published": false,
        "backend": args.backend,
        "dry_run": true,
        "commands": plan.commands,
        "secret_env": plan
            .secret_env
            .iter()
            .map(|(name, set)| json!({ "name": name, "set": set }))
            .collect::<Vec<_>>(),
    });
    Ok((body, lines))
}

/// `--var key=value` pairs.
fn parse_vars(raw: &[String]) -> Result<Vec<(String, String)>, ChunkError> {
    raw.iter()
        .map(|item| {
            item.split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .ok_or_else(|| {
                    ChunkError::new("upload_cli_config", format!("--var {item:?} is not key=value"))
                })
        })
        .collect()
}

/// A command line for a person to read: arguments with spaces or quotes are single-quoted.
fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn envelope(command: &str, mut body: Value, ok: bool) -> Value {
    if let Some(map) = body.as_object_mut() {
        map.insert("schema".into(), json!(JSON_SCHEMA));
        map.insert("ok".into(), json!(ok));
        map.insert("command".into(), json!(command));
    }
    body
}

fn print_json(doc: &Value) {
    match serde_json::to_string_pretty(doc) {
        Ok(text) => println!("{text}"),
        Err(err) => eprintln!("io_error: cannot encode JSON: {err}"),
    }
}

fn fail(command: &str, json_mode: bool, err: &ChunkError) -> ExitCode {
    fail_text(command, json_mode, err.code, &err.message)
}

fn fail_text(command: &str, json_mode: bool, code: &str, message: &str) -> ExitCode {
    if json_mode {
        print_json(&json!({
            "schema": JSON_SCHEMA,
            "ok": false,
            "command": command,
            "code": code,
            "message": message,
        }));
    } else {
        eprintln!("{code}: {message}");
    }
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use clap::{Command, Subcommand};

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-release-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn long_help_names_usage_examples_and_exit_codes() {
        let mut command = ReleaseCommand::augment_subcommands(Command::new("release"));
        let mut buf = Vec::new();
        command.write_long_help(&mut buf).expect("help");
        let text = String::from_utf8(buf).expect("utf-8");
        assert!(text.contains("usage:"), "{text}");
        assert!(text.contains("Examples:"), "{text}");
        assert!(text.contains("Exit codes:"), "{text}");
        assert!(text.contains("regia release sign"), "{text}");
        assert!(text.contains("regia release upload"), "{text}");
    }

    #[test]
    fn verify_of_a_missing_directory_exits_1() {
        let missing = scratch("missing").join("absent");
        let code = run(&ReleaseCommand::Verify(VerifyArgs {
            dir: missing,
            deep: false,
            tiers: Vec::new(),
            require_signature: false,
            trust_key: Vec::new(),
            json: true,
        }));
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn diff_of_a_missing_path_exits_2_and_a_pair_exits_0() {
        let missing = scratch("diff-missing").join("absent.json");
        let code = run(&ReleaseCommand::Diff(DiffArgs {
            old: missing.clone(),
            new: missing,
            platform: None,
            tier: None,
            json: true,
            markdown: false,
        }));
        assert_eq!(code, ExitCode::from(2));

        let dir = scratch("diff-ok");
        let body = r#"{"format":1,"platform":"linux","tier":"low","version":"1.0.0","chunks":[{"name":"core","asset":"linux-core.tar.zst","size":1,"unpacked":1,"files":["bin/app"]}],"links":[]}"#;
        let old = dir.join("manifest-linux-low.json");
        let new = dir.join("manifest-linux-low-next.json");
        std::fs::write(&old, body).expect("old");
        std::fs::write(&new, body).expect("new");
        let code = run(&ReleaseCommand::Diff(DiffArgs {
            old,
            new,
            platform: None,
            tier: None,
            json: true,
            markdown: false,
        }));
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn upload_plan_of_a_missing_directory_exits_2() {
        let missing = scratch("upload-missing").join("absent");
        let code = run(&ReleaseCommand::UploadPlan(UploadArgs {
            dir: missing,
            dry_run: false,
            retries: 3,
        }));
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn sign_refuses_a_key_inside_the_project_and_a_missing_directory() {
        let project = scratch("sign-project");
        std::fs::write(project.join("Regia.toml"), "name = \"lane\"\n").expect("toml");
        let inside = project.join("secret.key");
        std::fs::write(&inside, b"not a signing key\n").expect("key");
        let dir = scratch("sign-dir");
        std::fs::write(dir.join("manifest-demo.json"), b"{}\n").expect("manifest");
        let code = run(&ReleaseCommand::Sign(SignArgs {
            dir,
            key: inside,
            date: "2026-11-18".to_string(),
            project: project.clone(),
            json: true,
        }));
        assert_eq!(code, ExitCode::from(2));

        let keys = scratch("sign-keys");
        let key = keys.join("release.key");
        std::fs::write(&key, b"not a signing key\n").expect("outside");
        let missing = scratch("sign-missing").join("absent");
        let code = run(&ReleaseCommand::Sign(SignArgs {
            dir: missing,
            key,
            date: "2026-11-18".to_string(),
            project,
            json: true,
        }));
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn upload_rejects_a_backend_that_is_not_fs() {
        let dir = scratch("upload-backend");
        let code = run(&ReleaseCommand::Upload(ReleaseUploadArgs {
            dir,
            backend: "https://example.invalid/release".to_string(),
            dry_run: true,
            retries: 3,
            var: Vec::new(),
            json: true,
        }));
        assert_eq!(code, ExitCode::from(2));
    }

    fn cli_release(tag: &str) -> (PathBuf, PathBuf) {
        let dir = scratch(tag);
        std::fs::write(dir.join("core.tar.zst"), b"chunk").expect("chunk");
        std::fs::write(dir.join("manifest-demo.json"), b"{\"format\":1}\n").expect("manifest");
        let config_dir = scratch(&format!("{tag}-config"));
        let config = config_dir.join("host.toml");
        std::fs::write(
            &config,
            "upload_cli = 1\nprogram = \"hosting-cli-that-is-never-started\"\n\
             put = [\"push\", \"{file}\", \"{target}:{channel}\"]\npublish = [\"publish\", \"{target}\"]\n\
             secret_env = [\"REGIA_TEST_HOST_TOKEN\"]\n[vars]\ntarget = \"studio/game\"\nchannel = \"beta\"\n",
        )
        .expect("config");
        (dir, config)
    }

    fn cli_args(
        dir: &std::path::Path,
        config: &std::path::Path,
        dry_run: bool,
        var: &[&str],
    ) -> ReleaseUploadArgs {
        ReleaseUploadArgs {
            dir: dir.to_path_buf(),
            backend: format!("cli:{}", config.display()),
            dry_run,
            retries: 3,
            var: var.iter().map(|v| (*v).to_string()).collect(),
            json: true,
        }
    }

    const TEST_TOKEN: &str = "tok-0123456789";

    fn token(name: &str) -> Option<String> {
        (name == "REGIA_TEST_HOST_TOKEN").then(|| TEST_TOKEN.to_string())
    }

    #[test]
    fn upload_cli_dry_run_prints_the_commands_and_starts_nothing() {
        let (dir, config) = cli_release("cli-dry");
        let args = cli_args(&dir, &config, true, &["channel=linux"]);
        let (body, lines) = upload_document(&args, &token).expect("dry run");
        assert_eq!(body["dry_run"], json!(true));
        assert_eq!(body["published"], json!(false));
        let core = dir.join("core.tar.zst").display().to_string();
        let manifest = dir.join("manifest-demo.json").display().to_string();
        assert_eq!(
            body["commands"],
            json!([
                [
                    "hosting-cli-that-is-never-started",
                    "push",
                    core,
                    "studio/game:linux"
                ],
                [
                    "hosting-cli-that-is-never-started",
                    "push",
                    manifest,
                    "studio/game:linux"
                ],
                ["hosting-cli-that-is-never-started", "publish", "studio/game"],
            ])
        );
        assert_eq!(
            body["secret_env"],
            json!([{ "name": "REGIA_TEST_HOST_TOKEN", "set": true }])
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("would run hosting-cli-that-is-never-started push")),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"secret REGIA_TEST_HOST_TOKEN set".to_string()),
            "{lines:?}"
        );
        let (body, lines) = upload_document(&args, &|_| None).expect("dry run without the credential");
        assert_eq!(body["secret_env"][0]["set"], json!(false));
        assert!(lines.contains(&"secret REGIA_TEST_HOST_TOKEN unset".to_string()));
        assert!(!body.to_string().contains(TEST_TOKEN));
    }

    #[test]
    fn upload_cli_refuses_a_credential_in_the_config_or_a_var() {
        let (dir, config) = cli_release("cli-secret");
        let var = format!("channel={TEST_TOKEN}");
        for dry_run in [true, false] {
            let args = cli_args(&dir, &config, dry_run, &[var.as_str()]);
            let err = upload_document(&args, &token).expect_err("refused");
            assert_eq!(err.code, "upload_cli_secret", "{err}");
            assert!(!err.message.contains(TEST_TOKEN), "{err}");
        }
        let args = cli_args(&dir, &config, true, &["nope=1"]);
        assert_eq!(
            upload_document(&args, &token).expect_err("undefined").code,
            "upload_cli_config"
        );
        let args = cli_args(&dir, &config, true, &["no-equals"]);
        assert_eq!(
            upload_document(&args, &token).expect_err("malformed").code,
            "upload_cli_config"
        );
    }

    #[test]
    fn upload_cli_real_run_needs_the_credential_and_the_program() {
        let (dir, config) = cli_release("cli-env");
        let args = cli_args(&dir, &config, false, &[]);
        assert_eq!(
            upload_document(&args, &|_| None).expect_err("unset").code,
            "upload_cli_env"
        );
        assert_eq!(
            upload_document(&args, &token).expect_err("no such program").code,
            "upload_cli_spawn"
        );
        let code = run(&ReleaseCommand::Upload(cli_args(
            &dir,
            &dir.join("missing.toml"),
            true,
            &[],
        )));
        assert_eq!(code, ExitCode::from(2));
    }
}
