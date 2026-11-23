# UI parity roadmap (proposed, ADR-273)

Status: **proposed**. Joins three independent notes into one prioritised gap list and the first eight lanes.
Inputs (all dated 2026-10-20, kept in the owner's scratch reports): `regia-ui-inventory.md` (our repo, v0.1.22),
`ui-research-godot.md` (Godot 4.8 beta, MIT), `ui-research-the legacy engine.md` (generic descriptions from public user docs).
When written (2026-10-20) nothing here was code; most rows have since been built (truth pass 2026-11-02, ADR-2312): input layers (`regia_ui::layers`), menus, popups, the virtualised list, tree, data grid and color picker (ADR-2045), the default widgets, drag and drop, the UI designer (spec 81, v0.2.9) and the screen-reader bridge (AccessKit, v0.2.10, ADR-1998); each has a `book/agents/ui-*.md` page. The tables below are the plan record, not a status list. Two stacks stay as decided in ADR-002: `regia_ui` (game runtime UI, taffy + cosmic-text + own
overlay) and the egui editor.

## 0. Rules for every lane in this roadmap

- **Cleanroom.** `CLEANROOM-DENY.txt` paths are never read. Public-documentation rows are reimplemented from the generic
  description in this document, never from proprietary source, and Regia Engine names must not be another product's feature or class names
  (we say `InputLayer`, `Fill`, `ObservableModel`, `LayerCache`; using the Regia glossary).
- **Permissive-reference rows** are MIT: ideas and code may be ported with a licence header and a `THIRD_PARTY_NOTICES` entry. Code
  porting needs a lane that fetches the exact upstream file from the public repo; the planning notes contain
  descriptions only. Rows marked UNVERIFIED in the permissive-reference note (version, API names) must be re-checked first.
- **Provenance classes:** `PORTABLE-MIT-WITH-ATTRIBUTION` (permissive reference), `CLEANROOM-REIMPLEMENT` (public documentation),
  `ORIGINAL` (our design; common CSS/web or textbook behaviour). Entries go in `docs/provenance.md` when each lands.
- **No render hot-path risk** for the first eight lanes: they touch `regia_ui` CPU-side systems, `regia_agent`
  and docs. No change to `ui-overlay.wgsl`, the batcher or `extract.rs` draw emission beyond adding fields read from
  new components. Anything that edits the shader or batcher is flagged `HOT` and is outside the first eight.
- **Rule 8:** every row names its agent surface. Tests are written first (rule 7).
- Size = lane-hours of one agent lane including tests, docs and gate fixes: S ≤ 3, M 3 to 8, L 8 to 20.

## 1. Gap table

Source: G = permissive reference, U = public-documentation reference, B = both, I = inventory only (own audit; ORIGINAL). Prov: MIT = PORTABLE-MIT-WITH-ATTRIBUTION,
CR = CLEANROOM-REIMPLEMENT, OR = ORIGINAL. Status: A absent, Pt partial (from the inventory).
Agent surface names are proposals; all go through the `ui.*` family introduced by gap U01.

### 1a. Agent, testing and editor tooling (blocks usability)

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| U01 | UI tree query, click/type/focus synthesis, `wait_until` on widget state, screenshot of a node (`ui.query`, `ui.click`, `ui.focus`, `ui.type`, `ui.state`) | A | P0 | I (U: runtime inspector; G: AccessKit tree doubles as agent tree) | OR | M | none | headless gallery: query finds node by name/text, click fires `Click`, type fills `TextInput`, `wait_until` on `Focused` | is the surface; `book/agents/ui.md` |
| U02 | Runtime UI inspector and layout-debug overlay (boxes, margins, focus graph, hover pick) | A | P1 | U | CR | M | U01 | overlay golden on gallery; query returns computed rects | `ui.debug_overlay` set/get; screenshot |
| U03 | Visual `.ui.ron` designer panel in the egui editor (tree, palette, details, live preview at device sizes with safe-area and orientation) | A | P0 for editor usability | U (+G editor docks) | CR / OR | L (split into 3 lanes) | U01, U05, T01 | editor offscreen capture goldens; round-trip a `.ui.ron` through the panel unchanged | `editor.ui_designer.*` (open, select, set_prop, save) |
| U04 | Designer device-preview profiles (resolution, DPI, safe-area, orientation flip) | A | P1 | U | CR | S | U03 | preview of Deck, phone portrait matches `UiScales` result | part of U03 |
| U05 | `.ui.ron` v2: includes/templates with parameters, named styles block, `RadialMenu` def; migrator from v1 | A | P1 | I | OR | M | formats freeze (`specs/12` § 4a) | migrator test, `check-formats --update` | `ui.spawn` params |
| U06 | Shared theme/tokens between editor (ADR-229) and runtime UI | A | P2 | I | OR | M | T01 | both resolve the same token file | none |
| U07 | Contrast and text-overflow lint (accessibility and pseudolocalisation checks) | A | P1 | G + I | OR | S | U01, L02 | lint reports overflowing nodes in pseudo-locale gallery | `ui.lint` |

### 1b. Layout

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| Y01 | Anchor presets, pivot and `pivot_ratio` on top of CSS inset (full-rect, centre, corners, edges, with offsets) | Pt | P1 | B | MIT idea + CR | S | none | preset sets expected insets at 3 resolutions | `ui.set_anchor_preset` |
| Y02 | Grid placement and spans (`grid_row`, `grid_column`, areas); raise the 16-track cap | Pt | P1 | I | OR | S | none | taffy placement test | via `ui.set_style` |
| Y03 | Sizing keywords (fit/min/max-content), viewport units | A | P2 | I | OR | S | none | layout semantics tests | via style |
| Y04 | `align_content`, wrap-reverse | A | P2 | I | OR | S | none | layout test | via style |
| Y05 | Split container (draggable divider) for runtime UI | A | P1 | B | MIT idea + CR | M | I01 | drag divider test, ratio clamps | `ui.drag` |
| Y06 | Foldable (accordion) container with exclusive group | A | P1 | G | MIT | S | none | open/close, group exclusivity | `ui.toggle_fold` |
| Y07 | Aspect-fit container (width-controls-height, fit, cover) | Pt (aspect_ratio exists) | P2 | B | OR | S | none | rect tests | style |
| Y08 | Subtree-local size override (min/max/exact) | P | done by min/max | U | - | - | - | - | - |
| Y09 | RTL layout mirroring (`TextDir` is dead today); layout direction follows locale | A | P1 | B | OR | M | none | RTL gallery golden, row order flips, insets mirror | `ui.set_layout_dir` |
| Y10 | Scrollbars: horizontal, thumb drag, track click, themeable; scroll-into-view on focus | Pt | P0 | I | OR | M | none | drag thumb test, focus scrolls item into view | `ui.scroll_to` |
| Y11 | Recursive disable of focus and pointer for a subtree (`Disabled` already excludes hit test per node) | Pt | P1 | G | MIT idea | S | none | modal blocks siblings with one flag | `ui.set_disabled` |
| Y12 | Auto-fit font size (min/max range) | Pt (`ScaleToFit`) | P2 | G | OR | S | none | text-fit test | style |
| Y13 | World-space and camera-target UI (`UiTarget::Camera`/`WorldSpace`) | A | P1 (HUD markers, diegetic) | B | CR | L, **HOT** | render team | golden of a world panel; pointer ray injection test | `ui.set_target`, `camera.raycast` reuse |
| Y14 | Pointer injection into world-space UI (in-world interaction) | A | P2 | U | CR | M | Y13 | ray hits widget | `ui.world_click` |
| Y15 | Safe-area, DPI curve, device policy | P | done | - | - | - | - | - | - |
| Y16 | Integer scale mode, DPI curve (piecewise) option. **Built** (ADR-2866): `ScaleMode::IntegerScale` (floor, letterbox), `UiScaleSettings.dpi_curve` (project file `assets/ui/project.uisettings.ron`), `ui.set_scale_mode` | A | P2 | B | OR | S | none | scale tests | `ui.set_scale_mode` |

### 1c. Styling, theming, animation

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| T01 | Theme asset file (`.theme.ron`), hot reload, type variations with base chaining (variation beats base), full override surface (padding, size, font, shadow, not only 6 visual fields) | Pt | P0 | B | MIT idea (type variations) + CR (shared style assets) | M | `specs/12` format entry | load, chain, hot-reload, golden for variation | `ui.theme` (get, set_variation, reload) |
| T02 | Property tweens and easing (opacity, colour, inset, size, transform), sequences, parallel, show/hide and screen transitions | A | P0 | B | MIT idea + CR | M | none | pure-function sampling like `ShotSequence::sample`, fixed step reproduces | `ui.tween`, `wait_until` on `Tweening` |
| T03 | Layout-neutral visual transform (translate, uniform scale, later rotate) and pivot ratio | A | P0 | G | MIT idea (offset transform, #87081) | M; rotation = **HOT** | T02 for use | rect/hit-test rules: visual-only by default | `ui.set_transform` |
| T04 | Timeline animation assets (named animations, tracks, reverse, loop, speed) | A | P2 | U | CR | M | T02 | sample() determinism | `ui.play_anim` |
| T05 | Fill (brush) kinds: image tiling modes for nine-slice centre/edges, tiled atlas sub-rect, flip/rotate, fit modes | Pt | P1 | B | MIT idea + CR | M | UI sprite-atlas asset | golden per mode | style |
| T06 | Multi-stop and radial gradients; inset shadow, spread; per-edge border colour | Pt | P1 | I | OR | M, **HOT** (shader) | shader owner | goldens | style |
| T07 | Text outline, shadow, stacked effects, underline, strikethrough, letter-spacing, weight, OpenType features | Pt | P1 | B | MIT idea + OR | L, **HOT** (atlas/shader for outline) | none | text goldens | `Text` spans |
| T08 | Cursor shapes (pointer, text, resize, not-allowed) | A | P1 | I | OR | S | platform seam | shape set on hover reported | `ui.state` reports cursor |
| T09 | Per-widget material/fill shader hook (UI materials) | A | P2 | U | CR | L, **HOT** | render | golden | none |
| T10 | Subtree draw cache with volatile flag; render-to-texture subtree with refresh period | Pt (damage tracking exists, ADR-076) | P2 | U | CR | L, **HOT** | measure first with `hud_bench` | bench numbers, no golden diff | `ui.stats` |
| T11 | Group (isolated) opacity, masks, blur/backdrop. **Built** (ADR-2866, backdrop ADR-1836): `GroupOpacity`, `RoundedClip`, `UiMask`, `UiMaskAsset` through pooled layers, mask-aware hit testing, `ui.set_group`, `ui.layer_stats` | A | P2 | I | OR | L, **HOT** | render | goldens | style |
| T12 | Theme audio items (sounds on hover, press, focus, confirm) | A | P1 | G (UNVERIFIED, beta 4.8) | MIT idea | S | kira bus | event emits sound request recorded by test sink | `audio.events`, `ui.state` |
| T13 | Icon set shipped and sprite-atlas UI asset (no manual image ids) | Pt | P1 | I | OR | M | asset pipeline | atlas load + icon in text | `ui.images` |
| T14 | Batching: multiple images per batch (spec 18 says 16) | Pt | P2 | I | OR | M, **HOT** | render | bench | `ui.stats` |
| T15 | Canvas: lines, polylines, arcs for graphs | Pt | P2 | I | OR | M, **HOT** | render | goldens | none |

### 1d. Widgets and data

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| W01 | `TextInput` completion: Shift-selection, clipboard, word nav, multiline Up/Down, undo/redo, mouse caret and drag-select, double-click word, IME preedit and candidate rect, filters (numeric, max, regex) | Pt | P0 | I | OR (textbook editing behaviour) | L (2 lanes) | clipboard platform seam | key-sequence tests, IME preedit test, no clipboard call outside seam | `ui.type`, `ui.key`, `ui.state` (selection) |
| W02 | Popup/overlay layer that escapes owner clip and z (Dropdown, Tooltip, menus) with edge-avoiding placement relative to an anchor | Pt | P0 | B | CR (popup anchor) + MIT idea | M | none | popup stays visible when owner clipped, flips at screen edge | `ui.open_popup` |
| W03 | Menu bar, popup/context menu (submenus, shortcuts shown, optional search field for long menus) | A | P0 for editor/tools, P1 game | B | MIT idea (searchable PopupMenu) + OR | M | W02, I02 | keyboard and pointer traversal | `ui.menu` |
| W04 | Dropdown: type-ahead and search; gamepad "activate before changing value" mode | Pt | P1 | B | OR + CR | S | W02 | typing letter selects | `ui.type` |
| W05 | Toast widget and notification stack (events exist) | Pt | P1 | I | OR | S | T02 | toast shows, expires, test via fixed step | `events.read`, `ui.query` |
| W06 | Spin box / number drag field with format property; unit suffix | A | P1 | G | MIT idea | S | W01 | step, clamp, format | `ui.set_value` |
| W07 | Radio / exclusive group, tri-state toggle, toggle button | A | P1 | I | OR | S | none | exclusivity | `ui.set_value` |
| W08 | Range slider, ticks, vertical slider, indeterminate progress, spinner | A | P2 | I | OR | S | none | tests | `ui.set_value` |
| W09 | Virtualised list v2: variable heights, widget rows with item/entry split and recycling, selection model (single/multi), type-ahead, sticky headers, tile (grid) mode | Pt | P0 | B | CR (item/entry split) + MIT idea (sticky headers) | L | none | 10 000 rows bench, recycle count, selection | `ui.list` (items, select, scroll_to) |
| W10 | Tree view (virtualised, expand/collapse, drop indicator) and table/data grid | A | P1 | B | CR + MIT idea | L | W09 | expand all, 10 000 nodes | `ui.tree` |
| W11 | Colour picker, search box, breadcrumbs, separator, progress dialog | A | P2 | G | MIT idea | M | W01 | tests | `ui.set_value` |
| W12 | Rich text: `<a>` links with events, `<u>`, `<s>`, `<br>`, font-relative inline images, per-glyph effects (wave, shake, fade), styles table for tags | Pt | P1 | B | MIT idea (BBCode set, effects) + CR | M | T07 for u/s | markup parse tests, effect sampling pure fn | `ui.text_links` |
| W13 | Tooltip v2: rich content, focus/gamepad trigger, edge avoidance | Pt | P1 | B | OR | S | W02 | tests | `ui.state` |
| W14 | Tab bar: closable, reorderable, scrolling strip, sizing strategy | Pt | P1 | G (UNVERIFIED options) | OR | S | none | tests | `ui.tabs` |
| W15 | Data binding v2: two-way, more targets (visibility, style class, colour, image, list items), converters, derived fields, one-time vs continuous, observable model notify-on-change | Pt | P1 | U | CR | L | none | write-back test, change-detect count | `ui.bindings` |
| W16 | Drag and drop for runtime UI: operation object with payload, ghost, grab offset, hover/drop/cancel, hover-switch of tabs | A | P1 | B | CR + MIT idea | M | I01 | drag item between lists, cancel restores | `ui.drag_drop` |
| W17 | Auto-focus fuzzy search API for palettes (FuzzySearch) | A | P2 | G (beta 4.8) | MIT | S | none | ranking tests | `ui.fuzzy` |
| W18 | Screen/page stack with history and transitions | A | P1 | U | CR | M | T02, I03 | push/pop restores focus and input mode | `ui.push_page` |

### 1e. Input, focus, navigation

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| I01 | UI events carry button, modifiers, click count; add Double/Right/Middle click, Scroll, KeyDown, long-press, repeat-on-hold | Pt | P0 | I | OR | M | none | synthetic input tests | `ui.click` options |
| I02 | Shortcut/accelerator registry with display labels and mnemonics | A | P1 | G + I | MIT idea | M | I01 | conflicts, context scoping | `ui.shortcuts` |
| I03 | Input layers: stack of layers, only the top receives input, each declares input mode and restores previous on pop | Pt (Modal scope) | P0 for pause menus | U | CR | M | none | modal over modal, pop restores mode | `ui.layers` |
| I04 | Explicit `tab_index`; focus mode (all/click/none) per node; per-direction nav rules (escape, stop, wrap, target). **Built** (UI lane 9, ADR-516): `TabIndex`, `FocusGroup` edge rules, `ui.nav_*`; focus mode not built (pointer clicks do not focus today) | Pt | P1 | B | MIT idea + CR | S | none | nav tests | `ui.focus` |
| I05 | Keyboard focus ring separated from mouse (no ring after a click) | Pt (`UiInputMode`) | P2 | G | MIT idea | S | none | ring state test | `ui.state` |
| I06 | Initial focus: auto-focus a default, restore after modal close | Pt | P1 | G | OR | S | none | tests | `ui.focus` |
| I07 | Abstract input actions table and device glyphs; auto action-hint bar | A | P1 (Deck/Android) | U | CR | M | regia_input | glyph follows last device | `ui.hints` |
| I08 | Multi-touch: pinch, two-finger scroll, swipe gestures, more than one active touch | Pt | P1 | I | OR | M | none | gesture sequences | `ui.touch` |
| I09 | Virtual joystick modes (fixed, dynamic, following) | Pt | P2 | G | MIT idea | S | none | mode tests | `ui.state` |
| I10 | Software cursor for gamepad over pointer-only UI | A | P2 | U (UNVERIFIED) | CR | M | I04 | tests | `ui.state` |
| I11 | On-screen keyboard integration (mobile, console) | A | P2 | U (UNVERIFIED) | OR | M | platform | Android manual | none |

### 1f. Text, localisation, accessibility

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| L01 | Consume `AccessibilitySettings.text_scale`, reduced motion, high contrast, colour-blind-safe palette in `regia_ui` | A | P0 (setting exists, does nothing) | I | OR | S | T02 for reduced motion | text_scale 1.5 re-lays out; reduced motion skips tweens | `settings.set`, `ui.state` |
| L02 | Pseudolocalisation preview (text expansion, accents, brackets) | A | P1 | G | MIT idea | S | none | gallery in pseudo locale, overflow lint | `ui.set_locale pseudo` |
| L03 | Accessibility tree and screen reader (AccessKit-class crate: role, label, state, landmarks, custom-widget hook, spoken summary) | A | P1 | B | MIT idea + CR; crate licence and `docs/03` entry needed | L (2 lanes) | U01 (shares the semantic tree) | tree matches gallery; platform runs manual | `ui.a11y_tree` |
| L04 | Per-locale assets and fonts, number/date formatting, culture fallback chain | A | P1 | U | CR | M | localization.rs | fallback order test | `ui.set_locale` |
| L05 | Placeholder highlighting for localisation authoring | A | P2 | G | MIT idea | S | editor | - | none |
| L06 | Complete script coverage (Hebrew, Devanagari, Thai fonts bundled or verified) | Pt | P1 | I | OR | M | fonts licence | `font_coverage.rs` extended | none |
| L07 | SDF/MSDF text for scaled and world-space text | A | P2 | B | CR (public papers) | L, **HOT** | Y13 | goldens | none |
| L08 | Text selection in labels, copy | A | P2 | I | OR | S | W01 | tests | `ui.state` |
| L09 | Ellipsis glyph for RTL, justify, max-lines | Pt | P2 | I | OR | S | none | text goldens | style |

### 1g. Editor (egui) workflow references

| ID | Gap | St | Pri | Src | Prov | Size | Deps | Test plan | Agent surface |
|---|---|---|---|---|---|---|---|---|---|
| E01 | Dock model: every main screen is a dock, closable, floatable, per-dock tab style | Pt (egui_dock) | P1 | G (beta 4.8) | MIT idea | M | none | layout save/load | `editor.dock.*` |
| E02 | Command palette with fuzzy search; rebindable editor keys UI | Pt | P1 | G + I | MIT idea | M | W17-like fuzzy, I02 | keybinding round-trip | `editor.command` |
| E03 | Outliner: sticky headers, drag-toggle visibility down rows, drop indicators; inspector section copy/paste | Pt | P2 | G | MIT idea | M | none | offscreen capture | `editor.outliner.*` |
| E04 | Remote/runtime inspector of the game UI tree in the editor (see U02) | A | P1 | G + U | OR | M | U01 | - | `ui.query` |

## 2. First eight lanes, ordered by value / cost

Each lane is one prompt-ready task. Common preamble for every lane: *Read `AGENTS.md`, `docs/01-cleanroom.md`,
`docs/specs/18-runtime-ui.md` (or the current UI spec), `docs/specs/23-agent-native.md` and this file. Never read paths in
`CLEANROOM-DENY.txt`. Write failing tests first. Run `cargo fmt --all`, clippy with `-D warnings`, the crate tests,
then `cargo xtask preflight --changed`. Add the agent method(s), the `book/agents/<page>.md` page and the
`book/COVERAGE.md` row. Add a `docs/provenance.md` entry using the provenance class in the gap table. Add an ADR only
if you deviate. Do not edit `ui-overlay.wgsl`, `extract.rs` draw emission or the batcher. No `unwrap`/`expect` on
runtime paths. Commit on your lane branch; the owner merges.*

### Lane 1: UI agent surface (U01) — M, highest value, unblocks every later lane's own tests

- **Task.** Give agents a way to see and drive `regia_ui`. Keep the dependency direction: `regia_ui` never names an
  agent type (ADR-291; an earlier draft cited ADR-202 here, which is the render-out HUD record, ADR-5665): add a small `UiAgentBridge` in `regia_ui` (a plugin that publishes a read-only snapshot
  resource and a command queue resource) and have the agent crate talk to those resources through reflection or a
  trait object registered by the host. Methods: `ui.query` (filter by name, text, class, state; returns id, name,
  type, computed rect, clip rect, text, states Hovered/Pressed/Focused/Disabled, value), `ui.click` (pointer click at
  node centre, button and count options), `ui.focus`, `ui.type` (commit text and keys), `ui.set_value`, `ui.state`
  (focus, input mode, scale, locale), plus `wait_until` predicates `ui.node_exists`, `ui.node_state`.
- **Files.** `crates/regia_ui/src/agent_bridge.rs` (new), `lib.rs`, `crates/regia_agent/src/methods/ui.rs` (new)
  and its registration, `book/agents/ui.md`, `book/COVERAGE.md`, `samples/ui_gallery` (agent-driven test).
- **Acceptance.** Headless gallery test: query by text finds the Settings button; `ui.click` fires `UiEvent::Click`;
  `ui.type` into a `TextInput` updates its content; `wait_until` on `Focused` succeeds after `ui.focus`; all of it
  under a fixed step. Methods needing mutation require `edit_scene` capability. Existing UI goldens unchanged.
- **Risk.** None for render. Watch the dependency direction (`cargo deny`/workspace graph test).

### Lane 2: Accessibility settings and reduced motion wiring (L01) + dead `text_dir` decision (Y09 first half) — S/M

- **Task.** Make `AccessibilitySettings.text_scale` real: multiply text sizes (and the `UiDevicePolicy` min size
  still applies) in `regia_ui` scale handling, trigger re-layout on change. Add `reduced_motion` and
  `high_contrast` fields to settings (format migrator if the settings file is frozen) and expose them as a resource
  `UiAccessibility` that tween and theme code will read later. Implement RTL mirroring for row/column direction and
  start/end insets when `Style.text_dir` is `Rtl` or the locale is RTL, without touching draw emission (mirroring
  is done in the style-to-taffy mapping in `layout.rs`).
- **Files.** `crates/regia_ui/src/{scale.rs,layout.rs,style.rs}`, `crates/regia_framework/src/settings.rs`,
  `book/agents/ui.md` (state section), gallery RTL page.
- **Acceptance.** Test: text_scale 1.5 grows measured text height by 1.5 within rounding and keeps the HUD inside the
  safe area; RTL gallery page lays out a row right to left and a golden is blessed with the reason in the commit
  message; settings file round-trips with a migrator test. Agent: `settings.set` for the three fields and
  `ui.state` reporting them.
- **Risk.** Layout only. Golden re-bless is intended and explained.

### Lane 3: Tween and layout-neutral transform (T02, T03 partial) — M

- **Task.** `UiTween` component set: animate `Opacity`, background/border colour, text colour, `inset`, `Style`
  width/height and a new visual-only `UiTransform { translate, scale, pivot_ratio }` (no rotation yet; rotation is
  a `HOT` follow-up). Easing enum (linear, quad, cubic, expo, back, bounce, elastic, plus cubic bezier). Sequences and
  parallel groups, show/hide helpers (`UiShow`/`UiHide` with fade or slide), completion event. Sampling is a pure
  function like `ShotSequence::sample`, driven by gameplay time so pause and fixed step reproduce. Translate and
  uniform scale are applied by adding an offset to the already computed rect on the CPU in the layout-output step,
  so layout is untouched and the draw emission is unchanged. Hit testing uses the transformed rect (documented
  choice, switchable with a flag). Reduced motion (Lane 2 resource, if landed) jumps to the end value.
- **Files.** `crates/regia_ui/src/{tween.rs (new),node.rs,interaction.rs}`, `book/agents/ui-animation.md`.
- **Acceptance.** Pure-sample unit tests for each ease; a button hover pop test; a fade-in screen test with a fixed
  step and a screenshot at t=0, 0.5 and 1 within tolerance; `ui.tween` agent method (start, stop, query) and
  `wait_until` on tween finished; theme colour transition keeps working.
- **Risk.** Small. Do not change the shader. No allocation per frame for idle tweens (`Changed` filters).

### Lane 4: Input events and input layers (I01, I03, I06) — M

- **Task.** Extend `UiEvent` with `button`, `modifiers`, `click_count`; add DoubleClick, ContextClick (secondary
  button), Scroll, long-press and repeat-on-hold for `Button`. Add `InputLayer` (stack resource plus component): only
  the top active layer receives pointer and navigation input; each layer declares input mode (UI only, UI and game,
  game only) and the previous mode is restored on pop; replaces ad hoc `Modal` scope handling while keeping the
  `Modal` widget API. Default focus on push and focus restore on pop. Keep `UiInputState` semantics for gameplay
  consumption.
- **Files.** `crates/regia_ui/src/{interaction.rs,focus.rs,widgets.rs}`, new `layers.rs`, authoring defs.
- **Acceptance.** Modal over modal over page: input only reaches the top, pop restores focus to the previous
  element; double-click timing test with fixed step; gamepad_nav tests unchanged; agent `ui.layers` (list, push, pop)
  and `ui.click` options (button, count, modifiers).
- **Risk.** Behaviour change to `Modal`; keep old tests as regression.

### Lane 5: Popup layer and menus (W02, W03, W04, W13) — M

- **Task.** A popup layer root (high `sort_order`, outside every owner's clip) with placement relative to an anchor
  node: below, above, left, right, flip at screen edges, shift to stay inside the safe area. Move Dropdown and Tooltip
  onto it (tooltip also on focus for gamepad). Add `PopupMenu` and `MenuBar` and `ContextMenu` widgets with items,
  separators, checkable items, submenus, keyboard and gamepad traversal, shortcut text slots (filled by Lane 7
  later), and an optional search field for long menus (the searchable-menu idea from the permissive-reference note). Type-ahead on
  Dropdown.
- **Files.** `crates/regia_ui/src/{popup.rs (new),widgets.rs,authoring.rs}`, gallery page.
- **Acceptance.** A dropdown inside a clipped scroll view opens fully visible; flips when near the bottom edge;
  submenu traversal by keyboard and by gamepad; closing returns focus to the opener; agent `ui.open_popup`,
  `ui.menu` (list, activate). Uses Lane 4's layer if landed; otherwise own scope with a TODO tracked in the ADR.
- **Risk.** Z and clip ordering use existing root `sort_order`; no extract change.

### Lane 6: TextInput completion, part 1 (W01) — L

- **Task.** Selection with Shift+arrows/Home/End, Ctrl+A, word jump (Ctrl+arrows), Ctrl+Backspace/Delete, clipboard
  through a new `ClipboardProvider` seam in the platform layer (`platform/<os>.rs`; stub for headless; Linux, Windows,
  Android implemented behind it), undo/redo with coalescing, mouse caret placement and drag-select from shaped glyph
  positions, double-click word and triple-click line select, multiline Up/Down and per-line Home/End, horizontal
  scrolling of long text, `read_only`, input filters (`Numeric`, `Integer`, `Max`, regex-free custom fn).
  Part 2 (a later lane): IME preedit display, candidate window rect, bidi caret.
- **Files.** `crates/regia_ui/src/widgets.rs` (split into `text_input.rs`), `text.rs` (glyph hit test API),
  `crates/regia_platform` clipboard seam, tests.
- **Acceptance.** Table-driven key sequence tests (including Unicode and combining marks); undo coalescing test;
  clipboard round trip through a mock provider; mouse selection test on shaped Latin and Arabic text; agent
  `ui.type`, `ui.key`, `ui.state` (caret, selection, content). Docs corrected so they no longer claim IME/clipboard
  until true.
- **Risk.** No render changes (caret and selection quads already exist or reuse background quads; verify).

### Lane 7: Theme asset, type variations, shortcut registry (T01, I02) — M

- **Task.** `.theme.ron` asset (frozen format entry in `specs/12` § 4a plus `docs/formats/theme-ron.md` and
  example) holding palette, fonts, sizes, classes and `variations` (name, base, overrides) with precedence
  per-node override > variation > base class > theme default; widen `VisualOverrides` to padding, min/max size, font
  size, shadow, gap, opacity; hot reload through the existing `.ui.ron` path; `UiThemes` loads from file with the
  built-in dark/light kept as defaults. Shortcut registry (`UiShortcut { chord, action, scope }`) with display strings
  and conflict reporting; widgets can show chords.
- **Files.** `crates/regia_ui/src/{theme.rs,shortcut.rs (new)}`, `docs/formats/`, `docs/specs/12-assets.md`,
  `cargo xtask check-formats --update` (the lane must run it or state it could not).
- **Acceptance.** Variation chain test (variation over base, nested variation); hot reload golden; migrator-ready
  version field; shortcut scope and conflict tests; agent `ui.theme_*` (list, get, set, select, preview, lint, reload; the surface of
  `ui-default-design.md` § 6.6, ADR-274, replaces the earlier `ui.theme` sketch) and `ui.shortcuts` (list, trigger).
- **Risk.** Format freeze: version 1 from day one; today's `UiTheme` serde shape is treated as version 0 and migrated (ADR-274); the hash must be recorded.

### Lane 8: Virtualised list v2 and selection (W09) + scrollbar completion (Y10) — L

- **Task.** Item/entry split: the list owns data items, instantiates row entities only for visible rows, recycles
  them, calls an entry-bind callback (or template with `Bind`), supports variable row heights with a measured prefix
  sum cache, tile mode (wrapped grid), single/multi selection with keyboard (Shift/Ctrl) and gamepad, type-ahead
  jump, sticky section headers, `scroll_to_item`. Scrollbars: horizontal axis, thumb drag, track click, auto-hide
  option, theme slots, scroll-into-view when focus moves to an offscreen row.
- **Files.** `crates/regia_ui/src/scroll.rs` (split `list.rs`), `extract.rs` only to read new scrollbar geometry if
  strictly needed (flag it; otherwise spawn scrollbar as ordinary child nodes), tests, `hud_bench`.
- **Acceptance.** 10 000 rows: spawned entity count bounded by visible plus overscan; scroll bench within the
  existing HUD budget; selection model tests; thumb drag test; focus scroll test; agent `ui.list`
  (items, select, scroll_to) and `ui.scroll_to`. Old `ListView` API remains as a thin wrapper.
- **Risk.** Medium: list code is 270 lines of dense scroll logic. Do not touch batching.

Not in the first eight on purpose: the visual designer (U03) waits for Lanes 1, 3 and 7 and the `.ui.ron` v2 (U05);
AccessKit (L03) waits for Lane 1's semantic snapshot; everything marked **HOT** waits for the render owner.

### 2a. Lane status (conformance audit 2026-11-11, ADR-5665)

All eight lanes are built. Where an ADR changed the lane text above, the ADR wins; open items are v0.5.x gaps.

| Lane | ADR(s) | Refined by the ADR | Acceptance evidence (tests) | Open (v0.5.x) |
|---|---|---|---|---|
| 1 U01 | ADR-291 | Reads on demand (no snapshot resource); seam is the `UiAgentHost` function pointer | `samples/ui_gallery/tests/agent_ui.rs`: `query_by_text_finds_the_settings_button`, `click_fires_one_click_event`, `type_into_a_text_input_updates_its_content`, `wait_until_focused_succeeds_after_ui_focus`, `a_read_only_session_cannot_click` | none |
| 2 L01/Y09 | ADR-341, ADR-518 | Flags in `ui.accessibility`, not `ui.state`; `serde(default)`, no migrator; RTL through taffy's direction | `tests/accessibility.rs` `text_scale_grows_measured_text_and_stays_inside_the_window`, `accessibility_fields_round_trip_and_default_in_older_files`; `rtl_text_dir_lays_a_row_out_right_to_left` | RTL gallery page and golden |
| 3 T02/T03 | ADR-343 | `UiTween::show`/`hide` helpers; `UiTweenFinished` message; `ui.tweening` atom | `tween.rs` ease tests, `a_scale_tween_pops_a_hovered_button_and_settles_back`, `reduced_motion_jumps_to_the_end_value`, `hit_testing_follows_the_transformed_rect_by_default` | fade-in screenshots at t = 0, 0.5, 1 (needs goldens); idle allocation now tested: `tests/tween_idle_alloc.rs` |
| 4 I01/I03/I06 | ADR-293, ADR-5665 | Timers on `RealTime` (ADR-5665 § 2); `ui.click` `modifiers` added by ADR-5665 | `suite/input_layers.rs` `modal_over_modal_over_page_only_the_top_gets_input_and_pop_restores_focus`, `two_clicks_inside_the_window_are_one_double_click_and_outside_are_two_clicks`; `click_with_modifiers_reports_them_on_the_click_and_releases_them_after` | none |
| 5 W02-W04/W13 | ADR-332 | `ui.menu` lists, `ui.menu_activate` activates; `ui.close_popup` | `suite/popup_menus.rs` `a_dropdown_inside_a_clipped_scroll_view_opens_fully_visible`, `a_dropdown_near_the_bottom_edge_flips_above`, `submenu_traversal_by_keyboard`, `submenu_traversal_by_gamepad`, `an_outside_click_closes_and_focus_returns_to_the_opener` | none (menus show registry chords since ADR-5665 § 5: `menu_rows_show_the_registry_chord_unless_the_item_sets_its_own`) |
| 6 W01 parts 1 and 2 | ADR-336, ADR-7208 | Clipboard seam is `regia_ui::clipboard` over `regia_platform`'s clipboard (Android: app-local); caret/selection in `ui.text_state`/`ui.text_select`; part 2 (ADR-7208): preedit drawn as an underlined span, IME rect through `regia_platform::ImeCursorArea` (Android no-op), visual Left/Right over `CaretMap::visual_stops` | `text_input.rs` `key_sequences_edit_and_select`, `undo_coalesces_typing_and_redo_restores`, `clipboard_round_trip_through_the_provider`; `tests/text_input.rs` `pointer_places_the_caret_drags_and_multi_clicks_select`, `pointer_drags_and_double_clicks_select_on_shaped_arabic_text`, `a_drag_over_arabic_in_mixed_text_selects_the_logical_range_under_the_pointer`, `preedit_is_drawn_inline_underlined_with_the_caret_inside`, `preedit_cursor_from_the_ime_places_the_caret_inside_the_composition`, `ime_rect_follows_the_caret_after_typing_and_scrolling`, `right_arrow_moves_visually_across_an_ltr_rtl_boundary`; `regia_agent` `text_state_reports_the_ime_rect_and_visual_caret_through_the_dispatcher`; ADR-9340: `ctrl_right_in_arabic_lands_on_the_next_visual_word_boundary`, `ctrl_right_steps_through_a_mixed_line_in_screen_order`, `end_lands_on_the_visual_end_of_a_right_to_left_line`, `home_in_a_right_to_left_paragraph_that_opens_with_a_number_goes_to_the_right_edge` (visual word moves, Home/End at the line's visual edges by the paragraph's UAX #9 P2/P3 base level) | Delete/Backspace stay logical in RTL text |
| 7 T01/I02 | ADR-274, ADR-289, ADR-374 | `shortcuts.rs`; `ui.shortcut_trigger`, `ui.shortcut_contexts` | `tests/theme_resolve.rs` precedence rows, `theme_tokens.rs` v0 migration, `tq_shortcuts.rs` conflict tests, `theme/agent.rs` `select_returns_the_previous_selection_and_rejects_unknown_names` (also `ui.theme_list`) | none: full padding and min/max size overrides reach layout since ADR-7208 (`tests/theme_slots.rs` `theme_padding_and_min_max_sizes_resolve_and_reach_layout`; file-change hot reload tested: `a_changed_theme_file_hot_reloads_and_a_broken_edit_keeps_the_last_good`) |
| 8 W09/Y10 | ADR-344 | Selection follows focus | `suite/list_virtual.rs` `ten_thousand_rows_keep_a_bounded_pool`, `thumb_drag_scrolls_the_view`, `focus_moving_into_a_view_scrolls_it_once`, `clicks_select_with_shift_and_ctrl` | `hud_bench` scroll numbers (scrollbar theme slots built, ADR-7208: `tests/theme_slots.rs` `the_built_in_thumb_draws_in_the_theme_scrollbar_thumb_slot`, `scrollbar_presets_take_the_track_and_thumb_slots`; gamepad selection tested: `the_dpad_moves_the_selection_with_focus`) |

### Dependency graph for the first eight

Lane 1 first (others use its `ui.*` surface in tests). Lanes 2, 3, 4, 7 are independent and can run in parallel
(Lane 3 reads Lane 2's resource if it landed). Lane 5 prefers Lane 4. Lane 6 and Lane 8 are independent. Merge order
suggestion: 1, 2, 4, 3, 7, 5, 6, 8.

## 3. Provenance map (the basis each lane cited; the lanes' `docs/provenance.md` rows have landed, e.g. the UI input, tween, popup and menu rows)

| Gap(s) | Note | Basis to cite |
|---|---|---|
| T01 variations | Godot note D4 (theme type variations), plus the legacy engine S25/S26 (shared style assets) | Godot docs (MIT project; idea), public the legacy engine user docs (generic description only) |
| T02, T03 | Godot S9 (#87081 offset transform), S10 (#70646 pivot ratio), the legacy engine S29/S30 (timeline caveats) | Godot release notes; easing functions are textbook (Penner easing, public) |
| Y01, Y05, Y06, Y07, Y11, Y12 | Godot D2/D5, S11 (#97495), S1 (GH-116791) | Godot docs and release notes |
| Y13, Y14 | the legacy engine S45 (in-world interaction) | public user docs |
| W02, W03, W04 | the legacy engine S44 (popup anchor), S28 (combo gamepad mode); Godot S9 (#114236 searchable menu) | |
| W09, W10 | the legacy engine S13 to S15 (item/entry split, tree); Godot S1 (GH-115697 sticky), S9 (#112993 drop indicator) | |
| W12 | Godot D6 (BBCode set, custom effects), S9 (#112617) | Godot docs; the legacy engine S12 for style tables |
| W15 | the legacy engine S16, S17, S30 (view model, binding modes, polling anti-pattern) | public user docs; pattern is generic MVVM |
| W16 | the legacy engine S39 (drag operation), Godot D-prior (UNVERIFIED API names) | |
| I01, I05, I06 | inventory plus Godot S10 (#110250) and D3 (initial focus) | |
| I02 | Godot (Shortcut resource, UNVERIFIED), inventory | |
| I03 | the legacy engine S19, S20, S23 (activation stack, per-layer input config) | public user docs, generic description |
| I04 | Godot D3 (focus modes, neighbours), the legacy engine S27 (per-direction rules) | |
| I07 | the legacy engine S24, S25, S21 (action table, hint bar, glyphs) | |
| L01, L02 | inventory (text_scale dead); Godot S1/S2 (GH-119443 pseudolocalisation) | |
| L03 | Godot S11, S9, S8 (AccessKit, landmarks); the legacy engine S37, S38 | AccessKit crate licence to be checked before dependency entry |
| L04 | the legacy engine S34 to S36 | public user docs |
| T04, T09, T10, T11 | the legacy engine S29, S31 to S33, S10 | generic description; no Epic names |
| T12 | Godot S1/S8 (GH-111455, GH-123867) UNVERIFIED | re-check before work |
| T05, T07 | Godot S9 (#113808), S11 (#104731); the legacy engine S10/S11 (draw modes) | |
| E01 to E04 | Godot S1 (GH-113051 docks), S10 (#102889 editor shortcuts), S1/S2 (FileSystem/Tree items) | |
| U01, U05, U06, Y02 to Y04, Y09, Y10, Y16, W01, W05 to W08, W13 to W14, | inventory only (ORIGINAL); W18 from the legacy engine S19/S23 stack idea | |

Notes on reliability inherited from the inputs: the public-documentation note used search digests only (no full-page fetches) and
several sources are 4.27 pages; the permissive-reference note marks `(prior)` rows UNVERIFIED (StyleBox variants, mouse filter,
Shortcut resource, tweens, drag-drop API names, tooltip API, MSDF, IME, GraphEdit). The inventory was a read-only
audit without a cargo run, so "present" means code exists, not that tests were re-run.

## 4. Open questions for the owner (product choices only)

1. Is the visual `.ui.ron` designer (U03, about three large lanes) wanted before DO27/Red-Rouge need it, or do hand
   authoring plus Lane 1's agent loop suffice for the next release?
2. Accessibility: is a screen-reader-grade accessibility tree (L03, adds a dependency) a ship requirement for the
   Steam release, or a post-launch goal? Colour-blind and text-scale are cheap and proposed regardless.
3. World-space UI (Y13): needed for DO27 markers or Red-Rouge diegetic screens soon? It touches the renderer.
4. Should the runtime UI and editor ever share one widget stack (U06, editor built with the runtime toolkit), or does
   ADR-002 (two stacks) stand? This roadmap assumes it stands.
5. Menus and shortcuts (Lanes 5, 7): game-facing too, or editor and tools only? Changes priority of W03 and I02.
6. Is a cap on UI animation in shipping builds wanted (reduced-motion default behaviour)?

## 2026-11-14 parity pass

On 2026-11-14 the owner asked that the best UI features of the top-tier engines be built into Regia's editor UX and
in-game UI designer as part of v0.6.0 "editor complete". The pass merged two behavior catalogs written from public user
documentation into 372 parity rows and checked each against the code: 84 done, 176 partial, 91 missing, 3 covered by
other v0.6.0 jobs and 18 out of scope. ADR-7752 holds the counts by area and status, the scope decisions (code editing
stays in the user's IDE, a desktop-only editor, play from an in-memory copy, no asset marketplace, git through its
command-line tool), the job list and what stays deferred. The behaviors are appended to spec 19 § 10, spec 81 § 11,
spec 18 § 10 and spec 36 § 19, each naming the job that builds it. The matrix itself stays outside the repository,
because its evidence column cites other products' documentation pages (`docs/provenance.md` lists the public page
URLs, ADR-1526). The runtime-UI items here that overlap this roadmap (recursive disable, back handling, markup in
documents, focus styles) are tracked in spec 18 § 10.
