# Dialogue UI: choices, karaoke and anchored captions (agents)

Guide topic: `agent.guide {"topic": "dialogue-ui"}`. ADR-3234, spec 56 § 12. Builds on
[Conversations, captions and subtitles](conversation.md).

Three player-facing pieces that every dialogue game otherwise builds itself:

- **Choice widgets** (`regia_ui::choice`): a vertical list or a radial wheel. Keyboard, gamepad, pointer and touch;
  focus lands on the default (or first enabled) option on open; disabled options show a reason and are skipped by
  navigation; an optional timer counts fixed simulation steps and picks the default; a right-to-left locale mirrors the
  list (marker and text on the right) and turns the wheel's left and right keys. A pick or a cancel arrives as the
  `ChoicePicked` message. `ConversationChoiceBridgePlugin` (`regia_framework::conversation::ui_bridge`) opens a widget
  for each conversation `Choice` step and calls `ConversationRunner::choose` with the pick; a cancel re-opens the choice.
- **Karaoke highlighting**: the words of a visible caption page change color as they are spoken. It is a color on text that
  is already wrapped, so line breaks and right-to-left runs are exactly those of the plain caption. Timing: the page window
  is divided between the words by estimated syllables (vowel groups; one unit per ideograph), tiling the window with no gap
  and ending at the page end. A line's sub-cues are separate pieces, so a sub-cue start is always a word start.
  Off by default.
- **Anchored captions**: with placement `anchored`, a caption follows a world point or entity (`CaptionAnchors`,
  projected with `CaptionAnchorView`). It sits above the speaker, clamped inside the safe area; an off-screen speaker gets
  the box at the nearest edge with an arrow glyph; two captions never overlap (the later slides up or down); a speaker behind
  the camera, or with no free spot, falls back to the bottom band.

Theme classes: `choice`, `choice-prompt`, `choice-option`, `choice-option-focused`, `choice-option-disabled`,
`choice-reason`, `choice-timer`, `choice-sector`.

## Methods

| Method | What |
|---|---|
| `ui.choice_state { widget? }` | Open widgets: kind, prompt, `rtl`, `focus`, `hover`, `timer { remaining, total, default }`, options with `enabled`, `reason`, `rect`, `classes`, `a11y_name`. |
| `ui.choice_pick { widget?, index? \| input? }` | Queues option `index` (from 0) or a navigation `input` (`up`, `down`, `left`, `right`, `next`, `prev`, `submit`, `cancel`) for the next frame. |
| `captions.anchor_state` | `placement`, `karaoke`, `active`, `anchors`, and per caption `mode` (`on_screen`, `edge_arrow`, `bottom_band`), `arrow`, `rect`. |
| `captions.set_dialogue_style { karaoke?, placement?, anchor? }` | Switches karaoke and `bottom_band` / `anchored`; `anchor { caption, world: [x, y, z] }` pins a caption to a world point (test hook). |

## Errors

`plugin_missing`, `widget_not_found` (no open widget with that id), `option_disabled` (the message carries the reason),
`option_out_of_range`, `invalid_placement`.

## Recipe

1. Add `ConversationChoiceBridgePlugin`; optionally set `ConversationChoiceUi { layout, timer_steps, default_index }`.
2. `conversation.play` a conversation with a `Choice` step; `ui.choice_state` shows the widget.
3. `ui.choice_pick { "index": 1 }`, step a frame: `conversation.state` runs on and `captions.state` shows the next line.
4. `captions.set_dialogue_style { "karaoke": true }`; step the clock and read `captions.state` text; the highlight is a color,
   so the lines do not change.
