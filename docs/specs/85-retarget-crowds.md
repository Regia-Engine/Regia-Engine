# Spec 85: Retargeting on real content, and crowds

Status: proposed (ADR-1857). Plan item: v0.3.x (b) "animation retargeting on real content (bone map, rest-pose fix,
root-motion scale)" and the "crowd animation at scale" row of `docs/master-plan.md`. Builds on `specs/17-animation.md`
§ 3b and § 4 (`regia_animation::retarget`, `bind_pose`, `profile`).

## 1. Purpose

`regia_animation::retarget` already pairs two rigs through roles, corrects rest-pose differences per bone and scales
translation by hip height. It was written and tested against small synthetic rigs with tidy names. Real rigs differ:

- **Names.** Exporters name joints `Skeleton_torso_joint_1`, `leg_joint_L_1`, `Bip01 L Thigh`, `DEF-thigh.L`,
  `lShldrBend`, `ik_foot_l`. The guesser must find the humanoid roles in these, and must not pick IK, twist or end
  joints.
- **Rest frames.** Bind rotations are rarely identity (bones along local X or Y, rolled frames, centimetre roots
  with a 0.01 scale).
- **Facing.** One exporter faces −Z, another +Z. Bone-by-bone rest correction cannot see a whole-body yaw.
- **Per-project fixes.** A bone map, a rest offset on one bone, a root-motion scale, a joint that must keep its
  bind translation. These must be saved next to the content, not rebuilt by code.
- **Quality.** Nobody can see a retarget is wrong until it is on screen. A report must say so: bone directions,
  foot sliding, root speed.

Crowds play the retargeted clips on hundreds of rigs. Each `Animator` evaluates a full graph per rig per frame. A
crowd needs a cheaper, deterministic path: one clip bank per rig family, phase-synchronised gait blending, stride
matching (playback rate from ground speed), level of detail by update interval, and pose sharing between agents in
the same gait state.

## 2. Retargeting on real content

### 2.1 Name guessing (`RetargetProfile::guess_humanoid`)

A joint name is split into lower-case **tokens**: at every non-alphanumeric character, at lower-to-upper case
changes (`lShldrBend` → `l shldr bend`), before the last capital of a capital run followed by a lower-case letter
(`LShoulder` → `l shoulder`) and at letter/digit changes (`Bip01` → `bip 01`). Then:

- **Side.** The first side token anywhere in the name gives the side: `l`/`left`/`lf`/`lft` or `r`/`right`/`rt`/`rgt`.
  With none, a core that starts with `left` or `right` (all-lower-case `leftupleg`) gives it.
- **Excluded joints.** A joint with a token in the exclusion list never takes a role: `twist`, `roll`, `ik`,
  `pole`, `end`, `nub`, `tip`, `target`, `helper`, `socket`, `attach`, `prop`, `weapon`, `corrective`.
- **Noise tokens.** These are dropped before matching: `mixamorig`, `def`, `org`, `mch`, `bip`, `bn`, `jnt`,
  `joint`, `skeleton`, `skel`, `bone`, `base`, `cc`, `bend`, `drv`, `deform` and digits.
- **Core.** The remaining tokens, joined, form the core. It matches the keyword table (old keywords plus
  `shldr` → upper arm, `collar` → shoulder, `torso`/`abdomen`/`chest` → spine, `hip` → hips).
- **Chain order.** The bare cores `arm` and `leg` are **generic**. After the specific keywords, the generic joints
  of one side, in depth order, fill that limb's still-missing roles in chain order:
  - arm: upper arm, lower arm, hand (shoulder first when there are four);
  - leg: upper leg, lower leg, foot, toes.

  Mixamo's `LeftArm`/`LeftLeg` then still map to the upper arm and the shin. CesiumMan's
  `Skeleton_arm_joint_L__4_ … __2_` fill a whole arm.
- **Topology fallbacks.**
  - With no hips keyword, the hips are the lowest common ancestor of the two upper legs.
  - With no head keyword, the head is the deepest joint of a neck chain of two or more joints.

The shallowest-joint-wins rule and the spine rule are unchanged.

### 2.2 Facing

The retargeter estimates each rig's forward direction from its bind pose: `left × up`, where `left` runs from the
right upper leg (or upper arm, or shoulder) to the left one. It snaps the yaw between the two rigs to the nearest multiple of
90°. A non-zero facing yaw `F` conjugates every object-space delta (`F·Δ·F⁻¹`), rotates the source rest directions
used by the rest correction, and rotates translation offsets, so root motion is turned too.
`Retargeter::facing_yaw()` reports the yaw. A rig without both sides has `F = identity`.

### 2.3 Per-role settings and the `.retarget.ron` asset

`RetargetSetup` (`regia_animation::retarget_setup`, file `.retarget.ron`, `RETARGET_FORMAT_VERSION = 1`):

```ron
(
    format_version: 1,
    name: "mixamo-to-guard",
    source: (roles: Guess, overrides: { "leftHand": "mixamorig:LeftHand" }),
    target: (roles: Profile("generic-humanoid"), overrides: {}),
    // Guess | ByName | Profile("<builtin name>") | Explicit({ role: joint })
    roles: {
        "hips": (translation: Some(Scaled)),          // Bind | Animated | Scaled
        "leftUpperArm": (rest_offset_deg: (0.0, 0.0, -10.0)),  // XYZ Euler, object space
        "rightToes": (skip: true),
    },
    translation_scale: HipHeight,      // HipHeight | Fixed(f32)
    root_motion: Auto,                 // Auto (= translation scale) | Fixed(f32) | Off
    align_facing: true,
)
```

- `RetargetSetup::build(&source, &target) -> Result<Retargeter, RetargetError>`.
- `Retargeter::new` is unchanged and equals a default setup with explicit profiles.
- `TranslationMode`:
  - `Bind` keeps the target's bind translation (the default for every joint except the root and the hips).
  - `Animated` copies the source's local translation (props, IK helpers in the same units).
  - `Scaled` moves by the source's object-space offset from bind times the translation scale (the default for the
    root and the hips).
- `rest_offset_deg` rotates the target bone's rest orientation before the source's motion is applied. It is the
  manual fix when the automatic rest correction is not enough.
- `root_motion: Fixed(k)` scales the root-motion joint's translation by `k` instead of the translation scale. `Off`
  drops root motion from baked clips.
- Format rules: frozen like every content format (`specs/12` § 4a). The loader refuses an unknown `format_version`.
  `cargo xtask check-formats` hashes the serde shape (`retarget`). `regia upgrade-project` lists the extension.
  Version 1 is the first freeze: there is no older version to migrate. A future v2 adds a migrator from v1, and
  `retarget_migration_sources()` lists the versions it reads.

### 2.4 Quality report

`Retargeter::check(clip: Option<&AnimationClip>) -> RetargetReport` (pure, deterministic):

- **Mapping.** Mapped roles; unmapped source and target roles; required humanoid roles missing on the target
  (checked only when the target maps the hips: a rig without hips is not treated as a humanoid with holes).
- **Rig.** Translation scale, root-motion scale, facing yaw, and the rest correction per role in degrees (largest
  first).
- **Per clip** (bake sampled at the clip rate, at most 600 frames):
  - the worst and mean bone-direction error between source and target over the mapped bone pairs, in degrees,
    with the worst role and time;
  - source and target root speed (m/s) and their ratio;
  - for a clip without root motion, source and target **stance speed** (§ 3.2, m/s), and the ratio from those;
  - foot skate on the source and target, m/s. This is the planted foot's horizontal speed in frames where the
    source foot is within 2 cm of its lowest height, is the lower foot, and moves at most a quarter of the root
    speed plus 0.1 m/s (the height and velocity footplant test of Kovar et al. 2002; height alone counts a run's
    first and last swing frames, where the foot moves at speed / (1 − duty)). A treadmill clip's feet are measured
    over the ground: the stance velocity is added back.
- **Leg ratio** (rig part): target over source leg length, hip joint to knee to ankle, both sides. A retargeted leg
  strides with its length, not with the hip height: CesiumMan's stride ratio onto the lab rigs is 9 % above their
  hip-height ratio. A stance ratio is therefore held against the leg ratio. Root motion scaled by hip height then skates by that
  difference, which `foot_skate` reports and `root_motion: Fixed(leg_ratio)` fixes.
- **Issues** (`code`, `detail`):

  | Code | Meaning |
  | --- | --- |
  | `missing_role` | a required humanoid role is missing |
  | `large_rest_correction` | a correction is above 100° (a probable wrong mapping) |
  | `direction_error` | the worst error is above 5° |
  | `root_motion_lost` | the source has root motion and the bake has none |
  | `speed_ratio` | the root speed ratio differs from the root-motion scale by more than 5 %, or the stance speed ratio from the leg ratio by more than 15 % |
  | `foot_skate` | the target skates more than the source skate, scaled by the translation scale (a treadmill clip: by the stance ratio), plus 0.05 m/s |
  | `facing_turned` | the facing yaw is non-zero; informational |

### 2.5 The Retarget panel (editor, ADR-7801)

The editor's Retarget panel (`regia_editor::anim::retarget_panel`, agent methods `editor.retarget_*`,
`book/agents/retargeting.md`) edits a `.retarget.ron` setup (§ 2.3; no format change):

- **Rigs.** A source and a target rig: a `.glb`/`.gltf` (first skin and clips), a `.bvh`, the demo biped or the test
  fixture pair.
- **Map.** Two columns (source joint, target joint) per role, grouped into chains (`regia_animation::retarget_chain`):
  - root: hips;
  - spine: spine, chest, upper chest;
  - head: neck, head;
  - arms: shoulder, upper arm, forearm, hand;
  - legs: thigh, shin, foot, toes;
  - "Other": custom roles.

  Both sides are saved as `Explicit` maps.
- **Auto-map.** First by chain: the § 2.1 guesses on both rigs, and every role both have is paired. Then by name: an
  unmapped target joint pairs with the source joint whose name matches without its `namespace:` prefix, case and
  punctuation. The pair is a custom role named after the target joint.
- **Alignment.** One `rest_offset_deg` per chain, written to each of its roles. "Align to T-pose/A-pose" sets the
  offset so the target's aim bone points along the reference direction while the source is at rest:
  - arms out to the target's sides, level for a T-pose and 45° down for an A-pose;
  - legs down;
  - spine and head up.

  The aim bones are upper arm → forearm, thigh → shin, spine → chest and neck → head.
- **Preview.** A source clip sampled on the source rig, and `Retargeter::retarget_pose` of the setup on the target,
  drawn side by side.
- **Retarget clips…** An editor job bakes the selected clips with `Retargeter::retarget_clip`. Each is written as
  `<out>/<clip>.cooked`: the cook's compressed clip payload in a cooked-file container. Clips are staged and moved
  into place only when all are baked, so a cancel writes none. A baked clip's end pose matches `retarget_pose` of the
  same setup within the compression tolerances.
- **Not built.** Writing the map into a BVH `.meta` (`BvhRetarget.bone_map` cannot hold roles or rest offsets). It
  needs a `BvhRetarget.setup` field and waits for a format step.

## 3. Crowds (`regia_animation::crowd`)

```rust
pub struct CrowdGait { pub clip: Arc<AnimationClip>, pub speed: f32, pub cycle: f32, pub velocity: Vec3, /* ground pin */ }
pub struct CrowdArchetype { /* skeleton, gaits sorted by speed, hip height */ }
impl CrowdArchetype {
    pub fn new(name, skeleton: Arc<Skeleton>, clips: Vec<AnimationClip>) -> Result<Self, CrowdError>;
    pub fn retargeted(name, rt: &Retargeter, source_clips: &[AnimationClip]) -> Result<Self, CrowdError>;
    pub fn with_gait_speed(self, clip: &str, speed: f32) -> Result<Self, CrowdError>;
}
pub struct CrowdLod { pub distance: f32, pub interval: u32, pub phase_buckets: u32 }
pub struct CrowdSettings { pub fixed_step: f32, pub lods: Vec<CrowdLod>, pub min_rate: f32, pub max_rate: f32, pub chunk: usize,
                           pub foot_lock: bool }
pub struct CrowdAgentDesc { pub archetype: usize, pub position: Vec3, pub yaw: f32, pub velocity: Vec3,
                            pub phase: f32, pub proportions: Option<InstanceBindPose> }
pub struct CrowdAnimator { /* archetypes, agents, settings, step counter, stats */ }
impl CrowdAnimator {
    pub fn add_archetype(&mut self, a: CrowdArchetype) -> usize;
    pub fn add_agent(&mut self, d: CrowdAgentDesc) -> Result<CrowdAgentId, CrowdError>;
    pub fn set_motion(&mut self, id, position: Vec3, velocity: Vec3) -> bool;  // false: no such agent
    pub fn set_viewer(&mut self, position: Option<Vec3>);
    pub fn step(&mut self);                       // one fixed step
    pub fn advance(&mut self, dt: f32) -> u32;    // whole steps, remainder carried
    pub fn pose(&self, id) -> Option<&Pose>; pub fn globals(&self, id) -> Option<&[Mat4]>;
    pub fn palette(&self, id) -> Option<&[Mat4]>; pub fn agent_state(&self, id) -> Option<CrowdAgentState>;
    pub fn stats(&self) -> &CrowdStats; pub fn digest(&self) -> u64;
}
```

Per step:

1. **Phase.** Every agent advances its phase. Phase is normalised and shared by all gaits (gaits blend
   phase-synchronised).
   - The ground speed `v` is the horizontal velocity. It selects the gait pair `(i, i+1)` and the weight
     `w = (v − sᵢ)/(sᵢ₊₁ − sᵢ)`, clamped to 0..1.
   - Speeds `s` are the archetype gait speeds times the agent's hip-height ratio.
   - Stride matching: gait `k` has stride `Lₖ = sₖ · cycleₖ` (how far a planted foot slides back per lap), so the
     blended pose slides back `lerp(Lᵢ, Lᵢ₊₁, w)` per lap. With `c = lerp(cycleᵢ, cycleᵢ₊₁, w)`, the playback rate
     is `clamp(v · c / lerp(Lᵢ, Lᵢ₊₁, w), min_rate, max_rate)`, and 1 when that stride is 0. The feet then move at
     the ground speed. (Dividing by the blended speed instead is exact only at the gaits themselves; mid-blend it
     slides by up to a fifth of the speed.)
   - `dφ = step · rate / c`.
   - Facing comes from the velocity when the speed is above 0.05 m/s and is otherwise kept.
2. **Due agents.** An agent's LOD tier is the first `CrowdLod` whose `distance` is not below its distance to the
   viewer (no viewer: tier 0). The agent updates this step when `(step + id) % interval == 0`; the id stagger spreads
   far agents over steps.
3. **Pose keys.** With `phase_buckets > 0` an agent's key is `(archetype, i, w quantised to 1/16, φ quantised to
   1/phase_buckets)`. Agents with the same key share one evaluated local pose, and the pose is evaluated at the
   bucket's value, never at the first agent's own value. With `phase_buckets == 0` the agent gets its own exact pose.
4. **Evaluation**, in parallel over the unique keys (`regia_core::tasks::par_chunks_mut`):
   - sample both gaits at `φ·duration`;
   - blend (translation lerp, rotation nlerp on the shortest arc);
   - pin the joint that carries the ground travel (§ 3.2) in place on the object's ground plane (the agent's
     position comes from the game or navigation).
5. **Finish**, in parallel over chunks of due agents:
   - the instance retarget (`bind_pose::retarget_to_instance`) when the agent has its own proportions;
   - forward kinematics (globals);
   - **foot locking** (`CrowdSettings::foot_lock`, default on) for agents in exact tiers (interval 1, no phase
     buckets) whose archetype has contiguous upper leg → lower leg → foot chains (guessed roles):
     - each gait has a contact window per foot, built once from 64 phase samples of the root-pinned cycle: a foot
       is planted when it is above its lowest point by at most 3 % of the hip height or a quarter of its own lift,
       whichever is more, and its ground velocity (root-pinned velocity plus the gait's velocity) moves it forward or
       sideways by at most half the gait speed plus 0.1 m/s and backward by at most the gait speed plus 0.1 m/s (the
       height and velocity footplant test of Kovar et al. 2002). A gap of at most 2 samples (about 3 % of the cycle)
       that the foot spends low is closed. The window is the longest planted run (touch-down phase and length), or
       "always" for a gait whose foot never lifts (an idle).
       The wider bands are for captured motion: on the CesiumMan walk the ankle dips twice in stance, by 3.4 cm on
       a 1 m hip, and its speed swings ±45 % around the gait speed, so 3 cm and a quarter of the speed left a window
       of about 4 % of the cycle. The gate is one-sided because only a forward foot can be a swing, at speed / (1 − duty)
       over the ground: one stance foot of that walk peaks at 1.8 times the gait speed, lagging the ground, and a
       symmetric gate cut its window to 11 %. The gap rule is for clips whose first key comes after time 0: they clamp
       there, and the foot looks stopped for two samples in mid-stance;
     - the gait pair's windows blend by the weight (touch-down phase on the short way round, length linearly; an
       "always" window follows the other gait), so the planted span follows the blended stance. Thresholding the
       blended contact instead switches to the walk's whole stance just below weight ½, which outruns the leg;
     - a planted foot is held at the world position where it touched down by two-bone leg IK
       (`ik::solve_two_bone`), and re-locked where the animation puts it if that drifts more than ¼ of the hip
       height away;
     - once the foot lifts, the lock fades out over 4 steps;
     - then forward kinematics again;
   - the skin palette.

   Agents not due keep their last palette. Why foot locking: blending two gaits with different duty factors (a walk
   plants each foot for ~55 % of the cycle, a run for ~35 %) at a shared phase gives poses where one gait's foot is
   planted and the other's swings, so the blended foot slides however exact the stride matching is. The lab measured
   0.4–0.85 m/s of mid-blend slide at 1.6–2.7 m/s before locking.

### 3.1 ECS bridge

- `CrowdAnimation` is a resource that wraps the `CrowdAnimator`.
- `CrowdMember { agent }` is a component on a rig entity with `SkinnedMesh`.
- `CrowdAnimationPlugin` steps the crowd in `FixedUpdate`. In `PostUpdate` (`AnimationApply`) it writes
  `SkinPalette` and `JointGlobals` for each member.
- Members need no `Animator` and no joint entities.

### 3.2 Gait speed on imported clips

The glTF importer's root-motion setting is off by default, so a real clip arrives with no root-motion track, and a
speed read from that track alone is 0: no stride matching, no gait order, no contact windows. `CrowdArchetype::new`
measures each clip, in this order:

1. **A root-motion track.** The travel of its joint over one lap.
2. **Travelling hips.** Without a track, the hips or the ancestor with the longest ground travel over one lap, when
   that is at least 0.1 hip heights per second (below that it is a mocap idle's drift). A clip exported with its
   travel on the pelvis then plays on the agent instead of walking away from it.
3. **Treadmill.** A clip that goes nowhere on its own, played in place, has feet that slide back at the gait speed
   during stance. On a rig with legs, the ground velocity is minus the stance velocity: per foot, the median velocity
   over its lowest 30 % of 64 phase samples (below the planted share of a run, so the median drops the swing frames
   among them), the two feet averaged. Below 0.1 hip heights per second the clip is an idle, with speed 0.
4. **By hand.** `with_gait_speed` sets a speed the rig cannot show, such as a rig without guessable legs. The gaits
   re-sort and their contact windows rebuild. `CrowdError::UnknownClip` (`unknown_clip`) and `BadValue` (`bad_value`)
   refuse an unknown clip and a negative or non-finite speed.

Travel is measured in object space, through the bind frame of the joint's parent, so a joint under a 0.01 root scale
measures meters. The pin keeps the component along the object's up direction in that parent frame; for a root joint
that is exactly "X and Z held".

## 4. Agent protocol (`book/agents/retarget-crowds.md`)

- `anim.retarget_check { source, target, clip?, setup? }` (read).
  - `source` and `target` are rig entities with `SkinnedMesh`. `clip` is a clip name in the source rig's animator
    (single clip or graph library). `setup` is `.retarget.ron` text (default: guessed humanoid profiles).
  - Returns the `RetargetReport` as JSON.
  - Errors: `F0003` (no such rig, or a clip the source rig does not play, message `[unknown_clip]`); `F0002`
    with `[bad_setup]`, `[no_common_roles]`, `[missing_joint]`, `[bad_skeleton]` (a skeleton whose parent list does
    not match its joints), or `[unknown_clip]` when `clip` is not a string.
- `anim.crowd_status { agent? }` (read).
  - Returns the crowd stats: agents, archetypes, step, evaluated poses (one per unique pose key), shared hits,
    agents updated and skipped, per-tier counts, digest.
  - With `agent`, it also returns that agent's archetype, tier, gait pair, weight, phase, rate, speed and yaw.
  - Errors: `F0003` (no `CrowdAnimation` resource, or no such agent); `F0002` `[bad_agent]`.

Guide topic `retarget_crowds`; coverage rows in `book/COVERAGE.md`.

## 5. Determinism

- Fixed step only (`advance` carries the remainder).
- Agents are processed in id order.
- Pose keys are pure functions of agent state.
- Shared poses are evaluated at the bucket value.
- The parallel stages write disjoint slots. The output is identical for every chunk size and thread count.
- `digest()` is FNV-1a over every agent's phase, yaw and palette bits, in id order.
- Gait measurement (§ 3.2) sorts with a total order and ties by sample index, so it is a pure function of the clip.

The crowd sample checks two runs, and a serial run against a chunked run, for a bit-identical digest.

## 6. Tests and acceptance

- **Unit (retarget).**
  - Tokenizer and guesses for Mixamo, game-style, Biped, Rigify, DAZ-style and the real CesiumMan joint names and
    hierarchy.
  - IK, twist and end joints are never picked.
  - Facing: a +Z-facing target driven by a −Z-facing source walks forward.
  - A centimetre rig (root scale 0.01) with rolled bind frames keeps the bone directions within 0.1°.
  - Per-role translation modes, rest offset, and the `Fixed` and `Off` root-motion settings.
- **Unit (setup).** RON round-trip, unknown version refused, unknown builtin profile refused, explicit role maps.
- **Unit (report).** A clean retarget has no issues. A wrong mapping (swapped arms) raises `direction_error` or
  `large_rest_correction`. A doubled root-motion scale raises `foot_skate`.
- **Unit (crowd).**
  - Stride matching keeps the planted foot's world skate below 0.1 m/s at walk speed, mid-blend and on a 1.2×
    instance.
  - In-place and travelling-hips clips (no root-motion track) measure the right speeds and forward, also on a
    centimetre rig, and hold planted feet; the hips stay on the agent. Explicit speeds re-order the gaits, and bad
    ones are refused.
  - Both foot paths measured from the CesiumMan walk make one contact window of 25–45 % of the cycle, in stance.
    Their stances differ, so neither foot is derived from the other.
  - A stance foot lagging the ground stays planted; the same stretch moving forward does not. Short gaps close
    only while the foot stays low.
  - Pose sharing evaluates fewer poses than agents.
  - LOD intervals update the expected agents per step.
  - Digest equality across chunk sizes.
- **Acceptance (sample `samples/crowd_lab`).**
  - One procedural source library (idle, walk, run) is retargeted onto three rigs: a centimetre Mixamo-named rig, a
    game-named A-pose rig facing +Z with twist and IK joints, and a short Biped-named rig.
  - 240 agents on `regia_nav::NavCrowd` cross a plaza for 300 fixed steps.
  - The sample prints stats, the mean planted-foot skate of exact-tier agents over their planted frames and the
    digest.
  - Its test runs the sample twice and once with chunk 1, and requires:
    - identical digests;
    - shared hits > 0;
    - zero retarget-report issues except `facing_turned`;
    - the skate bound.
  - A second test runs the lab on in-place and on travelling-hips clips and requires the same report and skate bound.
- **Real content (optional, network).** `crowd_lab --cesium` loads the Khronos CesiumMan sample from the anim_lab
  cache, guesses its roles, retargets its walk onto the three rigs, prints the reports, plays the bake as a crowd gait
  and prints the measured speed and the foot skate with and without locking. This is recorded in the lane report and
  not run by CI.

## 7. Platforms

- Pure CPU code with no platform branches. It builds on Linux, Windows and Android aarch64 (`cargo ndk -t arm64-v8a
  check -p regia`).
- The sample's `--cesium` mode reads the cooked cache that `anim_lab` makes on the desktop (`anim_lab` imports
  with `regia_asset_pipeline`; the sample itself needs only `regia_asset`) and is desktop-only. On Android it prints a
  message and exits 0, like `anim_lab`.
- macOS: testing only.

## 8. Performance budget

- Crowd step for 500 agents of 60 joints on one desktop core: ≤ 2 ms with sharing on (tiers 0/64/16 buckets), ≤ 6
  ms with sharing off.
- `crowd.step` has a `tracing` span, and each parallel chunk has a `par_chunk` span.
- A hand-run `#[ignore]` timing test prints the numbers (no wall-clock asserts).
- Memory: one `Pose` and one palette per agent, plus the per-step key cache.

## 9. Out of scope

- Full-body IK retargeting (chain IK on the target after transfer), finger curl mapping, and retargeting additive
  clips.
- GPU animation (vertex-animation textures).
- Crowd-specific rendering. The palette feeds the existing skinned path and skinned instancing (ADR-1368).
- Crowd graphs beyond the gait blend: crowd agents that need montages use a full `Animator`.
- A crowd editor panel. The retargeting part has one: § 2.5 (ADR-7801).
