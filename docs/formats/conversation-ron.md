# Conversation formats (v1) and cue captions (cue v3)

Spec 56, ADR-1697. All files are RON with `format_version: 1`; unknown fields are an error that names `path:line:col`. A
`format_version` newer than the build is an error.

## `assets/conversations/<id>.conv.ron` (`conversation`)

`id`, `notes?`, `lines: [Line]`, `steps: [Step]` (absent: every line in file order).
Line: `id`, `speaker`, `text` (Fluent key), `note?`, `emotion?`, `tags?`, `voice` (`Auto`, `None`, `Explicit({locale: path})`),
`duration` (`Clip` default, `Fixed(ms)`, `Estimate`), `cues` (`[(at_ms, key)]`, ascending), `priority?`, `keep_breaks?`.
Step: `Line(id)`, `Label(name)`, `Goto(label)`, `If(cond:, then:, else:)`, `Choice(prompt:?, options: [(text:, cond:?, goto:)])`,
`Event(name)`, `End`. Optional values may be written bare (`emotion: "weary"`) or as `Some("weary")`.

## `speakers.ron` (`speakers`)

`speakers: { id: (name_key:?, color:? "#RRGGBB", voiced: true, narration: false, side: Start) }`.

## `conversation.ron` (`conversation_settings`)

`voice_path` (`{locale}`, `{conversation}`, `{line}`, `{speaker}`, `{index}`), `voice_fallback` (`Source`/`Silent`), `clip_policy`
(`Warn`/`Require`), `estimate_cps` (14), `overlap` (`Stack`/`ReplaceSameSpeaker`/`Queue`), `max_visible` (3), `min_display_ms`
(1000), `max_extend_ms` (2000), `reading_cps` (17), `reading_cps_cjk` (9, a placeholder), `default_priority`,
`max_steps_per_frame` (64), `rescale_cues` (not built).

## `cue_events.ron` (`cue_events`)

`events: { "door_slam": (key: "cap-door-slam", kind: Sound, planned_ms: 1500) }`.

## `.cue.ron` v3

Adds the optional `caption: Some((key:, kind: Sound|Speech|Narration, priority: 0, planned_ms: None))`. Every v1 and v2 file
loads unchanged. **Migration:** `regia upgrade-project` stamps `version: 3` (inserted for v1, rewritten for v2) and keeps the
rest of the file, comments included.
