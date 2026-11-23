// SPDX-License-Identifier: MPL-2.0
//! Localized asset variants (spec 36 § 6, ADR-1882): non-text content (a texture with baked words, a
//! voice recording) is localized by path. For an asset at `<rel>` under `assets/`, a locale `L` may
//! provide `localized/L/<rel>`. [`localized_candidates`] lists the paths to try for a fallback chain;
//! [`resolve_localized_with`] picks the first one that exists with the base's cooked type.
//!
//! This crate does not depend on `regia_ui`: the text and audio fallback chains are plain tag lists
//! ([`LocaleChains`]) the UI and audio plugins set.

use regia_app::prelude::*;

/// The folder (under `assets/`) that holds the per-locale variants.
pub const LOCALIZED_DIR: &str = "localized";

/// Which fallback chain a localized asset follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocaleGroup {
    /// The text locale's chain: textures, UI art, cue text, everything that is not spoken audio.
    Text,
    /// The audio locale's chain: sounds, voices and dialogue.
    Audio,
}

impl LocaleGroup {
    /// The group a path belongs to by its kind (spec 36 § 6): sounds, sound cues, voices,
    /// dialogues and conversations are [`LocaleGroup::Audio`]; everything else is
    /// [`LocaleGroup::Text`].
    #[must_use]
    pub fn for_path(rel: &str) -> Self {
        let lower = rel.to_ascii_lowercase();
        const AUDIO: [&str; 9] = [
            ".ogg",
            ".wav",
            ".flac",
            ".mp3",
            ".opus",
            ".cue.ron",
            ".voice.ron",
            ".dialogue.ron",
            ".conv.ron",
        ];
        if AUDIO.iter().any(|ext| lower.ends_with(ext)) {
            Self::Audio
        } else {
            Self::Text
        }
    }

    /// `text` or `audio`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Audio => "audio",
        }
    }
}

/// The text and audio fallback chains localized assets resolve through (most specific first, the
/// source locale last). The UI plugin keeps them in step with its `Localization`.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct LocaleChains {
    /// The text locale's chain (`fr-CA`, `fr`, `en`).
    pub text: Vec<String>,
    /// The audio locale's chain.
    pub audio: Vec<String>,
}

impl LocaleChains {
    /// The chain of `group`.
    #[must_use]
    pub fn chain(&self, group: LocaleGroup) -> &[String] {
        match group {
            LocaleGroup::Text => &self.text,
            LocaleGroup::Audio => &self.audio,
        }
    }
}

/// The paths to try for `rel` (an `assets/`-relative path, `/` separators): `localized/<L>/<rel>` for
/// each locale of `chain` in order, then `rel` itself last. Empty or repeated tags are skipped; a
/// `rel` that already names a variant is returned alone.
#[must_use]
pub fn localized_candidates(rel: &str, chain: &[String]) -> Vec<String> {
    let rel = rel.trim_start_matches('/');
    if localized_of(rel).is_some() {
        return vec![rel.to_owned()];
    }
    let mut out: Vec<String> = Vec::with_capacity(chain.len() + 1);
    for locale in chain.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        let candidate = format!("{LOCALIZED_DIR}/{locale}/{rel}");
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out.push(rel.to_owned());
    out
}

/// `Some((locale, base))` when `path` is a localized variant (`localized/fr/ui/sign.png` gives
/// `("fr", "ui/sign.png")`). The registry tags variants with `localized=<locale>` and
/// `localized_of=<base>`.
#[must_use]
pub fn localized_of(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(LOCALIZED_DIR)?.strip_prefix('/')?;
    let (locale, base) = rest.split_once('/')?;
    (!locale.is_empty() && !base.is_empty()).then_some((locale, base))
}

/// The resolved path of `rel` for `chain`: the first candidate ([`localized_candidates`]) that
/// `type_of` knows (it returns the cooked type path of an existing asset) with the same cooked type
/// as the base. A variant with another type is skipped (the validator reports it; the loader never
/// loads it). When the base is unknown, the first existing variant wins; with nothing found, `rel`.
pub fn resolve_localized_with<'a>(
    rel: &str,
    chain: &[String],
    type_of: impl Fn(&str) -> Option<&'a str>,
) -> String {
    let base_type = type_of(rel.trim_start_matches('/'));
    for candidate in localized_candidates(rel, chain) {
        match (type_of(&candidate), base_type) {
            (Some(found), Some(base)) if found == base => return candidate,
            (Some(_), None) => return candidate,
            _ => {}
        }
    }
    rel.trim_start_matches('/').to_owned()
}

/// One localized asset whose resolved path moved after a locale switch
/// (`AssetServer::reload_localized`). A consumer reloads `rel` through its new path.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct LocalizedAssetChanged {
    /// The base path the game asked for.
    pub rel: String,
    /// The group (chain) it follows.
    pub group: LocaleGroup,
    /// The path it resolved to before.
    pub from: String,
    /// The path it resolves to now.
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(tags: &[&str]) -> Vec<String> {
        tags.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn candidates_follow_the_chain_then_the_base() {
        assert_eq!(
            localized_candidates("ui/sign.png", &chain(&["fr-CA", "fr", "en"])),
            [
                "localized/fr-CA/ui/sign.png",
                "localized/fr/ui/sign.png",
                "localized/en/ui/sign.png",
                "ui/sign.png"
            ]
        );
        assert_eq!(
            localized_candidates("/a.png", &chain(&["", "de", "de"])),
            ["localized/de/a.png", "a.png"]
        );
        assert_eq!(localized_candidates("a.png", &[]), ["a.png"]);
        assert_eq!(
            localized_candidates("localized/fr/a.png", &chain(&["de"])),
            ["localized/fr/a.png"],
            "a variant path is not localized again"
        );
    }

    #[test]
    fn variants_parse_back_to_locale_and_base() {
        assert_eq!(
            localized_of("localized/fr/ui/sign.png"),
            Some(("fr", "ui/sign.png"))
        );
        assert_eq!(localized_of("localized/fr"), None);
        assert_eq!(localized_of("localized//a.png"), None);
        assert_eq!(localized_of("ui/localized/fr/a.png"), None);
    }

    #[test]
    fn resolution_falls_back_to_the_base_and_ignores_type_mismatches() {
        let assets = [
            ("ui/sign.png", "Texture"),
            ("localized/fr/ui/sign.png", "Texture"),
            ("localized/de/ui/sign.png", "Mesh"),
            ("localized/ja/only.png", "Texture"),
        ];
        let type_of = |p: &str| assets.iter().find(|(path, _)| *path == p).map(|(_, t)| *t);
        assert_eq!(
            resolve_localized_with("ui/sign.png", &chain(&["fr-CA", "fr", "en"]), type_of),
            "localized/fr/ui/sign.png"
        );
        assert_eq!(
            resolve_localized_with("ui/sign.png", &chain(&["it", "en"]), type_of),
            "ui/sign.png",
            "no variant: the base"
        );
        assert_eq!(
            resolve_localized_with("ui/sign.png", &chain(&["de", "en"]), type_of),
            "ui/sign.png",
            "a type-mismatched variant is ignored"
        );
        assert_eq!(
            resolve_localized_with("only.png", &chain(&["ja"]), type_of),
            "localized/ja/only.png",
            "a variant without a base still resolves"
        );
        assert_eq!(
            resolve_localized_with("none.png", &chain(&["ja"]), type_of),
            "none.png"
        );
    }

    #[test]
    fn groups_by_kind() {
        assert_eq!(
            LocaleGroup::for_path("audio/dialogue/hello.OGG"),
            LocaleGroup::Audio
        );
        assert_eq!(LocaleGroup::for_path("cues/door.cue.ron"), LocaleGroup::Audio);
        assert_eq!(
            LocaleGroup::for_path("dialogue/intro.dialogue.ron"),
            LocaleGroup::Audio
        );
        assert_eq!(LocaleGroup::for_path("ui/sign.png"), LocaleGroup::Text);
        assert_eq!(LocaleGroup::for_path("subs/hello.subs.ron"), LocaleGroup::Text);
        let chains = LocaleChains {
            text: chain(&["fr", "en"]),
            audio: chain(&["ja", "en"]),
        };
        assert_eq!(chains.chain(LocaleGroup::Audio), chain(&["ja", "en"]).as_slice());
        assert_eq!(LocaleGroup::Text.as_str(), "text");
    }
}
