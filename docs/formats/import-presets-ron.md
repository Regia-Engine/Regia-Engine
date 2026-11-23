# `config/import_presets.ron` (named import presets, v1)

Spec: `docs/specs/12-assets.md` § 7 (importer settings), `docs/specs/19-editor.md` § 2 (Content Browser) and the w12 UI
parity addendum § 8.10. Proposed in ADR-7904. Registered in `cargo xtask check-formats` and `regia upgrade-project` as
`import-presets`. Fuzz target `asset.import_presets`; corpus fixture
`crates/regia_agent/tests/fixtures/format-corpus/import-presets/`.

The project's shared file is `config/import_presets.ron`, next to `config/import_rules.ron`. A preset is a named
importer settings object for one importer kind: the same RON text a source's `.meta` `settings` carries. A preset never
applies by itself (project-wide defaults are what import rules are for): the Import settings panel, the import dialog
and `editor.import_presets { action: "apply" }` copy its settings into the sources the user picks.

```ron
(
    format_version: 1,                 // must be 1
    presets: [                         // optional; display order
        (name: "UI sprite", importer: "texture", settings: "(usage: Some(ui), compress: Some(false))"),
        (name: "Character", importer: "gltf", settings: "(skin_influences: 8, scale: 0.01)"),
    ],
)
```

- Unknown fields are errors (`deny_unknown_fields`).
- `importer`: `texture`, `gltf`, `obj`, `fbx` or `audio` (the Import settings panel's importer kinds).
- Names: not blank, at most 128 bytes, no control characters, unique per importer ignoring case (the same name may
  exist for two importers).
- `settings` must parse as a RON settings object (`()` or `(key: value, ...)`). The keys are the importer's own; the
  editor checks the values against the importer when a preset is saved and again when it is applied (a preset saved by
  a newer editor with a key this one does not know keeps the key: the cook warns about it).
- The editor writes the file atomically and reads it again before every change, so presets another user committed
  meanwhile are kept. A file that does not parse is reported (`import_presets_unreadable`) and never overwritten.
