# Spec 18 — Runtime UI (`regia_ui`)

Game-facing UI (HUDs, menus, inventories, touch controls). The editor uses egui instead (ADR-002).

## 1. Model
- UI nodes are ECS entities under a `UiRoot { target: Window|Camera|WorldSpace{size, ppm}, scale_mode,
  sort_order }`. Hierarchy = UI tree.
- `UiNode` + `Style` (taffy style: display flex/grid/block/none, position relative/absolute, size,
  min/max, margin, padding, border, flex props, grid template, gap, align/justify, aspect ratio,
  overflow visible/clip; scrolling is the `ScrollView` widget, ADR-5615) + visual components.
- Visual components: `Background { color | gradient (linear 2-stop Tier 1) | image (9-slice) }`,
  `Border { widths, color, radius (per corner) }`, `Shadow { offset, blur, color }` (drawn on every
  tier; a game may omit it on low tiers), `UiImage { id, uv, tint, preserve_aspect }` (`id` is the renderer's image id, `uv` the sub-rect; ADR-5615), `Text`, `Opacity`, `ClipChildren`,
  `UiCanvas { rects }` (filled rects in the node's unit square: graphs and meters that change every
  frame without re-running layout; ADR-160).
- Widgets (entities + components + systems): `Button`, `Toggle`, `Slider`, `ProgressBar`, `TextInput`
  (single/multi-line, selection, copy/paste through the `UiClipboard` seam, undo, filters, IME commits;
  drawing the IME composition and its candidate window are ADR-273 W01 part 2, ADR-336), `Dropdown`, `ScrollView` (drag/inertia on touch,
  wheel, scrollbars), `ListView` (virtualised for large lists), `TabBar`, `Modal`, `Tooltip`,
  `VirtualStick`, `VirtualButton` (write to input actions, `specs/11` § 7).
- Layout: taffy runs in `PostUpdate/UiLayout` only when dirty; results stored in `ComputedNode
  { rect, clip_rect, z }`.

## 2. Text
- cosmic-text `FontSystem` resource, fonts from `Font` assets + project default font stack with
  fallback fonts (bundle a permissive default: e.g. Noto Sans subset / Inter, OFL-1.1, listed in
  third-party notices).
- `Text { spans: [(string_or_loc_key, style)], align, wrap, overflow: Visible|Clip|Ellipsis|ScaleToFit
  (default `Visible`: clip only where an ancestor clips; ADR-5615), line_height }`, rich-text markup subset (`<b>`, `<i>`, `<color=#..>`, `<size=..>`, `<icon=name>`).
- Shaping, bidi and line breaking by cosmic-text; glyphs rasterised with swash into a dynamic
  **glyph atlas** (R8 for monochrome, RGBA8 for color emoji), LRU eviction, separate atlas per font
  size bucket; sub-pixel positioning off on mobile. SDF text for world-space UI (Tier 2).
- Must render correctly: Latin, Cyrillic, Greek, CJK, Arabic (RTL + shaping), Hebrew, Devanagari, emoji.

## 3. Rendering
- Extract builds a draw list of quads (rect, uv, color, corner radii, border, clip rect, texture id)
  sorted by z; batched into as few draws as possible (texture array or bindless-lite: an array of up to 8
  atlas pages per atlas kind, and one draw per arbitrary image (a bound image id change starts a new batch)).
- Rounded rects/borders/shadows via SDF in the fragment shader (one uber-shader).
- Render graph node at `overlay` (screen-space). Camera-target and world-space roots (ADR-2406) draw through
  the frame's pass extension point: a texture camera's root into that camera's image at `overlay`; a world
  root into an offscreen texture of its `size`, composited as a depth-tested quad at `after_post` (after the
  transparent pass and the post stack, so the panel keeps its authored colours). Both passes exist only while
  such a root does.
- Scale modes: `ConstantPixelSize`, `ScaleWithScreen { reference: (1920,1080), match_width }` (1 = match width, 0 = height, 0.5 =
  blend; ADR-5615), `ConstantPhysicalSize` (dp), `IntegerScale { reference }` (ADR-2866: the floor of the fit
  scale, at least 1, centred with a letterbox, for pixel art). An optional piecewise DPI curve
  (`UiScaleSettings.dpi_curve`, `[dpi, scale]` points; a project sets it in
  `assets/ui/project.uisettings.ron`) shapes `ConstantPhysicalSize`. Respects `SafeArea` via
  `SafeAreaPadding` component.
- Group layers (ADR-2866): `GroupOpacity` draws a node's subtree into a pooled offscreen layer composited once
  at its opacity (a stacking context); `RoundedClip` clips children to the node's rounded rect, antialiased;
  `UiMask` multiplies the subtree by an image's alpha (`stretch`, `contain`, `cover`, `tile`), and `UiMaskAsset`
  names that image by manifest path. Masks are a destination-in pass on the layer, so nested masks intersect; hit
  testing honors both. A layer is cut to what its list can show. No layer for opacity 1, a single primitive, an
  empty subtree or a rounded clip with square corners; past `UiLayerBudget` a group draws non-isolated
  (`ui_group_layer_budget`).

## 4. Interaction
- Pointer: mouse + multi-touch hit testing on `ComputedNode` (respecting clip + `Interactable`
  component + `pick_through`), states (`hovered`, `pressed`, `focused`, `disabled`), `UiEvent`s
  (`Click`, `PointerEnter/Leave`, `DragStart/Drag/DragEnd`, `ValueChanged`, `Submitted`, `FocusChanged`)
  delivered as ECS events and to scripts on the owning entity / bubbling to ancestors (ADR-7070: `bubble_ui_events`
  writes a `UiBubbledEvent { current, event, depth }` per ancestor, nearest first, for every kind but
  `PointerEnter`/`PointerLeave`, stopping after a `UiStopBubble` node; scripts get `ScriptEvent::Ui { entity, origin,
  event }` on the target and on each ancestor).
- **Focus navigation** (gamepad/keyboard/TV): directional navigation using geometry (nearest node in
  direction with overlap preference), explicit overrides (`NavLinks { up, down, left, right }`), focus
  scopes (modal traps focus), default focus per screen, the `UiNavActions` resource (up/down/left/right, next/prev, submit, cancel; ADR-5615)
  filled by the keyboard path and `ui_gamepad_navigation`, or, while the `Menu` input context is declared and enabled
  for player 0, by `ui_menu_navigation` from its actions (`Up Down Left Right Next Prev Submit Cancel`, `Navigate`
  stick; `InputConfig::with_menu_context`; the raw paths stand down, ADR-7070). Visible focus ring style.
  *(batch v020, ADR-213 Steam Deck)*: `ui_gamepad_navigation` maps the standard gamepad (D-pad/left stick with
  repeat, A/B, LB/RB) to `UiNavActions`; the first press focuses the default node; Tab/LB/RB follow reading
  order; left/right drive a focused `Slider`; `UiInputMode` tells pointer from navigation;
  `UiDevicePolicy` sets a minimum text size (12 px on a Deck) and an optional title-safe margin,
  and on handhelds caps a reference-scaled root at the scale where its reference fits the window
  (`fit_reference`, ADR-526: a 16:9 layout is not clipped on the 16:10 panel);
  `ScaleMode::HANDHELD` is the 1280×800 reference.
  *(UI lane 9, ADR-516)*: the focus graph (`regia_ui::focus_graph`). `FocusGroup` on a container keeps
  directional moves inside it while a candidate exists there; each edge is `Escape` (default), `Stop` or `Wrap`;
  a group can cycle Tab (`TabRule::Cycle`) and remember its last focused child. `TabIndex` orders Tab (positive
  first, negative skipped). An explicit link to a non-navigable node falls through to geometry; a link to itself
  blocks the direction. Stale focus (despawned, hidden, outside the scope) recovers instead of locking
  navigation, and in navigation mode a despawned or hidden focus moves to the nearest node at once. Agents:
  `ui.nav_graph`, `ui.nav_move`, `ui.nav_lint` (`book/agents/ui-navigation.md`).
- Input routing: when the UI consumes a pointer/nav event, gameplay input contexts don't see it (ADR-7070:
  `regia_input::UiPointerGate` hides mouse-button bindings of every context but the exempt ones, `Menu` by default,
  from the frame after the UI took the pointer until the press ends).
  *(ADR-7976)*: mouse buttons use a press-to-release latch, `regia_input::PointerClaims`. `regia_ui::interaction::
  claim_pointer_buttons` runs in `PreUpdate` after `update_pointer` and `InputSet::Devices` and before
  `InputSet::Evaluation`, so a claim is in place in the frame of its press edge. A button whose press edge lands on a
  hit-testable node (the hit `ui_interaction` takes: top layer only, none under `game_only`), or while a press already
  belongs to a node, is claimed until that button is up again. Action evaluation reads a claimed button as up for every
  binding that names it (any trigger: down, pressed, released, hold, tap, pulse, chord), so gameplay sees neither edge. A drag that leaves
  the widget stays claimed; a press that started on the world stays the game's when it moves over UI. On whenever
  `UiPlugin` is present (`PointerClaims::set_enabled(false)` turns it off). Claims are derived state: recordings and
  replays never store them. Keyboard and gamepad are routed by focus and layers, not claims.
  *(ADR-8786)*: while claims are enabled, the ADR-7070 gate is set only while a primary press belongs to a node, not
  on hover, so it never hides a world press that moves over UI; with claims off it keeps the hover-or-press reading.
  Host navigation keys (h/j/k/l or any other map): a host system ordered `.before(regia_ui::focus::ui_focus_navigation)`
  sets `UiNavActions`; the arrows, Tab and Enter keep working (recipe in `book/agents/ui-input.md`).

## 5. Styling and authoring
- `UiTheme` asset: palette, fonts, sizes, named style classes (`"button.primary"`) with state variants
  (hover/pressed/focused/disabled) and transitions (duration, easing).
- `.ui.ron` layout assets (tree of nodes with classes, components, bindings); spawned like prefabs:
  `regia_ui::spawn_ui(world, name)` by library name (ADR-5615). Tier 1: authored by hand or code; Tier 2: visual UI designer in editor.
- Pictures in `.ui.ron` (v9, ADR-3148): an `image` or `mask` names its picture by asset path or icon-atlas name, loaded
  in the background with a placeholder until ready and a fallback on failure; the cook packs what layouts name
  (`docs/specs/81-ui-designer.md` § 4.6).
- Data binding (Tier 1, simple): `Bind { source: BindSource::Resource("Score.value") | Component(..) | Item(..),
  target: BindTarget::Text, format: Some("{:03}") }` (typed source and target, ADR-5615; `BindSource::ScriptField("Hud.score")`
  reads a script's field through `Script::reflect_fields` on the entity or its nearest scripted ancestor, one way, ADR-7070) polled per frame when source changed (change detection).
- Animations: `Tween` (from framework) on UI properties; screen transitions (fade/slide) helpers.

## 6. Localization
- Fluent (`.ftl`) files per locale under `assets/localization/<locale>/`; `Localization` resource
  selects locale (system default → project fallback). `Text` spans can be `Loc("menu-start")` with
  arguments. Language switch at runtime re-resolves all texts. Missing keys show `⟦key⟧` in dev builds.

## 7. Acceptance (M7)
- `samples/ui_gallery`: every widget, theme switch, RTL + CJK + emoji text page, 1,000-item virtualised
  list scrolling at 60 fps on mobile reference, fully navigable by gamepad and keyboard without mouse,
  touch-scroll with inertia on Android, safe-area handling on a notched device/emulator, language switch.
- Golden images for text rendering (Latin/Arabic/CJK) and the widget gallery.

## 8. Accessibility (ADR-865, ADR-1621, ADR-1998)
- `regia_ui::a11y` infers a semantic tree from the widgets (role, name, value, numeric range, states,
  bounds, live regions; `Accessible` overrides), diffs it per frame only while a screen reader or exporter is
  live, and routes reader requests (press, raise, lower, set text, focus, close, scroll) through the UI's
  input path with refusals for disabled, inert and unknown nodes.
- The OS bridge is the off-by-default `accesskit` feature: `regia_ui::accesskit_bridge` sends the tree as
  AccessKit tree updates (a full tree when a reader connects, then only changes, property-only changes
  within a per-frame node budget) to the adapter `regia_platform::accessibility` attaches to the primary
  window behind `platform/<os>.rs` (AT-SPI, UI Automation, NSAccessibility; none on Android yet), and maps the
  adapter's action requests back to UI requests. Node ids are the entity bits.
- Agent: `ui.a11y_tree`, `ui.a11y_announce`, `ui.a11y_act`, `ui.a11y_platform_tree`, `ui.a11y_platform_connect`,
  `ui.a11y_platform_action`, `ui.a11y_audit` (ADR-3025) (book `agents/ui-a11y.md`, `agents/ui-a11y-platform.md`,
  `agents/ui-a11y-audit.md`).

### Status (ADR-5615, conformance audit 2026-11-10)

Built since the audit: `VirtualStick`/`VirtualButton` write the `VirtualAxes`/`VirtualButtons`
resources and are merged into input actions — `inject_virtual_controls` (ADR-5610) holds the
stick's action at its deflection and a pressed button's at `1.0` through
`regia_input::InputInjection`, releasing on lift so the action reads its device bindings again (a
stick wins over a button on the same action; no-op without an input layer).

As built (ADR-7070, lane post05-ui-input): virtual controls feed input actions (`inject_virtual_controls`, ADR-5610);
UI events bubble (`UiBubbledEvent`, `UiStopBubble`) and reach scripts as `ScriptEvent::Ui`; gameplay mouse bindings
read `UiPointer.consumed_pointer` through `regia_input::UiPointerGate`; navigation reads the `Menu` input context
while it is enabled; `BindSource::ScriptField` binds a script's reflected field (code only; `.ui.ron` spelling not
added).

Not built yet; each is a v0.5.x gap, not a change of intent:
- Sub-pixel positioning is off on Android only. As built (ADR-7208): the mono and color atlases (up to 8 pages each)
  tag every page with a font size bucket (up to 16, 32, 64 px, larger; `atlas::size_bucket`) and pack a glyph only into
  pages of its bucket; the buckets share the kind's texture array (separate pages, not separate textures).
  As built (ADR-8832): the UI extract pins every page it draws from until the frame ends, so LRU recycling never
  wipes a page this frame's quads still sample; a glyph that cannot fit while every page is pinned is not drawn
  that frame and is counted (`GlyphAtlases::thrash`, `ui.glyph_atlas`). Immediate callers outside a frame
  (`regia_quick`, tests) keep the plain LRU recycle.
  As built (ADR-8930): `regia_quick` brackets each `QuickRenderer::build` the same way; its shape-cache hits pin their
  pages (`GlyphAtlases::use_page`), and a text that lost glyphs to a full atlas is not cached
  (`QuickRenderer::glyph_atlas_thrash`). Only callers that use no frame (tests) keep the plain LRU recycle.
- § 7: the widget-gallery golden (`samples/ui_gallery/tests/goldens/theme_sheet_{dark,light}.png`) is not blessed, so
  that comparison is skipped; the 60 fps mobile list, Android touch inertia and the notched safe area have no automated
  check.

## 9. Game widgets (ADR-2958)

`regia_ui::game_widgets` holds a lobby list, a scoreboard, a radar and minimap, and world markers and target brackets with
lead. They take plain data; `regia_framework::game_ui` feeds them from `GameRules::scoreboard` and `regia_net::Lobbies`.
Anything that moves is placed by `UiTransform`, so it costs no layout pass. `.ui.ron` v10 authors them
(`docs/formats/ui-ron.md`); agents use `ui.lobby_list`, `ui.scoreboard`, `ui.radar`, `ui.world_markers` and
`ui.target_bracket` (`book/agents/ui-game-widgets.md`).


## 10. As planned (ADR-7752, UI parity)

Planned for v0.6.0; nothing here is built at the time of writing. The designer parts are in spec 81 § 11.

- **Recursive disable** (3241). Disabling a node disables its whole subtree: no focus, no input, drawn in the disabled
  style; enabling restores the previous state. `ui.set_enabled` (planned) takes `recursive`.
- **Back handling** (3241). A screen that opts in closes on back (Esc, gamepad B), and the focus that opened it is
  restored. The back key is a UI action, so it is remappable; a screen that does not opt in ignores it.
  `ui.screen_back` (planned); `ui.screens` reports which screens close on back.
- **Containers and markup in documents** (3242). `.ui.ron` gains split, foldable and tab containers and markup text
  (the existing markup parser, applied when a document is built) in one version step with a migrator and an
  `upgrade-project` row; the format-bump plan carries the row.
- **Navigation neighbors and focus styles** (3242). A node can name its up, down, left and right neighbors, which win
  over the geometric search; a style state that applies to keyboard focus only (not pointer hover or touch) joins the
  existing states, so a focus ring can differ from a hover.
- **Layout variants** (3243). Resolved at layout time by orientation and size range; an override replaces only the base
  fields it names.
- **Running-game inspection** (3244). The UI Inspector keeps a bounded ring of recent events per node and applies live
  style edits that are never written to the document unless copied. `ui.inspect` reports them.
- Every control in this section is keyboard-operable and takes colors from theme tokens (§ 8).

## 11. Runtime overlay, font and loading APIs (P-16, ADR-8051)

- `regia_framework::perf_overlay::PerformanceOverlayPlacement` is a separate resource:
  `corner: OverlayCorner` and `offset: [f32; 2]`, inward in logical UI pixels. Defaults remain
  top left, 10 px. Live changes take effect before layout. Nonfinite/negative offsets use
  10 px in the Rust API; agent input rejects them atomically. Placement is not a saved player setting.
- `TextSystem::set_default_family(Some(name))` selects a loaded default sans-serif family;
  `None` restores Inter. Unknown/empty names return `DefaultFontError::Unavailable` without
  mutation. Font changes invalidate both layout and shaping before the next layout pass.
  Explicit span families, bundled script fallback and emoji behavior stay active.
- `regia_framework::loading_widget` provides an opt-in `PipelineLoadingPlugin` and a widget
  spawned under a host's UI root. `PipelineLoadingState` reports queued jobs and deferred jobs
  separately; a completed queue with deferred jobs still waits. Errors stay visible. Empty
  queues are complete; restarted queues may lower the displayed fraction to report real work.
- `ShaderCompileNotice` uses explicit monotonic timestamps, with configurable cached/unknown
  and uncached delays (5,000 and 1,500 ms). It appears strictly after the delay, hides on completion
  and resets on later work or a clock regression. It does not estimate remaining time.
- Agent: `perf_overlay.get`, `perf_overlay.toggle {placement}`, `ui.set_default_font`,
  `ui.text_shape` (`default_family`), `ui.inspect` (`pipeline_loading` on the panel), and
  `render.pipeline_warm` (the source queue). Guide: `book/agents/ui-overlay-font-loading.md`.
