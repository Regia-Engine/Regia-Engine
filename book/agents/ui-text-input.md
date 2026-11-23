# UI text fields (agents)

Guide topic: `agent.guide {"topic": "ui_text_input"}`. ADR-273 gap W01 parts 1 and 2, ADR-336, ADR-7208, ADR-9340.

A runtime-UI `TextInput` edits like a desktop text field. This page lists what it does, how an agent drives and
reads it, and what is not done yet.

## What the field does

Keys go to the focused, enabled field. Ctrl also works as Cmd (Meta); word moves also take Alt (Option).

| Keys | Effect |
|---|---|
| Left / Right | One caret stop left / right on screen (a grapheme cluster: a letter with its accents, an emoji sequence). In right-to-left text Right moves toward the start of the text; across a left-to-right / right-to-left boundary the logical caret jumps to the stop that is next on screen. At a line's edge the move goes on to the neighboring line. Without a layout (no fonts) the move is logical. With a selection and no Shift, the selection collapses to its left / right end on screen. |
| Ctrl+Left / Ctrl+Right | The next word edge on screen to the left / right (Unicode word boundaries): in left-to-right text the start of the previous word / the end of the next word; in right-to-left text the end of the next word / the start of the previous word; in mixed text the edges come in screen order. At the line's edge the move goes on to the neighboring line. A `secret` field, a field with an IME composition and a field without a layout move logically (start of the previous / end of the next word). |
| Up / Down | Multi-line: the previous / next visual line at the same x. Single-line: left to focus navigation. |
| Home / End | Start / end of the visual line in a multi-line field, of the text in a single-line field, with the caret drawn at the line's visual start / end edge: left / right in a left-to-right paragraph, right / left in a right-to-left one, even where a run of the other direction touches the edge. Ctrl+Home / Ctrl+End: the whole text. |
| Shift + a move | Extends the selection from its anchor. |
| Backspace / Delete | Deletes the selection, else one grapheme cluster. With Ctrl: to the word boundary. |
| Ctrl+A, Ctrl+C, Ctrl+X, Ctrl+V | Select all, copy, cut, paste through the UI clipboard. A `secret` field is never copied or cut. |
| Ctrl+Z; Ctrl+Shift+Z or Ctrl+Y | Undo; redo. Typing joins one undo step until a new word starts, the caret moves or another kind of edit happens; consecutive single Backspace or Delete presses join one step. 100 steps are kept. |
| Enter | Single-line: sends `Submitted(text)`. Multi-line: inserts a line break; Ctrl+Enter submits. |
| Tab, Escape | Focus navigation, as for every widget. |

Ctrl+Alt is AltGr on many layouts, so Ctrl+Alt+key types the character instead of running a shortcut.

Pointer: a press focuses the field and places the caret at the nearest caret boundary of the shaped glyphs (Shift
extends the selection), dragging selects, a double click selects the word (or the run of spaces, or the punctuation
mark) and a triple click the line (the whole text in a single-line field). Carets come from the glyph clusters
`cosmic-text` shaped, so Arabic and other right-to-left text hit-tests by its visual position, and a ligature is split
evenly between the characters it covers.

Each edit sends `ValueChanged(Text)` once per frame with the new text. `read_only` fields select and copy but do not
change. `filter` takes `Integer`, `Numeric` (both accept a partly typed number such as `-` or `3.`) or `Custom(fn)`,
a rule over the whole text after the edit; an edit the filter refuses is dropped whole, a paste is never half applied.
`max_chars` keeps whole grapheme clusters. Line breaks pasted into a single-line field become spaces.

A field with a definite width and height scrolls: single-line fields scroll sideways and multi-line fields wrap and
scroll down, so the caret stays visible. A field sized by its content keeps its old layout and does not scroll.

### IME composition

While an input method composes (Chinese, Japanese, Korean, Vietnamese telex and other input methods), the
composition (`preedit`) is drawn inline at the caret, underlined, in the label's style; the caret is drawn inside it at
the input method's cursor (`preedit_cursor`, else the composition's end) and the selection highlight is hidden. The
composition is not part of `text` and sends no `ValueChanged`; the commit replaces the selection like typing.

The focused field reports a rect in window physical pixels (`ime_rect`): the caret, or the whole composition while
composing. It follows typing, the field's own scroll and the field's layout, and goes to the platform layer through the
`regia_platform::ImeCursorArea` resource, which hands it to the OS window (`set_ime_cursor_area`), so the candidate
window opens next to the text. Android input methods show candidates on the soft keyboard, so the rect is not passed on
there.

The caret, the selection rectangles and the label are child nodes marked `TextInputPart::{Caret, Selection, Label}`;
`ui.query` lists them as plain nodes.

Moving focus away (click, Tab, agent) abandons an in-progress composition: the `preedit` is dropped, the label loses
the underlined ghost immediately, and no `ValueChanged` fires for it. Only a commit (`ImeEvent::Commit`) inserts text.

In a multi-line field a long composition wraps like typed text; the reported `ime_rect` then covers the caret's (last)
line only, not the whole wrapped composition.

### Clipboard

Copy, cut and paste go through the `UiClipboard` resource, which holds one `ClipboardProvider`. The default uses the
system clipboard on Linux, Windows and macOS and keeps an app-local copy, so copy and paste still work inside the app
where the platform has no clipboard (Android today, or a desktop without a display). Tests insert
`UiClipboard::new(MemoryClipboard::default())`.

## Methods

Positions are character indices (Unicode scalar values), not bytes.

- `ui.key { keys, node?, name? }`: presses each chord in order on the next update. A chord is modifiers (`Ctrl`,
  `Shift`, `Alt`, `Meta`) joined with `+` to a key: `Left`, `Right`, `Up`, `Down`, `Home`, `End`, `Enter`, `Backspace`,
  `Delete`, `Tab`, `Escape`, `Space`, a character such as `a` or `é`, and `Ctrl++` for the plus key. Each chord sends
  its modifier keys down, the key down and up, then the modifiers up, through the same keyboard messages a window
  sends, so focus navigation and the field see real keys. Up to 64 chords per call. Keys go to the focused node; `node`
  or `name` names a TextInput to focus first. Returns `{ node, sent, applies: "next update" }`. Mutating; permission
  `edit_scene`.
- `ui.text_state { node?, name? }`: the focused field, or the one named. Returns `{ node, focused, text, length,
  caret, anchor, selection: [start, end], selected_text, line, column, multiline, read_only, secret, filter,
  max_chars, placeholder, scroll, preedit, preedit_cursor, ime_rect, visual_caret, can_undo, can_redo }`.
  `preedit_cursor` is `[start, end]` in characters of `preedit` (null: at its end). `ime_rect` is `[x, y, w, h]` in
  window physical px, the rect handed to the OS for the IME candidate window; `visual_caret` is `{ x, y, height }`, the
  drawn caret (in mixed-direction text a boundary offset has two screen positions, and this is the one shown). Both are
  null while the field is not focused or before its first layout. A `secret` field returns its masked text and a null
  `selected_text`. Read-only.
- `ui.text_select { caret, anchor?, node?, name? }`: sets the caret and selection at once (clamped, moved back to a
  grapheme boundary) and returns the `ui.text_state` answer. `anchor` defaults to `caret`. Mutating; permission
  `edit_scene`.

`ui.type { text }` (page [Runtime UI](ui.md)) still commits text as IME input; use `ui.key` for shortcuts.

```json
{"method": "ui.focus", "params": {"name": "email"}}
{"method": "ui.type", "params": {"text": "ada@example.com"}}
{"method": "ui.key", "params": {"keys": ["Ctrl+Shift+Left", "Ctrl+C", "End", "Ctrl+V"]}}
{"method": "play.step", "params": {"frames": 1}}
{"method": "ui.text_state", "params": {"name": "email"}}
{"method": "ui.text_select", "params": {"name": "email", "anchor": 0, "caret": 3}}
```

## Right-to-left and mixed text (ADR-9340)

A paragraph's direction is its base level by UAX #9 rules P2 and P3: the first strong character decides, so
`123 مرحبا` is a right-to-left paragraph even though it opens with a number (digits are weak). Every visual line of a
wrapped paragraph has the paragraph's direction (`CaretLine::rtl`, `CaretMap::line_rtl`).

The line's two edges belong to its logical ends: the visual start (the right edge in a right-to-left paragraph) is the
line's first byte and the visual end its last. Home in `123 مرحبا` puts the caret at offset 0 drawn at the right edge,
right of the number; End puts it at the end of the text drawn at the left edge. `ui.text_state`'s `visual_caret`
reports the drawn position, `caret` the logical one.

Ctrl+Right walks the word edges on screen (`CaretMap::visual_word_step`): from the left end of `كلمة اخرى ثالثة` it
lands on the start of `ثالثة`, then `اخرى`, then `كلمة`; in `ab كتب قلم cd` it lands after `ab`, then on the start of
`قلم` (the right edge of the leftmost Arabic word on screen), then after `كتب`, then after `cd`.

## Errors

Errors carry `data.reason`:

| Reason | Meaning |
|---|---|
| `ui_no_focus` | No node named and nothing has focus. |
| `ui_not_text_input` | The node is not a `TextInput`. |
| `ui_node_not_found` | No node has that id or name. |
| `ui_node_ambiguous` | More than one node has that name; pass `node`. |
| `ui_bad_key` | A chord has an unknown modifier or names no key. |
| `ui_key_empty` | `keys` is missing or empty. |
| `ui_bad_param` | A parameter has the wrong type (for example a negative index). |

## Not done yet

- Delete and Backspace (with or without Ctrl) stay logical in right-to-left text.
- A label with an ellipsis (`TextOverflow::Ellipsis`) drops a number that opens a right-to-left paragraph
  (cosmic-text 0.19; fields and plain labels lay it out since ADR-9340).
- At a left-to-right / right-to-left run boundary the caret stops keep one stop per screen position: the run that does
  not have the paragraph direction gets no stop of its own, so a caret exactly on that boundary byte can jump two stops
  on a Left / Right. Stops of zero-width clusters (combining marks) collapse to one, and the same-position dedup
  (0.5 px) uses a different tolerance than the visual-caret hint match (0.75 px). Movement edge cases in complex
  scripts, not corrupting.
- On Android the system clipboard is not connected; copy and paste stay inside the app.
