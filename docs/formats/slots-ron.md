# `.slots.ron`: the skeleton slot table (format v1)

Loader: `regia_animation::SkeletonSlots::from_ron_str` (validates names and groups; newer versions are rejected).
Spec: `docs/specs/34-montage-extensions.md` § 3 (ADR-1644). Frozen (`docs/specs/12-assets.md` § 4a): `format_version`,
no migrators yet, the `slots` entry in `docs/formats/format-hashes.ron`; `regia upgrade-project` knows the suffix.
The file sits beside the skeleton asset. A missing file means the engine default table: one full-body slot
`default` in group `default`.

```ron
(
    format_version: 1,
    slots: [
        (name: "upper_body", group: "upper", mask: Some((joints: ["spine"], include_children: true))),
        (name: "full", group: "default", mask: None),
    ],
)
```

A slot name is unique, lowercase ASCII, digits and `_`, 1 to 48 characters. A mask joint the skeleton lacks is an
error naming the joint (`SkeletonSlots::validate_against`). `SkeletonSlots::slot_defs` gives the player's `SlotDef`
list; explicit `MontagePlayer.slots` entries win by name when a project builds rigs in code.
`Montage::validate_against` reports one `montage_unknown_slot` issue per track that plays a slot the table lacks
(a warning at load, an error in the cook step).
