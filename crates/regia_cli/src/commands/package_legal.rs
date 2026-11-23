// SPDX-License-Identifier: MPL-2.0
//! The engine's licence notice and the default engine credit that `regia package` writes into every
//! package (ADR-10225, book: Licensing).
//!
//! Regia Engine is under the Mozilla Public License 2.0. MPL § 3.2 asks whoever distributes the
//! engine in executable form to tell the recipients where the engine's source code is. Every package
//! therefore carries:
//!
//! - [`NOTICE_FILE`]: the MPL notice, the engine version and the source-location line. The location is
//!   `[package] engine_source_url` in `Regia.toml` (a studio that changed engine files points it at its
//!   own published copy) or the engine's [`ENGINE_SOURCE_URL`], the public repository. A manifest that
//!   still names the old placeholder [`SOURCE_PLACEHOLDER`] (the "not set" value) makes a shipping
//!   package warn ([`release_warning`]);
//! - [`LICENSE_FILE`]: the verbatim MPL-2.0 text (the repository's `LICENSE`).
//!
//! The credit is not a licence condition. By default [`CREDITS_FILE`] gets the line [`ENGINE_CREDIT`]
//! (after the project's own `CREDITS.txt`, when it has one); `regia package --no-engine-credit` or
//! `[package] engine_credit = false` leaves it out.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// The verbatim MPL-2.0 text of the engine: the crate's copy of the repository's `LICENSE` (inside the
/// crate so `cargo package` and `cargo vendor` keep it, ADR-1485; a test keeps the two byte-identical).
pub const ENGINE_LICENSE_TEXT: &str = include_str!("../../assets/LICENSE-MPL-2.0.txt");

/// The engine notice in a package: MPL notice, engine version, source location.
pub const NOTICE_FILE: &str = "REGIA-ENGINE-NOTICE.txt";

/// The engine's licence text in a package.
pub const LICENSE_FILE: &str = "REGIA-ENGINE-LICENSE.txt";

/// The credits text in a package (the project's `CREDITS.txt` plus the engine credit line).
pub const CREDITS_FILE: &str = "CREDITS.txt";

/// The default engine credit line.
pub const ENGINE_CREDIT: &str = "Made with Regia Engine";

/// The "not set" source location (`<release host>`), the engine default before the public repository
/// existed. A project manifest that still says it makes a shipping package warn.
pub const SOURCE_PLACEHOLDER: &str = "<release host>";

/// The engine setting for the public source location of this engine build: the public repository
/// (owner decision 2026-11-20). `xtask` `dist::ENGINE_SOURCE_URL` names the same place.
pub const ENGINE_SOURCE_URL: &str = "https://github.com/Regia-Engine/Regia-Engine";

/// What the legal files of one package say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legal {
    /// Write [`ENGINE_CREDIT`] into [`CREDITS_FILE`].
    pub engine_credit: bool,
    /// Where the engine's source code is (MPL § 3.2).
    pub source_url: String,
}

impl Legal {
    /// The settings of a project: `[package] engine_credit` (bool, default `true`) and
    /// `[package] engine_source_url` (string, default [`ENGINE_SOURCE_URL`]) from `Regia.toml`;
    /// `no_engine_credit` (the CLI flag) wins over the manifest.
    ///
    /// # Errors
    /// An unreadable manifest, invalid TOML, or a key of the wrong type or an empty URL.
    pub fn resolve(project_dir: &Path, no_engine_credit: bool) -> Result<Self> {
        let text = match regia_core::brand::project_manifest(project_dir) {
            Some(path) => {
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
            }
            None => String::new(),
        };
        let mut legal = Self::from_manifest(&text)?;
        if no_engine_credit {
            legal.engine_credit = false;
        }
        Ok(legal)
    }

    /// [`Legal::resolve`] on manifest text.
    ///
    /// # Errors
    /// Invalid TOML, or a key of the wrong type, or an empty URL.
    pub fn from_manifest(text: &str) -> Result<Self> {
        let table: toml::Table = toml::from_str(text).context("Regia.toml is not valid TOML")?;
        let package = table.get("package").and_then(toml::Value::as_table);
        let engine_credit = match package.and_then(|p| p.get("engine_credit")) {
            None => true,
            Some(toml::Value::Boolean(on)) => *on,
            Some(_) => bail!("Regia.toml [package] engine_credit must be true or false"),
        };
        let source_url = match package.and_then(|p| p.get("engine_source_url")) {
            None => ENGINE_SOURCE_URL.to_owned(),
            Some(toml::Value::String(url)) if !url.trim().is_empty() => url.trim().to_owned(),
            Some(_) => bail!("Regia.toml [package] engine_source_url must be a non-empty string"),
        };
        Ok(Self {
            engine_credit,
            source_url,
        })
    }

    /// `true` while the source location is still the placeholder.
    pub fn source_is_placeholder(&self) -> bool {
        self.source_url == SOURCE_PLACEHOLDER
    }
}

/// The warning a shipping (release) package prints while the source location is the placeholder, or
/// `None`.
pub fn release_warning(legal: &Legal, shipping: bool) -> Option<String> {
    (shipping && legal.source_is_placeholder()).then(|| {
        format!(
            "{NOTICE_FILE} says the engine source is at `{SOURCE_PLACEHOLDER}`: the source location is not \
             set. Before you distribute this build, set `[package] engine_source_url` in Regia.toml to where \
             the Regia Engine source (with any changes you made to its files) is published (MPL-2.0 § 3.2)"
        )
    })
}

/// The text of [`NOTICE_FILE`].
pub fn notice_text(legal: &Legal) -> String {
    format!(
        "Regia Engine notice\n\
         ===================\n\
         \n\
         This program includes Regia Engine {version}.\n\
         \n\
         Regia Engine's source code files are subject to the terms of the Mozilla Public License,\n\
         v. 2.0 (MPL-2.0). The licence text is in {LICENSE_FILE} beside this file; you can also\n\
         obtain one at https://mozilla.org/MPL/2.0/.\n\
         \n\
         Source code: the Source Code Form of the Regia Engine files in this program is available at\n\
         {url}\n\
         \n\
         This notice covers Regia Engine's own files only. The game's own code and content are under\n\
         the game's own terms. Third-party components and their licences are listed in\n\
         THIRD_PARTY_NOTICES.md.\n",
        version = env!("CARGO_PKG_VERSION"),
        url = legal.source_url,
    )
}

/// The text of [`CREDITS_FILE`]: the project's own credits (`<project>/CREDITS.txt`), then the engine
/// credit line when it is on and not already there. `None` when there is nothing to write.
pub fn credits_text(project_credits: Option<&str>, engine_credit: bool) -> Option<String> {
    let mut text = project_credits.unwrap_or_default().to_owned();
    if engine_credit && !text.lines().any(|line| line.trim() == ENGINE_CREDIT) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(ENGINE_CREDIT);
        text.push('\n');
    }
    (!text.is_empty()).then_some(text)
}

/// Writes [`NOTICE_FILE`], [`LICENSE_FILE`] and (when there is any credit) [`CREDITS_FILE`] into `dest`.
///
/// # Errors
/// An unreadable project `CREDITS.txt` or a write failure.
pub fn write(dest: &Path, project_dir: &Path, legal: &Legal) -> Result<()> {
    let put = |name: &str, text: &str| -> Result<()> {
        let path = dest.join(name);
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    };
    put(NOTICE_FILE, &notice_text(legal))?;
    put(LICENSE_FILE, ENGINE_LICENSE_TEXT)?;
    let own = project_dir.join(CREDITS_FILE);
    let project_credits = match std::fs::read_to_string(&own) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", own.display())),
    };
    match credits_text(project_credits.as_deref(), legal.engine_credit) {
        Some(text) => put(CREDITS_FILE, &text)?,
        None => {
            // A credits file from an earlier package into the same directory must not survive.
            let stale = dest.join(CREDITS_FILE);
            if stale.is_file() {
                std::fs::remove_file(&stale).with_context(|| format!("removing {}", stale.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_licence_text_is_the_mpl() {
        assert!(ENGINE_LICENSE_TEXT.starts_with("Mozilla Public License Version 2.0\n"));
        assert!(ENGINE_LICENSE_TEXT.contains("Exhibit B - \"Incompatible With Secondary Licenses\" Notice"));
    }

    #[test]
    fn the_crate_copy_of_the_licence_matches_the_repository_file() -> Result<()> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.join("../../LICENSE");
        if !root.is_file() {
            return Ok(()); // a packaged or vendored copy of the crate has no repository around it
        }
        assert_eq!(
            ENGINE_LICENSE_TEXT,
            std::fs::read_to_string(root)?,
            "copy LICENSE to crates/regia_cli/assets/LICENSE-MPL-2.0.txt"
        );
        Ok(())
    }

    #[test]
    fn manifest_keys_and_the_flag() -> Result<()> {
        let default = Legal::from_manifest("[project]\nname = \"g\"\n")?;
        assert!(default.engine_credit);
        assert_eq!(default.source_url, ENGINE_SOURCE_URL);
        let off = Legal::from_manifest(
            "[package]\nengine_credit = false\nengine_source_url = \" https://x.invalid/src \"\n",
        )?;
        assert!(!off.engine_credit);
        assert_eq!(off.source_url, "https://x.invalid/src");
        assert!(Legal::from_manifest("[package]\nengine_credit = \"no\"\n").is_err());
        assert!(Legal::from_manifest("[package]\nengine_source_url = \"\"\n").is_err());
        assert!(Legal::from_manifest("[package]\nengine_source_url = 3\n").is_err());
        // Other `[package]` keys (the hygiene stage rules) are not this module's business.
        assert!(Legal::from_manifest("[package]\nstage_allow = ['content/**']\n")?.engine_credit);

        let dir = regia_test_utils::unique_temp_dir("pkg_legal_resolve");
        std::fs::write(dir.join("Regia.toml"), "[package]\nengine_credit = true\n")?;
        assert!(Legal::resolve(&dir, false)?.engine_credit);
        assert!(!Legal::resolve(&dir, true)?.engine_credit, "the flag wins");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn the_engine_default_is_the_public_repository_and_does_not_warn() -> Result<()> {
        assert_eq!(ENGINE_SOURCE_URL, "https://github.com/Regia-Engine/Regia-Engine");
        assert_ne!(ENGINE_SOURCE_URL, SOURCE_PLACEHOLDER);
        let default = Legal::from_manifest("")?;
        assert!(!default.source_is_placeholder());
        assert!(release_warning(&default, true).is_none());
        assert!(notice_text(&default).contains("\nhttps://github.com/Regia-Engine/Regia-Engine\n"));
        // A project that still names the old placeholder is "not set": a shipping package warns.
        let stale = Legal::from_manifest("[package]\nengine_source_url = \"<release host>\"\n")?;
        assert!(stale.source_is_placeholder());
        assert!(release_warning(&stale, true).is_some());
        Ok(())
    }

    #[test]
    fn release_warning_only_for_shipping_with_the_placeholder() {
        let placeholder = Legal {
            engine_credit: true,
            source_url: SOURCE_PLACEHOLDER.into(),
        };
        let set = Legal {
            engine_credit: true,
            source_url: "https://x.invalid/src".into(),
        };
        assert!(release_warning(&placeholder, true).is_some_and(|w| w.contains("engine_source_url")));
        assert!(release_warning(&placeholder, false).is_none());
        assert!(release_warning(&set, true).is_none());
    }

    #[test]
    fn notice_names_the_licence_and_the_source() {
        let text = notice_text(&Legal {
            engine_credit: false,
            source_url: "https://x.invalid/regia".into(),
        });
        assert!(text.contains("Mozilla Public License"));
        assert!(text.contains("MPL-2.0"));
        assert!(text.contains(LICENSE_FILE));
        assert!(text.contains("\nhttps://x.invalid/regia\n"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn credits_default_on_kept_once_and_removable() {
        assert_eq!(
            credits_text(None, true).as_deref(),
            Some("Made with Regia Engine\n")
        );
        assert_eq!(credits_text(None, false), None);
        assert_eq!(
            credits_text(Some("Game by Someone"), true).as_deref(),
            Some("Game by Someone\n\nMade with Regia Engine\n")
        );
        assert_eq!(
            credits_text(Some("Game\n\nMade with Regia Engine\n"), true).as_deref(),
            Some("Game\n\nMade with Regia Engine\n"),
            "never twice"
        );
        assert_eq!(credits_text(Some("Game\n"), false).as_deref(), Some("Game\n"));
    }

    #[test]
    fn write_puts_the_three_files_and_drops_a_stale_credit() -> Result<()> {
        let project = regia_test_utils::unique_temp_dir("pkg_legal_project");
        let dest = regia_test_utils::unique_temp_dir("pkg_legal_dest");
        let on = Legal {
            engine_credit: true,
            source_url: SOURCE_PLACEHOLDER.into(),
        };
        write(&dest, &project, &on)?;
        assert!(std::fs::read_to_string(dest.join(NOTICE_FILE))?.contains(SOURCE_PLACEHOLDER));
        assert_eq!(
            std::fs::read_to_string(dest.join(LICENSE_FILE))?,
            ENGINE_LICENSE_TEXT
        );
        assert_eq!(
            std::fs::read_to_string(dest.join(CREDITS_FILE))?,
            "Made with Regia Engine\n"
        );
        let off = Legal {
            engine_credit: false,
            ..on.clone()
        };
        write(&dest, &project, &off)?;
        assert!(!dest.join(CREDITS_FILE).exists(), "no credits left behind");
        std::fs::write(project.join(CREDITS_FILE), "Art: A\n")?;
        write(&dest, &project, &off)?;
        assert_eq!(std::fs::read_to_string(dest.join(CREDITS_FILE))?, "Art: A\n");
        write(&dest, &project, &on)?;
        assert_eq!(
            std::fs::read_to_string(dest.join(CREDITS_FILE))?,
            "Art: A\n\nMade with Regia Engine\n"
        );
        std::fs::remove_dir_all(&project)?;
        std::fs::remove_dir_all(&dest)?;
        Ok(())
    }
}
