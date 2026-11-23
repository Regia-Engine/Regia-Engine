// SPDX-License-Identifier: MPL-2.0
//! `.video.ron` and `.videolist.ron` (specs/50 § 4). `format_version` starts at 1; loaders refuse
//! future versions with an error naming the version (a v2 ships a migrator, `specs/12` § 4a).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::player::LoopMode;

/// Current `.video.ron` version.
pub const VIDEO_SOURCE_VERSION: u32 = 1;
/// Current `.videolist.ron` version.
pub const VIDEO_LIST_VERSION: u32 = 1;

/// How much of the file is read ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Preload {
    /// Packets as needed.
    #[default]
    Stream,
    /// The streaming reader, the same as [`Preload::Stream`]. There is no separate one-second buffer.
    FirstSecond,
    /// The whole file into memory, refused above 64 MiB (32 MiB on Android) before the buffer is
    /// allocated.
    WholeFile,
}

fn one() -> f32 {
    1.0
}

fn sfx() -> String {
    "sfx".to_string()
}

fn black() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

/// One clip's locator and playback defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoSource {
    /// Format version (1).
    pub format_version: u32,
    /// The media file (`.webm` or `.mkv`), project-relative.
    pub file: String,
    /// Per-platform replacements keyed `linux`, `windows`, `android`, `macos` (any case).
    #[serde(default)]
    pub variants: BTreeMap<String, String>,
    /// Audio bus name.
    #[serde(default = "sfx")]
    pub audio_bus: String,
    /// Volume.
    #[serde(default = "one")]
    pub volume: f32,
    /// Loop policy.
    #[serde(default)]
    pub loop_mode: LoopMode,
    /// Read-ahead policy.
    #[serde(default)]
    pub preload: Preload,
    /// Texture color while closed.
    #[serde(default = "black")]
    pub clear_color: [f32; 4],
}

impl VideoSource {
    /// A source for `file` with defaults.
    pub fn new(file: impl Into<String>) -> Self {
        Self {
            format_version: VIDEO_SOURCE_VERSION,
            file: file.into(),
            variants: BTreeMap::new(),
            audio_bus: sfx(),
            volume: 1.0,
            loop_mode: LoopMode::Off,
            preload: Preload::Stream,
            clear_color: black(),
        }
    }

    /// Parses `.video.ron` text.
    pub fn from_ron(text: &str) -> Result<Self, MediaError> {
        check_version(text, VIDEO_SOURCE_VERSION, ".video.ron")?;
        ron::from_str(text).map_err(|e| MediaError::invalid(format!(".video.ron: {e}")))
    }

    /// Writes `.video.ron` text.
    pub fn to_ron(&self) -> Result<String, MediaError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| MediaError::invalid(format!(".video.ron: {e}")))
    }

    /// The file for a platform (`linux`, `windows`, `android`, `macos`): the variant if any.
    pub fn file_for(&self, platform: &str) -> &str {
        self.variants
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(platform))
            .map_or(self.file.as_str(), |(_, v)| v.as_str())
    }
}

/// The platform key for this build.
pub fn platform_key() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// An ordered list of sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoList {
    /// Format version (1).
    pub format_version: u32,
    /// `.video.ron` paths.
    pub entries: Vec<String>,
    /// Return to entry 0 after the last.
    #[serde(default)]
    pub loop_list: bool,
    /// Seeded shuffle.
    #[serde(default)]
    pub shuffle: Option<u64>,
}

impl VideoList {
    /// Parses `.videolist.ron` text.
    pub fn from_ron(text: &str) -> Result<Self, MediaError> {
        check_version(text, VIDEO_LIST_VERSION, ".videolist.ron")?;
        let list: Self =
            ron::from_str(text).map_err(|e| MediaError::invalid(format!(".videolist.ron: {e}")))?;
        if list.entries.len() > crate::list::MAX_ENTRIES {
            return Err(MediaError::LimitExceeded(format!(
                "{} playlist entries (max {})",
                list.entries.len(),
                crate::list::MAX_ENTRIES
            )));
        }
        Ok(list)
    }
}

/// The serde shape of `.video.ron` for `cargo xtask check-formats` (specs/12 § 4a).
pub fn video_source_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<VideoSource>(),
        serde_variant_names::<LoopMode>(),
        serde_variant_names::<Preload>(),
    ])
}

/// The serde shape of `.videolist.ron` for `cargo xtask check-formats`.
pub fn video_list_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[serde_field_names::<VideoList>()])
}

/// The versions a built-in migration step leaves (`from` side): none, v1 is the first version of
/// both formats.
pub fn video_migration_sources() -> Vec<u32> {
    Vec::new()
}

fn check_version(text: &str, current: u32, what: &str) -> Result<(), MediaError> {
    #[derive(Deserialize)]
    struct Peek {
        format_version: u32,
    }
    let peek: Peek = ron::from_str::<Peek>(text)
        .map_err(|e| MediaError::invalid(format!("{what}: no readable format_version ({e})")))?;
    if peek.format_version > current {
        return Err(MediaError::invalid(format!(
            "{what} format_version {} is newer than this engine ({current}); upgrade the engine",
            peek.format_version
        )));
    }
    if peek.format_version == 0 {
        return Err(MediaError::invalid(format!(
            "{what}: format_version 0 is not valid"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"(format_version: 1,
 file: "movies/intro.webm",
 variants: { "Android": "movies/intro_720.webm" },
 audio_bus: "sfx", volume: 1.0, loop_mode: Off, preload: Stream,
 clear_color: (0.0, 0.0, 0.0, 1.0))"#;

    #[test]
    fn spec_example_parses_and_picks_variant() {
        let s = VideoSource::from_ron(EXAMPLE).expect("parses");
        assert_eq!(s.file_for("android"), "movies/intro_720.webm");
        assert_eq!(s.file_for("linux"), "movies/intro.webm");
        let again = VideoSource::from_ron(&s.to_ron().expect("writes")).expect("round trip");
        assert_eq!(again, s);
    }

    #[test]
    fn future_version_is_refused_by_name() {
        let text = EXAMPLE.replace("format_version: 1", "format_version: 2");
        let err = VideoSource::from_ron(&text).expect_err("refused");
        assert!(err.to_string().contains("format_version 2"), "{err}");
    }

    #[test]
    fn fingerprints_name_every_field() {
        let source = video_source_schema_fingerprint();
        for field in [
            "format_version",
            "file",
            "variants",
            "audio_bus",
            "clear_color",
            "Count",
            "WholeFile",
        ] {
            assert!(source.contains(field), "{field} missing from {source}");
        }
        assert!(video_list_schema_fingerprint().contains("shuffle"));
        assert!(video_migration_sources().is_empty());
    }

    #[test]
    fn list_parses() {
        let l = VideoList::from_ron(r#"(format_version: 1, entries: ["a.video.ron", "b.video.ron"], loop_list: false, shuffle: None)"#)
            .expect("parses");
        assert_eq!(l.entries.len(), 2);
    }

    /// Spec 50 § 4 and § 2.4: a newer `.videolist.ron` is refused by name (`container_invalid`), and a
    /// list over 4,096 entries is `limit_exceeded`.
    #[test]
    fn list_refuses_a_newer_version_and_too_many_entries() {
        let err = VideoList::from_ron(r#"(format_version: 2, entries: [])"#).expect_err("v2");
        assert_eq!(err.code(), "container_invalid");
        assert!(
            err.to_string().contains(".videolist.ron format_version 2"),
            "{err}"
        );
        let zero = VideoList::from_ron(r#"(format_version: 0, entries: [])"#).expect_err("v0");
        assert_eq!(zero.code(), "container_invalid");
        let entries = |n: usize| vec!["\"a.video.ron\""; n].join(",");
        let full = format!(
            "(format_version: 1, entries: [{}])",
            entries(crate::list::MAX_ENTRIES)
        );
        assert_eq!(
            VideoList::from_ron(&full).expect("4,096 entries").entries.len(),
            4096
        );
        let over = format!(
            "(format_version: 1, entries: [{}])",
            entries(crate::list::MAX_ENTRIES + 1)
        );
        assert_eq!(
            VideoList::from_ron(&over).expect_err("4,097").code(),
            "limit_exceeded"
        );
    }
}
