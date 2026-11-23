# `.data.ron` (data asset, format v1)

One reflected value of a game or engine type, saved under `assets/` (ADR-7234, `regia_editor::data_asset`, spec
`docs/specs/19-editor.md` § 2, agent page `book/agents/editor-creators.md`). The value's fields are the type's, so
only the envelope is a Regia Engine format.

Since ADR-7310 the editor reads and writes the runtime envelope of `docs/formats/data-ron.md` (`regia_asset::data_asset`,
ADR-7232): `DataAsset(format_version, type, value)`. That page owns the format (its `cargo xtask check-formats` row is
`data-asset`); this page lists what the editor adds. The editor still reads its first spelling, `type_path:` for
`type:` without the `DataAsset` name, and rewrites it in the runtime form on the next save. A game loads only types
registered as data assets (a `#[derive(Asset)]` type registered with `register_data_asset` or `#[reflect(Asset)]`;
the derive alone registers nothing); since ADR-7490 the editor's New ▸ Data asset
list and `editor.create_asset` offer and accept only those types, so every file it authors loads in a game.

## RON form

```ron
DataAsset(
    format_version: 1,
    type: "my_game::waves::WaveSpec",   // the registered type path (a short name is accepted on read)
    value: (
        count: 12,
        delay: 0.5,
        name: "opening",
    ),
)
```

- `type` names a type registered as a data asset (a `#[derive(Asset)]` type, which implies `Default`, registered with
  `register_data_asset` or `#[reflect(Asset)]`).
- `value` is the reflect serializer's RON for that type, the same form a level stores a component in. Fields left
  out keep the type's defaults; an unknown field or a value of the wrong shape is refused (`data_asset_bad_value`).
- The editor writes the value pretty-printed and indented one level under `value:`; a newline inside a string is
  written as `\n`.

Versions: **v1** is the first version. A file with a `format_version` of 0 or newer than this build is refused with
`data_asset_version`. The runtime loader has landed (ADR-7232), so the envelope's `cargo xtask check-formats` row is
`data-asset` (`docs/formats/data-ron.md`) and any change needs a migrator there. The editor's tests (`data_asset.rs`)
hold its side, including `mutated_data_asset_text_never_panics`, the seeded mutation test listed for this page in the
fuzz registry, and `the_editor_writes_the_runtime_envelope_and_reads_the_old_spelling` (ADR-7310).

## Errors

| Code | Meaning |
|---|---|
| `data_asset_parse` | The text is not the envelope shape. |
| `data_asset_version` | `format_version` is 0 or newer than this build. |
| `data_asset_unknown_type` | `type_path` is not registered, or registered but not as a data asset (ADR-7490; the editor's list, create and default value refuse it). |
| `data_asset_bad_value` | The value does not fit the type. |
