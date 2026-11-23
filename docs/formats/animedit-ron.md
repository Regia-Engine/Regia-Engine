# `.animedit.ron` — animation authoring sidecar (format v1)

Loader: `regia_animation::authoring::AnimEditDocument::from_ron_str`. Written by the editor's
Clip and Skeleton panels (and `editor.anim_clip` / `editor.anim_skeleton`) next to the rig's
source: `Fox.glb` → `Fox.glb.animedit.ron`. Frozen like every content format
(`docs/specs/12-assets.md` § 4a): the loader peeks `format_version`, rejects newer versions, runs
the registered migrators (none yet), and `cargo xtask check-formats` hashes the serde shape
(`animedit` in `docs/formats/format-hashes.ron`). `regia upgrade-project` knows the suffix.

```ron
(
    format_version: 1,
    // Added or replaced by name on the imported skeleton (joint must exist).
    sockets: [
        (name: "weapon", joint: "hand_r",
         offset: (translation: (0.0, -0.05, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0))),
    ],
    // Two-bone chains for the graph's TwoBoneIK nodes: [root, mid, tip], each the parent of the next.
    ik_chains: [
        (name: "LegL", joints: ("thigh_l", "shin_l", "foot_l"), pole_param: Some("LegLPole")),
    ],
    clips: [
        (
            clip: "Walk",
            // Replaces the imported clip's notifies (the editor copies them in on the first edit).
            notifies: [
                (time: 0.25, name: "Footstep_L", payload: None),
                (time: 0.75, name: "Footstep_R", payload: Some("(volume: 0.8)")),
            ],
            // Named float curves; baked to linear keys at the clip's sample rate plus every key.
            curves: [
                (name: "FootPlant_L", curve: (keys: [
                    (location: 0.0, value: 1.0, kind: Cubic(left_tangent: 0.0, right_tangent: 0.0)),
                    (location: 0.5, value: 0.0, kind: Linear),
                ])),
            ],
            // Property tracks by reflected path (non-skeletal animation).
            property_tracks: [],
            // Root-motion extraction override: None | XZ | XYZ | Yaw (None = keep the import's).
            root_motion: Some(XZ),
        ),
    ],
)
```

Curve key kinds: `Constant` (hold until the next key), `Linear`, `Cubic(left_tangent,
right_tangent)` — Hermite with slopes in value per second (a cubic key into a constant or linear
key arrives with slope 0). Applying: `AnimEditDocument::apply_to_clip` (notifies replaced and
sorted; curves and property tracks added or replaced by name/path; root-motion mode set, adding a
root-joint track for a clip without one) and `apply_to_skeleton` (sockets on unknown joints are
skipped and reported).

The editor also writes `<graph>.animgraph.layout.ron` next to each graph: canvas positions keyed
by runtime node path (`root/inner/state:Idle`; state boxes as `…/state:Idle@state`). It is
editor-only, unversioned beyond `format_version: 1`, and safe to delete (nodes are re-laid out).
