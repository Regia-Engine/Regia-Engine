// SPDX-License-Identifier: MPL-2.0
//! Steam language names → BCP 47 tags (ADR-1618).
//!
//! Steam reports the language a player picked for a game as one of its API language names
//! (`english`, `schinese`, `koreana`, ...). The table is the list of supported languages in the
//! public Steamworks documentation ("Localization and Languages"); each name maps to the tag a
//! Regia project names its locale directory with.

/// `(Steam API language name, BCP 47 tag)`, in the order of the public Steamworks list.
pub const STEAM_LANGUAGES: &[(&str, &str)] = &[
    ("arabic", "ar"),
    ("bulgarian", "bg"),
    ("schinese", "zh-Hans"),
    ("tchinese", "zh-Hant"),
    ("czech", "cs"),
    ("danish", "da"),
    ("dutch", "nl"),
    ("english", "en"),
    ("finnish", "fi"),
    ("french", "fr"),
    ("german", "de"),
    ("greek", "el"),
    ("hungarian", "hu"),
    ("indonesian", "id"),
    ("italian", "it"),
    ("japanese", "ja"),
    ("koreana", "ko"),
    ("norwegian", "nb"),
    ("polish", "pl"),
    ("portuguese", "pt"),
    ("brazilian", "pt-BR"),
    ("romanian", "ro"),
    ("russian", "ru"),
    ("spanish", "es"),
    ("latam", "es-419"),
    ("swedish", "sv"),
    ("thai", "th"),
    ("turkish", "tr"),
    ("ukrainian", "uk"),
    ("vietnamese", "vi"),
];

/// The BCP 47 tag for a Steam API language name (ASCII case ignored); `None` for an empty or
/// unknown name.
#[must_use]
pub fn steam_language_tag(name: &str) -> Option<&'static str> {
    let name = name.trim();
    STEAM_LANGUAGES
        .iter()
        .find(|(steam, _)| steam.eq_ignore_ascii_case(name))
        .map(|(_, tag)| *tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_tags() {
        assert_eq!(steam_language_tag("english"), Some("en"));
        assert_eq!(steam_language_tag("schinese"), Some("zh-Hans"));
        assert_eq!(steam_language_tag("tchinese"), Some("zh-Hant"));
        assert_eq!(steam_language_tag("koreana"), Some("ko"));
        assert_eq!(steam_language_tag("brazilian"), Some("pt-BR"));
        assert_eq!(steam_language_tag("latam"), Some("es-419"));
        assert_eq!(steam_language_tag(" Japanese "), Some("ja"));
        assert_eq!(steam_language_tag(""), None);
        assert_eq!(steam_language_tag("klingon"), None);
    }

    #[test]
    fn the_table_has_unique_names_and_tags() {
        for (i, (name, tag)) in STEAM_LANGUAGES.iter().enumerate() {
            for (other, other_tag) in &STEAM_LANGUAGES[i + 1..] {
                assert_ne!(name, other);
                assert_ne!(tag, other_tag);
            }
            assert_eq!(*name, name.to_ascii_lowercase());
        }
    }
}
