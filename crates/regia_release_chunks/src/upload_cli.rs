// SPDX-License-Identifier: MPL-2.0
//! Hosting-CLI upload backend (ADR-8846).
//!
//! A release host is reached through its own command-line uploader, run as a child process.
//! The program and its argument templates come from a small TOML file (`upload_cli = 1`). The
//! file holds no credentials: a credential reaches the child only through an environment
//! variable the file names in `secret_env`. Every value of such a variable is replaced by
//! `<redacted:NAME>` in every command line, output line, receipt and error this module returns,
//! and a config whose text contains a credential value is refused (`upload_cli_secret`). A dry
//! run prints the command lines and starts nothing.
//!
//! The ADR-8687 requirements at pin `c9513623f` (`regia-absorb-4`) name a hosted upload. This
//! module is original Rust; no script or tool text was copied.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::ChunkError;
use crate::upload::{RemoteFile, UploadBackend};

/// Version key `upload_cli` of the config this build reads.
pub const UPLOAD_CLI_FORMAT: u32 = 1;

/// Largest config file read.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// Largest child output kept per stream. More is read and dropped.
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of redacted output kept on a [`CliCall`] and in an error message.
const OUTPUT_TAIL_BYTES: usize = 2048;
/// Child time limit when the config sets none: 30 minutes.
const DEFAULT_TIMEOUT_S: u64 = 1800;
/// Largest `timeout_s`: one day.
const MAX_TIMEOUT_S: u64 = 86_400;
/// Secret values shorter than this are not redacted or searched (they would match ordinary text).
pub const MIN_SECRET_LEN: usize = 4;
/// Placeholders every template may use besides the config's `vars`.
const BUILTIN_PLACEHOLDERS: [&str; 3] = ["file", "name", "dir"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    upload_cli: u32,
    program: String,
    put: Vec<String>,
    #[serde(default)]
    list: Vec<String>,
    #[serde(default)]
    publish: Vec<String>,
    #[serde(default)]
    secret_env: Vec<String>,
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default)]
    timeout_s: Option<u64>,
}

/// A parsed `upload_cli = 1` config.
///
/// Templates are argument lists. `{file}` is the local file path, `{name}` its base name,
/// `{dir}` the release directory, and `{key}` a value of `vars`. `put` must use `{file}`;
/// `list` and `publish` may not use `{file}` or `{name}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadCliConfig {
    /// Program to run: a name looked up on `PATH`, or a path.
    pub program: String,
    /// Arguments for one file.
    pub put: Vec<String>,
    /// Arguments that print `SIZE NAME` lines of what the host already holds. Empty: nothing listed.
    pub list: Vec<String>,
    /// Arguments run once after every `put`. Empty: no publish step.
    pub publish: Vec<String>,
    /// Environment variables that hold credentials. Each must be set for a real upload.
    pub secret_env: Vec<String>,
    /// Template values, overridable with [`UploadCliConfig::with_vars`].
    pub vars: BTreeMap<String, String>,
    /// Time limit for one child process.
    pub timeout: Duration,
}

impl UploadCliConfig {
    /// Parse config text.
    ///
    /// # Errors
    ///
    /// `upload_cli_config` for a TOML error, an unknown key, another `upload_cli` version, an
    /// empty program, a `put` without `{file}`, an unknown placeholder, a bad variable name or a
    /// `timeout_s` outside 1 to 86400.
    pub fn parse(text: &str) -> Result<Self, ChunkError> {
        let file: ConfigFile =
            toml::from_str(text).map_err(|err| config_error(format!("not an upload_cli config: {err}")))?;
        if file.upload_cli != UPLOAD_CLI_FORMAT {
            return Err(config_error(format!(
                "upload_cli {} is not read by this build (reads {UPLOAD_CLI_FORMAT})",
                file.upload_cli
            )));
        }
        if file.program.trim().is_empty() {
            return Err(config_error("program is empty"));
        }
        if file.program.contains('{') || file.program.contains('}') {
            return Err(config_error("program takes no placeholders"));
        }
        for key in file.vars.keys() {
            if !is_var_name(key) || BUILTIN_PLACEHOLDERS.contains(&key.as_str()) {
                return Err(config_error(format!(
                    "vars key {key:?} must be lowercase ascii, digits and _, and not file, name or dir"
                )));
            }
        }
        for name in &file.secret_env {
            if !is_env_name(name) {
                return Err(config_error(format!(
                    "secret_env entry {name:?} must be uppercase ascii, digits and _"
                )));
            }
        }
        if file.put.is_empty() {
            return Err(config_error("put is empty"));
        }
        let timeout_s = file.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S);
        if timeout_s == 0 || timeout_s > MAX_TIMEOUT_S {
            return Err(config_error(format!(
                "timeout_s {timeout_s} is outside 1 to {MAX_TIMEOUT_S}"
            )));
        }
        let mut put_has_file = false;
        for arg in &file.put {
            put_has_file |= placeholders(arg)?.iter().any(|p| p == "file");
        }
        if !put_has_file {
            return Err(config_error("put must pass {file}"));
        }
        for (field, args, allowed_file) in [
            ("put", &file.put, true),
            ("list", &file.list, false),
            ("publish", &file.publish, false),
        ] {
            for arg in args {
                for name in placeholders(arg)? {
                    let builtin = BUILTIN_PLACEHOLDERS.contains(&name.as_str());
                    if builtin && !allowed_file && name != "dir" {
                        return Err(config_error(format!("{field} may not use {{{name}}}")));
                    }
                    if !builtin && !file.vars.contains_key(&name) {
                        return Err(config_error(format!(
                            "{field} uses {{{name}}}, which vars does not define"
                        )));
                    }
                }
            }
        }
        Ok(Self {
            program: file.program,
            put: file.put,
            list: file.list,
            publish: file.publish,
            secret_env: file.secret_env,
            vars: file.vars,
            timeout: Duration::from_secs(timeout_s),
        })
    }

    /// Read and parse a config file.
    ///
    /// # Errors
    ///
    /// `io_error` when the file cannot be read, `upload_cli_config` when it is too large or
    /// does not parse.
    pub fn load(path: &Path) -> Result<Self, ChunkError> {
        let meta = fs::metadata(path).map_err(|err| ChunkError::io(path, &err))?;
        if meta.len() > MAX_CONFIG_BYTES {
            return Err(config_error(format!(
                "{} is larger than {MAX_CONFIG_BYTES} bytes",
                path.display()
            )));
        }
        let text = fs::read_to_string(path).map_err(|err| ChunkError::io(path, &err))?;
        Self::parse(&text)
    }

    /// Replace `vars` values. Each override must name a key the config defines.
    ///
    /// # Errors
    ///
    /// `upload_cli_config` for an undefined key.
    pub fn with_vars(mut self, overrides: &[(String, String)]) -> Result<Self, ChunkError> {
        for (key, value) in overrides {
            let Some(slot) = self.vars.get_mut(key) else {
                return Err(config_error(format!(
                    "--var {key} is not a vars key of the config"
                )));
            };
            slot.clone_from(value);
        }
        Ok(self)
    }

    /// Refuse a config whose program, templates or vars contain the value of a `secret_env`
    /// variable. The message names the variable and the field, never the value.
    ///
    /// # Errors
    ///
    /// `upload_cli_secret`.
    pub fn check_no_secret_text(&self, secrets: &[(String, String)]) -> Result<(), ChunkError> {
        let mut fields: Vec<(String, &str)> = vec![("program".to_string(), self.program.as_str())];
        for (field, args) in [
            ("put", &self.put),
            ("list", &self.list),
            ("publish", &self.publish),
        ] {
            fields.extend(args.iter().map(|arg| (field.to_string(), arg.as_str())));
        }
        fields.extend(
            self.vars
                .iter()
                .map(|(key, value)| (format!("vars.{key}"), value.as_str())),
        );
        for (name, value) in secrets {
            if value.len() < MIN_SECRET_LEN {
                continue;
            }
            if let Some((field, _)) = fields.iter().find(|(_, text)| text.contains(value.as_str())) {
                return Err(ChunkError::new(
                    "upload_cli_secret",
                    format!(
                        "{field} holds the value of {name}; keep credentials in the environment only, never in the config"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The command lines an upload of `names` from `dir` runs, in order: `list` (when set),
    /// one `put` per name, then `publish` (when set). Program first in each.
    ///
    /// # Errors
    ///
    /// `unsafe_path` when a path is not UTF-8.
    pub fn plan_commands(&self, dir: &Path, names: &[String]) -> Result<Vec<Vec<String>>, ChunkError> {
        let mut out = Vec::new();
        if !self.list.is_empty() {
            out.push(self.argv(&self.list, dir, None)?);
        }
        for name in names {
            out.push(self.argv(&self.put, dir, Some((name, &dir.join(name))))?);
        }
        if !self.publish.is_empty() {
            out.push(self.argv(&self.publish, dir, None)?);
        }
        Ok(out)
    }

    fn argv(
        &self,
        template: &[String],
        dir: &Path,
        file: Option<(&str, &Path)>,
    ) -> Result<Vec<String>, ChunkError> {
        let dir_text = utf8(dir)?;
        let mut values: BTreeMap<&str, String> = BTreeMap::new();
        values.insert("dir", dir_text.to_string());
        if let Some((name, path)) = file {
            values.insert("name", name.to_string());
            values.insert("file", utf8(path)?.to_string());
        }
        for (key, value) in &self.vars {
            values.insert(key.as_str(), value.clone());
        }
        let mut argv = vec![self.program.clone()];
        for arg in template {
            argv.push(expand(arg, &values)?);
        }
        Ok(argv)
    }
}

/// The values of `secret_env`, read through `lookup` (normally `std::env::var`).
///
/// # Errors
///
/// `upload_cli_env` naming the first variable that is unset or empty.
pub fn resolve_secrets(
    config: &UploadCliConfig,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, ChunkError> {
    let mut out = Vec::new();
    for name in &config.secret_env {
        match lookup(name) {
            Some(value) if !value.is_empty() => out.push((name.clone(), value)),
            _ => {
                return Err(ChunkError::new(
                    "upload_cli_env",
                    format!("{name} is not set; the config names it in secret_env"),
                ));
            }
        }
    }
    Ok(out)
}

/// Replace every secret value of at least [`MIN_SECRET_LEN`] bytes in `text` with
/// `<redacted:NAME>`. Longer values are replaced first.
#[must_use]
pub fn redact(text: &str, secrets: &[(String, String)]) -> String {
    let mut ordered: Vec<&(String, String)> = secrets
        .iter()
        .filter(|(_, v)| v.len() >= MIN_SECRET_LEN)
        .collect();
    ordered.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
    let mut out = text.to_string();
    for (name, value) in ordered {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("<redacted:{name}>"));
        }
    }
    out
}

/// One child process run, already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliCall {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Exit status, `None` when the child was stopped at the time limit or killed by a signal.
    pub status: Option<i32>,
    /// Attempt number, from 1.
    pub attempt: u32,
    /// Last bytes of standard output.
    pub stdout: String,
    /// Last bytes of standard error.
    pub stderr: String,
}

/// Upload backend that runs the configured hosting CLI once per file.
#[derive(Debug)]
pub struct CliBackend {
    config: UploadCliConfig,
    dir: PathBuf,
    secrets: Vec<(String, String)>,
    attempts: u32,
    calls: RefCell<Vec<CliCall>>,
}

impl CliBackend {
    /// A backend for the release directory `dir`. `secrets` come from [`resolve_secrets`] and
    /// are passed to each child as environment variables.
    ///
    /// # Errors
    ///
    /// `upload_cli_secret` when the config text holds a secret value.
    pub fn new(
        config: UploadCliConfig,
        dir: &Path,
        secrets: Vec<(String, String)>,
    ) -> Result<Self, ChunkError> {
        config.check_no_secret_text(&secrets)?;
        Ok(Self {
            config,
            dir: dir.to_path_buf(),
            secrets,
            attempts: 1,
            calls: RefCell::new(Vec::new()),
        })
    }

    /// Run each `put` up to `attempts` times (at least once) while it exits non-zero.
    #[must_use]
    pub fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    /// Every child run so far, redacted.
    #[must_use]
    pub fn calls(&self) -> Vec<CliCall> {
        self.calls.borrow().clone()
    }

    fn run(&self, argv: &[String], attempts: u32) -> Result<String, ChunkError> {
        let shown = argv
            .iter()
            .map(|arg| redact(arg, &self.secrets))
            .collect::<Vec<_>>();
        let mut last = None;
        for attempt in 1..=attempts {
            let outcome = self.run_once(argv)?;
            let call = CliCall {
                argv: shown.clone(),
                status: outcome.status,
                attempt,
                stdout: tail(&redact(&outcome.stdout, &self.secrets)),
                stderr: tail(&redact(&outcome.stderr, &self.secrets)),
            };
            self.calls.borrow_mut().push(call.clone());
            if outcome.timed_out {
                return Err(ChunkError::new(
                    "upload_cli_timeout",
                    format!(
                        "`{}` ran longer than {} s and was stopped",
                        shown.join(" "),
                        self.config.timeout.as_secs()
                    ),
                ));
            }
            if outcome.status == Some(0) {
                if outcome.stdout_truncated {
                    return Err(ChunkError::new(
                        "upload_cli_failed",
                        format!("`{}` printed more than {MAX_OUTPUT_BYTES} bytes", shown.join(" ")),
                    ));
                }
                return Ok(outcome.stdout);
            }
            last = Some(call);
        }
        let call = last.unwrap_or_else(|| CliCall {
            argv: shown.clone(),
            status: None,
            attempt: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
        let status = call
            .status
            .map_or_else(|| "no status".to_string(), |code| format!("status {code}"));
        Err(ChunkError::new(
            "upload_cli_failed",
            format!(
                "`{}` exited with {status} after {} attempt(s): {}",
                call.argv.join(" "),
                call.attempt,
                call.stderr.trim()
            ),
        ))
    }

    fn run_once(&self, argv: &[String]) -> Result<RunOutcome, ChunkError> {
        let Some((program, args)) = argv.split_first() else {
            return Err(config_error("empty command line"));
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.secrets {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|err| {
            ChunkError::new(
                "upload_cli_spawn",
                redact(&format!("cannot start {program}: {err}"), &self.secrets),
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| std::thread::spawn(move || read_bounded(pipe)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| std::thread::spawn(move || read_bounded(pipe)));
        let deadline = Instant::now() + self.config.timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(err) => {
                    let _ = child.kill();
                    return Err(ChunkError::new(
                        "upload_cli_failed",
                        format!("waiting for {program}: {err}"),
                    ));
                }
            }
        };
        let (stdout, stdout_truncated) = join_reader(stdout);
        let (stderr, _) = join_reader(stderr);
        Ok(RunOutcome {
            status,
            timed_out,
            stdout,
            stdout_truncated,
            stderr,
        })
    }
}

impl UploadBackend for CliBackend {
    fn list(&self) -> Result<Vec<RemoteFile>, ChunkError> {
        if self.config.list.is_empty() {
            return Ok(Vec::new());
        }
        let argv = self.config.argv(&self.config.list, &self.dir, None)?;
        let stdout = self.run(&argv, 1)?;
        parse_listing(&stdout)
    }

    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<(), ChunkError> {
        static STAGE: AtomicU64 = AtomicU64::new(0);
        let stage = std::env::temp_dir().join(format!(
            "regia-upload-cli-{}-{}",
            std::process::id(),
            STAGE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&stage).map_err(|err| ChunkError::io(&stage, &err))?;
        let path = stage.join(crate::upload::safe_name(name)?);
        let result = fs::write(&path, bytes)
            .map_err(|err| ChunkError::io(&path, &err))
            .and_then(|()| self.put_file(name, &path));
        let _ = fs::remove_dir_all(&stage);
        result
    }

    fn put_file(&mut self, name: &str, path: &Path) -> Result<(), ChunkError> {
        let name = crate::upload::safe_name(name)?;
        let argv = self
            .config
            .argv(&self.config.put, &self.dir, Some((name, path)))?;
        self.run(&argv, self.attempts).map(|_| ())
    }

    fn publish(&mut self) -> Result<(), ChunkError> {
        if self.config.publish.is_empty() {
            return Ok(());
        }
        let argv = self.config.argv(&self.config.publish, &self.dir, None)?;
        self.run(&argv, 1).map(|_| ())
    }
}

/// What a `cli:` upload of a directory would run. Nothing was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliDryRun {
    /// Planned file names in upload order.
    pub files: Vec<String>,
    /// Command lines in run order (program first), redacted.
    pub commands: Vec<Vec<String>>,
    /// Each `secret_env` name and whether it is set and non-empty. Values are never kept.
    pub secret_env: Vec<(String, bool)>,
}

/// Plan a `cli:` upload of `dir` without starting anything (ADR-8846). Credential variables are
/// read through `lookup` only to report whether they are set, to refuse a config that holds a
/// set value (`upload_cli_secret`) and to redact.
///
/// # Errors
///
/// `release_dir` when `dir` cannot be planned, `upload_cli_secret`, or `unsafe_path`.
pub fn cli_dry_run(
    dir: &Path,
    config: &UploadCliConfig,
    opts: &crate::UploadPlanOptions,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CliDryRun, ChunkError> {
    let plan = crate::upload_plan_with(dir, opts)?;
    let files: Vec<String> = plan
        .groups
        .iter()
        .flat_map(|group| group.files.iter().cloned())
        .collect();
    let set: Vec<(String, String)> = config
        .secret_env
        .iter()
        .filter_map(|name| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(|value| (name.clone(), value))
        })
        .collect();
    config.check_no_secret_text(&set)?;
    let commands = config
        .plan_commands(dir, &files)?
        .into_iter()
        .map(|argv| argv.iter().map(|arg| redact(arg, &set)).collect())
        .collect();
    let secret_env = config
        .secret_env
        .iter()
        .map(|name| (name.clone(), set.iter().any(|(found, _)| found == name)))
        .collect();
    Ok(CliDryRun {
        files,
        commands,
        secret_env,
    })
}

/// `SIZE NAME` lines. Blank lines are skipped.
///
/// # Errors
///
/// `upload_cli_list` naming the first line that is not a decimal size and a name.
pub fn parse_listing(text: &str) -> Result<Vec<RemoteFile>, ChunkError> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed = line
            .split_once(char::is_whitespace)
            .and_then(|(size, name)| Some((size.parse::<u64>().ok()?, name.trim())))
            .filter(|(_, name)| !name.is_empty());
        let Some((size, name)) = parsed else {
            return Err(ChunkError::new(
                "upload_cli_list",
                format!("list output line {} is not `SIZE NAME`", index + 1),
            ));
        };
        out.push(RemoteFile {
            name: name.to_string(),
            size,
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(out)
}

struct RunOutcome {
    status: Option<i32>,
    timed_out: bool,
    stdout: String,
    stdout_truncated: bool,
    stderr: String,
}

fn read_bounded(mut pipe: impl Read) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                if read > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buf[..read.min(room)]);
            }
        }
    }
    (kept, truncated)
}

fn join_reader(handle: Option<std::thread::JoinHandle<(Vec<u8>, bool)>>) -> (String, bool) {
    let (bytes, truncated) = handle.and_then(|h| h.join().ok()).unwrap_or_default();
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - OUTPUT_TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn placeholders(arg: &str) -> Result<Vec<String>, ChunkError> {
    let mut out = Vec::new();
    let mut rest = arg;
    while let Some(open) = rest.find(['{', '}']) {
        if rest[open..].starts_with('}') {
            return Err(config_error(format!("argument {arg:?} has a `}}` without `{{`")));
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(config_error(format!("argument {arg:?} has a `{{` without `}}`")));
        };
        let name = &after[..close];
        if !is_var_name(name) {
            return Err(config_error(format!(
                "argument {arg:?} has a bad placeholder {{{name}}}"
            )));
        }
        out.push(name.to_string());
        rest = &after[close + 1..];
    }
    Ok(out)
}

fn expand(arg: &str, values: &BTreeMap<&str, String>) -> Result<String, ChunkError> {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| config_error(format!("argument {arg:?} has a `{{` without `}}`")))?;
        let name = &after[..close];
        let value = values
            .get(name)
            .ok_or_else(|| config_error(format!("placeholder {{{name}}} has no value here")))?;
        out.push_str(value);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn utf8(path: &Path) -> Result<&str, ChunkError> {
    path.to_str().ok_or_else(|| {
        ChunkError::new(
            "unsafe_path",
            format!(
                "{} is not UTF-8 and cannot be passed to the hosting CLI",
                path.display()
            ),
        )
    })
}

fn config_error(message: impl Into<String>) -> ChunkError {
    ChunkError::new("upload_cli_config", message)
}
