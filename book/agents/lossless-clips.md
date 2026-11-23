# Lossless animation clips (`lossless_clips`)

The cook compresses animation clips (`cooked-clip` v3, joint tracks key-reduced and quantized within the
spec 17 tolerances; see [Program loudness and compressed clips](audio-program-loudness.md#animation-clips-compressed-at-cook)).
A source can opt out: with `lossless_clips: true` in its `.meta` importer settings, every clip it produces is
cooked as a `Raw` body, bit-exact to the clip its importer decoded. Use it for clips whose small joint errors
show: hand contacts, props held between two hands, close-up facial or finger work, cinematic takes compared
frame by frame against a reference.

The switch is per source asset (`.meta` v2, ADR-7819). It applies to the glTF, FBX, USD and BVH importers, to
every skin clip and per-node morph clip of a glTF, and to a BVH's retargeted copy. It does not turn off
import-time key reduction: a BVH's `rotation_tolerance_degrees` and `translation_tolerance` still apply (set
them to 0 to keep every authored key). On dense clips a compressed payload is under a quarter of the
lossless one, so lossless clips cost four times the disk and pak space or more; in memory both are ordinary
`f32` clips.

## Setting it

Through the agent protocol, on a source that has been cooked once:

```json
{"method": "editor.set_import_settings",
 "params": {"path": "assets/anim/handshake.glb", "settings": "(lossless_clips: true)"}}
```

The call writes the settings into the `.meta`, replacing folder rules for that source, and re-cooks:
the result's `errors` is empty and the clip sub-assets are rewritten. `settings` replaces the whole
settings object, so pass the source's other settings too. Omitting `settings` (or `null`) clears the
override, and the next cook compresses again.

For a folder, add a rule to `config/import_rules.ron` (see [Import rules](import-rules.md)):

```ron
(format_version: 1, rules: [
    (glob: "assets/cinematics/**", settings: "(lossless_clips: true)"),
])
```

`assets.import_rules { path }` shows whether the switch reached a source and which layer set it.
`regia assets import-rules check` knows the field for the `gltf`, `fbx` and `bvh` importers, so it is never
reported as `import_rule_unknown_field`.

## Verifying it

- `editor.set_import_settings` answers `errors: []`; a refused settings string answers
  `asset_settings_invalid` (F0002 with `data.reason`).
- In Rust, `AnimationClip::compressed_cooked_payload(payload)` is `None` for a lossless cooked clip, and
  `AnimationClip::from_cooked_payload(payload)` equals the imported clip bit for bit.

## Older projects

`.meta` v1 files load and migrate to v2 with the switch **off**: a v1 file that already named
`lossless_clips` (the v1 glTF importer ignored it, the v1 BVH importer refused it) has the field dropped, so
it cooks exactly as before. `regia upgrade-project --write` writes the v2 file; one that never named the
key only has its version number changed, comments kept. A file with a newer version is refused. The format
is described in `docs/formats/meta.md`.

## Error codes

| Code | When |
|---|---|
| `asset_not_imported` | `editor.set_import_settings` on a source with no `.meta`. |
| `asset_settings_invalid` | `editor.set_import_settings` with a settings string that is not a RON object. |
