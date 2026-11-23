# `.skelprofile.ron` — skeleton profile (format v1)

Loader: `regia_animation::SkeletonProfile::from_ron_str`. A profile describes the canonical joints of a rig family
(names, hierarchy, retarget roles, import aliases, rest-pose rule). Frozen like every content format: the loader checks
`format_version`; `cargo xtask check-formats` hashes the serde shape (`skelprofile` in `docs/formats/format-hashes.ron`).

```ron
(
    format_version: 1,
    name: "arm-rig",
    humanoid: false,             // true (default): the required VRM humanoid roles must all be assigned
    allow_extra_joints: true,    // false: a skeleton with joints the profile does not list fails
    rest_pose: TPose(tolerance_deg: 8.0),   // Any (default) | TPose { tolerance_deg } | APose { angle_deg, tolerance_deg }
    joints: [                    // parents are listed before their children
        (name: "root", role: Some("hips")),
        (name: "upper", role: Some("leftUpperArm"), parent: Some("root"), aliases: ["upperarm_l"]),
        (name: "pad", parent: Some("root"), required: false),
    ],
)
```

- `role` is a VRM 1.0 humanoid bone name (`hips`, `spine`, `leftUpperArm`, ...) or any custom role; roles are what
  `RetargetProfile` and the retargeter pair joints by. `humanoid: true` requires the 16 required VRM roles.
- Matching a skeleton: exact name, then an alias, then a case-insensitive name, then (for humanoid roles) the name guess of
  `RetargetProfile::guess_humanoid`; guessed matches are reported in `guessed`.
- The hierarchy check requires the nearest present listed ancestor to be an ancestor of the joint in the skeleton, so
  twist or prop joints in between are fine and absent optional joints (upper chest, shoulders) are skipped.
- Rest pose: the arm elevation is the angle below horizontal of the upper-arm to lower-arm direction in the bind
  pose (0 = T-pose, 45 = typical A-pose).
- Built-ins: `generic-humanoid` (the 22 retarget roles, Mixamo aliases) and `vrm` (adds eyes, jaw, 30 finger bones,
  T-pose within 10 degrees). A project's own profile (for example DO27's 67-joint rig) is a file the project owns.
