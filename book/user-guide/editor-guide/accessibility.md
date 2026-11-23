# Editor accessibility

What works today for keyboard-only use, screen readers, larger text and color contrast in the
editor, and what does not yet. The review behind this page (November 2026, every finding with
its file and line) is `docs/evidence/editor-a11y-2026-11.md`; findings are numbered **E-n**
(fixed) and **FU-n** (follow-ups). The game-side runtime UI has its own page,
[Accessibility](../accessibility.md).

## Keyboard

- **Every panel of this guide opens from the keyboard**: Ctrl+Shift+P, type `open`, pick **Open
  the … tab** (E-9). Bind any of those commands to a key in **Edit ▸ Preferences… ▸ Keymap**.
- **Tab** and **Shift+Tab** move focus through the buttons, fields, checkboxes and sliders of the
  editor in drawing order: the menu bar, the play toolbar, then each docked panel. **Space** or
  **Enter** presses the focused button; arrow keys change a focused slider.
- The keys each panel handles are listed on its page; [the index](index.md#keys-that-work-everywhere)
  has the ones that work everywhere.
- **Play-in-editor** is fully keyboard driven: Alt+P plays, Escape stops, and the toolbar's
  **▶ Resume** (shown while paused, E-1) and **⏭ Step** take focus with Tab.
- **Viewport panes are focusable widgets** ("Viewport pane 1" to "Viewport pane 4"): Tab into a
  pane and it becomes the active pane, so its keys — `W`/`E`/`R` gizmo mode, `F` frame selection,
  `G` game view, the camera bookmarks, the 2D mode keys (`B`, `,` `.` `[` `]`) and the terrain
  tool keys (`T`, `[` `]`, `-` `=`, `0`-`9`, `,` `.`) — act on that pane without a click (FU-9).
  Clicking a pane still activates it. **Space** and **Enter** on a focused pane do nothing: they
  never pick or clear the selection (picking is a mouse click). A focused text field keeps every
  key it types.
- **A focused pane keeps the keys** in the four-pane layout: while a pane holds keyboard focus,
  its keys stay with it even when the pointer is over another pane, so hovering no longer moves
  the keys until you click a pane (a click on another pane takes the keys there and drops the
  focus) or press **Escape**. Meanwhile the hovered pane can be the one reported active: the
  SpaceMouse button actions (camera views) and the `active` pane agents read follow it. Without
  pane focus, the keys follow the pointer as before.
- The **material and procedural graphs**, the **sequence curve and section strip**, and the
  **profiler's flame view and trace timeline** each have a focusable list beside the canvas
  (ADR-7383): add a node wired after the focused one (Ctrl+Shift+A), connect pins by name
  (**Connect…**), delete, move by Alt+arrow or by one frame (Left / Right), and read span times
  without a hover. Each canvas is announced with its role and a count ("Material graph canvas, 12
  nodes"). The keys are on each area's page.
- The **animation graph**, its **state machine**, the **state tree**, the **VFX curves**, the
  **conversation graph**, the **UI animation keys**, the **camera rig graph**, the **blend
  spaces**, the **animation timeline**, the **curve editor** (animation clips and shot curves),
  the **clip notifies** and the **content grid** follow the same pattern (FU-5): Tab reaches the
  list, Up / Down move between items, Enter selects (Enter again opens a state machine or a
  state's sub-graph, or previews a conversation from its node), Left / Right or Alt+arrows move
  the item (Shift: a coarser step), Delete removes it, Ctrl+Shift+A adds one after it, and
  **Ctrl+Shift+C** starts a connection where the canvas has wires (type the target by name,
  Enter connects, Escape cancels). The focused item draws a ring in the theme's focus color on
  the canvas. In the content grid, arrows move the focus and the selection between tiles.
- Not yet: the 2D tile palette, the montage strip, the sequencer timeline rows, the procedural
  curve parameter, the curve table plot and the UI designer canvas still need the mouse for
  some edits; the agent methods on each page do them without a mouse. Shift+arrows extend the
  selection in the Outliner; the content grid has one selected asset, so Shift does not extend
  there yet.

## Screen readers

egui builds an AccessKit tree of the editor's widgets with a role and a name for each; the
editor now names its glyph buttons (**▾**/**▸** in the Outliner, **↻** in Content, **−**/**+**
in the trace timeline, the **x** buttons of the UI designer), its checkboxes and fields whose
visible label is a separate widget (Preferences, the procedural graph's settings, Project
Settings, the Content search) (E-2 to E-8), the Inspector's boolean checkboxes (named after
their field) and its list buttons **+ − ↑ ↓** ("Add element", "Remove element 0", "Move element
0 up", "Move element 0 down") (FU-6), the **⚠** warnings of the material and procedural graphs
(read as text: the label is the warning text, not a button) (FU-7), the fracture panel's half-extent fields ("Half
extent x/y/z") (FU-8) and the viewport panes (FU-9).

The editor window hands that tree to the operating system's screen reader (FU-1, ADR-7746):
Orca on Linux, NVDA or Narrator on Windows, VoiceOver on macOS. Start the reader, then the
editor (or the other way round: the editor notices a reader that starts later). The window is
read as "Regia Engine Editor"; the play toolbar and the viewport toolbar are announced as
toolbars, and each dock panel as a pane named after its tab (Inspector, Content, Outliner, ...).
The Outliner's eye and lock say what they do and to which row ("Hide Rock in the viewport",
"Lock Rock: not pickable in the viewport"); the Outliner's search field is "Search the Outliner",
and the viewport toolbar's menus are "Viewport quality" and "Gizmo pivot".

With a reader you can press buttons, tick checkboxes, open collapsed sections, select rows,
move the focus, and change number fields and sliders (the reader's increase and decrease
commands, or typing a value). To change a text field, focus it and type. Set
`REGIA_ACCESSKIT=0` before starting the editor to keep it off the accessibility bus.

Checked with tests only so far: the runs with Orca, NVDA and VoiceOver on real windows are
listed in the agent page's real-window steps and the evidence file.

## Text size

**Edit ▸ Preferences… ▸ Appearance ▸ UI scale** zooms the whole interface from 50 % to 300 %, so
150 % and 200 % are both available and kept between sessions. **Ctrl+=** (or Ctrl+Plus) and
**Ctrl+-** zoom in and out by 10 % and **View ▸ Reset zoom** (Ctrl+Alt+0) goes back to 100 %;
each changes the **UI scale** preference, so the zoom is kept between sessions. All three are
commands you can rebind in **Edit ▸ Preferences… ▸ Keymap** (`view.zoom_in`, `view.zoom_out`,
`view.reset_zoom`). **Ctrl+0** is only camera bookmark 0 in the viewport; it no longer resets
the zoom (FU-4, ADR-7733).

The UI scale multiplies your display's own scale: at 200 % on a display set to 150 %, text and
controls are three times their 100 %-display size. Before ADR-8589 the editor window reset the zoom to
the display scale every frame, so the preference saved but changed nothing on screen.

Symbols the editor writes (the Content browser's ← and → buttons, the ▸ in menu paths, the ● before a
changed setting, ✓, ⎇) come from fonts that ship with the editor; none draws as an empty box
(ADR-8589).

## Color contrast

`editor.a11y_contrast` measures a theme's colors against WCAG 2.x AA: every text color on every
surface it is drawn on (panels, windows, fields, the ground, and resting, pressed and open
controls) needs 4.5:1, and control outlines, the keyboard focus ring and small marks need 3:1;
[Editor accessibility (agents)](../../agents/editor-a11y.md) documents it. The editor's built-in
looks, **Piercing Graphite** (the default), **AMOLED Night**, **Paper** and **High contrast**
(**Edit ▸ Preferences… ▸ Appearance ▸ Theme**), pass every pair (ADR-8585; FU-2 is closed). The
lowest ratios in the default look are error text on a window (5.8:1), secondary text on a pressed
button (5.7:1) and a control's outline on a window (4.0:1).

Controls at rest have a visible outline, and a control you reach with Tab shows a 2 px ring in the
theme's Signal color (white in the dark looks, black in Paper, yellow in High contrast); a text
field you click into shows the ring too.

The bridged UI themes (**Theme** `regia-dark`, `regia-light`) take their button, field and
dropdown colors and their warning, error, success, unsaved and log colors from the theme's own
roles (ADR-8589): a control at rest has the panel's color and the theme's strong border as its
outline, hovered and open controls a lifted surface, and every text color, every control state and
the audio meters meet WCAG AA on both themes. Before, both kept the default look's dark buttons and
bright accents, which on `regia-light` meant dark buttons, a yellow "unsaved" label and black meter
bars on white panels (FU-3). Pairs that still miss on the full measured list are listed in
`docs/evidence/editor-a11y-2026-11.md`.
