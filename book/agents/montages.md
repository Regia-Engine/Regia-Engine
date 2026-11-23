# Montages, sync groups and motion warping: drive and verify

User guide: `book/user-guide/montages.md`. Spec: `docs/specs/17-animation.md` § 4b (ADR-1465). These calls need
`regia_animation::AnimationPlugin` and a rig with an `Animator` (entity ids come from `anim.list`). Montage clips
resolve against the rig's `MontagePlayer` clip library first, then the animator's graph library.

## Methods

| Method | What it does |
|---|---|
| `anim.montage_state` | Slots, montage library, playing instances, queue, warp targets and the last 64 events. |
| `anim.montage_play` | Play (or queue) a library montage by name or an inline `.montage.ron`; adds a `MontagePlayer` if needed. |
| `anim.montage_stop` | Blend out one montage (`id`) or all of them, reported as interrupted. |
| `anim.montage_jump` | Jump to a section now, or with `next` relink where a section goes at its end. |
| `anim.montage_set_warp_target` | Set, move or clear (`position: null`) a motion-warping target. |
| `anim.sync_state` | Declared sync groups and markers, and each group's current leader. |
| `anim.warp_state` | Warp targets (montage and scripted), scripted warps with progress, and the root-motion route. |
| `anim.scripted_warp` | Add, remove, pause or resume scripted warps and set their targets; no montage needed. |
| `anim.root_motion_route` | Read or set where extracted root motion goes. |
| `anim.montage_describe` | Read a `.montage.ron` (or a child `.montagevar.ron`): timeline positions, links, slots used, issues. |
| `anim.montage_edit` | One atomic, validated, undoable edit of a montage file; linked notifies and sections move. |
| `anim.montage_slots` | A skeleton's slot table (`.slots.ron`) and the montages that play each slot. |
| `anim.montage_slot_edit` | Add, change, rename (rewrites every referencing montage) or delete a slot. |
| `anim.montage_control` | Turn a playing montage around (`reverse`) or time-stretch the rest of it (`duration`). |
| `anim.montage_commands` | Turn command recording on or off and read the player's command clock, waiting commands and last recorded commands (spec 34 § 9). |

## Play and watch

```json
{"method": "anim.montage_play", "params": {"entity": 4294967310, "ron":
  "(format_version: 1, name: \"attack\", tracks: [(slot: \"full_body\", segments: [(clip: \"Swing\")])], sections: [(name: \"windup\", start: 0.0), (name: \"strike\", start: 0.4, next: Stop)], notifies: [(name: \"combo\", time: 0.3, branch: true), (name: \"hitbox\", time: 0.45, duration: 0.2)], warps: [(target: \"enemy\", start: 0.1, end: 0.45)])"}}
→ { "entity": 4294967310, "id": 1, "montage": "attack", "queued": false, "created_player": true }
```

`ron` adds (or replaces) the montage in the rig's library, so later calls can pass `"montage": "attack"`. `rate`
is 0..=100 (default 1; a negative rate is `bad_rate`, and play backward is `reverse: true`). `section`, `start_time` and `queue: true` (start when the slot group's montage begins its natural blend-out)
are optional. Then `play.step` and read the state:

```json
{"method": "anim.montage_state", "params": {"entity": 4294967310}}
→ { "player": true, "slots": [ ... ], "library": [ { "name": "attack", "sections": [ ... ], "warps": [ ... ] } ],
    "instances": [ { "id": 1, "montage": "attack", "section": "strike", "time": 0.52, "length": 1.1,
                     "weight": 1.0, "rate": 1.0, "phase": "playing", "interrupted": false } ],
    "queued": [], "warp_targets": [ ... ],
    "events": [ { "id": 1, "kind": "started" }, { "kind": "branch_point", "name": "combo" },
                { "kind": "section", "from": "windup", "to": "strike" },
                { "kind": "window_begin", "name": "hitbox", "payload": null } ] }
```

`phase` is `blending_in`, `playing`, `holding` (reached the end with `auto_blend_out: false`) or `blending_out`.
Event kinds: `started`, `section` (`from`, `to`), `notify` (`name`, `payload`), `window_begin`, `window_end`,
`branch_point`, `blending_out` (`interrupted`), `ended` (`interrupted`; `false` means the montage completed).
Events are in timeline order within a frame; the same inputs and frame times give the same list.

## Sections, branches, stop

```json
{"method": "anim.montage_jump", "params": {"entity": 4294967310, "section": "strike"}}
{"method": "anim.montage_jump", "params": {"entity": 4294967310, "section": "loop", "next": "finish"}}
{"method": "anim.montage_stop", "params": {"entity": 4294967310, "blend": 0.1}}
```

`next` is `"continue"`, `"stop"` or a section name (a section that names itself loops). With
`"branch": "combo"` the jump to `section` is armed instead: it happens exactly when playback reaches that
branching point (once), whatever the frame rate, like `MontagePlayer::on_branch` in game code. The answer then has
`"armed": "combo"`.

```json
{"method": "anim.montage_jump", "params": {"entity": 4294967310, "section": "strike", "branch": "combo"}}
```

## Motion warping

```json
{"method": "anim.montage_set_warp_target", "params": {"entity": 4294967310, "name": "enemy",
  "position": [1.5, 0.0, -2.5], "facing_yaw": 0.6}}
→ { "name": "enemy", "before": null, "after": [1.5, 0.0, -2.5] }
```

While the montage's time is inside a warp window for `enemy`, its root motion is rotated and scaled so the rig's
origin reaches `position` at the window end (`facing_yaw` or, without it, the direction to the target, when the
window has `rotation: true`). Moving the target re-aims the rest of the window. `position: null` clears it.
A frame that crosses a window edge is split at the edge (spec 34 § 4a): only its part inside the window is warped,
so the rig is on the target at the window end and then moves by the montage's authored motion after the end.

Warp windows (montage format v2, spec 34) can also set `mode` (`PathScale` rescales the path without turning it),
`rotation_kind` (`Auto`, `MatchTarget`, `FaceTarget`), `rotation_fraction` (the turn finishes after that fraction of
the window and holds) and `anchor` (`Root`, `Bone("hand")`, `Offset((x, y, z))`): with a bone anchor the *bone*, not
the rig origin, lands on the target. `anim.montage_state` lists these fields per window. A bone the skeleton lacks logs
`montage_unknown_anchor_bone` and falls back to the root.

Verify: place the rig, set the target, play, `play.step` until `anim.montage_state` shows the instance's `time` past
the window end, then read the rig's `Transform` (`scene.get_entity` with `"components": ["Transform"]`) and check it is within a few centimetres of
the target; take `seeing.screenshot` before and after to see the lunge land.

## Sync groups

```json
{"method": "anim.sync_state", "params": {"entity": 4294967310}}
→ { "setup": { "groups": ["gait"], "marked_clips": [ { "clip": "Walk", "markers": [ { "name": "left", "time": 0.0 } ] } ] },
    "groups": [ { "name": "gait", "leader": { "clip": "Run", "time": 0.21, "speed": 1.0, "looping": true } } ] }
```

`setup` is `null` when the animator has no `SyncSetup` (clips keep their own clocks). Verify phase alignment by
reading the clip times in `anim.get_state`'s debug nodes: followers sit at the leader's marker position (or
normalised phase).

## Scripted warps and the root-motion route

A scripted warp bends the root motion of a graph clip (a dash, a lunge) toward a target for `duration` seconds of
animator time, then removes itself. It runs after the montage stage, on the final root motion, and needs no montage:

```json
{"method": "anim.scripted_warp", "params": {"entity": 4294967310, "op": "set_target", "name": "enemy",
  "position": [0.0, 0.0, -5.0]}}
{"method": "anim.scripted_warp", "params": {"entity": 4294967310, "op": "add", "id": "dash", "target": "enemy",
  "duration": 0.5, "rotation": true}}
{"method": "anim.warp_state", "params": {"entity": 4294967310}}
```

`add` with an existing `id` restarts it with the full duration. `pause_all` stops every warp (their time stops too)
and `resume_all` releases them. The clip needs root motion (`RootMotionMode` other than `None`); without it the
offset is spread linearly and `warp_no_root_motion` is logged once. A finished warp writes a `ScriptedWarpEvent`
message. `anim.root_motion_route { entity, route }` chooses where extracted motion goes: `ignore` (discarded),
`report` (shown by the animator, not applied), `graph_and_montages` (the default) or `only_montages`.

## Author montages (spec 34 M8, M10; ADR-1870)

Paths are project-relative (`anim/combo.montage.ron`; no `..`, not absolute). Files are written through a temp file
and a rename, so a reader never sees half a file.

```json
{"method": "anim.montage_describe", "params": {"asset": "anim/combo.montage.ron", "slots": "chars/hero.slots.ron"}}
→ { "asset": "anim/combo.montage.ron", "kind": "montage", "name": "combo", "length": 2.0,
    "tracks": [{ "index": 0, "slot": "full", "segments": [{ "index": 0, "clip": "a", "start": 0.0, "end": 1.0,
      "rate": 1.0, "loops": 1, "clip_length": null, "at": 0.0, "length": 1.0 }, ...] }],
    "sections": [{ "name": "swing", "start": 1.0, "next": "continue",
      "follow": { "track": 0, "segment": 1, "mode": "shift_with_start" } }], "issues": [], ... }
```

Segments play back to back from time 0, so a segment has no free timeline position: `at` is the sum of the
lengths before it (`(end - start) / rate * loops`; a segment that plays to the end of a clip the running app has
not loaded has `length: null`, and an issue `montage_clip_length_unknown`). A child montage (`.montagevar.ron`)
is described resolved against its parent, with `variant_of` and `overrides`.

```json
{"method": "anim.montage_edit", "params": {"asset": "anim/combo.montage.ron",
  "edit": {"op": "move_segment", "track": 0, "segment": 0, "end": 0.5}}}
→ { "saved": true, "undo_depth": 1, "moved": [{ "kind": "section", "index": 1, "name": "swing", "from": 1.0, "to": 0.5 }],
    "unlinked": [], "montage": { ... } }
```

An edit runs on a copy, moves the items linked to a changed segment, and is refused with nothing changed when the
result does not validate. Follow modes (`follow: { track, segment, mode }`): `keep_time` stays; `shift_with_start`
moves with the segment's timeline start (trimming a segment moves the segments after it, never the segment
itself); `scale_with_length` keeps its fractional position inside the segment. A removed segment's followers are
unlinked and keep their time. The first section always starts at 0.

| `op` | Fields |
|---|---|
| `add_segment` | `slot` (a new track when no track plays it), `clip`, `index?`, `start?`, `end?`, `rate?`, `loops?` |
| `move_segment` | `track`, `segment`, then any of `start`, `end`, `to_clip_end: true`, `to` (new position on the track), `rate`, `loops` |
| `remove_segment` | `track`, `segment` (a track left empty is removed; the main track keeps one) |
| `set_section` | `name`, `start?` (required for a new section), `next?` (`"continue"`, `"stop"`, `{ "jump": name }`), `follow?` |
| `remove_section` | `name` |
| `link_sections` | `from`, `to?` (a section) or `stop: true`; neither = continue |
| `add_notify` | `name`, `time`, `duration?`, `payload?`, `branch?`, `follow?` |
| `remove_notify` | `name` (every notify of that name) |
| `set_follow` | `kind: "section" \| "notify"`, `name`, `follow` (null clears) |
| `set_blend` | `blend_in?`, `blend_out?`, `blend_out_lead?`, `auto_blend_out?` |
| `add_warp` | `target`, `start`, `end`, `translation?`, `vertical?`, `rotation?`, `mode?` (`similarity`, `path_scale`), `rotation_kind?` (`auto`, `match_target`, `face_target`), `rotation_fraction?`, `anchor_bone?` or `anchor_offset?` |
| `remove_warp` | `index` |
| `undo` | none: restores the text before the last edit of this asset (16 steps) |
| `set_override`, `remove_override` | child montages only: `track`, `segment`, `clip`, `rescale?` |

`dry_run: true` validates and reports without writing. Clip lengths come from the clip libraries of the running
app's montage players and animators.

The editor's Montage panel makes every one of these edits and the slot-table ops below from its controls (warp
form, blend form, follow and section links, a slot field that starts a second track, the slot table), through these
same functions: a click writes the bytes this method writes and is one step of its undo stack
([editor gameplay data panels](editor-data-panels.md)). The warp form has every spec 34 M2 field (`mode`,
`rotation_kind`, `rotation_fraction`, and a bone or offset anchor); slot-table edits are steps of the panel's undo
journal (`editor.montage_panel_slot_edit`), so the panel's Undo takes them back too. `editor.create_asset { kind:
"montage", path }` (Content ▸ New ▸ Montage) writes a montage named after the file: one track on the default slot
playing a clip named `idle` to its end, which loads and validates as is.

## Slot tables

```json
{"method": "anim.montage_slots", "params": {"path": "chars/hero.slots.ron"}}
→ { "path": "chars/hero.slots.ron", "default": false, "slots": [{ "name": "upper", "group": "upper",
    "mask": { "joints": ["spine_02"], "include_children": true } }], "groups": ["upper"],
    "used_by": { "upper": ["anim/wave.montage.ron"] }, "used_by_graphs": { "upper": ["anim/hero.animgraph.ron"] },
    "graph_issues": [{ "code": "animgraph_unknown_slot", "graph": "anim/old.animgraph.ron", "slot": "tail" }] }
{"method": "anim.montage_slot_edit", "params": {"path": "chars/hero.slots.ron",
  "op": {"op": "rename", "from": "upper", "to": "upper_body"}}}
→ { "op": "rename", "slots": [...], "rewritten": ["anim/wave.montage.ron"], "unparsed": [] }
```

A missing table file is the engine default (one full-body slot `default`, `"default": true`); the first edit
creates the file. `op.op` is `add { name, group?, joints?, include_children? }`, `set { name, group?, joints?,
full_body? }`, `rename { from, to }` or `delete { name }`. Referencing montages and graph `Slot` nodes are found by
scanning the project's `.montage.ron` and `.animgraph.ron` files (pass `montages: [paths]` and `graphs: [paths]` to
limit the scan, for example when two skeletons share a slot name). Rename rewrites every referencing file and the
table, all or none (every temp file is written before the first rename, and a rename that fails puts back the files
already replaced — bytes and file permissions); a read-only target — the table itself, or any referencing
montage or graph a rewrite would touch — refuses the whole edit on every host (`montage_read_only`, for all four
ops; clearing the read-only attribute is the way forward). A rename also drops the undo steps of
the montages it rewrote, since their older texts name the old slot. Delete is refused while a montage or a graph
node names the slot (`slot_in_use`, listing them) unless `force: true`. Rename and delete list in `unparsed` the
files that may name the slot but do not parse (they are left alone: fix them and rename there by hand, or a stale
slot name stays in them). `graph_issues` lists graph `Slot`
nodes naming a slot the table lacks; at run time such a node logs one `animgraph_unknown_slot` warning per rig —
once per content combination, so a swapped graph, an in-place graph edit and a changed rig slot list are each
warned about again.

## Reverse and time-stretch

```json
{"method": "anim.montage_play", "params": {"entity": 4294967310, "montage": "attack", "reverse": true, "duration": 0.6}}
{"method": "anim.montage_control", "params": {"entity": 4294967310, "reverse": false, "duration": 1.5}}
→ { "entity": 4294967310, "instance": { "id": 1, "montage": "attack", "time": 0.21, "rate": 1.4, "reverse": false } }
```

`reverse: true` plays from the end (of `section`, or of the montage; `start_time` counts back from it) toward 0.
Sections are crossed backward in timeline order (a `section` event each); section links are forward links and are
not taken; notifies and branching points fire when crossed; a notify window begins at its end edge. Root motion of
the on-screen player runs backward along the authored path. Warp windows and sync-group leadership are off while a
montage plays in reverse. Predicted root motion does not. `MontagePlayback` has no reverse flag: a rate that is not
positive and finite stays at the start and contributes no root motion, and one step is not capped at the player's
64 section crossings. `NetMontagePlay` replicates `montage`, `section`, `start_tick` and `rate` only, and the step
builds playback from that rate.
`duration` sets the play rate so the timeline from the start point (or, for `anim.montage_control`, from the current
time) to the end takes that many wall seconds, rate curve included; it replaces `rate`. Instances report `reverse`
in `anim.montage_state`.

## Montages inside the anim graph

An anim graph (`.animgraph.ron` v3) can play a slot's montages inside the graph with a `Slot { name, source,
eval_idle_source }` node (`book/agents/animation-editor.md`, kind `slot`): nodes above it (IK, layers) see the
montage pose, and the montage stage after the graph does not blend that slot again. Slots no node names still blend
after the graph.

## Error codes


Errors carry the protocol code (`F0002` bad parameters, `F0003` not found) and a stable `data.error_code`:

| `error_code` | Meaning |
|---|---|
| `entity_missing` | The entity does not exist. |
| `no_animator` | The entity has no `Animator`. |
| `no_montage_player` | Stop, jump or warp on a rig without a `MontagePlayer`. |
| `montage_required` | Neither `montage` nor `ron` was given. |
| `montage_unknown` | No library montage of that name. |
| `montage_invalid` | The `ron` text (or a file) does not parse or validate, or an edit would leave it invalid (the message says why). |
| `montage_clip_missing` | A segment's clip is in neither library. |
| `montage_not_playing` | The `id` is not playing (or queued). |
| `no_montage_playing` | Jump with nothing playing. |
| `section_unknown` | No such section (or a relink names an unknown section). |
| `section_required` | `anim.montage_jump` without `section`. |
| `branch_unknown` | No branching point of that name (or no such section to jump to). |
| `branch_with_next` | `branch` and `next` together. |
| `name_required` | `anim.montage_set_warp_target` without `name`. |
| `position_required` | `anim.montage_set_warp_target` without `position`. |
| `warp_target_invalid` | `position` is not three finite numbers, or `facing_yaw` comes with `position: null`. |
| `bad_rate` | `rate` is negative, or `duration` is not a positive number of seconds. |
| `warp_bad_duration` | `anim.scripted_warp` add: `duration` missing, zero, negative or not finite. |
| `warp_bad_name` | An empty or missing `id`, `target` or `name`. |
| `warp_bad_fraction` | `rotation_fraction` is outside `(0, 1]`. |
| `warp_bad_mode` | `mode` or `rotation_kind` is not one of the listed names. |
| `warp_unknown_op` | `op` is missing or not one of the listed operations. |
| `warp_unknown_id` | `remove` names a scripted warp the rig does not have. |
| `warp_unknown_target` | `clear_target` (or `anim.montage_set_warp_target` with `position: null`) names a target that is not set. |
| `root_route_unknown` | `route` is not one of the four route names. |
| `bad_number`, `bad_string`, `bad_bool`, `bad_id` | A parameter has the wrong type. |
| `bad_path` | An asset path is absolute, has `..` or `:`, names a Windows reserved device (`NUL`, `CON`, …), has the wrong suffix, or resolves outside the project. |
| `montage_unknown_asset` | The montage, child montage or parent file cannot be read. |
| `montage_bad_edit` | The edit is wrong: unknown `op`, index out of range, unknown name, nothing to undo, a clip length the edit needs but cannot know, a timing edit on a child montage. |
| `montage_read_only` | The edit would rewrite a read-only file (the table, or a referencing montage or anim graph); nothing changed. |
| `slot_in_use` | Delete of a slot montages or graph `Slot` nodes still name (pass `force`). |
| `animgraph_unknown_slot` | An issue in `graph_issues` (and a log warning at run time), not an error: a graph `Slot` node names a slot the table or the rig lacks. |
| `slot_unknown` | No slot of that name in the table. |
| `montage_unknown_slot` | An issue, not an error: in `anim.montage_state` `load_issues` (a library montage's track names a slot the player's table lacks; it plays full body) and in `anim.montage_describe` `issues` with `slots`. The cook fails on it. |
| `montage_past_end` | An issue, not an error: in `anim.montage_describe` `issues` of a variant, a section, notify, warp or root-motion window that lies past the variant's timeline end. |

A rejected `anim.montage_play` adds no player and changes nothing.

## Skeleton slot table (`.slots.ron`)

Every rig of one skeleton offers the same montage slots. A `SkeletonSlots` table
(`regia_animation::skeleton_slots`, spec 34 § 3, ADR-1644) lists each slot's name, group and joint mask, so a montage
that names a slot the skeleton lacks is found when the table is checked (`Montage::validate_against` reports a
`montage_unknown_slot` issue per track) instead of at play time. Without a table the engine default applies: one
full-body slot `default` in group `default`.

```ron
(
    format_version: 1,
    slots: [
        (name: "default", group: "default"),
        (name: "upper_body", group: "attacks"),
    ],
)
```

- `name` is unique, lowercase ASCII, digits and `_`, 1 to 48 characters; `group` is required (a montage interrupts
  the playing montage whose main slot shares its group); `mask` (optional) lists the joints the slot covers, the
  whole body when absent. `validate_against(&Skeleton)` names a mask joint the skeleton does not have.
- `slot_defs()` feeds `MontagePlayer::new`.
- Renaming a slot rewrites every montage that names it, or none; deleting one lists the referencing montages and is
  refused while any exist unless forced (`slot_unknown` for a slot the table does not have).

## Variant montages (`.montagevar.ron`)

A variant derives a montage from a parent by swapping clip segments (the same combo with another weapon's clips)
without copying its timing (`regia_animation::montage_variant`, spec 34 § 6, ADR-1644). Everything else (sections,
notifies, warps, blends, flags) comes from the parent and cannot be named in the file. A variant's parent may not
itself be a variant.

```ron
(
    format_version: 1,
    name: "combo_axe",
    parent: "combo_sword",
    overrides: [
        (track: 0, segment: 1, clip: "AxeSwing", rescale: true),
    ],
)
```

- `name` is the resolved montage's name (the library key); `parent` is a montage name or the path of its
  `.montage.ron` (the file stem is used).
- Each override names a `track` and a `segment` index in the parent and the replacement `clip`. With
  `rescale: true` the segment's rate is set so the replacement fills the original segment's duration.
- `MontageLibrary` holds montages and the variants resolved from them; replacing a parent re-resolves its
  variants, so a rebuilt parent shows up on the next play.
- Reading fails for a version newer than the engine, an empty `name` or `parent`, or an override with no clip;
  resolving fails for a track or segment the parent does not have, or a rescale whose clip length is unknown.
- `regia upgrade-project` re-saves `.montagevar.ron` files through the format's migrators (v1 is the first version).

## Load warnings, cook errors and past-end items (spec 34 M1, M4; ADR-7216)

- A montage added to a player whose slot table lacks one of its track slots is a load warning: the log says
  `montage_unknown_slot`, `anim.montage_state` lists it under `load_issues` (`code`, `track`, `detail`), and
  `anim.montage_play` still plays it full body in group `default` (`MontagePlayer::try_play` refuses it). A player
  with no slot table reports nothing.
- `regia cook` fails a montage whose track slot no `.slots.ron` of the project names (`montage_unknown_slot`, the
  montage's path in the error list). A project without slot tables defines its slots in code, so nothing is checked.
- A variant with a `rescale: false` override can be shorter than its parent. Its inherited sections, notifies and
  windows keep their times; `anim.montage_describe` of the variant lists each one past the new end as a
  `montage_past_end` issue (`kind`, `index`, `name`, `time`, `end`), and `MontageLibrary::past_end(name)` returns the
  same list. A section that starts exactly at the end is empty, not past it.

## Owner despawn (spec 34 M5)

Despawning a rig ends its montages the way `cancel_all` does: a playing montage gets `blending_out` then `ended`,
both `interrupted: true`; a queued one gets a single `ended` (`interrupted: true`) and never `started`. The
`MontageEvent` messages are written while the rig despawns, so a waiting caller always sees a terminal event;
`anim.montage_state` cannot read a despawned rig (`entity_missing`).

## Montage commands (spec 34 § 9 replication hook)

Every playback change a gameplay system makes to a `MontagePlayer` (play, queue, stop, cancel all, jump, relink,
branch action, rate, reverse, time-stretch, warp targets) is one serializable `MontageCommand`. With recording on,
the player keeps each call's command stamped with its command clock (animation seconds the evaluate system has
advanced it by); the plugin writes them as `MontageCommandRecorded` messages. A replay or network layer writes
`ApplyMontageCommand` messages on another app: each command waits until that player's clock reaches its time and
applies before that frame's evaluation, so the second player makes the same instances, ids and events. The
transport is not in the animation crate: a network layer sends the postcard bytes of `TimedMontageCommand`.

```json
{"method": "anim.montage_commands", "params": {"entity": 4294967310, "record": true}}
```

answers `{ entity, recording, clock, waiting, recent: [{ at, command, detail }] }`: `command` is the lowercase name
(`play`, `queue`, `stop`, `cancel_all`, `jump_to_section`, `set_next_section`, `on_branch`, `set_rate`,
`set_reverse`, `stretch_to`, `set_warp_target`, `clear_warp_target`, `shift_warp_targets`), `detail` the full
command, `recent` the last 32 recorded commands, `waiting` the received commands whose time has not come. Omit
`record` to only read. Errors: `no_montage_player`, `bad_bool`. A received `play` of a montage the player's library
lacks is dropped with the log code `montage_command_unknown_montage`.

## Montage commands over the network (ADR-9370)

`regia::montage_net::MontageNetPlugin` (umbrella crate, feature `net`; opt-in, add it on every process of a session
after `NetPlugin` and the `AnimationPlugin`) is the transport. On the server it drains each recording player of a
replicated rig and broadcasts every command as a reliable, ordered `MontageCommandMsg { tick, seq, bytes }` on that
rig: `bytes` is the postcard form of `TimedMontageCommand`, `tick` the server tick it goes out on and `seq` the
player's sequence number. On a client a `FixedUpdate` system applies each message to the rig's player in the tick
the net layer delivers it, which is the stamped tick, so the montage starts on the same tick and plays the same
sections at the same positions. A message whose `(tick, seq)` is not newer than the last one applied (a duplicate or
a late resend) is dropped, never applied twice. Commands a gameplay system makes in `FixedUpdate` play on clients in
the same tick; commands made in `Update` go out with the next tick and play one tick later on clients.

`anim.montage_commands` reports the counts in `net`:

```json
{"net": {"sent": 0, "received": 8, "dropped": 2, "last_applied": {"tick": 80, "seq": 7}}}
```

`sent` counts on the server's player, `received` and `dropped` on a client's; a rig that is not networked reads
zeros and `last_applied: null`. Log codes: `montage_command_dropped` (debug: a duplicate or late command),
`montage_command_no_player` (a command for a rig with no player yet), `montage_command_bad_bytes` (damaged bytes),
`montage_command_too_large` (over 4096 bytes, not sent), `montage_command_not_sent` (the net layer refused the
send), `montage_command_unknown_montage` (the client's library lacks the montage).

