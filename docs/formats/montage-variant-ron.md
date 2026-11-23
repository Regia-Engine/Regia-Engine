# `.montagevar.ron`: variant montages (format v1)

Loader: `regia_animation::MontageVariant::from_ron_str`. Resolver: `MontageVariant::resolve` and
`MontageLibrary::add_variant`. Spec: `docs/specs/34-montage-extensions.md` § 6 (ADR-1644). Frozen
(`docs/specs/12-assets.md` § 4a): `format_version`, no migrators yet, the `montage-variant` entry in
`docs/formats/format-hashes.ron`; `regia upgrade-project` knows the suffix.

```ron
(
    format_version: 1,
    name: "combo_axe",
    parent: "assets/anim/combo.montage.ron",   // or just the montage name "combo"
    overrides: [
        (track: 0, segment: 2, clip: "swing_axe", rescale: true),   // rescale defaults to false
    ],
)
```

The resolved montage is a deep copy of the parent with each override's clip substituted and the variant's own name.
`rescale: true` sets the segment rate so the replacement fills the original segment's timeline duration
(`rate = new span x loops / old duration`); `false` keeps the rate and the timeline length may change. Sections,
notifies, warps, blends and flags are inherited and cannot be named. A parent that is itself a variant is refused
(`VariantError::ParentIsVariant`). Replacing the parent in a `MontageLibrary` re-resolves its variants; one that no
longer resolves is dropped and reported.
