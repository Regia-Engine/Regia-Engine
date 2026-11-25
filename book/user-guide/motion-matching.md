# Motion matching

<!-- index: Motion matching -->
<!-- index: Motion database -->
<!-- index: Trajectory predictor -->
<!-- index: Foot lock -->

Motion matching plays locomotion without a hand-built state machine. It cuts every frame of a set
of clips into a feature vector: where the character is going, and how its feet are moving. Every
few frames it searches for the frame that best continues the current pose along the path the game
asks for, and jumps there. The method is the one from Simon Clavet's GDC 2016 talk "Motion Matching
and The Road to Next-Gen Animation". It is also the baseline (non-learned) system in Holden et al.,
"Learned Motion Matching" (SIGGRAPH 2020). The code lives in `regia_animation::motion_matching`.
The behavior spec is [spec 52](../../docs/specs/52-motion-matching.md). This chapter is the
runtime: build a database, run the matcher, predict a path, and plant the feet. Editing the
`.motiondb.ron` document is included below. The panel that lists live candidates during play
belongs to the editor guide. Agent calls for the database are on
[Motion matching (agents)](../agents/motion-matching.md). Learned compression of the database
is not built.

## Build a database

```rust,ignore
use regia_animation::motion_matching::{FeatureSchema, MotionClip, MotionDatabase};

let root = skeleton.joint_index("root").unwrap_or(0);
let feet = ["foot_l", "foot_r"].iter().filter_map(|n| skeleton.joint_index(n)).collect();
let schema = FeatureSchema::new(root, feet);
let database = MotionDatabase::build(
    &skeleton,
    &[
        MotionClip { clip: &walk, looping: true },
        MotionClip { clip: &run, looping: true },
        MotionClip { clip: &walk_to_stop, looping: false },
    ],
    schema,
)?;
```

The clips must carry their root motion in the root joint's tracks: the character has to move
through the clip's object space, as it does in a captured take. Each frame's features are, in the
character's own frame (the root projected onto the ground, turned to where the root faces):

| Group | Floats | What it is |
|---|---|---|
| `root_velocity` | 3 | The root joint's velocity. |
| `trajectory_position` | 2 per time | Where the character will be 1/3, 2/3 and 1 s ahead (`trajectory_times`). |
| `trajectory_direction` | 2 per time | Which way it will face at those times. |
| `foot_position` | 3 per foot | Each foot relative to the ground point. |
| `foot_velocity` | 3 per foot | Each foot's velocity. |

Schema settings:

- `forward` (default `+Z`): the root joint's local axis that points where the character faces.
- `sample_rate` (default 30): database frames per second.
- `trajectory_times`: the future sample times.
- `weights`: how much each group counts. A group's features are normalized together, by the
  group's mean and standard deviation, then multiplied by its weight. So 1.0 means "as important
  as the other groups", 2.0 makes the group's differences cost four times as much, and 0 turns
  the group off.

A looping clip gives `ceil(duration × rate)` frames, and its future samples continue into the next
lap. A clip that does not loop gives `floor(duration × rate) + 1` frames, and its future samples
stop at its last frame. Build errors (`MotionMatchError`) name the problem: no clips, a bad sample
rate, time, weight or forward axis, a joint outside the skeleton, a clip with a zero or non-finite
duration, or a non-finite key.

Build the database once, at load or cook time, and share it (`Arc`) between characters. It can
also come from a `.motiondb.ron` source (joints and clips by name, tags, spans, trims; see
`docs/formats/motiondb-ron.md`) through `MotionDatabaseSource::build`, and be stored cooked
(`MotionDatabase::to_cooked_payload`, loaded with `regia_animation::cooked::load_cooked_motion_database`).

`regia cook` cooks every `.motiondb.ron` under `assets/` (its skeleton and clips are glTF sub-assets,
`hero.glb#skeleton/Hips`, `hero.glb#clip/Walk`), and a clip with a NaN or infinite key fails the cook with
`clip_non_finite`. At runtime, add `MotionMatchPlugin` and give the character a
`MotionDatabaseRef::path("anim/hero.motiondb.ron")`: the plugin loads the cooked database (with its skeleton and
clips) into `MotionDatabases` and inserts the `MotionMatching` and `MotionMatchPlayer`. Add an `AnimatorMotionMatch` to
an entity with an `Animator` to show the matched pose through the animator (on in chosen graph states, or by a
parameter, with a fade). `samples/mm_locomotion` shows all of it on clips made in code; see
`book/agents/motion-matching.md` for the agent and editor surface.

Optional channels: `history_times` (where the root was, seconds ago), `pose_joints` (hands, hips,
head: position and/or velocity, each with a weight), `sample_weights` (make the near future count
more than the far future) and `normalization` (`GroupDeviation`, `PerDimension`, `None`). Without
them the feature layout is the five baseline groups.

## Edit a database in the editor

The editor's **Motion database** window edits a `.motiondb.ron` source. Open it from the **Motion Matching**
panel (**Edit database**), from Content ▸ New ▸ **Motion database**, or from the command palette
(`view.toggle_motion_database`). It has the schema form (feature groups and weights, sample times, pose joints,
contact labels, mirror pairs), the tag table, and the clip list with each clip's flags and tag chips.

Under each clip, the **span timeline** shows the clip's database frames with one row per span:

- drag on the bottom row to add an `unindexed`, `pass_through`, `landing_bias` (with its bias) or `tag` span;
- drag a span to move it, or drag its edges to resize it (edges snap to frames);
- right-click a span to delete it;
- or use the keyboard: click the strip, Up/Down to pick a span, E to pick its start, end or body, Left/Right to
  move it one frame (Shift: ten), N to add a span at the cursor, Delete to remove it.

`pass_through` spans, `unindexed` spans, and tag spans with the same names that overlap or touch one of their kind
are merged into one (they bake the same frames; spec 52 § 2.5); tag spans with different names stay separate, and
`landing_bias` spans are kept apart because their biases add. Every edit is one undo step.

Click or drag the ruler to scrub: the preview poses the skeleton at that frame and lists the frame's feature vector
by group, its tags and landing bias, and whether it can be a jump target. With **Live** on, the database is rebuilt
in the background after each edit, so the build warnings and the stats stay current; click a warning to jump to
its clip. Unsaved changes are autosaved with your other documents and offered for recovery after a crash.
`book/agents/motion-database-editor.md` lists the agent methods that do the same (`editor.mm_db_edit` span ops,
`editor.mm_db_scrub`).

## Run the matcher

```rust,ignore
use regia_animation::motion_matching::{MatcherSettings, MotionMatcher, TrajectoryPoint};

let mut matcher = MotionMatcher::new(MatcherSettings::default());
// Every frame: the path the controller wants, in the character frame (x right, y forward).
let path: Vec<TrajectoryPoint> = desired_path();
let step = matcher.update(&database, &path, dt)?;
if step.jumped {
    // Switch the animator to `database.clips()[step.playback.clip].name` at
    // `step.playback.time`, with an inertialized transition.
}
```

`MatcherSettings`:

| Field | Default | Effect |
|---|---|---|
| `search_interval` | 0.1 s | Time between searches. |
| `min_jump_interval` | 0.2 s | No jump within this time of the previous jump. |
| `transition_cost` | 0.1 | Added to every jump candidate's cost. Staying on the playing clip does not pay it. |
| `exclude_window` | 0.25 s | Frames this close to the playing frame, in the same clip, are not jump targets. |
| `method` | `Auto` | `Auto` (scan for small databases or rare tag filters, tree otherwise), `KdTree` or `BruteForce`. All pick the same frame with the same cost. |
| `stay_bias`, `loop_clip_bias` | 0 | Added to the continuation's cost, or to candidates of looping clips (±100). |
| `lockout_window` | 0 s | A frame chosen as a jump target is not a candidate again for this long. |
| `urgent_threshold` | off | Search at once when the desired path moved this far (normalized) since the last search. |
| `min_search_gap` | 0 s | Earliest urgent search after the previous one. |
| `stagger` | on | Spread characters' searches over the interval (slot = entity index mod 8). |
| `play_rate` | 1 | Multiplies playback time. |
| `live_pose_blend` | 0 | Blend the query's pose features toward the host's `LivePose` (1 = live only). |

A search also runs at once when nothing plays yet, and when a clip that does not loop reaches its
end. The query is the playing frame's pose features plus the desired path. The matcher jumps only
when the best other frame plus `transition_cost` costs less than continuing. Raise
`transition_cost` or `min_jump_interval` when playback flickers between two similar clips; lower
them when the character reacts late to a new path.

On an entity, use the `MotionMatching` component (database, matcher, desired `trajectory`, and
`ground` when the schema has history samples). Install `MotionMatchPlugin` (opt-in;
`AnimationPlugin` does not add it): it steps every matcher (stagger slots, the
`MotionMatchBudget` cap on searches per frame), sends a `JumpEvent` per jump, and advances a
`MotionMatchPlayer` on the same entity. `step_motion_matching` still works on its own and does
nothing once the plugin is installed.

`MotionMatchPlayer` plays the matched clip and inertializes every jump (0.1 to 0.25 s, longer for
costlier jumps). It removes the root's ground motion from its `pose()` and reports it as
`root_motion()` (a `RootMotionDelta` per frame) for the character controller. Its optional warps
(`warps`, fed by `warp_inputs`) adjust the matched pose: stride warping to the character's real
speed (playback rate or foot placement), orientation warping toward the desired heading (spine
counter-turn), and distance matching for stops and landings (`distance_matched` clips play to the
remaining distance instead of the clock).

To see why a frame won, `MotionMatcher::candidate_report(&database, 8)` lists the cheapest frames
with their cost per feature group and a status (`best`, `continuing`, `near_playing`, …). The agent
protocol reads all of this with `anim.motion_match`, tunes it with `anim.motion_match_set` and
summarizes the database with `anim.motion_match_database` (`book/agents/motion-matching.md`).

## Predict the path, plant the feet

Instead of writing `MotionMatching::trajectory` every frame, add a `TrajectoryPredictor` and set its desired
velocity; the `MotionMatchPlugin` predicts the path with critically damped springs (closed form, so a long frame
cannot destabilize it) and writes it before the matcher runs. For a strafe, set `facing = FacingMode::Aim` and
`aim_yaw`: the character keeps facing the aim while it moves. A framework character can drive it from its input
with `regia_framework::motion_matching_character::MotionMatchCharacter`.

<!-- ignore-reason: a fragment of a game system: `commands` and `hero` come from the game's own system -->
```rust,ignore
use regia_animation::motion_matching::{FacingMode, TrajectoryPredictor};

let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
predictor.desired_velocity = Vec3::new(1.2, 0.0, 0.0); // world space, m/s
predictor.facing = FacingMode::Aim;                       // strafe right while facing +Z
predictor.aim_yaw = 0.0;
commands.entity(hero).insert(predictor);
```

Give the schema `contacts: Some(ContactSettings::default())` (in `.motiondb.ron`: `contacts: Some(( ... ))`) to label
each foot planted or not at build time, and set `phase: true` for the gait phase. Both groups weigh 0 until you set
`weights.foot_contact` / `weights.foot_phase`, so the search is unchanged until you ask; a phase weight keeps
transitions on the right foot when the pose weights are low. `MotionMatchFootLocks` on the character gives one
`FootLock` per foot: feed `lock.ik_target(animated_foot)` to foot IK to keep a planted foot from sliding. A
`MotionMatchMontageScope` restricts the search to tagged frames while a montage plays a given section.

## Limits

- The matcher only picks frames; `MotionMatchPlayer` blends them. Without the player, pair jumps
  with an inertialized transition yourself (`book/user-guide/inertialization.md`).
- Mirrored entries take their joint pairs from a `MirrorTable` (or `mirror_pairs` in the source);
  the skeleton asset has no mirror table yet. Learned (neural) compression of the database is not
  built; the editor's **Motion Matching** panel shows the candidates and the database
  (`book/agents/motion-matching.md`). The `motion_matching` gizmo group draws the paths and feet.
- The ground is the XZ plane (Y up). The trajectory ignores height.
