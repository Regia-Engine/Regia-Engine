// SPDX-License-Identifier: MPL-2.0
//! `regia new` (`docs/specs/21` § 1). The scaffolding lives in [`regia_cli::new_project`]
//! so the editor's project browser calls the same function.

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use regia_cli::distribution::EngineSource;

/// Create a project from a template.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// Project display name. The directory and crate name are the snake_case form.
    pub name: String,
    /// Template id: `blank`, `first_person`, `third_person`, `top_down`, `quick` (one crate, one
    /// `main.rs`), or `net-game` (also `net_game`: dedicated server, client and game crates).
    #[arg(long, default_value = "blank")]
    pub template: String,
    /// Directory to create the project in (default: the current directory).
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Where the project's engine crates come from (spec 94): `path:<dir>` (an engine checkout or a
    /// release; relative = inside the project), `git:<url>#<tag>` or `registry:<name>[@<version>]`.
    /// Default: the engine checkout this `regia` runs from (`REGIA_ENGINE_DIR`, or the checkout
    /// around the executable); a downloaded `regia` outside any checkout uses the public repository
    /// at this version's tag, `git:https://github.com/Regia-Engine/Regia-Engine#v<version>`.
    #[arg(long)]
    pub engine_source: Option<String>,
}

/// Creates the project and prints its root.
pub fn run(args: &NewArgs) -> anyhow::Result<()> {
    let parent = match &args.path {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    // Parse the source before anything is written.
    let given = args
        .engine_source
        .as_deref()
        .map(|text| EngineSource::parse(text, regia_core::engine_version::CURRENT))
        .transpose()?;
    let checkout = regia_cli::new_project::engine_checkout();
    let source = default_source(given, checkout.is_some());
    let request = regia_cli::new_project::NewProjectRequest {
        name: args.name.clone(),
        template: args.template.clone(),
        parent,
    };
    // Without a local checkout the templates take the source's text, and the re-pin below
    // replaces it in every manifest (ADR-10544).
    let created = match (&checkout, &source) {
        (None, Some(source)) => regia_cli::new_project::create_with_engine(&request, &source.to_string())?,
        _ => regia_cli::new_project::create(&request)?,
    };
    if let Some(source) = &source {
        let edits = regia_cli::distribution::rewrite_pin(&created.root, source).with_context(|| {
            format!(
                "created {} with the default engine pin, but cannot pin it to {source}",
                created.root.display()
            )
        })?;
        for edit in &edits {
            let path = created.root.join(&edit.file);
            std::fs::write(&path, &edit.text).with_context(|| format!("cannot write {}", path.display()))?;
        }
        eprintln!("engine: {source}");
    }
    // The engine's tested dependency versions (spec 94 § 3.2): from the path engine, or the
    // checkout this `regia` runs from; git and registry sources carry no local lockfile.
    let engine_dir = match &source {
        Some(source) => source.local_dir(&created.root),
        None => checkout,
    };
    if let Some(dir) = engine_dir
        && regia_cli::distribution::seed_lockfile(&created.root, &dir)?
    {
        eprintln!("Cargo.lock: seeded from {}", dir.display());
    }
    println!("{}", created.root.display());
    Ok(())
}

/// The engine source `regia new` pins: the one given; none (the local checkout) when this `regia`
/// runs from a checkout; else the public repository at this version's tag (ADR-10544), so a
/// downloaded release creates a project that builds.
fn default_source(given: Option<EngineSource>, has_checkout: bool) -> Option<EngineSource> {
    given.or_else(|| {
        (!has_checkout).then(|| EngineSource::Git {
            url: crate::commands::package::package_legal::ENGINE_SOURCE_URL.to_owned(),
            tag: format!("v{}", regia_core::engine_version::CURRENT),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-10544: a release `regia` outside any checkout pins the public repository at its own tag;
    /// inside a checkout, or with `--engine-source`, nothing changes.
    #[test]
    fn a_regia_without_a_checkout_pins_the_public_tag() {
        let version = regia_core::engine_version::CURRENT;
        assert_eq!(
            default_source(None, false),
            Some(EngineSource::Git {
                url: "https://github.com/Regia-Engine/Regia-Engine".to_owned(),
                tag: format!("v{version}"),
            })
        );
        assert_eq!(
            default_source(None, false).map(|s| s.to_string()),
            Some(format!(
                "git:https://github.com/Regia-Engine/Regia-Engine#v{version}"
            ))
        );
        assert_eq!(default_source(None, true), None);
        let given = EngineSource::parse("path:/opt/engine", version).ok();
        assert_eq!(default_source(given.clone(), false), given);
        assert_eq!(default_source(given.clone(), true), given);
    }

    /// ADR-10544: a project created without a local checkout is re-pinned to the git tag in every
    /// manifest; no engine dependency keeps the placeholder path.
    #[test]
    fn a_project_without_a_checkout_builds_against_the_git_tag() -> anyhow::Result<()> {
        let parent = regia_test_utils::unique_temp_dir("new-no-checkout");
        let Some(source) = default_source(None, false) else {
            anyhow::bail!("no default source");
        };
        let created = regia_cli::new_project::create_with_engine(
            &regia_cli::new_project::NewProjectRequest {
                name: "Downloaded".into(),
                template: "blank".into(),
                parent: parent.clone(),
            },
            &source.to_string(),
        )?;
        let edits = regia_cli::distribution::rewrite_pin(&created.root, &source)?;
        assert!(!edits.is_empty());
        for edit in &edits {
            std::fs::write(created.root.join(&edit.file), &edit.text)?;
        }
        let game = std::fs::read_to_string(created.root.join("game/Cargo.toml"))?;
        let version = regia_core::engine_version::CURRENT;
        assert!(
            game.contains("https://github.com/Regia-Engine/Regia-Engine"),
            "{game}"
        );
        assert!(game.contains(&format!("\"v{version}\"")), "{game}");
        let pin = regia_cli::distribution::read_pin(&created.root)?;
        let sources = pin.sources(&created.root);
        assert_eq!(sources, vec![source.to_string()], "{game}");
        let _ = std::fs::remove_dir_all(&parent);
        Ok(())
    }
}
