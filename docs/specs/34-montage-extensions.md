# Spec 34: Montage and motion-warping extensions

Status: proposed (ADR-1577). Plan item G3 (`BUILD-PLAN-1027.md` § 3), rung v0.3.x (b). Behavior written from the
public-sources spec set (ADR-1526); set area `runtime-animation` (montages, with the motion-warping and root-motion
material). **This is a delta spec.** The plan listed montages and warping as missing; they were built after the
plan's source review (ADR-1465, `docs/specs/17-animation.md` § 4b, `regia_animation::{montage, montage_player, warp}`,
`book/agents/montages.md`). This page lists only what the public behavior description has that Regia does not have
yet, as a prioritized list a builder can take one item at a time. Nothing here changes the semantics of existing
assets; § 1a summarizes the baseline so this page stands alone.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: implemented (ADR-1465), spec records gaps: `regia_animation::{montage, montage_player, warp}`.

## 1a. Baseline (implemented, ADR-1465)

- `Montage` (`montage.rs`, `.montage.ron`, `MONTAGE_FORMAT_VERSION` 1, no migrations yet): `tracks` (track 0 is the main
  track: root motion and slot group), `sections` with `SectionNext`, `notifies`, `blend_in`/`blend_out`, `auto_blend_out`,
  `root_motion`, `root_motion_off: Vec<TimeWindow>`, `warps: Vec<WarpWindow { target, start, end, translation, vertical,
  rotation }>`, `sync_group`; `validate`, `layout`, `from_ron_str`/`to_ron_string`.
- `MontagePlayer` (component; `pub slots: Vec<SlotDef { name, group, mask: Option<JointMaskDef> }>`): `play`,
  `play_named`, `queue`, `stop`, `jump_to_section`, `set_next_section`, `on_branch`, `set_rate`, warp targets
  (`set_warp_target`/`clear_warp_target`/`warp_target`), `instances`, `take_events`; events `Started`, `Section`,
  `Notify`, `WindowBegin/End`, `BranchPoint`, `BlendingOut { interrupted }`, `Ended { interrupted }`. An undefined slot
  is full body in group `default`. `stop(None)` drops queued montages silently (no event).
- `warp.rs`: `WarpTarget { position, facing: Option<Quat> }`, `WarpInput`, `warp_frame` (similarity correction: rotate
  and uniformly scale the remaining authored horizontal path onto the offset; linear spread when there is no
  horizontal motion; rotation turns to `facing` or toward the target; rig forward is -Z).
- `Animator::last_root_motion()`; per-clip `RootMotionMode { None, XZ, XYZ, Yaw }` (`clip.rs`); `ANIMGRAPH_FORMAT_VERSION`
  is 2 with registered migrations; agent `anim.montage_state|play|stop|jump|set_warp_target`, `anim.sync_state`
  (`book/agents/montages.md`).
- The montage stage runs after the whole anim graph; slots apply after the graph (limitation fixed by M9).

## 1. Purpose

Close the gaps between Regia's montages and the way action-playback is described publicly, in the order of value for
a third-person game template (vaults, attacks, interactions with a hand-plant on a target):

| # | Item | Rung | Format impact |
|---|---|---|---|
| M1 | Skeleton-owned slot table | v1 of this spec | new file `.slots.ron` v1 |
| M2 | Warp window extensions: modes, rotation fraction, anchor | v1 | `.montage.ron` v2 |
| M3 | Script-authored warps (no montage) | v1 | none (runtime) |
| M4 | Variant montages (derived, overrides only) | v1 | new file `.montagevar.ron` v1 |
| M5 | Checked play and the never-started rule | v1 | none (runtime) |
| M6 | Root-motion scope | v1 | none (runtime) |
| M7 | Rate curve | v2 | `.montage.ron` v2 |
| M8 | Segment links (notifies and sections follow edits) | v2 | `.montage.ron` v2 |
| M9 | Graph `Slot` node | v2 | `.animgraph.ron` v3 |
| M10 | Editor panel and agent edit surface | v2 | none |

"v1" items are one builder lane (about M); "v2" items follow and are independent of each other. Network
replication of montage play and root motion is **out of scope** until the networking crate exists; § 9 defines the
hook that keeps it possible.

## 2. Terms

Existing: `Montage`, `MontagePlayer`, `SlotDef { name, group, mask }`, `WarpWindow`, `WarpTarget`, `MontageEvent`
(spec 17 § 4b). New: **slot table** (the list of slots a skeleton offers), **anchor** (what the warp measures from),
**scripted warp** (a warp window created by code, not by a montage), **variant** (a montage derived from a parent),
**rate curve**.

## 3. M1: Skeleton-owned slot table

Today slots are defined per rig on `MontagePlayer.slots`, so two montages can name a slot that one rig lacks and
the error appears only at play time. Make the *skeleton* the owner: every rig of one skeleton offers the same lane
names.

- File `<skeleton>.slots.ron` (format name `slots`, v1) beside the skeleton asset:
  `(format_version: 1, slots: [(name: "upper_body", group: "upper", mask: Some(<JointMaskDef>)), (name: "full", group: "default", mask: None)])`.
  Names are unique, lowercase ASCII, digits, `_`, 1..=48 chars. A missing file means the engine default table
  (one full-body slot `default`, group `default`), which is exactly today's behavior.
- `SkeletonSlots` (`regia_animation::skeleton_slots`): `from_ron_str`, `to_ron_string`, `validate(&Skeleton)`
  (a mask joint the skeleton lacks is an error naming the joint), `get(name)`, `groups()`.
- `MontagePlayer::from_skeleton_slots(&SkeletonSlots)`; `MontagePlayer.slots` remains settable (explicit rig-side
  definitions win by name over the table, for projects that build rigs in code).
- `Montage::validate_against(&SkeletonSlots) -> Vec<MontageIssue>`: every track slot must exist (`montage_unknown_slot`
  with track index); a missing slot is a *warning* at load (fallback: full body in group `default`, as today) and an
  *error* in the cook step (the check is `Montage::validate_against`, a montage-aware function; spec 32's validators
  run over registry data and cannot read montage text, so Decision (ADR-1585): no spec-32 validator in v1, the cook
  and `anim.montage_describe` report the issue).
- Slot editing (add, rename, delete) is an editor/agent operation on the table: **rename and delete first list every
  montage that names the slot** (via the asset registry dependency edge `montage → skeleton`, plus a text scan of
  `.montage.ron` for `slot:` values); delete is refused while references exist unless `force`, and rename rewrites
  the referencing montages atomically (all or none).
- Rule when a montage starts: same-group interruption is by *table* group, as today by player group.

Acceptance: table round-trip; mask joint missing → error with the joint; two rigs of one skeleton resolve one
montage identically; montage with unknown slot warns at load and fails the validator; rename rewrites two
referencing montages atomically and refuses when one of them is read-only; delete without `force` lists the
referencers.

## 4. M2: Warp window extensions (`.montage.ron` v2)

Existing `WarpWindow { target, start, end, translation, vertical, rotation }` corrects root motion by a similarity
(yaw rotation plus uniform scale). Add (all `#[serde(default)]`, so v1 data reads as v2 with defaults reproducing
v1 behavior bit-for-bit):

| Field | Values | Meaning |
|---|---|---|
| `mode` | `Similarity` (default, current), `PathScale` | `PathScale` rescales the remaining authored horizontal path uniformly onto the remaining offset **without rotating it** (the path keeps its authored heading; use when the montage already faces the target) |
| `rotation_kind` | `Auto` (default), `MatchTarget`, `FaceTarget` | `Auto` = today's rule (match the target facing when `WarpTarget.facing` is set, else face the target). The explicit forms ignore `facing` / ignore it being absent, respectively |
| `rotation_fraction` | `f32` in `(0, 1]`, default 1 | The turn completes when this fraction of the window has elapsed, then holds; eased linearly in time before that |
| `anchor` | `Root` (default), `Bone(String)`, `Offset(Vec3)` | What lands on the target at `end`. `Root`: the rig origin (today). `Bone(name)`: that bone's position at the window end, taken from the sampled authored pose (so a hand-plant lands the *hand* on the ledge, not the origin). `Offset(v)`: a fixed rig-space offset from the origin |
| `tint` | `[u8;3]`, editor only | window color in the timeline; ignored at runtime |

Rules:

1. `Bone(name)` anchor: at each frame the correction needs the anchor's offset from the origin at window end. The
   montage layout can sample the main track's pose at `end` (same sampler used for root motion); cache that offset
   per (montage, window) after the first request. A bone name that the skeleton lacks is a load-time
   `montage_unknown_anchor_bone` warning; the window then falls back to `Root`.
2. The target position given to the corrector becomes `target − R·anchor_offset`, where `R` is the warped rig yaw
   at the window end (so rotating the rig also moves the bone's world position consistently). Solve by one fixed-point
   iteration (two passes) since `R` depends on the facing; the error after two passes must be under 1e-3 m for
   anchor offsets up to 2 m. (Re-derived in § 4a: exact when the facing does not depend on the aim; otherwise at
   most `|A|³/|o_h|²`.)
3. Warping applies only inside `[start, end]` of montage time; outside it the montage's authored root motion passes
   through untouched (existing rule). A frame that straddles an edge is split there (§ 4a).
4. A window whose named target is not set is inactive (no correction, no error); a window with `end <= start` is a
   load error (existing validation).
5. Rotation: the yaw remaining to the final facing is spread over `rotation_fraction × (end − start)` and the
   remainder holds at zero extra rotation. Only the yaw is used, up-axis +Y.
6. `PathScale` vs `Similarity` are mutually exclusive per window; combining `PathScale` with `rotation: true` is allowed (the
   path is scaled, the facing turns independently).

Migration: `MONTAGE_FORMAT_VERSION` 2 (one bump for all additive montage fields of M2, M7 and M8: the fields are
accepted and validated in the first lane even if M7/M8 behavior lands later, so no second bump is needed;
Decision (ADR-1585)), `montage_migration_sources()` returns `[1]`, the schema fingerprint is updated, `from_ron_str` migrates v1 by filling defaults (a pure identity migration
because every new field has a default); `regia upgrade-project` rewrites `.montage.ron` files; the lead updates
`check-formats` and the manual (spec 12 § 4a). A runtime reading a v2 file with only v1 code refuses it by version, not
by partial parse.

Acceptance: a v1 file migrates and plays bit-identically (golden-free root-motion trace comparison); `PathScale` never
changes path heading (assert direction of every per-frame delta); `rotation_fraction: 0.5` reaches the final yaw at
the midpoint of the window and holds; a `Bone` anchor 0.8 m ahead of the origin makes the bone, not the origin, land
within 1 cm of the target at `end` for 10 random targets and 2 yaw settings; an unknown bone warns and falls back.

## 4a. Addendum (ADR-2604): split at window edges, error bounds re-derived

Rule 3 says warping applies only inside `[start, end]`. A frame is a step `[a, b]` of montage time and need not
line up with the window, so a frame that straddles an edge used to warp its whole root motion, the part outside
the window included. The rule now holds per step.

**Split.** Let the window be `[s, e]` and the step `[a, b]`. The inside piece is `[lo, hi] = [max(a, s), min(b, e)]`;
when `hi <= lo` the window is not active in this step (so a zero-length window, already a load error, never warps).
With `M(x, y)` the montage's authored root motion from `x` to `y` (the layout's `root_motion_between`, each clip
filtered by its import mode, `root_motion_off` skipped) and `∘` composition of deltas (the later translation in
the frame the earlier rotation reached):

- `before = M(a, s)` when `a < s`, else no motion; `after = M(e, b)` when `b > e`, else no motion.
- `inside = before⁻¹ ∘ R ∘ after⁻¹`, where `R` is the frame's root motion as the montage stage left it. Composed
  back unwarped, `before ∘ inside ∘ after = R` exactly; for a montage at full weight on the root, `inside = M(lo, hi)`.
- The corrector warps `inside` from the rig placement after `before` (`rest = M(hi, e)`, `frame_time = hi − lo`,
  `rest_time = e − hi`, and the rotation part clipped to `[lo, hi]` the same way), and the frame's result is
  `before ∘ W(inside) ∘ after`.
- A step wholly inside the window (`a >= s`, `b <= e`) takes the old path unchanged, bit for bit.
- Scripted warps (§ 5) start on the frame they are added, so only their end can be straddled. Graph root motion
  has no authored timeline to cut, so the frame `R` covering `dt` is split at constant velocity: with
  `σ = frame_time / dt`, `inside = (σ·R.t, slerp(1, R.q, σ))`, `after = inside⁻¹ ∘ R`, result `W(inside) ∘ after`.
- Known limit: when the montage does not own the root motion (blending in or out, or a slot mask under 1 at the
  root), `before` and `after` are still the montage's authored motion at full weight and the difference is carried
  by `inside`, which the warp then corrects with the rest. Two windows touched by one step: only the first listed
  is warped (as before); the other's part passes through. A rig with non-uniform scale: the composed pieces of one
  frame get the scale after their turn (the rig applies `R·(S·t)` to the whole frame), so a piece that follows a
  turning piece in the same frame is off by the scale's anisotropy; a uniform scale is exact, and a later frame's
  re-aim cancels the error unless the window ends in that frame.

**Error bounds.** `u = 2⁻²⁴` (unit roundoff of `f32`), `N` frames in the run, `P` the rig origin, `T` a target that
does not move during the last inside piece, `A` the anchor offset, `S = |P(s)| + |T − P(s)| + |M(e, end)|` the
length scale of the path.

1. *Outside the window.* The authored motion before `s` and after `e` is passed through; the deviation from an
   unwarped run is zero before `s` (up to the rounding of recomputing `M(a, s)`, at most `4u·|M(a, s)|`), and after
   `e` the rig moves by `M(e, ·)` in the warped frame. Before this addendum the part `[a, s]` was bent by the first
   frame's similarity (scale `k`, turn `θ`): a deviation of `|M(a, s)|·|k·R(θ) − I| <= v·(s − a)·(|k − 1| + 2k·sin(|θ|/2))`.
2. *Landing at the window end (Root anchor, `Similarity` or the linear spread, vertical, rotation).* The last
   inside piece has `rest = 0` and `rest_time = 0`, so its share is 1: the similarity maps the piece's horizontal
   motion `a` onto the remaining offset `b` exactly in exact arithmetic (rotate by the angle between them, scale by
   `|b|/|a|`), the linear spread adds `b − a`, the height adds `to.y − remaining.y`, and the yaw becomes the needed
   yaw. In `f32` the landing is off by a few `u·|b|` (two `atan2`, one wrap, one rotation, one scale: under `8u·|b|`).
   The character controller then adds each later frame's motion with an error of at most `u·|P|` per addition, and
   the re-aim of every frame cancels all error made before the last piece. So at the end of the montage
   `|P(end) − (T + M(e, end))| <= (2N + 8)·u·S` (the factor 2 covers the composition of the split pieces in each
   frame). Before this addendum the error was `|M(e, b)|` of the straddling frame, the authored motion past the end
   that was mapped onto the target: up to `v·dt`, 3.3 cm for a 1 m/s move at 30 Hz, and up to the whole
   after-window motion of a long step. `PathScale` never turns, so it lands `|b|` along the authored heading, off
   the target by `2|b|·sin(φ/2)` where `φ` is the angle between the authored remaining path and the offset; the split
   does not change that.
3. *Moving target.* The last piece aims at the target as sampled at the start of its frame; the miss at the window
   end is the target's motion during that frame, at most `|v_T|·dt`.
4. *Rule 2, two-pass anchor solve.* The aim `o` solves `o = g(o) = T − P − R(ψ(o))·A`, where `ψ` is the final facing.
   From `o₀ = T − P`, two passes give `o₁ = g(o₀)` and `o₂ = g(o₁)`; the warp puts the origin on `P + o₂` and turns
   to `ψ(o₂)`, so the anchor lands at `T − R(ψ(o₁))·A + R(ψ(o₂))·A`. With `κ = sup |∂ψ/∂o|`, `|o₁ − o₀| = |A|` and
   `|R(ψ(x))·A − R(ψ(y))·A| <= |A|·κ·|x − y|`, so the landing error is at most `(|A|·κ)²·|A|`.
   - The facing does not depend on the aim (`rotation: false`, `MatchTarget`, `Auto` with a target facing): `κ = 0`,
     the solve is exact after one pass, and the error is rounding only (`a_two_metre_anchor_with_a_fixed_facing_lands_within_the_rule_2_bound`
     measures a 2 m anchor under `1e-3` m at every cadence). Rule 2's `1e-3` m bound for anchors up to 2 m holds here.
   - The facing looks along the aim (`FaceTarget`, `Auto` without a facing): `ψ` is the yaw of `o`, `κ = 1/|o_h|`
     with `o_h` the horizontal part of the aim, and the guaranteed bound is `|A|³/|o_h|²`. That is under `1e-3` m only
     while `|o_h| >= sqrt(|A|³/1e-3)` (32 m for `|A| = 1`, 89 m for `|A| = 2`). Close to the window end it is
     conditioning, not the pass count, that decides: an anchor along the direction of travel (a hand reaching
     forward) is well conditioned in practice (the § 4 acceptance test lands a 0.8 m forward bone within 1 cm), while
     an anchor off to the side makes `ψ` chase its own aim. Rule 2's claim is therefore corrected to: under `1e-3` m
     when the facing does not depend on the aim; otherwise bounded by `|A|³/|o_h|²`. Use `MatchTarget` with a target
     facing for side anchors.

Tests: `crates/regia_animation/tests/warp_window_edges.rs` (start, end, both edges in one step, a rotating window
inside one step, a zero-length window, the end-position bound over 4 targets and 7 frame cadences, the rule 2
anchor bound) and `scripted_warps::tests::a_frame_past_the_warp_end_warps_only_its_share_inside`.

## 5. M3: Script-authored warps

Gameplay sometimes wants to bend root motion from a *graph clip* (not a montage) for a time: a dash that ends at an
enemy. Provide the same corrector without a montage window.

- `ScriptedWarps` (component on the rig): `add(ScriptedWarp { id: String, target: String, duration: f32, mode,
  translation, vertical, rotation, rotation_kind, rotation_fraction, anchor }) -> Result<(), WarpError>`;
  `remove(id)`; `pause_all()` (turns every scripted warp off without deleting; they stay listed with
  `enabled: false`); `resume_all()`; `list()`.
- A scripted warp starts at the frame it is added and runs `duration` seconds of animator time (frame deltas, so it is
  deterministic and pauses with the animator), then removes itself and emits `WarpEvent::Finished { id }`. Adding an
  id that exists replaces it (restarts). `duration <= 0` or non-finite is `WarpError::BadDuration`.
- The corrector runs on the same final root-motion value the montage stage produces (graph root motion plus
  montage root motion), after the montage stage. When both a montage window and a scripted warp are active, the montage
  window is applied first, then the scripted warp on the result (documented order; avoid overlapping windows).
- Targets live in the same per-rig map as montage targets (`MontagePlayer::set_warp_target`); a `WarpTargets`
  helper on the animator exposes `set/clear/get` so rigs without a `MontagePlayer` can use scripted warps.
- A clip warped by a scripted warp needs its root motion mode not `None`; the corrector sees zero horizontal motion
  otherwise and falls back to the linear spread (existing rule), and a warning `warp_no_root_motion` is logged once
  per scripted warp.

Acceptance: a dash clip with 3 m authored forward motion and a target 5 m away ends within 1 cm of the target when the
scripted warp finishes; `pause_all` mid-way leaves the remaining motion unwarped; a replaced id restarts with full
duration; frame-rate independence (30, 60, 144 fixed steps give the same end position within 1 mm).

## 6. M4: Variant montages

A variant reuses a parent montage and swaps selected clip segments (for example the same combo with different weapon
animations) without copying timing.

- File `<name>.montagevar.ron` (format name `montage_variant`, v1): `(format_version: 1, name: "...", parent:
  "assets/anim/combo.montage.ron", overrides: [(track: 0, segment: 2, clip: "swing_axe", rescale: true)])`.
- `MontageVariant::resolve(parent: &Montage) -> Result<Montage, VariantError>`: a deep copy of the parent with each
  override's `clip` substituted. With `rescale: true` the replacement's `rate` is set so it fills the original
  segment's duration: `rate = (new_clip_span) / (old_segment_duration)`, where durations are `(end − start) / rate ×
  loops` for the old segment and `(clip_end − start) × loops` for the new one; a zero duration is an error. With
  `rescale: false` the segment keeps its original `rate`, and the timeline length may change (sections, notifies
  and warp windows keep their times; validation reports any that now lie past the end).
- Everything else (sections, notifies, warps, blends, flags) is inherited unchanged and is **read-only** in the
  variant: the file cannot name other fields. A variant's parent may not itself be a variant (derivation stops after
  one generation): `VariantError::ParentIsVariant`. The variant's resolved name is its own `name`.
- Library and play API: `MontageLibrary::add_variant`; a variant name plays like any montage. Agents list it with
  `variant_of`.
- When the parent file changes (dev builds), variants re-resolve on the next play.

Acceptance: resolved variant equals the parent except the overridden clips and rates; rescale makes the segment's
duration equal within 1e-5 s; variant of a variant fails; override index out of range names track and segment;
editing the parent re-resolves.

## 7. M5 and M6: runtime semantics

**M5 Checked play.** `MontagePlayer::try_play(montage, PlayOptions) -> Result<MontageId, PlayError>` with errors
`UnknownSlot`, `UnknownSection(name)`, `BadRate`, `StartPastEnd`, `NoMainTrack`, never panicking. `play` stays and
keeps its behavior (it returns an id even when the montage later fails). Event contract, enforced by tests written
first (the built `stop` drops queued entries without any event, so the third rule below is a behavior change):

- `Ended { interrupted: false }` only for an instance that reached its end uncut; `BlendingOut { interrupted: false }`
  likewise.
- Any cut (stop, same-group preemption, owner despawn) emits `BlendingOut { interrupted: true }` once and later
  `Ended { interrupted: true }`.
- An instance that was *queued and dropped* (full stop, or its owner despawned before it started) emits no `Started`
  and exactly one `Ended { interrupted: true }`, so a waiting caller always gets a terminal event.
- Blend-out start with `auto_blend_out: false` never fires until stopped; `Ended` follows only after the blend
  finishes completely.

**M6 Root-motion scope.** `RootMotionRoute` (component or animator setting; default `GraphAndMontages`, the current
behavior): `Ignore` (extracted motion is discarded and the rig is not moved by animation), `Report` (computed and
exposed through `Animator::last_root_motion` but not applied), `GraphAndMontages` (both contribute, weighted;
today), `OnlyMontages` (graph clips contribute zero root motion; montage root motion applies). Per-clip
`RootMotionMode::None` still wins for that clip. The scope setting does not change the pose, only the motion
output. Document that the animation evaluation stays on the same thread as today (no scheduling change is
specified here).

## 8. M7 to M10 (second step)

**M7 Rate curve (`.montage.ron` v2).** `rate_curve: Option<RateCurve>` with `RateCurve { keys:
[(time: f32, rate: f32)], interpolation: Linear | Step }`: the effective rate at montage time `t` is `rate(t) ×
base rate × play-call rate` (all `> 0`; the curve sample is clamped to `[0.05, 20]`, the base rates multiply after the clamp). Montage time advances by integrating the rate over
the frame: for a frame of `dt` the montage advances `∫ rate`, computed exactly for `Step` and by midpoint
substeps of 1/240 s for `Linear` (at most 4096 per call; a longer hitch uses longer substeps), so frame rate does not change the end time (tolerance 1e-3 s). Notify,
section and branch times stay in *montage* time (authored positions), only the wall duration changes. `None` = rate
1. A key list must be sorted with strictly increasing times and positive rates (`montage_bad_rate_curve`).

**M8 Segment links.** `MontageNotify` and `MontageSection` gain `follow: Option<SegmentFollow { track: usize, segment:
usize, mode: KeepTime | ShiftWithStart | ScaleWithLength }>`. It only affects *editing*: when the editor or an agent edit
moves or resizes that segment, a following item moves: `KeepTime` stays at its time; `ShiftWithStart` shifts by the
segment's start change; `ScaleWithLength` keeps its fractional position within the segment (scaled by the length change).
A section kept at the start of the timeline is corrected back to 0 after edits (existing rule: the first section
starts at 0). Runtime ignores `follow`. Provide `Montage::edit_segment(track, segment, new_start, new_end) ->
Result<EditReport, MontageError>` applying the rule and returning moved items; `validate` runs after every edit.

**M9 Graph `Slot` node (`.animgraph.ron` v3).** A new node `Slot { name: String, source: NodeRef, eval_idle_source:
bool }`. At runtime it outputs its `source` pose while no montage plays in slot `name`, and while one does it
outputs the source pose blended with that slot's montage pose by the instance weight (masked as the slot's mask).
Because the node sits inside the graph, nodes downstream (IK, layers) see the montage pose, which removes the
spec 17 § 4b limitation that slots apply after the whole graph. A graph with no `Slot` node behaves exactly as
today (the post-graph stage still handles slots not named by any node). `eval_idle_source: false` skips evaluating
`source` while the slot's blend weight is 1 (cost saving); `true` evaluates it regardless (state machines under it
keep running). A slot named by a node but absent from the skeleton table is a load warning. A montage slot played
through a node must not also be applied by the post-graph stage in the same frame (no double blend): the runner
marks consumed slots. Migration: `ANIMGRAPH_FORMAT_VERSION` 3; v2 graphs load unchanged (no new required field);
`upgrade-project` bumps the number; the animation editor (`book/agents/animation-editor.md`) gets the node type.

**M10 Editor and agent edit surface.** A montage editor panel (timeline of slot tracks with segments by drag-drop of
clips, section markers with follower selection, notify and warp-window lanes, play range preview with pose preview
over the selected rig, preview one section chain or the whole timeline order). Agent methods (all on the existing
`anim.*` namespace; page additions to `book/agents/montages.md`): `anim.montage_describe { asset }` (read: tracks,
segments, sections, notifies, warps, slots used, issues), `anim.montage_edit { asset, edit }` (edit_assets; `edit` is a
tagged object: `add_segment`, `move_segment`, `remove_segment`, `set_section`, `link_sections`, `add_notify`,
`set_blend`, `add_warp`; atomic, validated, undoable), `anim.slots { skeleton }` (read), `anim.slot_edit { skeleton,
op }` (edit_assets; rename/delete per § 3), `anim.warp_state { entity }` (read: active windows, scripted warps, targets,
remaining distance). Errors (lowercase): `montage_unknown_asset`, `montage_bad_edit`, `montage_invalid`,
`slot_in_use`, `slot_unknown`, `warp_unknown_target`.

## 9. Replication hook (not implemented here)

Every runtime change a gameplay system makes to a player goes through a small command value
(`MontageCommand::{Play, Stop, JumpToSection, SetNextSection, SetRate, SetWarpTarget}`, serializable) that the
existing methods construct and apply, and that a future `regia_net` layer can send. Commands carry the animator time
at which they apply. Applying a command stream to a fresh player reproduces the same instances and events (tested
now with a recorded stream, so replication later is a transport problem). Root-motion correction on a client is the
networking crate's job.

## 10. Determinism, performance, platforms

- All new math is `f32`, fixed iteration counts, no random numbers; two runs of the same inputs are bit-identical on
  a given platform. Cross-platform results may differ in the last bit (same as the rest of the animation crate).
- Budget: a rig with 3 playing montages, 2 warp windows and 1 scripted warp adds ≤ 20 µs per frame over today's montage
  stage on a desktop core; `Bone` anchor sampling happens once per (montage, window) and is cached. `tracing` spans
  already exist for the montage stage; add `scripted_warps`.
- Linux, Windows, Android aarch64: pure Rust in `regia_animation`; the panel is desktop-only.

## 11. Tests and acceptance (summary)

Per item as written in §§ 3-8, plus: v1 `.montage.ron` fixtures from the repo migrate and play with unchanged
output; `check-formats` and `upgrade-project` cover `slots`, `montage` v2, `montage_variant` and `animgraph` v3 (the
lead runs `cargo xtask check-formats --update` and `cargo xtask manual`); `agent-coverage` and `book-check` pass for
the new methods; all codes lowercase and documented.

## 12. Out of scope

Reverse playback (negative rate; v1 stays `≥ 0`), network replication and root-motion correction (§ 9), drag-and-drop
authoring parity with any particular editor, linked-layer montage sharing across graph instances, a character
movement component (the existing movement code consumes `last_root_motion`), vertical root-motion yielding to gravity
(a character-controller decision in `regia_framework`, not an animation one).

## 13. Provenance

Public user documentation of montage authoring, slots and groups, section chains, notifies and windows, motion
warping and root-motion extraction (behavior only); algorithms: motion warping (Witkin and Popović 1995), motion path
editing (Gleicher 2001), already cited in ADR-1465. Set area: `runtime-animation`. Row in
`docs/provenance.md` (ADR-1577).

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
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 14. Decisions and owner questions

- M5: today dropped queued montages are silent; the spec makes them emit one `Ended { interrupted: true }` (new event
  for hosts that count events; list under Upgrade). Decision (ADR-1585).
- M6: `RootMotionRoute` is separate from the per-clip `RootMotionMode` import setting; a clip with `None` stays at zero
  in every route (decided; no overlap with `systems.rs` use of `RootMotionMode`).
- M2: the two-pass anchor solve is accepted (error bound stated in rule 2); an exact solve is not required.
- Owner decisions: none.

## 15. Built status (ADR-5633, 2026-11-10)

Where the code differs from the text above, the code is right, and each difference has an ADR:

- **M1:** there is no `MontagePlayer::from_skeleton_slots`. A player for a table is
  `MontagePlayer::new(table.slot_defs(), clips)`, and it does not merge rig-side definitions by name (ADR-1644).
  `SkeletonSlots::validate(&Skeleton)` is named `validate_against`.
- **M3:** `ScriptedWarps` keeps its own target map. There is no `WarpTargets` helper on the animator, and
  `MontagePlayer::set_warp_target` does not reach scripted warps (ADR-1644). A scripted warp's `Bone` anchor reads the
  current evaluated pose. `ScriptedWarps::add` also refuses `BadName` and `BadFraction`.
- **M4:** the format name is `montage-variant`. `MontageLibrary::add_variant` takes a clip-span lookup.
  `rescale` is `new span × loops / old duration` (ADR-1644). A variant re-resolves when its parent is replaced in the
  library. A variant file that names any field other than its own is a load error (ADR-5633).
- **M8:** `edit_segment(track, segment, new_start, new_end)` takes the segment's clip-local trim points
  (`new_end: None` = the clip's end) and returns `MontageEditError` (ADR-1870).
- **M9:** the `Slot` node's `source` is a boxed sub-tree, not a `NodeRef` (ADR-1870).
- **M10:** the slot methods are `anim.montage_slots` and `anim.montage_slot_edit` (ADR-1870). `variant_of` is reported
  by `anim.montage_describe`.

Not built yet (v0.5.x; § 16 below built all but § 10, ADR-7216):

- § 9: the `MontageCommand` replication hook and its recorded-stream test.
- M1: the unknown-slot warning at load and the error in the cook step. `Montage::validate_against` and
  `anim.montage_describe` report the issue today.
- M4: with `rescale: false`, nothing reports items that now lie past the end.
- M5: a despawn does not call `cancel_all` by itself, so montages are not cut automatically when their owner is
  despawned. Call `cancel_all` instead.
- § 10: there is no measured 20 µs budget.

Tests for the § 3 and § 4 acceptance items that other files do not prove as written are in
`crates/regia_animation/tests/montage_spec34_conformance.rs`.

## 16. As built: the deferred items (ADR-7216, sprint post05-anim-abilities)

- **§ 9 replication hook: built.** `regia_animation::montage_command::MontageCommand` (`Play { montage, options,
  queued }`, `Stop`, `CancelAll`, `JumpToSection`, `SetNextSection`, `OnBranch`, `SetRate`, `SetReverse`,
  `StretchTo`, `SetWarpTarget`, `ClearWarpTarget`, `ShiftWarpTargets`; serde) and `TimedMontageCommand { at,
  command }`. Refined: the player methods *record* their command (when `set_recording(true)`) rather than being
  rewritten as build-then-apply; `MontagePlayer::apply_command` runs a command through the same method, so the two
  paths cannot drift. "Animator time" is the player's command clock: seconds of animation time the evaluate system
  has advanced it by, every evaluation, playing or not. Received commands (`ApplyMontageCommand` messages) wait in
  the player's inbox until the clock reaches `at` and apply before that frame's evaluation; recorded ones go out as
  `MontageCommandRecorded` messages after it. `Play` carries the montage *name*: the receiver must have it in its
  library (`montage_command_unknown_montage` otherwise). `regia_animation` has no edge to `regia_net`: the transport
  (postcard bytes of `TimedMontageCommand`) belongs to the umbrella crate's net wiring. Test:
  `tests/montage_post05.rs` `a_recorded_montage_play_replays_on_a_second_app` (same events on the same frames and
  the same instance snapshots every frame on a second `App`, the stream sent as postcard bytes).
- **M1 load warning and cook error: built.** `MontagePlayer::add_montage` logs `montage_unknown_slot` and keeps it in
  `load_issues()` (a player with no slot table reports nothing). The cook check is
  `skeleton_slots::cook_slot_errors(&Montage, &[SkeletonSlots])`, run by `regia_asset_pipeline::montage_cook` in
  `cook_project`: refined, because a montage file names no skeleton (no `montage → skeleton` edge exists), the
  check is against the union of the project's `.slots.ron` tables, and a project with none is not checked (slots in
  code or the engine default). Montages still ship raw.
- **M4 past-end report: built.** `Montage::timeline_length(clip_span)` and `Montage::items_past_end(end)` (sections,
  notifies, warp and root-motion windows, code `montage_past_end`, slack 1e-5 s); `MontageLibrary::add_variant`
  logs them and keeps them in `MontageLibrary::past_end(name)`; `anim.montage_describe` of a variant lists them.
- **M5 owner despawn: built.** An `on_despawn` hook on `MontagePlayer` runs `cancel_all` (not recorded as a
  command: the despawn itself replicates) and writes the events as `MontageEvent` messages at once.
- **§ 10 frame budget:** still not measured (unchanged).

## 17. As built: the network transport (ADR-9370, sprint post05w3-montage-net-abilities-cook)

- **§ 9 transport: built.** `regia::montage_net::MontageNetPlugin` (umbrella crate, feature `net`, opt-in) sends a
  networked rig's recorded commands from the server as a reliable broadcast `MontageCommandMsg { tick, seq, bytes }`
  (postcard of `TimedMontageCommand`, at most 4096 bytes) and applies them on clients in a `FixedUpdate` system (after
  `NetSet::Gameplay`, before `NetSet::Predict`) in the tick the net layer delivers them, which is the stamped tick.
  Refined: the receiver applies through `MontagePlayer::apply_command` at once instead of `ApplyMontageCommand` (whose
  `at` is the sender's player clock, which does not line up across machines). A message whose `(tick, seq)` is not
  newer than the last applied is dropped (`MontageNetState`, `MontagePlayer::net_state`). Commands made in
  `FixedUpdate` play on clients in the same tick; commands made in `Update` go out with the next tick and play one tick
  later. `anim.montage_commands` reports `net { sent, received, dropped, last_applied }`. Test:
  `crates/regia/tests/montage_net.rs` (two apps over a hand-moved loopback; same statuses and events every tick; a
  duplicate and a late resend change nothing).


## 18. As built: panel authoring parity (sprint w9-anim-authoring)

- The Montage panel (`crates/regia_editor/src/montage_panel.rs`) now authors everything `anim.montage_edit` and
  `anim.montage_slot_edit` can: a warp form (`add_warp`), a blend form (`set_blend`, changed fields only), section
  links (**Jump to** / **Continue** / **Stop at end**, `link_sections`), follow links on a selected section or notify
  (`set_follow`, set and clear), a slot field that adds a segment on a second slot track (`add_segment` with a slot no
  track plays), and a slot-table editor (`anim.montage_slots` to load; add, rename and delete through
  `anim.montage_slot_edit`). Each control builds the agent's edit object and calls the same function, so the bytes
  and the undo stack are the agent's. No format change. Tests: `montage_panel::authoring_tests` (each operation
  clicked in a headless panel, compared byte for byte with the agent call, one undo step, Undo restores the bytes; the
  slot table round-trips add + rename + delete to the original bytes and refuses a slot in use with `slot_in_use`).
