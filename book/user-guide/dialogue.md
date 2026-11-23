# Dialogue

Spoken lines that depend on who speaks to whom live in `.dialogue.ron` files, the speakers and listeners in `.voice.ron`
files, and the subtitle timing of a recording in a `.subs.ron` file next to the sound. The agent page
([dialogue lines and subtitles](../agents/dialogue-subtitles.md)) has the full format; this page is the workflow.

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
