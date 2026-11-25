# Dialogue

<!-- index: Dialogue -->
<!-- index: Voice -->
<!-- index: Subtitle -->
<!-- index: Caption -->
<!-- index: Recording sheet -->

Spoken lines that depend on who speaks to whom live in `.dialogue.ron` files, the speakers and listeners in `.voice.ron`
files, and the subtitle timing of a recording in a `.subs.ron` file next to the sound. One line at a time, chosen by
context, is this page. Scripted conversations with steps and choices are the conversation runner
([conversations and captions](../agents/conversation.md)), a separate system that shares the caption board.

The line player is [spec 56](../../docs/specs/56-dialogue-and-subtitles.md). PO exchange, localized
assets, and the per-line sound variants are [spec 36](../../docs/specs/36-localization-exchange-dialogue-subtitles.md)
(reconciled in ADR-1882). The agent page
([dialogue lines and subtitles](../agents/dialogue-subtitles.md)) has the full format; this page is the workflow.
Text files and locale switching are [Localization](localization.md). A game that needs a HUD and a cue together
follows [UI and audio](../how-to/play/ui-and-audio.md).

1. Write the voices (`assets/dialogue/<id>.voice.ron`) and the dialogue (`assets/dialogue/<name>.dialogue.ron`).
2. Add the transcript keys (`dlg-<dialogue>-<line>` unless a line names its own) to the source locale's `.ftl`.
3. Record: `regia loc dialogue-sheet export --locale en --csv sheet.csv` gives the cast one row per line with the note.
4. Translate the text through the PO exchange (`regia loc export`/`import`) or a recording sheet
   (`regia loc dialogue-sheet export|import --locale fr`).
5. Put localized recordings under `assets/localized/<locale>/` with the base path (`assets/localized/fr/audio/a.ogg`
   replaces `assets/audio/a.ogg` for French audio). The player's audio language (`GameSettings::audio_language`) may
   differ from the text language.
6. Play a line from game code with `regia_framework::dialogue::play_line(world, &DialogueRequest::new("ranger_intro",
   "greeting"))`; its subtitle shows with the other captions and follows the player's caption options.

## What to expect on screen

The line plays on the Voice bus. The caption board, not a fixed 90 percent-width area, places the
text. Long cues page one line at a time when `one_at_a_time` is set. Caption options are
`GameSettings::captions`. A voiced line stays on the sound's clock. `subtitle_sets.sync` re-bases
a cue set on a playhead the caller reads.

Ids for a voice, a dialogue name, and a line are lowercase letters, digits, and `_`, 1 to 48
characters. `regia loc validate`, cook, and package report `dialogue_duplicate_line`,
`dialogue_unknown_voice`, `dialogue_missing_text`, `dialogue_missing_sound`,
`dialogue_parse_error`, and the warning `subtitle_past_end`.

A recording sheet (`regia loc dialogue-sheet import`) writes accepted strings into the locale's
`imported.ftl`. It warns `loc_sheet_missing_recording` when the recording file is absent. With
`--strict`, a stale row fails the import and no project file is written.
