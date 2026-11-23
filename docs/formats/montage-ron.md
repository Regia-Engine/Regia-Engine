# `.montage.ron`: montage (format v2)

Loader: `regia_animation::Montage::from_ron_str` (validates names, times and links; newer versions are rejected).
Spec: `docs/specs/17-animation.md` § 4b (ADR-1465). Frozen like every content format (`docs/specs/12-assets.md`
§ 4a): `format_version`, a migrator table (v1 → v2, ADR-1645: identity, every v2 field has a default) and the `montage` entry in `docs/formats/format-hashes.ron`;
`regia upgrade-project` knows the suffix. Every field except `name`, `tracks`, the segments' `clip`, the sections'
`name`/`start`, the notifies' `name`/`time` and the windows' bounds has a default.

```ron
(
    format_version: 2,
    name: "attack",                       // library key
    rate: 1.0,                            // multiplied by the play call's rate
    tracks: [                             // one per slot; track 0 = main (root motion, slot group)
        (slot: "full_body", segments: [
            (clip: "Windup"),                                   // start 0, end = clip end, rate 1, loops 1
            (clip: "Swing", start: 0.1, end: Some(0.9), rate: 1.2, loops: 1),
        ]),
        (slot: "face", segments: [(clip: "Shout")]),
    ],
    sections: [                           // strictly increasing, first at 0; none = one section "main"
        (name: "windup", start: 0.0),                       // next: Continue (default)
        (name: "loop", start: 0.5, next: Jump("loop")),     // loops until relinked
        (name: "finish", start: 1.0, next: Stop),
    ],
    notifies: [
        (name: "swoosh", time: 0.55),                           // instant
        (name: "hitbox", time: 0.6, duration: 0.25, payload: Some("(damage: 20)")),  // window
        (name: "combo_input", time: 0.9, branch: true),         // branching point (no duration)
    ],
    blend_in: (time: 0.2, curve: Smooth),     // Linear | Smooth
    blend_out: (time: 0.2, curve: Smooth),
    blend_out_lead: None,                 // seconds before the end the auto blend-out starts (None = blend_out.time)
    auto_blend_out: true,                 // false: hold the last pose until stopped
    root_motion: true,                    // the main track's root motion drives the rig
    root_motion_off: [(start: 1.2, end: 1.4)],
    warps: [(target: "enemy", start: 0.2, end: 0.6, translation: true, vertical: false, rotation: true,
             // v2 (all optional; the defaults reproduce v1 behavior):
             mode: Similarity,               // Similarity | PathScale (scale the path without turning it)
             rotation_kind: Auto,            // Auto | MatchTarget | FaceTarget
             rotation_fraction: 1.0,         // (0, 1]: the turn completes after this fraction of the window
             anchor: Root,                   // Root | Bone("hand") | Offset((0.0, 0.0, -0.8)) (rig space)
             tint: None)],                   // editor timeline color, [r, g, b]; ignored at runtime
    rate_curve: None,                     // v2: forward playback integrates it (RateCurve::advance), reverse uses rate_back (spec 34 M7):
                                          // Some((keys: [(0.0, 1.0), (0.5, 2.0)], interpolation: Linear | Step))
    sync_group: Some("gait"),             // while blending out, the main track leads this .sync.ron group
)
```

Validation errors: empty name, no tracks or an empty track, two tracks on one slot, a non-finite or non-positive
rate, `loops: 0`, `end <= start`, sections not strictly increasing from 0 or sharing a name, a jump to an unknown
section, negative or non-finite times, a branching point with a duration, windows that end before they start. Clip
names resolve at play time (`MontageError::MissingClip`).

v2 also lets a section or notify carry `follow: Some((track: 0, segment: 1, mode: KeepTime | ShiftWithStart |
ScaleWithLength))`, an editing link the runtime ignores (spec 34 M8): `Montage::edit_segment` / `apply_edit` and
`anim.montage_edit` move linked items when a segment is trimmed, reordered, inserted or removed (ADR-1870). Extra v2 validation errors: `rotation_fraction`
outside `(0, 1]`, a non-finite `Offset` anchor, an empty `Bone` name, a bad rate curve (`montage_bad_rate_curve`:
empty, non-increasing times, non-positive rates), a `follow` naming a missing track or segment. A bone anchor whose
bone the skeleton lacks logs `montage_unknown_anchor_bone` once and anchors on the root.
