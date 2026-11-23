# `.motiondb.ron`: motion-matching databases (format v2), and the cooked database

Loader: `regia_animation::motion_matching::MotionDatabaseSource::from_ron_str`; build:
`MotionDatabaseSource::build(&skeleton, |clip_path| ...)`. Spec: `docs/specs/52-motion-matching.md` § 4 (ADR-1831).
Frozen (`docs/specs/12-assets.md` § 4a): `format_version`, the `motiondb` entry in
`docs/formats/format-hashes.ron`; `regia upgrade-project` knows the suffix and migrates v1 to v2.

**v2 (ADR-2858)** adds `schema.contacts` (contact labels and gait phase, below) and the weights `foot_contact` and
`foot_phase` (default 0). A v1 file is a valid v2 file without them, so the migration
(`regia_animation::motion_matching::asset::migrate_motiondb`) only rewrites `format_version`; the database a v1 file
builds is unchanged bit for bit.

```ron
(
  format_version: 2,
  skeleton: "characters/hero.skeleton",
  schema: (
    root_joint: "hips",
    forward: (0.0, 0.0, 1.0),                 // default +Z
    feet: ["left_foot", "right_foot"],
    trajectory_times: [0.333, 0.667, 1.0],    // default; at most 16
    history_times: [0.25, 0.5],               // optional; at most 8
    sample_rate: 30.0,                        // default
    weights: ( foot_velocity: 0.5 ),          // any group, default 1 each, 0..1000
    normalization: group_deviation,           // or per_dimension, none
    pose_joints: [ ( joint: "right_hand", velocity: false, weight: 0.5 ) ],
    sample_weights: ( trajectory: [2.0, 1.0, 1.0] ),   // one per sample time, 0..100
    // v2, optional: per-foot contact labels (and the gait phase) derived at build time
    contacts: Some(( height_on: 0.02, height_off: 0.05, speed_on: 0.15, speed_off: 0.35, phase: true )),
  ),
  tags: ["walk", "run", "stop"],              // at most 32, bit order
  mirror_pairs: [("left_foot", "right_foot"), ("left_hand", "right_hand")],   // for mirrored entries
  entries: [
    ( clip: "anim/walk.clip", looping: true, tags: ["walk"],
      spans: [ ( kind: landing_bias(-0.5), range: (0.0, 0.2) ),
               ( kind: tag(["stop"]), range: (0.5, 0.6) ) ] ),
    ( clip: "anim/run.clip", looping: true, tags: ["run"], trim: (0.1, 0.2) ),
    ( clip: "anim/stop.clip", tags: ["stop"], distance_matched: true ),
  ],
)
```

- Joints are names, resolved against the skeleton at build time; a missing name is
  `MotionMatchError::UnknownJoint`. Clip paths are asset paths looked up by the caller (`UnknownClip { entry }`).
- Span kinds: `pass_through`, `unindexed`, `landing_bias(b)` with `b` in `[-100, 100]`, `tag([names])`. Ranges are
  half-open clip seconds. `trim: (start, end)` makes that many seconds at each end pass-through (played, never a jump
  target).
- `mirrored: true` adds the entry again as `<clip>.mirrored`, left and right swapped through `mirror_pairs` and the
  lateral (X) axis negated; the skeleton must be mirror-symmetric (`BadMirrorTable` otherwise). Without
  `mirror_pairs` it fails with `NoMirrorTable { entry }`.
- `distance_matched: true` marks a stop, pivot or landing for the player's distance matching.
- `contacts` (v2): every foot of every frame is labelled planted while its height above its lowest point in the clip
  is at most `height_on` and its speed at most `speed_on`, and stays planted until the height exceeds `height_off` or
  the speed `speed_off` (hysteresis; each `*_off` at least its `*_on`, all finite and not negative, else
  `BadContacts`). The labels are the `foot_contact` group (1 float per foot). With `phase: true` the `foot_phase` group
  adds the phase of each foot's step cycle, touchdown to touchdown, as `(cos, sin)` (2 floats per foot; `(0, 0)` where
  no cycle is defined). Both groups come after every other group and weigh 0 unless `weights` sets them, so adding
  `contacts` never changes a search by itself; the labels also drive the runtime foot lock.

## Cooked database

A cooked `.fckd` file with the type tag `regia_animation::cooked::motion_database_type_tag()`
(`TypePathHash::of("regia_animation::MotionDatabase")`); loader `regia_animation::cooked::load_cooked_motion_database`,
payload API `MotionDatabase::{to_cooked_payload, from_cooked_payload, cooked_payload_version}`.

Payload: postcard, little-endian. `format_version: u32 = 3` is the first field (distinct from the container's
version), then the resolved schema (v2: ten group weights and the optional contact settings `(height_on, height_off,
speed_on, speed_off, phase)`), clip ranges (with the distance-matched flag, source entry and mirrored flag), frame refs, per-frame tags, biases
and flags, raw features, per-dimension mean and scale, the k-d tree arrays (leaf size, depth, row order, nodes with
their pivot rows), the v3 optional arrays (below), per-tag frame counts, the target count, the most negative bias, build warnings, 16 evenly
spaced normalized rows and the mirror partner of every joint (empty without mirrored ranges); then the 64-bit FNV-1a hash of everything before it as the last eight bytes
(`MotionDatabase::content_hash`).

Normalized features are not stored. The loader checks the hash, refuses a newer `format_version`
(`UnsupportedFormatVersion`), recomputes the normalized rows with the build's own `(raw - mean) * scale` code
(bit-exact), checks the 16 stored rows bit for bit and every array's shape and index range, and rebuilds the tree from
its arrays (each split value must equal its pivot row's coordinate). Any failure is `CorruptDatabase`.

The cook is deterministic: the same inputs on the same toolchain give the same bytes. Raw features come from
trigonometry and matrix inverses whose last bits can differ between platforms' math libraries, so the bytes are exact
on the cooking machine and within 1e-4 relative per feature across machines; runtimes never recompute raw features.
The loader still reads a v1 payload (eight weights, no contacts) and a v2 payload as the same database: it computes
the clip speeds from the stored raw features, and the player samples the clips for the arc lengths on first use.

**v3 (ADR-7817): optional arrays.** After the k-d tree arrays: `arc_lengths`, a list of `(clip, distances, speeds)`
for the distance-matched clips in clip order (`distances` is the cumulative ground distance of the root path from the
clip's first frame, meters, starting at 0 and never decreasing; `speeds` the ground speed at each frame from its
neighbors' positions, m/s; one entry per frame of the clip each), then `clip_speed`, one float per database frame
(`sqrt(vx·vx + vz·vz)` of the frame's raw root velocity, m/s). An empty list means "not stored". The runtime reads
them when present and computes the same values when absent (`MotionDatabase::clip_speed`,
`MotionMatchPlayer::arc_table`), so results do not depend on them. A source build stores both; the loader checks their
shape (`CorruptDatabase` otherwise).

Migration: `regia upgrade-project` rewrites a v1 or v2 cooked database (`.fckd` of the motion database type) at v3
with its header kept and the clip speeds added (`regia_animation::cooked::migrate_cooked_motion_database_file`,
payload `regia_animation::motion_matching::migrate_cooked_motiondb`). The arc lengths need the clips: the next
`regia cook`, or `MotionDatabase::bake_arc_lengths(skeleton, clips)` in a host, adds them; until then the player
samples the clips as before.
