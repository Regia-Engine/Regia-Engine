// SPDX-License-Identifier: MPL-2.0
//! The voice-over script and the missing-VO report
//! (`docs/specs/56-dialogue-and-subtitles.md` §§ 6.1, 6.2). Pure: they return text and rows, the
//! CLI writes files.

use std::collections::BTreeSet;

use regia_ui::captions::grapheme_count;
use serde_json::{Value, json};

use super::ConversationLibrary;
use super::asset::{ConversationAsset, DurationSource, VoiceSpec};
use super::validate::ValidateEnv;
use super::voice::{VoiceContext, clip_path_for, resolve_voice};

/// The status of a voiced line in a locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoStatus {
    /// The locale's own clip exists.
    Recorded,
    /// Another locale's clip is used.
    Fallback,
    /// No clip anywhere in the chain.
    Missing,
}

impl VoStatus {
    /// The CSV name (`recorded`, `fallback`, `missing`).
    pub fn csv_name(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Fallback => "fallback",
            Self::Missing => "missing",
        }
    }
}

/// One row of the voice-over script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoRow {
    /// The conversation id.
    pub conversation: String,
    /// The line id.
    pub line: String,
    /// The speaker id.
    pub speaker: String,
    /// The speaker's display name in the locale.
    pub speaker_name: String,
    /// The locale.
    pub locale: String,
    /// The text in the locale (or the fallback).
    pub text: String,
    /// True when `text` came from a fallback locale.
    pub fallback_text: bool,
    /// The cast note.
    pub note: String,
    /// The emotion tag.
    pub emotion: String,
    /// Tags joined by `;`.
    pub tags: String,
    /// Graphemes in `text`.
    pub chars: usize,
    /// The planned length in milliseconds, if known.
    pub max_ms: Option<u64>,
    /// The file name the recording must have.
    pub filename: String,
    /// The recording status.
    pub status: VoStatus,
}

/// The CSV header, in column order.
pub const VO_COLUMNS: [&str; 14] = [
    "conversation",
    "line",
    "speaker",
    "speaker_name",
    "locale",
    "text",
    "fallback_text",
    "note",
    "emotion",
    "tags",
    "chars",
    "max_ms",
    "filename",
    "status",
];

fn selected<'a>(library: &'a ConversationLibrary, only: &[String]) -> Vec<&'a ConversationAsset> {
    library
        .conversations
        .values()
        .filter(|c| only.is_empty() || only.contains(&c.id))
        .collect()
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Builds the script rows for `locale`: one per voiced line, conversations by id then file order.
pub fn vo_rows(
    library: &ConversationLibrary,
    env: &dyn ValidateEnv,
    source_locale: &str,
    locale: &str,
    only: &[String],
) -> Vec<VoRow> {
    let chain = env.chain(locale);
    let source_chain = env.chain(source_locale);
    let mut rows = Vec::new();
    for asset in selected(library, only) {
        for (index, line) in asset.lines.iter().enumerate() {
            let speaker = library
                .speakers
                .speakers
                .get(&line.speaker)
                .cloned()
                .unwrap_or_default();
            if !speaker.voiced || line.voice == VoiceSpec::None {
                continue;
            }
            let exists = |p: &str| env.clip_exists(p);
            let own_ctx = VoiceContext {
                conversation: &asset.id,
                index,
                speaker_voiced: true,
                template: Some(library.settings.voice_path.as_str()),
                fallback_policy: super::asset::VoiceFallback::Source,
                chain: &chain,
                exists: &exists,
            };
            let resolution = resolve_voice(line, &own_ctx, locale);
            let status = match (&resolution.clip, resolution.fallback) {
                (None, _) => VoStatus::Missing,
                (Some(_), true) => VoStatus::Fallback,
                (Some(_), false) => VoStatus::Recorded,
            };
            let own_path = clip_path_for(line, &own_ctx, locale).unwrap_or_default();
            let text_own = env.has_text_in(locale, &line.text);
            let text = env.text(locale, &line.text).unwrap_or_else(|| line.text.clone());
            let source_ctx = VoiceContext {
                conversation: &asset.id,
                index,
                speaker_voiced: true,
                template: Some(library.settings.voice_path.as_str()),
                fallback_policy: super::asset::VoiceFallback::Source,
                chain: &source_chain,
                exists: &exists,
            };
            let max_ms = match line.duration {
                DurationSource::Fixed(ms) => Some(u64::from(ms)),
                _ => resolve_voice(line, &source_ctx, source_locale)
                    .clip
                    .and_then(|c| env.clip_duration_us(&c))
                    .map(|us| us / 1000),
            };
            let name = speaker
                .name_key
                .as_deref()
                .and_then(|k| env.text(locale, k))
                .unwrap_or_else(|| line.speaker.clone());
            rows.push(VoRow {
                conversation: asset.id.clone(),
                line: line.id.clone(),
                speaker: line.speaker.clone(),
                speaker_name: name,
                locale: locale.to_string(),
                chars: grapheme_count(&text),
                text,
                fallback_text: !text_own,
                note: line.note.clone(),
                emotion: line.emotion.clone().unwrap_or_default(),
                tags: line.tags.join(";"),
                max_ms,
                filename: file_name(&own_path),
                status,
            });
        }
    }
    rows
}

fn csv_field(out: &mut String, field: &str) {
    if field.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&field.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(field);
    }
}

/// RFC 4180 CSV: UTF-8, CRLF line ends, fields quoted when they hold a comma, quote or newline;
/// a header row first. `bom` prefixes EF BB BF.
pub fn vo_csv(rows: &[VoRow], bom: bool) -> String {
    let mut out = String::new();
    if bom {
        out.push('\u{FEFF}');
    }
    out.push_str(&VO_COLUMNS.join(","));
    out.push_str("\r\n");
    for row in rows {
        let max_ms = row.max_ms.map(|m| m.to_string()).unwrap_or_default();
        let fields: [&str; 14] = [
            &row.conversation,
            &row.line,
            &row.speaker,
            &row.speaker_name,
            &row.locale,
            &row.text,
            if row.fallback_text { "true" } else { "false" },
            &row.note,
            &row.emotion,
            &row.tags,
            &row.chars.to_string(),
            &max_ms,
            &row.filename,
            row.status.csv_name(),
        ];
        for (i, field) in fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            csv_field(&mut out, field);
        }
        out.push_str("\r\n");
    }
    out
}

/// The state of a line in the missing-VO report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportStatus {
    /// Its own clip.
    Ok,
    /// Another locale's clip.
    Fallback,
    /// No clip.
    Missing,
    /// A clip no line names.
    Orphan,
}

impl ReportStatus {
    /// The lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fallback => "fallback",
            Self::Missing => "missing",
            Self::Orphan => "orphan",
        }
    }
}

/// One entry of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    /// The conversation (empty for an orphan clip).
    pub conversation: String,
    /// The line (empty for an orphan clip).
    pub line: String,
    /// The clip path for an orphan, else the expected path.
    pub clip: String,
    /// The status.
    pub status: ReportStatus,
}

/// The report of one locale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleVoReport {
    /// The locale.
    pub locale: String,
    /// The entries: lines in dialogue order, then orphans sorted.
    pub entries: Vec<ReportEntry>,
}

impl LocaleVoReport {
    /// How many entries have that status.
    pub fn count(&self, status: ReportStatus) -> usize {
        self.entries.iter().filter(|e| e.status == status).count()
    }

    /// `ok` over voiced lines, in percent (100 when there are no voiced lines).
    pub fn coverage_percent(&self) -> f64 {
        let voiced = self.count(ReportStatus::Ok)
            + self.count(ReportStatus::Fallback)
            + self.count(ReportStatus::Missing);
        if voiced == 0 {
            100.0
        } else {
            self.count(ReportStatus::Ok) as f64 * 100.0 / voiced as f64
        }
    }
}

fn template_matches(template: &str, locale: &str, path: &str) -> bool {
    // Literal segments with wildcards for every placeholder except {locale}.
    let mut pieces: Vec<Option<String>> = Vec::new(); // Some(literal) | None = wildcard
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            pieces.push(Some(rest[..open].to_string()));
        }
        let Some(close) = rest[open..].find('}') else {
            pieces.push(Some(rest[open..].to_string()));
            rest = "";
            break;
        };
        let name = &rest[open + 1..open + close];
        if name == "locale" {
            pieces.push(Some(locale.to_string()));
        } else {
            pieces.push(None);
        }
        rest = &rest[open + close + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Some(rest.to_string()));
    }
    fn go(pieces: &[Option<String>], text: &str) -> bool {
        match pieces.split_first() {
            None => text.is_empty(),
            Some((Some(lit), tail)) => text.strip_prefix(lit.as_str()).is_some_and(|r| go(tail, r)),
            Some((None, tail)) => {
                let limit = text.find('/').unwrap_or(text.len());
                (0..=limit)
                    .filter(|&i| text.is_char_boundary(i))
                    .any(|i| go(tail, &text[i..]))
            }
        }
    }
    go(&pieces, path)
}

/// Builds the report for the locales.
pub fn vo_report(
    library: &ConversationLibrary,
    env: &dyn ValidateEnv,
    source_locale: &str,
    locales: &[String],
) -> Vec<LocaleVoReport> {
    let mut out = Vec::new();
    for locale in locales {
        let rows = vo_rows(library, env, source_locale, locale, &[]);
        let mut entries = Vec::new();
        let mut named: BTreeSet<String> = BTreeSet::new();
        // The expected path of each line's own clip, by (conversation, line).
        let mut expected: std::collections::BTreeMap<(String, String), String> =
            std::collections::BTreeMap::new();
        for asset in library.conversations.values() {
            for (index, line) in asset.lines.iter().enumerate() {
                let chain = env.chain(locale);
                let exists = |p: &str| env.clip_exists(p);
                let ctx = VoiceContext {
                    conversation: &asset.id,
                    index,
                    speaker_voiced: true,
                    template: Some(library.settings.voice_path.as_str()),
                    fallback_policy: super::asset::VoiceFallback::Source,
                    chain: &chain,
                    exists: &exists,
                };
                if let Some(path) = clip_path_for(line, &ctx, locale) {
                    expected.insert((asset.id.clone(), line.id.clone()), path.clone());
                    named.insert(path);
                }
            }
        }
        for row in &rows {
            let status = match row.status {
                VoStatus::Recorded => ReportStatus::Ok,
                VoStatus::Fallback => ReportStatus::Fallback,
                VoStatus::Missing => ReportStatus::Missing,
            };
            entries.push(ReportEntry {
                conversation: row.conversation.clone(),
                line: row.line.clone(),
                clip: expected
                    .get(&(row.conversation.clone(), row.line.clone()))
                    .cloned()
                    .unwrap_or_else(|| row.filename.clone()),
                status,
            });
        }
        for clip in library.clips.keys() {
            if template_matches(&library.settings.voice_path, locale, clip) && !named.contains(clip) {
                entries.push(ReportEntry {
                    conversation: String::new(),
                    line: String::new(),
                    clip: clip.clone(),
                    status: ReportStatus::Orphan,
                });
            }
        }
        out.push(LocaleVoReport {
            locale: locale.clone(),
            entries,
        });
    }
    out
}

/// The report as JSON.
pub fn report_json(reports: &[LocaleVoReport]) -> Value {
    json!({
        "locales": reports.iter().map(|r| json!({
            "locale": r.locale,
            "ok": r.count(ReportStatus::Ok),
            "fallback": r.count(ReportStatus::Fallback),
            "missing": r.count(ReportStatus::Missing),
            "orphan": r.count(ReportStatus::Orphan),
            "coverage_percent": r.coverage_percent(),
            "entries": r.entries.iter().map(|e| json!({
                "conversation": e.conversation, "line": e.line, "clip": e.clip, "status": e.status.name(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// The report as CSV (`locale,conversation,line,clip,status`, CRLF).
pub fn report_csv(reports: &[LocaleVoReport]) -> String {
    let mut out = String::from("locale,conversation,line,clip,status\r\n");
    for r in reports {
        for e in &r.entries {
            let fields = [
                r.locale.as_str(),
                &e.conversation,
                &e.line,
                &e.clip,
                e.status.name(),
            ];
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                csv_field(&mut out, f);
            }
            out.push_str("\r\n");
        }
    }
    out
}

/// The report as text, one line per entry plus a summary per locale.
pub fn report_text(reports: &[LocaleVoReport]) -> String {
    let mut out = String::new();
    for r in reports {
        for e in &r.entries {
            if e.status == ReportStatus::Ok {
                continue;
            }
            if e.status == ReportStatus::Orphan {
                out.push_str(&format!("{} orphan {}\n", r.locale, e.clip));
            } else {
                out.push_str(&format!(
                    "{} {} {}#{}\n",
                    r.locale,
                    e.status.name(),
                    e.conversation,
                    e.line
                ));
            }
        }
        out.push_str(&format!(
            "{}: {} ok, {} fallback, {} missing, {} orphan ({:.0}% covered)\n",
            r.locale,
            r.count(ReportStatus::Ok),
            r.count(ReportStatus::Fallback),
            r.count(ReportStatus::Missing),
            r.count(ReportStatus::Orphan),
            r.coverage_percent()
        ));
    }
    out
}
