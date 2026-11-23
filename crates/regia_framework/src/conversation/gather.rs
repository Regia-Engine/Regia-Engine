// SPDX-License-Identifier: MPL-2.0
//! The `regia loc gather` hook for conversations
//! (`docs/specs/56-dialogue-and-subtitles.md` § 6): every line text key and sub-cue key, with the
//! cast note, emotion, speaker and a character budget as Fluent comments above the message.

use super::ConversationLibrary;
use super::asset::{ConversationAsset, DurationSource};
use super::voice::{VoiceContext, resolve_voice};

/// One key found in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatheredKey {
    /// The Fluent key.
    pub key: String,
    /// 1-based line in the `.conv.ron` file where the key first appears.
    pub line_no: u32,
    /// Comment lines (without the leading `# `), in order.
    pub notes: Vec<String>,
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_of(source: &str, key: &str) -> u32 {
    let needle = format!("\"{key}\"");
    source
        .lines()
        .position(|l| l.contains(&needle))
        .map_or(1, |i| u32::try_from(i + 1).unwrap_or(u32::MAX))
}

/// The keys of one conversation. `source` is the file text (for line numbers); `source_locale`
/// and `chain` decide the clip whose length sets the budget.
pub fn gather_keys(
    asset: &ConversationAsset,
    source: &str,
    library: &ConversationLibrary,
    source_locale: &str,
) -> Vec<GatheredKey> {
    let mut out: Vec<GatheredKey> = Vec::new();
    let chain = vec![source_locale.to_string()];
    for (index, line) in asset.lines.iter().enumerate() {
        let speaker = library
            .speakers
            .speakers
            .get(&line.speaker)
            .cloned()
            .unwrap_or_default();
        let exists = |p: &str| library.clips.contains_key(p);
        let ctx = VoiceContext {
            conversation: &asset.id,
            index,
            speaker_voiced: speaker.voiced,
            template: Some(library.settings.voice_path.as_str()),
            fallback_policy: library.settings.voice_fallback,
            chain: &chain,
            exists: &exists,
        };
        let planned_ms = match line.duration {
            DurationSource::Fixed(ms) => Some(u64::from(ms)),
            _ => resolve_voice(line, &ctx, source_locale)
                .clip
                .and_then(|c| library.clips.get(&c).copied().flatten())
                .map(|us| us / 1000),
        };
        let mut notes = vec![format!("speaker: {}", line.speaker)];
        if !line.note.trim().is_empty() {
            notes.push(format!("note: {}", one_line(&line.note)));
        }
        if let Some(emotion) = &line.emotion {
            notes.push(format!("emotion: {emotion}"));
        }
        if let Some(ms) = planned_ms {
            let chars = u64::from(library.settings.reading_cps) * ms / 1000;
            notes.push(format!("budget: {chars} chars"));
        }
        let mut push = |key: &str, notes: Vec<String>| {
            if !out.iter().any(|k| k.key == *key) {
                out.push(GatheredKey {
                    key: key.to_string(),
                    line_no: line_of(source, key),
                    notes,
                });
            }
        };
        push(&line.text, notes.clone());
        for cue in &line.cues {
            push(&cue.1, notes.clone());
        }
    }
    for step in &asset.steps {
        if let super::asset::Step::Choice { prompt, options } = step {
            let mut add = |key: &str| {
                if !out.iter().any(|k| k.key == *key) {
                    out.push(GatheredKey {
                        key: key.to_string(),
                        line_no: line_of(source, key),
                        notes: vec!["choice option text".to_string()],
                    });
                }
            };
            if let Some(prompt) = prompt {
                add(prompt);
            }
            for option in options {
                add(&option.text);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::asset::{ConversationLine, Speaker, SubCue, VoiceSpec};

    #[test]
    fn keys_carry_note_speaker_and_budget() {
        let mut library = ConversationLibrary::default();
        library
            .speakers
            .speakers
            .insert("guard".into(), Speaker::default());
        let asset = ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![ConversationLine {
                id: "a".into(),
                speaker: "guard".into(),
                text: "conv-a".into(),
                note: "Said\nwhile waving".into(),
                emotion: Some("weary".into()),
                tags: Vec::new(),
                voice: VoiceSpec::Auto,
                duration: DurationSource::Fixed(2000),
                cues: vec![SubCue(0, "conv-a-1".into())],
                priority: None,
                keep_breaks: false,
            }],
            steps: Vec::new(),
        };
        let source = "(\n  lines: [(id: \"a\", text: \"conv-a\", cues: [(0, \"conv-a-1\")])],\n)\n";
        let keys = gather_keys(&asset, source, &library, "en");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key, "conv-a");
        assert_eq!(keys[0].line_no, 2);
        assert_eq!(
            keys[0].notes,
            vec![
                "speaker: guard",
                "note: Said while waving",
                "emotion: weary",
                "budget: 34 chars"
            ]
        );
    }
}
