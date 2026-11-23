// SPDX-License-Identifier: MPL-2.0
//! The cooked conversation `<id>.convc` (spec 56 § 4.3, ADR-1882): postcard, `format_version` first.
//! It carries the conversation (lines, steps, text keys) and, per staged locale, each line's resolved
//! voice clip with its length and whether it is a fallback, built with the same resolution as § 2.2.
//! Text is not cooked in: it stays in the locale bundles, so a string fix never re-cooks an audio table.
//! `regia package` writes one per conversation; a clip missing in a staged locale with `clip_policy:
//! Require` is the cook error `conv_missing_clip` (a warning otherwise).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::asset::{ClipPolicy, ConversationAsset, VoiceSpec};
use super::voice::{VoiceContext, resolve_voice};
use super::{ConversationError, ConversationLibrary};

/// The `format_version` of `.convc`.
pub const CONVC_VERSION: u32 = 1;

/// The file extension of a cooked conversation.
pub const CONVC_EXTENSION: &str = "convc";

/// One line's clip in one locale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedClip {
    /// The clip path (relative to `assets/`), `None` when the line has none in this locale.
    pub clip: Option<String>,
    /// The clip length in microseconds, when known.
    pub duration_us: Option<u64>,
    /// The clip belongs to another locale of the chain.
    pub fallback: bool,
    /// The locale the clip belongs to.
    pub clip_locale: Option<String>,
}

/// `<id>.convc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedConversation {
    /// Must be [`CONVC_VERSION`]; first, so a reader checks it before the rest.
    pub format_version: u32,
    /// The conversation as authored (lines, steps, text keys).
    pub asset: ConversationAsset,
    /// Staged locale to line id to clip.
    pub clips: BTreeMap<String, BTreeMap<String, CookedClip>>,
}

/// The serde shape of `.convc` for `cargo xtask check-formats` (`docs/formats/convc.md`, ADR-2316):
/// the cooked wrapper and clip records in postcard order, then the embedded conversation's own
/// fingerprint (a change to `.conv.ron`'s shape changes the cooked shape too).
#[must_use]
pub fn convc_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    let mut out = join_field_lists(&[
        serde_field_names::<CookedConversation>(),
        serde_field_names::<CookedClip>(),
    ]);
    out.push(';');
    out.push_str(&super::asset::conversation_schema_fingerprint());
    out
}

/// `.convc` is at its first version: no migration steps (a cook output: an older file is cooked
/// again).
#[must_use]
pub fn convc_migration_sources() -> Vec<u32> {
    Vec::new()
}

impl CookedConversation {
    /// The postcard bytes.
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ConversationError> {
        postcard::to_allocvec(self).map_err(|e| ConversationError::Asset(format!("convc: {e}")))
    }

    /// Reads `.convc` bytes; a version this build does not read is an error (re-cook).
    ///
    /// # Errors
    ///
    /// Bad bytes or another `format_version`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ConversationError> {
        let (version, _) = postcard::take_from_bytes::<u32>(bytes)
            .map_err(|e| ConversationError::Asset(format!("convc: {e}")))?;
        if version != CONVC_VERSION {
            return Err(ConversationError::Asset(format!(
                "convc format_version {version} is not {CONVC_VERSION}; cook again"
            )));
        }
        postcard::from_bytes(bytes).map_err(|e| ConversationError::Asset(format!("convc: {e}")))
    }

    /// The clip of `line` in `locale`, if cooked.
    #[must_use]
    pub fn clip(&self, locale: &str, line: &str) -> Option<&CookedClip> {
        self.clips.get(locale)?.get(line)
    }
}

/// A cook finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookIssue {
    /// `conv_missing_clip`.
    pub code: &'static str,
    /// An error fails the cook.
    pub error: bool,
    /// `<conversation>#<line>`.
    pub line: String,
    /// The staged locale.
    pub locale: String,
    /// What is wrong.
    pub message: String,
}

/// Cooks `asset` for the staged `locales`. `chain(locale)` gives each locale's fallback chain (starting with
/// itself); clips are the library's known clips (with their lengths).
#[must_use]
pub fn cook_conversation(
    asset: &ConversationAsset,
    library: &ConversationLibrary,
    locales: &[String],
    chain: &dyn Fn(&str) -> Vec<String>,
) -> (CookedConversation, Vec<CookIssue>) {
    let exists = |p: &str| library.clips.contains_key(p);
    let mut issues = Vec::new();
    let mut clips = BTreeMap::new();
    for locale in locales {
        let chain = chain(locale);
        let mut table = BTreeMap::new();
        for (index, line) in asset.lines.iter().enumerate() {
            // As the validator: an unknown speaker takes the default speaker's `voiced`.
            let voiced = library
                .speakers
                .speakers
                .get(&line.speaker)
                .cloned()
                .unwrap_or_default()
                .voiced;
            let ctx = VoiceContext {
                conversation: &asset.id,
                index,
                speaker_voiced: voiced,
                template: Some(library.settings.voice_path.as_str()),
                fallback_policy: library.settings.voice_fallback,
                chain: &chain,
                exists: &exists,
            };
            let resolution = resolve_voice(line, &ctx, locale);
            if voiced && line.voice != VoiceSpec::None && resolution.clip.is_none() {
                issues.push(CookIssue {
                    code: "conv_missing_clip",
                    error: library.settings.clip_policy == ClipPolicy::Require,
                    line: format!("{}#{}", asset.id, line.id),
                    locale: locale.clone(),
                    message: format!(
                        "no voice clip for `{}#{}` in `{locale}` or its fallbacks",
                        asset.id, line.id
                    ),
                });
            }
            let duration_us = resolution
                .clip
                .as_ref()
                .and_then(|c| library.clips.get(c).copied().flatten());
            table.insert(
                line.id.clone(),
                CookedClip {
                    clip: resolution.clip,
                    duration_us,
                    fallback: resolution.fallback,
                    clip_locale: resolution.locale,
                },
            );
        }
        clips.insert(locale.clone(), table);
    }
    (
        CookedConversation {
            format_version: CONVC_VERSION,
            asset: asset.clone(),
            clips,
        },
        issues,
    )
}

impl ConversationLibrary {
    /// Adds the conversations of `<dir>/*.convc` that the library does not have from source (a packaged
    /// build ships the cooked files). Returns how many were added.
    ///
    /// # Errors
    ///
    /// A cooked file cannot be read or decoded.
    pub fn load_cooked(&mut self, dir: &std::path::Path) -> Result<usize, ConversationError> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(0);
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == CONVC_EXTENSION))
            .collect();
        paths.sort();
        let mut added = 0;
        for path in paths {
            let bytes = std::fs::read(&path)
                .map_err(|e| ConversationError::Asset(format!("{}: {e}", path.display())))?;
            added += usize::from(self.add_cooked(&bytes)?);
        }
        Ok(added)
    }

    /// Adds one cooked conversation unless the library has it from source; whether it was added.
    pub(crate) fn add_cooked(&mut self, bytes: &[u8]) -> Result<bool, ConversationError> {
        let cooked = CookedConversation::from_bytes(bytes)?;
        if self.conversations.contains_key(&cooked.asset.id) {
            return Ok(false);
        }
        for table in cooked.clips.values() {
            for clip in table.values() {
                if let Some(path) = &clip.clip {
                    self.clips.entry(path.clone()).or_insert(clip.duration_us);
                }
            }
        }
        self.conversations.insert(cooked.asset.id.clone(), cooked.asset);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::asset::parse_conversation;

    const CONV: &str = r#"(
        format_version: 1,
        id: "intro",
        lines: [
            (id: "hello", speaker: "guard", text: "conv-intro-hello"),
            (id: "quiet", speaker: "guard", text: "conv-intro-quiet", voice: None),
        ],
        steps: [Line("hello"), Line("quiet"), End],
    )"#;

    fn library() -> ConversationLibrary {
        let mut lib = ConversationLibrary::default();
        let conv = parse_conversation("intro.conv.ron", CONV).expect("parses");
        lib.conversations.insert(conv.id.clone(), conv);
        lib.settings.voice_path = "vo/{locale}/{conversation}/{line}.ogg".into();
        lib.clips.insert("vo/en/intro/hello.ogg".into(), Some(1_500_000));
        lib
    }

    fn chain(locale: &str) -> Vec<String> {
        let mut out = vec![locale.to_owned()];
        if locale != "en" {
            out.push("en".to_owned());
        }
        out
    }

    #[test]
    fn cook_resolves_clips_per_locale_with_fallback_and_round_trips() {
        let lib = library();
        let asset = &lib.conversations["intro"];
        let (cooked, issues) = cook_conversation(asset, &lib, &["en".into(), "fr".into()], &chain);
        assert!(issues.is_empty(), "{issues:?}");
        let en = cooked.clip("en", "hello").expect("en");
        assert_eq!(en.clip.as_deref(), Some("vo/en/intro/hello.ogg"));
        assert_eq!(en.duration_us, Some(1_500_000));
        assert!(!en.fallback);
        let fr = cooked.clip("fr", "hello").expect("fr");
        assert!(fr.fallback, "fr borrows the en clip");
        assert_eq!(fr.clip_locale.as_deref(), Some("en"));
        assert_eq!(cooked.clip("fr", "quiet").and_then(|c| c.clip.clone()), None);
        let bytes = cooked.to_bytes().expect("bytes");
        assert_eq!(CookedConversation::from_bytes(&bytes).expect("reads"), cooked);
        // Deterministic.
        assert_eq!(
            cook_conversation(asset, &lib, &["en".into(), "fr".into()], &chain)
                .0
                .to_bytes()
                .expect("again"),
            bytes
        );
        let mut newer = bytes.clone();
        newer[0] = 9;
        assert!(CookedConversation::from_bytes(&newer).is_err());
    }

    #[test]
    fn a_missing_clip_is_an_error_only_with_require() {
        let mut lib = library();
        lib.clips.clear();
        let asset = lib.conversations["intro"].clone();
        let (_, issues) = cook_conversation(&asset, &lib, &["en".into()], &chain);
        assert_eq!(issues.len(), 1, "the unvoiced line reports nothing: {issues:?}");
        assert_eq!(issues[0].code, "conv_missing_clip");
        assert!(!issues[0].error, "warn by default");
        lib.settings.clip_policy = ClipPolicy::Require;
        let (_, issues) = cook_conversation(&asset, &lib, &["en".into()], &chain);
        assert!(issues[0].error);
    }

    /// Like the Android APK mount: a flat listing, no host path.
    struct FlatMount(std::collections::BTreeMap<String, Vec<u8>>);

    impl regia_platform::VfsMount for FlatMount {
        fn read(&self, relative: &str) -> std::io::Result<Vec<u8>> {
            self.0
                .get(relative)
                .cloned()
                .ok_or_else(|| std::io::ErrorKind::NotFound.into())
        }
        fn open(&self, relative: &str) -> std::io::Result<Box<dyn regia_platform::VfsReader>> {
            Ok(Box::new(std::io::Cursor::new(self.read(relative)?)))
        }
        fn exists(&self, relative: &str) -> bool {
            self.0.contains_key(relative)
        }
        fn list_dir(&self, relative: &str) -> std::io::Result<Vec<regia_platform::VfsDirEntry>> {
            Ok(self
                .0
                .keys()
                .filter_map(|k| k.strip_prefix(relative)?.strip_prefix('/'))
                .map(|name| regia_platform::VfsDirEntry {
                    name: name.to_owned(),
                    is_dir: false,
                })
                .collect())
        }
        fn describe(&self) -> String {
            "flat test mount".to_owned()
        }
    }

    #[test]
    fn a_package_without_a_host_directory_loads_through_the_vfs() {
        let lib = library();
        let (cooked, _) = cook_conversation(&lib.conversations["intro"], &lib, &["en".to_owned()], &chain);
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            "conversations/intro.convc".to_owned(),
            cooked.to_bytes().expect("bytes"),
        );
        files.insert(
            "conversations/speakers.ron".to_owned(),
            br#"(format_version: 1, speakers: { "guard": (voiced: true) })"#.to_vec(),
        );
        let mut vfs = regia_platform::Vfs::new();
        vfs.mount(regia_platform::MountPoint::Assets, FlatMount(files));
        let loaded = ConversationLibrary::load_vfs(&vfs).expect("loads");
        assert!(loaded.conversations.contains_key("intro"));
        assert!(loaded.speakers.speakers.contains_key("guard"));
        assert!(!loaded.clips.is_empty(), "the cooked table names the clips");
        assert!(
            ConversationLibrary::load_vfs(&regia_platform::Vfs::new())
                .expect("defaults")
                .conversations
                .is_empty()
        );
    }

    #[test]
    fn a_packaged_library_loads_the_cooked_files() {
        let lib = library();
        let dir = regia_test_utils::unique_temp_dir("convc-load");
        let (cooked, _) = cook_conversation(&lib.conversations["intro"], &lib, &["en".into()], &chain);
        std::fs::write(dir.join("intro.convc"), cooked.to_bytes().expect("bytes")).expect("write");
        let mut packaged = ConversationLibrary::default();
        assert_eq!(packaged.load_cooked(&dir).expect("load"), 1);
        assert!(packaged.conversations.contains_key("intro"));
        assert_eq!(
            packaged.clips.get("vo/en/intro/hello.ogg"),
            Some(&Some(1_500_000))
        );
        assert_eq!(
            packaged.load_cooked(&dir).expect("again"),
            0,
            "source wins, no duplicate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
