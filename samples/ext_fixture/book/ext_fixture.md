# Extension host fixture

`ext_fixture` exists for the extension host tests (`crates/regia_editor/tests/dynamic_extension.rs`).

`ext_fixture.marker` (read, no params; its `params_schema` has `additionalProperties: false`, so a call with any field answers `invalid_params`) answers `{ marker, count }`: the marker baked at build time
and a counter carried across reloads. It has no error codes of its own. A system spawns one
`LevelFeatures` entity per frame naming the marker, which lets the test count which build ran.
