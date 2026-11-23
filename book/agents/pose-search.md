# Pose search (offline motion matching): scope and contract

`regia_animation::pose_search` is an **offline, library-only** motion-matching building block:
a `FeatureDatabase` of per-frame feature rows built from `AnimationClip`s, exact nearest-neighbour
search (`SearchMethod::BruteForce` and `SearchMethod::KdTree` agree, ties keep the lowest frame
index), and a `SearchState` cursor that hands a `(clip, time)` `PoseSelection` to the existing
`Inertializer` for inertialized switches.

There is deliberately **no agent method** for it yet (ADR-4869): nothing in the engine runtime
consumes it, and the agent-observable motion matching feature is
[`motion_matching`](motion-matching.md) (`anim.motion_match`, `anim.motion_match_set`), which is
a separate runtime module. This page records the placed scope so the deferred agent surface is
explicit: when a runtime consumer lands, it gains methods and this page lists them.

## Host contract

A host (game, tool or test) drives the API directly:

1. `FeatureDatabase::build(&skeleton, &[ClipSource { clip, looping }], &config)` bakes one row
   per sampled frame. Non-looping clips drop frames that cannot cover the longest trajectory
   horizon; looping clips keep every sample and extend futures past the end by whole laps,
   composing each lap with the cycle transform (the lap displacement turns with the cycle yaw).
2. Build a query with `FeatureDatabase::query_from_pose(&skeleton, &pose, prev, &future)`
   (live pose plus desired trajectory) or `FeatureDatabase::feature_at(...)` (a database frame's
   own row, for tests).
3. `SearchState::search(&db, &query, method)` picks a `PoseSelection`. The first search, and any
   cross-clip win, sets `blend = true`; a cheaper frame of the current clip must beat continuing
   by more than the replay threshold. Once a non-looping clip has played to its clamped end there
   is nothing to continue: the next search adopts its best frame with `blend = true`, even in the
   same clip and whatever the threshold (ADR-7241). Only a match at the very time already showing
   (a frame stored at the clip end) keeps playing without a blend.
4. `SearchState::advance(&db, dt)` moves the playback clock without searching (looping clips
   wrap), clearing `blend` and refreshing `frame` to the stored frame nearest the new time.
   `PoseSelection::cost` is a last-query value and is not recomputed by `advance`.
5. `PoseSelection::begin_blend(&InertialBlend { ... }, &mut inertializer, &mut target)` samples
   the selected clip into `target` and, when `blend` is set, starts the inertializer.

## Units

`TrajectorySample::offset` is in metres, in the frame of the root's global position and rotation.
The root's scale is not part of that frame, so a centimetre rig (root scale 0.01) takes `1.0` for
one metre; the database's trajectory channels are baked in the same frame (ADR-7241). The
joint-position and joint-velocity channels are measured through the root's full transform, scale
included, but `query_from_pose` computes them from the live pose the same way the build does, so a
host never converts them.

## Root motion

`begin_blend` pins the sampled root in place exactly like the runtime evaluator: the effective
mode is `InertialBlend::root_motion`, or the clip's own `RootMotionTrack` mode when the override
is `None`. The visible root therefore never drifts with the clip's authored root motion, and
across a loop wrap there is no reset. The caller extracts progression with
`AnimationClip::root_motion_delta_step` (or `root_motion_delta`) and feeds it to the character
controller, as with graph-evaluated clips.

## Errors

Every fallible entry point returns `PoseSearchError` (`thiserror`): configuration and sampling
errors at build time (`BadSampleInterval`, `JointOutOfRange`, `NonFiniteFeature`, ...), query
shape errors (`FeatureLengthMismatch`, `TrajectoryMismatch`, `PoseMismatch`, `NonFiniteQuery`),
and playback errors (`BadTime`, `BadDelta`, `ClipOutOfRange`, `BadBlendDuration`). Times past a
clip's duration are rejected (`BadTime` for `feature_at`, `BadFrameTime` for raw rows), and a
non-looping `feature_at` also rejects times whose horizon would pass the clip end — the frames a
build would not store are not queryable.

## Verify

The module's behaviour is covered by unit tests in `crates/regia_animation/src/pose_search.rs`
(search equivalence and tie rules, replay threshold, horizon dropping, lap rotation, root-motion
pinning through a wrap, time validation, malformed-pose rejection, metre trajectories on a
centimetre-scaled root, and the jump away from a non-looping clip's clamped end). Until an agent method
exists, there is nothing to call from the agent protocol; verify changes with
`cargo test -p regia_animation --lib pose_search`.
