// SPDX-License-Identifier: MPL-2.0
//! Generators: procedural content as a declared, re-runnable asset source (`docs/specs/23` § 6,
//! `docs/formats/generator-toml.md`, ADR-2412).
//!
//! A generator is declared in `generators/<name>.toml`: its kind (an external `command`, or the
//! built-in `heightfield`), seed, inputs and declared outputs. [`run`] runs it into a private
//! staging directory under `.regia/gen/staging/`, checks that every declared output was written,
//! installs the outputs into the project (only the files whose bytes changed are rewritten, each
//! atomically) and records provenance `generator + seed` in each output's `.meta`. [`verify`]
//! re-runs the generators into staging directories and compares every byte with the project's
//! copies: any changed, missing or extra file is a [`GenDiff`] (DO27's pack-doctor rule).
//!
//! The command contract: the process starts in the project root with `REGIA_GEN_NAME`,
//! `REGIA_GEN_SEED`, `REGIA_GEN_OUT` (the staging root) and `REGIA_PROJECT_DIR` set, and the
//! placeholders `{seed}`, `{out}`, `{project}` and `{name}` in its arguments replaced. It writes
//! each declared output at the same relative path under `REGIA_GEN_OUT`, never into the project.
//! A generator that is not deterministic for a fixed seed fails `verify`. The process does not
//! inherit the caller's environment (a CI step keeps its secrets there): it starts from an empty
//! one plus the variables in [`GENERATOR_ENV_ALLOW`] that the caller has set (ADR-4922).

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Current version of `generators/<name>.toml`.
pub const GENERATOR_FORMAT_VERSION: u32 = 1;

/// The project folder that holds generator declarations.
pub const GENERATORS_DIR: &str = "generators";

/// Where runs stage their outputs before install or comparison (inside the project, so an
/// install is a same-volume rename).
pub const STAGING_DIR: &str = ".regia/gen/staging";

/// The only variables of the caller's environment a `command` generator inherits (when set): what a
/// program needs to start and find its tools and temp folder on Linux, macOS and Windows. Anything
/// else, a CI step's tokens included, stays out (threat-model finding 25, ADR-4922); the generator
/// also gets its own `REGIA_GEN_*` and `REGIA_PROJECT_DIR`.
pub const GENERATOR_ENV_ALLOW: [&str; 13] = [
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
];

/// The longest a `command` generator may run when its file sets no `timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// The most bytes of a failed command's stderr kept in the error.
const STDERR_TAIL: usize = 4096;
/// How long to wait for the end of a finished command's stderr before using what arrived.
const STDERR_GRACE: Duration = Duration::from_millis(500);
/// How long a failed command's error waits for the first stderr bytes when none arrived in
/// `STDERR_GRACE`: on a loaded machine the reader thread may not have run yet, and the stderr is
/// the error message.
const STDERR_FAILED_WAIT: Duration = Duration::from_secs(5);

/// What a generator is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratorKind {
    /// Any executable (a Rust bin in the project, a Python or Blender script, ...).
    Command,
    /// The built-in value-noise heightfield: one `.r16` output (16-bit little endian, row major).
    Heightfield,
}

impl GeneratorKind {
    /// The kind's name as the file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Heightfield => "heightfield",
        }
    }
}

/// `[generator.heightfield]`: parameters of the built-in heightfield kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeightfieldParams {
    /// Samples per row, 2..=8193.
    pub width: u32,
    /// Rows, 2..=8193.
    pub height: u32,
    /// Noise octaves, 1..=16 (default 5).
    #[serde(default = "default_octaves")]
    pub octaves: u32,
    /// Lattice cells per sample of the first octave, in (0, 1] (default 1/64).
    #[serde(default = "default_frequency")]
    pub frequency: f64,
    /// Frequency factor between octaves, in [1, 4] (default 2).
    #[serde(default = "default_lacunarity")]
    pub lacunarity: f64,
    /// Amplitude factor between octaves, in (0, 1] (default 0.5).
    #[serde(default = "default_gain")]
    pub gain: f64,
}

fn default_octaves() -> u32 {
    5
}
fn default_frequency() -> f64 {
    1.0 / 64.0
}
fn default_lacunarity() -> f64 {
    2.0
}
fn default_gain() -> f64 {
    0.5
}

/// `[generator]`: one generator declaration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratorDef {
    /// What runs.
    pub kind: GeneratorKind,
    /// The seed every run passes on (`{seed}`, `REGIA_GEN_SEED`) and the `.meta` records.
    pub seed: u64,
    /// What the generator makes, for listings.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `kind = "command"`: the program and its arguments (placeholders `{seed}`, `{out}`,
    /// `{project}`, `{name}`). A relative program path with a separator is project-relative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// `kind = "command"`: the longest run in seconds (default [`DEFAULT_TIMEOUT_SECS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Project-relative files or folders the generator reads (each must exist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Project-relative files, or folders (a trailing `/`), the generator writes. A folder output
    /// owns every file under it.
    pub outputs: Vec<String>,
    /// `kind = "heightfield"`: its parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heightfield: Option<HeightfieldParams>,
}

/// `generators/<name>.toml` v1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratorFile {
    /// Must be [`GENERATOR_FORMAT_VERSION`].
    pub format_version: u32,
    /// The declaration.
    pub generator: GeneratorDef,
}

/// Why a generator could not be loaded, run or compared. [`GenError::code`] is the stable code
/// the CLI and the agent protocol report.
#[derive(Debug, thiserror::Error)]
pub enum GenError {
    /// No `generators/<name>.toml`.
    #[error("no generator `{0}` (generators/{0}.toml)")]
    NotFound(String),
    /// The file does not parse or breaks a rule.
    #[error("generators/{name}.toml: {message}")]
    Invalid {
        /// Generator name.
        name: String,
        /// What is wrong.
        message: String,
    },
    /// The file is newer than this engine reads.
    #[error("generators/{name}.toml is format_version {found}; this engine reads up to {supported}")]
    UnsupportedVersion {
        /// Generator name.
        name: String,
        /// The file's version.
        found: u32,
        /// [`GENERATOR_FORMAT_VERSION`].
        supported: u32,
    },
    /// A declared input does not exist.
    #[error("generator `{name}`: input {path} does not exist")]
    InputMissing {
        /// Generator name.
        name: String,
        /// The project-relative input.
        path: String,
    },
    /// The command did not start.
    #[error("generator `{name}`: cannot start {program}: {message}")]
    SpawnFailed {
        /// Generator name.
        name: String,
        /// The program.
        program: String,
        /// The OS error.
        message: String,
    },
    /// [`GenControl`] asked the run to stop.
    #[error("generator `{name}` was cancelled")]
    Cancelled {
        /// Generator name.
        name: String,
    },
    /// The command ended with a failure status.
    #[error("generator `{name}` failed ({status}): {stderr}")]
    Failed {
        /// Generator name.
        name: String,
        /// The exit status.
        status: String,
        /// The end of its stderr.
        stderr: String,
    },
    /// The command ran longer than its timeout and was killed.
    #[error("generator `{name}` ran longer than {secs} s and was stopped")]
    Timeout {
        /// Generator name.
        name: String,
        /// The timeout.
        secs: u64,
    },
    /// An output path leads outside the project (a symlink), or is a symlink itself.
    #[error("generator `{name}`: output {path} leads outside the project or is a symlink")]
    PathDenied {
        /// Generator name.
        name: String,
        /// The project-relative output.
        path: String,
    },
    /// The run did not write a declared output.
    #[error("generator `{name}` did not write its declared output {path}")]
    OutputMissing {
        /// Generator name.
        name: String,
        /// The project-relative output.
        path: String,
    },
    /// A file operation failed.
    #[error("generator `{name}`: {message}")]
    Io {
        /// Generator name.
        name: String,
        /// What failed.
        message: String,
    },
}

impl GenError {
    /// The stable error code (lowercase, `_`): `gen_not_found`, `gen_invalid`,
    /// `gen_unsupported_version`, `gen_input_missing`, `gen_spawn_failed`, `gen_failed`,
    /// `gen_timeout`, `gen_output_missing`, `gen_path_denied`, `gen_io`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "gen_not_found",
            Self::Invalid { .. } => "gen_invalid",
            Self::UnsupportedVersion { .. } => "gen_unsupported_version",
            Self::InputMissing { .. } => "gen_input_missing",
            Self::SpawnFailed { .. } => "gen_spawn_failed",
            Self::Cancelled { .. } => "gen_cancelled",
            Self::Failed { .. } => "gen_failed",
            Self::Timeout { .. } => "gen_timeout",
            Self::OutputMissing { .. } => "gen_output_missing",
            Self::PathDenied { .. } => "gen_path_denied",
            Self::Io { .. } => "gen_io",
        }
    }
}

fn io_err(name: &str, what: impl std::fmt::Display) -> GenError {
    GenError::Io {
        name: name.to_string(),
        message: what.to_string(),
    }
}

fn invalid(name: &str, message: impl Into<String>) -> GenError {
    GenError::Invalid {
        name: name.to_string(),
        message: message.into(),
    }
}

/// True for a generator name: `[a-z0-9_-]+`, at most 64 bytes.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The declaration file of generator `name`.
#[must_use]
pub fn generator_path(project: &Path, name: &str) -> PathBuf {
    project.join(GENERATORS_DIR).join(format!("{name}.toml"))
}

/// A project-relative path the generator may name: relative, no `..`, no `.regia/` and no
/// `generators/` (a generator never rewrites declarations). Returns it with `/` separators.
fn checked_rel(name: &str, field: &str, raw: &str) -> Result<String, GenError> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid(name, format!("{field}: an empty path")));
    }
    if raw.contains('\\') {
        return Err(invalid(name, format!("{field}: `{raw}` uses `\\`; write `/`")));
    }
    // The shared rule first (ADR-4922): Windows reads `C:x`, `nul`, `a:b` and `...` as a drive, a
    // device, a stream and `..`, which the component walk below treats as plain names on Linux.
    if let Err(e) = regia_core::project_path::check_relative(trimmed) {
        return Err(invalid(
            name,
            format!(
                "{field}: `{raw}` is not a portable project path ({}): {e}",
                e.code()
            ),
        ));
    }
    let path = Path::new(trimmed);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(invalid(
                    name,
                    format!("{field}: `{raw}` must be a project-relative path without `..`"),
                ));
            }
        }
    }
    match parts.first().map(String::as_str) {
        None => Err(invalid(name, format!("{field}: an empty path"))),
        // Case-insensitive: Windows and macOS file systems fold case.
        Some(first)
            if first.eq_ignore_ascii_case(".regia")
                || first.eq_ignore_ascii_case(GENERATORS_DIR)
                || first.to_ascii_lowercase().starts_with(".git") =>
        {
            Err(invalid(
                name,
                format!("{field}: `{raw}` is inside .regia/, generators/ or .git"),
            ))
        }
        Some(_) => Ok(parts.join("/")),
    }
}

impl GeneratorDef {
    /// Checks the declaration's rules (see the format page).
    ///
    /// # Errors
    ///
    /// [`GenError::Invalid`] naming the first broken rule.
    pub fn validate(&self, name: &str) -> Result<(), GenError> {
        if self.outputs.is_empty() {
            return Err(invalid(name, "outputs: declare at least one output"));
        }
        let mut seen = Vec::new();
        for output in &self.outputs {
            let rel = checked_rel(name, "outputs", output)?;
            // A folder output owns every file under it (a run removes the ones it did not write),
            // so a top-level folder such as `assets/` is refused.
            if output.ends_with('/') && !rel.contains('/') {
                return Err(invalid(
                    name,
                    format!(
                        "outputs: `{output}` is a top-level folder; name a subfolder such as `{rel}/gen/`"
                    ),
                ));
            }
            if seen.iter().any(|other: &String| {
                other == &rel
                    || other.starts_with(&format!("{rel}/"))
                    || rel.starts_with(&format!("{other}/"))
            }) {
                return Err(invalid(
                    name,
                    format!("outputs: `{output}` overlaps another output"),
                ));
            }
            seen.push(rel);
        }
        for input in &self.inputs {
            checked_rel(name, "inputs", input)?;
        }
        if self.timeout_secs == Some(0) {
            return Err(invalid(name, "timeout_secs: must be at least 1"));
        }
        match self.kind {
            GeneratorKind::Command => {
                if self
                    .command
                    .first()
                    .is_none_or(|program| program.trim().is_empty())
                {
                    return Err(invalid(name, "command: kind = \"command\" needs a program"));
                }
                if self.heightfield.is_some() {
                    return Err(invalid(name, "heightfield: only for kind = \"heightfield\""));
                }
            }
            GeneratorKind::Heightfield => {
                if !self.command.is_empty() {
                    return Err(invalid(name, "command: only for kind = \"command\""));
                }
                let params = self.heightfield.as_ref().ok_or_else(|| {
                    invalid(
                        name,
                        "heightfield: kind = \"heightfield\" needs [generator.heightfield]",
                    )
                })?;
                if self.outputs.len() != 1 || !self.outputs[0].ends_with(".r16") {
                    return Err(invalid(
                        name,
                        "outputs: a heightfield writes exactly one `.r16` file",
                    ));
                }
                let side = 2..=8193;
                if !side.contains(&params.width) || !side.contains(&params.height) {
                    return Err(invalid(name, "heightfield: width and height must be in 2..=8193"));
                }
                if !(1..=16).contains(&params.octaves) {
                    return Err(invalid(name, "heightfield: octaves must be in 1..=16"));
                }
                if !(params.frequency > 0.0 && params.frequency <= 1.0) {
                    return Err(invalid(name, "heightfield: frequency must be in (0, 1]"));
                }
                if !(1.0..=4.0).contains(&params.lacunarity) {
                    return Err(invalid(name, "heightfield: lacunarity must be in [1, 4]"));
                }
                if !(params.gain > 0.0 && params.gain <= 1.0) {
                    return Err(invalid(name, "heightfield: gain must be in (0, 1]"));
                }
            }
        }
        Ok(())
    }

    /// The declared outputs as project-relative `/` paths, each with whether it is a folder.
    #[must_use]
    pub fn output_paths(&self) -> Vec<(String, bool)> {
        self.outputs
            .iter()
            .map(|raw| {
                (
                    raw.trim_end_matches('/').trim_start_matches("./").to_string(),
                    raw.ends_with('/'),
                )
            })
            .collect()
    }
}

/// The version of a generator file's bytes (`format_version` at the top level), or `Ok(None)`
/// when the TOML is not a generator declaration (no `[generator]` table). `regia
/// upgrade-project` and the fuzz target call this.
///
/// # Errors
///
/// Not UTF-8, not TOML, or a `format_version` that is not a non-negative integer.
pub fn generator_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    if !table.get("generator").is_some_and(toml::Value::is_table) {
        return Ok(None);
    }
    match table.get("format_version") {
        None => Err("format_version is missing".to_string()),
        Some(value) => value
            .as_integer()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| "format_version is not a non-negative integer".to_string()),
    }
}

/// Parses and validates the bytes of `generators/<name>.toml`.
///
/// # Errors
///
/// [`GenError::UnsupportedVersion`] for a newer file, [`GenError::Invalid`] otherwise.
pub fn parse_generator(name: &str, bytes: &[u8]) -> Result<GeneratorDef, GenError> {
    let text = std::str::from_utf8(bytes).map_err(|e| invalid(name, format!("not UTF-8: {e}")))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| invalid(name, e.message().to_string()))?;
    let version = table
        .get("format_version")
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| invalid(name, "format_version: missing or not an integer"))?;
    let version = u32::try_from(version).map_err(|_| invalid(name, "format_version: out of range"))?;
    if version > GENERATOR_FORMAT_VERSION {
        return Err(GenError::UnsupportedVersion {
            name: name.to_string(),
            found: version,
            supported: GENERATOR_FORMAT_VERSION,
        });
    }
    if version < 1 {
        return Err(invalid(name, "format_version: the first version is 1"));
    }
    let file: GeneratorFile = toml::from_str(text).map_err(|e| invalid(name, e.message().to_string()))?;
    file.generator.validate(name)?;
    Ok(file.generator)
}

/// Loads generator `name` from the project.
///
/// # Errors
///
/// [`GenError::NotFound`] when there is no such file (or the name is not a valid one), else as
/// [`parse_generator`].
pub fn load(project: &Path, name: &str) -> Result<GeneratorDef, GenError> {
    if !is_valid_name(name) {
        return Err(GenError::NotFound(name.to_string()));
    }
    let path = generator_path(project, name);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(GenError::NotFound(name.to_string()));
        }
        Err(e) => return Err(io_err(name, format!("cannot read {}: {e}", path.display()))),
    };
    parse_generator(name, &bytes)
}

/// Every `generators/*.toml` of the project, sorted by name, each loaded (or the reason it does
/// not load). A file whose stem is not a valid generator name is listed as invalid.
#[must_use]
pub fn list(project: &Path) -> Vec<(String, Result<GeneratorDef, GenError>)> {
    let Ok(entries) = std::fs::read_dir(project.join(GENERATORS_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            file.strip_suffix(".toml").map(str::to_string)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let loaded = if is_valid_name(&name) {
                load(project, &name)
            } else {
                Err(invalid(&name, "the file name must be [a-z0-9_-]+.toml"))
            };
            (name, loaded)
        })
        .collect()
}

/// A private staging directory, removed when dropped.
struct Staging(PathBuf);

impl Staging {
    fn new(project: &Path, name: &str) -> Result<Self, GenError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = project
            .join(STAGING_DIR)
            .join(format!("{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| io_err(name, format!("cannot create {}: {e}", dir.display())))?;
        Ok(Self(dir))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One produced file: its project-relative path and bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProducedFile {
    /// Project-relative, `/` separators.
    pub path: String,
    /// The bytes the run wrote.
    pub bytes: Vec<u8>,
}

/// Every file under `dir` (recursively, sorted, `.meta` sidecars skipped) as paths relative to
/// `base` with `/` separators.
fn files_under(base: &Path, dir: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_root_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(std::io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if crate::db::is_sidecar_or_scratch_name(&file_name) {
            continue;
        }
        let rel = entry.path().strip_prefix(base).map_err(std::io::Error::other)?;
        out.push(
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
    Ok(out)
}

/// The question [`GenControl::cancel`] asks while a command runs.
pub type GenCancel = Arc<dyn Fn() -> bool + Send + Sync>;

/// The stderr line callback of [`GenControl::on_line`].
pub type GenLine = Arc<dyn Fn(&str) + Send + Sync>;

/// The project-relative path test of [`GenControl::deny_path`].
pub type GenDeny = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How a caller watches and stops a run on another thread (`gen.run` as a job, ADR-2821). The
/// default watches nothing and never stops, as [`run`] and [`verify`] run.
#[derive(Clone, Default)]
pub struct GenControl {
    /// Polled while a command runs and until the run ends: `true` kills its process tree and ends
    /// the run with [`GenError::Cancelled`]. On Unix that is everything in the command's process
    /// group, also what it left running after it exited (a process that leaves the group, such as a
    /// daemon calling `setsid`, is not stopped); a cancellable run that fails (the command, an output,
    /// a refused path, the install) stops its group too.
    /// On Windows it is the command's live process tree (`taskkill /T`), which does not reach a
    /// process the command left behind after it exited (ADR-5974).
    pub cancel: Option<GenCancel>,
    /// Called on each stderr line of a command as it arrives (on a reader thread).
    pub on_line: Option<GenLine>,
    /// Environment variables a command must not see.
    pub env_remove: Vec<String>,
    /// Paths the run may not write or remove: checked for every produced file and every file a
    /// folder output would remove, before anything is installed ([`GenError::PathDenied`]).
    pub deny_path: Option<GenDeny>,
}

impl GenControl {
    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|cancel| cancel())
    }
}

/// Sends `SIGKILL` to every process in process group `pgid`.
#[cfg(unix)]
fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The process group a cancellable command runs in, and what stops it (ADR-5974).
///
/// On Unix a holder process (`cat` reading a pipe only this process writes, so it also ends if this
/// process dies) leads the group, and the command joins it. The holder is reaped only when the run
/// is over, so the group id stays reserved for the whole run (POSIX does not reuse a process group
/// id while the group has a member): signalling the group can never reach an unrelated process,
/// even after the command itself has exited and only what it started in the background is left.
///
/// Dropped, it kills the whole group, unless the run was marked [`CommandGroup::succeeded`] and no
/// cancel is asked: then it ends the holder alone and leaves the group be (a successful generator
/// may start a build server on purpose). So a run that fails anywhere (the command, a missing
/// output, a refused path, the install) or is cancelled stops what the command left. A plain run
/// (no cancel to poll) and Windows have no holder: Windows stops the live command's tree with
/// `taskkill /T` instead, which cannot reach a process whose parent has already exited.
struct CommandGroup<'a> {
    control: &'a GenControl,
    holder: Option<std::process::Child>,
    succeeded: bool,
}

impl<'a> CommandGroup<'a> {
    /// No group: a plain run, or a generator that runs no command.
    fn none(control: &'a GenControl) -> Self {
        Self {
            control,
            holder: None,
            succeeded: false,
        }
    }

    /// Starts the holder of a cancellable run's group (Unix). Without one (it could not start) the
    /// command leads its own group as before, and only a cancel or timeout while it runs stops it.
    fn new(control: &'a GenControl) -> Self {
        #[cfg(unix)]
        if control.cancel.is_some() {
            let mut holder = Command::new("cat");
            holder
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            std::os::unix::process::CommandExt::process_group(&mut holder, 0);
            match holder.spawn() {
                Ok(holder) => {
                    return Self {
                        control,
                        holder: Some(holder),
                        succeeded: false,
                    };
                }
                Err(e) => tracing::warn!(
                    target: "regia_asset_pipeline",
                    "gen: no process-group holder ({e}); a cancel after the command exits cannot stop what it left"
                ),
            }
        }
        Self::none(control)
    }

    /// The group id the command joins, while the holder lives.
    #[cfg(unix)]
    fn id(&self) -> Option<u32> {
        self.holder.as_ref().map(std::process::Child::id)
    }

    /// Stops a command that is still running, with everything in its group (Unix) or process tree
    /// (Windows), then the group's holder. Best effort: the caller reaps the command.
    fn kill_running(&mut self, child: &mut std::process::Child) {
        #[cfg(unix)]
        if self.control.cancel.is_some() && self.holder.is_none() {
            // No holder: the live command leads its own group.
            kill_group(child.id());
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &child.id().to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.kill();
        self.kill();
    }

    /// Kills every process left in the group, the holder last, and reaps the holder.
    fn kill(&mut self) {
        if let Some(mut holder) = self.holder.take() {
            #[cfg(unix)]
            kill_group(holder.id());
            let _ = holder.kill();
            let _ = holder.wait();
        }
    }

    /// Marks the run as succeeded: dropped without a cancel asked, the group runs on.
    fn succeeded(&mut self) {
        self.succeeded = true;
    }
}

impl Drop for CommandGroup<'_> {
    fn drop(&mut self) {
        if !self.succeeded || self.control.cancelled() {
            self.kill();
        } else if let Some(mut holder) = self.holder.take() {
            // The holder alone; the rest of the group runs on.
            let _ = holder.kill();
            let _ = holder.wait();
        }
    }
}

/// Runs generator `name` into a fresh staging directory and returns what it produced (sorted by
/// path). Nothing in the project changes.
///
/// # Errors
///
/// Any [`GenError`] of the run.
pub fn generate(project: &Path, name: &str, def: &GeneratorDef) -> Result<Vec<ProducedFile>, GenError> {
    generate_observed(project, name, def, &GenControl::default()).map(|(produced, _)| produced)
}

/// [`generate`] with a [`GenControl`]. Also returns the command's process group: the caller keeps
/// it until the run is over, so a cancel that comes after the command exited still stops what the
/// command left behind (dropping it with a cancel asked kills the group).
fn generate_observed<'a>(
    project: &Path,
    name: &str,
    def: &GeneratorDef,
    control: &'a GenControl,
) -> Result<(Vec<ProducedFile>, CommandGroup<'a>), GenError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "gen_generate", generator = name).entered();
    for input in &def.inputs {
        let rel = checked_rel(name, "inputs", input)?;
        if !project.join(&rel).exists() {
            return Err(GenError::InputMissing {
                name: name.to_string(),
                path: rel,
            });
        }
    }
    let staging = Staging::new(project, name)?;
    let group = match def.kind {
        GeneratorKind::Command => run_command(project, name, def, &staging.0, control)?,
        GeneratorKind::Heightfield => {
            let params = def
                .heightfield
                .as_ref()
                .ok_or_else(|| invalid(name, "heightfield parameters missing"))?;
            let (rel, _) = def
                .output_paths()
                .into_iter()
                .next()
                .ok_or_else(|| invalid(name, "outputs: none"))?;
            let target = staging.0.join(&rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_err(name, e))?;
            }
            std::fs::write(&target, heightfield_r16(params, def.seed)).map_err(|e| io_err(name, e))?;
            CommandGroup::none(control)
        }
    };
    let mut produced = Vec::new();
    for (rel, is_dir) in def.output_paths() {
        let staged = staging.0.join(&rel);
        if is_dir {
            if !staged.is_dir() {
                return Err(GenError::OutputMissing {
                    name: name.to_string(),
                    path: format!("{rel}/"),
                });
            }
            for file in files_under(&staging.0, &staged).map_err(|e| io_err(name, e))? {
                let bytes = std::fs::read(staging.0.join(&file)).map_err(|e| io_err(name, e))?;
                produced.push(ProducedFile { path: file, bytes });
            }
        } else {
            if !staged.is_file() {
                return Err(GenError::OutputMissing {
                    name: name.to_string(),
                    path: rel,
                });
            }
            let bytes = std::fs::read(&staged).map_err(|e| io_err(name, e))?;
            produced.push(ProducedFile { path: rel, bytes });
        }
    }
    produced.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((produced, group))
}

/// The command's arguments with the placeholders replaced.
fn expand(arg: &str, name: &str, seed: u64, out: &Path, project: &Path) -> String {
    arg.replace("{seed}", &seed.to_string())
        .replace("{out}", &plain_path(out))
        .replace("{project}", &plain_path(project))
        .replace("{name}", name)
}

/// Reads `reader` to its end, keeping only the last [`STDERR_TAIL`] bytes in `tail` and passing each
/// line (without its newline; a line longer than [`STDERR_TAIL`] is passed in pieces) to `on_line`.
fn drain_tail(
    mut reader: impl std::io::Read,
    tail: &Mutex<Vec<u8>>,
    on_line: Option<&(dyn Fn(&str) + Send + Sync)>,
) {
    let mut chunk = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(on_line) = on_line {
            for &byte in &chunk[..n] {
                if byte == b'\n' || line.len() >= STDERR_TAIL {
                    on_line(String::from_utf8_lossy(&line).trim_end());
                    line.clear();
                }
                if byte != b'\n' {
                    line.push(byte);
                }
            }
        }
        let Ok(mut tail) = tail.lock() else { return };
        tail.extend_from_slice(&chunk[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
    if let Some(on_line) = on_line
        && !line.is_empty()
    {
        on_line(String::from_utf8_lossy(&line).trim_end());
    }
}

/// Runs a command generator into `out`. Returns its process group (see [`generate_observed`]); a
/// run that fails, is cancelled or times out has already stopped everything in it.
fn run_command<'a>(
    project: &Path,
    name: &str,
    def: &GeneratorDef,
    out: &Path,
    control: &'a GenControl,
) -> Result<CommandGroup<'a>, GenError> {
    let mut argv = def
        .command
        .iter()
        .map(|a| expand(a, name, def.seed, out, project));
    let program = argv.next().ok_or_else(|| invalid(name, "command: empty"))?;
    // A relative program path with a separator names a file in the project.
    // A Windows-authored `tools\\gen` names the same file on every host (audit F10).
    let program_path =
        if (program.contains('/') || program.contains('\\')) && Path::new(&program).is_relative() {
            project.join(program.replace('\\', "/"))
        } else {
            PathBuf::from(&program)
        };
    let mut command = Command::new(&program_path);
    command.env_clear();
    for key in GENERATOR_ENV_ALLOW {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .args(argv)
        .current_dir(project)
        .env("REGIA_GEN_NAME", name)
        .env("REGIA_GEN_SEED", def.seed.to_string())
        .env("REGIA_GEN_OUT", plain_path(out))
        .env("REGIA_PROJECT_DIR", plain_path(project))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for key in &control.env_remove {
        command.env_remove(key);
    }
    // A run that can be cancelled gets its own process group, so the cancel stops what it started,
    // even after the command itself exited (ADR-5974). A plain run stays in the caller's, so Ctrl+C
    // at a terminal reaches it.
    let mut group = CommandGroup::new(control);
    #[cfg(unix)]
    if control.cancel.is_some() {
        // Join the holder's group; without a holder, lead a new one (0).
        let pgid = group.id().and_then(|id| i32::try_from(id).ok()).unwrap_or(0);
        std::os::unix::process::CommandExt::process_group(&mut command, pgid);
    }
    let mut child = command.spawn().map_err(|e| GenError::SpawnFailed {
        name: name.to_string(),
        program: program.clone(),
        message: e.to_string(),
    })?;
    // Drain stderr on a thread so a chatty generator never blocks on a full pipe; only the last
    // `STDERR_TAIL` bytes are kept, so a generator that floods stderr costs no memory. Each line also
    // goes to `control.on_line` as it arrives.
    let tail = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    if let Some(pipe) = child.stderr.take() {
        let tail = Arc::clone(&tail);
        let on_line = control.on_line.clone();
        std::thread::spawn(move || {
            drain_tail(pipe, &tail, on_line.as_deref());
            let _ = done_tx.send(());
        });
    }
    let secs = def.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if control.cancelled() {
            group.kill_running(&mut child);
            let _ = child.wait();
            return Err(GenError::Cancelled {
                name: name.to_string(),
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                group.kill_running(&mut child);
                let _ = child.wait();
                return Err(GenError::Timeout {
                    name: name.to_string(),
                    secs,
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                group.kill_running(&mut child);
                let _ = child.wait();
                return Err(io_err(name, format!("waiting for {program}: {e}")));
            }
        }
    };
    if !status.success() {
        // What a failed command left running is stopped too; its pipe then ends with its writers,
        // and whatever they wrote is still read below.
        group.kill();
    }
    // A daemon the generator started may hold the pipe open for good: wait a moment for the end of
    // the stream, then take what has arrived (audit F4). The wait is bounded, so the cancel and the
    // timeout never wait on the pipe.
    use std::sync::mpsc::RecvTimeoutError;
    let mut finished = !matches!(done_rx.recv_timeout(STDERR_GRACE), Err(RecvTimeoutError::Timeout));
    if !status.success() && !finished {
        // Nothing read yet usually means the reader has not been scheduled (a loaded machine), not
        // a silent command: wait longer for the first bytes or the end, still bounded.
        let until = Instant::now() + STDERR_FAILED_WAIT;
        while !finished && tail.lock().is_ok_and(|t| t.is_empty()) && Instant::now() < until {
            finished = !matches!(
                done_rx.recv_timeout(Duration::from_millis(50)),
                Err(RecvTimeoutError::Timeout)
            );
        }
    }
    let err_bytes = tail.lock().map(|t| t.clone()).unwrap_or_default();
    if !status.success() {
        let start = err_bytes.len().saturating_sub(STDERR_TAIL);
        return Err(GenError::Failed {
            name: name.to_string(),
            status: status.to_string(),
            stderr: String::from_utf8_lossy(&err_bytes[start..]).trim().to_string(),
        });
    }
    if control.cancelled() {
        // Asked while the command was ending: `group` drops here and kills what it left.
        return Err(GenError::Cancelled {
            name: name.to_string(),
        });
    }
    Ok(group)
}

/// Refuses `path` (a project file the generator writes, removes or compares) when it, or a folder
/// on the way to it, is a symlink leading outside the project, or when it is a symlink itself.
fn check_target(project: &Path, name: &str, rel: &str) -> Result<(), GenError> {
    let path = project.join(rel);
    let denied = || GenError::PathDenied {
        name: name.to_string(),
        path: rel.to_string(),
    };
    if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(denied());
    }
    regia_core::project_path::check_under_root(project, &path).map_err(|_| denied())
}

/// [`check_target`] for every declared output.
fn check_outputs(project: &Path, name: &str, def: &GeneratorDef) -> Result<(), GenError> {
    for (rel, _) in def.output_paths() {
        check_target(project, name, &rel)?;
    }
    Ok(())
}

/// A path as external programs expect it: Windows' verbatim `\\?\C:\...` form (what
/// `canonicalize` returns there) without its prefix; other paths unchanged. The text form of
/// `regia_core::project_path::strip_verbatim` (ADR-10616).
#[must_use]
pub fn plain_path(path: &Path) -> String {
    regia_core::project_path::strip_verbatim(path)
        .display()
        .to_string()
}

/// What one installed output file did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutputReport {
    /// Project-relative path.
    pub path: String,
    /// Size in bytes.
    pub bytes: u64,
    /// BLAKE3 of the bytes, hex.
    pub hash: String,
    /// True when the project's file was written (new or changed bytes).
    pub written: bool,
    /// True when the `.meta` provenance was written (new, or a different generator or seed).
    pub provenance_written: bool,
}

/// What [`run`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunReport {
    /// Generator name.
    pub generator: String,
    /// Its kind.
    pub kind: String,
    /// The seed it ran with.
    pub seed: u64,
    /// Every output file, sorted by path.
    pub outputs: Vec<OutputReport>,
    /// Files removed from folder outputs because the run no longer writes them.
    pub removed: Vec<String>,
}

/// Runs generator `name` and installs its outputs: changed files are rewritten atomically,
/// unchanged ones are left alone, files under a folder output that the run did not write are
/// removed (with their `.meta`), and each output's `.meta` records `generator + seed`. Running
/// twice with the same inputs leaves the project byte-identical.
///
/// # Errors
///
/// Any [`GenError`]; on error the project is unchanged unless the failure came mid-install.
pub fn run(project: &Path, name: &str) -> Result<RunReport, GenError> {
    run_observed(project, name, &GenControl::default())
}

/// [`run`] with a [`GenControl`]. A cancel before the install changes nothing in the project.
///
/// # Errors
///
/// As [`run`], and [`GenError::Cancelled`].
pub fn run_observed(project: &Path, name: &str, control: &GenControl) -> Result<RunReport, GenError> {
    let def = load(project, name)?;
    check_outputs(project, name, &def)?;
    // Held to the end of the run: an error or a cancel below, or a cancel asked by the time it drops
    // (after the install, which then stands), stops what the command left running.
    let (produced, mut group) = generate_observed(project, name, &def, control)?;
    if control.cancelled() {
        return Err(GenError::Cancelled {
            name: name.to_string(),
        });
    }
    if let Some(deny) = &control.deny_path {
        let denied = |path: &str| GenError::PathDenied {
            name: name.to_string(),
            path: path.to_string(),
        };
        if let Some(file) = produced.iter().find(|file| deny(&file.path)) {
            return Err(denied(&file.path));
        }
        for (rel, is_dir) in def.output_paths() {
            let dir = project.join(&rel);
            if is_dir && dir.is_dir() {
                let existing = files_under(project, &dir).map_err(|e| io_err(name, e))?;
                if let Some(file) = existing
                    .iter()
                    .find(|file| deny(file) && !produced.iter().any(|p| &&p.path == file))
                {
                    return Err(denied(file));
                }
            }
        }
    }
    let mut outputs = Vec::new();
    for file in &produced {
        check_target(project, name, &file.path)?;
        let target = project.join(&file.path);
        let same = std::fs::read(&target).is_ok_and(|old| old == file.bytes);
        if !same {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_err(name, e))?;
            }
            crate::cook_ledger::write_atomic(&target, &file.bytes)
                .map_err(|e| io_err(name, format!("cannot write {}: {e}", file.path)))?;
        }
        let provenance_written = stamp_generator_provenance(&target, name, def.seed)
            .map_err(|e| io_err(name, format!("{}: {e}", file.path)))?;
        outputs.push(OutputReport {
            path: file.path.clone(),
            bytes: file.bytes.len() as u64,
            hash: blake3::hash(&file.bytes).to_hex().to_string(),
            written: !same,
            provenance_written,
        });
    }
    let mut removed = Vec::new();
    for (rel, is_dir) in def.output_paths() {
        let dir = project.join(&rel);
        if !is_dir || !dir.is_dir() {
            continue;
        }
        for existing in files_under(project, &dir).map_err(|e| io_err(name, e))? {
            if produced.iter().any(|p| p.path == existing) {
                continue;
            }
            check_target(project, name, &existing)?;
            let path = project.join(&existing);
            std::fs::remove_file(&path)
                .map_err(|e| io_err(name, format!("cannot remove {existing}: {e}")))?;
            let _ = std::fs::remove_file(regia_asset::meta_path_for(&path));
            removed.push(existing);
        }
    }
    group.succeeded();
    Ok(RunReport {
        generator: name.to_string(),
        kind: def.kind.name().to_string(),
        seed: def.seed,
        outputs,
        removed,
    })
}

/// How a project file differs from a fresh run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Both exist and the bytes differ.
    Changed,
    /// The run writes it; the project does not have it.
    Missing,
    /// The project has it under a folder output; the run does not write it.
    Extra,
}

/// One difference [`verify`] found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenDiff {
    /// Generator name.
    pub generator: String,
    /// Project-relative path.
    pub path: String,
    /// What differs.
    pub kind: DiffKind,
    /// BLAKE3 of the fresh run's bytes (hex), when the run wrote the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
    /// BLAKE3 of the project's bytes (hex), when the project has the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found_hash: Option<String>,
    /// The first byte offset that differs (`Changed` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_difference: Option<u64>,
}

/// One generator's verify result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VerifyEntry {
    /// Generator name.
    pub generator: String,
    /// Output files compared.
    pub files: usize,
    /// True when every byte matched.
    pub ok: bool,
    /// Why the generator could not be re-run (`code: message`), when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What [`verify`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    /// One entry per generator, in name order.
    pub generators: Vec<VerifyEntry>,
    /// Every difference, by generator then path.
    pub diffs: Vec<GenDiff>,
}

impl VerifyReport {
    /// True when every generator re-ran and matched byte for byte.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.diffs.is_empty() && self.generators.iter().all(|g| g.ok)
    }
}

fn hex_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Compares one generator's fresh run with the project's files.
fn compare(project: &Path, name: &str, def: &GeneratorDef, produced: &[ProducedFile]) -> Vec<GenDiff> {
    let mut diffs = Vec::new();
    for file in produced {
        match std::fs::read(project.join(&file.path)) {
            Ok(found) if found == file.bytes => {}
            Ok(found) => {
                let first = found
                    .iter()
                    .zip(&file.bytes)
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| found.len().min(file.bytes.len()));
                diffs.push(GenDiff {
                    generator: name.to_string(),
                    path: file.path.clone(),
                    kind: DiffKind::Changed,
                    expected_hash: Some(hex_hash(&file.bytes)),
                    found_hash: Some(hex_hash(&found)),
                    first_difference: Some(first as u64),
                });
            }
            Err(_) => diffs.push(GenDiff {
                generator: name.to_string(),
                path: file.path.clone(),
                kind: DiffKind::Missing,
                expected_hash: Some(hex_hash(&file.bytes)),
                found_hash: None,
                first_difference: None,
            }),
        }
    }
    for (rel, is_dir) in def.output_paths() {
        let dir = project.join(&rel);
        if !is_dir || !dir.is_dir() {
            continue;
        }
        for existing in files_under(project, &dir).unwrap_or_default() {
            if produced.iter().any(|p| p.path == existing) {
                continue;
            }
            let found = std::fs::read(project.join(&existing)).ok();
            diffs.push(GenDiff {
                generator: name.to_string(),
                path: existing,
                kind: DiffKind::Extra,
                expected_hash: None,
                found_hash: found.as_deref().map(hex_hash),
                first_difference: None,
            });
        }
    }
    diffs
}

/// Re-runs the generators named in `names` (every declared generator when empty) into staging
/// directories and compares every output byte with the project. The project is never written.
///
/// # Errors
///
/// [`GenError::NotFound`] for a named generator that does not exist. A generator that fails to
/// load or run is reported in its [`VerifyEntry`] instead, and the others still run.
pub fn verify(project: &Path, names: &[String]) -> Result<VerifyReport, GenError> {
    verify_observed(project, names, &GenControl::default())
}

/// [`verify`] with a [`GenControl`].
///
/// # Errors
///
/// As [`verify`], and [`GenError::Cancelled`] naming the generator the cancel stopped.
pub fn verify_observed(
    project: &Path,
    names: &[String],
    control: &GenControl,
) -> Result<VerifyReport, GenError> {
    let _span = tracing::info_span!(target: "regia_asset_pipeline", "gen_verify").entered();
    let selected: Vec<(String, Result<GeneratorDef, GenError>)> = if names.is_empty() {
        list(project)
    } else {
        let mut out = Vec::new();
        for name in names {
            match load(project, name) {
                Err(GenError::NotFound(missing)) => return Err(GenError::NotFound(missing)),
                other => out.push((name.clone(), other)),
            }
        }
        out
    };
    let mut report = VerifyReport::default();
    // Every generator's process group, held to the end of the verify (see `run_observed`).
    let mut groups = Vec::new();
    for (name, loaded) in selected {
        if control.cancelled() {
            return Err(GenError::Cancelled { name });
        }
        let outcome = loaded.and_then(|def| {
            check_outputs(project, &name, &def)?;
            generate_observed(project, &name, &def, control).map(|(produced, group)| (def, produced, group))
        });
        match outcome {
            Err(GenError::Cancelled { name }) => return Err(GenError::Cancelled { name }),
            Ok((def, produced, mut group)) => {
                group.succeeded();
                groups.push(group);
                let diffs = compare(project, &name, &def, &produced);
                report.generators.push(VerifyEntry {
                    generator: name.clone(),
                    files: produced.len(),
                    ok: diffs.is_empty(),
                    error: None,
                });
                report.diffs.extend(diffs);
            }
            Err(err) => report.generators.push(VerifyEntry {
                generator: name.clone(),
                files: 0,
                ok: false,
                error: Some(format!("{}: {err}", err.code())),
            }),
        }
    }
    // A cancel asked after the last generator: `groups` drops with it and stops what they left.
    if control.cancelled() {
        let name = report
            .generators
            .last()
            .map(|g| g.generator.clone())
            .unwrap_or_default();
        return Err(GenError::Cancelled { name });
    }
    Ok(report)
}

/// The importer id a new `.meta` of a generated file names: the lowercase extension (the cook's
/// texture and data importers use the same ids).
fn importer_for(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "generated".to_string())
}

/// Records provenance `generator + seed` in the `.meta` of `source` (`docs/specs/23` § 5),
/// creating the `.meta` when absent. A `.meta` that already names this generator and seed is left
/// untouched, so a re-run does not change its bytes. Other provenance fields (extensions) are
/// kept; the review state resets to pending when the generator or seed changes.
///
/// Returns whether the `.meta` was written.
///
/// # Errors
///
/// The `.meta` could not be read, parsed or written.
pub fn stamp_generator_provenance(source: &Path, generator: &str, seed: u64) -> Result<bool, String> {
    let mut meta = crate::db::AssetDatabase::load_or_create_meta(source, &importer_for(source))
        .map_err(|e| e.to_string())?;
    let origin = format!("{GENERATORS_DIR}/{generator}.toml");
    if let Some(p) = &meta.provenance
        && p.tool == regia_asset::ProvenanceTool::Generator
        && p.generator.as_deref() == Some(generator)
        && p.seed == Some(seed)
        && p.origin.as_deref() == Some(origin.as_str())
    {
        return Ok(false);
    }
    let previous = meta.provenance.take().unwrap_or_default();
    meta.provenance = Some(regia_asset::Provenance {
        origin: Some(origin),
        tool: regia_asset::ProvenanceTool::Generator,
        generator: Some(generator.to_string()),
        seed: Some(seed),
        client: None,
        session: None,
        model: None,
        created_utc: Some(rfc3339_utc_now()),
        reviewer: regia_asset::ReviewerState::Pending,
        extensions: previous.extensions,
    });
    let text = meta.to_ron_string().map_err(|e| e.to_string())?;
    crate::cook_ledger::write_atomic(&regia_asset::meta_path_for(source), text.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// The current time as RFC 3339 UTC (`2026-11-03T12:34:56Z`). Days to (year, month, day) is
/// Hinnant's `civil_from_days` (public algorithm, `docs/provenance.md`).
fn rfc3339_utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let tod = secs.rem_euclid(86_400);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// SplitMix64's output function (Vigna, public domain): a 64-bit integer hash.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The lattice value at integer point (`x`, `y`) of octave `octave`, in [0, 1).
fn lattice(x: i64, y: i64, seed: u64, octave: u32) -> f64 {
    let h = splitmix64(
        seed ^ splitmix64(
            (x as u64).wrapping_mul(0x9E37_79B9) ^ splitmix64((y as u64) ^ (u64::from(octave) << 48)),
        ),
    );
    // The top 53 bits as a double in [0, 1).
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Value noise at (`x`, `y`): lattice values blended with the cubic smoothstep.
fn value_noise(x: f64, y: f64, seed: u64, octave: u32) -> f64 {
    let (xf, yf) = (x.floor(), y.floor());
    let (xi, yi) = (xf as i64, yf as i64);
    let (tx, ty) = (x - xf, y - yf);
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sy = ty * ty * (3.0 - 2.0 * ty);
    let v00 = lattice(xi, yi, seed, octave);
    let v10 = lattice(xi + 1, yi, seed, octave);
    let v01 = lattice(xi, yi + 1, seed, octave);
    let v11 = lattice(xi + 1, yi + 1, seed, octave);
    let top = v00 + (v10 - v00) * sx;
    let bottom = v01 + (v11 - v01) * sx;
    top + (bottom - top) * sy
}

/// The built-in heightfield: fractal value noise (octaves summed with `gain` and `lacunarity`,
/// normalized), quantized to 16 bits, little endian, row major. Only IEEE add, multiply, divide
/// and floor are used, so the bytes are the same on every platform.
#[must_use]
pub fn heightfield_r16(params: &HeightfieldParams, seed: u64) -> Vec<u8> {
    let _span = tracing::info_span!(target: "regia_asset_pipeline", "gen_heightfield").entered();
    let mut out = Vec::with_capacity(params.width as usize * params.height as usize * 2);
    for row in 0..params.height {
        for col in 0..params.width {
            let (mut sum, mut norm) = (0.0f64, 0.0f64);
            let (mut amplitude, mut frequency) = (1.0f64, params.frequency);
            for octave in 0..params.octaves {
                sum += amplitude
                    * value_noise(
                        f64::from(col) * frequency,
                        f64::from(row) * frequency,
                        seed,
                        octave,
                    );
                norm += amplitude;
                amplitude *= params.gain;
                frequency *= params.lacunarity;
            }
            let v = (sum / norm).clamp(0.0, 1.0);
            let q = (v * 65535.0 + 0.5).floor() as u16;
            out.extend_from_slice(&q.to_le_bytes());
        }
    }
    out
}

/// Field names of every struct and enum in the file, for `cargo xtask check-formats`.
#[must_use]
pub fn generator_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<GeneratorFile>(),
        serde_field_names::<GeneratorDef>(),
        serde_field_names::<HeightfieldParams>(),
        serde_variant_names::<GeneratorKind>(),
    ])
}

/// `generators/<name>.toml` is at its first version: no migration steps.
#[must_use]
pub fn generator_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests;
