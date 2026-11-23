# Motion matching: observe, probe, verify

Motion matching (`book/user-guide/motion-matching.md`) runs on entities with a
`regia_animation::motion_matching::MotionMatching` component. The host advances it with
`step_motion_matching`. `anim.motion_match` reads it and can probe a path; it never changes what plays. `anim.motion_match_set`
changes the matcher settings and the tag filter (see below).

## Observe

```json
{"method": "anim.motion_match", "params": {"entity": 4294967310}}
→ { "entity": 4294967310,
    "database": { "frames": 60, "dimensions": 27, "sample_rate": 30.0,
                  "trajectory_times": [0.333, 0.667, 1.0], "root_joint": 0, "feet": [1, 2],
                  "groups": { "root_velocity": { "start": 0, "len": 3, "weight": 1.0 }, ... },
                  "clips": [ { "name": "walk", "duration": 1.0, "looping": true, "start": 0, "frames": 30 },
                             { "name": "run", ... } ],
                  "kd": { "nodes": 15, "leaves": 8, "depth": 4 } },
    "settings": { "search_interval": 0.1, "min_jump_interval": 0.2, "transition_cost": 0.1,
                  "exclude_window": 0.25, "method": "kd_tree" },
    "trajectory": [ { "position": [0.0, 0.5], "direction": [0.0, 1.0] }, ... ],
    "playback": { "clip": 0, "clip_name": "walk", "time": 0.4, "frame": 12 },
    "searches": 4, "jumps": 1,
    "last_search": { "best": { "frame": 41, "clip": 1, "clip_name": "run", "time": 0.37, "cost": 17.2 },
                     "continuation_cost": 0.003, "jumped": false, "held": false },
    "last_error": null, "probe": null }
```

- `last_search.best` is the cheapest jump candidate. Its `cost` does not include the transition
  cost. `continuation_cost` is the cost of staying on the playing frame (`null` before anything
  played or after a clip that does not loop ended).
- `jumped` is true when the search moved playback.
- `held` is true when a candidate beat staying but `min_jump_interval` had not passed since the
  last jump.
- `jumps` counts every jump, including the first pick.
- `last_error` is the last update's error message (a trajectory of the wrong length or with a
  non-finite value), or `null`.

## Probe a path

Pass `speed` (meters per second, straight ahead) or `trajectory` (one
`{ position: [x, z], direction?: [x, z] }` per trajectory time, in the character frame with z
forward; `direction` defaults to `[0, 1]`). `probe.best` is the frame that path would pick from the
playing frame (or from a neutral pose before anything plays). `probe.continuation_cost` is the cost
of staying, so `best.cost + settings.transition_cost < continuation_cost` means the matcher would
jump at its next search:

```json
{"method": "anim.motion_match", "params": {"entity": 4294967310, "speed": 4.0}}
→ { ..., "probe": { "best": { "frame": 44, "clip": 1, "clip_name": "run", ... },
                    "continuation_cost": 23.9 } }
```

## Settings and tags

`anim.motion_match_set` changes the matcher without a rebuild. Settings: `search_interval`, `min_jump_interval`,
`transition_cost`, `exclude_window`, `method` and three biases: `stay_bias` is added to the continuation's cost
(negative holds the playing clip, positive releases it sooner), `loop_clip_bias` is added to candidates that belong
to looping clips, and `lockout_window` keeps a frame chosen as a jump target from being chosen again for that many
seconds. Biases are clamped to +-100. A database with tags (`MotionDatabase::annotate`) lists them under `tags` in
`anim.motion_match`; `require` and `forbid` take tag names and restrict every search to frames that have all the
required tags and none of the forbidden ones. A frame's landing bias (from spans) is part of the cost a candidate
reports in `last_search.best.cost`; pass-through and unindexed spans are never targets.

```json
{"method": "anim.motion_match_set", "params": {"entity": 4294967310, "stay_bias": -2.0, "require": ["walk"]}}
→ { "entity": 4294967310, "settings": { ..., "stay_bias": -2.0 }, "require": ["walk"], "forbid": [] }
```

If the filter leaves no frame while nothing plays yet, the first pick ignores the filter (`filter_ignored` is
logged); later searches with no eligible frame keep the continuation. Everything is validated before anything
changes. Error codes (in brackets in the message): `motion_bad_number`, `motion_bad_method`, `motion_bad_tags`,
`motion_tag_unknown`.

## Errors

- `F0003`: no such entity, or the entity has no `MotionMatching`.
- `F0002`: bad parameters. The message starts with a reason code:
  - `[bad_probe]`: both `speed` and `trajectory` were passed.
  - `[bad_speed]`: `speed` is not a finite number.
  - `[bad_trajectory]`: not an array, the wrong number of points, or a point without a finite
    `[x, z]` `position`.
  - `[bad_candidates]`: `candidates` is not an integer from 1 to 32.
  - `[unknown_tag]`: `filter` is not `{ require?: [tag], forbid?: [tag] }` or names a tag the database lacks.
  - `[bad_predict]` (`anim.motion_match_set`): `predict` is not `{ velocity?: [x, z], facing?: "velocity" | "aim",
    aim_yaw?, turn_rate? }` with finite numbers (velocity within +-100 m/s, turn rate within +-100 rad/s).
  - `[no_predictor]` (`anim.motion_match_set`): `predict` was passed but the entity has no `TrajectoryPredictor`.
  - `[bad_times]` (`anim.mm_predict`): `times` is not 1 to 16 numbers in (0, 10] seconds.

## Verify a locomotion change

1. Write the desired path into the component (game code or a test hook), then step (`play.step`).
2. Poll `anim.motion_match` until `playback.clip_name` is the clip you expect. Check that `jumps`
   grows once per real change, not every search: a `jumps` count that climbs while the path stays
   the same is flicker. Raise `transition_cost` or `min_jump_interval` to fix it.
3. Capture frames across the jump (`seeing.screenshot`) and compare them by eye.

The numeric proof is in `crates/regia_animation/src/motion_matching/tests.rs`:

- a single-clip database always returns that clip;
- a straight walking path picks the walk over the run;
- a transition cost stops flicker between two nearly equal walks;
- the minimum time between jumps holds;
- the k-d tree returns exactly the brute-force frame on random data;
- building and matching are deterministic.

## Read the candidate list

`anim.motion_match { entity, candidates: 8 }` adds `candidates`: the eight cheapest indexed frames of
the latest search plus the continuation, each with:

- `frame`, `clip`, `clip_name`, `time`, `mirrored` (true for a frame of a mirrored entry);
- `distance` (the canonical squared distance), `groups` (the same distance split per feature group;
  the parts add up to `distance`), `bias` (landing bias), `extra_bias` (loop-clip bias, or the stay
  bias for the continuation), `transition`, and `total` (what the jump rule compares);
- `status`: `best`, `continuing`, `candidate`, `pass_through`, `locked_out`, `near_playing` or
  `filtered`.

Pass `filter: { require: [tag], forbid: [tag] }` to judge the statuses (and steer a probe) with
another tag filter without changing the matcher. A group that dominates the `total` of the wrong
winner is the one to down-weight; a clip that never appears means the data lacks that motion,
which no weight fixes.

`last_search` also reports `reason` (`jumped`, `continued`, `held`, `no_candidates`), `urgent`,
`method_used` (`brute_force` or `kd_tree`; `auto` resolves to one of them), `nodes_visited` and
`frames_scanned`; the top level adds `deferrals` (searches the per-frame budget postponed) and
`warnings` (`{ kind: "filter_ignored" }` when the first pick had to ignore the filter, and the database's build warnings `{ clip, clip_name, kind: "missing_root_motion" }`).

## Tune at runtime

`anim.motion_match_set` takes, besides the settings above:

- `settings: { name: value }`: any matcher setting, including `urgent_threshold` (number, or null
  for off), `min_search_gap`, `stagger` (bool), `play_rate`, `live_pose_blend` and `method`
  (`brute_force`, `kd_tree`, `auto`);
- `trajectory`: the desired path (as in a probe);
- `filter: { require, forbid }`;
- `weights: { group: value }`: a runtime weight overlay (0 to 1000 per group; `null` removes it).
  The database itself is not modified (`anim.motion_match_database` still shows the built
  hashes). When a group moves beyond ±50 % of its build weight every search scans, and
  `method_used` says `brute_force`;
- `search: "now"`: search at the next update; `reset: true`: forget what plays (counters kept).

The result lists what was `applied`. Error codes: `bad_setting`, `bad_weight`, `unknown_tag`,
`bad_trajectory` (plus the older `motion_*` codes for the top-level parameters).

`MotionMatching` and `MotionMatchPlayer` are reflected components (with `MotionMatchPlugin`
installed), so the generic scene methods reach them too: `scene.get_entity { entity, components:
["MotionMatching"] }` shows `matcher.settings`, `matcher.rate_scale`, `trajectory`, `ground` and
`report_candidates`, and `scene.set_property { entity, component: "MotionMatching", path:
"matcher.settings.play_rate", value: 1.5 }` writes one field (`MotionMatchPlayer`: `duration_min`,
`duration_max`, `duration_cost_ref`, `strip_root`, `warps`, `warp_inputs`). The database, clips,
skeleton and runtime state are not reflected; `anim.motion_match_set` stays the checked way to
tune (it validates; a reflected write does not, and the matcher sanitizes settings at its next
update). A scene save skips both components (they cannot be rebuilt from a file; save the
`MotionDatabaseRef` instead).

## Tag filters

Tags come from the database's annotations (`TagTable`, spans). `filter.require` keeps frames that
have every listed tag, `filter.forbid` drops frames with any. A filter that leaves nothing keeps the
playing clip (`last_search.reason` is `no_candidates`); before anything plays it is ignored for the
first pick and `warnings` lists `filter_ignored`.

## Verify no flicker

1. Set a constant path with `anim.motion_match_set { trajectory }`.
2. `play.step` 600 frames, then read `anim.motion_match` twice, 100 steps apart.
3. `jumps` must not grow between the two reads. If it does, read `candidates`: two rows of nearly
   equal `total` from different clips are the cause; raise `transition_cost`, set a negative
   `stay_bias`, or add a `lockout_window`.

## Database summary

`anim.motion_match_database { entity }` returns `frames`, `dimensions`, `padded` (rounded up to the
eight-lane width), `groups`, `clips`, `tags` (frames per tag), `spans` (pass-through, unindexed and
biased frame counts), `targets`, `normalization`, `mean_scale_hash`, `content_hash` (64-bit FNV-1a,
hex), `kd` (with `leaf_size`), `median_jump_cost` (the player's default blend reference) and
`warnings`, plus `asset` (the database's registered asset path, or `null` for one built in code
and not registered). Each clip says whether it is `distance_matched`. `cooked_arrays` (ADR-7817) says which optional
cooked arrays the database carries: `clip_speed` (the per-frame clip speed; always true, since a database loaded from
an older cooked file computes it from its raw features) and `arc_length_clips` (the clips whose arc lengths are
stored). A database built from a `.motiondb.ron` or cooked at version 3 lists its distance-matched clips; one loaded
from an older cooked file lists none until a re-cook. Stride warping and distance matching give the same results either way: the player computes a
missing array from the clips, so the arrays only save that work on the first distance-matched playback. Error codes: `unknown_database` and `bad_params`; the asset-path form below adds
`corrupt_database`, `unsupported_version`, `missing_dependency` and `database_unreadable`.

## See it in the viewport

`debug.set_gizmos { group: "motion_matching", enabled: true }` draws, for every entity with a
`MotionMatching` and a transform: the desired path from the character's ground point with a facing
tick per sample (cyan), the playing frame's own path (gray) and its foot positions (orange). Compare
the two paths across a jump with `seeing.screenshot`; a gray path that never bends toward the cyan
one means the database lacks that turn.

## Cooked databases and asset paths

A `.motiondb.ron` under `assets/` is cooked by `regia cook` (ADR-2409): the cook resolves its skeleton and clips
(glTF sub-assets, `<file>#skeleton/<name>` and `<file>#clip/<name>`), refuses a clip with a NaN or infinite key
(`clip_non_finite`; the same scan fails the glTF's own cook), builds the features and the k-d tree once, and writes a
`.fckd` of type `regia_animation::MotionDatabase` (`docs/formats/motiondb-ron.md`). A cook error names its code:
`motiondb_parse`, `motiondb_version`, `motiondb_unknown_skeleton`, `motiondb_unknown_clip`,
`motiondb_clip_skeleton`, `clip_non_finite`, `motiondb_build` or `motiondb_write`.

At runtime the app keeps every database it knows in `MotionDatabases`, by asset path. A scene names one with a
`MotionDatabaseRef` (an asset id or a path, `player: true` to also play it); `MotionMatchPlugin` resolves it into a
`MotionMatching` (and a `MotionMatchPlayer` over the cooked clips) and updates it when the database reloads.

```json
{"method": "anim.motion_match_database", "params": {"database": "anim/hero.motiondb.ron"}}
→ { "entity": null, "asset": "anim/hero.motiondb.ron", "id": "…", "generation": 1, "playable": true,
    "frames": 312, "dimensions": 27, "clips": [ … ], "tags": { "walk": 180, … }, "content_hash": "…", … }
```

The path may carry `assets/` or not. A path the registry lacks is loaded from the cooked cache or the paks when the
app has an asset server; otherwise the answer is `[unknown_database]`. `anim.motion_match_databases` lists every
registered database (`asset`, `id`, `frames`, `clips`, `dimensions`, `generation`, `playable`, `content_hash`, and the
`entities` whose matcher uses it) and the paths that failed to load (`errors: [{ asset, code, message }]`).

## Matching through the Animator

An entity with an `Animator`, a `MotionMatching`, a `MotionMatchPlayer` and an `AnimatorMotionMatch` shows the player's
pose through its animator: the stage runs after the graph evaluates and blends the player's pose over the graph's with
its weight, so sockets, the joint writes and the skin follow motion matching. The weight works like a graph state:
`states: ["Locomotion"]` turns it on while the root state machine is in one of those states, `weight_param` names a
float or bool graph parameter that multiplies it, and `fade` (seconds, default 0.2) ramps it linearly at fixed steps.
At weight 1 the animator's pose equals the player's bit for bit. The player's root motion, scaled by the weight, joins
the animator's through its `root_motion_route`.

Verify a hand-off: read `editor.mm_panel_get` (or `anim.motion_match`) while toggling the parameter with
`anim.set_param`; the stage's weight is visible in the scene inspector as `AnimatorMotionMatch.current`.

## The Motion Matching panel

The editor's **Motion Matching** panel shows, for the selected character, what plays, the counters and the candidate
table of the latest search, each row with its cost split per feature group as a stacked bar: a segment is `below`
(the group costs no more than for the best candidate), `above` (it costs more) or `ignored` (the group's weight is 0),
drawn with the theme's ok, alert and muted colors. A database section lists the asset path, the clips with their frame
ranges and tags, the tree and the warnings. `Freeze` keeps the report while the game plays on.

- `editor.mm_panel_get` → `{ characters: [{ entity, asset, frames }], selected, candidates, frozen, report: { playing,
  searches, jumps, method_used, rows: [{ frame, clip_name, time, total, transition, bias, extra_bias, status, groups:
  [{ group, cost, class }] }], continuation }, database: { asset, frames, clips, kd, warnings }, error }`.
- `editor.mm_panel_select { entity?, candidates?, sort?, descending? }` picks the character (`mm_panel_not_found`
  without `MotionMatching`, `mm_panel_bad_entity`), the row count, 1 to 32 (`mm_panel_bad_candidates`), and the
  database tab's clip and tag order: `index`, `name`, `frames`, `duration` or `tags` (`mm_panel_bad_sort`).
- The database tab (ADR-2858) also lists `tag_frames: [{ name, frames }]` in the same order, a `Cost parts` table
  with every candidate's per-group cost, and, with contact labels, `strip: { clip_name, feet, current, contacts,
  phases }`: the playing clip's contact label and phase per frame and foot, the current frame marked.
- `editor.mm_panel_freeze { frozen }` freezes or unfreezes the report (`mm_panel_bad_frozen`).

## Contact labels, phase and foot locks

A database whose schema has `contacts` (`.motiondb.ron` v2, ADR-2858) labels every foot of every frame planted or
not at build time and, with `phase: true`, stores each foot's gait phase. Both are feature groups (`foot_contact`,
`foot_phase`) that weigh 0 unless the schema or a runtime overlay sets them, so turning labels on never changes a
search by itself.

- `anim.motion_match { entity }` adds `database.contacts` (the thresholds) and, for the playing frame,
  `playback.contacts: [bool per foot]` and `playback.phases: [phase in [0, 1) or null per foot]`. The candidate rows'
  `groups` include `foot_contact` and `foot_phase`.
- `anim.motion_match_set { entity, weights: { foot_contact: 1, foot_phase: 1 } }` tries them at runtime.
- With a `MotionMatchFootLocks` component the read also has `foot_locks: [{ planted, target, weight }]`: the world
  point a planted foot is pinned to and how much foot IK should follow it (1 while planted, falling to 0 over the
  release time after lift-off).

Verify: walk a straight path, read `playback.contacts` every frame, and check that while a foot reads `true` its
`foot_locks[i].target` does not move.

## Trajectory prediction and strafing

A `TrajectoryPredictor` next to `MotionMatching` writes the desired path every frame from a desired velocity, using
closed-form critically damped springs for the velocity and the facing (`desired_turn_rate` says the desire is
turning, so a steady turn predicts a curved path; still closed form); game code (or the `regia_framework`
`MotionMatchCharacter` bridge from `CharacterInput`) sets only the velocity. `facing: aim` with `aim_yaw` strafes:
the character keeps facing the aim while it moves.

- `anim.mm_predict { entity, times? }` → `{ desired_velocity, facing, aim_yaw, desired_turn_rate, desired_yaw,
  settings, state: {
  position, velocity, yaw, yaw_rate }, trajectory: [{ time, position, direction }], history: [{ ago, position, yaw }],
  drives_matcher }`. `F0003` without a predictor.
- `anim.motion_match_set { entity, predict: { velocity: [1.2, 0], facing: "aim", aim_yaw: 0 } }` steers it.
- `anim.motion_match { entity }` reports `predictor: true` when a predictor writes the path.

Verify a strafe: set `predict` as above, `play.step` 30 frames, and read `anim.motion_match`: `playback.clip_name`
is a strafe clip (tag `strafe` in `tags`), and `anim.mm_predict` shows every `trajectory[i].direction` near `[0, 1]`.

## Montage scopes

A `MotionMatchMontageScope { rules: [{ montage, section, filter }] }` next to a `MontagePlayer` restricts searches to
the rule's tag filter while that montage plays that section (first matching rule; blending out does not count). The
scope adds to the matcher's own filter and is strict: if nothing is eligible the matcher keeps playing and reports
`no_candidates`.

- `anim.motion_match { entity }` shows `scope: { require, forbid }` while one is active and `montage_scope: { rules,
  active }` with the component.
- `anim.motion_match_set { entity, scope: { require: ["stagger"] } }` sets a scope by hand; `scope: null` clears it.

## The mm_locomotion sample

`samples/mm_locomotion` walks one character over a scripted course (stand, walk, turn right, run, turn left, stop) on
clips made in code: no files. The database is built from the same `.motiondb.ron` source a cook reads and registered at
`procedural/locomotion.motiondb.ron`; the hero names it with a `MotionDatabaseRef` and plays it through its `Animator`.
`mm_locomotion --agent` keeps it running with the agent server. A verification loop:

1. `anim.motion_match_databases` → the hero's entity under `entities`.
2. `anim.motion_match_database { database: "procedural/locomotion.motiondb.ron" }` → six clips, `playable: true`.
3. Read `anim.motion_match { entity }` until `playback.clip_name` is `run` (the course's run leg).
4. `anim.motion_match_set { entity, filter: { forbid: ["run"] }, search: "now" }` and read again: the matcher left
   `run`. `samples/mm_locomotion/tests/agent_dogfood.rs` runs exactly this over the real protocol.
5. `anim.mm_predict { entity }`: the course sets only a desired velocity; the hero's predictor makes the path
   (`drives_matcher: true`).
