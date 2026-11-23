// SPDX-License-Identifier: MPL-2.0
//! Locale start-up order, player-driven switches and write-back (ADR-1618).
//!
//! At start-up the locale comes from the first source that yields a *loaded* locale, in a fixed
//! order: the launch flag, the environment override, the saved settings language, the Steam
//! per-game language, the OS locale, then the project source locale. Each value is matched
//! against the loaded locales with [`match_loaded`] (exact tag, then its parents, ignoring case).
//! [`LocaleStartup`] records the winner and the raw value, and `localization.status` reports it.
//!
//! A player-driven switch is a [`PlayerSetLocale`] message: the UI applies it and answers with a
//! [`LocaleWriteBack`] message, which the settings plugin stores in `GameSettings::language`. An
//! agent-driven switch writes nothing back unless the call asks for it.

use regia_app::prelude::*;
use regia_platform::LocaleHints;

use crate::localization::Localization;

/// The message id every locale defines with its own name in its own language, for language
/// menus (`locale-name = Deutsch`). The validator warns when a locale lacks it.
pub const SELF_NAME_KEY: &str = "locale-name";

/// Where the start-up locale came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocaleSource {
    /// The `--locale <tag>` launch flag.
    Flag,
    /// The `REGIA_LOCALE` environment override.
    Environment,
    /// The saved settings language.
    Settings,
    /// The Steam per-game language.
    Steam,
    /// The OS locale.
    Os,
    /// The project source locale (nothing else matched).
    Project,
}

impl LocaleSource {
    /// Every source in start-up order.
    pub const ORDER: [LocaleSource; 6] = [
        LocaleSource::Flag,
        LocaleSource::Environment,
        LocaleSource::Settings,
        LocaleSource::Steam,
        LocaleSource::Os,
        LocaleSource::Project,
    ];

    /// The stable name agents see: `flag`, `environment`, `settings`, `steam`, `os`, `project`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LocaleSource::Flag => "flag",
            LocaleSource::Environment => "environment",
            LocaleSource::Settings => "settings",
            LocaleSource::Steam => "steam",
            LocaleSource::Os => "os",
            LocaleSource::Project => "project",
        }
    }
}

/// How the start-up locale was chosen (inserted by `load_localization`).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct LocaleStartup {
    /// The winning source.
    pub source: LocaleSource,
    /// The winning source's raw value, as it came in (`ja_JP.UTF-8`, `japanese` is already a tag).
    pub raw: String,
    /// The loaded locale that was selected.
    pub selected: String,
    /// Sources that had a value no loaded locale matched, before the winner: `(source, raw)`.
    pub skipped: Vec<(LocaleSource, String)>,
}

/// A player chose this locale (language menu). The UI selects it and answers with
/// [`LocaleWriteBack`]; an unloaded tag falls back to its nearest loaded parent.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct PlayerSetLocale(pub String);

/// The player's choice to store in the saved settings (`GameSettings::language`).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct LocaleWriteBack {
    /// The tag to store (what the player chose, normalized).
    pub locale: String,
}

/// Cleans a raw locale value into a BCP 47 shaped tag: trims, drops a POSIX encoding or modifier
/// (`ja_JP.UTF-8@euro` → `ja-JP`), turns `_` into `-`. `None` for an empty value and for the
/// POSIX placeholders `C` and `POSIX`.
#[must_use]
pub fn normalize_tag(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let end = trimmed.find(['.', '@']).unwrap_or(trimmed.len());
    let tag = trimmed[..end].replace('_', "-");
    if tag.is_empty() || tag.eq_ignore_ascii_case("c") || tag.eq_ignore_ascii_case("posix") {
        None
    } else {
        Some(tag)
    }
}

/// Script and region tags that name the same written form, tried after the exact tag at each
/// step: `zh-TW` finds a loaded `zh-Hant` and `zh-Hans` finds `zh-CN`.
fn equivalents(tag: &str) -> &'static [&'static str] {
    match tag.to_ascii_lowercase().as_str() {
        "zh-hans" | "zh-hans-cn" => &["zh-CN", "zh-SG"],
        "zh-cn" | "zh-sg" | "zh-hans-sg" => &["zh-Hans"],
        "zh-hant" | "zh-hant-tw" => &["zh-TW", "zh-HK", "zh-MO"],
        "zh-tw" | "zh-hk" | "zh-mo" | "zh-hant-hk" | "zh-hant-mo" => &["zh-Hant"],
        _ => &[],
    }
}

/// The loaded locale that best matches `raw`: the exact tag, then (after each tag) its
/// script/region equivalents, then its parents (`fr-CA-x` → `fr-CA` → `fr`). Case is ignored
/// (`pt-br` finds `pt-BR`). `None` when nothing matches.
#[must_use]
pub fn match_loaded(localization: &Localization, raw: &str) -> Option<String> {
    let mut tag = normalize_tag(raw)?;
    let find = |wanted: &str| {
        localization
            .available
            .iter()
            .find(|loaded| loaded.eq_ignore_ascii_case(wanted))
            .cloned()
    };
    loop {
        if let Some(found) = find(&tag) {
            return Some(found);
        }
        if let Some(found) = equivalents(&tag).iter().find_map(|alias| find(alias)) {
            return Some(found);
        }
        tag = tag.rsplit_once('-')?.0.to_owned();
    }
}

/// Picks the start-up locale from `hints` in the fixed order and selects it in `localization`.
/// Pure over its inputs: tests inject every source through [`LocaleHints::isolated`].
pub fn choose_startup_locale(localization: &mut Localization, hints: &LocaleHints) -> LocaleStartup {
    let candidates = [
        (LocaleSource::Flag, hints.flag_value()),
        (LocaleSource::Environment, hints.env_value()),
        (LocaleSource::Settings, hints.settings.clone()),
        (LocaleSource::Steam, hints.steam.clone()),
        (LocaleSource::Os, hints.os_value()),
    ];
    let mut skipped = Vec::new();
    for (source, raw) in candidates {
        let Some(raw) = raw.map(|r| r.trim().to_owned()).filter(|r| !r.is_empty()) else {
            continue;
        };
        match match_loaded(localization, &raw) {
            Some(matched) => {
                localization.set_locale(&matched);
                return LocaleStartup {
                    source,
                    raw,
                    selected: localization.selected.clone(),
                    skipped,
                };
            }
            None => skipped.push((source, raw)),
        }
    }
    LocaleStartup {
        source: LocaleSource::Project,
        raw: localization.fallback().to_owned(),
        selected: localization.selected.clone(),
        skipped,
    }
}

/// `Update`: applies [`PlayerSetLocale`] messages and answers each with a [`LocaleWriteBack`].
/// The choice is stored even when it equals the current locale (a player who picks the language
/// the OS gave them still replaces "platform default" with an explicit choice). A tag no loaded
/// locale matches is ignored.
pub fn apply_player_locale(
    mut requests: MessageReader<PlayerSetLocale>,
    localization: Option<ResMut<Localization>>,
    mut write_back: MessageWriter<LocaleWriteBack>,
) {
    let Some(mut localization) = localization else {
        requests.clear();
        return;
    };
    for PlayerSetLocale(raw) in requests.read() {
        let Some(matched) = match_loaded(&localization, raw) else {
            tracing::warn!(target: "regia_ui", "player locale {raw:?} matches no loaded locale; ignored");
            continue;
        };
        localization.set_locale(&matched);
        if let Some(locale) = normalize_tag(raw) {
            write_back.write(LocaleWriteBack { locale });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(tags: &[&str]) -> Localization {
        let sources = tags
            .iter()
            .map(|t| ((*t).to_owned(), format!("hello = Hello {t}\n")))
            .collect();
        Localization::new(sources, Some("en".to_owned())).expect("loads")
    }

    fn hints() -> LocaleHints {
        LocaleHints::isolated()
    }

    #[test]
    fn normalize_cleans_posix_forms() {
        assert_eq!(normalize_tag("ja_JP.UTF-8").as_deref(), Some("ja-JP"));
        assert_eq!(normalize_tag(" sr_RS@latin ").as_deref(), Some("sr-RS"));
        assert_eq!(normalize_tag("en-US").as_deref(), Some("en-US"));
        assert_eq!(normalize_tag("C"), None);
        assert_eq!(normalize_tag("POSIX"), None);
        assert_eq!(normalize_tag("  "), None);
    }

    #[test]
    fn match_is_exact_then_parents_ignoring_case() {
        let loc = loaded(&["en", "fr", "pt-BR", "zh-Hant"]);
        assert_eq!(match_loaded(&loc, "fr-CA").as_deref(), Some("fr"));
        assert_eq!(match_loaded(&loc, "pt-br").as_deref(), Some("pt-BR"));
        assert_eq!(match_loaded(&loc, "pt-PT"), None);
        assert_eq!(match_loaded(&loc, "zh-TW").as_deref(), Some("zh-Hant"));
        assert_eq!(match_loaded(&loc, "de"), None);
        let simplified = loaded(&["en", "zh-CN"]);
        assert_eq!(match_loaded(&simplified, "zh-Hans").as_deref(), Some("zh-CN"));
    }

    #[test]
    fn order_is_flag_env_settings_steam_os_project() {
        let tags = ["de", "es", "fr", "it", "ja", "en"];
        let mut full = LocaleHints {
            flag: Some("de".into()),
            env: Some("es".into()),
            settings: Some("fr".into()),
            steam: Some("it".into()),
            os: Some("ja".into()),
            ..hints()
        };
        let expected = [
            (LocaleSource::Flag, "de"),
            (LocaleSource::Environment, "es"),
            (LocaleSource::Settings, "fr"),
            (LocaleSource::Steam, "it"),
            (LocaleSource::Os, "ja"),
        ];
        for (source, tag) in expected {
            let mut loc = loaded(&tags);
            let start = choose_startup_locale(&mut loc, &full);
            assert_eq!((start.source, start.selected.as_str()), (source, tag));
            assert_eq!(start.raw, tag);
            match source {
                LocaleSource::Flag => full.flag = None,
                LocaleSource::Environment => full.env = None,
                LocaleSource::Settings => full.settings = None,
                LocaleSource::Steam => full.steam = None,
                _ => {}
            }
        }
        let mut loc = loaded(&tags);
        full.os = None;
        let start = choose_startup_locale(&mut loc, &full);
        assert_eq!(start.source, LocaleSource::Project);
        assert_eq!(start.selected, "en");
    }

    #[test]
    fn an_unloaded_value_is_skipped_and_recorded() {
        let mut loc = loaded(&["en", "fr"]);
        let start = choose_startup_locale(
            &mut loc,
            &LocaleHints {
                flag: Some("xx".into()),
                settings: Some("fr-CA".into()),
                os: Some("en".into()),
                ..hints()
            },
        );
        assert_eq!(start.source, LocaleSource::Settings);
        assert_eq!(start.raw, "fr-CA");
        assert_eq!(start.selected, "fr");
        assert_eq!(start.skipped, vec![(LocaleSource::Flag, "xx".to_owned())]);
    }

    #[test]
    fn empty_and_blank_sources_count_as_absent() {
        let mut loc = loaded(&["en", "ja"]);
        let start = choose_startup_locale(
            &mut loc,
            &LocaleHints {
                settings: Some(String::new()),
                steam: Some("  ".into()),
                os: Some("ja_JP.UTF-8".into()),
                ..hints()
            },
        );
        assert_eq!((start.source, start.selected.as_str()), (LocaleSource::Os, "ja"));
        assert!(start.skipped.is_empty());
    }

    #[test]
    fn nothing_matching_selects_the_project_source() {
        let mut loc = loaded(&["en", "ja"]);
        let start = choose_startup_locale(
            &mut loc,
            &LocaleHints {
                settings: Some("ko".into()),
                os: Some("sv-SE".into()),
                ..hints()
            },
        );
        assert_eq!(start.source, LocaleSource::Project);
        assert_eq!(start.raw, "en");
        assert_eq!(start.skipped.len(), 2);
    }

    #[test]
    fn player_choice_applies_and_writes_back() {
        let mut app = App::new();
        app.add_message::<PlayerSetLocale>()
            .add_message::<LocaleWriteBack>()
            .insert_resource(loaded(&["en", "fr", "ja"]))
            .add_systems(Update, apply_player_locale);
        app.world_mut().write_message(PlayerSetLocale("fr-CA".into()));
        app.world_mut().write_message(PlayerSetLocale("xx".into()));
        app.update();
        assert_eq!(app.world().resource::<Localization>().locale(), "fr");
        let written: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<LocaleWriteBack>>()
            .drain()
            .collect();
        assert_eq!(
            written,
            vec![LocaleWriteBack {
                locale: "fr-CA".into()
            }]
        );
    }
}
