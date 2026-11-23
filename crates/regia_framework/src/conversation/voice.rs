// SPDX-License-Identifier: MPL-2.0
//! Voice resolution and line durations (`docs/specs/56-dialogue-and-subtitles.md` §§ 2.2, 2.3).
//!
//! Both are pure functions: the caller passes the fallback chain and the lookups, so the same input
//! always gives the same answer.

use regia_ui::captions::grapheme_count;

use super::asset::{ConversationLine, ConversationSettings, DurationSource, VoiceFallback, VoiceSpec};

/// The answer of [`resolve_voice`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoiceResolution {
    /// The clip path, if one was found.
    pub clip: Option<String>,
    /// The locale the clip belongs to.
    pub locale: Option<String>,
    /// True when the clip's locale differs from the requested one.
    pub fallback: bool,
}

/// What [`resolve_voice`] needs besides the line.
pub struct VoiceContext<'a> {
    /// The conversation id.
    pub conversation: &'a str,
    /// The zero-based ordinal of the line in file order (`{index}` in the template).
    pub index: usize,
    /// Whether the line's speaker is voiced.
    pub speaker_voiced: bool,
    /// The project template (`voice_path`), or `None` to use explicit paths only.
    pub template: Option<&'a str>,
    /// What to do when only a fallback locale has a clip.
    pub fallback_policy: VoiceFallback,
    /// The fallback chain of the requested locale, starting with the locale itself.
    pub chain: &'a [String],
    /// Whether a clip asset exists.
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// Substitutes `{locale}`, `{conversation}`, `{line}`, `{speaker}` and `{index}` (zero-padded to
/// three digits) in a clip path template.
pub fn substitute_template(
    template: &str,
    locale: &str,
    conversation: &str,
    line: &str,
    speaker: &str,
    index: usize,
) -> String {
    template
        .replace("{locale}", locale)
        .replace("{conversation}", conversation)
        .replace("{line}", line)
        .replace("{speaker}", speaker)
        .replace("{index}", &format!("{index:03}"))
}

/// The clip a locale would carry for a line, ignoring existence: the explicit path if the line has
/// one for that locale, else the template substitution.
pub fn clip_path_for(line: &ConversationLine, ctx: &VoiceContext<'_>, locale: &str) -> Option<String> {
    if let VoiceSpec::Explicit(map) = &line.voice
        && let Some(path) = map.get(locale)
    {
        return Some(path.clone());
    }
    ctx.template
        .map(|t| substitute_template(t, locale, ctx.conversation, &line.id, &line.speaker, ctx.index))
}

/// Resolves the clip of `line` for `locale`: walks the chain and stops at the first locale with a
/// clip (explicit path, else the template when that asset exists).
pub fn resolve_voice(line: &ConversationLine, ctx: &VoiceContext<'_>, locale: &str) -> VoiceResolution {
    if !ctx.speaker_voiced || line.voice == VoiceSpec::None {
        return VoiceResolution::default();
    }
    let own = [locale.to_string()];
    let chain: &[String] = if ctx.chain.is_empty() { &own } else { ctx.chain };
    for candidate in chain {
        let explicit = matches!(&line.voice, VoiceSpec::Explicit(map) if map.contains_key(candidate));
        let Some(path) = clip_path_for(line, ctx, candidate) else {
            continue;
        };
        // An explicit path is trusted (a missing file is the validator's finding); a template
        // path counts only when the asset exists.
        if explicit || (ctx.exists)(&path) {
            let fallback = candidate != locale;
            if fallback && ctx.fallback_policy == VoiceFallback::Silent {
                return VoiceResolution {
                    clip: None,
                    locale: Some(candidate.clone()),
                    fallback: true,
                };
            }
            return VoiceResolution {
                clip: Some(path),
                locale: Some(candidate.clone()),
                fallback,
            };
        }
    }
    VoiceResolution::default()
}

/// How a line's planned length was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationBasis {
    /// The clip length.
    Clip,
    /// The authored length.
    Fixed,
    /// The text estimate.
    Estimate,
}

/// The planned length of a line in microseconds. `clip_us` is the resolved clip's length (from the
/// cooked table) when a clip resolved and its header could be read; `text` is the line text in the
/// requested locale.
pub fn plan_duration(
    source: DurationSource,
    clip_us: Option<u64>,
    text: &str,
    settings: &ConversationSettings,
) -> (u64, DurationBasis) {
    match source {
        DurationSource::Fixed(ms) => (u64::from(ms) * 1000, DurationBasis::Fixed),
        DurationSource::Clip if clip_us.is_some() => (clip_us.unwrap_or(0), DurationBasis::Clip),
        DurationSource::Clip | DurationSource::Estimate => {
            (estimate_us(text, settings), DurationBasis::Estimate)
        }
    }
}

/// `ceil(graphemes / estimate_cps)` whole seconds, floored at the minimum display time.
pub fn estimate_us(text: &str, settings: &ConversationSettings) -> u64 {
    let graphemes = grapheme_count(text) as u64;
    let seconds = graphemes.div_ceil(u64::from(settings.estimate_cps.max(1)));
    (seconds * 1_000_000).max(u64::from(settings.min_display_ms) * 1000)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn line(voice: VoiceSpec) -> ConversationLine {
        ConversationLine {
            id: "greet".into(),
            speaker: "guard".into(),
            text: "k".into(),
            note: String::new(),
            emotion: None,
            tags: Vec::new(),
            voice,
            duration: DurationSource::Clip,
            cues: Vec::new(),
            priority: None,
            keep_breaks: false,
        }
    }

    fn chain() -> Vec<String> {
        ["pt-BR", "pt-PT", "pt", "en"]
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn ctx<'a>(chain: &'a [String], exists: &'a dyn Fn(&str) -> bool) -> VoiceContext<'a> {
        VoiceContext {
            conversation: "intro",
            index: 4,
            speaker_voiced: true,
            template: Some("vo/{locale}/{conversation}_{line}_{index}.ogg"),
            fallback_policy: VoiceFallback::Source,
            chain,
            exists,
        }
    }

    #[test]
    fn own_clip_wins() {
        let chain = chain();
        let exists = |p: &str| p == "vo/pt-BR/intro_greet_004.ogg" || p == "vo/pt-PT/intro_greet_004.ogg";
        let result = resolve_voice(&line(VoiceSpec::Auto), &ctx(&chain, &exists), "pt-BR");
        assert_eq!(result.clip.as_deref(), Some("vo/pt-BR/intro_greet_004.ogg"));
        assert!(!result.fallback);
    }

    #[test]
    fn a_missing_own_clip_uses_the_next_locale_with_fallback_flag() {
        let chain = chain();
        let exists = |p: &str| p == "vo/pt-PT/intro_greet_004.ogg";
        let result = resolve_voice(&line(VoiceSpec::Auto), &ctx(&chain, &exists), "pt-BR");
        assert_eq!(result.clip.as_deref(), Some("vo/pt-PT/intro_greet_004.ogg"));
        assert_eq!(result.locale.as_deref(), Some("pt-PT"));
        assert!(result.fallback);
    }

    #[test]
    fn silent_policy_yields_no_clip_but_reports_the_fallback() {
        let chain = chain();
        let exists = |p: &str| p == "vo/en/intro_greet_004.ogg";
        let mut c = ctx(&chain, &exists);
        c.fallback_policy = VoiceFallback::Silent;
        let result = resolve_voice(&line(VoiceSpec::Auto), &c, "pt-BR");
        assert_eq!(result.clip, None);
        assert!(result.fallback);
    }

    #[test]
    fn unvoiced_speaker_and_voice_none_never_resolve() {
        let chain = chain();
        let exists = |_: &str| true;
        let mut c = ctx(&chain, &exists);
        assert_eq!(
            resolve_voice(&line(VoiceSpec::None), &c, "pt-BR"),
            VoiceResolution::default()
        );
        c.speaker_voiced = false;
        assert_eq!(
            resolve_voice(&line(VoiceSpec::Auto), &c, "pt-BR"),
            VoiceResolution::default()
        );
    }

    #[test]
    fn explicit_path_wins_over_the_template() {
        let chain = vec!["en".to_string()];
        let exists = |_: &str| true;
        let mut map = BTreeMap::new();
        map.insert("en".to_string(), "audio/vo/en/custom.ogg".to_string());
        let result = resolve_voice(&line(VoiceSpec::Explicit(map)), &ctx(&chain, &exists), "en");
        assert_eq!(result.clip.as_deref(), Some("audio/vo/en/custom.ogg"));
    }

    #[test]
    fn no_clip_anywhere_resolves_to_nothing() {
        let chain = chain();
        let exists = |_: &str| false;
        let result = resolve_voice(&line(VoiceSpec::Auto), &ctx(&chain, &exists), "pt-BR");
        assert_eq!(result, VoiceResolution::default());
    }

    #[test]
    fn durations() {
        let settings = ConversationSettings::default(); // estimate_cps 14, min display 1000 ms
        assert_eq!(
            plan_duration(DurationSource::Clip, Some(2_345_000), "x", &settings),
            (2_345_000, DurationBasis::Clip)
        );
        assert_eq!(
            plan_duration(DurationSource::Fixed(2600), Some(1), "x", &settings),
            (2_600_000, DurationBasis::Fixed)
        );
        // 29 graphemes at 14 per second: ceil(2.07) = 3 s.
        let text = "a".repeat(29);
        assert_eq!(
            plan_duration(DurationSource::Estimate, None, &text, &settings),
            (3_000_000, DurationBasis::Estimate)
        );
        // A missing clip degrades to the estimate; a short text floors at the minimum display.
        assert_eq!(
            plan_duration(DurationSource::Clip, None, "hi", &settings),
            (1_000_000, DurationBasis::Estimate)
        );
    }

    #[test]
    fn template_substitution_pads_the_index() {
        assert_eq!(
            substitute_template(
                "{speaker}/{conversation}/{index}_{line}_{locale}",
                "en",
                "c",
                "l",
                "s",
                7
            ),
            "s/c/007_l_en"
        );
    }
}
