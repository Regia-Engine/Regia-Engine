# `.drivers.ron` — character rig drivers (format v1)

Loader: `regia_animation::DriverSet::from_ron_str`; compile with `DriverSet::compile(&skeleton, &morph_names)`
into a `CompiledRig`, attach it with the `RigDrivers` component (or `anim.rig_attach`). Frozen like every
content format (`docs/specs/12-assets.md` § 4a): the loader checks `format_version`, `cargo xtask check-formats`
hashes the serde shape (`drivers` in `docs/formats/format-hashes.ron`). No migrations yet.

One file per character (or per body family). Every section is optional; evaluation order is
**twists, helpers, drivers (document order), correctives**. Reference file:
`crates/regia_animation/tests/fixtures/arm.drivers.ron`.

```ron
(
    format_version: 1,
    twists: [(
        name: "forearm_twist",
        source: "hand",                 // joint whose roll is distributed
        axis: (0.0, 1.0, 0.0),          // bone axis in the source's local frame (default +Y)
        driven: [
            (joint: "twist1", weight: 0.3333),   // optional per-target `axis`
            (joint: "twist2", weight: 0.6667),
        ],
        source_keep: 1.0,               // fraction of its own roll the source keeps (default 1)
    )],
    helpers: [(
        name: "elbow_pad",
        joint: "pad",                   // the posed helper
        source: "fore",                 // the joint that drives it
        follow: 0.5,                    // fraction of the source's rotation delta copied
        offset: [                       // translation keyed on the source's rotation angle (degrees)
            (angle_deg: 0.0, offset: (0.0, 0.0, 0.0)),
            (angle_deg: 100.0, offset: (0.0, 0.0, 0.03)),
        ],
    )],
    drivers: [(
        name: "elbow_bulge",
        input: JointAngle(joint: "fore", axis: (1.0, 0.0, 0.0)),
        curve: (keys: [(location: 0.0, value: 0.0, kind: Linear), (location: 100.0, value: 1.0, kind: Linear)]),
        output: Morph("elbow_bulge"),
        blend: Add,                     // Add (default) | Set | Multiply; morph and material outputs
        weight: 1.0,                    // multiplier on the curve value (default 1)
    )],
    correctives: [(
        name: "elbow_crease",
        joints: ["fore"],
        poses: [(name: "bent100", rotations: [(100.0, 0.0, 0.0)], morphs: [("elbow_crease", 1.0)])],
        radius: 0.8,                    // Gaussian kernel width, radians (default 0.8)
        weight: 1.0,
    )],
)
```

## Driver inputs and outputs

| Input | Reads |
|---|---|
| `Param(name)` | an animator parameter: float, int (as a number) or bool (0 or 1); unset reads 0 |
| `Morph(name)` | a morph weight as written so far (earlier drivers, the animation) |
| `JointAngle { joint, axis }` | signed roll about `axis` (joint-local) away from the bind rotation, degrees, -180..180 |
| `JointSwing { joint, axis }` | unsigned swing away from `axis` with the roll removed, degrees, 0..180 |
| `JointRotation { joint }` | total rotation away from the bind rotation, degrees, 0..180 |

| Output | Writes |
|---|---|
| `Morph(name)` | a morph weight, combined by `blend` |
| `Material(name)` | a material parameter in `RigState::materials` (wrinkle maps), combined by `blend`, starts at 0 |
| `JointRotate { joint, axis }` | rotates the joint (local frame) by the value in degrees; always additive |
| `JointTranslate { joint, direction }` | adds `direction * value` metres to the local position |
| `JointScale { joint, per_unit }` | multiplies the local scale by `1 + per_unit * value` |

The curve's key `location` is the input, `value` the output; outside the keys it clamps to the end keys. An empty
curve is rejected.

## Pose-space correctives

Each pose lists, per watched joint, an euler rotation (XYZ, degrees) as a delta from the bind rotation in the joint's
local frame, and the morph weights wanted there. The bind pose is an implicit example with all weights 0. The
coefficients are solved at compile time (Gaussian radial basis functions over the combined joint-rotation
distance); at run time the interpolated weights are added to the morph bank. Each example reproduces its authored
weights exactly. Between examples the weights blend smoothly; far from every example they fall to 0, so place examples
at the extremes you sculpted.

## Compile errors

`compile` reports all problems at once (`DriverError::Unresolved`): unknown joints, unknown morph targets, empty or
non-finite curves, zero axes, duplicate driver names, corrective shape mismatches.
