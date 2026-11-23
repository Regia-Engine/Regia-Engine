# `.theme.ron` v2 (`regia.theme`)

A UI theme as data: colors, sizes, type, motion and per-widget looks, in one RON file that can extend
another. Loader: `regia_ui::theme_file` (`parse_theme_ron`, `ThemeSet`). Design and the token values:
`docs/specs/ui-default-design.md` §§ 2 and 6. Example: [`theme.example.theme.ron`](theme.example.theme.ron).
Built-in files: `crates/regia_ui/assets/themes/regia-{dark,light,high-contrast}.theme.ron`.

Status: **frozen** at `format_version: 2` (lane D6, ADR-290; v2: ADR-8105 (e), ADR-8303; registry name `theme` in
`format-hashes.ron`).

| Version | Change | Migration |
|---|---|---|
| 0 | the pre-v1 `UiTheme` serde shape | v0 → current by the v0 migrator (below) |
| 1 | the token, type and variation file | none |
| 2 | adds the `focus_visible` layer of a type or variation and the style `rules` | 1 → 2 rewrites only the `format_version` number |
Adding a token key or widget property is additive and never bumps the version; renaming or removing a key, or
changing its meaning or unit, needs a version bump, a migrator and `regia upgrade-project` support
(`docs/specs/12-assets.md` § 4a).

## File shape

```ron
(
    format_version: 2,
    name: "my-game",                // unique in a ThemeSet
    extends: Some("regia-dark"),  // None for a root theme
    scheme: dark,                   // dark | light; omitted = inherit (root default dark)
    density: comfortable,           // comfortable | compact; omitted = inherit (root default comfortable)
    tokens: { "color.accent": "#e8793a", "radius.md": 3 },
    densities: { "compact": { "size.control.md": 26 } },
    fonts: { "Rajdhani": (files: ["fonts/Rajdhani-Regular.ttf"]) },
    types: { "Button": (props: { "radius": "@radius.md" }, hover: { ... }) },
    variations: { "Button.primary": (extends: "Button", props: { ... }) },
    sounds: {},                     // reserved
    rules: [                        // v2: style rules, see below
        (inside: Some("Toolbar"), widget: Some("Button"), props: { "background": "@color.surface" }),
    ],
)
```

Every field except `format_version` and `name` may be left out. A file with no `format_version`, or `0`, is the
pre-v1 `UiTheme` shape and is migrated on load (below). A v1 file reads as v2 with no rules and no
`focus_visible` layers (and saves as v2); a file that declares v1 but uses `rules` or a `focus_visible` layer is
refused. A newer version is an error.

## Values

A token value is a number (UI px) or a string. A string is one of:

| Form | Example | Value |
|---|---|---|
| color | `#rgb`, `#rrggbb`, `#rrggbbaa` | sRGB color |
| duration | `80ms`, `0.08s` | seconds |
| easing | `cubic-bezier(0.2, 0, 0, 1)`, `linear` | curve (x control points within 0..1) |
| reference | `@color.accent` | the value of another token, in the merged chain |
| operation | `lighten(c, x)`, `darken(c, x)`, `alpha(c, a)`, `mix(c1, c2, t)` | color |
| anything else | `Inter` | text (font family) |

Operation arguments are colors, numbers, references or nested operations. `lighten` and `darken` move OKLab
lightness by `x` (`0..1` units) and keep hue; `alpha` replaces the alpha; `mix` interpolates in OKLab, alpha
included. A result outside the sRGB gamut is pulled in by scaling chroma at constant lightness. There are no other
expressions. The derived values in the spec's § 2.1 table are approximations: the shipped expressions are
normative (`lighten(#3d9df0, 0.06)` is `#5ab0ff`, not `#5aaef4`).

Token keys are `group.name[-modifier]`. Compound values are split into scalar tokens: `elevation.2.y`,
`elevation.2.blur`, `elevation.2.alpha`, with `elevation.color` (spec § 2.5); type roles are
`type.<role>.size`, `.line`, `.weight`.

## `extends` and density

Resolution walks the chain root first; a child key replaces the parent's. Expressions are resolved after the
merge, so a child that changes `color.accent` restyles every derived token it inherits. `densities.<name>` is
applied after the same file's `tokens` when that density is active, so within one file the density block wins and a
child's plain `tokens` entry overrides a parent's density block (give the child its own block to change both).

## Types and variations

`types` holds widget types and parts (`Button`, `Slider.track`); `variations` holds variations (`Button.primary`).
Both use the same shape: `props` (normal state), the state layers `hover`, `pressed`, `focused`, `focus_visible`
(v2), `disabled`, `selected`, `error`, and `sizes` (`sm`, `lg`). `focus_visible` applies over `focused` only while
the focus came from the keyboard or a gamepad (`FocusRingPolicy`, by default `navigation_only`): a clicked control
keeps the quieter `focused` look. Each layer is a map from property name to value; a value is a
number, a string or a list (`border: [1, "@color.border"]`). Strings that start with `@` or `#`, or are calls, are
token expressions and are checked on load; other strings are keywords. Property names are open: the set widens in
lane D2 without a format change and unknown properties survive a load and save. A variation's `extends` names its
base (`Button.primary` extends `Button`; omitted means the part before the dot). A base that is itself a variation
must exist and must not loop; a bare widget type name is supplied by the widget code and is not checked.

Child files merge types and variations per property.

## Style rules (v2)

`rules` is a list of structured selectors; there is no selector string to parse. Each rule (`deny_unknown_fields`):

| Field | Meaning |
|---|---|
| `inside: Some("Toolbar")` | some ancestor, at any depth, has this widget type (a descendant selector) |
| `widget: Some("Button")` | the node has this widget type |
| `variation: Some("primary")` | needs `widget`; the node's variation chain holds `<widget>.<variation>` (so `danger`, which extends `primary`, matches) |
| `classes: ["danger-zone"]` | the node carries every one of these classes |
| `state: Some(hover)` | `hover`, `pressed`, `focused`, `focus_visible`, `disabled`, `selected` or `error`; omitted = the normal state |
| `props` | the properties, as in a type's layer |

A rule needs at least one of `inside`, `widget` or `classes` (a rule for every node is refused, naming `rules[i]`).
A node's widget types are the type and variation keys its classes pick (`button` gives `Button`, `primary` gives
`Button.primary` and `Button`; a class that names a type, with a dot or an upper-case first letter, gives itself,
so `"Toolbar"` gives `Toolbar`). One resolved theme may name at most 64 distinct `inside` types.

Order: the matching rules apply after the node's types and variations and before game classes built in code and the
node's `ThemeOverride`, in ascending **specificity**, then in chain order (a parent theme's rules before a child's)
and file order; later wins per property. Specificity is the pair (`variation` and `classes` conditions counted,
`widget` and `inside` conditions counted), compared first element first: `[hot]` (1, 0) beats `Toolbar Button`
(0, 2), which beats `Button` (0, 1). A rule composes like one more variation: its state layer joins the fixed state
order, so a normal-state rule that sets `background` also covers the type's hover background, while a rule that sets
only other properties keeps the type's hover and pressed feedback. A theme with no rules and no `focus_visible`
layer resolves exactly as v1.

Inspectors and the agent show a rule by display text that is never parsed back: `inside`, then `widget` or
`widget.variation`, then `[class]` per class, then `:state` (`Toolbar Button`, `[danger-zone] :hover`,
`Button.primary :focus_visible`).

## Load errors

Every error names the theme and a key path (`tokens.color.accent-hover`, `densities.compact.size.row`,
`types.Button.hover.background`, `variations.Button.danger.extends`). Kinds: `Parse` (RON position), `UnsupportedVersion`,
`Value` (bad syntax or wrong kind, e.g. `lighten` of a duration), `UnknownReference`, `Cycle` (with the chain),
`UnknownParent`, `ExtendsCycle`, `Variation`. `ThemeSet::insert` and `load_ron` re-validate the new theme and every
theme that extends it in both densities first; on any error the set is unchanged, so the previous good theme stays
active (hot reload relies on this).

Unknown token keys are kept and saved. `ThemeSet::unknown_token_keys` lists those absent from `regia-dark`
(the linter reports them as warnings). Unknown top-level fields are ignored on load.

## v0 migration

Format version 0 is today's serde shape of `UiTheme` (`name`, `colors` with ten slots, `radius`, `font_px`) and
`ThemeClasses`. `regia_ui::theme_migrate::migrate_v0` maps:

| v0 | v1 |
|---|---|
| `colors.background` / `surface` / `surface_hi` | `color.bg` / `color.surface` / `color.surface-raised` |
| `colors.primary` / `on_primary` | `color.accent` / `color.on-accent` |
| `colors.text` / `text_dim` | `color.text` / `color.text-muted` |
| `colors.border` / `overlay` / `danger` | `color.border` / `color.scrim` / `color.danger` |
| `radius` | `radius.md` |
| `font_px` | `type.body.size` |
| class `button` / `tab` / `panel` / `toggle` / `slider-track` | type `Button` / `Tabs` / `Panel` / `Toggle` / `Slider.track` |
| class `nav` / `surface` | variation `Button.nav` / `Panel.surface` |
| class `primary`, `secondary`, `ghost`, `danger`, `icon` | variation `Button.<name>` |
| any other class | variation `Panel.<name>` |

The result extends `regia-light` when the v0 background is light (OKLab lightness 0.5 or more), else
`regia-dark`. Colors are written as 8-bit hex, so a color that was a hex string round-trips exactly and an
arbitrary linear color is rounded to the nearest sRGB code. Class visuals keep their v0 property names
(`background`, `border_color`, `border_width`, `radius`, `text_color`, `focus_ring`); a class `transition` becomes
`transition: ["<n>ms", "@ease.standard"]`. `regia upgrade-project --write` rewrites `.theme.ron` files this way.

## How the runtime reads types and variations

`regia_ui::apply_themes` (ADR-289) resolves each node from the active theme. A node's classes pick types and
variations through the v0 class table above (`button` is `Button`, `primary` is `Button.primary`), or name one
directly (`"Button.ghost"`). A later class wins: a `.ui.ron` button widget puts its own `button` class first, before
the authored classes, so `classes: ["primary"]` shows `Button.primary` (ADR-8303). For each property the first hit
wins: the node's `ThemeOverride`, the matching style rules (most specific first), the variation chain nearest first,
the widget type, then the theme defaults (`@color.text`, `@color.border`, `@radius.md`, the
disabled rule). Each of those layers composes its own states in the order `props`, `sizes.sm|lg`, `selected`,
`hover`, `pressed`, `error`, `focused`, `focus_visible`; every layer's `disabled` applies last.

Property names the resolver reads (others are kept and ignored; the linter will report them):

| Property | Value |
|---|---|
| `background`, `border_color`, `text_color`, `icon_color` | color |
| `border` | `[width, color]` (either order, either one) |
| `border_width`, `radius` (number or a list, first entry used), `padding_x`, `height`, `min_width`, `gap` | UI px |
| `text_px` or `font_size` | UI px |
| `focus_ring` | color, or `[color, width, offset]`; also `focus_ring_width`, `focus_ring_offset` |
| `opacity` (clamped to 0..1), `scale`, `elevation` (step) | number |
| `translate` | `[x, y]` UI px, visual only |
| `transition` | duration, or `[duration, easing]` (the easing is read by lane D8) |

`ThemeScope { theme }` on a node resolves its subtree against another loaded theme; `ActiveTheme` selects the theme,
density, high contrast and reduced motion. Control height and inline padding fall back to `size.control.<size>` and
`size.pad-x.<size>` for nodes that carry a `WidgetSize`.
