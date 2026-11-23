// SPDX-License-Identifier: MPL-2.0
//! The conversation validator (`docs/specs/56-dialogue-and-subtitles.md` § 6.3).
//!
//! Pure over a [`ValidateEnv`]: the same project always gives the same issues in the same order
//! (conversation, line, code, locale).

use std::collections::{BTreeMap, BTreeSet};

use regia_ui::captions::{
    BlockMetrics, EmMeasure, MIN_CONTRAST, Rgb, grapheme_count, is_cjk_locale, safe_rect, wrap,
};
use serde_json::{Value, json};

use super::asset::{ClipPolicy, ConversationAsset, ConversationSettings, SpeakerSet, Step, VoiceSpec};
use super::voice::{VoiceContext, plan_duration, resolve_voice};

/// How serious an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// For information only.
    Info,
    /// Worth fixing.
    Warning,
    /// Blocks a cook or `--strict` run.
    Error,
}

impl Severity {
    /// The lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The code, `conv_*`.
    pub code: &'static str,
    /// The severity.
    pub severity: Severity,
    /// The conversation id (empty for project-level findings).
    pub conversation: String,
    /// The line or step the issue is about (may be empty).
    pub line: String,
    /// The locale (may be empty).
    pub locale: String,
    /// What is wrong.
    pub message: String,
    /// How to fix it.
    pub fix: String,
}

impl Issue {
    /// The JSON object the agent returns.
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "severity": self.severity.name(),
            "conversation": self.conversation,
            "line": self.line,
            "locale": self.locale,
            "message": self.message,
            "fix": self.fix,
        })
    }
}

/// What the validator asks of the project.
pub trait ValidateEnv {
    /// Whether the locale itself defines the key (no fallback).
    fn has_text_in(&self, locale: &str, key: &str) -> bool;
    /// Text of a key in a locale, falling back along the chain.
    fn text(&self, locale: &str, key: &str) -> Option<String>;
    /// The fallback chain of a locale, starting with itself.
    fn chain(&self, locale: &str) -> Vec<String>;
    /// Whether a clip asset exists.
    fn clip_exists(&self, path: &str) -> bool;
    /// The clip length in microseconds; `None` when the header cannot be read.
    fn clip_duration_us(&self, path: &str) -> Option<u64>;
}

/// What to validate.
pub struct ValidateInput<'a> {
    /// The conversations.
    pub conversations: &'a [ConversationAsset],
    /// The speaker set.
    pub speakers: &'a SpeakerSet,
    /// Project settings.
    pub settings: &'a ConversationSettings,
    /// The locales to check (staged locales).
    pub locales: &'a [String],
    /// The source locale (the first of the chain; text there is authoritative).
    pub source_locale: &'a str,
    /// Promote warnings to errors (`--strict`).
    pub strict: bool,
}

/// The result: issues in a fixed order plus counts per severity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// The issues.
    pub issues: Vec<Issue>,
}

impl Report {
    /// Counts of `(errors, warnings, infos)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |s| self.issues.iter().filter(|i| i.severity == s).count();
        (
            count(Severity::Error),
            count(Severity::Warning),
            count(Severity::Info),
        )
    }

    /// The count per code.
    pub fn by_code(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for issue in &self.issues {
            *out.entry(issue.code).or_insert(0) += 1;
        }
        out
    }

    /// True when any error exists.
    pub fn has_errors(&self) -> bool {
        self.counts().0 > 0
    }

    /// The agent result: `{issues, counts}`.
    pub fn to_json(&self) -> Value {
        let (errors, warnings, infos) = self.counts();
        json!({
            "issues": self.issues.iter().map(Issue::to_json).collect::<Vec<_>>(),
            "counts": {"error": errors, "warning": warnings, "info": infos, "total": self.issues.len()},
        })
    }
}

fn issue(
    code: &'static str,
    severity: Severity,
    conversation: &str,
    line: &str,
    locale: &str,
    message: String,
    fix: &str,
) -> Issue {
    Issue {
        code,
        severity,
        conversation: conversation.to_string(),
        line: line.to_string(),
        locale: locale.to_string(),
        message,
        fix: fix.to_string(),
    }
}

/// Validates the conversations of a project.
pub fn validate(input: &ValidateInput<'_>, env: &dyn ValidateEnv) -> Report {
    let mut issues = Vec::new();
    for (id, speaker) in &input.speakers.speakers {
        if let Some(color) = speaker.color.as_deref().and_then(Rgb::from_hex)
            && color.contrast(Rgb::BLACK) < MIN_CONTRAST
        {
            issues.push(issue(
                "conv_speaker_contrast",
                Severity::Warning,
                "",
                id,
                "",
                format!(
                    "speaker `{id}` color {} is below {MIN_CONTRAST}:1 against the caption background",
                    color.to_hex()
                ),
                "pick a lighter color; the presenter replaces it with the text color",
            ));
        }
    }
    for asset in input.conversations {
        structure(asset, input, &mut issues);
        for locale in input.locales {
            locale_checks(asset, locale, input, env, &mut issues);
        }
    }
    if input.strict {
        for i in &mut issues {
            if i.severity == Severity::Warning {
                i.severity = Severity::Error;
            }
        }
    }
    issues.sort_by(|a, b| {
        (&a.conversation, &a.line, a.code, &a.locale).cmp(&(&b.conversation, &b.line, b.code, &b.locale))
    });
    Report { issues }
}

fn structure(asset: &ConversationAsset, input: &ValidateInput<'_>, issues: &mut Vec<Issue>) {
    let conv = asset.id.as_str();
    let mut seen = BTreeSet::new();
    for line in &asset.lines {
        if !seen.insert(line.id.as_str()) {
            issues.push(issue(
                "conv_dup_line",
                Severity::Error,
                conv,
                &line.id,
                "",
                format!("line id `{}` is used twice", line.id),
                "give every line a unique id",
            ));
        }
        if !input.speakers.speakers.contains_key(&line.speaker) {
            issues.push(issue(
                "conv_unknown_speaker",
                Severity::Error,
                conv,
                &line.id,
                "",
                format!("speaker `{}` is not in speakers.ron", line.speaker),
                "add the speaker to assets/conversations/speakers.ron",
            ));
        }
    }
    let steps = asset.effective_steps();
    let mut labels: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, step) in steps.iter().enumerate() {
        if let Step::Label(name) = step
            && labels.insert(name.as_str(), i).is_some()
        {
            issues.push(issue(
                "conv_bad_label",
                Severity::Error,
                conv,
                name,
                "",
                format!("label `{name}` is defined twice"),
                "rename one of the labels",
            ));
        }
    }
    // Control-flow graph over step indices.
    let target = |label: &str, issues: &mut Vec<Issue>| -> Option<usize> {
        let found = labels.get(label).copied();
        if found.is_none() {
            issues.push(issue(
                "conv_bad_label",
                Severity::Error,
                conv,
                label,
                "",
                format!("no label `{label}`"),
                "add the label or fix the name",
            ));
        }
        found
    };
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); steps.len()];
    let mut blocking = vec![false; steps.len()];
    for (i, step) in steps.iter().enumerate() {
        let next = (i + 1 < steps.len()).then_some(i + 1);
        match step {
            Step::Line(id) => {
                blocking[i] = true;
                if asset.line(id).is_none() {
                    issues.push(issue(
                        "conv_bad_label",
                        Severity::Error,
                        conv,
                        id,
                        "",
                        format!("step names line `{id}` which does not exist"),
                        "fix the line id",
                    ));
                }
                edges[i].extend(next);
            }
            Step::Label(_) | Step::Event(_) => edges[i].extend(next),
            Step::Goto(label) => edges[i].extend(target(label, issues)),
            Step::If { then, else_, .. } => {
                edges[i].extend(target(then, issues));
                edges[i].extend(target(else_, issues));
            }
            Step::Choice { options, .. } => {
                blocking[i] = true;
                for option in options {
                    edges[i].extend(target(&option.goto, issues));
                }
            }
            Step::End => {}
        }
    }
    // Reachability from step 0.
    let mut reachable = vec![false; steps.len()];
    let mut stack = if steps.is_empty() { Vec::new() } else { vec![0] };
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut reachable[i], true) {
            continue;
        }
        stack.extend(edges[i].iter().copied());
    }
    let mut played = BTreeSet::new();
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Line(id) if reachable[i] => {
                played.insert(id.as_str());
            }
            Step::Line(id) => issues.push(issue(
                "conv_unreachable",
                Severity::Warning,
                conv,
                id,
                "",
                format!("line `{id}` can never be reached"),
                "remove the step or add a path to it",
            )),
            Step::Label(name) if !reachable[i] => issues.push(issue(
                "conv_unreachable",
                Severity::Warning,
                conv,
                name,
                "",
                format!("label `{name}` can never be reached"),
                "remove it or jump to it",
            )),
            _ => {}
        }
    }
    if !asset.steps.is_empty() {
        for line in &asset.lines {
            if !played.contains(line.id.as_str())
                && !steps
                    .iter()
                    .any(|s| matches!(s, Step::Line(id) if *id == line.id))
            {
                issues.push(issue(
                    "conv_unreachable",
                    Severity::Warning,
                    conv,
                    &line.id,
                    "",
                    format!("line `{}` is never played by any step", line.id),
                    "add a Line step or delete the line",
                ));
            }
        }
    }
    // A reachable cycle made only of non-blocking steps never yields: infinite loop.
    let mut color = vec![0u8; steps.len()];
    let mut cycle_at = None;
    for start in 0..steps.len() {
        if !reachable[start] || blocking[start] || color[start] != 0 {
            continue;
        }
        // Iterative DFS over non-blocking nodes.
        let mut path: Vec<(usize, usize)> = vec![(start, 0)];
        color[start] = 1;
        while let Some(&(node, child)) = path.last() {
            let next = edges[node].get(child).copied();
            if let Some(entry) = path.last_mut() {
                entry.1 += 1;
            }
            match next {
                Some(n) if blocking[n] => {}
                Some(n) => match color[n] {
                    0 => {
                        color[n] = 1;
                        path.push((n, 0));
                    }
                    1 => {
                        cycle_at = Some(n);
                    }
                    _ => {}
                },
                None => {
                    color[node] = 2;
                    path.pop();
                }
            }
            if cycle_at.is_some() {
                break;
            }
        }
        if cycle_at.is_some() {
            break;
        }
    }
    if let Some(step) = cycle_at {
        issues.push(issue(
            "conv_no_exit",
            Severity::Error,
            conv,
            &format!("step {step}"),
            "",
            "a reachable loop has no Line, Choice or End: it would never yield".to_string(),
            "put a Line, Choice or End inside the loop",
        ));
    }
}

fn locale_checks(
    asset: &ConversationAsset,
    locale: &str,
    input: &ValidateInput<'_>,
    env: &dyn ValidateEnv,
    issues: &mut Vec<Issue>,
) {
    let conv = asset.id.as_str();
    let chain = env.chain(locale);
    let size = {
        let metrics = BlockMetrics::default();
        let safe = safe_rect(&metrics, 0.0);
        (safe.h / 24.0, (0.62 * safe.w).min(1400.0))
    };
    for (index, line) in asset.lines.iter().enumerate() {
        let speaker = input
            .speakers
            .speakers
            .get(&line.speaker)
            .cloned()
            .unwrap_or_default();
        if locale != input.source_locale && !env.has_text_in(locale, &line.text) {
            issues.push(issue(
                "conv_untranslated",
                Severity::Warning,
                conv,
                &line.id,
                locale,
                format!(
                    "`{}` has no text in `{locale}` (a fallback supplies it)",
                    line.text
                ),
                "translate the key",
            ));
        }
        if !line.cues.is_empty() && locale != input.source_locale {
            let source_count = line.cues.len();
            let have = line.cues.iter().filter(|c| env.has_text_in(locale, &c.1)).count();
            if have != 0 && have != source_count {
                issues.push(issue(
                    "conv_cue_count_mismatch",
                    Severity::Error,
                    conv,
                    &line.id,
                    locale,
                    format!("`{locale}` has {have} of {source_count} sub-cue pieces"),
                    "translate every sub-cue key so the piece count matches the source",
                ));
            }
        }
        // Voice.
        let exists = |p: &str| env.clip_exists(p);
        let ctx = VoiceContext {
            conversation: conv,
            index,
            speaker_voiced: speaker.voiced,
            template: Some(input.settings.voice_path.as_str()),
            fallback_policy: super::asset::VoiceFallback::Source,
            chain: &chain,
            exists: &exists,
        };
        let voiced = speaker.voiced && line.voice != VoiceSpec::None;
        let resolution = resolve_voice(line, &ctx, locale);
        if voiced {
            match (&resolution.clip, resolution.fallback) {
                (None, _) => {
                    let severity = if input.settings.clip_policy == ClipPolicy::Require {
                        Severity::Error
                    } else {
                        Severity::Warning
                    };
                    issues.push(issue(
                        "conv_missing_clip",
                        severity,
                        conv,
                        &line.id,
                        locale,
                        format!("no voice clip for `{locale}` or any fallback"),
                        "record the line or set `voice: None`",
                    ));
                }
                (Some(_), true) => issues.push(issue(
                    "conv_clip_fallback",
                    Severity::Info,
                    conv,
                    &line.id,
                    locale,
                    format!(
                        "uses the clip of `{}`",
                        resolution.locale.as_deref().unwrap_or("?")
                    ),
                    "record this locale to remove the fallback",
                )),
                (Some(_), false) => {}
            }
        }
        let mut clip_us = None;
        if let Some(clip) = &resolution.clip {
            clip_us = env.clip_duration_us(clip);
            if clip_us.is_none() {
                issues.push(issue(
                    "conv_clip_missing_header",
                    Severity::Error,
                    conv,
                    &line.id,
                    locale,
                    format!("the duration of `{clip}` cannot be read"),
                    "re-export the clip as a valid ogg, wav or flac",
                ));
            }
        }
        let text = env.text(locale, &line.text).unwrap_or_default();
        let (planned, _) = plan_duration(line.duration, clip_us, &text, input.settings);
        let cps = if is_cjk_locale(locale) {
            input.settings.reading_cps_cjk
        } else {
            input.settings.reading_cps
        };
        let seconds = planned as f64 / 1_000_000.0;
        let too_fast = seconds > 0.0 && grapheme_count(&text) as f64 / seconds > f64::from(cps);
        let max_lines = input.settings.caption_policy().max_lines;
        let wrapped = wrap(&text, size.1, max_lines, false, size.0, &mut EmMeasure);
        if too_fast || wrapped.overflow {
            issues.push(issue(
                "conv_line_too_long",
                Severity::Warning,
                conv,
                &line.id,
                locale,
                if too_fast {
                    format!(
                        "{} characters in {seconds:.1} s is faster than {cps} per second",
                        grapheme_count(&text)
                    )
                } else {
                    format!(
                        "the text wraps to {} lines (limit {max_lines})",
                        wrapped.lines.len()
                    )
                },
                "shorten the text or lengthen the line",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::asset::{ConversationLine, DurationSource, Speaker, SubCue};

    struct Env {
        texts: BTreeMap<(String, String), String>,
        clips: BTreeMap<String, Option<u64>>,
    }

    impl ValidateEnv for Env {
        fn has_text_in(&self, locale: &str, key: &str) -> bool {
            self.texts.contains_key(&(locale.to_string(), key.to_string()))
        }
        fn text(&self, locale: &str, key: &str) -> Option<String> {
            self.chain(locale)
                .iter()
                .find_map(|l| self.texts.get(&(l.clone(), key.to_string())).cloned())
        }
        fn chain(&self, locale: &str) -> Vec<String> {
            if locale == "en" {
                vec!["en".into()]
            } else {
                vec![locale.to_string(), "en".into()]
            }
        }
        fn clip_exists(&self, path: &str) -> bool {
            self.clips.contains_key(path)
        }
        fn clip_duration_us(&self, path: &str) -> Option<u64> {
            self.clips.get(path).copied().flatten()
        }
    }

    fn line(id: &str, speaker: &str) -> ConversationLine {
        ConversationLine {
            id: id.into(),
            speaker: speaker.into(),
            text: format!("k-{id}"),
            note: String::new(),
            emotion: None,
            tags: Vec::new(),
            voice: VoiceSpec::Auto,
            duration: DurationSource::Clip,
            cues: Vec::new(),
            priority: None,
            keep_breaks: false,
        }
    }

    fn speakers() -> SpeakerSet {
        let mut set = SpeakerSet::default();
        set.speakers.insert("guard".into(), Speaker::default());
        set.speakers.insert(
            "ghost".into(),
            Speaker {
                color: Some("#202060".into()),
                ..Speaker::default()
            },
        );
        set
    }

    fn base_env() -> Env {
        let mut texts = BTreeMap::new();
        for id in ["ok", "missing", "fallback", "long", "cued", "dead", "loop_a"] {
            texts.insert(("en".to_string(), format!("k-{id}")), "Hello there".to_string());
        }
        texts.insert(
            ("en".to_string(), "k-long".to_string()),
            "word ".repeat(80).trim().to_string(),
        );
        texts.insert(("en".to_string(), "cue-a".to_string()), "a".to_string());
        texts.insert(("en".to_string(), "cue-b".to_string()), "b".to_string());
        Env {
            texts,
            clips: BTreeMap::new(),
        }
    }

    #[test]
    fn a_fixture_project_yields_exactly_the_expected_codes() {
        let settings = ConversationSettings {
            voice_path: "vo/{locale}/{conversation}_{line}.ogg".into(),
            ..ConversationSettings::default()
        };
        let mut env = base_env();
        env.clips.insert("vo/en/c_ok.ogg".into(), Some(2_000_000));
        env.clips.insert("vo/en/c_fallback.ogg".into(), Some(2_000_000));
        env.clips.insert("vo/en/c_long.ogg".into(), Some(2_000_000));
        env.clips.insert("vo/en/c_cued.ogg".into(), Some(2_000_000));
        env.clips.insert("vo/en/c_dead.ogg".into(), Some(2_000_000));
        env.clips.insert("vo/en/c_loop_a.ogg".into(), Some(2_000_000));
        env.texts.insert(("de".into(), "k-ok".into()), "Hallo".into());
        env.texts.insert(("de".into(), "cue-a".into()), "a".into());
        env.clips.insert("vo/de/c_ok.ogg".into(), Some(1_000_000));
        let mut cued = line("cued", "guard");
        cued.cues = vec![SubCue(0, "cue-a".into()), SubCue(500, "cue-b".into())];
        let conv = ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![
                line("ok", "guard"),
                line("missing", "guard"),
                line("fallback", "guard"),
                line("long", "guard"),
                cued,
                line("dead", "guard"),
                line("loop_a", "guard"),
            ],
            steps: vec![
                Step::Line("ok".into()),
                Step::Line("missing".into()),
                Step::Line("fallback".into()),
                Step::Line("long".into()),
                Step::Line("cued".into()),
                Step::Goto("end".into()),
                Step::Line("dead".into()),
                Step::Label("spin".into()),
                Step::Goto("spin".into()),
                Step::Label("end".into()),
                Step::End,
            ],
        };
        let locales = vec!["en".to_string(), "de".to_string()];
        let speakers = speakers();
        let input = ValidateInput {
            conversations: &[conv],
            speakers: &speakers,
            settings: &settings,
            locales: &locales,
            source_locale: "en",
            strict: false,
        };
        let report = validate(&input, &env);
        let codes = report.by_code();
        let expected: BTreeMap<&str, usize> = BTreeMap::from([
            ("conv_missing_clip", 2),       // `missing` in en and in de
            ("conv_clip_fallback", 5),      // de uses the en clip for five lines
            ("conv_untranslated", 6),       // de lacks six of the seven line texts
            ("conv_cue_count_mismatch", 1), // de has one of the two sub-cue pieces
            ("conv_line_too_long", 2),      // the 80-word line in en and in de
            ("conv_unreachable", 3),        // line `dead`, label `spin`, line `loop_a` never played
            ("conv_speaker_contrast", 1),
        ]);
        let got: BTreeMap<&str, usize> = codes.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(got, expected);
        assert!(report.has_errors()); // the cue mismatch
    }

    #[test]
    fn dead_label_and_infinite_loop_are_found() {
        let settings = ConversationSettings::default();
        let env = base_env();
        let conv = ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![line("ok", "guard")],
            steps: vec![
                Step::If {
                    cond: "x".into(),
                    then: "spin".into(),
                    else_: "nowhere".into(),
                },
                Step::Label("spin".into()),
                Step::Goto("spin".into()),
            ],
        };
        let locales = vec!["en".to_string()];
        let speakers = speakers();
        let input = ValidateInput {
            conversations: &[conv],
            speakers: &speakers,
            settings: &settings,
            locales: &locales,
            source_locale: "en",
            strict: false,
        };
        let report = validate(&input, &env);
        let codes = report.by_code();
        assert_eq!(codes.get("conv_bad_label"), Some(&1));
        assert_eq!(codes.get("conv_no_exit"), Some(&1));
        assert!(report.has_errors());
    }

    #[test]
    fn strict_promotes_warnings_and_dup_and_unknown_speaker_are_errors() {
        let settings = ConversationSettings::default();
        let env = base_env();
        let conv = ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![line("ok", "guard"), line("ok", "nobody")],
            steps: Vec::new(),
        };
        let locales = vec!["en".to_string()];
        let speakers = speakers();
        let mut input = ValidateInput {
            conversations: std::slice::from_ref(&conv),
            speakers: &speakers,
            settings: &settings,
            locales: &locales,
            source_locale: "en",
            strict: false,
        };
        let loose = validate(&input, &env);
        assert_eq!(loose.by_code().get("conv_dup_line"), Some(&1));
        assert_eq!(loose.by_code().get("conv_unknown_speaker"), Some(&1));
        let warnings = loose.counts().1;
        assert!(warnings > 0);
        input.strict = true;
        let strict = validate(&input, &env);
        assert_eq!(strict.counts().1, 0);
        assert_eq!(strict.counts().0, loose.counts().0 + warnings);
    }

    /// Spec 56 § 6.3: a duplicate `Label` is `conv_bad_label` (error), and a clip that exists but
    /// whose duration cannot be read is `conv_clip_missing_header` (error).
    #[test]
    fn duplicate_label_and_unreadable_clip_header_are_errors() {
        let settings = ConversationSettings::default();
        let mut env = base_env();
        // Listed (exists) but no readable length.
        env.clips.insert("vo/broken.ogg".into(), None);
        let mut ok = line("ok", "guard");
        ok.voice = VoiceSpec::Explicit(BTreeMap::from([("en".to_string(), "vo/broken.ogg".to_string())]));
        let conv = ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![ok],
            steps: vec![
                Step::Label("top".into()),
                Step::Line("ok".into()),
                Step::Label("top".into()),
                Step::End,
            ],
        };
        let locales = vec!["en".to_string()];
        let speakers = speakers();
        let input = ValidateInput {
            conversations: &[conv],
            speakers: &speakers,
            settings: &settings,
            locales: &locales,
            source_locale: "en",
            strict: false,
        };
        let report = validate(&input, &env);
        let codes = report.by_code();
        assert_eq!(codes.get("conv_bad_label"), Some(&1), "{codes:?}");
        assert_eq!(codes.get("conv_clip_missing_header"), Some(&1), "{codes:?}");
        assert_eq!(
            codes.get("conv_missing_clip"),
            None,
            "an existing clip is not missing"
        );
        assert!(report.has_errors());
    }

    #[test]
    fn order_is_stable() {
        let settings = ConversationSettings::default();
        let env = base_env();
        let conv = ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![line("ok", "guard"), line("missing", "guard")],
            steps: Vec::new(),
        };
        let locales = vec!["en".to_string()];
        let speakers = speakers();
        let input = ValidateInput {
            conversations: &[conv],
            speakers: &speakers,
            settings: &settings,
            locales: &locales,
            source_locale: "en",
            strict: false,
        };
        assert_eq!(validate(&input, &env), validate(&input, &env));
    }
}
