// SPDX-License-Identifier: MPL-2.0
//! The project's texture cook policy per target platform (ADR-2063, `docs/specs/12-assets.md`
//! § 7.1): which block family an Android cook writes (ASTC or ETC2 / EAC) and which ASTC
//! profile (block ladder) it uses. Read from `Regia.toml`:
//!
//! ```toml
//! [cook.textures.android]
//! format = "astc"            # "astc" (default) or "etc2"
//! astc_profile = "per_texture" # "per_texture" (default), "high" or "low"
//! ```
//!
//! The policy covers the generic texture cook (standalone images, glTF material maps, the packed
//! detail and aux maps) and the terrain material cook ([`crate::cook_terrain_material`]). Desktop
//! cooks write BCn whatever the policy says.
//!
//! The cook applies it with [`with_texture_policy`] around each source's import; a host that calls
//! [`crate::cook_texture_rgba8`] itself can do the same.

use std::cell::Cell;
use std::path::Path;

use crate::texture_compress::{AndroidTextureFormat, TextureCookQuality};

/// Which ASTC blocks an Android cook picks (the block ladder, `specs/12` § 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AstcProfile {
    /// Each texture's own `.meta` `quality` decides: `high` (the default) 4×4, `low` 6×6 (8×8
    /// for masks, and where 6 does not divide the size; normals stay 4×4).
    #[default]
    PerTexture,
    /// 4×4 for every texture (8 bits a texel), whatever its `.meta` says.
    High,
    /// The `low` ladder for every texture: 6×6 (3.56 bits) or 8×8 (2 bits); normals 4×4.
    Low,
}

impl AstcProfile {
    /// `per_texture`, `high` or `low`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::PerTexture => "per_texture",
            Self::High => "high",
            Self::Low => "low",
        }
    }

    /// Parses [`Self::label`].
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "per_texture" => Some(Self::PerTexture),
            "high" => Some(Self::High),
            "low" => Some(Self::Low),
            _ => None,
        }
    }

    /// The quality a texture whose `.meta` asks for `quality` cooks at under this profile.
    #[must_use]
    pub fn quality(self, quality: TextureCookQuality) -> TextureCookQuality {
        match self {
            Self::PerTexture => quality,
            Self::High => TextureCookQuality::High,
            Self::Low => TextureCookQuality::Low,
        }
    }
}

/// The texture cook policy of one platform (ADR-2063).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize)]
pub struct TexturePolicy {
    /// The Android block family: ASTC (default) or ETC2 / EAC.
    pub android_format: AndroidTextureFormat,
    /// The ASTC block ladder (ignored for ETC2, which has one bit rate).
    pub astc_profile: AstcProfile,
}

/// The `Regia.toml` keys of `[cook.textures.android]`.
const KEYS: [&str; 2] = ["format", "astc_profile"];

impl TexturePolicy {
    /// The policy `platform` cooks with under the `Regia.toml` text `manifest`: the
    /// `[cook.textures.<platform>]` table, or the default. Only `android` has settings.
    ///
    /// # Errors
    /// The TOML does not parse, or `[cook.textures]` holds an unknown platform, key or value
    /// (the message names it).
    pub fn from_manifest_toml(manifest: &str, platform: &str) -> Result<Self, String> {
        let root: toml::Table = manifest.parse().map_err(|e| format!("Regia.toml: {e}"))?;
        let Some(textures) = root
            .get("cook")
            .and_then(toml::Value::as_table)
            .and_then(|cook| cook.get("textures"))
        else {
            return Ok(Self::default());
        };
        let textures = textures
            .as_table()
            .ok_or("Regia.toml: [cook.textures] must be a table")?;
        for name in textures.keys() {
            if name != "android" {
                return Err(format!(
                    "Regia.toml: [cook.textures.{name}]: only `android` has texture settings (desktop cooks write BCn)"
                ));
            }
        }
        let Some(android) = textures.get("android") else {
            return Ok(Self::default());
        };
        let android = android
            .as_table()
            .ok_or("Regia.toml: [cook.textures.android] must be a table")?;
        for key in android.keys() {
            if !KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "Regia.toml: [cook.textures.android] has unknown key `{key}` (known: {})",
                    KEYS.join(", ")
                ));
            }
        }
        let text = |key: &str| -> Result<Option<&str>, String> {
            match android.get(key) {
                None => Ok(None),
                Some(toml::Value::String(s)) => Ok(Some(s.as_str())),
                Some(_) => Err(format!(
                    "Regia.toml: [cook.textures.android] {key} must be a string"
                )),
            }
        };
        let mut policy = Self::default();
        if let Some(format) = text("format")? {
            policy.android_format = AndroidTextureFormat::from_label(format).ok_or_else(|| {
                format!("Regia.toml: [cook.textures.android] format `{format}` is not `astc` or `etc2`")
            })?;
        }
        if let Some(profile) = text("astc_profile")? {
            policy.astc_profile = AstcProfile::from_label(profile).ok_or_else(|| {
                format!(
                    "Regia.toml: [cook.textures.android] astc_profile `{profile}` is not `per_texture`, `high` or `low`"
                )
            })?;
        }
        if platform == "android" {
            Ok(policy)
        } else {
            Ok(Self::default())
        }
    }

    /// The policy of `platform` from `<project_root>/Regia.toml` (the default when the file is
    /// missing).
    ///
    /// # Errors
    /// As [`Self::from_manifest_toml`]; an unreadable (not missing) manifest.
    pub fn for_project(project_root: &Path, platform: &str) -> Result<Self, String> {
        match std::fs::read_to_string(project_root.join("Regia.toml")) {
            Ok(text) => Self::from_manifest_toml(&text, platform),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Regia.toml: {e}")),
        }
    }

    /// Whether `family` is the block family this policy writes on `platform`.
    ///
    /// Uncompressed textures match every platform. `Other` is not a mismatch by itself.
    /// Android must match [`AndroidTextureFormat`] (ASTC or ETC2, not either). Desktop
    /// (`linux`, `windows`, `macos`) expects BC. Any other platform name accepts every family.
    #[must_use]
    pub fn accepts_family(self, family: crate::texture_stats::TextureFamily, platform: &str) -> bool {
        use crate::texture_stats::TextureFamily;
        match family {
            TextureFamily::Uncompressed | TextureFamily::Other => true,
            TextureFamily::Astc | TextureFamily::Bc | TextureFamily::Etc2 => match platform {
                "android" => match self.android_format {
                    AndroidTextureFormat::Astc => family == TextureFamily::Astc,
                    AndroidTextureFormat::Etc2 => family == TextureFamily::Etc2,
                },
                "linux" | "windows" | "macos" => family == TextureFamily::Bc,
                _ => true,
            },
        }
    }

    /// The text the Android cache key adds for this policy: empty for the default (so caches
    /// cooked before the policy existed stay valid), else the non-default settings.
    #[must_use]
    pub fn key_text(self) -> String {
        if self == Self::default() {
            return String::new();
        }
        format!(
            ";format={};astc_profile={}",
            self.android_format.label(),
            self.astc_profile.label()
        )
    }
}

thread_local! {
    static ACTIVE: Cell<TexturePolicy> = const {
        Cell::new(TexturePolicy {
            android_format: AndroidTextureFormat::Astc,
            astc_profile: AstcProfile::PerTexture,
        })
    };
}

/// Runs `f` with `policy` as the texture policy of the Android cook functions on this thread
/// ([`crate::cook_texture_rgba8`] and the importers built on it), restoring the previous one
/// afterwards (also when `f` panics).
pub fn with_texture_policy<R>(policy: TexturePolicy, f: impl FnOnce() -> R) -> R {
    struct Restore(TexturePolicy);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(self.0));
        }
    }
    let _restore = Restore(ACTIVE.with(|active| active.replace(policy)));
    f()
}

/// The policy active on this thread ([`with_texture_policy`]; the default outside one).
#[must_use]
pub fn active_texture_policy() -> TexturePolicy {
    ACTIVE.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture_stats::TextureFamily;

    #[test]
    fn android_accepts_only_the_policy_family() {
        let astc = TexturePolicy::default();
        assert!(astc.accepts_family(TextureFamily::Astc, "android"));
        assert!(!astc.accepts_family(TextureFamily::Bc, "android"));
        assert!(!astc.accepts_family(TextureFamily::Etc2, "android"));
        assert!(astc.accepts_family(TextureFamily::Uncompressed, "android"));
        assert!(astc.accepts_family(TextureFamily::Other, "android"));
        assert!(astc.accepts_family(TextureFamily::Bc, "linux"));
        assert!(!astc.accepts_family(TextureFamily::Astc, "windows"));
        let etc2 = TexturePolicy {
            android_format: AndroidTextureFormat::Etc2,
            ..TexturePolicy::default()
        };
        assert!(etc2.accepts_family(TextureFamily::Etc2, "android"));
        assert!(!etc2.accepts_family(TextureFamily::Astc, "android"));
    }

    #[test]
    fn the_manifest_table_sets_the_android_policy() {
        let toml =
            "[project]\nname = \"x\"\n[cook.textures.android]\nformat = \"etc2\"\nastc_profile = \"low\"\n";
        let policy = TexturePolicy::from_manifest_toml(toml, "android").expect("parses");
        assert_eq!(policy.android_format, AndroidTextureFormat::Etc2);
        assert_eq!(policy.astc_profile, AstcProfile::Low);
        assert_eq!(policy.key_text(), ";format=etc2;astc_profile=low");
        // Desktop platforms cook BCn: the table does not apply.
        assert_eq!(
            TexturePolicy::from_manifest_toml(toml, "linux").expect("parses"),
            TexturePolicy::default()
        );
        // No table: the default, whose key text is empty (existing caches stay valid).
        let none = TexturePolicy::from_manifest_toml("[project]\nname = \"x\"\n", "android").expect("parses");
        assert_eq!(none, TexturePolicy::default());
        assert_eq!(none.key_text(), "");
    }

    #[test]
    fn bad_settings_are_errors_that_name_the_key() {
        for (toml, needle) in [
            ("[cook.textures.android]\nformat = \"pvrtc\"\n", "format `pvrtc`"),
            (
                "[cook.textures.android]\nastc_profile = \"medium\"\n",
                "astc_profile `medium`",
            ),
            (
                "[cook.textures.android]\nfromat = \"etc2\"\n",
                "unknown key `fromat`",
            ),
            ("[cook.textures.android]\nformat = 2\n", "format must be a string"),
            (
                "[cook.textures.windows]\nformat = \"etc2\"\n",
                "[cook.textures.windows]",
            ),
            ("[cook]\ntextures = 3\n", "must be a table"),
            ("not toml [", "Regia.toml"),
        ] {
            let err = TexturePolicy::from_manifest_toml(toml, "android").expect_err(toml);
            assert!(err.contains(needle), "{toml}: {err}");
        }
    }

    #[test]
    fn profiles_map_the_per_texture_quality() {
        use TextureCookQuality::{High, Low};
        assert_eq!(AstcProfile::PerTexture.quality(Low), Low);
        assert_eq!(AstcProfile::PerTexture.quality(High), High);
        assert_eq!(AstcProfile::High.quality(Low), High);
        assert_eq!(AstcProfile::Low.quality(High), Low);
        for p in [AstcProfile::PerTexture, AstcProfile::High, AstcProfile::Low] {
            assert_eq!(AstcProfile::from_label(p.label()), Some(p));
        }
    }

    #[test]
    fn the_scope_restores_the_previous_policy() {
        let etc2 = TexturePolicy {
            android_format: AndroidTextureFormat::Etc2,
            astc_profile: AstcProfile::PerTexture,
        };
        assert_eq!(active_texture_policy(), TexturePolicy::default());
        let inner = with_texture_policy(etc2, || {
            let nested = with_texture_policy(TexturePolicy::default(), active_texture_policy);
            (active_texture_policy(), nested)
        });
        assert_eq!(inner, (etc2, TexturePolicy::default()));
        assert_eq!(active_texture_policy(), TexturePolicy::default());
        let caught = std::panic::catch_unwind(|| with_texture_policy(etc2, || panic!("boom")));
        assert!(caught.is_err());
        assert_eq!(active_texture_policy(), TexturePolicy::default());
    }
}
