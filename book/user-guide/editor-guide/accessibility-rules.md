# Accessibility rules for editor panels

These are the rules for people who build editor panels and tools in `regia_editor` (or an editor extension). Each
rule is one the editor's own tests enforce (`regia_editor::a11y_audit`, the agent-parity scan, the theme tests), so a
panel that follows them passes review the first time. The person-facing page is [Editor
accessibility](accessibility.md); the agent page is [Editor accessibility (agents)](../../agents/editor-a11y.md).

## 1. Every action has a keyboard path

- Draw controls with egui widgets that take focus (buttons, checkboxes, text fields, combo boxes, selectable labels,
  drag values). A person reaches them with Tab and Shift+Tab and activates them with Space or Enter.
- An action that lives on a painted canvas (a graph, a timeline, a curve) also needs a focusable list beside it:
  register it in `regia_editor::canvas_keys`, as the material graph, the procedural graph, the sequencer and the
  profiler do. A drag on a canvas is never the only way.
- A panel that opens from a menu or a toolbar also gets a command palette entry (`window.open_<panel>`), so it opens
  without the pointer.
- No action is mouse-only: no hover-only buttons, no right-click-only menus without a menu bar or palette twin.
- Prove it with a test: feed `egui::Event::Key { key: Tab, .. }` frames into the panel, read the focused AccessKit
  node, and activate it with Enter or typed text (the Theme page's
  `the_accent_is_reachable_and_editable_from_the_keyboard` is a short example).

## 2. Every control has a name

- A button whose visible text is a glyph (`x`, `−`, `↻`, `▾`) uses `a11y_audit::glyph_button(ui, glyph, spoken)`:
  the spoken name is also its hover text.
- A checkbox or field whose label is a separate widget uses `a11y_audit::bare_checkbox` or `a11y_audit::name`.
- A drag value with no label beside it uses `a11y_audit::named_drag_value` ("Half extent x").
- A focusable pane uses `a11y_audit::spoken_only` (a name without a pane-sized tooltip).
- A status glyph whose color or shape carries meaning ("⚠") uses `a11y_audit::warning_glyph`: it stays a label that
  speaks its message.
- Check it in a test: turn AccessKit on (`ctx.enable_accesskit()`), draw the panel, and assert
  `a11y_audit::unnamed_widgets(&update)` is empty.
- Every control is also drawn through an `agent_parity::action_*` helper or listed in `agent-parity-allow.ron` with
  its agent route, so an agent can do what the control does (`agent_parity_scan`).

## 3. Colors come from the theme, in measured pairs

- Never write a literal color (`Color32::from_rgb`, `Color32::RED`) for chrome. Read the theme: `theme::ui_tokens(ctx)`
  in panel code, `theme::tokens(world)` where you have the world. Users switch themes (including the high-contrast
  one) and change the diagnostic colors; literal colors ignore both.
- Draw text only on the surface its token pair names, so the measured contrast is the contrast on screen:

| Foreground token | On | Needs (AA) | High contrast (AAA) |
|---|---|---|---|
| `text`, `strong`, `muted` | `panel` | 4.5:1 | 7:1 |
| `text` | `well` (text fields, recessed fills) | 4.5:1 | 7:1 |
| `widgets.button_text` / `hover_text` / `open_text` | `widgets.idle` / `hover` / `open` | 4.5:1 | 7:1 |
| `on_signal` | `signal` (the selection) | 4.5:1 | 7:1 |
| `widgets.link`, `warn`, `error`, `accents.*` text colors | `panel` | 4.5:1 | 7:1 |
| a control's boundary (`widgets.idle`, or `line` when the fill is the panel color) | `panel` | 3:1 | 3:1 |

- `editor.a11y_contrast` (with `level: "aaa"` for the high-contrast bar) measures these pairs for any theme; a new
  pair your panel needs belongs in `a11y_audit::editor_contrast` with a test.
- Diagnostic colors go through `theme::diagnostic_color(tokens, level)` (or the `accents.log_*` tokens), so the
  user's error, warning, info and success colors apply.

## 4. Color is never the only meaning

- Pair every meaning-carrying color with text or a shape: the log writes `ERROR` and `WARN`, the Problems panel
  writes the severity, a node error has a "⚠" glyph that speaks its message.
- Check a panel under View ▸ Color Vision (Simulate protanopia, deuteranopia and tritanopia): if two states can no
  longer be told apart, they are relying on hue alone.
- Hues that must stay distinct (gizmo axes, graph pin kinds) come from token sets chosen to survive the simulations
  (`theme::DATA_KIND_DEFAULTS` is a colorblind-safe set).

## 5. Layouts scale

- Sizes are points from egui's spacing and text styles, never pixel constants for text boxes. Users set both the
  whole-UI scale (`appearance.ui_scale`) and the text scale (`appearance.text_scale`, 0.8 to 2.0).
- Let labels wrap or elide (`Label::wrap`, `egui::TextWrapMode`), give fields a desired width rather than an exact
  one, and put long rows in a scroll area.
- Check it with `a11y_audit::clipped_text` on a frame drawn at text scale 2.0: no text may extend past its clip edge
  (`main_panels_at_text_scale_two_clip_no_text` runs the editor's main panels at 1920×1080).

## 6. Text

- US English in every label, hint and message.
- Name things by what they do ("Save theme as…", "Use theme"), not by an internal id.

## 7. Side effects run once

egui may run a panel's UI twice in one frame. A per-frame side effect (a file write, a status message, a counter)
is guarded with `ctx.cumulative_frame_nr()`; actions that follow a click or a key press already run once.
