// SPDX-License-Identifier: MPL-2.0
//! Localization (`specs/18` § 6): Fluent `.ftl` resources per locale under
//! `assets://localization/<locale>/`, a [`Localization`] resource selecting the active locale,
//! runtime language switching (bump of the generation counter → all texts re-resolve next frame),
//! and `⟦key⟧` markers for missing keys in dev builds.
//!
//! The content-data workflow (`docs/specs/content-data.md` § 1, ADR-1466) adds a fallback chain
//! ([`fallback_chain`]: `fr-CA` → `fr` → the project fallback, plus edges from
//! `assets://localization/localization.ron`), a [`LocaleChanged`] message after every switch,
//! pseudo-localization ([`Localization::set_pseudo`]) and the [`loc!`](crate::loc) macro for keys
//! in Rust code.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use fluent_bundle::FluentArgs;
use fluent_bundle::FluentResource;
use fluent_bundle::FluentValue;
use fluent_bundle::concurrent::FluentBundle;
use regia_app::prelude::*;
use unic_langid::LanguageIdentifier;

use crate::UiError;

/// Monotonic locale generation: bumped on every language switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalizationGeneration(pub u64);

/// The active localization state.
#[derive(Resource, Default)]
// Localization is not Debug (FluentBundle); tests use Result Display.
pub struct Localization {
    bundles: HashMap<String, FluentBundle<FluentResource>>,
    /// Message ids per locale (for coverage reports).
    keys: BTreeMap<String, BTreeSet<String>>,
    fallback: String,
    /// Selected locale tag ("en-US", "ar", "ja").
    pub selected: String,
    /// Available locale tags, sorted.
    pub available: Vec<String>,
    /// Bumped on every switch; text digest includes it.
    pub generation: LocalizationGeneration,
    /// Extra fallback edges by locale (spec § 1.3).
    fallbacks: BTreeMap<String, Vec<String>>,
    /// Pseudo-localization on (spec § 1.8).
    pseudo: bool,
    /// Key display on (spec 36 § 5): `resolve` shows `⟪key⟫` instead of text.
    key_display: bool,
    /// Each locale's combined `.ftl` text as authored (for the validator and agents).
    sources: BTreeMap<String, String>,
    /// The player's audio locale (spec 36 § 5); `None` follows the text locale.
    audio_override: Option<String>,
    /// Bumped whenever the effective audio locale changes ([`AudioLocaleChanged`]).
    audio_generation: u64,
    /// Where the texts came from: `.ftl` files or shipping bundles.
    origin: LocalizationOrigin,
    /// The game's UTC offset in seconds, read by `DATETIME()` for unix-seconds arguments
    /// (`crate::loc_format`, ADR-2698); shared with every bundle's function.
    utc_offset: std::sync::Arc<std::sync::atomic::AtomicI32>,
}

/// Where a [`Localization`]'s texts were read from (`localization.status` `origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalizationOrigin {
    /// Development: `assets://localization/<locale>/*.ftl` (or built in memory).
    #[default]
    Files,
    /// A packaged build: the bundles listed by `loc/index.ron` ([`Localization::load_bundles`]).
    Bundle,
}

impl LocalizationOrigin {
    /// `files` or `bundle`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Bundle => "bundle",
        }
    }
}

/// The path of the shipping bundle index inside the packaged assets (spec 36 § 4).
pub const BUNDLE_INDEX_PATH: &str = "loc/index.ron";

/// The fallback chain of `locale` (`docs/specs/content-data.md` § 1.3): the locale, each of its
/// `extra` fallbacks followed by their own chains, its parents (`fr-CA` → `fr`), then `source`.
/// Each locale appears once; only locales in `loaded` are returned.
#[must_use]
pub fn fallback_chain(
    locale: &str,
    loaded: &BTreeSet<String>,
    extra: &BTreeMap<String, Vec<String>>,
    source: &str,
) -> Vec<String> {
    fn visit(
        locale: &str,
        loaded: &BTreeSet<String>,
        extra: &BTreeMap<String, Vec<String>>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if locale.is_empty() || !seen.insert(locale.to_owned()) {
            return;
        }
        if loaded.contains(locale) {
            out.push(locale.to_owned());
        }
        for next in extra.get(locale).into_iter().flatten() {
            visit(next, loaded, extra, seen, out);
        }
        if let Some((parent, _)) = locale.rsplit_once('-') {
            visit(parent, loaded, extra, seen, out);
        }
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    visit(locale, loaded, extra, &mut seen, &mut out);
    visit(source, loaded, extra, &mut seen, &mut out);
    out
}

/// Sent the frame after the selected locale changed (spec § 1.4). Read it with
/// `MessageReader<LocaleChanged>`.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct LocaleChanged {
    /// The locale selected before.
    pub from: String,
    /// The locale selected now.
    pub to: String,
    /// The localization generation after the switch.
    pub generation: u64,
}

/// Sent the frame after the effective audio locale changed (spec 36 § 5): the player set or cleared an
/// audio override, or the text locale moved while the audio locale follows it. Text switches keep
/// sending [`LocaleChanged`] unchanged.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct AudioLocaleChanged {
    /// The audio locale before.
    pub from: String,
    /// The audio locale now.
    pub to: String,
    /// [`Localization::audio_generation`] after the change.
    pub generation: u64,
}

/// A localization key with its arguments, as written by [`loc!`](crate::loc) in Rust code.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocText {
    /// The Fluent message key.
    pub key: &'static str,
    /// Arguments, each formatted with `Display`.
    pub args: Vec<(String, String)>,
}

impl LocText {
    /// Resolves the text in `localization`'s selected locale (with its fallback chain).
    #[must_use]
    pub fn resolve(&self, localization: &Localization) -> String {
        localization.resolve(self.key, &self.args)
    }
}

/// A localization key in Rust code: `loc!("menu-start")` or `loc!("items", count = n)`.
///
/// The value is a [`LocText`]; resolve it with [`LocText::resolve`]. `regia loc gather` finds keys
/// by looking for `loc!("` in `.rs` files, so the key must be a string literal.
#[macro_export]
macro_rules! loc {
    ($key:literal $(,)?) => {
        $crate::localization::LocText { key: $key, args: ::std::vec::Vec::new() }
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::localization::LocText {
            key: $key,
            args: ::std::vec![$((::std::string::String::from(stringify!($name)), ($value).to_string())),+],
        }
    };
}

/// The localization plugin's systems: [`LocaleChanged`] after each switch.
pub struct LocalizationPlugin;

impl Plugin for LocalizationPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<LocaleChanged>()
            .add_message::<AudioLocaleChanged>()
            .add_message::<crate::locale_startup::PlayerSetLocale>()
            .add_message::<crate::locale_startup::LocaleWriteBack>()
            .add_message::<regia_asset::localized::LocalizedAssetChanged>()
            .init_resource::<regia_asset::localized::LocaleChains>()
            .add_systems(
                Update,
                (
                    crate::locale_startup::apply_player_locale,
                    send_locale_changed,
                    send_audio_locale_changed,
                    sync_localized_assets,
                )
                    .chain(),
            );
    }
}

/// Sends [`LocaleChanged`] when the generation moved since the last frame.
pub fn send_locale_changed(
    localization: Option<Res<Localization>>,
    mut last: Local<Option<(u64, String)>>,
    mut changed: MessageWriter<LocaleChanged>,
) {
    let Some(localization) = localization else {
        return;
    };
    let now = localization.generation.0;
    match last.as_ref() {
        None => *last = Some((now, localization.selected.clone())),
        Some((generation, from)) if *generation != now => {
            if *from != localization.selected {
                changed.write(LocaleChanged {
                    from: from.clone(),
                    to: localization.selected.clone(),
                    generation: now,
                });
            }
            *last = Some((now, localization.selected.clone()));
        }
        Some(_) => {}
    }
}

/// Sends [`AudioLocaleChanged`] when the effective audio locale moved since the last frame.
pub fn send_audio_locale_changed(
    localization: Option<Res<Localization>>,
    mut last: Local<Option<(u64, String)>>,
    mut changed: MessageWriter<AudioLocaleChanged>,
) {
    let Some(localization) = localization else {
        return;
    };
    let now = localization.audio_generation;
    let audio = localization.audio_locale().to_owned();
    match last.as_ref() {
        None => *last = Some((now, audio)),
        Some((generation, from)) if *generation != now => {
            if *from != audio {
                changed.write(AudioLocaleChanged {
                    from: from.clone(),
                    to: audio.clone(),
                    generation: now,
                });
            }
            *last = Some((now, audio));
        }
        Some(_) => {}
    }
}

/// Keeps [`regia_asset::localized::LocaleChains`] (and the asset server's copy) in step with the text
/// and audio chains, and after a switch re-resolves localized assets (spec 36 § 6): each asset whose
/// path moved is announced as a [`regia_asset::localized::LocalizedAssetChanged`] message, so its owner
/// loads the new variant without a restart.
pub fn sync_localized_assets(
    localization: Option<Res<Localization>>,
    mut chains: ResMut<regia_asset::localized::LocaleChains>,
    server: Option<ResMut<regia_asset::AssetServer>>,
    mut seen: Local<Option<(u64, u64)>>,
    mut changed: MessageWriter<regia_asset::localized::LocalizedAssetChanged>,
) {
    let Some(localization) = localization else {
        return;
    };
    let generations = (localization.generation.0, localization.audio_generation);
    if *seen != Some(generations) {
        *seen = Some(generations);
        let wanted = regia_asset::localized::LocaleChains {
            text: localization.chain(localization.locale()),
            audio: localization.audio_chain(),
        };
        if *chains != wanted {
            *chains = wanted;
        }
    }
    let Some(mut server) = server else {
        return;
    };
    if server.locale_chains() != &*chains {
        server.set_locale_chains(chains.clone());
        changed.write_batch(server.reload_localized());
    }
}

impl Localization {
    /// Builds a localization from `(locale, ftl-source)` pairs. `fallback` is used for missing
    /// locales; the first locale is the default selection.
    pub fn new(sources: Vec<(String, String)>, fallback: Option<String>) -> Result<Self, UiError> {
        let mut this = Self::default();
        for (locale, source) in sources {
            this.load_locale(locale, source)?;
        }
        this.finish(fallback);
        Ok(this)
    }

    /// Like [`Localization::new`], but a locale whose tag or text does not load is left out and
    /// its error returned next to the rest, so one bad locale does not remove every other one.
    pub(crate) fn new_lenient(
        sources: Vec<(String, String)>,
        fallback: Option<String>,
    ) -> (Self, Vec<UiError>) {
        let mut this = Self::default();
        let mut errors = Vec::new();
        for (locale, source) in sources {
            if let Err(e) = this.load_locale(locale.clone(), source) {
                this.sources.remove(&locale);
                errors.push(e);
            }
        }
        this.finish(fallback);
        (this, errors)
    }

    /// Adds or replaces one locale's texts from a combined `.ftl` source, keeping every other
    /// loaded locale. A host that already loaded its own texts (the UI plugin's startup load, or
    /// a game's bundles) keeps them; pass the existing source plus the new messages to merge
    /// ([`Localization::source_text`] reads a locale's current text back). The locale's messages
    /// replace earlier ones with the same id (`add_resource_overriding`); other locales are
    /// untouched. Returns the parse error and changes nothing when the source does not parse.
    ///
    /// After a successful add the available-locale list is refreshed and the generation moves, so
    /// texts on screen re-resolve next frame; the selected locale stays as it was
    /// ([`Localization::set_locale`] moves it).
    ///
    /// # Errors
    /// [`UiError::Localization`] when `locale` is not a valid language tag or the source has
    /// `.ftl` parse errors.
    pub fn add_locale(&mut self, locale: String, source: String) -> Result<(), UiError> {
        self.load_locale(locale, source)?;
        // Shaped text is keyed by the generation: without a bump, replaced messages of the
        // selected locale (or of its fallback chain) stayed on screen with the old wording.
        self.generation.0 += 1;
        Ok(())
    }

    /// [`Localization::add_locale`] without the generation bump (construction: nothing is on
    /// screen yet, and a fresh localization starts at generation 0).
    fn load_locale(&mut self, locale: String, source: String) -> Result<(), UiError> {
        let previous = self.sources.get(&locale).cloned();
        if let Err(err) = self.add_locale_inner(locale.clone(), source) {
            // `add_locale_inner` records the raw source before it parses. A new locale is
            // dropped again; a replacement keeps the source that was already loaded. The
            // bundle is written only after a successful parse, so this restores the text too.
            match previous {
                Some(old) => {
                    self.sources.insert(locale, old);
                }
                None => {
                    self.sources.remove(&locale);
                }
            }
            return Err(err);
        }
        self.available = self.bundles.keys().cloned().collect();
        self.available.sort();
        if !self.bundles.contains_key(&self.fallback) {
            self.fallback = self.available.first().cloned().unwrap_or_else(|| "en".to_owned());
        }
        Ok(())
    }

    fn add_locale_inner(&mut self, locale: String, source: String) -> Result<(), UiError> {
        let langid: LanguageIdentifier = locale
            .parse()
            .map_err(|e| UiError::Localization(format!("bad locale {locale:?}: {e}")))?;
        self.sources.insert(locale.clone(), source.clone());
        // Plural selects pick through Regia's CLDR table (`route_plurals`); a source that does
        // not parse is left alone so the error below names the author's text.
        let source = route_plurals(&source).unwrap_or(source);
        let source_text = source.clone();
        let resource = FluentResource::try_new(source).map_err(|(_, errs)| {
            UiError::Localization(format!(
                "ftl parse errors in {locale}: {}",
                errs.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        })?;
        let mut bundle = FluentBundle::new_concurrent(vec![langid]);
        // Isolating marks (U+2068/U+2069) are for bidi safety; HUD bindings want bare text.
        bundle.set_use_isolating(false);
        // concurrent::FluentBundle::add_resource_overriding returns () (overwrites).
        bundle.add_resource_overriding(resource);
        let plural_locale = locale.clone();
        bundle
            .add_function(PLURAL_FUNCTION, move |positional, _named| {
                match positional.first() {
                    // `NUMBER()` results select by the digits they show (ADR-2698); with
                    // `type: "ordinal"` they select ordinal categories instead (ADR-3359).
                    Some(value @ FluentValue::Custom(_)) => {
                        match crate::loc_format::formatted_operands(value) {
                            Some(operands) => {
                                let kind = if crate::loc_format::formatted_is_ordinal(value) {
                                    crate::plural::PluralKind::Ordinal
                                } else {
                                    crate::plural::PluralKind::Cardinal
                                };
                                FluentValue::from(
                                    crate::plural::category_of_kind(&plural_locale, kind, &operands).as_str(),
                                )
                            }
                            None => value.clone(),
                        }
                    }
                    Some(FluentValue::Number(number)) => {
                        match crate::plural::PluralOperands::from_decimal(&number.as_string()) {
                            Some(operands) => {
                                FluentValue::from(crate::plural::category(&plural_locale, &operands).as_str())
                            }
                            None => FluentValue::Number(number.clone()),
                        }
                    }
                    Some(other) => other.clone(),
                    None => FluentValue::None,
                }
            })
            .map_err(|e| UiError::Localization(format!("{locale}: {e}")))?;
        crate::loc_format::register_functions(&mut bundle, &locale, self.utc_offset.clone())
            .map_err(|e| UiError::Localization(format!("{locale}: {e}")))?;
        self.keys.insert(
            locale.clone(),
            crate::loc_manifest::ftl_keys(&source_text).into_iter().collect(),
        );
        self.bundles.insert(locale, bundle);
        Ok(())
    }

    fn finish(&mut self, fallback: Option<String>) {
        self.available = self.bundles.keys().cloned().collect();
        self.available.sort();
        self.fallback = fallback
            .filter(|f| self.bundles.contains_key(f))
            .or_else(|| self.available.first().cloned())
            .unwrap_or_else(|| "en".to_owned());
        self.selected = self.fallback.clone();
    }

    /// Sets the UTC offset `DATETIME()` shows unix-seconds arguments at (the player's local time
    /// zone, as the game knows it; there is no time-zone database). Returns `true` when the offset
    /// changed (texts re-resolve next frame); an offset beyond ±18 hours is refused (`false`).
    pub fn set_utc_offset_seconds(&mut self, seconds: i32) -> bool {
        use std::sync::atomic::Ordering;
        if seconds.abs() > crate::loc_format::MAX_UTC_OFFSET_SECONDS
            || self.utc_offset.load(Ordering::Relaxed) == seconds
        {
            return false;
        }
        self.utc_offset.store(seconds, Ordering::Relaxed);
        self.generation.0 += 1;
        true
    }

    /// The UTC offset `DATETIME()` uses for unix-seconds arguments (0 until the game sets one).
    #[must_use]
    pub fn utc_offset_seconds(&self) -> i32 {
        self.utc_offset.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The selected locale tag.
    pub fn locale(&self) -> &str {
        &self.selected
    }

    /// The fallback locale tag used when the selected locale lacks a key.
    pub fn fallback(&self) -> &str {
        &self.fallback
    }

    /// Switches the active locale. A locale that is not loaded selects its nearest loaded parent
    /// (`fr-CA` with only `fr` loaded selects `fr`). Returns `true` when the selection changed.
    pub fn set_locale(&mut self, locale: &str) -> bool {
        let Some(target) = self.nearest_loaded(locale) else {
            return false;
        };
        if target == self.selected {
            return false;
        }
        self.selected = target;
        self.generation.0 += 1;
        if self.audio_override.is_none() {
            self.audio_generation += 1;
        }
        true
    }

    /// `locale` itself when loaded, else its nearest loaded parent (`fr-CA` → `fr`); `None` when
    /// neither is loaded.
    pub fn nearest_loaded(&self, locale: &str) -> Option<String> {
        let mut tag = locale;
        loop {
            if self.bundles.contains_key(tag) {
                return Some(tag.to_owned());
            }
            tag = tag.rsplit_once('-')?.0;
        }
    }

    /// Sets the extra fallback edges (`fallbacks` of `localization.ron`) and re-resolves texts.
    pub fn set_fallbacks(&mut self, fallbacks: BTreeMap<String, Vec<String>>) {
        if fallbacks != self.fallbacks {
            self.fallbacks = fallbacks;
            self.generation.0 += 1;
        }
    }

    /// The extra fallback edges.
    pub fn fallbacks(&self) -> &BTreeMap<String, Vec<String>> {
        &self.fallbacks
    }

    /// The loaded locales `locale` resolves through, in order ([`fallback_chain`]).
    pub fn chain(&self, locale: &str) -> Vec<String> {
        let loaded: BTreeSet<String> = self.bundles.keys().cloned().collect();
        fallback_chain(locale, &loaded, &self.fallbacks, &self.fallback)
    }

    /// Turns pseudo-localization on or off (spec § 1.8). [`Localization::resolve`] accents
    /// letters, pads by about 40 % and wraps the message in `[` `]`. [`Localization::resolve_in`]
    /// runs the same accent-and-pad transform and does not add the brackets. Placeables keep
    /// their values. Returns `true` when the mode changed (texts re-resolve next frame).
    pub fn set_pseudo(&mut self, on: bool) -> bool {
        if on == self.pseudo {
            return false;
        }
        self.pseudo = on;
        let transform: Option<fn(&str) -> std::borrow::Cow<'_, str>> =
            if on { Some(crate::pseudo::transform) } else { None };
        for bundle in self.bundles.values_mut() {
            bundle.set_transform(transform);
        }
        self.generation.0 += 1;
        true
    }

    /// Whether pseudo-localization is on.
    pub fn pseudo(&self) -> bool {
        self.pseudo
    }

    /// Turns key display on or off (spec 36 § 5): while on, [`Localization::resolve`] returns
    /// `⟪key name=value⟫` for every key (the form of `loc_pseudo::key_display`), so a tester sees which
    /// element uses which key. It wins over pseudo-localization. Returns `true` when the mode changed
    /// (texts re-resolve next frame).
    pub fn set_key_display(&mut self, on: bool) -> bool {
        if on == self.key_display {
            return false;
        }
        self.key_display = on;
        self.generation.0 += 1;
        true
    }

    /// Whether key display is on.
    pub fn key_display(&self) -> bool {
        self.key_display
    }

    /// The audio locale (spec 36 § 5): the override when set, else the selected text locale. Spoken
    /// dialogue and localized audio resolve through [`Localization::audio_chain`].
    #[must_use]
    pub fn audio_locale(&self) -> &str {
        self.audio_override.as_deref().unwrap_or(&self.selected)
    }

    /// The audio override, `None` while audio follows the text locale.
    #[must_use]
    pub fn audio_override(&self) -> Option<&str> {
        self.audio_override.as_deref()
    }

    /// Sets the audio locale (`Some(tag)`) or clears it so audio follows the text locale again
    /// (`None` or an empty tag). The tag need not have text loaded: a game may ship Japanese voices
    /// with English text. `fr_CA.UTF-8` style values are normalized. Returns `Ok(true)` when the
    /// effective audio locale changed ([`AudioLocaleChanged`] goes out next frame).
    ///
    /// # Errors
    ///
    /// The tag is not a BCP 47 language tag.
    pub fn set_audio_locale(&mut self, tag: Option<&str>) -> Result<bool, UiError> {
        let wanted = match tag.map(str::trim).filter(|t| !t.is_empty()) {
            None => None,
            Some(raw) => {
                let tag = crate::locale_startup::normalize_tag(raw)
                    .filter(|t| t.parse::<LanguageIdentifier>().is_ok())
                    .ok_or_else(|| UiError::Localization(format!("bad locale {raw:?}")))?;
                Some(tag)
            }
        };
        if wanted == self.audio_override {
            return Ok(false);
        }
        let before = self.audio_locale().to_owned();
        self.audio_override = wanted;
        let changed = before != self.audio_locale();
        if changed {
            self.audio_generation += 1;
        }
        Ok(changed)
    }

    /// Bumped on every change of the effective audio locale.
    #[must_use]
    pub fn audio_generation(&self) -> u64 {
        self.audio_generation
    }

    /// The fallback chain for audio: the audio locale, its extra fallbacks and parents, then the
    /// source locale. Unlike [`Localization::chain`] the audio locale and its parents are kept even
    /// when no text is loaded for them (localized audio is found by path, spec 36 § 6).
    #[must_use]
    pub fn audio_chain(&self) -> Vec<String> {
        let audio = self.audio_locale().to_owned();
        let mut loaded: BTreeSet<String> = self.bundles.keys().cloned().collect();
        let mut tag = audio.as_str();
        loop {
            loaded.insert(tag.to_owned());
            match tag.rsplit_once('-') {
                Some((parent, _)) => tag = parent,
                None => break,
            }
        }
        fallback_chain(&audio, &loaded, &self.fallbacks, &self.fallback)
    }

    /// Where the texts came from (`files` in development, `bundle` in a packaged build).
    #[must_use]
    pub fn origin(&self) -> LocalizationOrigin {
        self.origin
    }

    /// Loads the shipping bundles a `loc/index.ron` lists (spec 36 § 4): `read(file)` returns the bytes
    /// of a file named by the index (`loc/fr.ftl`), from a pak or a directory. Each bundle's content
    /// hash is checked; the index's source locale is the fallback and its edges are applied.
    ///
    /// # Errors
    ///
    /// A bundle cannot be read, its hash does not match the index, or its text does not parse.
    pub fn load_bundles(
        index: &crate::loc_bundle::BundleIndex,
        mut read: impl FnMut(&str) -> Result<Vec<u8>, String>,
    ) -> Result<Self, UiError> {
        let mut sources = Vec::with_capacity(index.locales.len());
        for bundle in &index.locales {
            let bytes = read(&bundle.file)
                .map_err(|e| UiError::Localization(format!("cannot read bundle {}: {e}", bundle.file)))?;
            let hash = regia_asset::content_hash_hex(&bytes);
            if hash != bundle.hash {
                return Err(UiError::Localization(format!(
                    "bundle {} does not match loc/index.ron (hash {hash}, index {}); run `regia loc compile` again",
                    bundle.file, bundle.hash
                )));
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| UiError::Localization(format!("bundle {} is not UTF-8", bundle.file)))?;
            sources.push((bundle.tag.clone(), text));
        }
        let mut this = Self::new(sources, Some(index.source.clone()))?;
        this.fallbacks.clone_from(&index.fallbacks);
        this.origin = LocalizationOrigin::Bundle;
        Ok(this)
    }

    /// `locale`'s combined `.ftl` text as loaded (before plural routing).
    pub fn source_text(&self, locale: &str) -> Option<&str> {
        self.sources.get(locale).map(String::as_str)
    }

    /// Translation coverage of a loaded `locale` against the source locale (spec 36 § 10):
    /// messages, translated, untranslated, placeholders, stale and percent. `None` when the locale is
    /// not loaded.
    #[must_use]
    pub fn coverage(&self, locale: &str) -> Option<crate::loc_po::LocaleCoverage> {
        let target = self.sources.get(locale)?;
        let source = self.sources.get(&self.fallback).map_or("", String::as_str);
        Some(crate::loc_po::coverage(
            locale,
            &[(self.fallback.clone(), source.to_owned())],
            &[(locale.to_owned(), target.clone())],
        ))
    }

    /// Runs the validator (`crate::loc_check`) over the loaded locales, with this localization's
    /// fallback (as the source locale) and fallback edges. Content keys are not gathered here; the
    /// `regia loc validate` command does that.
    pub fn validate(&self) -> crate::loc_check::Report {
        let parsed: Vec<crate::loc_check::ParsedLocale> = self
            .sources
            .iter()
            .map(|(locale, text)| {
                crate::loc_check::parse_locale(locale, &[(format!("localization/{locale}"), text.clone())])
            })
            .collect();
        crate::loc_check::validate(
            &parsed,
            &crate::loc_check::CheckOptions {
                source: self.fallback.clone(),
                fallbacks: self.fallbacks.clone(),
                used_keys: None,
            },
        )
    }

    /// `locale`'s own name in its own language (its `locale-name` message, for language menus);
    /// `None` when the locale does not define it. No fallback: a missing name is not borrowed
    /// from another language.
    #[must_use]
    pub fn self_name(&self, locale: &str) -> Option<String> {
        self.resolve_in(locale, crate::locale_startup::SELF_NAME_KEY, &[])
    }

    /// Cycles to the next available locale (wraps). Returns the new locale tag.
    pub fn cycle_locale(&mut self) -> &str {
        if self.available.len() < 2 {
            return &self.selected;
        }
        let idx = self
            .available
            .iter()
            .position(|l| l == &self.selected)
            .unwrap_or(0);
        let next = (idx + 1) % self.available.len();
        self.selected = self.available[next].clone();
        self.generation.0 += 1;
        if self.audio_override.is_none() {
            self.audio_generation += 1;
        }
        &self.selected
    }

    /// Resolves `key` with optional Fluent arguments. Missing keys render `⟦key⟧` in dev builds
    /// and the bare key in shipping builds.
    ///
    /// An argument whose text is a canonical number (`"3"`, `"-1.5"`, not `"007"` or `"1e3"`) is
    /// passed to Fluent as a number, so plural selectors (`{ $count -> [one] ... *[other] ... }`)
    /// and exact numeric variants (`[0]`) choose by the active locale's plural rules.
    ///
    /// The selected locale's fallback chain is tried in order ([`Localization::chain`]).
    pub fn resolve(&self, key: &str, args: &[(String, String)]) -> String {
        if self.key_display {
            return crate::loc_pseudo::key_display(key, args);
        }
        for locale in self.chain(&self.selected) {
            if let Some(out) = self.resolve_in(&locale, key, args) {
                return if self.pseudo {
                    crate::pseudo::wrap(&out)
                } else {
                    out
                };
            }
        }
        if cfg!(debug_assertions) {
            format!("\u{27E6}{key}\u{27E7}")
        } else {
            key.to_owned()
        }
    }

    /// Resolves `key` in exactly `locale` (no fallback). `None` when the locale or the key does not
    /// exist there, or the message has no value. A value that formats to nothing is an answer: the
    /// chain stops there instead of showing the next language.
    pub fn resolve_in(&self, locale: &str, key: &str, args: &[(String, String)]) -> Option<String> {
        let bundle = self.bundles.get(locale)?;
        let value = bundle.get_message(key)?.value()?;
        let mut fluent_args = FluentArgs::new();
        for (k, v) in args {
            match canonical_number(v) {
                Some(n) => fluent_args.set(k.clone(), n),
                None => fluent_args.set(k.clone(), v.as_str()),
            }
        }
        let mut errors = Vec::new();
        Some(
            bundle
                .format_pattern(value, Some(&fluent_args), &mut errors)
                .into_owned(),
        )
    }

    /// The message ids `locale` defines itself, sorted. Empty when the locale is unknown.
    pub fn keys_in(&self, locale: &str) -> Vec<&str> {
        self.keys
            .get(locale)
            .map(|k| k.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Whether `locale` itself defines a message `key` (no fallback).
    pub fn has_in(&self, locale: &str, key: &str) -> bool {
        self.bundles.get(locale).is_some_and(|b| b.has_message(key))
    }

    /// Whether `key` resolves anywhere in the selected locale's fallback chain.
    pub fn has(&self, key: &str) -> bool {
        self.chain(&self.selected).iter().any(|l| self.has_in(l, key))
    }

    /// The locale of the chain that provides `key` for the selected locale, if any.
    pub fn provider_of(&self, key: &str) -> Option<String> {
        self.chain(&self.selected)
            .into_iter()
            .find(|l| self.has_in(l, key))
    }
}

/// The Fluent function plural selects are routed through (`route_plurals`).
pub const PLURAL_FUNCTION: &str = "REGIA_PLURAL";

/// Rewrites every plural select (`{ $n -> [one] ... *[other] ... }`) to choose its category with
/// [`PLURAL_FUNCTION`], which asks Regia's CLDR table ([`crate::plural`]) instead of the runtime's
/// bundled rules (`intl_pluralrules` 7.0.2 drops the `n % 100` of Arabic's `few` and `many`, so
/// 103 read as `other`; ADR-1466). Exact-number variants (`[0]`) still match first: they stay on
/// an outer select whose default holds the routed one. `None` when the source has no plural select
/// or does not parse.
fn route_plurals(source: &str) -> Option<String> {
    use fluent_syntax::ast;
    fn is_category(key: &ast::VariantKey<String>) -> bool {
        matches!(key, ast::VariantKey::Identifier { name } if name != "other" && crate::plural::PluralCategory::parse(name).is_some())
    }
    fn pattern(p: &mut ast::Pattern<String>, changed: &mut bool) {
        for element in &mut p.elements {
            if let ast::PatternElement::Placeable { expression } = element {
                expression_routed(expression, changed);
            }
        }
    }
    fn expression_routed(e: &mut ast::Expression<String>, changed: &mut bool) {
        match e {
            ast::Expression::Inline(ast::InlineExpression::Placeable { expression }) => {
                expression_routed(expression, changed);
            }
            ast::Expression::Inline(_) => {}
            ast::Expression::Select { selector, variants } => {
                for variant in variants.iter_mut() {
                    pattern(&mut variant.value, changed);
                }
                let routed = matches!(selector, ast::InlineExpression::FunctionReference { id, .. } if id.name == PLURAL_FUNCTION);
                let numeric_default = variants
                    .iter()
                    .any(|v| v.default && matches!(v.key, ast::VariantKey::NumberLiteral { .. }));
                if routed || numeric_default || !variants.iter().any(|v| is_category(&v.key)) {
                    return;
                }
                *changed = true;
                let plural_selector = ast::InlineExpression::FunctionReference {
                    id: ast::Identifier {
                        name: PLURAL_FUNCTION.to_owned(),
                    },
                    arguments: ast::CallArguments {
                        positional: vec![selector.clone()],
                        named: Vec::new(),
                    },
                };
                let (numeric, named): (Vec<_>, Vec<_>) = std::mem::take(variants)
                    .into_iter()
                    .partition(|v| matches!(v.key, ast::VariantKey::NumberLiteral { .. }));
                if numeric.is_empty() {
                    *selector = plural_selector;
                    *variants = named;
                    return;
                }
                let inner = ast::Expression::Select {
                    selector: plural_selector,
                    variants: named,
                };
                // Exact-number keys compare with the value itself, not `NUMBER()`'s formatted result.
                let raw = match &*selector {
                    ast::InlineExpression::FunctionReference { id, arguments }
                        if id.name == crate::loc_format::NUMBER_FUNCTION =>
                    {
                        arguments.positional.first().cloned()
                    }
                    _ => None,
                };
                if let Some(raw) = raw {
                    *selector = raw;
                }
                *variants = numeric;
                variants.push(ast::Variant {
                    key: ast::VariantKey::Identifier {
                        name: "other".to_owned(),
                    },
                    value: ast::Pattern {
                        elements: vec![ast::PatternElement::Placeable { expression: inner }],
                    },
                    default: true,
                });
            }
        }
    }
    let mut resource = fluent_syntax::parser::parse(source.to_owned()).ok()?;
    let mut changed = false;
    for entry in &mut resource.body {
        match entry {
            ast::Entry::Message(message) => {
                if let Some(value) = &mut message.value {
                    pattern(value, &mut changed);
                }
                for attribute in &mut message.attributes {
                    pattern(&mut attribute.value, &mut changed);
                }
            }
            ast::Entry::Term(term) => {
                pattern(&mut term.value, &mut changed);
                for attribute in &mut term.attributes {
                    pattern(&mut attribute.value, &mut changed);
                }
            }
            _ => {}
        }
    }
    if !changed {
        return None;
    }
    let out = fluent_syntax::serializer::serialize(&resource);
    fluent_syntax::parser::parse(out.as_str()).is_ok().then_some(out)
}

/// `Some(n)` when `text` is a finite number written the way Rust would print it back.
fn canonical_number(text: &str) -> Option<f64> {
    let n: f64 = text.parse().ok()?;
    (n.is_finite() && n.to_string() == text).then_some(n)
}

/// `Startup`: loads `assets://localization/<locale>/*.ftl` for every locale directory found and
/// selects the system locale when present (project fallback otherwise).
///
/// `assets://localization/localization.ron`, when present, names the source locale (the last
/// fallback) and extra fallback edges; a file that does not parse is logged and ignored.
pub fn load_localization(world: &mut World) {
    let loaded = match read_bundles(world) {
        Some(Ok(bundled)) => Some(bundled),
        Some(Err(err)) => {
            tracing::error!(target: "regia_ui", "localization: {err}");
            None
        }
        None => {
            let sources = read_ftl_sources(world);
            if sources.is_empty() {
                tracing::debug!(target: "regia_ui", "no localization files; texts use literal strings");
                return;
            }
            let config = read_config(world);
            // One bad locale is logged and left out; the others still load (levels audit H2).
            let (mut loc, errors) = Localization::new_lenient(sources, Some(config.source.clone()));
            for err in &errors {
                tracing::error!(target: "regia_ui", "{err}; the locale is left out");
            }
            loc.set_fallbacks(config.fallbacks);
            (!loc.available.is_empty()).then_some(loc)
        }
    };
    match loaded {
        Some(mut loc) => {
            loc.generation = LocalizationGeneration::default();
            // Flag, environment, saved settings, Steam, OS, then the project source (ADR-1618).
            let hints = world
                .get_resource::<regia_platform::LocaleHints>()
                .cloned()
                .unwrap_or_default();
            let startup = crate::locale_startup::choose_startup_locale(&mut loc, &hints);
            tracing::info!(
                target: "regia_ui",
                "localization: {} locale(s) {:?}, selected {} (from {}: {:?})",
                loc.available.len(),
                loc.available,
                loc.selected,
                startup.source.name(),
                startup.raw
            );
            world.insert_resource(startup);
            world.insert_resource(loc);
        }
        None => tracing::error!(target: "regia_ui", "localization: no locale loaded"),
    }
}

/// The shipping bundles of a packaged build (`assets://loc/index.ron`), `None` when there is no index
/// (development reads the `.ftl` files). An index that does not parse is an error, not a silent
/// fall back to files a package does not contain.
fn read_bundles(world: &World) -> Option<Result<Localization, UiError>> {
    let vfs = world.get_resource::<regia_platform::Vfs>()?;
    let path = regia_platform::VfsPath::parse(&format!("assets://{BUNDLE_INDEX_PATH}")).ok()?;
    let bytes = vfs.read_sync(&path).ok()?;
    let index = match crate::loc_bundle::BundleIndex::from_ron(&String::from_utf8_lossy(&bytes)) {
        Ok(index) => index,
        Err(err) => return Some(Err(UiError::Localization(format!("{BUNDLE_INDEX_PATH}: {err}")))),
    };
    Some(Localization::load_bundles(&index, |file| {
        let path = regia_platform::VfsPath::parse(&format!("assets://{file}")).map_err(|e| e.to_string())?;
        vfs.read_sync(&path)
            .map(|b| b.to_vec())
            .map_err(|e| e.to_string())
    }))
}

/// `assets://localization/localization.ron`, or the defaults.
fn read_config(world: &World) -> crate::loc_config::LocalizationConfig {
    let default = crate::loc_config::LocalizationConfig::default;
    let Some(vfs) = world.get_resource::<regia_platform::Vfs>() else {
        return default();
    };
    let Ok(path) = regia_platform::VfsPath::parse("assets://localization/localization.ron") else {
        return default();
    };
    let Ok(bytes) = vfs.read_sync(&path) else {
        return default();
    };
    match crate::loc_config::LocalizationConfig::from_ron(&String::from_utf8_lossy(&bytes)) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(target: "regia_ui", "{err}; using the default fallbacks");
            default()
        }
    }
}

/// Appends one `.ftl` file to a locale's combined source (levels audit H1/H2): a file that does not
/// parse on its own is logged and left out, so it cannot take its neighbors with it, and every file
/// ends with a newline, so a file without a trailing newline cannot merge into the next file's first
/// message.
fn append_ftl(combined: &mut String, name: &str, text: &str) {
    if let Err((_, errs)) = FluentResource::try_new(text.to_owned()) {
        let first = errs.first().map(ToString::to_string).unwrap_or_default();
        tracing::error!(
            target: "regia_ui",
            "{name}: {} ftl parse error(s), first: {first}; the file is left out",
            errs.len()
        );
        return;
    }
    combined.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        combined.push('\n');
    }
}

/// Reads all `*.ftl` files under `assets://localization/<locale>/`.
fn read_ftl_sources(world: &World) -> Vec<(String, String)> {
    let Some(vfs) = world.get_resource::<regia_platform::Vfs>() else {
        return Vec::new();
    };
    let Ok(dir) = regia_platform::VfsPath::parse("assets://localization") else {
        return Vec::new();
    };
    let Ok(entries) = vfs.list_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for locale in entries.iter().filter(|e| e.is_dir) {
        let locale_dir = regia_platform::VfsPath::parse(&format!("assets://localization/{}", locale.name));
        let Ok(locale_dir) = locale_dir else {
            continue;
        };
        let mut combined = String::new();
        if let Ok(files) = vfs.list_dir(&locale_dir) {
            let mut names: Vec<&String> = files
                .iter()
                .map(|e| &e.name)
                .filter(|n| n.ends_with(".ftl"))
                .collect();
            names.sort();
            for name in names {
                let path = regia_platform::VfsPath::parse(&format!(
                    "assets://localization/{}/{}",
                    locale.name, name
                ));
                let Ok(path) = path else {
                    continue;
                };
                match vfs.read_sync(&path) {
                    Ok(bytes) => {
                        append_ftl(&mut combined, &path.to_string(), &String::from_utf8_lossy(&bytes))
                    }
                    Err(err) => {
                        tracing::warn!(target: "regia_ui", "cannot read {path}: {err}")
                    }
                }
            }
        }
        if !combined.is_empty() {
            out.push((locale.name.clone(), combined));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: &str = "menu-start = Start Game\nlang = Language: English\n";
    const AR: &str = "menu-start = ابدأ اللعب\nlang = اللغة: العربية\n";
    const JA: &str = "menu-start = ゲームを開始\nlang = 言語: 日本語\n";

    fn loc() -> Localization {
        Localization::new(
            vec![
                ("en".to_owned(), EN.to_owned()),
                ("ar".to_owned(), AR.to_owned()),
                ("ja".to_owned(), JA.to_owned()),
            ],
            Some("en".to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn missing_fallback_selects_the_first_sorted_locale() {
        for _ in 0..16 {
            let l = Localization::new(
                vec![
                    ("ja".to_owned(), JA.to_owned()),
                    ("fr".to_owned(), EN.to_owned()),
                    ("ar".to_owned(), AR.to_owned()),
                ],
                Some("en".to_owned()),
            )
            .unwrap();
            assert_eq!(l.locale(), "ar");
        }
    }

    #[test]
    fn resolves_selected_and_fallback() {
        let mut l = loc();
        assert_eq!(l.resolve("menu-start", &[]), "Start Game");
        assert!(l.set_locale("ja"));
        assert_eq!(l.resolve("menu-start", &[]), "ゲームを開始");
        assert_eq!(l.generation.0, 1);
        assert!(l.set_locale("ar"));
        assert_eq!(l.resolve("menu-start", &[]), "ابدأ اللعب");
    }

    #[test]
    fn missing_key_shows_marker() {
        let l = loc();
        assert_eq!(l.resolve("nope", &[]), "⟦nope⟧");
        assert!(!l.has("nope"));
        assert!(l.has("menu-start"));
    }

    #[test]
    fn key_display_shows_keys_and_wins_over_pseudo() {
        let mut l = loc();
        let args = vec![("n".to_owned(), "3".to_owned())];
        assert!(l.set_key_display(true));
        assert!(!l.set_key_display(true), "no change the second time");
        assert_eq!(l.resolve("menu-start", &[]), "\u{27EA}menu-start\u{27EB}");
        assert_eq!(
            l.resolve("menu-start", &args),
            crate::loc_pseudo::key_display("menu-start", &args)
        );
        assert_eq!(
            l.resolve("nope", &[]),
            "\u{27EA}nope\u{27EB}",
            "missing keys show too"
        );
        l.set_pseudo(true);
        assert_eq!(
            l.resolve("menu-start", &[]),
            "\u{27EA}menu-start\u{27EB}",
            "key display wins"
        );
        let generation = l.generation.0;
        assert!(l.set_key_display(false));
        assert_eq!(l.generation.0, generation + 1);
        assert!(!l.key_display());
        assert_ne!(l.resolve("menu-start", &[]), "\u{27EA}menu-start\u{27EB}");
    }

    #[test]
    fn cycle_wraps() {
        let mut l = loc();
        // `available` is sorted: ar, en, ja. Starting at en → ja → ar → en.
        assert_eq!(l.cycle_locale(), "ja");
        assert_eq!(l.cycle_locale(), "ar");
        assert_eq!(l.cycle_locale(), "en");
    }

    #[test]
    fn unknown_locale_rejected() {
        let mut l = loc();
        assert!(!l.set_locale("zz"));
        assert_eq!(l.locale(), "en");
        assert_eq!(l.generation.0, 0);
    }

    #[test]
    fn fluent_args() {
        let l = Localization::new(
            vec![("en".to_owned(), "score = Score: { $points }".to_owned())],
            Some("en".to_owned()),
        )
        .unwrap();
        assert_eq!(
            l.resolve("score", &[("points".to_owned(), "42".to_owned())]),
            "Score: 42"
        );
    }

    #[test]
    fn parse_errors_surface() {
        // Invalid locale tag is a hard error even when the FTL body is fine.
        let err = Localization::new(
            vec![("not a locale!!!".to_owned(), "ok = yes".to_owned())],
            Some("en".to_owned()),
        )
        .map(|_| ())
        .unwrap_err();
        assert!(err.to_string().contains("locale"), "{err}");
    }

    /// A host that replaces the selected locale's texts sees them on screen: shaped text is keyed
    /// by the generation, so the add must move it (a failed add must not).
    #[test]
    fn add_locale_moves_the_generation_so_texts_reshape() {
        let mut loc = Localization::new(
            vec![("de".to_owned(), "menu-quit = Beenden\n".to_owned())],
            Some("de".to_owned()),
        )
        .expect("loads");
        let before = loc.generation.0;
        loc.add_locale("de".to_owned(), "menu-quit = Verlassen\n".to_owned())
            .expect("adds");
        assert_eq!(loc.resolve("menu-quit", &[]), "Verlassen");
        assert!(loc.generation.0 > before, "texts on screen keep the old wording");
        let after = loc.generation.0;
        loc.add_locale("de".to_owned(), "broken = { $x".to_owned())
            .expect_err("bad ftl");
        assert_eq!(loc.generation.0, after);
    }

    #[test]
    fn a_failed_add_locale_keeps_the_previous_source() {
        let mut loc = Localization::new(
            vec![("de".to_owned(), "menu-quit = Beenden\n".to_owned())],
            Some("de".to_owned()),
        )
        .expect("loads");
        let err = loc
            .add_locale("de".to_owned(), "broken = { $x".to_owned())
            .expect_err("bad ftl");
        assert!(err.to_string().contains("ftl parse errors"), "{err}");
        assert_eq!(loc.source_text("de"), Some("menu-quit = Beenden\n"));
        assert_eq!(loc.resolve("menu-quit", &[]), "Beenden");
    }

    const EN_PLURAL: &str =
        "items = { $count ->\n    [0] No items\n    [one] One item\n   *[other] { $count } items\n}\n";
    const AR_PLURAL: &str = "items = { $count ->\n    [zero] لا عناصر\n    [one] عنصر\n    [two] عنصران\n    [few] { $count } عناصر\n    [many] { $count } عنصرا\n   *[other] { $count } عنصر\n}\n";
    const JA_PLURAL: &str = "items = { $count }個\n";

    fn plural_loc() -> Localization {
        Localization::new(
            vec![
                ("en".to_owned(), EN_PLURAL.to_owned()),
                ("ar".to_owned(), AR_PLURAL.to_owned()),
                ("ja".to_owned(), JA_PLURAL.to_owned()),
            ],
            Some("en".to_owned()),
        )
        .unwrap()
    }

    fn count(n: &str) -> Vec<(String, String)> {
        vec![("count".to_owned(), n.to_owned())]
    }

    #[test]
    fn english_plurals() {
        let l = plural_loc();
        assert_eq!(l.resolve("items", &count("0")), "No items");
        assert_eq!(l.resolve("items", &count("1")), "One item");
        assert_eq!(l.resolve("items", &count("2")), "2 items");
        assert_eq!(l.resolve("items", &count("11")), "11 items");
    }

    #[test]
    fn arabic_plural_categories() {
        let mut l = plural_loc();
        assert!(l.set_locale("ar"));
        assert_eq!(l.resolve("items", &count("0")), "لا عناصر");
        assert_eq!(l.resolve("items", &count("1")), "عنصر");
        assert_eq!(l.resolve("items", &count("2")), "عنصران");
        assert_eq!(l.resolve("items", &count("3")), "3 عناصر");
        assert_eq!(l.resolve("items", &count("11")), "11 عنصرا");
        assert_eq!(l.resolve("items", &count("100")), "100 عنصر");
    }

    #[test]
    fn japanese_has_one_form() {
        let mut l = plural_loc();
        assert!(l.set_locale("ja"));
        assert_eq!(l.resolve("items", &count("1")), "1個");
        assert_eq!(l.resolve("items", &count("5")), "5個");
    }

    /// ADR-3359: `NUMBER(…, type: "ordinal")` selects through the CLDR ordinal table.
    #[test]
    fn number_type_ordinal_selects_ordinal_categories() {
        let l = Localization::new(
            vec![(
                "en".to_owned(),
                "place = { NUMBER($count, type: \"ordinal\") ->\n    [one] { $count }st\n    [two] { $count }nd\n    [few] { $count }rd\n   *[other] { $count }th\n}\n\
                 bad = { NUMBER($count, type: \"superlative\") }\n".to_owned(),
            )],
            Some("en".to_owned()),
        )
        .unwrap();
        for (n, want) in [
            ("1", "1st"),
            ("2", "2nd"),
            ("3", "3rd"),
            ("4", "4th"),
            ("11", "11th"),
            ("12", "12th"),
            ("13", "13th"),
            ("21", "21st"),
            ("22", "22nd"),
            ("23", "23rd"),
            ("101", "101st"),
        ] {
            assert_eq!(l.resolve("place", &count(n)), want, "en {n}");
        }
        // A bad `type` value shows NUMBER() (the error value), never a panic.
        assert_eq!(l.resolve("bad", &count("1")), "NUMBER()");
    }

    #[test]
    fn non_canonical_numbers_stay_text() {
        assert_eq!(canonical_number("42"), Some(42.0));
        assert_eq!(canonical_number("-1.5"), Some(-1.5));
        assert_eq!(canonical_number("007"), None);
        assert_eq!(canonical_number("1e3"), None);
        assert_eq!(canonical_number("NaN"), None);
        assert_eq!(canonical_number(""), None);
        let l = Localization::new(
            vec![("en".to_owned(), "code = Code { $id }".to_owned())],
            Some("en".to_owned()),
        )
        .unwrap();
        assert_eq!(
            l.resolve("code", &[("id".to_owned(), "007".to_owned())]),
            "Code 007"
        );
    }

    #[test]
    fn resolve_in_has_no_fallback() {
        let l = Localization::new(
            vec![
                ("en".to_owned(), "a = A\nb = B\n".to_owned()),
                ("ja".to_owned(), "a = エー\n".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .unwrap();
        assert_eq!(l.resolve_in("ja", "a", &[]).as_deref(), Some("エー"));
        assert_eq!(l.resolve_in("ja", "b", &[]), None);
        assert_eq!(l.resolve_in("zz", "a", &[]), None);
        assert!(l.has_in("en", "b"));
        assert!(!l.has_in("ja", "b"));
        // The selected-locale path still falls back to English.
        let mut l = l;
        assert!(l.set_locale("ja"));
        assert_eq!(l.resolve("b", &[]), "B");
    }

    /// The whole start-up path: files under `assets://localization`, hints injected, the
    /// `LocaleStartup` record inserted next to the `Localization`.
    #[test]
    fn load_localization_uses_the_hints_and_records_the_winner() {
        let root = regia_test_utils::unique_temp_dir("loc-startup");
        for (locale, text) in [
            ("en", "hello = Hello\n"),
            ("de", "hello = Hallo\n"),
            ("ja", "hello = こんにちは\n"),
        ] {
            let dir = root.join("localization").join(locale);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("main.ftl"), text).expect("write");
        }
        let run = |hints: regia_platform::LocaleHints| {
            let mut world = World::new();
            let mut vfs = regia_platform::Vfs::new();
            vfs.mount(
                regia_platform::MountPoint::Assets,
                regia_platform::DirMount::read_only(&root),
            );
            world.insert_resource(vfs);
            world.insert_resource(hints);
            load_localization(&mut world);
            let selected = world.resource::<Localization>().locale().to_owned();
            let start = world.resource::<crate::locale_startup::LocaleStartup>().clone();
            (selected, start)
        };
        let hints = regia_platform::LocaleHints {
            settings: Some("de".into()),
            os: Some("ja_JP.UTF-8".into()),
            ..regia_platform::LocaleHints::isolated()
        };
        let (selected, start) = run(hints.clone());
        assert_eq!(selected, "de");
        assert_eq!(start.source, crate::locale_startup::LocaleSource::Settings);
        let (selected, start) = run(regia_platform::LocaleHints {
            settings: None,
            ..hints
        });
        assert_eq!(selected, "ja");
        assert_eq!(start.source, crate::locale_startup::LocaleSource::Os);
        assert_eq!(start.raw, "ja_JP.UTF-8");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn load_from(tag: &str, files: &[(&str, &str, &str)]) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        for (locale, name, text) in files {
            let dir = root.join("localization").join(locale);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join(name), text).expect("write");
        }
        let mut world = World::new();
        let mut vfs = regia_platform::Vfs::new();
        vfs.mount(
            regia_platform::MountPoint::Assets,
            regia_platform::DirMount::read_only(&root),
        );
        world.insert_resource(vfs);
        world.insert_resource(regia_platform::LocaleHints::isolated());
        load_localization(&mut world);
        (world, root)
    }

    /// Levels audit H1: a file without a trailing newline does not swallow the next file's first
    /// message.
    #[test]
    fn ftl_files_without_a_trailing_newline_stay_separate() {
        let (world, root) = load_from(
            "loc-newline",
            &[
                ("en", "a.ftl", "menu-start = Start"),
                ("en", "b.ftl", "menu-quit = Quit\n"),
            ],
        );
        let loc = world.resource::<Localization>();
        assert_eq!(loc.resolve_in("en", "menu-start", &[]).as_deref(), Some("Start"));
        assert_eq!(loc.resolve_in("en", "menu-quit", &[]).as_deref(), Some("Quit"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Levels audit H2: one unparseable file drops only itself, and a locale that cannot load at all
    /// (a bad tag) drops only that locale.
    #[test]
    fn one_bad_locale_file_does_not_drop_the_rest() {
        let (world, root) = load_from(
            "loc-bad-file",
            &[
                ("en", "main.ftl", "hello = Hello\n"),
                ("de", "bad.ftl", "broken = { $x\n"),
                ("de", "main.ftl", "hello = Hallo\n"),
                ("notalanguagetag", "main.ftl", "hello = ?\n"),
            ],
        );
        let loc = world.resource::<Localization>();
        assert_eq!(loc.resolve_in("de", "hello", &[]).as_deref(), Some("Hallo"));
        assert_eq!(loc.resolve_in("en", "hello", &[]).as_deref(), Some("Hello"));
        assert!(!loc.has_in("de", "broken"));
        assert_eq!(loc.available, vec!["de".to_string(), "en".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
        // The strict constructor still refuses the bad tag; the lenient one reports and skips it.
        let sources = vec![
            ("en".to_string(), "hello = Hello\n".to_string()),
            ("notalanguagetag".to_string(), "hello = ?\n".to_string()),
        ];
        assert!(Localization::new(sources.clone(), None).is_err());
        let (loc, errors) = Localization::new_lenient(sources, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(loc.available, vec!["en".to_string()]);
        assert!(loc.sources.keys().all(|k| k == "en"));
    }
    #[test]
    fn audio_follows_text_until_set_then_splits_and_clearing_re_follows() {
        let mut l = loc();
        assert_eq!(l.audio_locale(), "en");
        assert!(l.audio_override().is_none());
        assert!(l.set_locale("ja"));
        assert_eq!(l.audio_locale(), "ja", "audio follows the text locale");
        let g = l.audio_generation();
        assert_eq!(g, 1, "a text switch moves the followed audio locale");
        assert!(l.set_audio_locale(Some("fr_CA.UTF-8")).expect("tag"));
        assert_eq!(l.audio_locale(), "fr-CA");
        assert_eq!(
            l.audio_chain(),
            ["fr-CA", "fr", "en"],
            "audio keeps tags without text"
        );
        assert!(l.set_locale("ar"));
        assert_eq!(l.audio_locale(), "fr-CA", "split: text moves alone");
        assert_eq!(l.audio_generation(), g + 1);
        assert!(!l.set_audio_locale(Some("fr-CA")).expect("same"));
        assert!(l.set_audio_locale(Some("not a tag!")).is_err());
        assert_eq!(l.audio_locale(), "fr-CA", "a bad tag changes nothing");
        assert!(l.set_audio_locale(None).expect("clear"));
        assert_eq!(l.audio_locale(), "ar", "cleared: follows the text again");
        assert_eq!(l.audio_chain(), l.chain("ar"));
        // Setting the override to the current text locale is not an effective change.
        assert!(!l.set_audio_locale(Some("ar")).expect("same tag"));
        assert_eq!(l.audio_override(), Some("ar"));
    }

    #[test]
    fn audio_locale_changed_message_is_sent_for_effective_changes_only() {
        let mut app = App::new();
        app.add_plugins(LocalizationPlugin);
        app.insert_resource(loc());
        app.update();
        let read = |app: &App| -> Vec<AudioLocaleChanged> {
            let messages = app.world().resource::<Messages<AudioLocaleChanged>>();
            messages.iter_current_update_messages().cloned().collect()
        };
        app.world_mut()
            .resource_mut::<Localization>()
            .set_audio_locale(Some("ja"))
            .expect("tag");
        app.update();
        let got = read(&app);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].from.as_str(), got[0].to.as_str()), ("en", "ja"));
        app.world_mut().resource_mut::<Localization>().set_locale("ar");
        app.update();
        assert!(
            read(&app).is_empty(),
            "the text moved, the audio override did not"
        );
        app.world_mut()
            .resource_mut::<Localization>()
            .set_audio_locale(None)
            .expect("clear");
        app.update();
        let got = read(&app);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].to, "ar");
        // While the audio follows the text, a text change moves the audio too and says so
        // (ADR-1882 (e); lane spec-15-35).
        app.world_mut().resource_mut::<Localization>().set_locale("en");
        app.update();
        let got = read(&app);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].from.as_str(), got[0].to.as_str()), ("ar", "en"));
        assert!(got[0].generation > 0);
    }

    /// Spec 36 § 6: the plugin keeps `LocaleChains` (what localized asset resolution reads) in step
    /// with the text and audio chains after a switch (lane spec-15-35, ADR-5778). The asset server
    /// side (re-resolve and `LocalizedAssetChanged`) is tested in `regia_asset`.
    #[test]
    fn locale_chains_follow_text_and_audio_switches() {
        use regia_asset::localized::LocaleChains;
        let mut app = App::new();
        app.add_plugins(LocalizationPlugin);
        app.insert_resource(loc());
        app.update();
        let chains = |app: &App| app.world().resource::<LocaleChains>().clone();
        let first = chains(&app);
        assert_eq!(first.text.first().map(String::as_str), Some("en"));
        assert_eq!(first.audio, first.text, "audio follows the text");
        app.world_mut().resource_mut::<Localization>().set_locale("ja");
        app.update();
        let moved = chains(&app);
        assert_eq!(moved.text.first().map(String::as_str), Some("ja"));
        assert_eq!(moved.audio.first().map(String::as_str), Some("ja"));
        app.world_mut()
            .resource_mut::<Localization>()
            .set_audio_locale(Some("ar"))
            .expect("tag");
        app.update();
        let split = chains(&app);
        assert_eq!(split.text.first().map(String::as_str), Some("ja"));
        assert_eq!(split.audio.first().map(String::as_str), Some("ar"), "{split:?}");
    }

    fn bundle_index(files: &[(&str, &str)]) -> (crate::loc_bundle::BundleIndex, BTreeMap<String, Vec<u8>>) {
        let mut bytes = BTreeMap::new();
        let mut locales = Vec::new();
        for (tag, text) in files {
            let file = format!("loc/{tag}.ftl");
            locales.push(crate::loc_bundle::BundleLocale {
                tag: (*tag).to_owned(),
                file: file.clone(),
                messages: 0,
                hash: regia_asset::content_hash_hex(text.as_bytes()),
            });
            bytes.insert(file, text.as_bytes().to_vec());
        }
        let index = crate::loc_bundle::BundleIndex {
            format_version: crate::loc_bundle::BUNDLE_INDEX_VERSION,
            source: "en".into(),
            locales,
            fallbacks: BTreeMap::from([("fr-CA".to_owned(), vec!["fr".to_owned()])]),
        };
        (index, bytes)
    }

    #[test]
    fn bundles_load_through_the_reader_and_a_placeholder_only_key_shows_the_source() {
        // A normalized fr bundle omits the placeholder key, so the source text shows via fallback.
        let fr = crate::loc_bundle::normalize_locale(&[(
            "fr/gathered.ftl".to_owned(),
            "# TODO translate\nquit = Quit\nstart = Commencer\n".to_owned(),
        )]);
        let (index, bytes) =
            bundle_index(&[("en", "quit = Quit\nstart = Start\n"), ("fr", fr.text.as_str())]);
        let mut l = Localization::load_bundles(&index, |file| {
            bytes.get(file).cloned().ok_or_else(|| format!("{file} absent"))
        })
        .expect("bundles");
        assert_eq!(l.origin(), LocalizationOrigin::Bundle);
        assert_eq!(l.origin().as_str(), "bundle");
        assert_eq!(l.fallbacks().get("fr-CA").map(Vec::len), Some(1));
        assert!(l.set_locale("fr"));
        assert_eq!(l.resolve("start", &[]), "Commencer");
        assert_eq!(
            l.resolve("quit", &[]),
            "Quit",
            "placeholder omitted: the source shows"
        );
        assert_eq!(l.provider_of("quit").as_deref(), Some("en"));
        assert_eq!(Localization::default().origin().as_str(), "files");
    }

    #[test]
    fn a_normalized_bundle_resolves_a_term_reference() {
        let en = crate::loc_bundle::normalize_locale(&[(
            "en/main.ftl".to_owned(),
            "-brand = Regia\nhello = Hello { -brand }\n".to_owned(),
        )]);
        assert!(en.text.contains("-brand = Regia"), "{}", en.text);
        let (index, bytes) = bundle_index(&[("en", en.text.as_str())]);
        let l = Localization::load_bundles(&index, |file| {
            bytes.get(file).cloned().ok_or_else(|| format!("{file} absent"))
        })
        .expect("bundles");
        assert_eq!(l.resolve("hello", &[]), "Hello Regia");
    }

    #[test]
    fn a_bundle_with_a_wrong_hash_or_a_missing_file_is_an_error() {
        let (mut index, bytes) = bundle_index(&[("en", "a = A\n")]);
        let reader = |file: &str| bytes.get(file).cloned().ok_or_else(|| format!("{file} absent"));
        index.locales[0].hash = "00".into();
        let err = Localization::load_bundles(&index, reader)
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        index.locales[0].file = "loc/zz.ftl".into();
        let err = Localization::load_bundles(&index, reader)
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("cannot read bundle"), "{err}");
    }

    /// A packaged layout (`loc/index.ron` + bundles, no `localization/` folder) loads as bundles.
    #[test]
    fn load_localization_prefers_the_bundle_index() {
        let root = regia_test_utils::unique_temp_dir("loc-bundled");
        let (index, bytes) = bundle_index(&[("en", "hello = Hello\n"), ("de", "hello = Hallo\n")]);
        std::fs::create_dir_all(root.join("loc")).expect("mkdir");
        for (file, data) in &bytes {
            std::fs::write(root.join(file), data).expect("write");
        }
        std::fs::write(root.join(BUNDLE_INDEX_PATH), index.to_ron().expect("ron")).expect("write");
        // A stray dev folder is ignored when the index exists.
        std::fs::create_dir_all(root.join("localization/xx")).expect("mkdir");
        std::fs::write(root.join("localization/xx/main.ftl"), "hello = X\n").expect("write");
        let mut world = World::new();
        let mut vfs = regia_platform::Vfs::new();
        vfs.mount(
            regia_platform::MountPoint::Assets,
            regia_platform::DirMount::read_only(&root),
        );
        world.insert_resource(vfs);
        world.insert_resource(regia_platform::LocaleHints {
            settings: Some("de".into()),
            ..regia_platform::LocaleHints::isolated()
        });
        load_localization(&mut world);
        let l = world.resource::<Localization>();
        assert_eq!(l.origin(), LocalizationOrigin::Bundle);
        assert_eq!(l.available, ["de", "en"]);
        assert_eq!(l.resolve("hello", &[]), "Hallo");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Spec 36 § 3 import step 6: a locale's files merge in name order and a later file wins, so a
    /// hand-written `main.ftl` overrides the generated `imported.ftl` at run time (lane
    /// spec-15-35, ADR-5778).
    #[test]
    fn a_hand_written_file_wins_over_imported_ftl() {
        let root = regia_test_utils::unique_temp_dir("loc-imported-order");
        for (file, text) in [
            ("localization/en/main.ftl", "hello = Hello\nbye = Bye\n"),
            (
                "localization/fr/imported.ftl",
                "hello = Bonjour (importé)\nbye = Au revoir\n",
            ),
            ("localization/fr/main.ftl", "hello = Bonjour\n"),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, text).expect("write");
        }
        let mut world = World::new();
        let mut vfs = regia_platform::Vfs::new();
        vfs.mount(
            regia_platform::MountPoint::Assets,
            regia_platform::DirMount::read_only(&root),
        );
        world.insert_resource(vfs);
        world.insert_resource(regia_platform::LocaleHints {
            settings: Some("fr".into()),
            ..regia_platform::LocaleHints::isolated()
        });
        load_localization(&mut world);
        let l = world.resource::<Localization>();
        assert_eq!(l.locale(), "fr");
        assert_eq!(
            l.resolve("hello", &[]),
            "Bonjour",
            "main.ftl sorts after imported.ftl and wins"
        );
        assert_eq!(
            l.resolve("bye", &[]),
            "Au revoir",
            "imported entries without a hand-written twin stay"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn coverage_counts_translated_placeholders_and_stale() {
        let en = "a = A\nb = B\nc = C { $n }\nd = D\nonly-attr =\n    .tip = T\n";
        let fr = format!(
            "a = Ah\n# TODO translate\nb = B\n{} {}\nc = Cé {{ $n }}\n",
            crate::loc_po::SOURCE_HASH_PREFIX,
            "0000000000000000"
        );
        let l = Localization::new(
            vec![("en".to_owned(), en.to_owned()), ("fr".to_owned(), fr)],
            Some("en".to_owned()),
        )
        .expect("locales");
        let c = l.coverage("fr").expect("fr");
        assert_eq!(c.messages, 4, "the attribute-only message is not counted");
        assert_eq!(c.translated, 2);
        assert_eq!(c.untranslated, 2);
        assert_eq!(c.placeholders, 1);
        assert_eq!(c.stale, 1, "c's recorded hash is not the source hash");
        assert!((c.percent - 50.0).abs() < 1e-9);
        let own = l.coverage("en").expect("en");
        assert_eq!((own.translated, own.untranslated, own.stale), (4, 0, 0));
        assert!(l.coverage("de").is_none());
    }
}
