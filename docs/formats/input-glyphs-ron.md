# Input glyph table: `.glyphs.ron`

The project's button-prompt glyphs per action and device family (`docs/specs/11` § 7, ADR-7070). Read at start-up from
`assets://input/project.glyphs.ron` when `UiPlugin::load_project_assets` is on; a missing file is an empty table. Rust:
`regia_ui::input_glyphs::InputGlyphTable`.

```ron
(
    version: 1,
    glyphs: {
        "activate": { "xbox": "Ⓐ", "playstation": "<icon=ps_cross>" },
        "Fire": { "keyboard_mouse": "LMB", "gamepad": "RT" },
    },
)
```

| Field | Meaning |
|---|---|
| `version` | 1. Zero or a higher value is refused (`input_glyphs_version`); the file is never partly read. |
| `glyphs` | Action name → device family → glyph text. Families: `keyboard_mouse`, `xbox`, `playstation`, `steam_deck`, or `gamepad` for all three pad families (a family's own entry wins). Any other family name is `input_glyphs_device`; an empty glyph is `input_glyphs_empty`. |

Action names are `InputConfig` action names (`"Fire"`) or the UI hint actions (`activate`, `back`, `secondary`,
`menu`, `prev_tab`, `next_tab`, `page_up`, `page_down`). A glyph is plain text or rich-text markup (`<icon=name>`).

Lookup order (`InputGlyphs::lookup`, agent `input.glyph`): the table; the built-in families for hint actions; the
action's first binding the family can press (highest-priority context first), named the way the family prints it
(`RT`/`R2`, `A`/`Cross`). Tooltip placeholders `{name}` use the same lookup when a table is loaded.

A bad file is logged once (`input_glyphs_invalid`) and the empty table is kept. Files over 1 MiB are refused
(`input_glyphs_too_large`).

## Versioning

Version 1 is the first. A later shape bumps `version` and ships a migrator with `regia upgrade-project`
(`docs/specs/12-assets.md` § 4a).
