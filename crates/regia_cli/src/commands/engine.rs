// SPDX-License-Identifier: MPL-2.0
//! `regia engine info | package | verify | chunk` (`docs/specs/94-distribution-upgrade.md` § 3.3, ADR-1852; chunks: ADR-8404).
//!
//! `info` reports this engine's version and line, the support window and, in a project, its
//! requirement and engine pin. `package` copies a tagged engine checkout into
//! `regia-engine-<version>/` with a `regia-engine.release.ron` manifest; `verify` checks a copy
//! against its manifest. `--json` prints one document per command (schema 1) for agents and CI;
//! the codes are listed in `book/agents/distribution-upgrade.md`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_cli::distribution::{
    self, EnginePin, Problem, Severity, engine_dir_version, package_release, read_pin, read_requirement,
    verify_release,
};
use regia_cli::new_project::engine_checkout;
use regia_core::engine_version::{
    self, Compatibility, EngineRequirement, EngineVersion, OLDEST_UPGRADABLE_LINE, RELEASE_LINES,
};
use serde_json::{Value, json};

/// Version of the `--json` documents.
pub const JSON_SCHEMA: u32 = 1;

/// `regia engine` subcommands.
#[derive(Debug, Subcommand)]
pub enum EngineCommand {
    /// This engine's version, release line and support window; in a project, its requirement and pin.
    Info(InfoArgs),
    /// Package an engine checkout as a release directory with a verified file manifest.
    Package(PackageArgs),
    /// Check a release directory against its `regia-engine.release.ron`.
    Verify(VerifyArgs),
    /// Plan, cut, pack or verify release chunks under a byte cap (ADR-8404).
    /// Boxed: the chunk arguments are several times the other engine subcommands
    /// (clippy `large_enum_variant`).
    #[command(subcommand)]
    Chunk(Box<super::release_chunk::ChunkCommand>),
}

/// `regia engine info`.
#[derive(Debug, Args)]
pub struct InfoArgs {
    /// Project directory (its section is reported when it holds a `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print the JSON document on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `regia engine package`.
#[derive(Debug, Args)]
pub struct PackageArgs {
    /// Directory the release folder is written into.
    #[arg(long)]
    pub out: PathBuf,
    /// Engine checkout to package (default: the checkout this `regia` runs from, ADR-10544).
    #[arg(long)]
    pub engine: Option<PathBuf>,
    /// Package a git checkout with uncommitted changes to tracked files.
    #[arg(long)]
    pub allow_dirty: bool,
    /// Print the JSON document on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `regia engine verify`.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// The release directory (holds `regia-engine.release.ron`).
    pub dir: PathBuf,
    /// Print the JSON document on stdout.
    #[arg(long)]
    pub json: bool,
}

/// Runs a `regia engine` subcommand.
pub fn run(command: &EngineCommand) -> Result<()> {
    match command {
        EngineCommand::Info(args) => info(args),
        EngineCommand::Package(args) => package(args),
        EngineCommand::Verify(args) => verify(args),
        EngineCommand::Chunk(command) => super::release_chunk::run(command),
    }
}

/// Prints `doc` (pretty JSON) when `json`, then fails when a problem is an error.
fn finish(json: bool, doc: &Value, problems: &[Problem], what: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(doc)?);
    }
    for problem in problems {
        eprintln!(
            "  {} {}: {}",
            problem.severity.as_str(),
            problem.code,
            problem.message
        );
    }
    let errors: Vec<&str> = problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .map(|p| p.code)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{what} failed: {}", errors.join(", "))
    }
}

/// The engine section of `info`.
fn engine_json() -> Value {
    let release = engine_checkout().and_then(|root| distribution::read_release(&root).ok());
    json!({
        "version": engine_version::CURRENT.to_string(),
        "line": engine_version::CURRENT.line().to_string(),
        "oldest_upgradable_line": OLDEST_UPGRADABLE_LINE.to_string(),
        "release_lines": RELEASE_LINES.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "release": release.map(|r| json!({ "version": r.version, "commit": r.commit })),
    })
}

/// The pin section shared by `info` and `upgrade-project`, plus its findings.
pub fn pin_json(project: &Path, pin: &EnginePin, engine: EngineVersion) -> (Value, Vec<Problem>) {
    let sources = pin.sources(project);
    let mut problems = Vec::new();
    if sources.len() > 1 {
        problems.push(Problem::warning(
            "engine_sources_mixed",
            format!(
                "engine crates come from {} sources: {}",
                sources.len(),
                sources.join(", ")
            ),
        ));
    }
    for source in &sources {
        if let Some(found) = source_version(project, source)
            && found != engine
        {
            problems.push(Problem::warning(
                "engine_pin_stale",
                format!(
                    "the project pins {source} (engine {found}); this regia is {engine}: pass \
                     `--engine <source>` to `regia upgrade-project` to move the pin"
                ),
            ));
        }
    }
    let doc = json!({
        "sources": sources,
        "sites": pin.sites.iter().map(|site| json!({
            "file": site.file,
            "line": site.line,
            "crate": site.krate,
            "source": site.engine_source_text(project),
        })).collect::<Vec<_>>(),
    });
    (doc, problems)
}

/// The engine version a pinned source names, when it can be known without the network: a path's
/// release manifest or workspace version, a registry version, a git tag that parses as a version.
fn source_version(project: &Path, source: &str) -> Option<EngineVersion> {
    if let Some(path) = source.strip_prefix("path:") {
        let dir = if distribution::is_absolute_text(path) {
            PathBuf::from(path)
        } else {
            project.join(path)
        };
        return engine_dir_version(&dir);
    }
    if let Some(rest) = source.strip_prefix("registry:") {
        return EngineVersion::parse(rest.rsplit_once('@')?.1).ok();
    }
    if let Some(rest) = source.strip_prefix("git:") {
        return EngineVersion::parse(rest.rsplit_once('#')?.1).ok();
    }
    None
}

/// The project section shared by `info` and `upgrade-project`: requirement, status, crossed lines.
pub fn requirement_json(requirement: Option<&EngineRequirement>, status: &Compatibility) -> Value {
    let crossed = match status {
        Compatibility::UpgradeNeeded { from, to } => engine_version::lines_crossed(*from, *to),
        Compatibility::Unpinned { to } => {
            let mut lines = engine_version::lines_crossed(OLDEST_UPGRADABLE_LINE, *to);
            lines.insert(0, OLDEST_UPGRADABLE_LINE);
            lines.dedup();
            lines
        }
        _ => Vec::new(),
    };
    json!({
        "requirement": requirement.map(|r| r.minimum.to_string()),
        "line": requirement.map(|r| r.line().to_string()),
        "status": status.code(),
        "lines_crossed": crossed.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "release_notes": crossed.iter().map(|l| format!("docs/releases/v{l}.*.md")).collect::<Vec<_>>(),
    })
}

/// The finding for a status `upgrade-project` refuses, if any.
pub fn status_problem(status: &Compatibility) -> Option<Problem> {
    match status {
        Compatibility::EngineTooOld { required } => Some(Problem::error(
            "engine_too_old",
            format!(
                "the project requires engine {required}; this regia is {}: use a newer engine (never downgrade)",
                engine_version::CURRENT
            ),
        )),
        Compatibility::UpgradeUnsupported { from, oldest } => Some(Problem::error(
            "upgrade_unsupported",
            format!(
                "the project targets {from}; this engine upgrades from {oldest} on: upgrade with an older engine first"
            ),
        )),
        _ => None,
    }
}

fn info(args: &InfoArgs) -> Result<()> {
    let mut problems = Vec::new();
    let engine = engine_version::CURRENT;
    eprintln!(
        "engine {engine} (line {}; upgrades from {} on)",
        engine.line(),
        OLDEST_UPGRADABLE_LINE
    );
    // A project is a directory with a manifest, or a quick project's lone `Cargo.toml`.
    let has_manifest = regia_core::brand::is_project_root(&args.project);
    let project = if has_manifest || args.project.join("Cargo.toml").is_file() {
        let mut section = json!({ "dir": args.project.display().to_string(), "engine": null });
        if has_manifest {
            match read_requirement(&args.project) {
                Ok(requirement) => {
                    let status = engine_version::assess(requirement.as_ref(), engine);
                    eprintln!(
                        "project {}: engine >= {} ({})",
                        args.project.display(),
                        requirement.map_or("unset".to_string(), |r| r.minimum.to_string()),
                        status.code()
                    );
                    section["engine"] = requirement_json(requirement.as_ref(), &status);
                    problems.extend(status_problem(&status));
                }
                Err(err) => problems.push(err.into()),
            }
        }
        match read_pin(&args.project) {
            Ok(pin) => {
                let (doc, found) = pin_json(&args.project, &pin, engine);
                for source in pin.sources(&args.project) {
                    eprintln!("  pin: {source}");
                }
                section["pin"] = doc;
                problems.extend(found);
            }
            Err(err) => {
                let warning = err.code == "engine_dep_layout";
                let mut problem: Problem = err.into();
                if warning {
                    problem.severity = Severity::Warning;
                }
                problems.push(problem);
            }
        }
        section
    } else {
        Value::Null
    };
    let ok = problems.iter().all(|p| p.severity == Severity::Warning);
    let doc = json!({
        "schema": JSON_SCHEMA,
        "kind": "regia_engine_info",
        "ok": ok,
        "engine": engine_json(),
        "project": project,
        "problems": problems.iter().map(Problem::to_json).collect::<Vec<_>>(),
    });
    finish(args.json, &doc, &problems, "engine info")
}

fn package(args: &PackageArgs) -> Result<()> {
    let Some(root) = args.engine.clone().or_else(engine_checkout) else {
        bail!("no engine checkout found (REGIA_ENGINE_DIR unset, none around `regia`): pass --engine <dir>");
    };
    let (doc, problems) = match package_release(&root, &args.out, args.allow_dirty) {
        Ok(packaged) => {
            eprintln!(
                "packaged engine {} ({} files, {} crates) into {}",
                packaged.release.version,
                packaged.release.files.len(),
                packaged.release.crates.len(),
                packaged.dir.display()
            );
            let bytes: u64 = packaged.release.files.iter().map(|f| f.size).sum();
            (
                json!({
                    "schema": JSON_SCHEMA,
                    "kind": "regia_engine_package",
                    "ok": true,
                    "dir": packaged.dir.display().to_string(),
                    "version": packaged.release.version,
                    "commit": packaged.release.commit,
                    "files": packaged.release.files.len(),
                    "bytes": bytes,
                    "crates": packaged.release.crates,
                    "problems": [],
                }),
                Vec::new(),
            )
        }
        Err(err) => {
            let problem: Problem = err.into();
            (
                json!({
                    "schema": JSON_SCHEMA,
                    "kind": "regia_engine_package",
                    "ok": false,
                    "problems": [problem.to_json()],
                }),
                vec![problem],
            )
        }
    };
    finish(args.json, &doc, &problems, "engine package")
}

fn verify(args: &VerifyArgs) -> Result<()> {
    let report = verify_release(&args.dir);
    if let Some(release) = &report.release {
        eprintln!(
            "release {} ({} files checked): {}",
            release.version,
            report.checked,
            if report.ok() { "ok" } else { "FAILED" }
        );
    }
    let doc = json!({
        "schema": JSON_SCHEMA,
        "kind": "regia_engine_verify",
        "ok": report.ok(),
        "dir": args.dir.display().to_string(),
        "version": report.release.as_ref().map(|r| r.version.clone()),
        "commit": report.release.as_ref().and_then(|r| r.commit.clone()),
        "checked": report.checked,
        "problems": report.problems.iter().map(Problem::to_json).collect::<Vec<_>>(),
    });
    finish(args.json, &doc, &report.problems, "engine verify")
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::engine_version::ReleaseLine;

    #[test]
    fn the_build_root_is_this_checkout() {
        assert!(engine_checkout().is_some_and(|root| root.join("crates/regia_cli/Cargo.toml").is_file()));
    }

    #[test]
    fn requirement_section_lists_the_crossed_lines_and_notes() {
        let req = EngineRequirement::parse("0.1").expect("req");
        let status = Compatibility::UpgradeNeeded {
            from: ReleaseLine::pre_1_0(1),
            to: ReleaseLine::pre_1_0(2),
        };
        let doc = requirement_json(Some(&req), &status);
        assert_eq!(doc["status"], "upgrade_needed");
        assert_eq!(doc["lines_crossed"], json!(["0.2"]));
        assert_eq!(doc["release_notes"], json!(["docs/releases/v0.2.*.md"]));
        let doc = requirement_json(
            None,
            &Compatibility::Unpinned {
                to: ReleaseLine::pre_1_0(2),
            },
        );
        assert_eq!(doc["status"], "unpinned");
        assert_eq!(doc["lines_crossed"], json!(["0.1", "0.2"]));
        assert!(doc["requirement"].is_null());
    }

    #[test]
    fn refused_statuses_have_problems_and_the_rest_do_not() {
        assert_eq!(
            status_problem(&Compatibility::EngineTooOld {
                required: EngineVersion::new(9, 0, 0)
            })
            .map(|p| p.code),
            Some("engine_too_old")
        );
        assert_eq!(
            status_problem(&Compatibility::UpgradeUnsupported {
                from: ReleaseLine::pre_1_0(0),
                oldest: OLDEST_UPGRADABLE_LINE
            })
            .map(|p| p.code),
            Some("upgrade_unsupported")
        );
        assert!(status_problem(&Compatibility::Current).is_none());
    }

    #[test]
    fn source_versions_are_read_without_the_network() {
        let p = Path::new("/nonexistent");
        assert_eq!(
            source_version(p, "registry:studio@0.2.4"),
            Some(EngineVersion::new(0, 2, 4))
        );
        assert_eq!(
            source_version(p, "git:https://x/r.git#v0.2.3"),
            Some(EngineVersion::new(0, 2, 3))
        );
        assert_eq!(source_version(p, "git:https://x/r.git#main"), None);
        assert_eq!(source_version(p, "path:/nonexistent/engine"), None);
        assert_eq!(
            source_version(
                p,
                &format!("path:{}", engine_checkout().unwrap_or_default().display())
            ),
            Some(engine_version::CURRENT)
        );
    }
}
