# `.data.ron` (custom data asset, format v1)

One value of a game's reflected Rust type, declared with `#[derive(Asset)]` (ADR-7232, `regia_asset::data_asset`,
spec `docs/specs/10-core.md` § 2.4 and `docs/specs/14-scene-and-framework.md` Part C). The value's fields are the
game's type, so only the envelope is a Regia Engine format.

```ron
DataAsset(
    format_version: 1,
    type: "my_game::WeaponStats",   // the type's full path, registered as a data asset
    value: (
        name: "rifle",
        damage: 34,
        range_m: 120.5,
    ),
)
```

- `type` must be registered with `App::register_data_asset::<T>()` or carry `#[reflect(Asset)]`; anything else is
  `data_asset_unknown_type` at load.
- `value` follows the reflected RON rules (spec 10 § 2.3): missing fields take the type's defaults, unknown fields are
  warnings (logged, and in `DataAssetFile::warnings`).
- `DataAssetFile::to_ron` writes the canonical form above (four-space indent, LF newlines).

The cook (importer `data_asset`) checks the envelope only (version, a non-empty `type`, a RON `value`) and stores the
text as written (CRLF turned into LF) in a cooked file of type `regia_asset::DataAsset`; the asset registry tags it
`kind: data_asset` and `type: <type path>`. A game loads it with `DataAssets::load_path(&mut server, path, &registry)`
and reads it with `get::<T>(id)`.

Versions: **v1** is the first. A newer `format_version` is refused (`data_asset_version`). Registered in
`cargo xtask check-formats` (`data-asset`) and `regia upgrade-project` (no migration steps yet).

Error codes (`DataAssetError::code`): `data_asset_parse`, `data_asset_version`, `data_asset_unknown_type`,
`data_asset_value`, `data_asset_write`, `data_asset_load`.
