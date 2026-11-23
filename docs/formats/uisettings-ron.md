# `.uisettings.ron` v2 (the project's UI settings)

Owner: `regia_ui::scale` (ADR-2866, ADR-8252). One optional file per project, at `assets/ui/project.uisettings.ron`, read once at
startup by `regia_ui::scale::load_ui_settings`. It lives under `assets/` because the project manifest does not ship, so
a packaged game carries these settings on every platform. Example:
[`uisettings.example.uisettings.ron`](uisettings.example.uisettings.ron).

```ron
(
    version: 2,
    dpi_curve: [(96.0, 1.0), (144.0, 1.25), (192.0, 1.75)],
    layer_budget_bytes: 33554432,
    scale_mode: IntegerScale(reference: (320.0, 180.0)),
)
```

| Field | Meaning |
|---|---|
| `version` | 2. Version 1 still loads and is read as 2. A newer version, or version 0, is refused |
| `dpi_curve` | Optional `(dpi, scale)` points for `ConstantPhysicalSize` roots, DPI strictly rising, both above zero; piecewise linear, the end values hold outside the range. DPI is 96 times the OS scale factor |
| `layer_budget_bytes` | Optional bytes of group layers one draw list may need (default 64 MiB) |
| `scale_mode` | Optional project scale rule (v2, ADR-8252). `ConstantPixelSize`, `ConstantPhysicalSize`, `ScaleWithScreen(reference: (w, h), match_width: 0.5)` or `IntegerScale(reference: (w, h))`. Absent keeps the engine default. Applied only to roots still on that default |

Unknown fields are refused. A file that does not load logs the warning `ui_settings_invalid` and changes nothing, so a
typo never blocks the game from starting. A value the game already set in code stays: code wins over the file. A root
whose scale mode is anything other than the engine default is left alone. `regia upgrade-project` rewrites a v1 file
by changing only the `version` number (`regia_ui::migrate_ui_settings_ron`).
