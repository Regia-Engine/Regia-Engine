// SPDX-License-Identifier: MPL-2.0
//! The project's texture cook quality (`docs/specs/21-tooling-and-packaging.md` § 2, ADR-7217):
//! the `[cook] texture_quality` key of `Regia.toml`.
//!
//! ```toml
//! [cook]
//! texture_quality = "final"   # "final" (default) or "draft"
//! ```
//!
//! `"final"` cooks every texture whose `.meta` names no `quality` at
//! [`TextureCookQuality::High`] (BC7 on desktop, ASTC 4×4 on Android), as before the key was read.
//! `"draft"` cooks them at [`TextureCookQuality::Low`] (BC1 / BC3 for colour, the low ASTC ladder),
//! which encodes faster and writes smaller paks for iteration. A texture's own `.meta` `quality`
//! wins either way. The setting covers the generic texture cook (standalone images, DDS, glTF and
//! converted-model material maps); terrain layers keep their own fixed quality.
//!
//! The cook applies it with [`with_default_texture_quality`] around each source's import, and keys
//! the cache on it only when it is `"draft"`, so caches cooked before the key existed stay valid.

use std::cell::Cell;
use std::path::Path;

use crate::texture_compress::TextureCookQuality;

/// The values of `[cook] texture_quality`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectTextureQuality {
    /// Shipping quality: [`TextureCookQuality::High`] unless a `.meta` says otherwise.
    #[default]
    Final,
    /// Iteration quality: [`TextureCookQuality::Low`] unless a `.meta` says otherwise.
    Draft,
}

impl ProjectTextureQuality {
    /// `final` or `draft`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Final => "final",
            Self::Draft => "draft",
        }
    }

    /// Parses [`Self::label`].
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "final" => Some(Self::Final),
            "draft" => Some(Self::Draft),
            _ => None,
        }
    }

    /// The cook quality of a texture whose `.meta` names none.
    #[must_use]
    pub fn cook_quality(self) -> TextureCookQuality {
        match self {
            Self::Final => TextureCookQuality::High,
            Self::Draft => TextureCookQuality::Low,
        }
    }

    /// The text the cook's cache key adds: empty for `final` (caches cooked before the key existed
    /// stay valid), else `;texture_quality=<label>`.
    #[must_use]
    pub fn key_text(self) -> String {
        match self {
            Self::Final => String::new(),
            other => format!(";texture_quality={}", other.label()),
        }
    }

    /// The setting of the `Regia.toml` text `manifest` (`final` when the key or the `[cook]` table
    /// is absent).
    ///
    /// # Errors
    /// The TOML does not parse, `[cook]` is not a table, or `texture_quality` is not one of the
    /// two strings (the message names the key and the value).
    pub fn from_manifest_toml(manifest: &str) -> Result<Self, String> {
        let root: toml::Table = manifest.parse().map_err(|e| format!("Regia.toml: {e}"))?;
        let Some(cook) = root.get("cook") else {
            return Ok(Self::default());
        };
        let cook = cook.as_table().ok_or("Regia.toml: [cook] must be a table")?;
        match cook.get("texture_quality") {
            None => Ok(Self::default()),
            Some(toml::Value::String(value)) => Self::from_label(value).ok_or_else(|| {
                format!("Regia.toml: [cook] texture_quality `{value}` is not `final` or `draft`")
            }),
            Some(_) => Err("Regia.toml: [cook] texture_quality must be a string (`final` or `draft`)".into()),
        }
    }

    /// The setting of `<project_root>/Regia.toml` (`final` when the file is missing).
    ///
    /// # Errors
    /// As [`Self::from_manifest_toml`]; an unreadable (not missing) manifest.
    pub fn for_project(project_root: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(project_root.join("Regia.toml")) {
            Ok(text) => Self::from_manifest_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Regia.toml: {e}")),
        }
    }
}

thread_local! {
    static ACTIVE: Cell<TextureCookQuality> = const { Cell::new(TextureCookQuality::High) };
}

/// Runs `f` with `quality` as the fallback texture quality of the cook on this thread (what a
/// texture without a `.meta` `quality` cooks at), restoring the previous one afterwards (also
/// when `f` panics).
pub fn with_default_texture_quality<R>(quality: TextureCookQuality, f: impl FnOnce() -> R) -> R {
    struct Restore(TextureCookQuality);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(self.0));
        }
    }
    let _restore = Restore(ACTIVE.with(|active| active.replace(quality)));
    f()
}

/// The fallback texture quality active on this thread ([`with_default_texture_quality`];
/// [`TextureCookQuality::High`] outside one).
#[must_use]
pub fn default_texture_quality() -> TextureCookQuality {
    ACTIVE.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_reads_final_and_draft_and_defaults_to_final() {
        let parse = ProjectTextureQuality::from_manifest_toml;
        assert_eq!(
            parse("[project]\nname = \"x\"\n"),
            Ok(ProjectTextureQuality::Final)
        );
        assert_eq!(
            parse("[cook]\nchunks = { base = [\"**\"] }\n"),
            Ok(ProjectTextureQuality::Final)
        );
        assert_eq!(
            parse("[cook]\ntexture_quality = \"final\"\n"),
            Ok(ProjectTextureQuality::Final)
        );
        assert_eq!(
            parse("[cook]\ntexture_quality = \"draft\"\n"),
            Ok(ProjectTextureQuality::Draft)
        );
        assert_eq!(ProjectTextureQuality::Final.key_text(), "");
        assert_eq!(ProjectTextureQuality::Draft.key_text(), ";texture_quality=draft");
        assert_eq!(
            ProjectTextureQuality::Draft.cook_quality(),
            TextureCookQuality::Low
        );
        assert_eq!(
            ProjectTextureQuality::Final.cook_quality(),
            TextureCookQuality::High
        );
    }

    #[test]
    fn bad_values_are_errors_that_name_the_key() {
        for (toml, needle) in [
            ("[cook]\ntexture_quality = \"ultra\"\n", "texture_quality `ultra`"),
            (
                "[cook]\ntexture_quality = 1\n",
                "texture_quality must be a string",
            ),
            ("cook = 3\n", "[cook] must be a table"),
            ("not toml [", "Regia.toml"),
        ] {
            let err = ProjectTextureQuality::from_manifest_toml(toml).expect_err(toml);
            assert!(err.contains(needle), "{toml}: {err}");
        }
    }

    #[test]
    fn the_scope_restores_the_previous_quality() {
        assert_eq!(default_texture_quality(), TextureCookQuality::High);
        let inner = with_default_texture_quality(TextureCookQuality::Low, default_texture_quality);
        assert_eq!(inner, TextureCookQuality::Low);
        assert_eq!(default_texture_quality(), TextureCookQuality::High);
        let caught = std::panic::catch_unwind(|| {
            with_default_texture_quality(TextureCookQuality::Low, || panic!("boom"))
        });
        assert!(caught.is_err());
        assert_eq!(default_texture_quality(), TextureCookQuality::High);
    }
}
