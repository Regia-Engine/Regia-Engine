# UI default design: tokens, widgets, theming and customization (proposed, ADR-274)

Status: **proposed** (2026-10-20); the ADR is still `proposed`, but the design is largely built (truth pass 2026-11-02, ADR-2312). In the code since the v0.2.0 line: `.theme.ron` themes with the built-ins `regia-dark`, `regia-light` and the two high-contrast themes, token resolution (`regia_ui::theme`, ADR-289), the D11 default widgets (`regia_ui::default_widgets`), the drawn tree, data grid and color picker (`regia_ui::parity_widgets`, ADR-2045), icon atlases (D16, `regia_ui::icons`), and the editor theme panel (D10, `regia_editor/src/theme_panel.rs`). The agent pages are `book/agents/ui-theme.md`, `ui-theme-panel.md`, `ui-theme-sheet.md` and `ui-default-widgets.md`. The tables below are the plan record; where a row and the code differ, the code and its book page are the truth. § 10 lists what the 2026-11-11 conformance audit (ADR-5664) found built differently or not built.

This spec defines the **default look and behavior** of Regia Engine's runtime UI (`regia_ui`, specs/18) so that a new
project looks finished out of the box, and defines the **customization mechanism** the owner (and any game) uses to
re-skin it in minutes. It builds on the UI parity roadmap (ADR-273, `docs/specs/ui-parity-roadmap.md`; gap IDs such as T01, W09, I03 below refer to its tables) and on the code audit of v0.1.22.

The companion file [`ui-default-design-mockup.html`](ui-default-design-mockup.html) is a static, self-contained page
that renders every default widget in every state with these tokens as CSS variables. Open it in any browser, flip
dark/light and comfortable/compact, change the accent, radius and font in its side panel, and copy the generated
`.theme.ron` snippet. The CSS variable names map one to one onto the token keys in § 2 (`--color-accent` is
`color.accent`, `--radius-md` is `radius.md`).

Two stacks stay as decided in ADR-002: `regia_ui` for game UI, egui for the editor. § 7 says how they share a theme.

---

## 1. Design principles

1. **Readable at game and at editor scale.** One theme, two densities. `comfortable` (default for games, TV, Deck,
   phones) has a 44 UI px control height and an 18 px body; `compact` (tools, debug panels, the editor) has 28 px and
   13 px. Densities change size tokens only; colors and shapes are shared. The existing `UiDevicePolicy` text floor
   (12 px on Deck) and `ScaleMode` stay in charge of physical size.
2. **Every input device is first class.** Every default widget is operable by gamepad, keyboard, pointer and touch
   with no per-game code. Touch targets are at least 44 UI px in `comfortable`. Hover is a hint, never the only way
   to reach information (tooltips also open on focus). The focus ring shows in navigation mode only (§ 5).
3. **Data, not code.** Every visual value a widget uses comes from a theme token or a widget-type property that can
   reference a token. No widget hard-codes a color, size or duration. Themes are files (§ 6) that load, chain,
   hot-reload and swap at runtime.
4. **Agent-inspectable.** Every token, every resolved widget style and every state can be read and changed through
   the agent protocol (rule 8) and previewed as a screenshot without changing the live game (§ 6.6).
5. **Derive, don't duplicate.** Hover, pressed, selection and disabled colors are derived from base roles by small
   color operations, so changing `color.accent` restyles every accent state. A game overrides a derived token only
   when it wants to break the rule.
6. **Accessible by default.** Text roles meet WCAG 2.x contrast (4.5:1 for body, 3:1 for large text and UI
   boundaries) against the surfaces they sit on, in both schemes; the theme linter (§ 6.6) checks this. Reduced motion,
   text scale and a high-contrast variant are theme-level switches (gap L01).
7. **Calm and legible, then branded.** The default is a neutral, quiet, modern dark UI that does not compete with
   game art, so heavy customization starts from a clean base rather than fighting a strong style.
8. **Cleanroom.** The token model, widget states and state ordering are original or from public sources (WCAG,
   CSS specs, OKLab). No layout, icon or style is copied from another engine (docs/01 § 3.5).

## 2. Default theme tokens

Tokens are named `group.name[-modifier]`. Values below are the **defaults**; the shipped files are
`crates/regia_ui/assets/themes/regia-dark.theme.ron` and `regia-light.theme.ron` (lane D1). Derived tokens are
written as expressions (§ 6.2): `@token` references another token; `lighten`, `darken`, `alpha` and `mix` operate in
OKLab lightness (Ottosson 2020, public), so derivations keep hue.

### 2.1 Color roles

| Token | Role | Dark | Light |
|---|---|---|---|
| `color.bg` | Window / screen backdrop | `#0f1318` | `#e9edf2` |
| `color.surface` | Panels, cards, default widget fill | `#171c23` | `#f6f8fa` |
| `color.surface-raised` | Buttons (secondary), hovered rows, headers | `#1f2630` | `#ffffff` |
| `color.surface-overlay` | Popups, menus, tooltips, dialogs | `#262e3a` | `#ffffff` |
| `color.surface-sunken` | Text fields, tracks, wells | `#0b0e12` | `#e3e8ee` |
| `color.border` | Hairlines, dividers, field outlines | `#2e3744` | `#c9d1db` |
| `color.border-strong` | Hovered outlines, control boundaries (3:1 on surface) | `#5f6b7c` | `#7c8796` |
| `color.text` | Body text | `#e8edf4` | `#18202a` |
| `color.text-muted` | Secondary text, captions, placeholders | `#9aa4b2` | `#566170` |
| `color.text-disabled` | Disabled text and glyphs | `#5d6673` | `#8f99a5` |
| `color.accent` | Primary action, selection, slider fill, active tab | `#3d9df0` | `#1d6fc4` |
| `color.accent-hover` | `lighten(@color.accent, 0.06)` | `#5ab0ff` | `#3382d8` |
| `color.accent-pressed` | `darken(@color.accent, 0.08)` | `#1c84d5` | `#0057a5` |
| `color.on-accent` | Text / icons on an accent fill | `#0b1220` | `#ffffff` |
| `color.selection` | Selected row / text selection: `alpha(@color.accent, 0.28)` (light: `alpha(@color.accent, 0.2)`) | `#3d9df047` | `#1d6fc433` |
| `color.focus-ring` | Keyboard/gamepad focus ring | `#9cd3ff` | `#0b4f9e` |
| `color.danger` | Destructive actions, errors | `#e5534b` | `#c4372f` |
| `color.on-danger` | Text on danger fill | `#1a0806` | `#ffffff` |
| `color.warning` | Warnings | `#e0a33a` | `#975a00` |
| `color.success` | Success, "on" confirmations | `#3fb950` | `#1f7a35` |
| `color.info` | Informational toasts: `@color.accent` | = accent | = accent |
| `color.scrim` | Modal backdrop | `#000000a6` | `#0f131866` |
| `color.shadow` | Shadow color | `#000000` | `#0f1318` |

Derived values in this table are what the shipped expressions compute (the loader is the source of truth;
ADR-277 § 4, reconciled by ADR-290): OKLab lightness steps of the accent, not hand-picked hex.

Contrast checks (computed with the WCAG 2.x relative-luminance formula; lane D1 turns them into a test):

| Pair | Dark | Light | Required |
|---|---|---|---|
| `text` on `surface` | 14.6 | 15.4 | 4.5 |
| `text-muted` on `surface` | 6.8 | 5.9 | 4.5 |
| `text-muted` on `surface-overlay` (dark) / `surface-sunken` (light) | 5.4 | 5.1 | 4.5 |
| `on-accent` on `accent` | 6.5 | 5.1 | 4.5 |
| `on-danger` on `danger` | 5.2 | 5.3 | 4.5 |
| `accent` on `surface` (slider fill, tab indicator) | 5.9 | 4.8 | 3 |
| `danger` / `warning` / `success` on `surface` | 4.6 / 7.7 / 6.7 | 5.0 / 5.2 / 5.1 | 4.5 |
| `border-strong` on `surface` (control boundary) | 3.2 | 3.4 | 3 |
| `focus-ring` on `bg` | 11.7 | 6.8 | 3 |
| `text-disabled` on `surface` | 3.0 | 2.7 | exempt (WCAG 1.4.3) |

`color.border` is decorative (dividers, field outlines on top of a fill change) and is not held to 3:1; controls
whose only boundary is a line use `border-strong`.

Mapping from today's `ThemeColors` (10 slots, v0.1.22) is exact for the slots that exist: `background`→`color.bg`,
`surface`→`color.surface`, `surface_hi`→`color.surface-raised`, `primary`→`color.accent`, `on_primary`→
`color.on-accent`, `text`→`color.text`, `text_dim`→`color.text-muted`, `border`→`color.border`, `overlay`→
`color.scrim`, `danger`→`color.danger`. The default accent and on-accent are the v0.1.22 values, so existing games
keep their accent.

A third built-in variant, `regia-high-contrast` (child of dark, gap L01): `text` `#ffffff`, `text-muted` `#d0d6de`,
`border` `#8d98a7` (5.9:1 on surface), `border.width.hairline` 2, `focus-ring` `#ffd400` (13:1 on bg),
`border.width.focus` 3.

### 2.2 Spacing, radius, border

| Scale | Tokens (UI px) |
|---|---|
| Spacing `space.*` (4 px base) | `0`=0, `1`=2, `2`=4, `3`=8, `4`=12, `5`=16, `6`=24, `7`=32, `8`=48, `9`=64 |
| Radius `radius.*` | `none`=0, `sm`=4, `md`=8 (v0.1.22 default), `lg`=12, `xl`=16, `pill`=9999 |
| Border `border.width.*` | `hairline`=1, `thick`=2, `focus`=2 (ring width), `focus-offset`=2 (gap between widget and ring), `focus-inset`=-2 (the ring offset of list rows: minus the ring width, so the ring lies inside the row; high contrast -3; ADR-372) |

### 2.3 Type

Font stack (all in-repo, SIL OFL 1.1, listed in `THIRD_PARTY_NOTICES.md`):

| Token | Default | Files |
|---|---|---|
| `font.body` | `Inter` | `crates/regia_ui/assets/fonts/Inter-{Regular,Bold,Italic}.ttf` |
| `font.display` | `@font.body` (Inter Bold) | same |
| `font.mono` | `JetBrains Mono` | today only a Latin woff2 under `book/theme/fonts/` (OFL); lane D4 adds the TTF to `regia_ui/assets/fonts` with its licence; until then the platform monospace |
| fallback chain | Noto Naskh Arabic, Noto Sans SC, Noto Sans KR, Noto Color Emoji (existing per-script chain in `text.rs`) | existing |

Inter ships as Regular, Bold and Italic statics, so the type scale uses weights 400 and 700 only. A semibold (600)
would need the Inter variable font; that is an owner choice (§ 9) because it adds ~300 KiB.

Type scale (`type.<role>.size` / `.line` / `.weight`, UI px):

| Role | Comfortable | Compact | Weight | Use |
|---|---|---|---|---|
| `type.caption` | 14 / 20 | 11 / 16 | 400 | hints, footnotes, shortcut labels |
| `type.body` | 18 / 26 | 13 / 18 | 400 | default text, inputs, list rows |
| `type.label` | 18 / 24 | 13 / 18 | 700 | buttons, tabs, field labels |
| `type.title` | 24 / 32 | 16 / 22 | 700 | dialog titles, panel headers |
| `type.headline` | 32 / 40 | 20 / 28 | 700 | screen titles |
| `type.display` | 48 / 56 | 28 / 36 | 700 | main menu, splash |
| `type.mono` | 16 / 24 | 12 / 18 | 400 | numbers, logs, codes |

`AccessibilitySettings.text_scale` multiplies every `type.*.size` and `.line` (gap L01), then the device floor applies.

### 2.4 Sizes and density

| Token | Comfortable | Compact |
|---|---|---|
| `size.control.sm` / `md` / `lg` (control height) | 32 / 44 / 56 | 22 / 28 / 36 |
| `size.icon.sm` / `md` / `lg` | 16 / 24 / 32 | 12 / 16 / 20 |
| `size.touch-min` | 44 | 28 (pointer only) |
| `size.pad-x.sm` / `md` / `lg` (control inline padding) | 12 / 16 / 24 | 6 / 8 / 12 |
| `size.row` (list/tree row) | 44 | 24 |
| `size.scrollbar` | 10 (hover 14) | 8 (hover 10) |

### 2.5 Elevation

Elevation maps to the existing `Shadow` component (offset, blur, color; outer box-shadow semantics), no spread, so it
renders today without a shader change.

| Token | Dark (x, y, blur → alpha of `color.shadow`; x is always 0 and has no token) | Light | Used by |
|---|---|---|---|
| `elevation.0` | none | none | flat surfaces |
| `elevation.1` | 0, 1, 2 → 0.35 | 0, 1, 2 → 0.10 | raised buttons, cards |
| `elevation.2` | 0, 4, 12 → 0.45 | 0, 4, 12 → 0.14 | menus, dropdowns, tooltips, toasts |
| `elevation.3` | 0, 12, 32 → 0.55 | 0, 12, 32 → 0.20 | dialogs |

Dark surfaces also step up in lightness with elevation (surface → raised → overlay), which reads better than shadow
alone on dark backgrounds.

### 2.6 Motion

| Token | Default | Use |
|---|---|---|
| `motion.instant` | 0 ms | reduced-motion target |
| `motion.fast` | 80 ms (v0.1.22 `transition`) | hover and press color changes |
| `motion.base` | 160 ms | toggles, checkbox, tab indicator, fold open |
| `motion.slow` | 240 ms | popups, dialogs, toasts in |
| `motion.page` | 320 ms | screen/page transitions |
| `ease.standard` | cubic-bezier(0.2, 0, 0, 1) | most transitions |
| `ease.enter` | cubic-bezier(0, 0, 0, 1) | things appearing |
| `ease.exit` | cubic-bezier(0.3, 0, 1, 1) | things leaving (use 0.75 × duration) |
| `ease.linear` | linear | progress, indeterminate loops |

Reduced motion: all durations become `motion.instant` except opacity fades, which cap at `motion.fast`; indeterminate
progress becomes a static pulse-free bar with a moving text dot (no sweeping). Today only color transitions exist;
the rest need the tween lane (T02, ADR-273 Lane 3).

### 2.7 Icon style

- Grid 24×24 UI px with a 2 px safe margin; drawn at 16, 20, 24, 32.
- Outline style, 1.75 px stroke at 24 (scaled with size), round caps and joins, 2 px corner radius on rectangles.
- A filled variant only for "selected/active" glyphs (tab icons, toggled tool buttons).
- Single color: icons tint with the text color of their context (`icon_color` defaults to `@color.text`, muted
  icons `@color.text-muted`), so themes recolor them for free.
- Shipped as a UI sprite atlas (gap T13) and addressable from text (`<icon=name>`), not by numeric image ids.
- Default set: about 60 glyphs (chevrons, check, close, plus, minus, search, gear, info, warning, error, eye,
  lock, play/pause, folder, file, drag handle, more, menu, arrows, gamepad face buttons). Source is an owner choice
  (§ 9 Q3): recommended a subset of a permissive set (ISC/MIT) with attribution under ADR-086, else drawn in-house.
  The mockup's inline SVG glyphs are drawn for this spec.

## 3. Default widget set

Every widget resolves its look from its **widget type** in the theme (§ 6.3), for example `Button`, then its
**variations** (`Button.primary`), then per-node overrides. States are composed in this fixed order, later wins per
property: `normal` → `size` (sm/md/lg) → `selected` → `hover` → `pressed` → `error` → `focused` → `disabled`.
`focused` only adds the ring (and nothing else by default) so focus never hides other state. `disabled` wins over
everything and suppresses hover/pressed. Hover is not applied in touch input mode.

Common defaults: label `type.label`, inline padding `size.pad-x.<size>`, height `size.control.<size>`, radius
`radius.md`, transition `motion.fast` / `ease.standard`; focus ring `border.width.focus` in `color.focus-ring` drawn
`border.width.focus-offset` outside the widget's border box, following its radius + offset; disabled = `opacity 0.45`
over the normal look plus `color.text-disabled` for text (one rule, so disabled works for every custom variation).

Status: P present today, Pt partial, A absent (from the inventory); Gap names the ADR-273 row that delivers it.

| Widget | Variants / sizes | Normal | Hover | Pressed | Focused | Disabled | Selected | Error | St / Gap |
|---|---|---|---|---|---|---|---|---|---|
| **Button** | `primary`, `secondary` (default), `ghost`, `danger`, `icon` (square); sm/md/lg | primary: fill `accent`, text `on-accent`; secondary: fill `surface-raised`, 1 px `border`, text `text`; ghost: no fill, text `text`; danger: fill `danger`, text `on-danger` | primary `accent-hover`; secondary fill `lighten(surface-raised,.04)`, border `border-strong`; ghost fill `alpha(text,.08)` | primary `accent-pressed`; secondary `surface-sunken`; translate y +1 px (T03) | ring | common rule | toggle-button: fill `selection`, border `accent` | danger outline 1 px `danger` | P (variants via classes) / W07 for toggle-button, I01 for repeat-on-hold |
| **Toggle (switch)** | sm/md | track 40×24 (`md`), fill `surface-sunken`, border `border-strong`; knob 18 px `text-muted` | border `text-muted` | knob widens 2 px | ring around track | common | on: track `accent`, knob `on-accent`, knob slides `motion.base` | track border `danger` | P / tri-state W07 |
| **Checkbox** | sm/md | 20×20 box, `radius.sm`, fill `surface-sunken`, border `border-strong` | border `text-muted` | fill `surface-raised` | ring | common | checked: fill `accent`, check glyph `on-accent`; indeterminate: dash | border `danger` + message below | A (no checkbox widget or class; the theme lint names the type only) / W07 |
| **Radio** | sm/md, group | 20 px circle, same fills as checkbox | as checkbox | as checkbox | ring | common | dot 8 px `on-accent` on `accent` | border `danger` | P behavior (`RadioButton`, `default_widgets.rs`; drawn as a button, no `Radio` theme type) / W07 |
| **Slider** | horizontal, vertical; with ticks; range | track 4 px `surface-sunken`, fill `accent`, thumb 20 px `text` with `elevation.1` | thumb grows to 22 | thumb `accent-pressed` ring 6 px `alpha(accent,.25)` | ring around thumb | common | - | track fill `danger` | P horizontal / W08 vertical, range, ticks |
| **Spin box / number field** | with unit suffix, drag-to-scrub | field look + stepper buttons (ghost icon buttons) | stepper hover as ghost | stepper pressed | field ring | common | - | field error | P behavior (`SpinBox`, `default_widgets.rs`) / W06 |
| **Text input** | single, multi-line, search (leading icon, clear button), secret | fill `surface-sunken`, border `border`, placeholder `text-muted`, caret `accent` 2 px | border `border-strong` | - | border `accent` + ring | common | text selection `selection` | border `danger`, helper text `danger` below in `type.caption` | Pt / W01 |
| **Dropdown** | sm/md/lg; searchable | as secondary button with trailing chevron | as secondary | as secondary | ring | common | open: chevron flips, popup = menu look | as text input | Pt / W02, W04 |
| **List** (virtualized) | single, multi-select; rows sm/md; tile mode | rows `size.row`, text `type.body`, subtitle `text-muted` | row fill `alpha(text,.05)` | row fill `alpha(text,.08)` | ring inset (−2 px, so it is not clipped) | common | row fill `selection` + 3 px leading bar `accent` | row text `danger` + icon | P fixed rows / W09 |
| **Tree** | virtualized; expand chevrons; indent 16 (comfortable) / 12 (compact) | list rows + chevron `text-muted` | as list | as list | as list | as list | as list | as list | P (`TreeView`, `parity_widgets.rs`; indent fixed at 16, the compact `size.tree-indent` 12 is not read yet) / W10 |
| **Tabs** | top, bottom; closable; scrollable strip | text `text-muted`, no fill | text `text` | fill `alpha(text,.05)` | ring | common | text `text`, 2 px indicator `accent` under the label, slides `motion.base` | dot `danger` badge | P / W14 |
| **Menu / popup / context menu** | menu bar, popup, context; checkable items, submenus, search | panel `surface-overlay`, 1 px `border`, `radius.md`, `elevation.2`, padding `space.2`; item rows `size.control.sm`, shortcut text `text-muted` right-aligned | item fill `alpha(text,.07)` | item fill `alpha(text,.10)` | item fill `selection` (nav mode replaces hover) | item text `text-disabled` | check glyph on checkable | - | P (`menu.rs`; `Menu`, `MenuBar`, `MenuItem` theme types) / W02, W03 |
| **Tooltip** | text; rich | `surface-overlay`, `type.caption`, padding `space.2`/`space.3`, `radius.sm`, `elevation.2`; delay 500 ms (pointer), 800 ms (focus) | - | - | opens on focus too | - | - | - | Pt / W13 |
| **Dialog (modal)** | info, confirm, destructive | `surface-overlay`, `radius.lg`, `elevation.3`, padding `space.6`; title `type.title`; scrim `color.scrim`; buttons right-aligned (game: confirm on the right, matching A position) | - | - | first focus: the safe action | - | - | destructive: confirm button `danger` | P (Modal) / I03, I06 |
| **Progress** | determinate bar, indeterminate bar, spinner (16/24/32) | track 6 px `surface-sunken`, `radius.pill`, fill `accent` | - | - | - | fill `text-disabled` | - | fill `danger` | P determinate / W08 |
| **Scroll** | vertical, horizontal; auto-hide | thumb `alpha(text,.25)`, `radius.pill`, width `size.scrollbar`; no track | thumb `alpha(text,.40)`, grows | thumb `alpha(text,.55)` | - | hidden | - | - | Pt / Y10 |
| **Split** | horizontal, vertical; min sizes | divider 1 px `border`, 8 px hit area | divider 2 px `border-strong` + resize cursor (T08) | divider `accent` | divider `accent` (nav: left/right moves 5 %) | - | - | - | P behavior (`Split`, `default_widgets.rs`; no 5 % keyboard step) / Y05 |
| **Foldable section** | single, exclusive group | header row `type.label` + chevron, body padding `space.4` | header fill `alpha(text,.05)` | header fill `alpha(text,.08)` | ring on header | common | open: chevron rotates 90° `motion.base` | header `danger` | P behavior (`Foldable`, `default_widgets.rs`) / Y06 |
| **Toast** | info, success, warning, danger | `surface-overlay`, leading 3 px bar in role color, `elevation.2`, auto-hide 4 s | pauses timer | - | - | - | - | - | Pt (`Toast`, `default_widgets.rs`: no role bar, no hover pause, styled as `Panel.surface`) / W05 |

Sizes: every control has `sm` / `md` (default) / `lg` mapped to `size.control.*`, `size.pad-x.*` and a type role
(`sm` uses `type.caption` size with `label` weight). The `comfortable`/`compact` density switch rescales all three.

## 4. Layout vocabulary

`regia_ui` layout is CSS flex/grid on taffy. The default design keeps that engine and names a small vocabulary on
top of it, so `.ui.ron` authors and agents think in containers instead of raw CSS.

| Concept | Meaning | `regia_ui` today | Needed (gap) |
|---|---|---|---|
| `Row` / `Column` | flex with gap | P (`direction`, `gap`) | `.ui.ron` v2 sugar (U05) |
| `Stack` | children overlap (absolute, inset 0) | P via absolute + inset | sugar (U05) |
| `Grid` | tracks + gap | P, max 16 tracks, auto-flow only | placement, spans, areas, raise cap (Y02) |
| `Wrap` | flex wrap | P (bool) | `align_content`, wrap-reverse (Y04) |
| `Scroll` | clipped scroll area | P | horizontal bar, thumb drag, scroll into view (Y10) |
| `Split` | two panes + draggable divider | P (`default_widgets::Split`) | Y05 |
| `Foldable` | collapsible section, exclusive groups | P (`default_widgets::Foldable`) | Y06 |
| `Panel` | surface fill + padding + radius | P (class `panel`/`surface`) | token-driven type (D2) |
| `AspectFit` | keep ratio: fit, cover, width-drives-height | Pt (`aspect_ratio`) | Y07 |
| `SafeArea` | pads by notch/overscan/title-safe | P (`SafeAreaPadding`, `UiDevicePolicy`) | designer preview (U04) |
| `Spacer` | flexible empty space | Pt (grow on an empty node) | sugar (U05) |

**Size flags.** Authors use three words, mapped to flex: `fill` (grow 1, shrink 1, stretch on cross axis), `fit`
(content size, no grow), `fixed(px)`; plus `min`/`max`. Viewport units and content keywords are Y03.

**Anchors.** Absolute children keep CSS `inset`; on top, anchor presets (`top-left`, `top`, `top-right`, `left`,
`center`, `right`, `bottom-left`, `bottom`, `bottom-right`, `fill`, `top-wide`, `bottom-wide`, `left-tall`,
`right-tall`) with offsets and a `pivot_ratio` (Y01; built: `regia_ui::anchors`, ADR-1842).

**Safe zones.** HUD roots set `safe_area: true` by default in the HUD template; menus inherit it from the screen root.
Title-safe margin from `UiDevicePolicy`. Designer and `ui.theme_preview` simulate device safe areas (U04).

**DPI scaling.** `ScaleMode::ScaleWithScreen { reference: 1920×1080, match: 0.5 }` stays the game default;
`HANDHELD` for Deck; density tokens choose comfortable/compact; integer scale and a piecewise DPI curve are Y16.

## 5. Focus and navigation model

- **Input modes** (exists as `UiInputMode`, extended with touch): `Pointer`, `Navigation` (keyboard or gamepad),
  `Touch`. The last device used sets the mode. The focus ring shows only in `Navigation` (I05). Hover styles are off
  in `Touch`. Device glyphs in hint bars follow the mode (I07).
- **Focusable by default:** buttons, toggles, checkboxes, radios, sliders, inputs, dropdowns, tabs, list/tree rows,
  menu items, fold headers, split dividers. Not focusable: labels, images, panels. Per-node `focus_mode`
  (`all`, `click`, `none`) overrides (I04).
- **Directional neighbors:** geometric search (existing `nav_score`) with optional explicit `NavLinks` per direction,
  and per-direction rules `escape` (leave the container), `stop`, `wrap`, `target(node)` (I04). Containers default to
  `escape`; lists and menus default to `stop` at ends; tab strips `wrap`.
- **Tab order:** reading order, with explicit `tab_index` override (I04).
- **Input layers:** a stack (I03). Each layer declares its input mode (`ui_only`, `ui_and_game`, `game_only`),
  default focus and restores the previous layer's focus and mode on pop. Dialogs, menus and popups each push a layer.
- **Initial focus:** every screen and layer names a default focus; if none, the first focusable in reading order gets
  focus on the first navigation input (I06).
- **Gamepad defaults:** A / Cross = activate; B / Circle = back / close layer; LB/RB = previous/next tab; LT/RT =
  page in long lists; Start = pause/menu; Y = secondary action (shown in the hint bar); D-pad/left stick move focus
  with repeat (existing ADR-213 timings); a focused slider takes left/right.
- **Keyboard defaults:** Tab/Shift+Tab, arrows, Enter/Space activate, Esc back, Home/End in lists, letters type-ahead
  in lists and dropdowns (W04), Ctrl+PgUp/PgDn tabs.
- **Pointer and touch:** click/tap activates; right click or long press (500 ms) opens a context menu; drag scrolls
  on touch; tooltips on hover delay or on focus.
- **Gamepad cursor:** an optional software cursor for pointer-only screens (maps, inventories), moved by the right
  stick, snapping to focusables when near (I10). Off by default.
- **Shortcuts:** a registry `UiShortcut { chord, action, scope }` with display labels and conflict reports; menus and
  tooltips show the chord in `type.caption` `text-muted` (I02).

## 6. Customization mechanism

### 6.1 Theme files

- **Format:** RON, extension `.theme.ron`, one theme per file, under `assets/ui/themes/` (games) and
  `crates/regia_ui/assets/themes/` (built-ins). RON matches every other Regia Engine authoring format (`.ui.ron`,
  `.material.ron`). The agent protocol carries the same structure as JSON (field names identical).
- **Frozen-format candidate:** `regia.theme`, `format_version: 1`, added to `specs/12` § 4a with
  `docs/formats/theme-ron.md`, an example, a JSON Schema and its hash in `docs/formats/format-hashes.ron`
  (`cargo xtask check-formats --update`). Proposed freeze: when lane D6 lands (v0.2.x).
- **Migrator rules (specs/12 § 4a):**
  - `format_version` 0 = today's serde shape of `UiTheme` (`name`, `colors` with 10 slots, `radius`, `font_px`) and
    `ThemeClasses` lists. `UiTheme` is already listed as frozen since M7, so a v0→v1 migrator ships with D1: slots map
    per § 2.1, `radius` → `radius.md`, `font_px` → `type.body.size`, each class becomes a variation of the widget type
    it styles (`button` → `Button`, `primary` → `Button.primary`, unknown names → a variation of `Panel`).
  - Adding a token key or widget property is **additive**: no version bump; old files keep defaults for new keys.
  - Unknown keys are kept on load and save (round trip), reported as warnings by the linter, never an error.
  - Renaming or changing the meaning or unit of a key bumps `format_version` and requires a migrator in
    `ThemeMigrators` (same mechanism as `UiMigrators`), run by `regia upgrade-project`, tested on DO27 content.
  - Removing a key is a rename to nothing: same rule.

### 6.2 File shape

```ron
(
    format_version: 1,
    name: "regia-dark",
    extends: None,                 // Some("regia-dark") for a child theme
    scheme: dark,                  // dark | light: picks prefers-color-scheme defaults
    density: comfortable,          // comfortable | compact
    tokens: {
        "color.accent": "#3d9df0",
        "color.accent-hover": "lighten(@color.accent, 0.06)",
        "color.selection": "alpha(@color.accent, 0.28)",
        "radius.md": 8,
        "font.body": "Inter",
        "motion.fast": "80ms",
        "ease.standard": "cubic-bezier(0.2, 0, 0, 1)",
        // ... every token of § 2
    },
    fonts: {
        "Inter": (files: ["fonts/Inter-Regular.ttf", "fonts/Inter-Bold.ttf", "fonts/Inter-Italic.ttf"]),
    },
    types: {
        "Button": (
            props: (height: "@size.control.md", padding_x: "@size.pad-x.md", radius: "@radius.md",
                    background: "@color.surface-raised", border: (1, "@color.border"), text: "@type.label",
                    text_color: "@color.text", transition: ("@motion.fast", "@ease.standard")),
            hover:   (background: "lighten(@color.surface-raised, 0.04)", border: (1, "@color.border-strong")),
            pressed: (background: "@color.surface-sunken", translate: (0, 1)),
            sizes: { sm: (height: "@size.control.sm", padding_x: "@size.pad-x.sm"),
                     lg: (height: "@size.control.lg", padding_x: "@size.pad-x.lg") },
        ),
        // Toggle, Checkbox, Radio, Slider, SpinBox, TextInput, Dropdown, List, Tree, Tabs, Menu,
        // Tooltip, Dialog, Progress, Scroll, Split, Foldable, Toast, Panel, Label
    },
    variations: {
        "Button.primary": (extends: "Button",
            props: (background: "@color.accent", text_color: "@color.on-accent", border: (0, "#0000")),
            hover: (background: "@color.accent-hover"), pressed: (background: "@color.accent-pressed")),
        "Button.danger": (extends: "Button.primary",
            props: (background: "@color.danger", text_color: "@color.on-danger"),
            hover: (background: "lighten(@color.danger, 0.05)"), pressed: (background: "darken(@color.danger, 0.08)")),
        "Button.ghost":  (extends: "Button", props: (background: "#0000", border: (0, "#0000"))),
    },
    sounds: {},                    // reserved for theme audio items (T12); empty by default
)
```

- **Value grammar:** colors `#rgb`, `#rrggbb`, `#rrggbbaa`; numbers are UI px; durations `"80ms"`/`"0.08s"`;
  references `"@key"`; color operations `lighten(c, x)`, `darken(c, x)`, `alpha(c, a)`, `mix(c1, c2, t)` on OKLab
  (x in lightness units 0..1). No other expressions: themes stay data. Reference cycles and unknown references are
  load errors with the key path (the previous good theme stays active).
- **Widget properties** (superset of today's six `VisualOverrides`, gap T01): `background` (color or gradient as
  `Background` supports), `border` (width, color), `radius` (one or four), `padding`, `padding_x`, `height`,
  `min_width`, `gap`, `text` (type role), `font`, `font_size`, `text_color`, `icon_color`, `shadow` (elevation token
  or explicit), `opacity`, `focus_ring` (color, width, offset), `translate` and `scale` (visual only, T03),
  `transition` (duration, easing), `cursor` (T08). Parts address sub-elements: `Slider.thumb`, `Slider.track`,
  `Slider.fill`, `Toggle.knob`, `Scroll.thumb`, `Tabs.indicator`, `List.row`, `Menu.item`.

### 6.3 Resolution and precedence

For one property of one node, the first hit wins:

1. per-node override (`ThemeOverride` component, or `style:` in `.ui.ron`),
2. the node's variation chain, nearest first (`Button.danger` → `Button.primary` → `Button`), each state-composed
   in § 3 order,
3. the widget type's defaults,
4. the theme's own defaults for that property group,

and every `@token` resolves through the **theme chain**: the active theme's tokens, then its `extends` parent, up to
a built-in. A **subtree theme** (`ThemeScope { theme }` on a node) resolves its descendants against another theme
(useful for a light in-world terminal inside a dark HUD). The type-variation and per-subtree ideas come from Godot's
public theme documentation (MIT project, idea only); the merge rules are ours.

Resolution is cached per (theme generation, type, variation, state, size); a theme change bumps the generation and
re-resolves only themed nodes once, with no per-frame cost while idle (fits ADR-076 damage tracking).

### 6.4 Runtime swap

`UiThemes` holds every loaded theme by name; `ActiveTheme` selects one (and density, and high-contrast). Swapping is
one resource write: the next frame re-resolves, and colors cross-fade over `motion.base` unless reduced motion is on.
Games expose dark/light/high-contrast and density in their settings screen through the existing settings resource.

### 6.5 Hot reload

In the editor and desktop dev builds, the asset watcher reloads a changed `.theme.ron` (and every child theme that
extends it) within one frame. Parse or reference errors keep the last good theme, log the key path and raise an
editor toast. Shipping builds load themes from the pak and do not watch.

### 6.6 Agent surface (rule 8)

New page `book/agents/ui-theme.md` and `book/COVERAGE.md` rows. Mutations need the `edit_scene` capability;
`persist: true` needs `edit_assets`.

| Method | Params | Returns |
|---|---|---|
| `ui.theme_list` | - | themes `[{ name, extends, scheme, path }]`, active, density |
| `ui.theme_get` | `theme?`, `keys?` (glob, e.g. `"color.*"`), `resolved?` (bool), `type?` (e.g. `"Button.primary"`), `state?` | `{ name, extends, format_version, tokens, types?, resolved? }`: raw expressions and resolved values side by side |
| `ui.theme_set` | `theme?`, `tokens?` map, `types?`, `variations?`, `persist?` (write file), `reset?` | `{ changed: [keys], warnings: [lint findings] }`; applies live next frame |
| `ui.theme_select` | `theme`, `density?`, `high_contrast?` | previous selection |
| `ui.theme_preview` | `tokens?` / `theme?` overrides (not applied live), `page?` (default `"theme-sheet"`, or a `.ui.ron` path), `size?` `[w,h]`, `scheme?`, `density?`, `device?` (safe-area profile) | screenshot path and PNG bytes via the existing offscreen capture path (ADR-202); live UI unchanged |
| `ui.theme_lint` | `theme?` | contrast failures (pair, ratio, required), unreachable references, unknown keys, touch targets under `size.touch-min` |
| `ui.theme_reload` | `theme?` | reload result with error key paths |

`wait_until` predicate: `ui.theme_generation >= n`.

### 6.7 Worked example: the owner re-skins the default in minutes

Goal: orange accent, sharper corners, a branded body font, for one game.

1. Create `assets/ui/themes/my-game.theme.ron`:

   ```ron
   (
       format_version: 1,
       name: "my-game",
       extends: Some("regia-dark"),
       tokens: {
           "color.accent": "#e8793a",
           "color.on-accent": "#140a04",
           "radius.sm": 2, "radius.md": 3, "radius.lg": 4,
           "font.body": "Rajdhani",
       },
       fonts: { "Rajdhani": (files: ["fonts/Rajdhani-Regular.ttf", "fonts/Rajdhani-Bold.ttf"]) },
   )
   ```

   Hover, pressed, selection, slider fill, active tab, toggle-on and focus visuals all follow, because they are
   derived from `color.accent` and the radius tokens. Every unlisted token comes from `regia-dark`.
2. Select it: `Regia.toml` `[ui] theme = "my-game"`, or at runtime `ui.theme_select { theme: "my-game" }`.
3. Check it: `ui.theme_lint` (contrast of `on-accent` on the new accent is reported if it fails), then
   `ui.theme_preview { theme: "my-game", density: "compact" }` for the full widget sheet, and the same with
   `scheme: "light"` if the game ships a light theme.
4. Iterate live: with the editor or a dev build running, saving the file restyles the game within a frame; or have an
   agent do it: `ui.theme_set { tokens: { "radius.md": 0 } }`, screenshot, compare, `persist: true` when happy.

Before writing a file, the owner can try the same three changes in the HTML mockup's side panel and copy its
generated snippet.

## 7. Editor integration

- **Stays egui** (ADR-002): the editor shell, docks, inspector, outliner, content browser, graph editors, timelines,
  logs. Its look stays the ADR-229 `ThemeTokens` (JSON).
- **Uses `regia_ui`:** anything that shows game UI: the theme sheet and theme preview, the future `.ui.ron` designer
  canvas (U03, rendered offscreen by `regia_ui` into an egui image), the runtime UI inspector overlay (U02, E04).
- **Shared theme, one direction:** a `.theme.ron` can also skin the editor. Lane D5 adds
  `ThemeTokens::from_ui_roles(&roles)`: `ground`/`raised` ← `color.surface-overlay`, `panel` ← `color.surface`,
  `well` ← `color.surface-sunken`, `line` ← `color.border`, `muted` ← `color.text-muted`, `text` ← `color.text`,
  `strong` ← `lighten(@color.text, 0.05)`, `signal` ← `color.accent`, `on_signal` ← `color.on-accent`, `warn` ←
  `color.warning`, `error` ← `color.danger`; overlay/gizmo accents keep their ADR-229 defaults unless the theme has an
  `editor.*` token group. Opt-in through editor preferences (`theme_source = "ui:<name>"`); the ADR-229 default look
  is unchanged until the owner says otherwise (§ 9 Q4).
- **Theme panel** (lane D10, egui): token list grouped by § 2 sections with color pickers and sliders, a live
  `regia_ui` theme-sheet preview (dark/light, comfortable/compact, device frame), lint results, save to file.
  Editor-only crate; never a runtime dependency.

## 8. Implementation roadmap

Common preamble for every lane: the ADR-273 lane preamble (read AGENTS.md, docs/01, specs/18, specs/23, this file;
never read `CLEANROOM-DENY.txt` paths; tests first; fmt, clippy `-D warnings`, crate tests, `cargo xtask preflight
--changed`; Android check; agent page and COVERAGE row; provenance row; no `ui-overlay.wgsl` or batcher edits unless
flagged HOT). Sizes as ADR-273: S ≤ 3 lane-hours, M 3 to 8, L 8 to 20. Prov: OR = ORIGINAL, MIT = PORTABLE-MIT-WITH-
ATTRIBUTION (idea or port), CR = CLEANROOM-REIMPLEMENT.

**D1 to D5 are independent of each other and can run in parallel** (disjoint files; D2 and D1 meet only at the
`mod` line in `lib.rs`).

| Lane | What | Files likely touched | Acceptance tests | Size | Prov | Deps |
|---|---|---|---|---|---|---|
| **D1** Theme tokens and `.theme.ron` v1 | Token map, value grammar (refs, OKLab ops, durations, easing), `extends` chain, load errors with key paths, v0→v1 migrator from today's `UiTheme`/`ThemeClasses`, built-in `regia-dark`, `regia-light`, `regia-high-contrast` files with § 2 values; format docs and hash | new `crates/regia_ui/src/theme/{tokens.rs,file.rs,color_ops.rs,migrate.rs}`, `crates/regia_ui/assets/themes/*.theme.ron`, `docs/formats/theme-ron.md` + example + schema, `docs/specs/12-assets.md` § 4a, `format-hashes.ron` | parse every built-in; chain override test; cycle and unknown-ref errors; OKLab op golden values; v0 file round-trips through migrator to equal resolved colors; WCAG ratios of § 2.1 asserted in a test; `check-formats` | M | OR (+ OKLab public paper; Godot variation idea MIT) | none |
| **D2** Widget style surface | Widen `VisualOverrides` to the § 6.2 property set; add `selected` and `error` states and the § 3 state order; `sm/md/lg` sizes; density; disabled rule; parts addressing | `crates/regia_ui/src/{theme.rs,widgets.rs,node.rs}` (adds `Selected`, `Invalid` markers) | state-order table test (each pair of states); size test; old class tests still pass; disabled suppresses hover/pressed | M | OR (+ Godot type-variation idea MIT) | none |
| **D3** Theme sheet page and baseline goldens | A `theme_sheet` page in `ui_gallery` showing every existing widget in every state and size, both built-in schemes; offscreen goldens; parity checklist against the HTML mockup | `samples/ui_gallery/src/{theme_sheet.rs,gallery.rs}`, `samples/ui_gallery/tests/offscreen_gallery.rs`, goldens | goldens blessed with reason "baseline before default-design lanes"; page reachable by gamepad only | S | OR | none |
| **D4** Icon atlas and mono font | UI sprite-atlas asset (gap T13), ~60-glyph default icon set per § 2.7 (source per § 9 Q3), `<icon=name>` resolves through it; JetBrains Mono TTF (OFL) added to the default font set | `crates/regia_ui/src/{icons.rs,text.rs}`, `crates/regia_ui/assets/{icons,fonts}`, `docs/ported-sources.toml` or licence file, `THIRD_PARTY_NOTICES` via `cargo xtask notices`, fonts README | icon golden at 16/24/32; tinting test; `notices --check`; font coverage test for mono | M | OR or MIT/ISC with attribution | none |
| **D5** Editor role bridge | `ThemeTokens::from_ui_roles` over a role-name map (§ 7 table), editor preference `theme_source`, `editor.set_theme { ui_theme }` | `crates/regia_editor/src/theme.rs`, `book/agents/theme.md` | default look unchanged test still passes; mapping test with the dark roles; offscreen editor capture with a UI theme applied | S | OR | none |
| D6 Resolver wiring | `apply_themes` resolves from D1 tokens + D2 props: variation chains, `ThemeOverride`, `ThemeScope`, `ActiveTheme` swap with cross-fade, resolution cache by generation | `crates/regia_ui/src/theme.rs`, `theme/resolve.rs` | precedence table test; subtree theme test; swap re-resolves once (change-detect count); D3 goldens re-blessed for the new default (intended) | M | OR | D1, D2 |
| D7 Agent surface and hot reload | `ui.theme_*` methods (§ 6.6), `wait_until` predicate, hot reload with last-good fallback, lint (contrast, refs, touch size) | `crates/regia_ui/src/theme/{agent.rs,lint.rs}`, agent bridge from ADR-273 Lane 1, `book/agents/ui-theme.md` | agent test: set accent, preview returns PNG, live UI unchanged until `theme_set`; broken file keeps last good; lint flags a 2:1 pair | M | OR | D6, ADR-273 Lane 1 |
| D8 Motion tokens | Transitions use `motion.*`/`ease.*`; reduced motion rule; pressed translate | `theme.rs`, tween module from ADR-273 Lane 3 | sampled easing values; reduced motion jumps | S | OR (Penner/CSS easing, public) | D6, ADR-273 Lane 3 |
| D9 Focus ring v2 (built, ADR-372) | Ring width/offset/color tokens, inset ring for list rows, ring only in navigation mode (I05) | `crates/regia_ui/src/{extract.rs (ring geometry only),focus.rs}` — **HOT-lite**: geometry, no shader change | ring goldens per widget; pointer click shows no ring | S | OR (+ Godot #110250 idea MIT) | D6 |
| D10 Editor theme panel | egui panel editing `.theme.ron` with live theme sheet preview, lint, save | `crates/regia_editor/src/theme_panel.rs` | offscreen capture golden; save round-trip unchanged | M | OR | D7 |
| D11 Missing default widgets | Radio, spin box, foldable, split, tree, toast (menus belong to ADR-273 Lane 5; D11 only styles them) built on the tokens | per ADR-273 rows W02-W10, Y05, Y06, W05, W06, W07 | per ADR-273 | M-L each | per ADR-273 | D2 (for looks) |

Relation to ADR-273: D1+D2+D6+D7 together deliver gap T01 (ADR-273 Lane 7 theme half); Lane 7's shortcut registry
(I02) stays separate. D4 delivers T13. ADR-273 Lanes 1 to 6 and 8 proceed unchanged and adopt the § 3 looks.

## 9. Owner product questions (each has a recommended default)

1. **Default accent.** Keep the current blue (`#3d9df0`) as the engine default, or a Regia Engine-branded ember orange?
   *Recommended: blue for the engine default (neutral under any game art, no clash with warning amber, existing games
   keep their look); each game sets its brand accent in a child theme, as § 6.7 shows.*
2. **Default scheme.** *Recommended: dark default, light shipped and selectable, high-contrast shipped.*
3. **Icon set source.** Draw our own ~60 glyphs, or ship a subset of a permissive set (ISC/MIT) with attribution?
   *Recommended: permissive subset now (cheap, consistent, legal under ADR-086); replace with an own set later if
   wanted.*
4. **Editor look.** Should the egui editor switch to the runtime default theme (§ 7), or keep today's ADR-229 look
   with the bridge opt-in? *Recommended: opt-in until you have compared both side by side.*
5. **Corner style.** *Recommended: 8 px engine default (`radius.md`); a squarer military look (2 to 4 px) belongs in
   DO27's child theme.*
6. **UI sounds in the default theme.** *Recommended: slots exist, no sounds by default (sound is too game-specific).*
7. **Semibold weight.** Add Inter's variable font (~300 KiB) for a 600 weight, or keep 400/700 statics?
   *Recommended: keep 400/700 now; revisit when the designer lands.*
8. **Default density for games.** *Recommended: `comfortable` for games on every platform (Deck and phones need it);
   `compact` for editor and tools.*

## 10. Conformance notes (audit 2026-11-11, ADR-5664)

Built as written and tested: every § 2.1 color and contrast row (`theme_tokens.rs`), the § 2.2 to 2.6 tables in both densities
(`theme_spec_tables.rs`), the § 3 state order (`theme.rs` `state_order_every_pair`), the § 6.3 precedence rows and the
`ThemeScope` (`theme_resolve.rs`), the § 6.4 swap and cross-fade, the § 6.6 methods (`book/agents/ui-theme.md`) and the § 7 role
bridge (`regia_editor/src/ui_roles.rs`). Built differently, each with its ADR:

- § 6.2 `props`, state layers and `sizes` are `{ "name": value }` maps, not the struct syntax of the example; compact sizes live in
  a `densities` block; elevation and type are scalar tokens (`elevation.N.y|blur|alpha`, `type.<role>.size|line|weight`); unknown
  top-level fields are ignored on load, unknown token and property keys round trip (ADR-277).
- § 6.1 `.theme.ron` v1 is frozen (ADR-290); adding a token or property stays additive.
- § 6.5 and § 6.6: hot reload polls every 30 frames and reports to the status line rather than a toast; the `persist` capability is
  `assets`; the `wait_until` predicate is `{ "predicate": "ui.theme_generation", "at_least": n }` (ADR-330).
- § 5: there is no `Touch` value of `UiInputMode` (`Pointer`, `Navigation`); touch is read from the pointer's active touch, which
  turns the hover layer off (`widget_style.rs` `touch_pointer_suppresses_the_hover_layer`). The focus ring shows in navigation
  mode only (ADR-372).
- § 7: the theme panel previews swatches, not the live theme sheet (ADR-371); `from_ui_roles` lives in `ui_roles.rs`.
- High contrast has a light variant, `regia-light-high-contrast` (ADR-1309), with the same overrides as § 2.1's dark one,
  including `border.width.focus-inset` -3 (the inset was missing until ADR-5664).

Not built (v0.5.x gaps, none blocks the shipped themes): a JetBrains Mono TTF in `regia_ui/assets/fonts` (§ 2.3; `font.mono` names
it with no files, so the platform monospace draws); the ~60-glyph default icon set and an inline `<icon=name>` glyph (§ 2.7; the
atlas mechanism exists, ADR-865, and `<icon=name>` selects the font family `icon-<name>`); a Checkbox widget; theme types for
Radio, SpinBox, Foldable, Split, Tabs, Toast, Progress and Scroll (they draw with button or panel looks); toast role bars and hover
pause; the compact tree indent; the split's 5 % keyboard step; `focus_mode` (`all`, `click`, `none`; an ADR-372 follow-up);
default edge rules for lists, menus and tab strips (§ 5; `FocusGroup` rules work when set); gamepad-cursor snapping
(`soft_cursor.rs`); elevation drawn as a `Shadow` (the tokens resolve into `Themed.elevation`, nothing draws them yet); a JSON
Schema for `.theme.ron`; `Regia.toml` `[ui] theme` (§ 6.7; select a theme with `ActiveTheme` or `ui.theme_select`).

