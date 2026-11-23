# .ragdoll.ron: ragdoll profile (v4)

Spec: `docs/specs/93-character-physics.md` § 2.3 and § 3, ADR-1693, ADR-2133 § 6 (v2 shapes). Code: `regia_framework::ragdoll::file`
(`RagdollProfileFile`; `RagdollProfile::from_ron`, `RagdollProfile::to_ron`). Guide: `book/agents/character-physics.md`.
Registered as `ragdoll` in `cargo xtask check-formats` and `regia upgrade-project`. Version 2 adds the per-bone `shape`
(capsule, sphere or box). A version 1 file has no `shape`: it loads with every bone a capsule, and the 1 → 2 migrator
(`regia_framework::ragdoll::file::migrate_ragdoll`, run by `regia upgrade-project`) writes `shape: Capsule` on every bone.
Version 3 adds the optional `active` block (PD motors, group strength, hit response, balance, get-up clips; spec 93 §
10, ADR-2498). A version 1 or 2 file has none and loads with zero gains (an `Active` ragdoll is then exactly `Limp`); the 2
→ 3 migrator only writes the new version, and a profile with zero gains writes no `active` block. An invalid block (a
motor on a joint with no bone or two motors on one joint, a negative `kp`/`kd`, `max_torque <= 0`, a strength outside
`0..=1`, a non-positive `full_impulse`, a negative `recover_time`, `fall_margin` or `fall_time`, `max_tilt` outside
`0..=1.5`, an empty clip name, a zero, overflowing or non-finite key rotation) is `ragdoll_bad_active`.
Version 4 (ADR-2963) adds the `Sole` body shape (a level foot) and the optional `active.recovery` block (stepping,
landing, planting on moving bodies). A version 3 file has neither: it loads with recovery off, exactly as before; the 3 → 4
migrator only writes the new version, and a profile with recovery off writes no `recovery` block. A `recovery` block with
`max_steps` above 16, a negative or non-finite length or time, or `pre_bend` outside `0..=0.5` is `ragdoll_bad_active`; a
sole with a non-positive width or height is `ragdoll_bad_bone`.
A newer `format_version` is refused before the body is read
(`ragdoll_format_version`); unknown fields are load errors with line and column (`ragdoll_parse`); an empty joint name or
a non-positive radius or mass, or a box with a non-positive width or depth, is `ragdoll_bad_bone`.

## Schema

```
RagdollProfileFile {
    format_version: u32,                 // 4 (1, 2 and 3 are read and migrated)
    forward: (f32, f32, f32),            // rig-space direction the character faces (Regia: (0, 0, -1))
    bones: [RagdollBoneFile],
    active: Option<RagdollActiveFile>,   // v3; absent = zero gains (passive)
}
RagdollActiveFile {
    motors: [RagdollMotorFile],
    strength: (spine: f32, arms: f32, legs: f32),          // default 1 each, 0..=1
    hit: (full_impulse: f32, recover_time: f32),           // default (200 kg m/s, 1 s)
    balance: (cd: f32, cv: f32, max_tilt: f32,             // default (1.0 rad/m, 0.3 rad/(m/s), 0.35 rad,
              fall_margin: f32, fall_time: f32,            //          0.05 m, 0.1 s,
              fall_strength: f32),                         //          0.0)
    get_up: [RagdollGetUpFile],                            // default []
    recovery: Option<RagdollRecoveryFile>,                 // v4; absent = recovery off (standing only)
}
RagdollRecoveryFile {                    // every field left out takes RagdollRecovery::full()'s value
    max_steps: u32,                      // 3; steps in one recovery, 0..=16 (0: never step)
    trigger_margin: f32,                 // 0.05 m; capture point outside the polygon that starts a step
    overshoot: f32,                      // 0.05 m; beyond the capture point
    max_step: f32,                       // 0.6 m; from the stance foot
    min_width: f32,                      // 0.14 m; sideways from the stance foot (no crossed legs)
    swing_time: f32,                     // 0.3 s
    lift: f32,                           // 0.08 m
    landing: bool,                       // true; land on the feet from the air
    pre_bend: f32,                       // 0.08; knee bend in the air (fraction of the leg), 0..=0.5
    crouch: f32,                         // 0.08 m; landing crouch
    absorb_time: f32,                    // 0.5 s; the crouch rises over this
    max_air_time: f32,                   // 2.0 s; airborne longer falls
    moving_support: bool,                // true; plant on one body that can move, in its frame
}
RagdollMotorFile {
    joint: String,                       // one of `bones` (the root bone is never driven)
    group: Spine | Arms | Legs,
    kp: f32,                             // N m/rad, >= 0
    kd: f32,                             // N m s/rad, >= 0
    max_torque: f32,                     // N m, > 0
    balance: None | Hip | Ankle,         // default None; Ankle bodies are the feet (support polygon)
}
RagdollGetUpFile {
    clip: String,                        // the clip the game plays (non-empty)
    facing: FaceUp | FaceDown | SideLeft | SideRight,
    pose: [(joint: String, rotation: (f32, f32, f32, f32))],  // key bones relative to the root joint, (x, y, z, w)
}
RagdollBoneFile {
    joint: String,                       // skeleton joint name
    radius: f32,                         // capsule or sphere radius, metres (> 0)
    mass: f32,                           // kg (> 0)
    length: Option<f32>,                 // body length along the bone; None measures it from the skeleton
    limits: Option<RagdollLimitsFile>,   // None: unlimited ball joint; ignored on the root bone
    shape: RagdollShapeFile,             // v2; absent (v1) = Capsule
}
RagdollShapeFile =
    Capsule                              // radius, cap tip to cap tip `length`
  | Sphere                               // radius, centred halfway along the bone
  | Box(width: f32, depth: f32)          // full extents (> 0): side to side, front to back (along `forward`)
  | Sole(width: f32, height: f32)        // v4: level foot box (> 0): side to side, thickness

RagdollLimitsFile {
    flex: Forward | Backward,            // which way the limb flexes, relative to `forward`
    bend: (f32, f32),                    // min/max flexion (rad), positive = towards the flex direction
    side: (f32, f32),                    // min/max sideways swing (rad)
    twist: (f32, f32),                   // min/max twist about the bone (rad)
}
```

The joint frame is built from the skeleton's bind pose: twist along the bone, bend about `bone × flex direction`, side
about the third axis; every range is measured from the bind pose.

Every shape sits along the bone, from the joint towards the nearest simulated (or first) child joint. A leaf bone with
`length: None` is four extents long (a capsule's radius, half a box's larger side), a sphere one diameter (it touches
its joint). A box's length runs along the bone, its depth along `forward` made perpendicular to the bone. The density is
the bone's mass over the shape's volume. A sole is the exception: it lies level (world up when the ragdoll is built),
`length` long along the level direction from the joint to its child (`forward` for a vertical bone), from a quarter of its
length behind the joint, with its underside at the lower of the joint and the child. Use it for imported foot bones, which
point down to the toes.

## Example

```ron
(
    format_version: 2,
    forward: (0.0, 0.0, -1.0),
    bones: [
        (joint: "hips", radius: 0.12, mass: 10.6, length: None, limits: None, shape: Box(width: 0.3, depth: 0.2)),
        (joint: "head", radius: 0.1, mass: 6.1, length: None, limits: None, shape: Sphere),
        (joint: "calf_l", radius: 0.05, mass: 4.6, length: None,
         limits: Some((flex: Backward, bend: (0.0, 2.3), side: (-0.05, 0.05), twist: (-0.15, 0.15))), shape: Capsule),
    ],
)
```

The same file in version 1 has no `shape` fields; `regia upgrade-project` rewrites it as above with `shape: Capsule`.

## Example (v3, active)

```ron
(
    format_version: 3,
    forward: (0.0, 0.0, -1.0),
    bones: [
        (joint: "hips", radius: 0.12, mass: 10.6, length: None, limits: None, shape: Box(width: 0.3, depth: 0.2)),
        (joint: "calf_l", radius: 0.05, mass: 4.6, length: None, limits: None, shape: Capsule),
        (joint: "foot_l", radius: 0.05, mass: 1.2, length: None, limits: None, shape: Box(width: 0.1, depth: 0.1)),
    ],
    active: Some((
        motors: [
            (joint: "calf_l", group: Legs, kp: 3600.0, kd: 12.0, max_torque: 600.0),
            (joint: "foot_l", group: Legs, kp: 2700.0, kd: 9.0, max_torque: 600.0, balance: Ankle),
        ],
        hit: (full_impulse: 150.0, recover_time: 0.8),
        get_up: [
            (clip: "getup_back", facing: FaceUp),
            (clip: "getup_front", facing: FaceDown, pose: [(joint: "calf_l", rotation: (0.0, 0.0, 0.0, 1.0))]),
        ],
    )),
)
```

**Migration note (v2 → v3).** No field changes meaning; `regia upgrade-project` rewrites the version. The lead
re-registers the format with `cargo xtask check-formats --update`.

**Migration note (v3 → v4).** No field changes meaning; `regia upgrade-project` rewrites the version. A v4 file with a
`Sole` bone or a `recovery` block does not load on older builds. The lead re-registers the format with `cargo xtask
check-formats --update`.
