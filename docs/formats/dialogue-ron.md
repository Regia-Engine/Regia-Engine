# `.voice.ron`, `.dialogue.ron`, `.subs.ron` (formats `voice`, `dialogue`, `subtitles`, v1)

ADR-1882, spec 36 §§ 7-8. Readers: `regia_asset::dialogue::{parse_voice, parse_dialogue, parse_subtitles}`. All three
start at `format_version: 1`, reject unknown fields and report `file:line:col` errors. A change that breaks a v1 reader
bumps the version and ships a migrator (`regia upgrade-project`).

- `voice`: `(format_version, id, name_key, gender?: Neutral|Feminine|Masculine|Common|Other, number?: Singular|Plural)`.
- `dialogue`: `(format_version, name, lines: [(id, speaker, listeners?, note?, text_key?, display_key?, mature?,
  variants?: [(speaker?, listener?, sound, cues?)])])`. Line ids and transcript keys are unique per dialogue.
- `subtitles`: `(format_version, priority?: i32 >= 0 (default 1), keep_breaks?, one_at_a_time?, cues: [(at, until?,
  text_key? | text?)])`; `at` non-decreasing and `>= 0`, `until >= at`, exactly one of `text_key`/`text`.

Migration note: new formats, nothing to migrate. The lead registers them with `cargo xtask check-formats --update`.
`keep_breaks` reaches the caption board (ADR-1882); `one_at_a_time` pages a long cue one line per page, shown in
sequence over the cue's time (ADR-2137; it was read and ignored before). Optional fields are written `Some(..)`.
