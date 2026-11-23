# `.retarget.ron` — retarget setup (format v2)

Loader: `regia_animation::RetargetSetup::from_ron_str`. A setup saves how animation authored for one skeleton plays on
another:

- how each side's roles are found;
- per-role settings;
- the translation and root-motion scale rules;
- facing alignment;
- the IK goals solved at bake (v2, ADR-8476).

`RetargetSetup::build(&source, &target)` returns the `Retargeter`. Spec: `docs/specs/85-retarget-crowds.md` § 2.3
(ADR-1857).

Frozen like every content format (`docs/specs/12-assets.md` § 4a):

- The loader reads version 2 and migrates version 1 (no IK goals). It refuses any other `format_version`.
- `cargo xtask check-formats` hashes the serde shape (`retarget` in `docs/formats/format-hashes.ron`).
- `regia upgrade-project` rewrites version 1 files at version 2 (`regia_animation::migrate_retarget`).

```ron
(
    format_version: 2,
    name: "mixamo-to-guard",
    // How each side's roles are found: Guess (default) | ByName | Profile("generic-humanoid" | "vrm")
    // | Explicit({ role: joint }). `overrides` replace single roles afterwards.
    source: (roles: Guess, overrides: { "leftHand": "mixamorig:LeftHand" }),
    target: (roles: Profile("generic-humanoid")),
    // Per-role settings (all optional).
    roles: {
        "hips": (translation: Some(Scaled)),                 // Bind | Animated | Scaled
        "leftUpperArm": (rest_offset_deg: (0.0, 0.0, -10.0)),  // XYZ Euler degrees, object space
        "rightToes": (skip: true),                           // leave unmapped (keeps its bind pose)
    },
    translation_scale: HipHeight,   // HipHeight (default) | Fixed(f32 > 0)
    root_motion: Auto,              // Auto (default: the translation scale) | Fixed(f32 >= 0) | Off
    align_facing: true,             // default true: turn the source by the 90°-snapped facing yaw
    // IK goals solved at bake (v2): [upper, lower, end] roles, ScaledWorld (default) | Contacts, weight 0..=1.
    ik_goals: [
        (roles: ("leftUpperLeg", "leftLowerLeg", "leftFoot"), mode: Contacts),
        (roles: ("rightUpperLeg", "rightLowerLeg", "rightFoot"), mode: Contacts),
        (roles: ("rightUpperArm", "rightLowerArm", "rightHand"), mode: ScaledWorld, weight: 0.5),
    ],
)
```

Every field except `format_version` has a default, so `(format_version: 2)` is a valid setup that guesses both sides.

## Fields

- **Roles.** These are VRM 1.0 humanoid bone names (`hips`, `spine`, `leftUpperArm`, ...) or any custom role. Joints
  pair across the rigs by role.
- **Guess.** `Guess` runs `RetargetProfile::guess_humanoid` (specs/85 § 2.1).
  - It reads joint-name tokens, so it handles Mixamo, game-style, Biped, deform-prefixed and camel-case names.
  - It fills generic `arm`/`leg` chains in depth order.
  - It finds the hips from the legs and the head from a neck chain when no joint is named for them.
- **Translation modes.**
  - `Bind` keeps the target's bind translation. This is the default for every joint except the skeleton root and
    the hips.
  - `Animated` copies the source's local translation unchanged.
  - `Scaled` moves the joint by the source's object-space offset from bind times the translation scale. This is the
    default for the skeleton root and the hips.
- **`rest_offset_deg`.** It rotates the target bone's rest orientation before the source's motion is applied. It is
  the manual fix for a rest pose that the automatic correction gets wrong. `Retargeter::check` reports it as
  bone-direction error.
- **`root_motion`.**
  - `Fixed(k)` moves the root-motion joint, and the scaled joints under it, by `k` times the source root travel.
    Strides still follow the translation scale, so a `k` that differs from it makes feet slide (the `foot_skate`
    issue).
  - `Off` drops root motion from baked clips.
- **`align_facing`.** Each rig's forward direction is `left × up` from its bind pose, where `left` is right upper leg
  (or upper arm) → left one. The yaw between the two rigs is snapped to 90°. Rigs exported facing +Z and −Z then move
  the same way.

- **`ik_goals`** (v2). Each goal names a three-joint chain by roles, `[upper, lower, end]`. The roles are humanoid or
  custom. On the target the three joints must be a parent → child chain. At bake (`Retargeter::retarget_clip`) the
  two-bone solver puts the chain's end on a goal taken from the source, after the pose transfer. It keeps the bend
  plane and the end's object-space rotation, and blends by `weight` (default 1). Goals solve in list order.
  - The goal is the source end's object-space position times the translation scale, turned by the facing yaw. Under
    `root_motion: Fixed(k)` the root-motion joint's travel moves by `k`, as the body does.
  - `ScaledWorld` (default) uses that goal on every frame. Use it for hands on a prop or a weapon grip: the gap
    between two hands is the source's gap times the scale.
  - `Contacts` is for feet and other ground contacts. The goal's height moves from the source's bind height of the
    end joint to the target's (the ankle height correction). Over every run of frames in which the source end is
    planted, the goal holds still at the run's mean, and it blends back to the free goal over 0.1 s on both sides. A
    run in which the source end travels more than 15 cm on the ground (a clip that walks in place) is not held.
    The planted frames come from the clip's `contact.<source end joint>` curve when the clip has one (cooked contact
    curves). Otherwise they come from the motion-matching contact labels of the source end over the baked frames.
  - A goal whose roles are not mapped on both rigs, or whose target joints are not a chain, is skipped by the bake.
    `Retargeter::check` reports it as `goal_unmapped` or `goal_not_a_chain`. It also reports each goal's residual and,
    for `Contacts` goals, the planted slide (`goal_unreached`, `goal_slide`).
  - With no goals, or only weight-0 goals, the bake is bit for bit the v1 bake.

## Migration

- **v1 → v2** adds `ik_goals`, empty. The setup then bakes exactly what it baked before. `RetargetSetup::from_ron_str`
  loads a v1 file as v2. `regia upgrade-project` (and `regia_animation::migrate_retarget`) rewrites it at version 2.
- Version 3 and later are refused (`bad_setup`).
