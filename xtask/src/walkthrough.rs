// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask walkthrough`: the getting-started path, run as a stranger would (ADR-2954).
//!
//! The rehearsal of the v1.0 row "an outside tester goes from the book to a packaged template". It
//! is **not** that proof: it runs on an engine machine with the engine team's toolchain and caches,
//! and the real outside tester is the owner's to arrange.
//!
//! The commands come from the book. A shell fence whose info string carries `walkthrough=<stage>`
//! (```` ```sh,walkthrough=install ````, optionally `,template=<id>`) is extracted from the pages in
//! [`PAGES`] and run literally by `bash`, with only the substitutions in [`substitutions`] (the
//! archive version placeholder). Every stage runs in a stranger's environment: a temporary `HOME`, a
//! working directory outside the repository, no `REGIA_*` or `FERRITE_*` variables and no repository
//! path on `PATH`. A terminal session is modelled per template: the environment and the working
//! directory a stage leaves behind are what the next stage of that template starts with, as in one
//! terminal window (the `export PATH=…` of the install page reaches later stages that way).
//!
//! Stages, in order (`--stage` runs only the named ones against the state a `--keep` run left):
//!
//! | Stage | Source | What runs |
//! |---|---|---|
//! | `engine-package` | harness | `regia engine package` of this checkout, archived as `regia-engine-<version>.tar.gz` |
//! | `install` | `book/start/install.md` | unpack the archive, build the tools, put them on `PATH` |
//! | `verify` | `book/start/install.md` | `regia engine verify` of the unpacked copy |
//! | `doctor` | `book/start/install.md` | `regia doctor` |
//! | `new` | getting-started, quick start, tutorials | `regia new` for every template |
//! | `smoke` | running, quick start, networked game | the game runs a few frames headless and exits |
//! | `cook` | running | `regia cook` |
//! | `package` | getting-started | `regia package --platform linux` |
//! | `run-package` | harness | unpack the package elsewhere, run its binary with `--exit-after-frames` |
//! | `upgrade` | running | `regia upgrade-project`, which must change no project file |
//! | `android` | running | `cargo ndk -t arm64-v8a check` of the `blank` project's Android crate |
//!
//! Output: `target/walkthrough/report.json` and one log per stage (and template). One
//! `CARGO_TARGET_DIR` is shared by the generated projects; the report lists that and every other
//! difference from a clean machine.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::Serialize;

use crate::book::fenced_blocks;

/// Version of `report.json`.
pub const REPORT_SCHEMA: u32 = 1;

/// The book pages the walkthrough reads, in reading order (paths under `book/`).
pub const PAGES: &[&str] = &[
    "start/install.md",
    "getting-started.md",
    "start/running.md",
    "start/editor-tour.md",
    "user-guide/quick-start.md",
    "tutorials/third-person.md",
    "tutorials/arena.md",
    "tutorials/networked-game.md",
];

/// Whether a stage runs once for the engine or once per template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Once, in the engine session.
    Engine,
    /// Once per selected template, in that template's session.
    Template,
    /// Once, for the template named in the stage's fences (`android`: `blank`).
    OneTemplate,
}

/// One stage of the walkthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stage {
    /// The name used in `walkthrough=<name>` and `--stage <name>`.
    pub name: &'static str,
    /// Engine or per template.
    pub scope: Scope,
    /// Run by the harness itself rather than from book fences.
    pub harness: bool,
    /// Time limit of the stage (one template).
    pub timeout: Duration,
}

const MIN: u64 = 60;

/// Every stage, in run order.
pub const STAGES: &[Stage] = &[
    Stage {
        name: "engine-package",
        scope: Scope::Engine,
        harness: true,
        timeout: Duration::from_secs(60 * MIN),
    },
    Stage {
        name: "install",
        scope: Scope::Engine,
        harness: false,
        timeout: Duration::from_secs(240 * MIN),
    },
    Stage {
        name: "verify",
        scope: Scope::Engine,
        harness: false,
        timeout: Duration::from_secs(20 * MIN),
    },
    Stage {
        name: "doctor",
        scope: Scope::Engine,
        harness: false,
        timeout: Duration::from_secs(10 * MIN),
    },
    Stage {
        name: "new",
        scope: Scope::Template,
        harness: false,
        timeout: Duration::from_secs(10 * MIN),
    },
    Stage {
        name: "smoke",
        scope: Scope::Template,
        harness: false,
        timeout: Duration::from_secs(180 * MIN),
    },
    Stage {
        name: "cook",
        scope: Scope::Template,
        harness: false,
        timeout: Duration::from_secs(60 * MIN),
    },
    Stage {
        name: "package",
        scope: Scope::Template,
        harness: false,
        timeout: Duration::from_secs(120 * MIN),
    },
    Stage {
        name: "run-package",
        scope: Scope::Template,
        harness: true,
        timeout: Duration::from_secs(10 * MIN),
    },
    Stage {
        name: "upgrade",
        scope: Scope::Template,
        harness: false,
        timeout: Duration::from_secs(10 * MIN),
    },
    Stage {
        name: "android",
        scope: Scope::OneTemplate,
        harness: false,
        timeout: Duration::from_secs(180 * MIN),
    },
];

/// The stage named `name`.
#[must_use]
pub fn stage(name: &str) -> Option<&'static Stage> {
    STAGES.iter().find(|s| s.name == name)
}

/// Arguments of `cargo xtask walkthrough`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Run only these stages (repeatable), against the state a `--keep` run left in `--work`.
    #[arg(long = "stage")]
    pub stages: Vec<String>,
    /// Only these templates (repeatable); default: all of them.
    #[arg(long = "template")]
    pub templates: Vec<String>,
    /// Keep the work directory (temporary home, engine copy, projects, shared target).
    #[arg(long)]
    pub keep: bool,
    /// Work directory (default: `regia-walkthrough` under the system temp directory).
    #[arg(long)]
    pub work: Option<PathBuf>,
    /// Print the commands each stage would run, and check the fences; run nothing.
    #[arg(long)]
    pub list: bool,
    /// A `regia` binary built from this checkout for the `engine-package` stage (default:
    /// `cargo run -p regia_cli`, which may rebuild the engine under the CLI's own feature set).
    #[arg(long)]
    pub regia: Option<PathBuf>,
}

/// A `walkthrough=` shell fence of a book page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkFence {
    /// Page under `book/`.
    pub page: String,
    /// 1-based index of the fence among all fences of the page.
    pub index: usize,
    /// The stage it belongs to.
    pub stage: String,
    /// The template it is limited to (`template=<id>`), if any.
    pub template: Option<String>,
    /// The commands, verbatim.
    pub script: String,
}

impl WalkFence {
    /// `book/<page> (fence #N)`.
    #[must_use]
    pub fn source(&self) -> String {
        format!("book/{} (fence #{})", self.page, self.index)
    }
}

/// The walkthrough tags of a fence info string: `Ok(None)` for a fence that is not a walkthrough
/// fence, `Ok(Some((stage, template)))` for one, and an error for a malformed one (a walkthrough tag
/// on a fence that is not `sh`, an empty value, or an unknown key).
pub fn parse_info(info: &str) -> Result<Option<(String, Option<String>)>, String> {
    let mut tokens = info.split(',').map(str::trim);
    let lang = tokens.next().unwrap_or("");
    let rest: Vec<&str> = tokens.collect();
    if !rest.iter().any(|t| t.starts_with("walkthrough=")) {
        return Ok(None);
    }
    if lang != "sh" {
        return Err(format!(
            "`{info}`: walkthrough fences are `sh` fences (commands only, no `$ ` prompts)"
        ));
    }
    let mut stage = None;
    let mut template = None;
    for token in rest {
        match token.split_once('=') {
            Some(("walkthrough", value)) if !value.is_empty() => stage = Some(value.to_owned()),
            Some(("template", value)) if !value.is_empty() => template = Some(value.to_owned()),
            _ => return Err(format!("`{info}`: unknown or empty token `{token}`")),
        }
    }
    Ok(stage.map(|s| (s, template)))
}

/// The walkthrough fences of one page's markdown, in order.
///
/// # Errors
///
/// A malformed walkthrough info string (see [`parse_info`]).
pub fn page_fences(page: &str, markdown: &str) -> Result<Vec<WalkFence>> {
    let mut out = Vec::new();
    for (i, (info, body)) in fenced_blocks(markdown).into_iter().enumerate() {
        let parsed = parse_info(&info).map_err(|e| anyhow::anyhow!("book/{page} (fence #{}): {e}", i + 1))?;
        if let Some((stage, template)) = parsed {
            out.push(WalkFence {
                page: page.to_owned(),
                index: i + 1,
                stage,
                template,
                script: body,
            });
        }
    }
    Ok(out)
}

/// Every walkthrough fence of [`PAGES`] under `book`.
///
/// # Errors
///
/// An unreadable page or a malformed fence.
pub fn book_fences(book: &Path) -> Result<Vec<WalkFence>> {
    let mut out = Vec::new();
    for page in PAGES {
        let path = book.join(page);
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        out.extend(page_fences(page, &text)?);
    }
    Ok(out)
}

/// Problems with a fence set: unknown stages or templates, a fence for a harness stage, a template
/// with no `new` fence, a one-template stage with no fence.
#[must_use]
pub fn fence_problems(fences: &[WalkFence], templates: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    for fence in fences {
        match stage(&fence.stage) {
            None => problems.push(format!("{}: unknown stage `{}`", fence.source(), fence.stage)),
            Some(s) if s.harness => {
                problems.push(format!(
                    "{}: `{}` is run by the harness, not from the book",
                    fence.source(),
                    s.name
                ));
            }
            Some(s) if s.scope == Scope::Engine && fence.template.is_some() => {
                problems.push(format!(
                    "{}: `{}` runs once; drop `template=`",
                    fence.source(),
                    s.name
                ));
            }
            Some(_) => {}
        }
        if let Some(t) = &fence.template
            && !regia_templates::TEMPLATE_IDS.contains(&t.as_str())
        {
            problems.push(format!("{}: unknown template `{t}`", fence.source()));
        }
    }
    for template in templates {
        if fences_for(fences, "new", Some(template)).is_empty() {
            problems.push(format!(
                "no `walkthrough=new,template={template}` fence: the book never creates it"
            ));
        }
    }
    for s in STAGES.iter().filter(|s| !s.harness) {
        let any = fences.iter().any(|f| f.stage == s.name);
        if !any {
            problems.push(format!("no fence for stage `{}`", s.name));
        }
    }
    problems
}

/// The fences that run for `stage` and `template`: the fences tagged with that template when there
/// are any, otherwise the untagged ones. Engine stages take every fence of the stage.
#[must_use]
pub fn fences_for<'a>(
    fences: &'a [WalkFence],
    stage_name: &str,
    template: Option<&str>,
) -> Vec<&'a WalkFence> {
    let of_stage = fences.iter().filter(|f| f.stage == stage_name);
    let Some(template) = template else {
        return of_stage.collect();
    };
    let tagged: Vec<&WalkFence> = of_stage
        .clone()
        .filter(|f| f.template.as_deref() == Some(template))
        .collect();
    if tagged.is_empty() {
        of_stage.filter(|f| f.template.is_none()).collect()
    } else {
        tagged
    }
}

/// The template a one-template stage runs for: the template of its first tagged fence.
#[must_use]
pub fn one_template_of<'a>(fences: &'a [WalkFence], stage_name: &str) -> Option<&'a str> {
    fences
        .iter()
        .filter(|f| f.stage == stage_name)
        .find_map(|f| f.template.as_deref())
}

/// The only rewrites applied to book commands: placeholders that stand for a value the reader
/// knows (the version on the archive they were given).
#[must_use]
pub fn substitutions(version: &str) -> Vec<(String, String)> {
    vec![("<version>".to_owned(), version.to_owned())]
}

/// `script` with every `(from, to)` of `subs` replaced, in order.
#[must_use]
pub fn substitute(script: &str, subs: &[(String, String)]) -> String {
    subs.iter()
        .fold(script.to_owned(), |text, (from, to)| text.replace(from, to))
}

/// The bash program for one stage: strict mode, the session's directory, the fence commands
/// echoed as they run, and a trap that saves the environment and directory the commands leave.
#[must_use]
pub fn stage_program(cwd: &Path, scripts: &[String], env_out: &Path, cwd_out: &Path) -> String {
    let mut program = String::new();
    let on_exit = format!(
        "__rc=$?; env -0 > {}; pwd > {}; exit $__rc",
        shell_quote(&env_out.to_string_lossy()),
        shell_quote(&cwd_out.to_string_lossy())
    );
    program.push_str(&format!("trap {} EXIT\n", shell_quote(&on_exit)));
    program.push_str("set -euo pipefail\n");
    program.push_str(&format!("cd -- {}\n", shell_quote(&cwd.to_string_lossy())));
    program.push_str("set -x\n");
    for script in scripts {
        program.push_str(script);
        if !script.ends_with('\n') {
            program.push('\n');
        }
    }
    program
}

/// `text` in single quotes for bash.
#[must_use]
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Variables bash sets by itself, never carried from one stage to the next.
const SHELL_OWN: &[&str] = &["_", "SHLVL", "PWD", "OLDPWD"];

/// Parses `env -0` output into a map, dropping the shell's own variables.
#[must_use]
pub fn parse_env_dump(bytes: &[u8]) -> BTreeMap<String, String> {
    bytes
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let text = String::from_utf8_lossy(entry);
            let (key, value) = text.split_once('=')?;
            (!key.is_empty() && !SHELL_OWN.contains(&key)).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Variables passed from the caller to the stranger's environment, when set: the display, the
/// Vulkan driver choice, the locale, the build-job limits of the host, and the Android SDK the
/// install page tells the reader to set.
pub const PASS_THROUGH: &[&str] = &[
    "DISPLAY",
    "XAUTHORITY",
    "VK_ICD_FILENAMES",
    "VK_DRIVER_FILES",
    "LANG",
    "LC_ALL",
    "TERM",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "ANDROID_NDK_HOME",
    "ANDROID_NDK_ROOT",
    "JAVA_HOME",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "CARGO_PROFILE_DEV_DEBUG",
];

/// The stranger's starting environment, built from the caller's `env`: a new `HOME`, `CARGO_HOME`
/// and `RUSTUP_HOME` kept at the caller's toolchain (so the run does not download Rust again; listed
/// as a difference), `PATH` without any entry under `exclude`, and [`PASS_THROUGH`]. Every other
/// variable, `REGIA_*` and `FERRITE_*` among them, is dropped.
#[must_use]
pub fn stranger_env(
    caller: &BTreeMap<String, String>,
    home: &Path,
    exclude: &[PathBuf],
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let caller_home = caller.get("HOME").map(PathBuf::from);
    let cargo_home = caller
        .get("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| caller_home.as_ref().map(|h| h.join(".cargo")));
    let rustup_home = caller
        .get("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| caller_home.as_ref().map(|h| h.join(".rustup")));
    let mut path: Vec<String> = caller
        .get("PATH")
        .map(|p| {
            std::env::split_paths(p)
                .filter(|entry| !exclude.iter().any(|ex| entry.starts_with(ex)))
                .map(|entry| entry.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if let Some(cargo_home) = &cargo_home {
        let bin = cargo_home.join("bin").to_string_lossy().into_owned();
        if !path.contains(&bin) {
            path.insert(0, bin);
        }
        env.insert("CARGO_HOME".to_owned(), cargo_home.to_string_lossy().into_owned());
    }
    if let Some(rustup_home) = rustup_home {
        env.insert(
            "RUSTUP_HOME".to_owned(),
            rustup_home.to_string_lossy().into_owned(),
        );
    }
    env.insert("PATH".to_owned(), path.join(":"));
    env.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
    for key in PASS_THROUGH {
        if let Some(value) = caller.get(*key) {
            env.insert((*key).to_owned(), value.clone());
        }
    }
    env
}

/// Replaces every [`PASS_THROUGH`] variable of a saved session `env` with the caller's value, or
/// removes it when the caller has none.
pub fn refresh_pass_through(env: &mut BTreeMap<String, String>, caller: &BTreeMap<String, String>) {
    for key in PASS_THROUGH {
        match caller.get(*key) {
            Some(value) => {
                env.insert((*key).to_owned(), value.clone());
            }
            None => {
                env.remove(*key);
            }
        }
    }
}

/// The differences between this run and a clean machine, written into the report.
#[must_use]
pub fn differences(target_dir: &Path) -> Vec<String> {
    vec![
        format!(
            "One CARGO_TARGET_DIR ({}) is shared by every generated project, so the engine is compiled for the \
             projects once; a clean machine compiles it once per project.",
            target_dir.display()
        ),
        "CARGO_HOME and RUSTUP_HOME point at the host's: the pinned toolchain and the crate downloads come from \
         its caches, not from the network."
            .to_owned(),
        "The host already has the system packages, the Vulkan driver (lavapipe here), xvfb and the Android NDK; \
         the walkthrough checks the book's list of them, not their installation."
            .to_owned(),
        "The engine archive is made from this checkout by `regia engine package` (with `--allow-dirty`), not \
         downloaded from a release channel (owner question Q15)."
            .to_owned(),
        "Variables passed through from the host: DISPLAY, XAUTHORITY, VK_ICD_FILENAMES, VK_DRIVER_FILES, the \
         locale, TMPDIR, the Android SDK variables, JAVA_HOME and the cargo job and debug-info limits."
            .to_owned(),
    ]
}

/// One problem the reading pass found in a page, and what was done about it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Finding {
    /// Page under `book/` (or the engine part).
    pub page: &'static str,
    /// What would stop or mislead an outside reader.
    pub problem: &'static str,
    /// The fix.
    pub fix: &'static str,
}

/// The reading pass's log (ADR-2954): every instruction that failed, was missing or assumed repo
/// knowledge, with its fix. Kept here so `report.json` carries it.
pub const FINDINGS: &[Finding] = &[
    Finding {
        page: "start/install.md",
        problem: "Says the engine is a checkout but never says how the reader gets it or what to do with the \
                  release archive `regia engine package` makes.",
        fix: "New section 0: unpack `regia-engine-<version>.tar.gz` where it will stay and work inside it.",
    },
    Finding {
        page: "start/install.md",
        problem: "No Linux system packages listed; the build stops at `alsa-sys`/`libudev-sys` without \
                  pkg-config and the -dev packages CI installs.",
        fix: "Lists the Debian/Ubuntu, Fedora and Arch package names (the set ci.yml installs), plus xvfb and \
              Mesa for headless runs.",
    },
    Finding {
        page: "start/install.md",
        problem: "`export PATH=\"$PWD/target/release:$PATH\"` only lasts for that terminal, and is wrong when \
                  CARGO_TARGET_DIR is set.",
        fix: "Says to put the line, with the absolute path, in the shell profile, and where the programs are \
              when CARGO_TARGET_DIR is set.",
    },
    Finding {
        page: "start/install.md",
        problem: "The install steps are bash only; a Windows reader has no `PATH` command and no unpack step.",
        fix: "A PowerShell block (tar, build, `$env:Path`) and where to make the path permanent.",
    },
    Finding {
        page: "start/install.md",
        problem: "Nothing checks that the unpacked engine is complete.",
        fix: "Adds `regia engine verify .` after the build.",
    },
    Finding {
        page: "start/install.md",
        problem: "`console` fences with `$ ` prompts cannot be pasted into a shell.",
        fix: "The walkthrough's commands are `sh` fences without prompts.",
    },
    Finding {
        page: "start/install.md",
        problem: "Section 4 runs plain `regia doctor`, which always checks the Android tools; with Android \
                  \"optional\" (section 5) a desktop-only reader gets `jdk_missing` and `gradle_missing` and a \
                  failed doctor (seen on buildhost).",
        fix: "Section 4 runs `regia doctor --platform linux` (or `windows`); plain `regia doctor` after the Android \
              setup. The default section list is in doctor.rs (listed for the lead).",
    },
    Finding {
        page: "getting-started.md",
        problem: "Does not say where to run `regia new`; run inside the engine folder, the project lands inside \
                  the engine release.",
        fix: "Says to run it from the folder that holds your projects (the walkthrough runs it from `~`).",
    },
    Finding {
        page: "getting-started.md",
        problem: "The template table lists `net-game` but `first_person` and `top_down` have no command anywhere \
                  in the book.",
        fix: "Example commands for `first_person` and `top_down` under the table.",
    },
    Finding {
        page: "getting-started.md",
        problem: "Step 3 tells the reader to add `regia = { path = \"../../regia/crates/regia\" }` lines: a \
                  path that only exists in the engine team's layout.",
        fix: "Says to copy the path prefix from the `regia` line `regia new` wrote.",
    },
    Finding {
        page: "start/running.md",
        problem: "`--exit-after-frames` is documented for the samples only; a generated game ignored it and ran \
                  forever, so `templates-check`'s 5-frame smoke and any scripted check of a new project could \
                  not end.",
        fix: "Every template's launcher (and the quick and net game mains) handles `--exit-after-frames N` and \
              returns the app's exit code; running.md documents it for projects.",
    },
    Finding {
        page: "start/running.md",
        problem: "No command to check the Android crate without a device, and `regia upgrade-project` (to run \
                  after every engine update, spec 94) is not mentioned in the getting-started pages.",
        fix: "Adds `cargo ndk -t arm64-v8a check -p my_game_android` and an \"After an engine update\" section.",
    },
    Finding {
        page: "user-guide/quick-start.md",
        problem: "`regia run` and `regia package` need `launcher/`; the quick template has none and the page did \
                  not say which commands apply.",
        fix: "Says `cargo run` runs it, `regia package` packages the root crate, and `regia run` is for \
              workspace templates.",
    },
    Finding {
        page: "tutorials/third-person.md",
        problem: "`regia package --profile shipping` needs cargo-zigbuild and zig (the glibc 2.31 floor) and \
                  install.md never mentions them.",
        fix: "Names the requirement and `--glibc host` next to the command.",
    },
    Finding {
        page: "tutorials/arena.md",
        problem: "Every command (`cargo run -p arena`, `--project samples/arena`, `cargo xtask soak`) needs the \
                  engine repository's workspace; a game developer with a release has `samples/` but the page does not \
                  say to run from the engine folder.",
        fix: "Says the page runs from the engine folder (the release includes the samples).",
    },
    Finding {
        page: "tutorials/networked-game.md",
        problem: "Uses `--template net-game` while `regia_templates::TEMPLATE_IDS` says `net_game` (both are \
                  accepted), and has no command that runs without a second terminal.",
        fix: "Says `regia package` does not package the net-game layout yet (seen in the run; package.rs, for \
              the lead), gives the release build of the client as the stand-in (the walkthrough's net_game \
              package stage) and adds the one-process check \
              `cargo run -p tag_yard_client -- --headless --host --bot --ticks 120`.",
    },
];

/// Owner questions the walkthrough surfaces (not decided here).
pub const OWNER_QUESTIONS: &[&str] = &[
    "docs/versions.md's v1.0 row says a game developer reaches a packaged build \"without engine source\", while \
     book/start/install.md says the engine is distributed as source and projects path-depend on the checkout. \
     The delivery channel is open owner question Q15; the walkthrough rehearses what the book says today (a \
     source release archive).",
];

/// The result of one stage (for one template).
#[derive(Debug, Clone, Serialize)]
pub struct StageResult {
    /// Stage name.
    pub stage: String,
    /// Template, for per-template stages.
    pub template: Option<String>,
    /// `ok`, `failed`, `timeout` or `skipped`.
    pub status: String,
    /// Where the commands came from (`book/<page> (fence #N)`, or `harness`).
    pub sources: Vec<String>,
    /// The commands as run (after substitution).
    pub commands: String,
    /// Wall-clock seconds.
    pub seconds: f64,
    /// The stage log, relative to the workspace root.
    pub log: String,
    /// Why it failed or was skipped.
    pub detail: Option<String>,
}

impl StageResult {
    fn key(&self) -> (String, Option<String>) {
        (self.stage.clone(), self.template.clone())
    }
}

/// `target/walkthrough/report.json`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// [`REPORT_SCHEMA`].
    pub schema: u32,
    /// `"regia_walkthrough"`.
    pub kind: &'static str,
    /// Every stage that ran passed.
    pub ok: bool,
    /// What this run is, and is not.
    pub note: &'static str,
    /// The engine version of the archive.
    pub engine_version: Option<String>,
    /// The work directory.
    pub work: String,
    /// The templates selected.
    pub templates: Vec<String>,
    /// How this run differs from a clean machine.
    pub differences_from_clean_machine: Vec<String>,
    /// The substitutions applied to book commands.
    pub substitutions: Vec<(String, String)>,
    /// One entry per stage (and template).
    pub stages: Vec<StageResult>,
    /// The reading pass's log: page, problem, fix.
    pub findings: Vec<Finding>,
    /// Questions for the owner.
    pub questions_for_owner: Vec<&'static str>,
}

/// The note every report carries.
pub const NOTE: &str = "Rehearsal on an engine machine, not the outside-tester proof of the v1.0 row: the real \
                        outside tester on a clean machine is the owner's to arrange.";

/// Paths of one run.
struct Layout {
    root: PathBuf,
    work: PathBuf,
    home: PathBuf,
    downloads: PathBuf,
    target: PathBuf,
    sessions: PathBuf,
    elsewhere: PathBuf,
    out: PathBuf,
    regia: Option<PathBuf>,
}

impl Layout {
    fn new(root: &Path, work: &Path, regia: Option<PathBuf>) -> Self {
        Self {
            root: root.to_path_buf(),
            work: work.to_path_buf(),
            home: work.join("home"),
            downloads: work.join("home/Downloads"),
            target: work.join("target"),
            sessions: work.join("sessions"),
            elsewhere: work.join("elsewhere"),
            out: root.join("target/walkthrough"),
            regia,
        }
    }

    fn session(&self, name: &str) -> (PathBuf, PathBuf) {
        (
            self.sessions.join(format!("{name}.env")),
            self.sessions.join(format!("{name}.cwd")),
        )
    }

    fn log(&self, stage_name: &str, template: Option<&str>) -> PathBuf {
        match template {
            Some(t) => self.out.join(format!("{stage_name}-{t}.log")),
            None => self.out.join(format!("{stage_name}.log")),
        }
    }
}

/// A terminal session: its environment and directory.
#[derive(Debug, Clone)]
struct Session {
    env: BTreeMap<String, String>,
    cwd: PathBuf,
}

fn load_session(layout: &Layout, name: &str) -> Result<Session> {
    let (env_path, cwd_path) = layout.session(name);
    let env = std::fs::read(&env_path).with_context(|| {
        format!(
            "no saved session `{name}` ({}): run the earlier stages first, with --keep",
            env_path.display()
        )
    })?;
    let cwd =
        std::fs::read_to_string(&cwd_path).with_context(|| format!("reading {}", cwd_path.display()))?;
    Ok(Session {
        env: parse_env_dump(&env),
        cwd: PathBuf::from(cwd.trim_end_matches('\n')),
    })
}

fn save_session(layout: &Layout, name: &str, session: &Session) -> Result<()> {
    std::fs::create_dir_all(&layout.sessions)?;
    let (env_path, cwd_path) = layout.session(name);
    let mut bytes = Vec::new();
    for (k, v) in &session.env {
        bytes.extend_from_slice(format!("{k}={v}").as_bytes());
        bytes.push(0);
    }
    std::fs::write(&env_path, bytes)?;
    std::fs::write(&cwd_path, format!("{}\n", session.cwd.display()))?;
    Ok(())
}

/// Outcome of a child process run with a time limit.
enum Outcome {
    Exited(std::process::ExitStatus),
    TimedOut,
}

/// Runs `command` with stdout and stderr appended to `log`, killing its process group after `limit`.
fn run_logged(mut command: Command, log: &Path, limit: Duration) -> Result<Outcome> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    command
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("starting the stage")?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Outcome::Exited(status));
        }
        if started.elapsed() > limit {
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{}", child.id()))
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn append(log: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "{text}")?;
    Ok(())
}

/// Runs `scripts` in `session` (bash), updating the session from what the commands leave behind.
fn run_scripts(
    layout: &Layout,
    session: &mut Session,
    scripts: &[String],
    log: &Path,
    limit: Duration,
) -> Result<(String, Option<String>)> {
    let env_out = layout.sessions.join("last.env");
    let cwd_out = layout.sessions.join("last.cwd");
    let _ = std::fs::remove_file(&env_out);
    let _ = std::fs::remove_file(&cwd_out);
    let program = stage_program(&session.cwd, scripts, &env_out, &cwd_out);
    append(log, &format!("# walkthrough: bash program\n{program}# ----"))?;
    let mut command = Command::new("bash");
    command
        .args(["--noprofile", "--norc", "-c", &program])
        .env_clear()
        .envs(&session.env)
        .current_dir(&session.cwd);
    let outcome = run_logged(command, log, limit)?;
    if let (Ok(env), Ok(cwd)) = (std::fs::read(&env_out), std::fs::read_to_string(&cwd_out)) {
        session.env = parse_env_dump(&env);
        session.cwd = PathBuf::from(cwd.trim_end_matches('\n'));
    }
    Ok(match outcome {
        Outcome::Exited(status) if status.success() => ("ok".to_owned(), None),
        Outcome::Exited(status) => ("failed".to_owned(), Some(format!("bash exited with {status}"))),
        Outcome::TimedOut => (
            "timeout".to_owned(),
            Some(format!("no exit within {} min", limit.as_secs() / MIN)),
        ),
    })
}

/// The caller's environment as a map.
fn caller_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// The version of the archive in `downloads` (`regia-engine-<version>.tar.gz`).
fn archive_version(downloads: &Path) -> Option<String> {
    std::fs::read_dir(downloads).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        name.strip_prefix("regia-engine-")?
            .strip_suffix(".tar.gz")
            .map(str::to_owned)
    })
}

/// `engine-package`: the engine team's side. Packages this checkout with the repository's `regia`
/// and archives the release directory into the stranger's downloads.
fn engine_package(layout: &Layout, log: &Path, limit: Duration) -> Result<(String, Option<String>)> {
    let regia = layout.regia.as_deref();
    let release = layout.work.join("release");
    let _ = std::fs::remove_dir_all(&release);
    std::fs::create_dir_all(&layout.downloads)?;
    let mut command = match regia {
        Some(regia) => Command::new(regia),
        None => {
            let mut cargo = Command::new("cargo");
            cargo.args(["run", "--locked", "-q", "-p", "regia_cli", "--bin", "regia", "--"]);
            cargo
        }
    };
    command
        .current_dir(&layout.root)
        .args(["engine", "package", "--engine"])
        .arg(&layout.root)
        .args(["--allow-dirty", "--out"])
        .arg(&release);
    append(
        log,
        &format!(
            "# harness: {} engine package --engine <repo> --allow-dirty --out <work>/release",
            regia.map_or_else(
                || "cargo run -p regia_cli --bin regia --".to_owned(),
                |r| r.display().to_string()
            )
        ),
    )?;
    match run_logged(command, log, limit)? {
        Outcome::Exited(status) if status.success() => {}
        Outcome::Exited(status) => {
            return Ok((
                "failed".to_owned(),
                Some(format!("regia engine package: {status}")),
            ));
        }
        Outcome::TimedOut => return Ok(("timeout".to_owned(), Some("regia engine package".to_owned()))),
    }
    let dir = std::fs::read_dir(&release)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("regia-engine-"))
        .context("regia engine package wrote no regia-engine-<version> directory")?;
    let archive = layout.downloads.join(format!("{dir}.tar.gz"));
    append(
        log,
        &format!(
            "# harness: tar -czf {} -C <work>/release {dir}",
            archive.display()
        ),
    )?;
    let mut tar = Command::new("tar");
    tar.arg("-czf").arg(&archive).arg("-C").arg(&release).arg(&dir);
    match run_logged(tar, log, limit)? {
        Outcome::Exited(status) if status.success() => {}
        _ => return Ok(("failed".to_owned(), Some("tar of the release".to_owned()))),
    }
    std::fs::remove_dir_all(&release)?;
    Ok(("ok".to_owned(), None))
}

/// The packaged archive of a project: `dist/linux/*-linux.tar.gz`.
fn package_archive(project: &Path) -> Option<PathBuf> {
    std::fs::read_dir(project.join("dist/linux"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("-linux.tar.gz"))
        })
}

/// The binary named by the `Exec=` line of a staged `.desktop` file.
#[must_use]
pub fn desktop_exec(desktop: &str) -> Option<String> {
    desktop.lines().find_map(|line| {
        let exec = line.strip_prefix("Exec=")?.trim();
        let first = exec.split_whitespace().next()?.trim_matches('"');
        Some(first.rsplit('/').next().unwrap_or(first).to_owned())
    })
}

/// `run-package`: unpacks the project's package into a directory of its own and runs the binary
/// for a few frames, from the home directory, in the template's session environment.
fn run_package(
    layout: &Layout,
    session: &Session,
    template: &str,
    log: &Path,
    limit: Duration,
) -> Result<(String, Option<String>)> {
    if let Some(name) = session
        .cwd
        .file_name()
        .and_then(|dir| release_binary_name(template, &dir.to_string_lossy()))
    {
        return run_release_binary(layout, session, template, &name, log, limit);
    }
    let Some(archive) = package_archive(&session.cwd) else {
        return Ok((
            "failed".to_owned(),
            Some(format!(
                "no dist/linux/*-linux.tar.gz in {}",
                session.cwd.display()
            )),
        ));
    };
    let dest = layout.elsewhere.join(template);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest)?;
    append(
        log,
        &format!("# harness: tar -xzf {} -C {}", archive.display(), dest.display()),
    )?;
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&archive).arg("-C").arg(&dest);
    if !matches!(run_logged(tar, log, limit)?, Outcome::Exited(s) if s.success()) {
        return Ok(("failed".to_owned(), Some("unpacking the package".to_owned())));
    }
    let staged = std::fs::read_dir(&dest)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir());
    let Some(staged) = staged else {
        return Ok((
            "failed".to_owned(),
            Some("the package holds no directory".to_owned()),
        ));
    };
    let desktop = std::fs::read_dir(&staged)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "desktop"))
        .and_then(|p| std::fs::read_to_string(p).ok());
    let Some(bin) = desktop.as_deref().and_then(desktop_exec) else {
        return Ok((
            "failed".to_owned(),
            Some("no .desktop Exec= line in the package".to_owned()),
        ));
    };
    let exe = staged.join(&bin);
    append(
        log,
        &format!("# harness: (cd ~ && {} --exit-after-frames 5)", exe.display()),
    )?;
    let mut command = Command::new(&exe);
    command
        .args(["--exit-after-frames", "5"])
        .env_clear()
        .envs(&session.env)
        .current_dir(&layout.home);
    Ok(match run_logged(command, log, limit)? {
        Outcome::Exited(status) if status.success() => ("ok".to_owned(), None),
        Outcome::Exited(status) => ("failed".to_owned(), Some(format!("{bin}: {status}"))),
        Outcome::TimedOut => (
            "timeout".to_owned(),
            Some(format!("{bin} did not exit after 5 frames")),
        ),
    })
}

/// The release binary the book hands out for a template `regia package` does not package: the
/// quick start's "a build you can hand to someone" is `cargo build --release` of the app crate, and
/// the networked-game tutorial builds `<crate>_client` until `regia package` handles the client/server
/// layout. `None` for templates whose package is a `regia package` archive.
pub fn release_binary_name(template: &str, project_dir: &str) -> Option<String> {
    match template {
        "quick" => Some(project_dir.to_owned()),
        "net_game" => Some(format!("{project_dir}_client")),
        _ => None,
    }
}

/// `run-package` for a template the book builds with `cargo build --release`
/// (`release_binary_name`): copy `$CARGO_TARGET_DIR/release/<name>` to a directory of its own and
/// run it from `~`.
fn run_release_binary(
    layout: &Layout,
    session: &Session,
    template: &str,
    name: &str,
    log: &Path,
    limit: Duration,
) -> Result<(String, Option<String>)> {
    let target = session
        .env
        .get("CARGO_TARGET_DIR")
        .map_or_else(|| session.cwd.join("target"), PathBuf::from);
    let built = target
        .join("release")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !built.is_file() {
        return Ok((
            "failed".to_owned(),
            Some(format!("no release binary {}", built.display())),
        ));
    }
    let dest = layout.elsewhere.join(template);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest)?;
    let exe = dest.join(built.file_name().unwrap_or_default());
    std::fs::copy(&built, &exe).with_context(|| format!("copying {}", built.display()))?;
    append(
        log,
        &format!(
            "# harness: cp {} {}; (cd ~ && {} --exit-after-frames 5)",
            built.display(),
            exe.display(),
            exe.display()
        ),
    )?;
    let mut command = Command::new(&exe);
    command
        .args(["--exit-after-frames", "5"])
        .env_clear()
        .envs(&session.env)
        .current_dir(&layout.home);
    Ok(match run_logged(command, log, limit)? {
        Outcome::Exited(status) if status.success() => ("ok".to_owned(), None),
        Outcome::Exited(status) => ("failed".to_owned(), Some(format!("{name}: {status}"))),
        Outcome::TimedOut => (
            "timeout".to_owned(),
            Some(format!("{name} did not exit after 5 frames")),
        ),
    })
}

/// Content hashes of a project's files, outside build output, for the upgrade no-op check.
fn project_snapshot(project: &Path) -> BTreeMap<String, String> {
    const SKIP: [&str; 5] = ["target", "dist", "build", ".regia", ".git"];
    let mut out = BTreeMap::new();
    let mut pending = vec![project.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if dir != project || !SKIP.contains(&entry.file_name().to_string_lossy().as_ref()) {
                    pending.push(path);
                }
            } else if kind.is_file()
                && let (Ok(bytes), Ok(rel)) = (std::fs::read(&path), path.strip_prefix(project))
            {
                let rel = rel.to_string_lossy().replace('\\', "/");
                out.insert(rel, blake3::hash(&bytes).to_hex().to_string());
            }
        }
    }
    out
}

/// The paths that differ between two snapshots.
#[must_use]
pub fn snapshot_changes(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect()
}

/// The directories directly under `dir`, without hidden ones (`~/.local`, `~/.config`).
fn subdirs(dir: &Path) -> BTreeSet<PathBuf> {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// The templates to walk: `requested` (validated), or all of them.
fn selected(requested: &[String]) -> Result<Vec<&'static str>> {
    crate::templates_check::selected(requested)
}

/// Runs `cargo xtask walkthrough`.
///
/// # Errors
///
/// A malformed fence, an unknown stage or template, or an IO error of the harness itself. A stage
/// that fails is reported in `report.json` and makes the exit code 1.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let templates = selected(&args.templates)?;
    for name in &args.stages {
        if stage(name).is_none() {
            bail!(
                "unknown stage `{name}` (stages: {})",
                STAGES.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
            );
        }
    }
    let fences = book_fences(&root.join("book"))?;
    let problems = fence_problems(&fences, regia_templates::TEMPLATE_IDS);
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("walkthrough: {p}");
        }
        bail!("{} problem(s) with the book's walkthrough fences", problems.len());
    }
    let work = args
        .work
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("regia-walkthrough"));
    if work.starts_with(root) {
        bail!(
            "--work {} is inside the repository; a stranger's machine has no checkout around it",
            work.display()
        );
    }
    let layout = Layout::new(root, &work, args.regia.clone());
    let run_all = args.stages.is_empty();
    let wanted = |name: &str| run_all || args.stages.iter().any(|s| s == name);

    if args.list {
        for s in STAGES {
            let targets: Vec<Option<&str>> = match s.scope {
                Scope::Engine => vec![None],
                Scope::Template => templates.iter().map(|t| Some(*t)).collect(),
                Scope::OneTemplate => vec![one_template_of(&fences, s.name)],
            };
            for t in targets {
                println!(
                    "== {}{}",
                    s.name,
                    t.map(|t| format!(" ({t})")).unwrap_or_default()
                );
                if s.harness {
                    println!("   (harness)");
                    continue;
                }
                for f in fences_for(&fences, s.name, t) {
                    println!("   # {}", f.source());
                    for line in f.script.lines() {
                        println!("   {line}");
                    }
                }
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if run_all {
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&layout.out);
    }
    std::fs::create_dir_all(&layout.home)?;
    std::fs::create_dir_all(&layout.out)?;
    std::fs::create_dir_all(&layout.sessions)?;

    let mut results: Vec<StageResult> = Vec::new();
    let version = |layout: &Layout| archive_version(&layout.downloads);
    let exclude = vec![root.to_path_buf(), work.clone()];

    for s in STAGES.iter().filter(|s| wanted(s.name)) {
        let targets: Vec<Option<&str>> = match s.scope {
            Scope::Engine => vec![None],
            Scope::Template => templates.iter().map(|t| Some(*t)).collect(),
            Scope::OneTemplate => one_template_of(&fences, s.name)
                .filter(|t| templates.contains(t))
                .map(|t| vec![Some(t)])
                .unwrap_or_default(),
        };
        for template in targets {
            let log = layout.log(s.name, template);
            let _ = std::fs::remove_file(&log);
            let label = format!(
                "{}{}",
                s.name,
                template.map(|t| format!(" ({t})")).unwrap_or_default()
            );
            println!("walkthrough: {label} ...");
            let started = Instant::now();
            let (status, detail, sources, commands) =
                run_stage(&layout, &fences, s, template, &exclude, &log, version(&layout))?;
            let seconds = started.elapsed().as_secs_f64();
            println!(
                "walkthrough: {label}: {status} ({seconds:.0} s){}",
                detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default()
            );
            results.push(StageResult {
                stage: s.name.to_owned(),
                template: template.map(str::to_owned),
                status,
                sources,
                commands,
                seconds,
                log: log
                    .strip_prefix(root)
                    .unwrap_or(&log)
                    .to_string_lossy()
                    .into_owned(),
                detail,
            });
            // After every stage, so a long run that is stopped still leaves its results.
            write_report(&layout, &templates, &results, run_all)?;
        }
    }

    let report_path = layout.out.join("report.json");
    let report = write_report(&layout, &templates, &results, run_all)?;
    println!("walkthrough: report {}", report_path.display());
    println!("walkthrough: {NOTE}");
    if run_all && !args.keep {
        let _ = std::fs::remove_dir_all(&work);
    } else {
        println!("walkthrough: work directory kept: {}", work.display());
    }
    let failed: Vec<String> = report
        .stages
        .iter()
        .filter(|r| r.status != "ok")
        .map(|r| {
            format!(
                "{}{}",
                r.stage,
                r.template.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()
            )
        })
        .collect();
    if failed.is_empty() {
        println!("walkthrough: ok");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("walkthrough: failed: {}", failed.join(", "));
        Ok(ExitCode::FAILURE)
    }
}

/// Builds `report.json` from this run's `results` (merged with an earlier report's other stages
/// when only some stages ran) and writes it atomically.
fn write_report(
    layout: &Layout,
    templates: &[&str],
    results: &[StageResult],
    run_all: bool,
) -> Result<Report> {
    let report_path = layout.out.join("report.json");
    let mut stages = results.to_vec();
    if !run_all
        && let Ok(text) = std::fs::read_to_string(&report_path)
        && let Ok(old) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(entries) = old.get("stages").and_then(|v| v.as_array())
    {
        let ran: BTreeSet<(String, Option<String>)> = results.iter().map(StageResult::key).collect();
        let mut kept: Vec<StageResult> = entries
            .iter()
            .filter_map(stage_from_json)
            .filter(|r| !ran.contains(&r.key()))
            .collect();
        kept.extend(stages);
        let order = |r: &StageResult| {
            STAGES
                .iter()
                .position(|s| s.name == r.stage)
                .unwrap_or(usize::MAX)
        };
        kept.sort_by_key(|r| (order(r), r.template.clone()));
        stages = kept;
    }
    let version = archive_version(&layout.downloads);
    let report = Report {
        schema: REPORT_SCHEMA,
        kind: "regia_walkthrough",
        ok: !stages.is_empty() && stages.iter().all(|r| r.status == "ok"),
        note: NOTE,
        engine_version: version.clone(),
        work: layout.work.display().to_string(),
        templates: templates.iter().map(|t| (*t).to_owned()).collect(),
        differences_from_clean_machine: differences(&layout.target),
        substitutions: version.map(|v| substitutions(&v)).unwrap_or_default(),
        stages,
        findings: FINDINGS.to_vec(),
        questions_for_owner: OWNER_QUESTIONS.to_vec(),
    };
    let tmp = report_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&report)?)?;
    std::fs::rename(&tmp, &report_path)?;
    Ok(report)
}

fn stage_from_json(v: &serde_json::Value) -> Option<StageResult> {
    Some(StageResult {
        stage: v.get("stage")?.as_str()?.to_owned(),
        template: v.get("template").and_then(|t| t.as_str()).map(str::to_owned),
        status: v.get("status")?.as_str()?.to_owned(),
        sources: v
            .get("sources")
            .and_then(|s| s.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
        commands: v
            .get("commands")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_owned(),
        seconds: v
            .get("seconds")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        log: v.get("log").and_then(|c| c.as_str()).unwrap_or("").to_owned(),
        detail: v.get("detail").and_then(|d| d.as_str()).map(str::to_owned),
    })
}

type StageOutcome = (String, Option<String>, Vec<String>, String);

/// Runs one stage for one template (or the engine).
fn run_stage(
    layout: &Layout,
    fences: &[WalkFence],
    s: &Stage,
    template: Option<&str>,
    exclude: &[PathBuf],
    log: &Path,
    version: Option<String>,
) -> Result<StageOutcome> {
    if s.name == "engine-package" {
        let (status, detail) = engine_package(layout, log, s.timeout)?;
        // A fresh engine session: a new terminal in the stranger's downloads folder.
        let session = Session {
            env: stranger_env(&caller_env(), &layout.home, exclude),
            cwd: layout.downloads.clone(),
        };
        save_session(layout, "engine", &session)?;
        return Ok((
            status,
            detail,
            vec!["harness".to_owned()],
            "regia engine package --allow-dirty --out <work>/release; tar -czf".to_owned(),
        ));
    }
    let session_name = template.unwrap_or("engine");
    let mut session = match load_session(layout, session_name) {
        Ok(session) => session,
        Err(err) if template.is_some() => {
            // A template's first stage: a new terminal (the engine session's environment, as if the
            // install page's PATH line were in the shell profile) in the home directory, with the
            // shared target directory.
            let mut session = load_session(layout, "engine").map_err(|_| err)?;
            session.cwd = layout.home.clone();
            session.env.insert(
                "CARGO_TARGET_DIR".to_owned(),
                layout.target.to_string_lossy().into_owned(),
            );
            session
        }
        Err(err) => return Err(err),
    };
    // The host's display, driver choice and limits belong to this invocation, not to the run that
    // saved the session (a second `xvfb-run` has another display and auth file).
    refresh_pass_through(&mut session.env, &caller_env());
    if s.name == "run-package" {
        let (status, detail) = run_package(layout, &session, template.unwrap_or("?"), log, s.timeout)?;
        return Ok((
            status,
            detail,
            vec!["harness".to_owned()],
            "tar -xzf dist/linux/*-linux.tar.gz; <bin> --exit-after-frames 5".to_owned(),
        ));
    }
    let Some(version) = version else {
        return Ok((
            "failed".to_owned(),
            Some("no engine archive: run engine-package first".to_owned()),
            Vec::new(),
            String::new(),
        ));
    };
    let chosen = fences_for(fences, s.name, template);
    if chosen.is_empty() {
        return Ok((
            "skipped".to_owned(),
            Some("no book fence for this stage and template".to_owned()),
            Vec::new(),
            String::new(),
        ));
    }
    let subs = substitutions(&version);
    let scripts: Vec<String> = chosen.iter().map(|f| substitute(&f.script, &subs)).collect();
    let sources: Vec<String> = chosen.iter().map(|f| f.source()).collect();
    let commands = scripts.join("");
    let before_dirs = subdirs(&session.cwd);
    let before_cwd = session.cwd.clone();
    let snapshot = (s.name == "upgrade").then(|| project_snapshot(&session.cwd));
    let (mut status, mut detail) = run_scripts(layout, &mut session, &scripts, log, s.timeout)?;
    if s.name == "new" && status == "ok" && session.cwd == before_cwd {
        // The reader `cd`s into the project they just made; the book says so in prose.
        let created: Vec<PathBuf> = subdirs(&before_cwd).difference(&before_dirs).cloned().collect();
        match created.as_slice() {
            [one] => {
                append(log, &format!("# walkthrough: cd {}", one.display()))?;
                session.cwd = one.clone();
            }
            other => {
                status = "failed".to_owned();
                detail = Some(format!(
                    "`regia new` should create one directory, found {}",
                    other.len()
                ));
            }
        }
    }
    if let Some(before) = snapshot
        && status == "ok"
    {
        let changed = snapshot_changes(&before, &project_snapshot(&session.cwd));
        if !changed.is_empty() {
            status = "failed".to_owned();
            detail = Some(format!("upgrade-project changed {}", changed.join(", ")));
        }
    }
    save_session(layout, session_name, &session)?;
    Ok((status, detail, sources, commands))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_strings_mark_stages_and_templates() {
        assert_eq!(parse_info("sh"), Ok(None));
        assert_eq!(parse_info("console"), Ok(None));
        assert_eq!(parse_info("rust,ignore"), Ok(None));
        assert_eq!(
            parse_info("sh,walkthrough=install"),
            Ok(Some(("install".into(), None)))
        );
        assert_eq!(
            parse_info("sh,walkthrough=new,template=quick"),
            Ok(Some(("new".into(), Some("quick".into()))))
        );
        assert_eq!(
            parse_info("sh, walkthrough=doctor"),
            Ok(Some(("doctor".into(), None)))
        );
        assert!(
            parse_info("console,walkthrough=install").is_err(),
            "prompts cannot be run"
        );
        assert!(parse_info("sh,walkthrough=").is_err());
        assert!(parse_info("sh,walkthrough=new,colour=red").is_err());
    }

    const PAGE: &str = "# Page\n\n```console\n$ regia new x\n```\n\n```sh,walkthrough=new,template=blank\nregia new \"My Game\"\n```\n\n```rust\nfn main() {}\n```\n\n```sh,walkthrough=package\nregia package --platform linux\n```\n";

    #[test]
    fn extraction_keeps_page_order_and_fence_numbers() {
        let fences = page_fences("getting-started.md", PAGE).expect("parse");
        assert_eq!(fences.len(), 2);
        assert_eq!(fences[0].index, 2);
        assert_eq!(fences[0].stage, "new");
        assert_eq!(fences[0].template.as_deref(), Some("blank"));
        assert_eq!(fences[0].script, "regia new \"My Game\"\n");
        assert_eq!(fences[1].index, 4);
        assert_eq!(fences[1].source(), "book/getting-started.md (fence #4)");
    }

    #[test]
    fn a_malformed_fence_names_its_page_and_number() {
        let err = page_fences("p.md", "```console,walkthrough=new\n$ x\n```\n").expect_err("console");
        assert!(err.to_string().contains("book/p.md (fence #1)"), "{err}");
    }

    #[test]
    fn tagged_fences_replace_untagged_ones_for_their_template() {
        let mut fences = page_fences("getting-started.md", PAGE).expect("parse");
        fences.push(WalkFence {
            page: "user-guide/quick-start.md".into(),
            index: 1,
            stage: "package".into(),
            template: Some("quick".into()),
            script: "regia package --platform linux --project .\n".into(),
        });
        let generic = fences_for(&fences, "package", Some("blank"));
        assert_eq!(generic.len(), 1);
        assert!(generic[0].template.is_none());
        let quick = fences_for(&fences, "package", Some("quick"));
        assert_eq!(quick.len(), 1);
        assert_eq!(quick[0].template.as_deref(), Some("quick"));
        assert!(
            fences_for(&fences, "new", Some("quick")).is_empty(),
            "a tagged `new` is not generic"
        );
        assert_eq!(one_template_of(&fences, "new"), Some("blank"));
    }

    #[test]
    fn substitution_replaces_only_the_placeholders() {
        let subs = substitutions("0.3.6");
        assert_eq!(
            substitute(
                "tar -xzf regia-engine-<version>.tar.gz\ncd regia-engine-<version>\n",
                &subs
            ),
            "tar -xzf regia-engine-0.3.6.tar.gz\ncd regia-engine-0.3.6\n"
        );
        assert_eq!(
            substitute("echo <name>", &subs),
            "echo <name>",
            "other angle brackets stay"
        );
    }

    #[test]
    fn the_program_runs_strict_in_the_session_directory_and_saves_the_session() {
        let program = stage_program(
            Path::new("/w/home"),
            &["regia doctor".into(), "cd x\n".into()],
            Path::new("/w/s/env"),
            Path::new("/w/s/cwd"),
        );
        let lines: Vec<&str> = program.lines().collect();
        assert!(lines[0].starts_with("trap '__rc=$?; env -0 > "), "{program}");
        assert!(
            lines[0].contains("'\\''/w/s/env'\\''"),
            "paths are quoted inside the trap: {program}"
        );
        assert_eq!(lines[1], "set -euo pipefail");
        assert_eq!(lines[2], "cd -- '/w/home'");
        assert_eq!(lines[3], "set -x");
        assert_eq!(&lines[4..], ["regia doctor", "cd x"]);
    }

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn env_dumps_drop_the_shells_own_variables() {
        let env = parse_env_dump(b"PATH=/a:/b\0_=/usr/bin/env\0SHLVL=1\0PWD=/x\0EMPTY=\0EQ=a=b\0");
        assert_eq!(env.get("PATH").map(String::as_str), Some("/a:/b"));
        assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(env.get("EQ").map(String::as_str), Some("a=b"));
        assert!(!env.contains_key("_") && !env.contains_key("SHLVL") && !env.contains_key("PWD"));
    }

    #[test]
    fn the_stranger_has_a_new_home_no_engine_variables_and_no_repo_on_path() {
        let caller: BTreeMap<String, String> = [
            ("HOME", "/home/dev"),
            (
                "PATH",
                "/repo/target/release:/usr/bin:/home/dev/.cargo/bin:/tmp/walk/bin:/bin",
            ),
            ("REGIA_LOG", "debug"),
            ("FERRITE_GPU", "1"),
            ("REGIA_PROJECT", "/repo"),
            ("DISPLAY", ":99"),
            ("CARGO_TARGET_DIR", "/repo/target"),
            ("RUSTFLAGS", "-Dwarnings"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let env = stranger_env(
            &caller,
            Path::new("/tmp/walk/home"),
            &[PathBuf::from("/repo"), PathBuf::from("/tmp/walk")],
        );
        assert_eq!(env["HOME"], "/tmp/walk/home");
        assert_eq!(env["PATH"], "/usr/bin:/home/dev/.cargo/bin:/bin");
        assert_eq!(env["CARGO_HOME"], "/home/dev/.cargo");
        assert_eq!(env["RUSTUP_HOME"], "/home/dev/.rustup");
        assert_eq!(env["DISPLAY"], ":99");
        for gone in [
            "REGIA_LOG",
            "FERRITE_GPU",
            "REGIA_PROJECT",
            "CARGO_TARGET_DIR",
            "RUSTFLAGS",
        ] {
            assert!(!env.contains_key(gone), "{gone}");
        }
        assert!(
            env.keys()
                .all(|k| !k.starts_with("REGIA_") && !k.starts_with("FERRITE_"))
        );
    }

    #[test]
    fn a_saved_session_takes_this_runs_display_and_keeps_its_own_path() {
        let mut saved: BTreeMap<String, String> = [
            ("DISPLAY", ":99"),
            ("XAUTHORITY", "/tmp/old-auth"),
            ("PATH", "/engine/target/release:/usr/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let caller: BTreeMap<String, String> = [("DISPLAY", ":100"), ("PATH", "/usr/bin")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        refresh_pass_through(&mut saved, &caller);
        assert_eq!(saved["DISPLAY"], ":100");
        assert!(!saved.contains_key("XAUTHORITY"), "a stale auth file is dropped");
        assert_eq!(
            saved["PATH"], "/engine/target/release:/usr/bin",
            "the session's own PATH stays"
        );
    }

    #[test]
    fn cargo_bin_is_added_when_the_path_lacks_it() {
        let caller: BTreeMap<String, String> = [("HOME", "/h"), ("PATH", "/usr/bin")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let env = stranger_env(&caller, Path::new("/w/home"), &[]);
        assert_eq!(env["PATH"], "/h/.cargo/bin:/usr/bin");
    }

    #[test]
    fn desktop_files_name_the_binary() {
        assert_eq!(
            desktop_exec("[Desktop Entry]\nName=My Game\nExec=my_game\n").as_deref(),
            Some("my_game")
        );
        assert_eq!(
            desktop_exec("Exec=\"./bin/my_game\" %U\n").as_deref(),
            Some("my_game")
        );
        assert_eq!(desktop_exec("Name=x\n"), None);
    }

    #[test]
    fn snapshots_report_added_removed_and_changed_files() {
        let a: BTreeMap<String, String> = [("a", "1"), ("b", "2")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let b: BTreeMap<String, String> = [("a", "1"), ("b", "3"), ("c", "4")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(snapshot_changes(&a, &b), ["b", "c"]);
        assert!(snapshot_changes(&a, &a).is_empty());
    }

    #[test]
    fn fence_problems_catch_unknown_stages_harness_stages_and_missing_templates() {
        let fence = |stage: &str, template: Option<&str>| WalkFence {
            page: "p.md".into(),
            index: 1,
            stage: stage.into(),
            template: template.map(str::to_owned),
            script: "true\n".into(),
        };
        let fences = vec![
            fence("bogus", None),
            fence("run-package", None),
            fence("install", Some("blank")),
            fence("new", Some("nope")),
        ];
        let problems = fence_problems(&fences, &["blank"]);
        let text = problems.join("\n");
        assert!(text.contains("unknown stage `bogus`"), "{text}");
        assert!(text.contains("`run-package` is run by the harness"), "{text}");
        assert!(text.contains("`install` runs once"), "{text}");
        assert!(text.contains("unknown template `nope`"), "{text}");
        assert!(text.contains("template=blank"), "{text}");
        assert!(text.contains("no fence for stage `doctor`"), "{text}");
    }

    /// The book itself: every template is created by some page, every book stage has a fence, and
    /// every fence parses. This is what keeps the walkthrough and the pages in step.
    #[test]
    fn the_book_has_a_fence_for_every_stage_and_template() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let fences = book_fences(&root.join("book")).expect("book fences");
        let problems = fence_problems(&fences, regia_templates::TEMPLATE_IDS);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        assert_eq!(one_template_of(&fences, "android"), Some("blank"));
    }

    #[test]
    fn stages_are_unique_and_known_by_name() {
        let names: BTreeSet<&str> = STAGES.iter().map(|s| s.name).collect();
        assert_eq!(names.len(), STAGES.len());
        assert!(stage("package").is_some_and(|s| s.scope == Scope::Template));
        assert!(stage("engine-package").is_some_and(|s| s.harness));
        assert!(stage("nope").is_none());
    }

    #[test]
    fn findings_and_report_name_the_rehearsal() {
        assert!(NOTE.contains("not the outside-tester proof"));
        assert!(!FINDINGS.is_empty());
        for f in FINDINGS {
            assert!(
                PAGES.contains(&f.page),
                "finding page {} is one of the walked pages",
                f.page
            );
        }
        assert!(OWNER_QUESTIONS[0].contains("Q15"));
    }

    #[test]
    fn release_binaries_stand_in_where_the_book_says_regia_package_cannot() {
        assert_eq!(
            release_binary_name("quick", "paddle_ball").as_deref(),
            Some("paddle_ball")
        );
        assert_eq!(
            release_binary_name("net_game", "tag_yard").as_deref(),
            Some("tag_yard_client")
        );
        for template in ["blank", "first_person", "third_person", "top_down"] {
            assert_eq!(release_binary_name(template, "my_game"), None, "{template}");
        }
        // The real book builds the net-game client in its own package fence, so the generic
        // `regia package` line is not used for that template.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let fences = book_fences(&root.join("book")).expect("book fences");
        let net = fences_for(&fences, "package", Some("net_game"));
        assert_eq!(net.len(), 1);
        assert!(net[0].script.contains("cargo build --release -p tag_yard_client"));
    }
}
