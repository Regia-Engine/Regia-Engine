# .formation.ron: formation slots (v1)

ADR-8358. Code: `regia_ai::formation_asset` (`FormationFile`), resolved by `regia_ai::formation`. Guide:
`book/agents/ai-squads.md`. Registry name `ai-formation`. Version 1 is new: there is no migrator and
`regia upgrade-project` has nothing to rewrite. A `format_version` other than 1 is refused
(`unsupported_version`) before the body is read.

A squad names a file through `ai.formation_set { asset }`. The file ships raw in a package. It is not cooked.

## Schema

```
FormationFile {
    format_version: u32,          // 1
    name: String = "",
    spacing_scale: f32 = 1.0,     // finite and above 0; multiplies every offset
    fill_order: [String] = [],    // empty: declaration order; otherwise each slot name once
    slots: [FormationSlotFile],   // 1 to 1024
}
FormationSlotFile {
    name: String,                 // unique, not empty
    offset: (x: f32, y: f32, z: f32),  // anchor frame: x right, z backward; finite; unique
    role: Option<String> = None,  // a tag accepts only a member with that role
}
```

`NaN`, a duplicate name or offset, an empty file, a bad fill order, or more than 1024 slots is refused.
The built-in names `ring`, `line` and `wedge` are the same offsets the code shapes emit, as presets, and
are not files.
