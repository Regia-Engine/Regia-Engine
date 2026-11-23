# Spec 17 — Animation (`regia_animation`)

## 1. Data
- `Skeleton` asset: joint names, parent indices (topologically sorted), bind pose (local TRS), inverse
  bind matrices, optional retarget metadata (Tier 2).
- `AnimationClip` asset: duration, sample rate, per-joint tracks (translation/rotation/scale; constant
  tracks collapsed), morph weight tracks, root motion track (extracted at import: none / XZ / XYZ / yaw
  from the root joint; `Yaw` is the twist about +Y alone and the translation modes report no rotation, ADR-2604,
  ADR-5614), **notifies** (`time, name, payload: Option<String>` holding RON text), **curves** (named float
  curves usable as graph parameters, e.g. `"FootPlant_L"`), plus generic **property tracks** targeting
  reflected fields by path (used by non-skeletal animation and cinematics).
- Compression (`compress::compress_clip`): drop keys within error tolerance (per-track adaptive, position error in metres,
  rotation in radians, measured at the joint *and* propagated to a virtual end-effector distance),
  quantise rotations (smallest-three: a 2-bit index and three signed 16-bit components, 50 bits stored in
  8 bytes per key), translations/scales (16-bit relative to track range). The cook does not run it yet:
  `regia cook` writes the raw f32 clip (`cooked-clip` v2, ADR-297), and `anim.clip_report` reports what
  compression would give (ADR-5614; compressing at cook is open work). The separate object-space packer
  `clip_compress` (48-bit or 32-bit smallest-three per track, skeletal TRS tracks only) is not cooked either.
  Sampling uses linear for translations/scales and nlerp for rotations (normalised; slerp for low key
  density option on raw clips; a compressed clip always samples nlerp, ADR-5614). Target: compressed ≤ 25%
  of raw f32 size with no visible artefacts on test clips.
  The quantisation error is inside the tolerance (ADR-1387): every key is quantised first, and key
  reduction measures the chord between the reconstructed keys against the full tolerance, including
  the chord's own endpoints. It does not subtract the track's half 16-bit step, or the smallest-three
  bound, from that reduction budget. A least-squares fit of the inner knot values (ends pinned) may
  replace those interpolating keys when the fitted set is smaller (ADR-1389). Collapsing a rotation
  track to one constant key still reserves the smallest-three bound: `quat_is_constant` uses
  `rotation_tolerance - QUAT_QUANTIZATION_ERROR` (1.1e-4 rad), and the same bound times the chain
  length on the propagated budget. Rotation error is measured as `2·atan2(|v|, |w|)` (an `f32`
  `acos` cannot resolve angles below ~5e-4 rad).
  `compress::measure_compression_error` reports the error a cooked clip actually has (local and
  object space); a track too long for 16-bit keys at its tolerance keeps every key and shows up there.
  As built (ADR-7227): the cook writes `cooked-clip` v3 with the joint tracks compressed this way
  (`AnimationClip::to_compressed_cooked_payload`, default tolerances, the clip's skeleton for chain
  lengths); morph weights, notifies, curves, property tracks and root motion stay `f32`; a slerp
  track keeps every key (quantized only) and its flag. Loading decompresses into an `AnimationClip`.
  v1/v2 payloads load and migrate losslessly (`Raw` v3 body); the migrator never compresses.
  As built (ADR-7819): a source whose `.meta` settings say `lossless_clips: true` (`.meta` v2; glTF, FBX, USD
  and BVH) cooks every clip as a `Raw` v3 body, bit-exact to the imported clip; a v1 `.meta` migrates with
  the switch off. Set per source with `editor.set_import_settings` or by an import rule.

## 2. Runtime components
- `SkinnedMesh { skeleton: Arc<Skeleton>, joints: Vec<Entity> }` (Tincal C1, ADR-242) — joint entities
  created from the glTF node hierarchy (so sockets/attachments are ordinary children). It lives in
  `regia_animation` next to `Skeleton`; `SkeletonBinding` is its deprecated alias.
  `regia_framework::skinning::SkinnedMeshPlugin` fills the render-owned `SkinJoints` from the
  animator's `SkinPalette`, or without an animator from the joints' world transforms × inverse bind
  matrices, so `regia_render` skins without depending on `regia_animation`
  (`02-architecture.md` § 1).
- `Animator { source: AnimatorSource — Graph { graph: Arc<AnimGraph>, clips: Arc<ClipLibrary> } (a graph plus the
  clips it references) or SingleClip { clip: Arc<AnimationClip>, looping }, parameters: ParameterSet, speed, update_mode:
  Always | WhenVisible | Lod(distance thresholds → update rate) }` (`crates/regia_animation/src/components.rs`).
- Evaluation in `Update` (serial over animators today; the budget below is a target, not measured parallel): sample → blend → post-process (IK) → write local
  `Transform` of joint entities in `PostUpdate/AnimationApply` (before transform propagation). Poses are
  computed in a compact SoA local-space buffer; writing back to ECS happens once per joint.
- Performance budget: 100 characters × 60 joints × 3-layer graph ≤ 2 ms desktop (all cores), 30
  characters ≤ 3 ms on mobile reference.

## 3. Anim graph (`.animgraph.ron`)
Node types (Tier 1):
- `Clip { clip, looping, speed, sync_group }`
- `BlendSpace1D { space: BlendSpace1D (param, samples: [(value, clip)]), sync_group }`, `BlendSpace2D { space: BlendSpace2D (param_x, param_y, samples: [(Vec2, clip)], triangles), sync_group }`
  (the node holds the space definition; triangulated sample space, barycentric weights; sync groups keep foot phases aligned via
  normalised time). A synced space advances one normalised clock at the weighted average of its
  active samples' durations; every active sample steps from that shared phase and only the dominant
  sample fires notifies (ADR-871). Non-finite inputs read as 0.
- `StateMachine { states, transitions }`; state = sub-graph; transition = `{ from (or Any), to,
  conditions: [param op value | clip_finished | notify], duration, curve, interruptible,
  blend: Crossfade | Inertialize | InertializeSpring }` (format v2, ADR-203; default `Crossfade`).
  `Inertialize` evaluates only the target state and decays the per-joint pose offset and velocity
  at the switch to zero over `duration` (Bollo's quintic, overshoot-safe; `InertializeSpring` = a
  critically damped spring); an interruption re-inertializes from the current output pose.
  Entering a state restarts its sub-graph; an any-state transition never re-enters the current
  state, an explicit self-transition restarts it (ADR-871). A cross-fade weights the target by
  `alpha` and the source by `1 − alpha` for poses, root motion, morphs and curves alike.
  Morph weights compose per node like the pose: blends lerp them (a target one side lacks counts as
  0), override layers move the targets they have by their weight, additive nodes and layers add
  theirs. Clips whose effective weight is under one half fire no notifies (a 50/50 blend fires both; a
  synced blend-space sample uses `weight × sample weight`, so a dominant sample under half is silent too);
  `Notify` conditions still see them.
- `Blend { a, b, alpha_param }`, `Additive { base, additive, weight_param }`
- `Layer { base, layers: [(graph, mask: JointMask, mode: Override|Additive, weight_param,
  weight_inertialize)] }` (`weight_inertialize` seconds eases weight changes, v2; 0 = off)
- `TwoBoneIK { inner, chain (3 joints), target: param|socket, pole, weight_param }`, `LookAt { inner, joint, target, max_yaw, max_pitch, weight_param }`,
  foot placement is not a node: `foot_ik` (`plan_foot_ik` / `solve_foot_ik`: raycast ground, adjust pelvis)
  feeds `TwoBoneIK` for the legs; the stateful `foot_ik::FootPlanter` (ADR-8088) adds foot tilt toward the
  ground normal, a world-space plant lock (a planted foot never slides over 1 cm while the clip skates) and
  dead-band, ease and slew smoothing of ground, pelvis and tilt; `apply_foot_plant` poses a skeleton with it,
  `FootPlant::ik_plan` feeds the same `TwoBoneIK` nodes, and the reflected `FootPlantReport` is the agent anchor.
  `FootPlacement` (ADR-8388) is that planter as a component: a `GroundProbe` resource supplies the hit, pose output runs after the ragdoll blend and before eye gaze (§ 4a), and graph output (`FootPlacementOutput::GraphParams`) is written in `Update` before `evaluate_animators` because `TwoBoneIK` reads its parameters during evaluation (a missing weight float is 0). Graph targets drop the bind tip height so the tip joint meets the contact; pose output keeps it so the sole does. The engine inserts `FootPlantReport` and `FootPlacementStatus`.
- `RootMotion { mode }` output for the character controller.
- `ChainIK { inner, root, tip, solver: Fabrik | Ccd, target: param|socket, pole, limits, iterations, tolerance,
  weight_param }` (format v4, ADR-8475): iterative IK over the chain from `root` down to `tip`, any length (tails,
  tentacles, spines, insect legs, cables), with `chain_ik::solve_chain`. `limits` is `Uniform(JointLimit)` or
  `PerJoint([(joint, limit)])` (cone or hinge, relative to the bind rotation); `pole` turns the solved chain about its
  root-to-tip line so its bend faces the pole; `iterations` is capped at 256 by the loader.
  `BodyAim { inner, joints: [(name, share)], aim_joint, axis, target, limits, passes, weight_param }` (format v4):
  turns the joints bottom first, each by its share of what is left, so `aim_joint`'s `axis` (default −Z) points at the
  target (spines, necks, turret yaw/pitch stacks), with `aim_ik::solve_body_aim_with`; each of `passes` (default 8,
  1..=32) removes the parallax of an aim joint away from the turning joints. Both resolve joint names once per rig
  and allocate nothing per frame once warm; an unknown joint is `EvalError::MissingJoint` like `TwoBoneIK`; a root
  and tip that do not form a chain warn `animgraph_chain_invalid` and pass the pose; a per-joint limit on a joint the
  node does not turn warns `animgraph_limit_joint_not_in_chain`. More IK nodes (also `TwoBoneIK`, `LookAt`) than the rig's
  64-slot caches keep one slot each (the cache grows; slots unused for more than 4 evaluations, left by a reloaded graph, are replaced first), so a
  warm rig never re-resolves (ADR-8735). A `ChainIK` slot hits only while its cached root and tip ids still carry their names and every
  parent link between them is unchanged (a cached failure, while the names still do not form a chain), so an in-place parent or name edit
  of the skeleton resolves the chain again without a per-frame lookup (ADR-8803). `ik_retarget::retarget_graph` carries `axis` and a hinge limit's axis
  through the paired joints' rest-frame difference (`Retargeter::retarget_joint_direction`), so the aim names the
  same anatomical direction on a rig with another bone roll; cone limits are roll-invariant and pass through.
  The v4 IK records (`ChainIK`, `BodyAim`, a `PerJoint` entry) refuse unknown fields, so a misspelled field is a load
  error rather than a silent default; a `BodyAim` turning joint with an empty name is refused at load; and the
  format fingerprint lists the payload shapes of `JointLimit`, `ChainLimits` and `IkTarget`, so a field change in a
  limit moves the v4 hash (ADR-8732; v4 was unreleased when this was decided, so it stays v4).
- `StrideWarp { inner, ground_speed_param, settings: StrideWarpSettings { strength, min_speed, ratio_min, ratio_max,
  response, mode: Rate | Pose }, legs: [[hip, knee, ankle]], pelvis, stride_length }` (format v4, ADR-8540): the
  ratio of the float `ground_speed_param` (m/s) to the inner graph's clip speed (its horizontal root motion this
  frame over the inner clock step; blend spaces blend root motion like poses), clamped and smoothed by the
  motion-matching warp code (`motion_matching::warp`, spec 52 § 2.10). `Rate` plays the whole inner graph on a clock scaled
  by the ratio (sync groups inside share that clock, so their phases hold; the ratio is the one smoothed up to the
  previous frame); `Pose` keeps the clock and scales each foot's offset from the pelvis along the movement direction
  by the ratio with two-bone leg IK, lowering (never raising) the pelvis when a leg cannot reach. An omitted
  `settings` is full strength, `Pose`. `OrientationWarp { inner, heading_param, pelvis, spine: [(name, share)],
  strength, max_delta }` (v4): turns the pelvis by `wrap(heading − clip_heading)` clipped to `±max_delta` (default
  π/2) times `strength`, and the spine back by its shares (summing to 1) so the chest keeps facing; headings are
  radians about +Y, 0 = forward (−Z), counter-clockwise positive (`warp_nodes::movement_heading`). Both read the root
  motion of the evaluation they wrap and leave it unchanged; an inner graph with no root motion passes the pose
  (`animgraph_warp_no_root_motion`, once). The smoothed ratio and last turn live in the graph instance
  (`GraphInstance::Warp`), are shown in the debug snapshot (`NodeDebugState::StrideWarp` / `OrientationWarp`, and
  `anim.get_state`'s `warps`), and are not part of a pose snapshot (a decoded instance starts at ratio 1). Distance
  matching stays motion-matching-only.

Parameters: `float`, `int`, `bool`, `trigger` (auto-reset after consumed), `vec2/vec3`. Set from Rust:
`animator.set("Speed", 3.2)`, `animator.trigger("Jump")`. Typed parameter structs via derive (optional).

Events: notifies fire as `AnimNotify { entity, name, payload }` ECS messages. State enter/exit
fires as `AnimStateEvent`. Neither is a `ScriptEvent`. `ScriptEvent` is collision and trigger
volumes only (spec 14 § B2); footsteps read `AnimNotify` directly.

## 4. Other animation features
- **Morph targets** (blend shapes; Tincal C2, ADR-243): `MorphWeights` (defined in `regia_scene`, one
  weight per target) come from the animator's morph tracks, composed with `MorphLayers` (override and
  additive expression layers, layer weights from float parameters, a clamp, normalized viseme sets);
  targets by index or by name (`MorphTargetNames`). The renderer applies them (`specs/13` § 8).
- **Attachments**: entity parented to a joint entity (weapon in hand). `Socket { rig, name, joint, offset }`
  helper component resolves by joint name.
- **Root motion**: graph outputs delta transform per frame; `Character` consumes it in root-motion mode.
  Every node combines its children's root motion with the weights it combines their poses with (a
  clip without root motion contributes zero; a layer only as far as its mask covers the skeleton
  root). A frame that laps a looping clip several times adds every lap; reverse playback (negative
  speed) moves it backwards (`AnimationClip::step`, `ClipStep`; ADR-871).
- **Property animation**: `PropertyAnimator { clip }` animates reflected fields of any component
  (doors, lights, UI). Same clip asset type, property tracks only. It is simulation, not
  presentation: the system advances and applies once per fixed step in `FixedUpdate`
  (`regia_app::FixedUpdateSet::Animation`), before the physics step syncs kinematic transforms,
  so every peer's fixed steps see the same values whatever the frame pacing (ADR-4653).
- **Retargeting** (Tier 2): humanoid mapping on skeleton assets.

## 4a. Eye gaze and per-instance bind poses (batch v020, Tincal C6/C9)
- **Eye gaze** (`regia_animation::gaze`): a `Gaze { rig: GazeRig, target, weight, morphs }` component on a
  rig turns the two eye joints at a `GazeTarget` (world point or entity) inside `apply_animation`, on
  the working pose after the ragdoll blend and foot placement and before the rig drivers, the spring bones, the joint
  writes and the palette (order: animator pose → ragdoll → foot placement → gait rig → eye gaze → twist/helpers/drivers → spring
  bones → instance retarget → joint write; ADR-244 gaze order, ADR-8388 foot placement, ADR-8707 gait rig), so drivers and head-rooted springs see
  the gazed head and eyes. A rig with an `InstanceBindPose` is gazed in its own proportions (solved on
  the instance rest pose, rotations mapped back to the shared pose). Per-eye limits
  (`EyeLimits`: in/out/up/down, default 35/40/26/31°), a saccade speed limit (9 rad/s), convergence
  (0 parallel … 1 each eye aims; targets nearer than `min_distance` are pushed out) and an optional
  `HeadFollow` (an `AimChain` solved first with `solve_body_aim`). Eyelid follow: `LidSpec` joints
  rotate by `follow × eye pitch` on top of the clip's rotation (blinks survive), and the four
  directional channels per eye (ARKit `eyeLookUp/Down/In/Out Left/Right`) go out through the
  `GazeMorphSink` trait; through a `LidMorphMap` the apply system writes them into the rig's composed
  morph weights before the rig drivers, which may read and override them. Those weights then become
  `MorphWeights`. `IndexedMorphs` is the sink for a by-index `(target, weight)` list. Pure solver:
  `solve_gaze`.
- **Per-instance bind poses** (`regia_animation::bind_pose`): an `InstanceBindPose` component (rest pose,
  inverse binds, bone length ratios) gives one avatar its own proportions over the shared `Skeleton` and
  clip set. The animator, graph, IK and inertialization stay in the shared skeleton's space;
  `apply_animation` retargets the final pose (`retarget_to_instance`: rotation offset from rest kept,
  translation offset scaled by the bone length ratio, scale ratio kept) and builds joint transforms, joint
  globals and the `SkinPalette` (`global × instance inverse bind`) from the instance. Root motion is not
  scaled (gameplay speed is the game's). The renderer already takes a per-rig palette: no render change.

## 5. Editor (M12)
- Anim graph editor: egui-snarl node canvas for blends/layers, state machine canvas (states as nodes,
  transitions as edges with condition editor), live preview viewport with parameter sliders, live
  debugging during PIE (highlight active state, show weights).
- Clip viewer: timeline scrubber, notify track editing (add/move/remove notifies), root-motion preview.
- Skeleton viewer: joint tree, sockets editing, two-bone IK chain definitions.
- Curve editor: named curves and property tracks (constant / linear / cubic keys).
- Authored clip and skeleton data (notifies, curves, property tracks, root-motion mode, sockets,
  IK chains) is saved in the rig's `.animedit.ron` sidecar (`docs/formats/animedit-ron.md`);
  graph canvas positions in an editor-only `.animgraph.layout.ron`. Agent surface:
  `editor.anim_*` (`book/agents/animation-editor.md`). See ADR "Animation authoring sidecar".

## 6. Acceptance (M12)
- Unit tests: sampling exactness at keys, compression error bounds, blend space weights sum to 1,
  transition timing, IK reaches reachable targets within 1 mm.
- `samples/anim_lab`: idle/walk/run blend space, state machine with start/loop/land transitions,
  upper-body layer (wave), two-bone IK and look-at reaching moving targets, footstep notifies driving
  audio, weapon socket; plus the DO27 cases — walker gait, turret aim driven by parameters, pilot attached
  to a socket.
  As built (ADR-7216): the moving-target part is an automated test with no download,
  `samples/anim_lab/tests/reach_acceptance.rs` over `anim_lab::reach` (a code-built rig and the
  checked-in `assets/reach.animgraph.ron` and `assets/turret.animgraph.ron`): for 200 fixed frames
  the hand, look and aim targets move every frame, and every frame the hand is within 1 mm of its
  target and the head's and the barrel's −Z rays pass within 1 mm of theirs (measured in the same
  frame: the solvers read that frame's parameters), with weight-0 control frames that miss by
  more than 5 cm. The downloaded Fox and CesiumMan cases stay in the binary only.
  As built (ADR-8807): the walker-gait case is a procedural walker, not a blend space:
  `samples/anim_lab/tests/locomotion_acceptance.rs` (`locomotion_scene_acceptance`) over
  `anim_lab::locomotion`. A `GaitRig` hexapod (driven by a scripted mover) walks over three 8 cm
  steps and a 10° slope, stops and turns in place; a `FootPlacement` biped with heel and toe probes
  walks up ten stairs again and again (a respawn takes it back to the bottom; the straight walk
  clip is never turned). Walking down is not in the acceptance yet (ADR-8807). Over the fixed 60 Hz run: no planted hexapod foot slides more
  than 0.1 mm, no foot target moves farther in a frame than the stepper's documented swing bound
  (about 11 cm for these settings), no forced step and no replant, a
  tripod is always down, leg tips within 1 mm of their targets, the body pitches on the slope; the
  biped's planted feet slide less than 1 cm and planted heels and toes never sink more than 1 mm
  under a tread (a respawn and its 30 settle frames excepted). The biped's one-frame ankle jump
  (8 cm) and heel/toe float (1 cm) bounds are in `locomotion_biped_stairs_have_no_pops`, ignored
  for a foot-placement finding (ADR-8807). Two runs are
  identical bit for bit. `tests/locomotion_agent.rs` (`locomotion_agent_dogfood`) checks the same
  scene through the agent protocol (`scene.query_entities`, `play.step`, `play.expect`,
  `anim.locomotion_state`); `locomotion_agent_screenshot` adds `play.wait_until` and an offscreen
  screenshot (no golden yet).
- M20, with `samples/third_person` (which needs `Character` and gameplay physics): the same graph driven
  by character state, the jump state machine, and foot IK on slopes.

## 4b. Montages, sync groups and motion warping (sprint 2026-10-21, ADR-1465)
Action playback on top of the anim graph: one-shot attacks, reloads, emotes and vaults that a
gameplay system starts, branches and stops at runtime, plus cross-node sync groups for locomotion
and runtime warping of root motion toward a target.

**Montage asset** (`.montage.ron`, frozen v2, see spec 34; `regia_animation::montage`). A `Montage` is a timeline:
- `tracks: [(slot, segments)]`: one track per slot; a track is clips played back to back
  (`segments: [(clip, start, end, rate, loops)]`, `end: None` = clip end). Track 0 is the main
  track: it alone provides root motion. Montage length = the longest track.
- `sections: [(name, start, next)]`: named time ranges (the first starts at 0; none = one implicit
  section `main`). `next` is `Continue` (play into the following section; the default), `Stop` (the
  montage ends at this section's end) or `Jump(name)` (a jump to itself loops the section).
  Links can be changed per playing instance (`set_next_section`).
- `notifies: [(name, time, duration, payload, branch)]`: `duration: 0` is an instant notify,
  `> 0` a window with a begin and an end event. `branch: true` makes it a branching point: playback
  stops exactly at its time inside the frame, the event fires, and an action armed for that name
  (`on_branch(name, JumpTo(section) | Stop)`) applies at that exact time; the rest of the frame plays
  on from there (frame-rate independent combos).
- `rate`, `blend_in`/`blend_out` `(time, curve: Linear | Smooth)`, `blend_out_lead` (seconds before
  the end at which the automatic blend-out starts; `None` = the blend-out time, so the blend ends
  with the montage), `auto_blend_out` (`false` = hold the last pose until stopped),
  `root_motion` (montage root motion drives the rig; the clip's import mode applies),
  `root_motion_off: [(start, end)]` (windows that drop root motion so input can steer),
  `warps: [(target, start, end, translation, vertical, rotation)]` (motion-warping windows, below),
  `sync_group` (while the montage blends out, its main track's clip leads that declared sync group,
  so the graph's locomotion picks up in step).

**Slots.** `MontagePlayer.slots: [SlotDef { name, group, mask }]` (rig-side; a mask is the graph's
`JointMaskDef`, `None` = full body; an undefined slot is full body in group `default`). The montage
stage runs after the anim graph in `evaluate_animators`: each playing montage track's pose is
blended over the graph output through its slot mask by the instance's blend weight, in start order
(a blending-out instance under the one blending in). Root motion blends like a layer: the graph's
by `1 − w·root_share`, the main track's by `w·root_share` (`root_share` = how much of the skeleton
root the main slot covers). Playing a montage whose main slot is in the same group as a playing
instance interrupts it: the old one blends out over the new one's blend-in time.

**Player API** (`MontagePlayer` component on the rig): `play(montage, PlayOptions { rate,
section, start_time })` → `MontageId`, `queue(..)` (starts when the group's current montage begins
its natural blend-out), `stop(id | all, blend)`, `jump_to_section`, `set_next_section`,
`on_branch`, `set_rate`, `set_warp_target(name, WarpTarget { position, facing })`,
`clear_warp_target`, `instances()` (status: section, time, weight, phase). Clips resolve against
the player's own library first, then the animator's graph library. Rates are finite and `≥ 0`
(reverse playback is supported through `PlayOptions.reverse` / `set_reverse`, with warp windows off).

**Events** (`MontageEvent { entity, id, montage, kind }`, in timeline order within a frame):
`Started`, `Section { from, to }`, `Notify { name, payload }`, `WindowBegin`/`WindowEnd`,
`BranchPoint { name }`, `BlendingOut { interrupted }`, `Ended { interrupted }` (`interrupted:
false` = completed). Open windows close (`WindowEnd`) when a jump leaves them or the instance
ends. Clip notifies inside segments still fire as `AnimNotify` while the instance's weight is at
least one half. The player keeps the last 64 events for agents.

**Sync groups** (`.sync.ron`, frozen v1, `regia_animation::sync`): `SyncSetup { groups: [(name,
roles: [(clip, MayLead | AlwaysLead | NeverLead)])], markers: [(clip, [(name, time)])] }`, set on an
animator with `Animator::set_sync_setup`. Every `Clip` node whose `sync_group` names a declared
group joins it: the leader (an `AlwaysLead` member, else the heaviest `MayLead` member of the
previous frame) advances on its own clock; followers take their time from the leader's step and
fire no notifies. With at least two marker names common to both clips, a follower matches the
leader's position between markers (marker sync); otherwise its normalised phase (length sync).
Groups not declared in a setup keep the pre-v0.1.28 behaviour (the clip's own clock). A synced blend
space whose `sync_group` names a declared group joins it as one member: its shared clock (weighted
sample length, named after its dominant sample) leads or follows by normalised phase.

**Motion warping** (`regia_animation::warp`). While montage time is inside a warp window whose
target is set, each frame's final root motion is corrected so the rig lands on the target at the
window end: the remaining horizontal root motion of the window (this frame plus the rest) is
rotated and uniformly scaled onto the remaining offset to the target (a similarity transform, so
the path keeps its shape); with no horizontal motion to scale, the offset is spread linearly over
the remaining window time. `vertical` spreads the height difference the same way; `rotation`
turns the rig to `facing` (or toward the target position) over the window, taking the rig's
object-space −Z as its forward axis (a rig authored facing another axis needs `facing` set). The correction is
recomputed every frame from the rig's current position (its transform plus undrained root motion),
so a moving target is tracked and the result converges at the window end.

Agent surface: `anim.montage_state`, `anim.montage_play`, `anim.montage_stop`,
`anim.montage_jump`, `anim.montage_set_warp_target`, `anim.sync_state`
(`book/agents/montages.md`); user guide `book/user-guide/montages.md`.

## S-17: six-leg tripod presentation stepper (ADR-8060)

`regia_animation::gait` accepts explicit `GaitParams` (six hip offsets, speed limits,
acceleration, step length, clearance, swing lift, slope limit, idle threshold and yaw rate)
and advances `GaitState` through `step_gait` using a host-provided floor sampler. The two
tripods alternate, planted XZ holds, swing follows a parabola, and body height follows
mean plant height. Power loss freezes all state. The local convention is +Z forward,
+X right, +Y up; hosts convert once when using a -Z-forward rig. This is presentation,
not a collision controller or a lockstep-compatible simulation replacement.

Named adaptations from S-17's source: no content tuning defaults; reject invalid input,
settings/state/floor values atomically; refuse time increments above one period; use the
same slope refusal at touchdown as in swing; no unused cross-slope setting; bounded yaw
wrapping and `shift_origin` for all stored world positions. AnimationPlugin registers
state reflection for existing scene and testing agent methods. Scheduling, origin shifts
and leg IK remain host responsibilities. Contract and error codes: `book/agents/tripod-gait.md`.
Acceptance: `regia_animation --test tripod_gait`, `regia_agent --test tripod_gait`.

### Plain-node TRS clips (P-11, ADR-8157)

`AnimationClip::node_animation` optionally holds a source-node hierarchy, bind
transforms and exact STEP/LINEAR/CUBICSPLINE channels. Plain-node animations
import independently of skins and cook as `nodeclip/<name>`. Sampling composes
parents in the imported scene frame and returns source-indexed local/world
matrices and an optional baked-geometry delta. Hosts bind these presentation
outputs explicitly per model instance; graph blending and automatic ECS
application are outside P-11. Both cooked writers retain node data losslessly.
Source-time slicing preserves original key boundaries and cubic tangents.
See [the v1 node body](../formats/node-tracks.md) and
[agent inspection](../../book/agents/plain-node-animation.md).

### Cooked contact curves and one foot lock (ADR-8308)

A glTF source whose `.meta` settings hold `contact_curves: Some((feet: [...], thresholds: (...),
looping: true, treadmill: false))` (every field optional; empty `feet` = the guessed humanoid
foot roles) cooks one float curve `contact.<foot joint>` per foot onto every skinned clip: the
motion-matching contact labels (spec 52 § 2.2a; height over the foot's own lowest point in the
clip, speed in object space, or over the clip's ground for a treadmill clip) at the clip's frames,
1 planted and 0 swinging. The cooked clip already stores named curves, so nothing in a format
changed; an older importer ignores the key. The graph blends the curves like any named curve and
writes them into the animator's parameters; `foot_ik::contact::contact_weight` reads one for
`FootPlantLeg::contact`, now a weight in `[0, 1]` (`None` keeps the lift rule over the bind ankle
height, which is wrong for clips whose sole is off the rig origin, retargeted clips and mixes).
`foot_ik::lock::PlantLock` is the crate's one lock state machine (touchdown pin, teleport re-pin,
stance exit, release with a fixed or a following pin): the planter, the motion-matching
`FootLocker` and the crowd's exact-tier lock use it with their pre-ADR outputs bit for bit.
`anim.clip_report` lists each clip's contact curves.

### S-17 driven mode: simulation body, presentation feet (ADR-8474)

The documented default for any game is `step_gait_driven(params, state, GaitBody { position, yaw,
velocity }, dt, floor)`: the host's simulation, physics mover or navigation owns the body, which is
copied bit for bit into `GaitState` (XZ, yaw, velocity; body Y stays the stance height). The stepper
advances the tripod phase from ground speed, places feet and follows stance height through the feet
core it shares with `step_gait`. `GaitBody` uses the object convention (+Y up, +X right, -Z forward,
yaw about +Y); `step_gait` keeps its +Z-forward integrating convention for toys and tests. Both modes
turn by the same yaw; only the local forward axis differs, so hip offsets convert by negating their
forward component (table in the `gait` module docs). A body jump longer than `step_length` in one step,
or a switch between modes, replants at neutral and is counted in the reflected `GaitState::replants`;
the reflected `GaitState::mode` records the last entry point. Body motion and gameplay footsteps
belong to the simulation; feet, IK, tilt, pelvis and body offsets are presentation and never feed a
checksum (ADR-8474 table). Acceptance: `driven_*`, `a_driven_teleport_replants_and_counts`,
`planted_feet_hold_world_xz_under_a_driven_body` in `regia_animation --test tripod_gait`.

### S-17 idle: settle and turn in place by stepping (ADR-8539)

At or below `idle_speed`, in both modes, feet no longer replant at neutral every step. The swing in
progress finishes on its arc with its landing bent smoothly to neutral (the hip under the body); then
each tripod with a foot farther than `settle_distance` from neutral takes a normal swing there, in
tripod order, one tripod at a time, each lasting `settle_period`. While yaw changes at idle (turning in
place) the step triggers are `turn_step_distance` from the rotated neutral or `turn_step_angle` about
the body center; the other tripod stays planted, so at least three feet are planted on every step and a
planted foot holds its world XZ until it steps. A foot whose neutral is refused by `max_slope` stays put.
With no swing in progress and no foot past its threshold, the reflected `GaitState::settled` is true and
the feet never move (height still follows the floor). New `GaitParams` fields validate as `gait_settings`;
an idle step that runs a swing longer than one `settle_period` is `gait_step_too_large`. Acceptance:
`no_foot_jumps_more_than_its_swing_bound`, `stop_settles_to_neutral_within_two_periods`,
`settled_feet_never_move`, `turn_in_place_steps_instead_of_sliding`,
`idle_follows_a_changing_floor_without_moving_xz`, `settle_and_turn_allocate_nothing` in
`regia_animation --test tripod_gait`; `settled_is_asserted_through_dispatcher` in
`regia_agent --test tripod_gait`.

### S-17 ground: layers, normals, no ground, footholds and reach (ADR-8792)

The steppers take any `GaitGround`: `sample(x, z, expected_y, band) -> Option<GroundHit { y, normal }>`.
`expected_y` is the body height target (`GaitState::body_height_target`) minus `clearance` (the host body's Y
at `at_rest`, `at_rest_driven` and on driven replants; ADR-10053: the eased visual height may lag the target) and `band` is `GaitParams::ground_band`, so a multi-layer adapter (tunnel floor under terrain,
deck under a bridge, stacked floors) answers the layer near the walker; the stepper also treats a height
outside the band as no ground. Every `Fn(f32, f32) -> f32` is a ground through a blanket impl (normal +Y,
always `Some`). `None` is not an error; a nonfinite height or normal, or a zero normal, is `gait_floor`.
A landing (walking, idle or initial plant) is refused when there is no ground, the height is outside the
band, the surface normal or the step from lift-off is steeper than `max_slope`, or the point is beyond the
leg's reach; it then tries `foothold_candidates` ring points (8 per ring, rings `foothold_spacing` apart,
fixed order in the body frame, at most 64) and takes the nearest ring's valid point with the smallest height
change. With none the foot holds its lift-off point (reflected counter `refused`) or, without ground there,
hangs `hang_drop` below the expected floor (`planted == false`, `FootMode::Hanging`). A planted foot beyond
`max_reach[i]` (3D from the hip at body height) is forced into an out-of-phase step of `settle_period`
(reflected counter `forced_steps`); after every step no planted foot is beyond reach, and fewer than three
planted feet occur only during a forced step or while a foot hangs. `FootTarget::normal` carries the unit
ground normal. `regia_animation` never queries physics, terrain or world geometry: hosts write adapters
(a physics raycast, a heightfield, a multi-layer tunnel query). Acceptance:
`tunnel_floor_is_chosen_by_expected_height`, `a_hole_is_stepped_around`, `cliff_edge_foot_hangs_and_reports`,
`no_planted_foot_ever_exceeds_reach`, `closure_floors_still_work`, `ground_queries_allocate_nothing` in
`regia_animation --test tripod_gait`; `ground_counters_are_asserted_through_dispatcher` in
`regia_agent --test tripod_gait`.

### S-17 patterns: any leg count and gait (ADR-8653)

`GaitParams::patterns` is a `GaitPatternSet` of one to `MAX_PATTERNS` (4) `GaitPattern { legs, phase_offset,
duty_factor }`, slowest first, all with the same `legs` (1 to `MAX_LEGS` = 16). Foot `i`'s phase is
`(cycle + phase_offset[i]) mod 1`: phase 0 is touchdown, stance while below `duty_factor`, swing from lift-off
to the next touchdown on the usual arc. Per-foot arrays (`hip_offsets`, `max_reach`, `GaitState::feet`,
`foot_modes`, `foot_phase`) have 16 entries with the first `legs` used; `GaitState::feet()` is that slice.
`step_length` is the body travel during one stance (stride `step_length / duty_factor`). The clock is the
pattern's lift-off schedule: feet with the same lift-off time (within 1e-5 cycle) form a group, groups run in
cycle order, `GaitState::beat` is the group whose turn it is and `phase` the progress through its beat. A beat
boundary lands every swing that has ended (within 1e-5 cycle) and then lifts the next group; a swing that ends
mid-beat lands there. A step may advance at most one stance (`gait_step_too_large`). Presets:
`tripod()` (6 feet, duty 0.5, bit for bit the ADR-8060 stepper), `ripple(n)` (even 4..=16, duty `1 - 2/n`),
`wave(n)` (even 2..=16, duty `(n-1)/n`, one foot in the air at a time, back to front), `quadruped_walk()` (0.75),
`trot()`, `pace()` (0.5), `bound()` (0.4), `biped_walk()` (0.6). While walking the set picks the pattern by
ground speed: up past `switch_speeds[k] + hysteresis`, down below `switch_speeds[k] - hysteresis`, one neighbor
at a time. A switch blends offsets (the short way, at most half a cycle) and duty over one cycle on per-foot
phases; neighboring duty factors differ by at most `MAX_DUTY_STEP` (0.5), so every foot's phase advances, feet
lift when it passes the blended duty and land when it wraps, and every step keeps the swing bound. A blend still
running when the gait goes idle ends at once; feet in the air finish as forced steps (`FootMode::Forced`). Idle
settling and turning in place step one lift-off group per `settle_period` in pattern order.
`GaitState::stability_margin` is the signed XZ distance from the body center to the convex hull of the planted
feet (McGhee and Frank 1968): positive inside, negative outside, `NO_SUPPORT_MARGIN` (`-f32::MAX`, finite, so
agent `lt`/`lte` checks hold) with no foot planted (ADR-8736); `support_margin(points, center)` computes it (negative
infinity for no points). A trot, pace or bound is never positive (at most two planted feet; a bound has flight
phases with none).
Invalid patterns and sets, and settings whose leg count differs from the state's, are `gait_pattern`
(`GaitError::Pattern`). `LEG_COUNT` is a deprecated alias of `TRIPOD_LEGS` (6). Reflected: `legs`, `beat`,
`pattern`, `pattern_index`, `blending`, `blend`, `foot_phase`, `stability_margin`. Acceptance: every test of
`regia_animation --test gait_patterns`; `pattern_fields_are_asserted_through_dispatcher` in
`regia_agent --test tripod_gait`; the unchanged `presentation_trace_pin` in `regia_animation --test tripod_gait`.

### S-17 GaitRig: the engine walks the rig (ADR-8707)

`regia_animation::gait_rig::GaitRig { params, legs: Vec<GaitLeg { root, tip, limits }>, body, mode, auto_hips,
ik_iterations, ik_tolerance, probe_height, probe_length }` on a rig entity (`SkinnedMesh`, `Animator`) is an
engine-side walker. `step_gait_rigs` runs in `PostUpdate` (`PostUpdateSet::AnimationApply`) before
`apply_animation`: it resolves the legs on first use and when the skeleton (pointer), the names, the limits or the
body name change (three joints root to tip: `ik::solve_two_bone_into` with the current bend; any other count:
`chain_ik::solve_chain` FABRIK with `GaitLeg::limits`); measures the neutral footholds from the bind pose
(`GaitParams::hip_offsets_from_skeleton(skeleton, legs, mode)`: each tip's bind XZ, driven `[x, -z]`, integrating
`[-x, -z]` for a stepper yaw of the rig yaw plus pi), scaled by the rig's `scale.x`, unless `auto_hips` is false;
steps the gait by `Time::delta` (equal substeps of at most 1/30 s, at most 8; a driven body is interpolated
across them; a substep refused as `gait_step_too_large` is retried at half the length, at most 4 halvings per
frame, and a finest slice still refused is stepped with the ground speed capped at
`step_length / h * (1 - 1/1024)`, not counted as an error, ADR-10053; a driven frame that moved the body more
than `step_length` is one replanting call; `reset` and the driven `last_translation` are consumed only by a step
that commits, ADR-10053; non-finite
`ik_tolerance`, `probe_height` or `probe_length` is `gait_rig_step`, ADR-8800); and stores the state in the rig's `GaitState` (inserted by the engine). Ground is the
`GroundProbes` resource (ADR-8388) cast straight down from `expected_y + ground_band` for `2 * ground_band`
(unbounded band: `probe_height` up, `probe_height + probe_length` long), ignoring the rig entity; with no probe it
is the plane at the rig's floor. Driven mode (the default) copies the rig transform's XZ and yaw (object
convention), its velocity from `GaitVelocity` or the transform's change over the frame, and asks the ground near
the rig's floor plus `clearance`. Integrating mode steps from `GaitControls { forward, strafe, steer }` and writes
the rig's translation (stance floor minus the bind floor) and yaw. In `apply_animation`, after pose-mode foot
placement and before eye gaze, the body joint moves by the stance floor (`GaitState::position.y - clearance`) minus
the rig's floor (`translation.y + scale * bind floor`, the bind floor being the lowest leg tip), as a world
vertical offset, and every resolved leg is solved to its foot target plus its bind tip height over the bind floor.
A rig without `GaitRig` takes the same path as before (no pose copy is added for it). `GaitRigStatus {
probe_installed, legs_resolved, issue, warnings, step_errors }` reports `gait_rig_leg_unresolved` (that leg is
skipped), `gait_rig_body_unresolved` (no body lift), `gait_rig_leg_count` (legs not the pattern's count: nothing
is stepped) and `gait_rig_step` (a refused step: state and pose unchanged). No allocation per frame after the
first resolve. Body pitch and roll are not applied. Acceptance: `regia_animation --test gait_rig`;
`regia_agent --test gait_rig`.

### S-17 posture: body height and tilt from the feet (ADR-8706)

`GaitParams::posture: GaitPosture` (reflected). `GaitPosture::UPRIGHT` (`swing_weight` 0, `height_tau` 0,
`height_slew` infinity, `attitude_weight` 0) is the stepper above bit for bit. Otherwise the body height target
`GaitState::body_height_target` is `clearance` plus the weighted mean of every foot's floor: planted feet at their
plant height (weight 1), feet in the air (pattern swings and forced steps toward ground) at the ground line under
them, lift-off height blended to landing height by swing progress (weight `swing_weight` in `[0, 1]`). Body Y eases
toward it (`y += (target - y) * (1 - det_exp(-dt / height_tau))`, `height_tau` 0 = at once) and moves at most
`height_slew * dt` per step; construction and replants snap to the target; with no foot planted or in the air over
ground the height holds. Tilt: a least-squares plane `y = a * right + b * forward + c` through the planted feet in
the stepper's frame (heights taken relative to the first foot so level ground gives exactly zero slopes). Feet
within about 1 % of a line (`det <= 1e-4 * spread^2`) fall back to the fit over planted feet plus feet in the air at
their ground line; collinear again, to the minimum-norm slope along their principal direction; every point at one
spot holds the tilt. `pitch = atan(b)`, `roll = atan(a * cos(pitch))` (both body axes in the plane), clamped to
`max_pitch` / `max_roll` (`[0, pi/2)`), times `attitude_weight` (`[0, 1]`), eased with `attitude_tau`; after the
ease the stored angles are clamped to `max_pitch * attitude_weight` / `max_roll * attitude_weight`, so weight 0
writes the identity at once (ADR-10053). A non-finite stored `pitch`, `roll`, `attitude` or `body_height_target`
is replaced (0, 0, identity, body Y) before a step and rebuilt by it (ADR-10053).
`GaitState::attitude` is `qx(pitch, sign by mode) * qz(roll)` as `[x, y, z, w]` in the local frame (+X right, +Y up,
+Z forward integrating / back driven), exactly the identity when both angles are zero. Tilt is output only: it
moves no foot. Out-of-range posture values are `gait_settings`. Reflected: `body_height_target`, `pitch`, `roll`,
`attitude`. Acceptance: every test of `regia_animation --test gait_posture`;
`posture_fields_are_asserted_through_dispatcher` in `regia_agent --test tripod_gait`.

### S-17 gait files: `.gait.ron` (ADR-8727, gap G7)

`regia_animation::gait_asset`. A `<name>.gait.ron` (format `gait` v1, `docs/formats/gait-ron.md`) holds a
walker's `GaitParams` as data: patterns (presets by name or explicit offsets and duty) with switch speeds and
hysteresis, speed limits, step, lift, clearance, slope and reach limits, settle and turn thresholds, foothold
search, ground band, optional posture, the body joint and the legs by root and tip joint name with optional hip
offset and reach overrides. `GaitAsset::from_ron` reads `format_version` first (`gait_version`), parses with unknown
fields refused (`gait_parse`), checks the leg list (`gait_legs`) and runs `GaitParams::validate` (`gait_settings`,
`gait_pattern`). `GaitAssets` (resource, installed by `AnimationPlugin`) stores validated assets by path and hands
out `Handle<GaitAsset>`; `reload_gait_assets` (`PreUpdate`) reads a file again on `SourceAssetChanged`, keeps the
last good value on a refused reload and records the error. `GaitRig::from_asset` keeps the handle in
`GaitRig::asset`; `step_gait_rigs` takes params, legs and body again whenever the handle's generation changes (the
same frame as the reload), and legs with a hip offset in the file keep it instead of the bind-pose measure. Agent:
`anim.gait_validate {path | text}`. Acceptance: every test of `regia_animation --test gait_asset`;
`gait_files_are_validated_through_dispatcher` in `regia_agent --test gait_asset`; fuzz target `anim.gait`; corpus
`gait/v1.ron`.
