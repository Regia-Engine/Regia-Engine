# Montages, sync groups and motion warping

A **montage** is a one-shot action (an attack, a reload, a vault, an emote) that gameplay starts on a character and
that plays over the character's anim graph. **Sync groups** keep locomotion cycles of different lengths in step while
they blend. **Motion warping** bends a montage's root motion at runtime so the character lands where the game needs
it (on the enemy it lunges at, on the ledge it vaults onto). Spec: `docs/specs/17-animation.md` § 4b.

## The montage asset (`.montage.ron`)

```ron
(
    format_version: 1,
    name: "attack",
    tracks: [
        (slot: "full_body", segments: [(clip: "Windup"), (clip: "Swing", rate: 1.2), (clip: "Recover", end: Some(0.6))]),
    ],
    sections: [
        (name: "windup", start: 0.0),
        (name: "swing", start: 0.5, next: Jump("recover")),
        (name: "combo", start: 1.1, next: Stop),
        (name: "recover", start: 1.6, next: Stop),
    ],
    notifies: [
        (name: "combo_input", time: 0.9, branch: true),
        (name: "hitbox", time: 0.6, duration: 0.25, payload: Some("(damage: 20)")),
        (name: "swoosh", time: 0.55),
    ],
    blend_in: (time: 0.15, curve: Smooth),
    blend_out: (time: 0.25, curve: Smooth),
    warps: [(target: "enemy", start: 0.2, end: 0.6, rotation: true)],
)
```

- **Tracks**: one per slot. A track's segments play back to back from time 0 (`start`/`end` trim the clip, `rate`
  speeds it up, `loops` repeats it). Track 0 is the main track: it gives the root motion and decides the slot group.
- **Sections** split the timeline. `next` is `Continue` (default: play on into the next section), `Stop` (the montage
  ends at this section's end) or `Jump("name")`; a section that jumps to itself loops. The first section starts at 0;
  without sections the montage has one section, `main`.
- **Notifies**: `duration: 0` fires once; `duration > 0` is a window with a begin and an end event. `branch: true` is a
  branching point: playback stops exactly at that time inside the frame, so a decision made in advance applies at the
  right moment whatever the frame rate.
- `blend_in`, `blend_out`, `blend_out_lead` (seconds before the end the blend-out starts; default the blend-out time),
  `auto_blend_out: false` (hold the last pose until stopped), `root_motion: false` (the montage does not move the
  character), `root_motion_off: [(start, end)]` (windows where gameplay steers instead), `warps` (below),
  `sync_group: Some("gait")` (while the montage blends out, its clip leads that sync group, so the locomotion the
  character returns to is already in step).

`Montage::from_ron_str` loads and validates a file; `regia upgrade-project` knows the format.

## Slots

Slots are the named places montages play into. Define them on the rig's `MontagePlayer`:

```rust,wrap
use regia_animation::prelude::*;
# use regia_ecs::prelude::World;
# let mut world = World::new();
# let rig = world.spawn_empty().id();
let player = MontagePlayer::new(
    vec![
        SlotDef::full_body("full_body", "body"),
        SlotDef::masked("upper_body", "upper", &["spine_01"]), // spine_01 and its children
    ],
    ClipLibrary::default(), // the clips montages use (the animator's library is searched after it)
);
world.entity_mut(rig).insert(player);
```

A montage track blends over the graph's pose through its slot's mask, by the montage's blend weight. An upper-body
reload therefore leaves the legs to the locomotion graph, and adds no root motion (the mask does not cover the
skeleton root). Playing a montage whose main slot is in the same **group** as a playing one interrupts it: the old
one blends out over the new one's blend-in time.

## Playing from game code

```rust,wrap
# use regia_animation::*;
# use regia_animation::montage::SectionNext;
# use regia_animation::montage_player::BranchAction;
# let mut player: MontagePlayer = todo!();
# let (attack, recover): (std::sync::Arc<Montage>, std::sync::Arc<Montage>) = todo!();
# let enemy_position = regia_core::math::Vec3::ZERO;
let id = player.play(attack.clone(), PlayOptions { rate: 1.0, ..PlayOptions::default() });
player.on_branch(Some(id), "combo_input", BranchAction::JumpTo("combo".into())); // the player pressed attack again
player.set_warp_target("enemy", WarpTarget::at(enemy_position));
player.queue(recover.clone(), PlayOptions::default());    // after this one
player.jump_to_section(None, "recover");
player.set_next_section(None, "swing", SectionNext::Stop);
player.stop(None, Some(0.1));
```

Read `MontageEvent` messages for what happened: `Started`, `Section { from, to }`, `Notify { name, payload }`,
`WindowBegin` / `WindowEnd`, `BranchPoint`, `BlendingOut { interrupted }` and `Ended { interrupted }`
(`interrupted: false` means it completed). Clip notifies inside the montage's clips still arrive as `AnimNotify`
while the montage dominates the pose (weight at least one half). `player.instances()` shows section, time, weight and
phase of every playing montage.

## Motion warping

A warp window names a target. While montage time is inside the window and the target is set
(`set_warp_target`), every frame's root motion is corrected so the character's origin reaches the target at the
window's end: the remaining motion of the window is turned and scaled onto the remaining distance, so the move
keeps its shape (a lunge stays a lunge, only longer or angled). `rotation: true` also turns the character to the
target's `facing`, or toward the target. Turning toward the target assumes the rig faces −Z in its object space;
a rig authored facing another axis should pass `facing`. `vertical: true` adjusts the height too (for a vault). The target can move
during the window; the rest of the window re-aims. The character must be root-motion driven
(`regia_framework::character::RootMotionDriven`) for the warped motion to move it.

Only montage time inside the window is warped. A frame that crosses the window's start or end is split there: the
authored motion before the start and after the end plays as authored, so the character lands on the target exactly
at the window end even when no frame ends there, then carries on with the montage's own motion (spec 34 § 4a). A
scripted warp splits the frame it ends in the same way.

## Sync groups (`.sync.ron`)

```ron
(
    format_version: 1,
    groups: [(name: "gait", roles: [("StartWalk", AlwaysLead)])],
    markers: [
        (clip: "Walk", markers: [(name: "left", time: 0.0), (name: "right", time: 0.52)]),
        (clip: "Run", markers: [(name: "left", time: 0.05), (name: "right", time: 0.33)]),
    ],
)
```

Load it with `SyncSetup::from_ron_str` and set it on the animator with
`animator.set_sync_setup(Some(Arc::new(setup)))`. Every graph `Clip` node whose `sync_group` names a declared group
joins it. The heaviest clip leads (`AlwaysLead` clips lead whenever they play, `NeverLead` clips never do); the
others follow its clock and stay quiet (no notifies). With at least two marker names shared by both clips, a follower
matches the leader's position between markers (a 4-step run lines its foot plants up with a 2-step walk); otherwise
it matches the leader's normalised phase. Groups that are not declared keep the old behaviour, so existing graphs are
unchanged. A synced blend space in a declared group joins it as one member (its weighted cycle, by normalised phase),
so an idle clip and a walk/run space cross-fade in step.

## Reverse, time-stretch and editing (ADR-1870)

`PlayOptions { reverse: true, .. }` plays a montage backward, from its end (or the end of `section`, minus
`start_time`) to 0; `MontagePlayer::set_reverse(id, on)` turns a playing montage around. Sections are crossed
backward (their forward links are not taken), notifies fire when crossed, and the rig moves back along the
authored root motion. Warp windows are off in reverse. `PlayOptions { duration: Some(0.8), .. }` sets the play rate
so the montage (rate curve included) takes 0.8 s; `MontagePlayer::stretch_to(id, seconds)` does the same for the
rest of a playing montage.

`Montage::edit_segment(track, segment, start, end)` trims a segment and moves the notifies and sections linked to it
with `follow` (`KeepTime`, `ShiftWithStart`, `ScaleWithLength`); `Montage::apply_edit(&MontageEdit, lengths)` covers
the other edits (add, reorder or remove segments, sections, links, notifies, blends, warp windows). Edits are
atomic: an edit that would leave the montage invalid changes nothing.

## Montages inside the anim graph

A `Slot { name, source, eval_idle_source }` node (`.animgraph.ron` v3) plays slot `name`'s montages at that point of
the graph: `source` plays while nothing is in the slot, and the montage pose is blended over it while something is.
Nodes above the slot node (IK, layers, look-at) then work on the montage pose, so a two-bone IK on top keeps a hand
on a ledge during a vault. Slots no node names keep blending after the whole graph, as before.

The `anim_lab` sample plays one end to end (`samples/anim_lab/src/slot_graph.rs`). Its graph holds a `Grip` layer over
a Slot node, so the hand keeps its grip while a wave montage moves the arm:

```ron
(
    format_version: 3,
    parameters: [("Grip", Float(1.0))],
    root: Layer((
        base: Slot((
            name: "upper",
            source: Clip((clip: "Idle", looping: true, speed: 1.0, sync_group: None)),
        )),
        layers: [(
            graph: Clip((clip: "Grip", looping: true, speed: 1.0, sync_group: None)),
            mask: (joints: ["hand"], include_children: true),
            mode: Override,
            weight_param: "Grip",
        )],
    )),
)
```

The same wave then plays backward in half its length (`PlayOptions { reverse: true, duration: Some(0.5), .. }`) and as
a child montage (`assets/wave_short.montagevar.ron`, which swaps the clip and keeps the timing). Run its checks with
`cargo test -p anim_lab`; `cargo run -p anim_lab` prints the `slot_graph:` line with the measured heights.

## Limits

- Montages play forward or in reverse with `rate >= 0`; a negative rate is refused.
- Motion warping is computed along the timeline as authored: a section jump during a warp window skips warping for
  that frame.
- Blend spaces join named groups by normalised phase only (no marker sync for spaces).
- The editor's Montage timeline panel (spec 34 M10, `book/agents/editor-data-panels.md`) edits montages; you can
  also author the RON by hand, through `Montage::apply_edit`, or through the agent protocol (`anim.montage_edit`,
  `book/agents/montages.md`).
