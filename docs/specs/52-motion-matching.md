# Spec 52: Motion matching

Status: proposed (ADR-1579). Target rung: v0.4.x (G13, crate `regia_animation`). A baseline already
ships in the repo (non-learned motion matching, k-d tree search, runtime matcher, one read-only agent
method); this spec records it in § 2.1 and specifies what is missing or must be hardened.

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_animation::motion_matching` (ADR-1478), implemented, spec records gaps (§ 2.1).

Source area: runtime-animation/runtime-animation-pose-search (ids only; no text carried over).

## 1. Purpose and scope

Motion matching plays locomotion (and other continuous motion) by searching recorded animation
instead of walking a hand-authored graph. Every few frames the runtime builds a query describing
the character's current pose and the path the game wants, finds the database frame whose features
are closest, and continues playback from there. Adding clips widens what the character can do
without adding transitions.

This spec covers: the feature schema (channels), normalization and weights, the clip database and
its tag/mask metadata, exact search and its acceleration, search cadence and continuity bias, the
hand-off to inertialized transitions, optional stride/orientation/distance warping of the matched
pose, a deterministic build and a cooked file format, a debug view, the agent surface, tests and a
performance budget.

It is not: learned (neural) compression of the database, graph/state-machine authoring (Spec 17),
montage triggering, character-movement or trajectory-prediction code (game code writes the desired
path), retargeting, or full-body IK beyond the foot contact hooks that already exist. Approximate
search is explicitly not part of the exactness contract (§ 2.7) and is a follow-up.

## 2. Regia terms and model

Glossary terms used: clip, pose, skeleton, root motion, inertialization, asset, cooked file,
component, resource, agent method. New nouns: **channel**, **feature vector**, **database**,
**span**, **tag**, **query**, **candidate**, **continuation**.

### 2.1 Baseline versus new (what exists)

| Area | Baseline in the repo | Status |
|---|---|---|
| Feature groups | Five fixed groups: root velocity, future trajectory position, future trajectory facing, foot position, foot velocity; all in the character frame (ground-projected root, turned to root forward) | exists |
| Schema | Root joint, forward axis, foot joint list, trajectory times (default 1/3, 2/3, 1 s), sample rate (default 30), per-group weights; validation with typed errors | exists |
| Normalization | Mean removal per dimension, divide by the group's pooled deviation, multiply by group weight; constant group divides by 1 | exists |
| Database | Built in memory from clips + looping flag; looping clips continue future samples into the next lap, non-looping clamp; central-difference velocities | exists |
| Search | Brute force and an exact k-d tree (leaf size 8, median split on widest spread); lowest frame index wins ties; exclusion spans | exists |
| Matcher | Search interval, minimum jump interval, transition cost added to non-continuation candidates, exclusion window around the playing frame, first pick from a mean-pose query, re-search at end of a non-looping clip | exists |
| ECS | `MotionMatching` component (shared `Arc` database, matcher, desired trajectory, last step/error) and a `step_motion_matching` system that hosts register themselves | exists |
| Agent | `anim.motion_match` (read): database summary, settings, playback, last search, probe by speed or trajectory | exists |
| Docs | `book/user-guide/motion-matching.md`, `book/agents/motion-matching.md`, tests in the crate | exists |
| Persistence | None: the database is rebuilt from clips ("not a file format") | new (§ 4) |
| Channels | Fixed five groups only | new (§ 2.2) |
| Past trajectory, extra joints, per-sample weights | None | new (§ 2.2, § 2.3) |
| Tags, spans, masks, cost bias | None ("tags not built") | new (§ 2.5) |
| SIMD brute force, filtered-search strategy, `Auto` | Brute force is scalar; tree only | new (§ 2.7) |
| Continuity: biases, reselect window, urgent search, stagger | Interval, min-jump, window only | new (§ 2.8) |
| Transition hand-off and root motion output | Host does it by hand | new (§ 2.9) |
| Stride, orientation and distance warping | None | new (§ 2.10) |
| Candidate list and per-group costs, debug gizmos, edit methods | None | new (§ 2.11, § 5) |

As built (ADR-7241, 2026-11-12): the separate offline, library-only `regia_animation::pose_search`
module (ADR-4869; not the runtime matcher above) follows the two rules this table records for the
matcher. Its trajectory offsets are metres in a frame built from the root's position and rotation,
with the root's scale left out (the character frame here carries no scale either; a centimetre rig
used to store 100 per metre). Its `SearchState` treats a non-looping clip that has played to its
clamped end as having no continuation, as the matcher's end-of-clip re-search does, so playback no
longer sticks on the end pose.

### 2.2 Channels and feature vectors

A feature vector has a fixed length decided by the schema. The schema is a list of **channels**.
Each channel contributes a contiguous slice of floats and belongs to one **normalization group**.
The five baseline groups remain the default and keep their names and order, so existing databases,
tests and the agent output are unchanged. Two additions, both optional (absent = baseline layout
byte for byte):

1. **Trajectory history** (`trajectory_history`): positions (2 floats each, character frame) of the
   root at the given **past** times (`history_times`, seconds before now, each finite and above
   zero, at most 8). Past samples of a frame near the start of a non-looping clip clamp to the clip
   start; of a looping clip they wrap into the previous lap (same lap-transform rule as the future
   samples). At runtime the matcher keeps a ring buffer of the character's actual ground positions
   and facing, sampled every update, from which it reads the query's past samples; before the
   buffer is full it extrapolates backward at the current velocity.
2. **Extra pose joints** (`pose_joints`): a list of `{ joint, position: bool, velocity: bool }`
   entries (hands, hips, head) beyond the feet. Positions are relative to the ground point, velocities
   are in the character frame. Layout: all extra positions, then all extra velocities, appended after
   the baseline groups. They form groups `pose_position` and `pose_velocity`.

Limits: at most 64 joints named across `feet` and `pose_joints`, at most 16 future plus 8 past
samples, at most 192 floats per vector (the lane path pads to a multiple of 8, § 2.7).
Validation errors extend the existing typed error enum (never panics): `BadHistoryTime`,
`TooManyDimensions`, `DuplicateJoint`.

Edge cases: a schema with zero feet and zero extra joints is valid (trajectory-only matching); a
group with weight 0 is kept in the vector (so the debug view can still show it) but contributes
nothing to cost; a group whose deviation is below 1e-6 is a constant group and is divided by 1.

### 2.2a Contact labels and gait phase (ADR-2858)

Optional schema field `contacts` (`ContactSettings`): at build time each foot of each frame is labelled planted
(enter at height above the clip's lowest foot point ≤ `height_on` and speed ≤ `speed_on`, leave above `height_off`
or `speed_off`; a looping clip runs a warm-up lap). Group `foot_contact` (1 float per foot) and, with `phase`, group
`foot_phase` (cos, sin of each foot's touchdown-to-touchdown phase; `(0, 0)` undefined) come after every other group.
Their weights default to 0, so an unset channel leaves every search bit identical (appended zero-weight dimensions
add exact zeros to the canonical lanes). The labels also drive the runtime foot lock (`FootLocker`).

### 2.3 Normalization and weights

Cost of frame `f` against a normalized query `q`: `cost(f) = Σ_d (q_d − n_{f,d})²` where
`n_f = (raw_f − mean) · scale`.

- **Normalization mode** (schema field `normalization`): `group_deviation` (baseline, default),
  `per_dimension` (each dimension divided by its own deviation; use only when dimensions within a
  group are unrelated), `none` (mean-centered only, scale = weight; for tests and hand-tuned
  data). Statistics are always computed over **all** frames of the database in frame order using
  `f64` accumulators, never over a tag-filtered subset, so a filter changes which frames are
  eligible but never any cost.
- **Group weight** (baseline): multiplies the normalized group; cost scales with its square. Range
  `[0, 1000]`, finite.
- **Sample weights** (new): a per-sample multiplier inside the trajectory groups (and the history
  group), a list the same length as the sample list, default all 1, each in `[0, 100]`. Applied
  with the group weight (`scale_d = group_weight · sample_weight / deviation`). This lets the
  near future count more than the far future without changing the group's balance against the feet.
- **Joint weights** for `pose_joints`: one multiplier per entry, same rule.
- Weights cannot repair data that does not contain the motion being asked for; the debug view
  (§ 2.11) is how an author finds that out. The spec does not clamp or auto-tune weights.

The effective per-dimension `mean` and `scale` are part of the database (inspectable, and stored in
the cooked file for verification).

### 2.4 Database

A database binds one skeleton, one schema, and an ordered list of **entries**: a clip reference,
a looping flag, a tag set, an optional mirror flag (below) and a list of spans. Frames are laid out
clip by clip in entry order at the schema sample rate (existing frame-count rules). Frame ids are
`u32`, so a database has at most 2^32 − 1 frames; a practical authoring guideline is 200,000 frames
(§ 10).

- **Mirror** (`mirrored: true` on an entry): the entry is added a second time with left/right joint
  pairs swapped and the lateral axis negated, using the skeleton's mirror table (a list of joint
  pairs, in the skeleton asset or the database source). Mirrored frames carry a `mirrored` bit so
  the player applies the same mirroring at playback. If no mirror table exists the build fails with
  `NoMirrorTable`.
- **Clip validity**: clips must carry root motion in the root joint's tracks; a clip with no root
  displacement at all is allowed (idle) but a build **warning** is attached to the database
  (`warnings` list, also in the agent output) when every foot velocity of a clip is within 1e-4 of
  zero for more than 90 % of frames and the root velocity is not, which usually means missing
  root motion.
- **End trim**: per entry `trim_start`/`trim_end` seconds (default 0) exclude frames at the clip
  ends from being search targets (they are still sampled for features of neighbors).

### 2.5 Tags, spans and masks

**Tag table**: up to 32 named tags per database (`"walk"`, `"stop"`, `"injured"`), each a bit of a
`u32` mask. An entry has a tag mask; a span may add tags for a time range.

**Span** (per entry, half-open `[start, end)` in clip seconds):

| Span kind | Effect on search |
|---|---|
| `pass_through` | Frames may play through the span but are never chosen as a jump target |
| `unindexed` | Frames are never targets and never counted as eligible, but still sampled for neighbors' features; the tree keeps their rows and the leaf predicate skips them (§ 2.7.4, ADR-5639) |
| `landing_bias(b)` | Adds `b` (finite, `[−100, 100]`) to the frame's cost when it is a jump target; negative favors it |
| `tag(mask)` | ORs the mask into the frames' tag mask |

Overlapping spans combine: biases add, masks OR, `unindexed` and `pass_through` are sticky. The
per-frame result is baked at build into three arrays: `tags: u32`, `bias: f32`, `flags: u8`
(`pass_through`, `unindexed`, `mirrored`).

**Query filter** (`SearchFilter`): `require: u32` (frame must have all these tag bits), `forbid: u32`
(frame must have none). `require = 0, forbid = 0` accepts everything. The matcher's own exclusion
window (existing) and the reselect window (§ 2.8) are applied on top. Filters are per search and
are set by game code (for example "only stop clips while braking") or by `anim.motion_match_set`.

Edge cases: a filter that leaves no eligible frame yields no candidate and the matcher keeps
playing the continuation (no error; `last_search.best = null`, reason `no_candidates`). If nothing
is playing yet and the filter leaves nothing, the matcher ignores the filter for the first pick and
records a warning (`filter_ignored`) so the character is never left without a clip.

### 2.5a Montage scopes (ADR-2858)

`MotionMatcher::scope` is a second `SearchFilter` ANDed with the filter; `MotionMatchMontageScope` sets it while a
montage plays a listed section (blend-out ignored). Strict: no eligible frame means `no_candidates`.

### 2.6 Query construction

A query has the same shape as one frame: root velocity, a trajectory point per future time, an
optional history position per past time, feet and extra joint features. The runtime builds it as:
pose features (root velocity, feet, extra joints, history) are read from the **playing frame's**
stored features (baseline behavior), blended with the character's live values by
`live_pose_blend ∈ [0,1]` (default 0 = playing frame only, baseline; 1 = live values from the
skeleton's current pose, which the host supplies as a `LivePose` struct). The trajectory is always
the game's desired path. The blend is a plain linear interpolation of the raw features before
normalization.

### 2.6a Trajectory prediction (ADR-2858)

`TrajectoryPredictor` makes the trajectory from a desired velocity: critically damped springs on the ground velocity
and the facing yaw, evaluated in closed form at each trajectory time, plus a history buffer. `FacingMode::Aim` with
`aim_yaw` strafes. `desired_turn_rate` says the desired velocity is turning at a constant rate (clamped to
`MAX_TURN_RATE`, 100 rad/s), which still has a closed form, so a steady turn predicts a curved path. Game code may
still write `MotionMatching::trajectory` by hand when no predictor is present.

### 2.7 Search and the exactness contract

`SearchMethod` becomes `BruteForce`, `KdTree`, `Auto` (default). All three are **exact**: for the
same database, normalized query, filter and exclusion, they return the same frame and a
bit-identical cost.

1. **Canonical distance**. Squared distance is computed with 8 independent `f32` lane accumulators
   over the padded vector (padding lanes are zero in both operands), lane `j` accumulating
   dimensions `j, j+8, j+16, …` in increasing order using separate multiply and add (no fused
   multiply-add), then reduced as `((l0+l4)+(l2+l6)) + ((l1+l5)+(l3+l7))`. This is the **only**
   cost function; scalar, SSE, NEON and wasm lanes must reproduce it bit for bit, which the
   lane layout makes possible. A scalar reference implementation is kept and is what the unit tests
   compare against.
2. **Biased cost**. The search compares `cost(f) + bias(f)` (bias 0 for frames without a
   `landing_bias`). Ties on the biased cost go to the lowest frame index.
3. **Brute force** scans eligible frames, with an early-out of a partial distance when the running
   sum of completed lanes already exceeds the best biased cost minus the frame's own bias (the
   bound uses a conservative `1 + 2^-20` slack so the early-out can never change the winner).
4. **k-d tree** (Bentley 1975; Friedman, Bentley, Finkel 1977): median split on the widest-spread
   dimension, ties on the split value ordered by frame index, leaf size 8 (configurable 4..32).
   Nearest-first descent, far side visited only when the splitting-plane distance squared, after
   subtracting the largest negative bias present in the database (stored per tree), exceeds the best
   biased cost times `(1 + 2^-20)`. The slack makes pruning conservative against rounding, so the
   result equals brute force exactly. Eligibility (filter, exclusion, `unindexed`, `pass_through`) is a
   per-row predicate checked at leaves; internal pruning does not depend on it.
5. **Auto**: brute force when the eligible fraction is below 1/8 or the database has fewer than
   2,048 frames; otherwise the tree. Because every method is exact, `Auto` is a pure speed choice
   and never changes results. The eligible-fraction estimate comes from per-tag counts stored in the
   database, not from scanning.
6. **Dimensionality note**. At 30 to 60 dimensions a tree prunes weakly on dense data; its
   benefit is data dependent. The performance acceptance (§ 10) therefore measures both on a
   representative database and records which wins; the tree must never be slower than brute force
   by more than the build-time `auto` threshold allows.
7. **Top-k**. `search_top_k(normalized, filter, exclusion, k)` (k ≤ 32) returns the k cheapest
   distinct eligible frames in increasing biased cost, ties by frame index. It is used by the debug
   view and by tests; the tree implementation keeps a bounded max-heap and prunes against its
   current k-th best.
8. **Approximate search** (PCA projection, product quantization, learned compression) is out of scope;
   if added later it is a separate `SearchMethod` variant that does not claim the contract above.

### 2.8 Cadence and continuity

Baseline settings stay (`search_interval`, `min_jump_interval`, `transition_cost`, `exclude_window`,
`method`). New settings, all with defaults that reproduce baseline behavior when left unchanged:

| Setting | Default | Meaning |
|---|---|---|
| `stay_bias` | 0 | Added to the continuation's cost; negative holds the playing clip longer, positive releases sooner |
| `loop_clip_bias` | 0 | Added to every candidate that belongs to a looping entry |
| `lockout_window` | 0 s | Frames chosen as a jump target within the last N seconds are not candidates again |
| `urgent_threshold` | off | If the normalized trajectory-group distance between the query now and the query at the last search exceeds this, search at the next update regardless of the interval (never earlier than `min_search_gap`, default 0 s = the next update, ADR-5639) |
| `stagger` | on | The first search interval is offset by `(entity index mod 8) / 8 · search_interval` so many characters do not search on the same frame; deterministic from the entity's index |
| `max_searches_per_frame` | 0 (unlimited) | A resource-level cap; characters over the cap defer their search to the next frame, oldest-deferred first (stable by entity bits) |
| `play_rate` | 1 | Multiplier applied to playback time; `jump_threshold_same_clip` inherits `exclude_window` |

Jump rule (replaces the single expression in the baseline, equal to it at default settings):
let `C = cost(continuing) + stay_bias` (absent after a non-looping clip ended), and for the
best eligible candidate `B = cost(b) + bias(b) + loop_clip_bias(b) + transition_cost`. Jump when
`B < C` and the minimum jump interval has passed; a jump is also allowed when nothing plays or the
clip ended. A candidate that beats `C` while the interval has not passed sets `held` (existing).
Sanitization (non-finite or negative values replaced by zero) applies to the new fields as well,
except the signed biases, which are clamped to `[−100, 100]` and replaced by 0 if non-finite.

State table of the matcher:

| State | Event | Next state | Notes |
|---|---|---|---|
| `Idle` (nothing playing) | update | `Playing` | first pick from mean-pose query, filter ignored if empty |
| `Playing` | update, search not due | `Playing` | advance time, wrap or clamp |
| `Playing` | search due, no better candidate | `Playing` | `held` or `no_candidates` recorded |
| `Playing` | search due, `B < C`, interval passed | `Jumping` | emits a `JumpEvent` |
| `Jumping` | next update | `Playing` | the host's inertializer owns smoothing (§ 2.9) |
| `Playing` | non-looping clip ends | `Playing` | immediate search; stays on last frame if no candidate |
| any | `reset()` or database swap | `Idle` | counters kept except `since_*` |

### 2.9 Transitions and root motion

Matching only picks frames; smoothness comes from **inertialization** (`regia_animation::inertialize`,
Bollo, GDC 2018). New in this spec is the glue:

- A jump produces `JumpEvent { from: Option<Playback>, to: Playback, cost, mirrored }` (`from` is `None` for the
  first pick, ADR-5639), published as an ECS event.
- `MotionMatchPlayer` (a component + system, opt-in like the matcher) owns an `Inertializer`, samples
  the target clip at the target time, and on a jump calls the inertializer's `start` with the pose
  currently shown as the source and the target pose as the target (it also supplies the previous-frame
  poses so velocities carry over). Blend duration is
  `lerp(duration_min, duration_max, saturate(jump_cost / duration_cost_ref))`, defaults 0.1 s, 0.25 s
  and the database's median candidate cost, so near-identical poses blend quickly and large
  jumps blend longer. A same-clip small time skip (|Δt| ≤ 2 frames) uses `duration_min` and the spring
  decay; others use the quintic decay (the existing `DecayMethod`).
- **Root motion output**: the player reports `RootMotionDelta { translation: Vec3, yaw: f32 }` for
  the frame, taken from the root joint's change between the previous and current clip sample in the
  character frame (including lap wrap and mirrored sign), so a character controller can apply it
  directly or ignore it and let its own velocity drive. Jumps never contribute a spurious delta (the
  delta is computed within one clip at one time step).
- Contract: on the frame of a jump, the inertialized output pose equals the pose shown on the
  previous frame up to the offset decay (the inertializer's own guarantee), and the pre-jump
  velocity is carried; the player tests assert pose continuity (§ 8).

### 2.10 Warping (optional post stage)

Three independent, opt-in corrections applied to the matched, inertialized pose. Each has a
`strength ∈ [0,1]` (default 0 = off) and a deterministic, fixed-step response.

- **Stride warping**: scale playback rate and foot spacing so the clip's speed meets the character's
  actual ground speed. `ratio = actual_speed / clip_speed(frame)` where `clip_speed` is stored per
  frame (root speed, from the database); below `min_speed` (default 0.1 m/s) the warp is off. The
  ratio is clamped to `[ratio_min, ratio_max]` (defaults 0.8, 1.25) and smoothed with a critically
  damped spring (stiffness `StrideWarpSettings::response`, default 12 per second; ADR-5639). The pose side moves each IK foot
  target toward/away from the hips along the movement direction by `(ratio − 1)` of the stride length
  and then runs the existing foot/leg IK to solve the legs; pelvis height is lowered when the legs
  would overextend, never raised. Playback-rate-only mode (no IK) is allowed (`stride_mode: rate`).
- **Orientation warping**: rotate the lower body toward the desired movement heading by
  `delta = wrap(desired_heading − clip_heading)` clamped to `±max_delta` (default 90 degrees; 0
  disables), distributing the counter-rotation across a spine chain (`spine_joints`, weights from
  low to high so the upper body keeps facing; sum of weights 1) and applying the leg rotation as
  IK foot-root yaw. A delta above the limit is clipped, not wrapped.
- **Distance matching** (for stops, pivots and landings): for a non-looping entry flagged
  `distance_matched`, the database stores a monotone arc-length table of the root path (cumulative
  ground distance per frame, 1 float per frame). When the game supplies `distance_to_target`, the
  player chooses the clip time whose remaining path length equals the supplied distance (binary search
  on the table, linear interpolation), instead of advancing time by `dt`; if the distance grows
  (target moved away) playback holds. A `stop_speed_threshold` (default 0.05 m/s) defines which
  frames count as stopped, and the table ends at the first stopped frame when `stop_at_end` is false,
  or at the last frame when true.

These stages reuse the foot and chain IK modules and never change search results. Montage-scoped
root warping (Spec 17 § 4b) is separate and unchanged.
The stride and orientation code lives in a crate-private module shared with the animation graph's `StrideWarp` and
`OrientationWarp` nodes (spec 17 § 3, ADR-8540); `motion_matching::warp` stays its public path, unchanged, same bits.

### 2.11 Debug view

A **candidate report** for the latest search: for each of the top-k candidates (default 8) plus the
continuation: frame, clip, time, mirrored, total biased cost, the cost split per group (the sum of
squared normalized differences over the group's slice, so the group costs add up to the raw feature
cost), the transition cost, the bias terms and a status (`best`, `continuing`, `pass_through`,
`locked_out`, `near_playing`, `filtered`). The report is produced only when requested (a flag on the
component or the agent call) because it costs one extra top-k search.

Viewport gizmo group `motion_matching` (off by default, toggled with `debug.set_gizmos`):
desired trajectory as a polyline with facing ticks, the matched frame's own trajectory (gray), foot
position spheres, and the playing frame's label. Editor: a table of the candidate report with a
per-group stacked bar (§ 6).

## 3. API shape in Rust

Crate `regia_animation`, module `motion_matching` (existing file plus a directory; new files listed).
Layer 2 (systems); depends only on lower layers already used (`regia_core`, `regia_ecs`, `regia_app`,
`regia_asset`). The agent handlers live in `regia_agent` (same-layer edge `regia_agent → regia_animation`
already exists). The editor panel lives in `regia_editor`; nothing here depends upward.

```text
motion_matching.rs      existing: schema, database, matcher, component
motion_matching/kd.rs   existing: exact tree (extended: bias bound, top-k, predicate leaves)
motion_matching/lanes.rs new: canonical distance (scalar reference + SIMD lanes), padded layout
motion_matching/tags.rs new: tag table, spans, SearchFilter
motion_matching/asset.rs new: MotionDatabaseAsset (RON source), cooked layout, migrations
motion_matching/player.rs new: MotionMatchPlayer, JumpEvent, RootMotionDelta
motion_matching/warp.rs new: stride, orientation, distance matching
motion_matching/debug.rs new: CandidateReport, gizmos
```

Sketched signatures (new or changed; bodies omitted):

```rust
pub enum NormalizationMode { GroupDeviation, PerDimension, None }
pub struct PoseJoint { pub joint: u32, pub position: bool, pub velocity: bool, pub weight: f32 }
pub struct FeatureSchema { /* existing fields */
    pub history_times: Vec<f32>, pub pose_joints: Vec<PoseJoint>,
    pub sample_weights: SampleWeights, pub normalization: NormalizationMode }
pub enum SearchMethod { BruteForce, KdTree, #[default] Auto }
pub struct SearchFilter { pub require: u32, pub forbid: u32 }
pub struct CostParts { pub total: f32, pub groups: [f32; FeatureGroup::COUNT], pub bias: f32 }
impl MotionDatabase {
    pub fn build(skeleton: &Skeleton, entries: &[MotionEntry<'_>], schema: FeatureSchema)
        -> Result<Self, MotionMatchError>;               // MotionClip stays as a thin alias
    pub fn search_filtered(&self, q: &[f32], f: SearchFilter, ex: Exclusion, m: SearchMethod)
        -> Option<SearchHit>;
    pub fn search_top_k(&self, q: &[f32], f: SearchFilter, ex: Exclusion, k: usize) -> Vec<SearchHit>;
    pub fn cost_parts(&self, q: &[f32], frame: usize) -> Option<CostParts>;
    pub fn warnings(&self) -> &[BuildWarning];
    pub fn content_hash(&self) -> u64;                   // FNV-1a over the cooked bytes (§ 7)
}
pub struct MatcherSettings { /* existing fields */ pub stay_bias: f32, pub loop_clip_bias: f32,
    pub lockout_window: f32, pub urgent_threshold: Option<f32>, pub stagger: bool, pub play_rate: f32 }
#[derive(Message)] pub struct JumpEvent { pub entity: Entity, pub from: Option<Playback>, pub to: Playback,
    pub cost: f32, pub mirrored: bool }
pub struct MotionMatchPlugin;   // registers matcher, player, warp systems and the event; opt-in
```

`MotionMatchPlugin` fixes the baseline gap that hosts register `step_motion_matching` by hand; the
old system stays public and unchanged, and registering both is a no-op duplicate guard (the plugin
detects an already-added system by a marker resource). Set ordering (ADR-4477): predictor, matcher,
player, inertialization and foot locks run in `FixedUpdate`, once per simulation step, before the
animator presentation stage in `Update`; the search budget is per simulation step. Root deltas from
multiple fixed steps in one presentation frame are composed and drained once by that stage. Components
and settings derive `Reflect` and serde (RON); the database itself is not
reflected (it is an asset, shared by `Arc<MotionDatabase>` from the asset registry).
As built (ADR-7216; deferred by ADR-1831 until then): `MotionMatching`, `MatcherSettings` and
`MotionMatchPlayer` derive `Reflect` (with `MotionMatcher`, `SearchMethod`, `TrajectoryPoint`,
`PlayerWarps`, `WarpInputs` and the warp settings), registered by `MotionMatchPlugin`. Reflected:
`matcher.settings`, `matcher.rate_scale`, `trajectory`, `ground`, `report_candidates`; the
player's blend range, `strip_root`, `warps` (without `stride_legs`) and `warp_inputs`. Not
reflected: the database, the clips and the skeleton (assets behind `Arc`), the filters, the live
pose, the latest results and the runtime state. The two components have no `FromReflect` (an
`Arc` asset has no default), so they are edited in place by path, not built from reflection.

Errors: `MotionMatchError` gains `NoMirrorTable`, `BadHistoryTime`, `TooManyDimensions`,
`DuplicateJoint`, `BadSpan`, `UnsupportedFormatVersion { found, supported }`,
`CorruptDatabase`. No `unwrap` on asset paths; load failures are values. `BadTagIndex` and `BadFilter` are not
errors (ADR-5639): a tag mask is raw `u32` bits (the table only names bits; an unknown name in a source or an agent
filter is `UnknownTag` / `unknown_tag`), and a filter whose `require` and `forbid` overlap is valid and yields
`no_candidates` like any filter that leaves nothing (§ 2.5).

## 4. Files and formats

**Source**: `*.motiondb.ron`, a RON struct with `format_version: 2` (ADR-2858). A version-1 source
is the same document without the optional contact fields; the loader reads it and rewrites the
version to 2:

```ron
(
  format_version: 2,
  skeleton: "characters/hero.skeleton",
  schema: ( root_joint: "hips", forward: (0, 0, 1), feet: ["left_foot", "right_foot"],
            trajectory_times: [0.333, 0.667, 1.0], history_times: [0.25, 0.5],
            sample_rate: 30.0, weights: ( ... ), normalization: group_deviation, pose_joints: [] ),
  tags: ["walk", "run", "stop"],
  entries: [ ( clip: "anim/walk.clip", looping: true, tags: ["walk"], mirrored: true,
               trim: (0.0, 0.0), spans: [ ( kind: landing_bias(-0.5), range: (0.0, 0.2) ) ],
               distance_matched: false ) ],
)
```

Joints are referenced by name in the source and resolved to indices at build/cook time (a missing
name is a hard error naming the entry). Clip paths are asset paths, so the importer can track
dependencies.

**Cooked**: `.fckd` with a database type tag (alongside the existing skeleton and clip tags in
`cooked.rs`), postcard payload, little-endian, `format_version: u32 = 3` as the first payload field
(distinct from the container header version). Payload order: schema (resolved), clip ranges, frame
refs, per-frame `tags/bias/flags`, raw features, per-dimension mean and scale, k-d tree arrays
(node list, row order), optional arc-length table, optional per-frame clip speed, tag counts,
warnings, then a 64-bit content hash of everything before it. Normalized features are **not**
stored: the loader recomputes them with the same `+ − ×` code as the build (bit-exact under IEEE
rules, no fused operations), then verifies the stored hash and a stored sample of 16 normalized
rows; a mismatch is `CorruptDatabase`.

**Loader and migration** (per `docs/specs/12-assets.md` § 4a): the source loader accepts
`format_version` 1 and 2. Version 1 is stamped to 2 (`migrate_motiondb` only rewrites the version;
the database a v1 file builds is unchanged). A source version above 2 returns
`UnsupportedFormatVersion { found, supported: 2 }`. A cooked payload of version 1 still loads (its
schema has no contacts); versions 1 and 2 load without the optional arrays. A cooked version above 3 returns
`UnsupportedFormatVersion { supported: 3 }`. `regia upgrade-project` runs the source migrator over `.motiondb.ron`
files and the cooked migrator over cooked databases (version 1 or 2 to 3, header kept, the clip speeds added).

*As built (ADR-7817):* cooked version 3 stores the optional arrays. The clip-speed array holds each frame's
`sqrt(vx·vx + vz·vz)` of the raw root velocity. For each distance-matched clip, the arc-length entry holds the
cumulative ground distance and the neighbor-difference ground speed per frame, two floats per frame rather than one,
because the stop cut (§ 2.10) depends on runtime settings and reads the speeds. The runtime uses them when present and
computes the same values, bit for bit, when absent: the loader computes missing clip speeds from the stored raw
features, and the player samples the clip for missing arc lengths on first use. A source build stores both, and
`MotionDatabase::bake_arc_lengths` completes a migrated database. The cooked migrator adds only the clip speeds,
because the arc lengths need the clips; the next cook adds them.

**Cook step**: runs in `regia_asset_pipeline` (desktop-only), calls `MotionDatabase::build`, writes
the `.fckd`. The runtime crates only read it. Android and Windows read the same bytes.

## 5. Agent-protocol surface

The existing method name is kept; two are added in the same `anim.` family, and the existing one
gains optional parameters (additive, old calls unchanged). Error codes follow the existing scheme
(`F0003` no entity/component, `F0002` bad parameters with a bracketed lowercase reason code).

| Method | Access | Params | Result | Reason codes (in `F0002`/`F0003` messages) |
|---|---|---|---|---|
| `anim.motion_match` (exists) | read | `entity`; optional `speed` or `trajectory` (probe, existing); new `candidates: n` (1..32), `filter: {require:[tag],forbid:[tag]}` | existing object plus `candidates` (when asked): `[ { frame, clip, clip_name, time, mirrored, total, groups: {name: cost}, bias, transition, status } ]`, `warnings`, and per-setting new fields | `bad_probe`, `bad_speed`, `bad_trajectory` (existing); `bad_candidates`, `unknown_tag` |
| `anim.motion_match_set` | play | `entity`; optional `settings` (any matcher setting), `trajectory`, `filter`, `weights` (group name to value), `search` (`now`), `reset` (bool) | `{ applied: [names], settings, filter }` | `bad_setting`, `bad_weight` (negative or not finite), `unknown_tag`, `bad_trajectory`; weights only change **runtime** query weights via the sample/group multiplier in an overlay on the database's scale (the database file is not modified) |
| `anim.motion_match_database` | read | `database` (asset path or entity) | `{ frames, dimensions, padded, groups, clips, tags: {name: count}, spans, normalization, mean_scale_hash, content_hash, kd, warnings }` | `unknown_database`, `bad_params` |

`anim.motion_match_set` is the scripted-tuning route for the agent (no scripting language is added);
changes are runtime-only and listed in the next `anim.motion_match` read. The `weights` overlay is
validated like schema weights and applied by rescaling the cached normalized features when the
`anim.motion_match_set` call lands (O(frames · dims), once per changed overlay, ADR-1831 (9) and ADR-5639; an
overlay of all ones is a no-op). A weight overlay changes costs but not the database's stored statistics. The overlay keeps the
exactness contract because the tree is built on the **unweighted-by-overlay** (schema-weighted) space and
overlay changes beyond ±50 % from the build weights force `BruteForce` for exactness; the report says
so (`method_used`).

Guide page: `book/agents/motion-matching.md` (extend, do not rename): add "Read the candidate list",
"Tune at runtime", "Verify no flicker" (jump counter does not grow while the path is constant), and a
"Tag filters" section. COVERAGE.md rows:

- `anim.motion_match` row (existing text kept) plus note: "candidates and filter parameters"
- `anim.motion_match_set` | `anim_motion_match_set` | motion matching
- `anim.motion_match_database` | `anim_motion_match_database` | motion matching

`cargo xtask agent-coverage` must pass (schema row, contract, guide topic, coverage row per method).
All three degrade gracefully when no `MotionMatching` component exists (`F0003`).

Verification loop for an agent: write a trajectory through `anim.motion_match_set`, `play.step`
N frames, `wait_until` `jumps` stops changing and `playback.clip_name` equals the expected clip,
read `candidates` to see why, and compare screenshots across the jump.

## 6. Editor surface

Desktop only (`regia_editor`). A **Motion database** asset inspector: schema form, clip list with
tag chips, span timeline per entry (add `unindexed`, `pass_through`, `landing_bias` ranges by dragging),
build-warnings list, size/frame/KD stats. A **Motion matching** debug panel for the selected
character: live candidate table with stacked per-group bars (bar segments styled by sign from the editor theme: cost below the best candidate's, above it, or ignored), a button to freeze the report, and the gizmo toggle. Everything is also reachable
through the agent methods in § 5, which are the primary surface.

**As built (part A, ADR-7740).** The Motion database inspector is the editor's **Motion database** window
(`regia_editor::mm_db_editor`), opened from the Motion Matching panel (**Edit database**), Content ▸ New ▸ Motion
database or `editor.mm_db_open`: the schema form (typed widgets clamped to the validator's ranges), the tag table
(add, remove, rename across the database), the clip list (add, remove, reorder; `looping`, `mirrored`,
`distance_matched`, trims; tag chips), the validator's errors (the runtime's checks), the editor's checks
(`untagged_clip`, `zero_weight`), and an in-memory rebuild job listing the runtime builder's warnings and the
stats (poses, per-clip coverage, frames per tag, feature ranges, tree size). Agent: `editor.mm_db_open`,
`editor.mm_db_state`, `editor.mm_db_edit`, `editor.mm_db_save`, `editor.mm_db_rebuild`
(`book/agents/motion-database-editor.md`). The optional cooked arc-length and clip-speed arrays of § 4 are
built since ADR-7817 (cooked version 3).

**As built (part B, ADR-7848).** Each entry has a span timeline (`mm_db_editor::timeline`): a ruler of the entry's
database frames, one row per span, and a bottom row; drag on the bottom row to add a span (`unindexed`,
`pass_through`, `landing_bias`, `tag`), drag a body to move it or an edge to resize it (moved edges snap to the
builder's frame times `k / rate`, unmoved edges keep their seconds), right-click to delete; keyboard alternatives
for every action (Left/Right one frame, Shift ten; Up/Down pick a span; E start/end/body; N add; Delete). Each
action is one op (`add_span`, `set_span`, `remove_span`, also `editor.mm_db_edit` ops; `range` in seconds or
`frames`) and one undo step. Overlaps follow § 2.5: a `pass_through`, `unindexed` or same-names `tag` span that
overlaps or touches one of its kind merges into their union (the baked frames are identical), `landing_bias` spans
stack. Scrubbing poses a preview skeleton (the animation editor's preview drawing) and shows the frame's raw feature
vector, which is the runtime builder's row: the entry is built alone with `MotionDatabaseSource::build`, and raw
features depend only on the entry's own clip (`editor.mm_db_scrub`). **Live** rebuilds through the rebuild job once
the document has been unchanged for 0.4 s, so warnings and stats follow edits; a warning or problem row selects the
entry and frame it names. Unsaved databases autosave as document kind `motion_db`. No format change.

## 7. Determinism notes

- Build: single-threaded in frame order; no hash-map iteration; statistics in `f64` accumulated in
  frame order; k-d splits break ties by (value, frame index). Same inputs give the same bytes.
  Feature extraction's yaw, facing pair and gait-phase trigonometry use the portable f64 series of
  ADR-4477, rounded once to `f32`, rather than the platform math library. The repo's cooked files
  are produced by the cook step only, so runtime platforms never recompute raw features.
- Runtime: normalization, distance and the jump rule use only IEEE `+ − × ÷ √` on `f32`; the canonical
  8-lane distance fixes the summation order, so all platforms and all methods return identical
  frames and costs (bit-exact). No fused multiply-add is allowed in `lanes.rs` (enforced by using
  separate operations and a unit test against the scalar reference on random data).
- The plugin's predictor, matcher and player advance by the fixed simulation step only; no
  presentation or wall clock. Stagger and the per-step search cap depend on entity index and stable
  ordering. One presentation frame can contain several budgeted steps. Fixed step (`play.step`)
  therefore reproduces the same jump sequence run to run; counters (`searches`, `jumps`) are the
  assertion currency. The legacy hand-registered `step_motion_matching` still uses its caller's `dt`.
- Prediction, root-motion yaw and stride response use the same portable series as cooking. The
  player's inertialized pose blending remains tolerance-based, asserted within 1e-4 m / 1e-3 rad.

## 8. Tests and acceptance

All tests run on the CPU in `cargo test -p regia_animation` (and `-p regia_agent` where stated); none
needs a GPU. No wall-clock assertions; no new golden images (a screenshot comparison across a jump is
a TODO for the lead, not created by the builder).

**Synthetic circular-walk database.** A test helper builds clips procedurally (no files): a root
joint travels at speed `v` on a circle of signed radius `R` (curvature `κ = 1/R`; `R = ±∞` straight)
with yaw rate `ω = v κ`; two foot joints oscillate along the heading with period `T` and opposite
phase. Entries: `straight` (looping), `left` (`R = 2`), `right` (`R = −2`), `walk_fast` (v = 2), `stop`
(non-looping, decelerating straight). Known analytic features (`t` seconds ahead, character frame,
x right, z forward): position `(R(1 − cos ωt), R sin ωt)` up to the sign convention fixed in the
helper's doc, direction `(sin ωt, cos ωt)`.

1. A query from a left-turn frame with the left-turn trajectory returns a frame in `left`; with the
   straight trajectory returns `straight`; with the right trajectory returns `right`; exact frame
   asserted (the nearest phase). Equal cost for all three methods.
2. Replace the weights by 0 on every group but `trajectory_position`: the answer depends only on
   the path and is still correct; set `trajectory_position` to 0 and `foot_position` to 1: the answer
   follows the foot phase and ignores the path.
3. Speed discrimination: a straight path at speed 2 picks `walk_fast`, at speed 1 picks `straight`;
   a mid speed 1.5 picks the clip whose normalized distance is smaller, matching the hand-computed
   cost within `1e-5`.
4. History channel: with `history_times` set, two clips identical now but with different pasts return
   different results for queries with different past positions; a schema without history gives the
   same database bytes as the baseline layout (regression guard).
5. Tag filters: `require = stop` returns only `stop` frames; `forbid = stop` never returns one;
   an impossible filter gives `no_candidates`; first-pick-with-empty-filter picks a frame and
   raises the `filter_ignored` warning.
6. Spans: `unindexed` removes frames from `search_top_k` entirely (the database's target count drops, ADR-5639);
   `pass_through` frames never appear as candidates but the clip plays through them in a
   run-through test (no jump lands there over 10 s of fixed steps); a `landing_bias(−b)` wins an
   otherwise exact tie and loses it for `+b`.
7. Exactness: on 20 random databases (sizes 1 to 20,000, dims 8 to 96, with duplicated rows, ties and
   biases) 1,000 random queries each: brute force, tree and `Auto` return the same frame and a
   bit-identical cost; top-k lists match; the SIMD lane distance equals the scalar reference bit for
   bit, including odd dimension counts that need padding.
8. Continuity: a query sitting between two near-identical clips does not flicker over 600 fixed steps
   (`jumps` ≤ 2); `stay_bias` negative suppresses a jump that happens at 0; `lockout_window`
   prevents returning to a just-left frame; `urgent_threshold` triggers a search before the interval
   when the path flips from left to right; stagger offsets differ between two entities and repeat
   identically across runs.
9. Jump rule at defaults equals the baseline on the existing 200-step trace (recorded in the test
   before the change; a regression test asserts equality of the full jump sequence).
10. Player: at a jump the output pose differs from the previous frame's pose by no more than the pose
    change the source clip would produce in one frame plus `1e-4`; after the blend duration the
    output equals the target clip's pose to `1e-4`; root-motion delta is finite and unaffected by
    the jump frame.
11. Warps: stride ratio clamp holds (ratio 3 is limited to 1.25; off below `min_speed`), orientation
    delta above `max_delta` clips, spine weights summing to 1 distribute a 40 degree warp so the head
    yaw equals the pelvis yaw plus 0 within `1e-4`, distance matching on a stop clip with arc length
    L places the time where remaining length equals `d` (checked at `d = L, L/2, 0`).
12. Build and format: two builds of the same inputs produce byte-identical cooked output; a cooked
    round trip yields the same normalized rows and search answers; a flipped byte gives
    `CorruptDatabase`; a source `format_version` of 2 builds, and a source `format_version` of 3
    gives `UnsupportedFormatVersion { supported: 2 }`; a cooked payload one version newer than
    `COOKED_MOTIONDB_VERSION` gives `UnsupportedFormatVersion { supported: 2 }`; a version-1 source
    is read and rewritten to version 2; schema validation
    errors each have a test; a build with no mirror table and a mirrored entry fails.
13. Agent (`cargo test -p regia_agent`): `anim.motion_match` old calls are unchanged (golden JSON
    of the existing test, compared as data); `candidates`, `anim.motion_match_set` (settings,
    filter, weights) and `anim.motion_match_database` round-trip; each reason code is hit; a
    weights overlay beyond ±50 % reports `method_used: "brute_force"`; missing component gives
    `F0003`.
14. Gates: `cargo xtask agent-coverage` and `check-layers` pass; `cargo ndk -t arm64-v8a check -p regia`
    compiles.
15. **GPU/device**: none required. An on-device smoke test (Android aarch64) runs the circular-walk
    suite and the exactness test under `cargo test` on the device runner, asserting identical answers
    to the host; the lead owns running it.

## 9. Platform notes

| Platform | Notes |
|---|---|
| Linux x86_64 | Safe lane code, vectorized by the compiler for the target baseline (SSE2); the canonical lane layout is the same everywhere |
| Windows x86_64 | Same as Linux; MSVC fused-multiply-add contraction is off by default for Rust, keep it that way |
| Android aarch64 | Compiler-vectorized NEON (two registers per 8-lane vector); same canonical distance; memory ceiling matters more (§ 10) |
| macOS (testing only) | Same safe module; no gating |

Decision (ADR-1585): no intrinsics and no new dependency. `unsafe_code = "deny"` rules out `core::arch` in
`regia_animation` (the crate is not in `docs/04` § 5), so `lanes.rs` is safe Rust over `[f32; 8]` accumulators in
exactly the order of § 2.7, written so LLVM auto-vectorizes it (SSE2 on x86_64, NEON on aarch64). Separate multiply
and add keep it free of fused operations. If measured numbers miss the § 10 budgets, the follow-up is the `wide`
crate (Zlib OR Apache-2.0 OR MIT; its internal `unsafe` is a dependency matter), which needs a `docs/03` row, an
ADR and `cargo deny check`; the canonical lane layout makes that swap bit-exact. postcard, RON, serde, thiserror and
tracing are already workspace dependencies. The cook step is desktop-only.

## 10. Performance budget

Min-spec machine class: the low-power laptop used for the min-spec lane (4 efficient cores, no wide-vector
assumption beyond SSE2) and the Android aarch64 reference. All numbers are budgets to be
measured and recorded in `docs/progress.md`; none is a claim of measured performance.

| Item | Budget |
|---|---|
| Single search, 10,000 frames × 48 padded floats, brute force | ≤ 0.15 ms |
| Single search, 100,000 frames × 48, `Auto` | ≤ 1.0 ms (tree or SIMD scan, whichever `Auto` picks) |
| Single search, 200,000 frames × 48, `Auto` | ≤ 2.0 ms |
| 20 characters, 10 Hz search each, 50,000-frame database | ≤ 0.5 ms per frame amortized (staggered) |
| Player + inertialization + warp, per character | ≤ 0.1 ms |
| Build (editor, 100,000 frames) | ≤ 60 s; cooked load ≤ 150 ms (recompute normalized rows + verify) |
| Memory | raw + normalized floats 2 × (padded dims × 4 B) per frame (≈ 0.4 KB at 48 dims); tree ≈ 12 B per frame; 100,000 frames ≈ 45 MB; ceiling 64 MB per database on min-spec, 128 MB total for all databases |

Tracing spans (target `regia_animation`): `motion_matching.search` (fields: method used, eligible
frames, visited nodes), `motion_matching.update`, `motion_matching.player`, `motion_matching.warp`,
`motion_matching.build` (build only), `motion_matching.load`. Counters exposed through the agent
read: searches, jumps, tree nodes visited (last search), frames scanned (last search).

## 11. Out of scope / follow-ups

- Learned motion matching (a network that replaces the database), PCA-projected or product-quantized
  approximate search, vantage-point trees; they need their own exactness statement.
- Built since (ADR-2858): phase channels and contact-labeled features (§ 2.2a), trajectory prediction with
  strafe/aim and a constant-rate turning intent (§ 2.6a), montage-scoped matching (§ 2.5a). Still open: a
  motion-matching graph node, a turning intent whose rate changes over the horizon, multi-database normalization sets
  (context pools can be done with tags).
- Slope/floor-normal foot placement beyond the existing foot IK.
- GPU search for very large databases.
- Gizmo golden images (TODO for the lead).

## 12. Provenance rows

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Feature set, search loop, transition cost, minimum jump interval | Pose + trajectory feature vectors per clip frame; periodic nearest-frame search; cost to switch | S. Clavet, "Motion Matching and The Road to Next-Gen Animation", GDC 2016 (talk); M. Büttner and S. Clavet, "Motion Matching - The Road to Next Gen Animation", Nucl.ai 2015 (talk) |
| Normalization with user weights, exclusion of neighboring frames, search interval | Baseline (non-learned) method and its database layout | D. Holden, O. Kanoun, M. Perepichka, T. Popa, "Learned Motion Matching", ACM Trans. Graph. 39(4), SIGGRAPH 2020 |
| Exact nearest-neighbor tree | k-d tree with median split, branch-and-bound nearest search | J. L. Bentley, "Multidimensional binary search trees used for associative searching", CACM 18(9), 1975; J. H. Friedman, J. L. Bentley, R. A. Finkel, "An algorithm for finding best matches in logarithmic expected time", ACM TOMS 3(3), 1977 |
| Smooth hand-off after a jump | Inertialization (offset decay with velocity carry) | D. Bollo, "Inertialization: High-Performance Animation Transitions", GDC 2018 (talk); D. Holden, spring-based inertialization articles |
| Stride/orientation/distance warping | Retiming and rescaling an authored motion to meet a constraint | A. Witkin and Z. Popovic, "Motion Warping", SIGGRAPH 1995; M. Gleicher, "Motion path editing", I3D 2001 |
| Cost-rounding-safe pruning | Conservative slack on the splitting-plane bound so floating-point rounding cannot change the winner | Original reasoning in this spec; general nearest-neighbor pruning as in the k-d tree papers above |

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Decisions (ADR-1585)

- Decision (ADR-1585): span kinds, bias names and the lockout window are Regia's own vocabulary (`pass_through`,
  `unindexed`, `landing_bias`, `stay_bias`, `loop_clip_bias`, `lockout_window`); behavior is unchanged from § 2.5 and § 2.8.
- Decision (ADR-1585): the `weights` overlay beyond +-50 percent forces brute force (reported as `method_used`); a tree
  rebuild per overlay is rejected because it stalls an interactive session by seconds.
- Decision (ADR-1585): the cooked file is bit-exact on the cooking machine and tolerance-based across machines (§ 7);
  runtimes never recompute raw features, so gameplay is unaffected.
- Decision (ADR-1585): the budgets in § 10 are targets recorded in `docs/progress.md`, not gates, until measured.
- Decision (ADR-1585): the cooked file stores raw features only (smaller file; load recomputes normalized rows).
- Decision (ADR-1585): stride warping may need a small pelvis-lowering hook in `regia_animation::foot_ik`; the builder
  adds it in-crate (no layering issue).
- Decision (ADR-1585): events use `Message` like the rest of `regia_animation` (`events.rs`).
- Formats to register: `.motiondb.ron` v2 (v1 loads through the migrator, ADR-2858);
  the cooked `.fckd` database tag is added beside the existing tags in `cooked.rs`.

### Owner decisions

None.

The contact labels of § 2.2a are shared: `label_contacts` also cooks the `contact.<foot>` curves of any clip with feet,
and `FootLocker` wraps the crate's one lock state machine `foot_ik::lock::PlantLock` (spec 17, ADR-8308).
