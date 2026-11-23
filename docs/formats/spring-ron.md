# `.spring.ron`: spring-bone setup (format v1)

Loader: `regia_animation::SpringBoneSetup::from_ron_str` (validates names and ranges; newer versions are rejected).
Put it next to the skeleton's source (`Fox.glb.spring.ron`) and give the rig
`SpringBones::new(setup)`. Frozen like every content format (`docs/specs/12-assets.md` § 4a): `format_version`,
a migrator table (none yet) and the `spring` entry in `docs/formats/format-hashes.ron`; `regia upgrade-project`
knows the suffix. Every field except `name`, `joints` and `shape` has a default.

```ron
(
    format_version: 1,
    step_hz: 60.0,                       // fixed internal step
    lod: (reduced_distance: 12.0, reduced_step_hz: 30.0, freeze_distance: 40.0),
    chains: [(
        name: "ponytail",
        joints: ["tail_0", "tail_1", "tail_2"],   // root first, each the child of the previous
        stiffness: 0.15,                 // 0..1 per step; 1 keeps the animated pose
        damping: 0.1,                    // 0..1 velocity lost per step
        gravity_dir: (0.0, -1.0, 0.0),
        gravity_power: 9.81,             // m/s^2
        hit_radius: 0.02,
        colliders: ["head", "neck"],
        enabled: true,
    )],
    colliders: [
        (name: "head", joint: "head", offset: (0.0, 0.08, 0.0), shape: Sphere(radius: 0.11)),
        (name: "neck", joint: "neck", shape: Capsule(radius: 0.05, tail: (0.0, 0.1, 0.0))),
    ],
)
```

The first joint of a chain keeps its animated position and is rotated to follow the second; every later joint is
simulated. Only rotations are written. Details: `book/agents/spring-bones.md`, ADR-237.
