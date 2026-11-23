// SPDX-License-Identifier: MPL-2.0
//! `localization.*`: the runtime side of the localization workflow (book `agents/localization.md`,
//! `docs/specs/content-data.md` § 1, ADR-1466): the selected locale and its fallback chain, a
//! locale switch, resolving one key, pseudo mode, the validator over the loaded locales, and the
//! CLDR plural table.

use regia_ecs::prelude::*;
use regia_framework::settings::GameSettings;
use regia_ui::locale_startup::match_loaded;
use regia_ui::text::TextSystem;
use regia_ui::{LocaleStartup, Localization, plural};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn not_loaded() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "loc_not_loaded",
        "this app has no localization loaded",
        "add .ftl files under assets/localization/<locale>/ and the UI plugin",
    )
}

fn localization(world: &World) -> Result<&Localization, RpcError> {
    world
        .get_resource::<Localization>()
        .filter(|l| !l.available.is_empty())
        .ok_or_else(not_loaded)
}

fn string_param<'a>(params: &'a Value, key: &str, code: &str, hint: &str) -> Result<&'a str, RpcError> {
    crate::params::opt_str(params, key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(code, format!("`{key}` must be a non-empty string"), hint))
}

fn status_json(world: &World, l: &Localization) -> Value {
    let names: serde_json::Map<String, Value> = l
        .available
        .iter()
        .map(|locale| (locale.clone(), json!(l.self_name(locale))))
        .collect();
    let startup = world.get_resource::<LocaleStartup>().map(|start| {
        json!({
            "source": start.source.name(),
            "raw": start.raw,
            "selected": start.selected,
            "skipped": start.skipped.iter().map(|(source, raw)| json!({
                "source": source.name(), "raw": raw,
            })).collect::<Vec<_>>(),
        })
    });
    json!({
        "selected": l.locale(),
        "source": l.fallback(),
        "available": l.available,
        "chain": l.chain(l.locale()),
        "fallbacks": l.fallbacks(),
        "pseudo": l.pseudo(),
        "key_display": l.key_display(),
        "origin": l.origin().as_str(),
        "audio": {
            "locale": l.audio_locale(),
            "override": l.audio_override(),
            "chain": l.audio_chain(),
            "generation": l.audio_generation(),
        },
        "generation": l.generation.0,
        "startup": startup,
        "names": names,
        "font_locale": world.get_resource::<TextSystem>().map(|fonts| fonts.locale().to_owned()),
    })
}

/// `localization.status {}`.
pub(crate) fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(status_json(world, localization(world)?))
}

/// `localization.set_locale { locale, persist? }`: uses the startup locale matching rules.
/// An agent-driven switch is not saved unless `persist` is true, which stores the resolved loaded
/// tag in `GameSettings::language`.
pub(crate) fn set_locale(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    localization(world)?;
    let locale = string_param(
        params,
        "locale",
        "bad_locale",
        "pass a locale tag such as \"fr-CA\"",
    )?;
    let persist = match params.get("persist").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| {
            invalid(
                "bad_flag",
                "`persist` must be true or false",
                "pass a JSON boolean",
            )
        })?,
    };
    let matched = match_loaded(world.resource::<Localization>(), locale).ok_or_else(|| {
        RpcError::new(
            NOT_FOUND,
            "loc_unknown_locale",
            format!("no loaded locale or parent of `{locale}`"),
            "`available` in localization.status lists the loaded locales",
        )
    })?;
    if persist && !world.contains_resource::<GameSettings>() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "no_settings",
            "this app has no saved settings to write the language to",
            "add the framework SettingsPlugin, or call without `persist`",
        ));
    }
    let mut l = world.resource_mut::<Localization>();
    let previous = l.locale().to_owned();
    let changed = l.set_locale(&matched);
    let written = if persist {
        world.resource_mut::<GameSettings>().language.clone_from(&matched);
        Some(matched)
    } else {
        None
    };
    let mut out = status_json(world, world.resource::<Localization>());
    out["previous"] = json!(previous);
    out["changed"] = json!(changed);
    out["persisted"] = json!(persist);
    out["saved_language"] = json!(written);
    // `LocaleChanged` goes out on the next frame's Update.
    out["message_next_frame"] = json!(changed);
    Ok(out)
}

/// `localization.resolve { key, args?, locale? }`.
pub(crate) fn resolve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let l = localization(world)?;
    let key = string_param(
        params,
        "key",
        "bad_key",
        "pass a Fluent message id such as \"menu-start\"",
    )?;
    let mut args = Vec::new();
    match params.get("args").filter(|v| !v.is_null()) {
        None => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                let text = match value {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(invalid(
                            "bad_args",
                            format!("argument `{name}` must be a string, number or boolean"),
                            "args is an object such as { \"count\": 3 }",
                        ));
                    }
                };
                args.push((name.clone(), text));
            }
        }
        Some(_) => {
            return Err(invalid(
                "bad_args",
                "`args` must be an object",
                "args is an object such as { \"count\": 3 }",
            ));
        }
    }
    let (text, provider) = match crate::params::opt_str(params, "locale")? {
        Some(locale) => {
            let chain = l.chain(locale);
            if chain.is_empty() {
                return Err(RpcError::new(
                    NOT_FOUND,
                    "loc_unknown_locale",
                    format!("no loaded locale in the chain of `{locale}`"),
                    "`available` in localization.status lists the loaded locales",
                ));
            }
            let found = chain
                .iter()
                .find_map(|c| l.resolve_in(c, key, &args).map(|t| (t, c.clone())));
            match found {
                Some((text, provider)) => (Some(text), Some(provider)),
                None => (None, None),
            }
        }
        None => {
            let provider = l.provider_of(key);
            let text = provider.as_ref().map(|_| l.resolve(key, &args));
            (text, provider)
        }
    };
    Ok(json!({
        "key": key,
        "text": text,
        "missing": provider.is_none(),
        "provider": provider,
        "pseudo": l.pseudo(),
        "selected": l.locale(),
    }))
}

/// `localization.set_pseudo { on }`.
pub(crate) fn set_pseudo(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    localization(world)?;
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("bad_flag", "`on` must be true or false", "pass a JSON boolean"))?;
    let mut l = world.resource_mut::<Localization>();
    let changed = l.set_pseudo(on);
    let mut out = status_json(world, world.resource::<Localization>());
    out["changed"] = json!(changed);
    Ok(out)
}

fn flag(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| {
            invalid(
                "bad_flag",
                format!("`{key}` must be true or false"),
                "pass a JSON boolean",
            )
        }),
    }
}

/// `localization.set_key_display { on }` (spec 36 § 5): every text shows its key as `⟪key⟫`.
pub(crate) fn set_key_display(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    localization(world)?;
    let on = flag(params, "on")?
        .ok_or_else(|| invalid("bad_flag", "`on` must be true or false", "pass a JSON boolean"))?;
    let changed = world.resource_mut::<Localization>().set_key_display(on);
    let mut out = status_json(world, world.resource::<Localization>());
    out["on"] = json!(on);
    out["changed"] = json!(changed);
    Ok(out)
}

/// `localization.coverage { locale? }` (spec 36 § 10): per loaded locale (or one), how much of the
/// source is translated, untranslated (with placeholders), stale against the source text, and the percent.
pub(crate) fn coverage(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let l = localization(world)?;
    let only = match params.get("locale").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            invalid(
                "bad_locale",
                "`locale` must be a non-empty string",
                "pass a tag such as \"fr\"",
            )
        })?),
    };
    let locales: Vec<String> = match only {
        Some(tag) => vec![l.nearest_loaded(tag).ok_or_else(|| {
            RpcError::new(
                NOT_FOUND,
                "loc_unknown_locale",
                format!("no loaded locale or parent of `{tag}`"),
                "`available` in localization.status lists the loaded locales",
            )
        })?],
        None => l.available.clone(),
    };
    let rows: Vec<Value> = locales
        .iter()
        .filter_map(|tag| l.coverage(tag))
        .map(|c| {
            json!({
                "locale": c.locale,
                "messages": c.messages,
                "translated": c.translated,
                "untranslated": c.untranslated,
                "placeholders": c.placeholders,
                "stale": c.stale,
                "percent": (c.percent * 10.0).round() / 10.0,
            })
        })
        .collect();
    Ok(json!({
        "source": l.fallback(),
        "origin": l.origin().as_str(),
        "locales": rows,
    }))
}

/// `localization.set_audio_locale { locale?, persist? }` (spec 36 § 5): sets the spoken-audio locale, or
/// with `locale` absent clears it so audio follows the text locale. `persist` stores it in
/// `GameSettings::audio_language`.
pub(crate) fn set_audio_locale(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    localization(world)?;
    let tag = match params.get("locale").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| {
                    invalid(
                        "bad_locale",
                        "`locale` must be a string",
                        "pass a tag such as \"ja\"",
                    )
                })?
                .to_owned(),
        ),
    };
    let persist = flag(params, "persist")?.unwrap_or(false);
    if persist && !world.contains_resource::<GameSettings>() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "no_settings",
            "this app has no saved settings to write the audio language to",
            "add the framework SettingsPlugin, or call without `persist`",
        ));
    }
    let mut l = world.resource_mut::<Localization>();
    let previous = l.audio_locale().to_owned();
    let changed = l.set_audio_locale(tag.as_deref()).map_err(|e| {
        invalid(
            "bad_locale",
            e.to_string(),
            "pass a BCP 47 tag such as \"ja\" or \"pt-BR\", or omit `locale` to follow the text",
        )
    })?;
    let saved = l.audio_override().unwrap_or("").to_owned();
    let out = {
        let l = world.resource::<Localization>();
        json!({
            "text": l.locale(),
            "audio": l.audio_locale(),
            "override": l.audio_override(),
            "chain": l.audio_chain(),
            "previous": previous,
            "changed": changed,
            "persisted": persist,
            // `AudioLocaleChanged` goes out on the next frame's Update.
            "message_next_frame": changed,
        })
    };
    if persist {
        world.resource_mut::<GameSettings>().audio_language = saved;
    }
    Ok(out)
}

/// `localization.validate { strict? }`: the validator over the loaded locales.
pub(crate) fn validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let l = localization(world)?;
    let strict = match params.get("strict").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| {
            invalid(
                "bad_flag",
                "`strict` must be true or false",
                "pass a JSON boolean",
            )
        })?,
    };
    let report = l.validate();
    Ok(json!({
        "ok": !report.fails(strict),
        "errors": report.count(regia_ui::loc_check::Severity::Error),
        "warnings": report.count(regia_ui::loc_check::Severity::Warning),
        "locales": report.locales.iter().map(|s| json!({
            "locale": s.locale, "keys": s.keys, "missing": s.missing,
            "untranslated": s.untranslated, "extra": s.extra, "chain": s.chain,
        })).collect::<Vec<_>>(),
        "findings": report.findings.iter().map(|f| json!({
            "code": f.code, "severity": f.severity.as_str(), "locale": f.locale,
            "key": f.key, "line": f.line, "message": f.message,
        })).collect::<Vec<_>>(),
    }))
}

/// How many findings `localization.glyph_coverage` returns before it sets `truncated`.
const GLYPH_FINDING_CAP: usize = 500;

/// `localization.glyph_coverage { locale?, pseudo?, charset? }` (spec 36, ADR-2550): the messages of the loaded
/// locales (or one) that hold a character no font of the running text system covers, with the message id
/// and line, and optionally each locale's character set (for font subsetting). The chain is every face the
/// app's `TextSystem` holds (the project's loaded fonts, the bundled set and, on a desktop, the system
/// fonts); an app without a text system is checked against the bundled set. The pseudo-locale (the
/// source text pseudo-localized in memory) is included unless `pseudo` is false.
pub(crate) fn glyph_coverage(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_ui::loc_glyphs::{self, FontChain};
    let l = localization(world)?;
    let only = match params.get("locale").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let tag = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                invalid(
                    "bad_locale",
                    "`locale` must be a non-empty string",
                    "pass a tag such as \"fr\"",
                )
            })?;
            Some(l.nearest_loaded(tag).ok_or_else(|| {
                RpcError::new(
                    NOT_FOUND,
                    "loc_unknown_locale",
                    format!("no loaded locale or parent of `{tag}`"),
                    "`available` in localization.status lists the loaded locales",
                )
            })?)
        }
    };
    let pseudo = flag(params, "pseudo")?.unwrap_or(true);
    let charset = flag(params, "charset")?.unwrap_or(false);
    let chain = world
        .get_resource::<TextSystem>()
        .map_or_else(FontChain::embedded, |text| {
            FontChain::from_font_db(text.font_system.db())
        });
    let locales: Vec<(String, Vec<(String, String)>)> = l
        .available
        .iter()
        .filter_map(|tag| {
            l.source_text(tag).map(|text| {
                (
                    tag.clone(),
                    vec![(format!("localization/{tag}"), text.to_owned())],
                )
            })
        })
        .collect();
    let report = loc_glyphs::check_glyphs(&locales, l.fallback(), &chain, pseudo);
    let wanted = |locale: &str| {
        only.as_deref()
            .is_none_or(|o| o == locale || locale == regia_ui::loc_pseudo::PSEUDO_LOCALE)
    };
    let mut findings = Vec::new();
    let mut total = 0usize;
    for f in report.findings.iter().filter(|f| wanted(&f.locale)) {
        total += 1;
        if findings.len() < GLYPH_FINDING_CAP {
            findings.push(json!({
                "code": f.code, "locale": f.locale, "key": f.key, "line": f.line, "message": f.message,
            }));
        }
    }
    let rows: Vec<Value> = report
        .charsets
        .iter()
        .filter(|(locale, _)| wanted(locale))
        .map(|(locale, chars)| {
            let mut row = json!({
                "locale": locale,
                "characters": chars.len(),
                "missing_messages": report.findings.iter().filter(|f| &f.locale == locale).count(),
            });
            if charset {
                row["charset"] = json!(chars.iter().collect::<String>());
            }
            row
        })
        .collect();
    Ok(json!({
        "source": l.fallback(),
        "fonts": chain.names(),
        "ok": total == 0,
        "locales": rows,
        "findings": findings,
        "truncated": total > findings.len(),
    }))
}

/// `localization.plural { locale, numbers }`: CLDR categories from Regia's table (no app state).
pub(crate) fn plural_method(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let locale = string_param(params, "locale", "bad_locale", "pass a locale tag such as \"ru\"")?;
    let Some(categories) = plural::categories(locale) else {
        return Err(RpcError::new(
            NOT_FOUND,
            "loc_unknown_plural_locale",
            format!("the plural table does not know `{locale}`"),
            "see book agents/localization.md for the languages in the table",
        ));
    };
    let numbers = params.get("numbers").and_then(Value::as_array).ok_or_else(|| {
        invalid(
            "bad_number",
            "`numbers` must be an array",
            "pass [1, 2, \"1.50\"]",
        )
    })?;
    let mut rows = Vec::new();
    for n in numbers {
        let text = match n {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        let category = plural::category_of_text(locale, &text).ok_or_else(|| {
            invalid(
                "bad_number",
                format!("`{n}` is not a plain decimal number"),
                "write numbers as 21 or \"1.50\" (no exponent)",
            )
        })?;
        rows.push(json!({ "number": text, "category": category.as_str() }));
    }
    Ok(json!({
        "locale": locale,
        "categories": categories.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "numbers": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        let l = Localization::new(
            vec![
                ("en".to_owned(), "menu-start = Start\nitems = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\nonly-en = English\n".to_owned()),
                ("fr".to_owned(), "menu-start = Commencer\nitems = { $count ->\n    [one] { $count } objet\n   *[other] { $count } objets\n}\n".to_owned()),
                ("ru".to_owned(), "menu-start = Начать\n".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("locales");
        world.insert_resource(l);
        world
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{err:?}"))
    }

    #[test]
    fn status_and_switch_with_parent_fallback() {
        let mut w = world();
        let out = status(&mut w, &json!({})).expect("status");
        assert_eq!(out["selected"], "en");
        let out = set_locale(&mut w, &json!({ "locale": "fr-CA" })).expect("switch");
        assert_eq!(out["selected"], "fr");
        assert_eq!(out["changed"], true);
        assert_eq!(out["chain"], json!(["fr", "en"]));
        let err = set_locale(&mut w, &json!({ "locale": "de" })).expect_err("unknown");
        assert!(code(&err).contains("loc_unknown_locale"), "{err:?}");
        let err = set_locale(&mut w, &json!({})).expect_err("bad");
        assert!(code(&err).contains("bad_locale"), "{err:?}");
    }

    #[test]
    fn resolve_reports_provider_and_missing() {
        let mut w = world();
        set_locale(&mut w, &json!({ "locale": "fr" })).expect("fr");
        let out = resolve(&mut w, &json!({ "key": "items", "args": { "count": 0 } })).expect("items");
        assert_eq!(out["text"], "0 objet");
        assert_eq!(out["provider"], "fr");
        let out = resolve(&mut w, &json!({ "key": "only-en" })).expect("fallback");
        assert_eq!(out["provider"], "en");
        let out = resolve(&mut w, &json!({ "key": "nope" })).expect("missing");
        assert_eq!(out["missing"], true);
        assert_eq!(out["text"], Value::Null);
        let out = resolve(&mut w, &json!({ "key": "menu-start", "locale": "ru" })).expect("ru");
        assert_eq!(out["text"], "Начать");
        let err = resolve(&mut w, &json!({ "key": "items", "args": [1] })).expect_err("args");
        assert!(code(&err).contains("bad_args"), "{err:?}");
    }

    #[test]
    fn pseudo_validate_and_plural() {
        let mut w = world();
        let out = set_pseudo(&mut w, &json!({ "on": true })).expect("pseudo");
        assert_eq!(out["pseudo"], true);
        let out = resolve(&mut w, &json!({ "key": "menu-start" })).expect("pseudo text");
        assert_eq!(out["text"], "[Šŧáŕŧ~~]");
        // An explicit locale still runs the bundle transform and skips the brackets.
        let out = resolve(&mut w, &json!({ "key": "menu-start", "locale": "en" })).expect("locale");
        assert_eq!(out["text"], "Šŧáŕŧ~~");
        assert_eq!(out["pseudo"], true);
        let out = validate(&mut w, &json!({})).expect("validate");
        let codes: Vec<&str> = out["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .filter_map(|f| f["code"].as_str())
            .collect();
        assert!(codes.contains(&"loc_untranslated"), "{codes:?}");
        let out =
            plural_method(&mut w, &json!({ "locale": "ru", "numbers": [1, 3, 5, "1.5"] })).expect("plural");
        let got: Vec<&str> = out["numbers"]
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|r| r["category"].as_str())
            .collect();
        assert_eq!(got, vec!["one", "few", "many", "other"]);
        let err = plural_method(&mut w, &json!({ "locale": "tlh", "numbers": [1] })).expect_err("unknown");
        assert!(code(&err).contains("loc_unknown_plural_locale"), "{err:?}");
        let err = plural_method(&mut w, &json!({ "locale": "ru", "numbers": ["1e3"] })).expect_err("bad");
        assert!(code(&err).contains("bad_number"), "{err:?}");
    }

    #[test]
    fn no_localization_is_an_error() {
        let mut w = World::new();
        let err = status(&mut w, &json!({})).expect_err("none");
        assert!(code(&err).contains("loc_not_loaded"), "{err:?}");
    }

    #[test]
    fn status_reports_startup_names_and_font_locale() {
        let mut w = World::new();
        let sources = vec![
            ("en".to_owned(), "locale-name = English\n".to_owned()),
            ("de".to_owned(), "locale-name = Deutsch\n".to_owned()),
            ("fr".to_owned(), "hello = Salut\n".to_owned()),
        ];
        let mut l = Localization::new(sources, Some("en".to_owned())).expect("locales");
        let hints = regia_platform::LocaleHints {
            flag: Some("xx".into()),
            steam: Some("de".into()),
            ..regia_platform::LocaleHints::isolated()
        };
        let start = regia_ui::locale_startup::choose_startup_locale(&mut l, &hints);
        w.insert_resource(l);
        w.insert_resource(start);
        w.insert_resource(TextSystem::embedded_only());
        let out = status(&mut w, &json!({})).expect("status");
        assert_eq!(out["selected"], "de");
        assert_eq!(out["startup"]["source"], "steam");
        assert_eq!(out["startup"]["raw"], "de");
        assert_eq!(
            out["startup"]["skipped"][0],
            json!({ "source": "flag", "raw": "xx" })
        );
        assert_eq!(out["names"]["de"], "Deutsch");
        assert_eq!(out["names"]["en"], "English");
        assert!(out["names"]["fr"].is_null());
        assert!(out["font_locale"].is_string());
    }

    #[test]
    fn audit_locale_switch_matches_startup_and_persists_loaded_tag() {
        for (tags, requested, selected) in [
            (vec!["en", "pt", "pt-BR"], "pt-br", "pt-BR"),
            (vec!["en", "fr-CA"], "fr-ca", "fr-CA"),
            (vec!["en", "fr-CA"], "fr_CA", "fr-CA"),
            (vec!["en", "pt"], "pt-BR", "pt"),
        ] {
            let mut w = World::new();
            let sources = tags
                .into_iter()
                .map(|tag| (tag.to_owned(), "hello = Hello\n".to_owned()))
                .collect();
            w.insert_resource(Localization::new(sources, Some("en".to_owned())).expect("locales"));
            w.insert_resource(GameSettings::default());
            let out = set_locale(&mut w, &json!({ "locale": requested, "persist": true })).expect("switch");
            assert_eq!(out["selected"], selected, "request {requested}");
            assert_eq!(out["saved_language"], selected);
            assert_eq!(w.resource::<GameSettings>().language, selected);
            let startup = regia_ui::locale_startup::choose_startup_locale(
                &mut w.resource_mut::<Localization>(),
                &regia_platform::LocaleHints {
                    settings: Some(selected.to_owned()),
                    ..regia_platform::LocaleHints::isolated()
                },
            );
            assert_eq!(startup.selected, selected);
        }
    }

    #[test]
    fn an_agent_switch_is_not_saved_unless_asked() {
        let mut w = world();
        w.insert_resource(GameSettings::default());
        let out = set_locale(&mut w, &json!({ "locale": "fr" })).expect("switch");
        assert_eq!(out["persisted"], false);
        assert!(out["saved_language"].is_null());
        assert_eq!(w.resource::<GameSettings>().language, "");
        let out = set_locale(&mut w, &json!({ "locale": "ru", "persist": true })).expect("persist");
        assert_eq!(out["persisted"], true);
        assert_eq!(out["saved_language"], "ru");
        assert_eq!(w.resource::<GameSettings>().language, "ru");
        let err = set_locale(&mut w, &json!({ "locale": "fr", "persist": "yes" })).expect_err("bad flag");
        assert!(code(&err).contains("bad_flag"), "{err:?}");
        assert_eq!(
            w.resource::<GameSettings>().language,
            "ru",
            "a bad call changes nothing"
        );
    }

    #[test]
    fn persist_without_settings_is_an_error_and_changes_nothing() {
        let mut w = world();
        let err = set_locale(&mut w, &json!({ "locale": "fr", "persist": true })).expect_err("no settings");
        assert!(code(&err).contains("no_settings"), "{err:?}");
        assert_eq!(w.resource::<Localization>().locale(), "en");
    }

    #[test]
    fn key_display_coverage_and_audio_locale() {
        let mut w = world();
        let out = set_key_display(&mut w, &json!({ "on": true })).expect("on");
        assert_eq!(
            (out["on"].clone(), out["changed"].clone()),
            (json!(true), json!(true))
        );
        assert_eq!(out["key_display"], true);
        let out = resolve(&mut w, &json!({ "key": "menu-start" })).expect("text");
        assert_eq!(out["text"], "\u{27EA}menu-start\u{27EB}");
        let err = set_key_display(&mut w, &json!({})).expect_err("no flag");
        assert!(code(&err).contains("bad_flag"), "{err:?}");
        set_key_display(&mut w, &json!({ "on": false })).expect("off");

        let out = coverage(&mut w, &json!({})).expect("coverage");
        assert_eq!(out["source"], "en");
        assert_eq!(out["origin"], "files");
        let rows = out["locales"].as_array().expect("rows");
        assert_eq!(rows.len(), 3);
        let fr = rows.iter().find(|r| r["locale"] == "fr").expect("fr");
        assert_eq!(
            (fr["messages"].clone(), fr["translated"].clone()),
            (json!(3), json!(2))
        );
        assert_eq!(fr["percent"], 66.7);
        let out = coverage(&mut w, &json!({ "locale": "ru-RU" })).expect("one");
        assert_eq!(out["locales"][0]["locale"], "ru");
        let err = coverage(&mut w, &json!({ "locale": "de" })).expect_err("unknown");
        assert!(code(&err).contains("loc_unknown_locale"), "{err:?}");

        let out = set_audio_locale(&mut w, &json!({ "locale": "ja" })).expect("audio");
        assert_eq!(out["audio"], "ja");
        assert_eq!(out["text"], "en");
        assert_eq!(out["chain"], json!(["ja", "en"]));
        assert_eq!(out["changed"], true);
        let status_out = status(&mut w, &json!({})).expect("status");
        assert_eq!(status_out["audio"]["override"], "ja");
        let err = set_audio_locale(&mut w, &json!({ "locale": "!!" })).expect_err("bad");
        assert!(code(&err).contains("bad_locale"), "{err:?}");
        let err = set_audio_locale(&mut w, &json!({ "persist": true })).expect_err("no settings");
        assert!(code(&err).contains("no_settings"), "{err:?}");
        assert_eq!(
            w.resource::<Localization>().audio_locale(),
            "ja",
            "all or nothing"
        );
        w.insert_resource(GameSettings::default());
        let out = set_audio_locale(&mut w, &json!({ "locale": "fr", "persist": true })).expect("persist");
        assert_eq!(out["persisted"], true);
        assert_eq!(w.resource::<GameSettings>().audio_language, "fr");
        let out = set_audio_locale(&mut w, &json!({ "persist": true })).expect("clear");
        assert_eq!(out["audio"], "en");
        assert!(out["override"].is_null());
        assert_eq!(w.resource::<GameSettings>().audio_language, "");
    }
    #[test]
    fn glyph_coverage_reports_uncovered_characters_with_lines_and_the_pseudo_locale() {
        let mut w = World::new();
        let l = Localization::new(
            vec![
                ("en".to_owned(), "greet = Hello\n".to_owned()),
                (
                    "fr".to_owned(),
                    "greet = Salut\ncount = { $n ->\n    [one] un\n   *[other] \u{A66E}\n}\n".to_owned(),
                ),
            ],
            Some("en".to_owned()),
        )
        .expect("locales");
        w.insert_resource(l);
        let out = glyph_coverage(&mut w, &json!({ "charset": true })).expect("coverage");
        assert_eq!(out["ok"], false);
        let found = out["findings"].as_array().expect("findings");
        let fr: Vec<_> = found.iter().filter(|f| f["locale"] == "fr").collect();
        assert_eq!(fr.len(), 1, "{found:?}");
        assert_eq!(fr[0]["code"], "loc_glyph_missing");
        assert_eq!(fr[0]["key"], "count");
        assert_eq!(fr[0]["line"], 4);
        let rows = out["locales"].as_array().expect("rows");
        assert!(rows.iter().any(|r| r["locale"] == "en-XA"), "{rows:?}");
        let fr_row = rows.iter().find(|r| r["locale"] == "fr").expect("fr");
        assert!(fr_row["charset"].as_str().is_some_and(|s| s.contains('\u{A66E}')));
        assert!(out["fonts"].as_array().is_some_and(|f| !f.is_empty()));
        // One locale, no pseudo.
        let out = glyph_coverage(&mut w, &json!({ "locale": "en", "pseudo": false })).expect("en");
        assert_eq!(out["ok"], true);
        assert_eq!(out["locales"].as_array().map(Vec::len), Some(1));
        let err = glyph_coverage(&mut w, &json!({ "locale": "de" })).expect_err("unknown");
        assert!(code(&err).contains("loc_unknown_locale"), "{err:?}");
        let err = glyph_coverage(&mut w, &json!({ "pseudo": "no" })).expect_err("flag");
        assert!(code(&err).contains("bad_flag"), "{err:?}");
        let mut empty = World::new();
        let err = glyph_coverage(&mut empty, &json!({})).expect_err("none");
        assert!(code(&err).contains("loc_not_loaded"), "{err:?}");
    }
}
