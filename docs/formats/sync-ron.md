# `.sync.ron`: sync groups and sync markers (format v1)

Loader: `regia_animation::SyncSetup::from_ron_str` (validates names and marker times, sorts markers; newer versions
are rejected). Spec: `docs/specs/17-animation.md` § 4b (ADR-1465). Frozen (`docs/specs/12-assets.md` § 4a):
`format_version`, no migrators yet, the `sync` entry in `docs/formats/format-hashes.ron`; `regia upgrade-project`
knows the suffix. Set it on an animator with `Animator::set_sync_setup`.

```ron
(
    format_version: 1,
    groups: [
        // Clip graph nodes with `sync_group: Some("gait")` join this group.
        (name: "gait", roles: [("StartWalk", AlwaysLead), ("Limp", NeverLead)]),   // others: MayLead
    ],
    markers: [
        (clip: "Walk", markers: [(name: "left", time: 0.0), (name: "right", time: 0.52)]),
        (clip: "Run", markers: [(name: "left", time: 0.05), (name: "right", time: 0.33)]),
    ],
)
```

A follower uses marker sync when it shares at least two marker names with the leader (only shared names count),
else normalised-phase sync. Validation errors: an empty or duplicate group name, a clip listed twice in a group or
in `markers`, an empty marker name, a negative or non-finite marker time.
