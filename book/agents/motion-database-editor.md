# Motion database editor

A motion-matching database is authored as a `.motiondb.ron` source (spec 52 § 4: the skeleton, the feature
schema, the tag table, the mirror pairs and the clip entries). The editor's **Motion database** window edits one:
a schema form (features, group weights, trajectory and history sample times, per-sample weights, pose joints,
contact labels, mirror pairs), the tag table, the clip list (add, remove and reorder clips; per-clip flags
`looping`, `mirrored`, `distance_matched` and the end trims; tags as chips), the validator's errors, the editor's
checks, and a **Rebuild** that builds the database in memory with the runtime builder and lists its warnings
and stats. Under each clip, a **span timeline** draws the clip's frames with its spans (`unindexed`,
`pass_through`, `landing_bias`, `tag`) as ranges you drag, and scrubbing it poses a **preview** skeleton and shows
the frame's feature vector. With **Live** on (the default) the warnings and stats rebuild in the background after
each edit. The cooked `.fckd` is never written by the editor; the cook does that (`regia cook`).

Open the window from the **Motion Matching** panel's Database section (**Edit database**, the selected
character's database), from Content ▸ New ▸ **Motion database** (a new file with the schema defaults, no tags
and no clips, opened at once), from the command palette (`view.toggle_motion_database`, also
`editor.command { run: "view.toggle_motion_database" }`), or with `editor.mm_db_open`. The `editor.mm_db_*` methods drive the same document as the window:
one route per window action, and `editor.undo` / `editor.redo` for Undo and Redo (every edit is one step on the
editor's shared undo stack). The file format did not change (version 2, ADR-7740).

## Methods

| Method | Params | Does |
|---|---|---|
| `editor.mm_db_open` | `path`, `discard?` | Opens a `.motiondb.ron` (project-relative or absolute, under the editor's document-path rule) and shows the window. Refused with `unsaved_changes` while the open one is dirty, unless `discard: true`. |
| `editor.mm_db_state` | — | The open document and everything the window shows (below). |
| `editor.mm_db_edit` | `ops`, `label?` | Applies the ops in order as one undo step, all or nothing. The reply adds `changed`. |
| `editor.mm_db_save` | `path?` | Saves the canonical text (the runtime's own writer, `MotionDatabaseSource::to_ron_string`) to where it was opened or to `path`. A document with validator errors is saved too: the source loader reads it, and the issue list says what the build will refuse. |
| `editor.mm_db_rebuild` | `action?`, `enabled?` | `run` (default) rebuilds and waits; `start` returns job `mm_db_rebuild:<n>` at once (`editor.cancel_job`, `agent.job_status` and the background-task row see it); `cancel` stops the running one; `live` with `enabled: true/false` turns the background rebuild after edits on or off (the window's **Live** box). |
| `editor.mm_db_scrub` | `entry`, `frame?` or `time?`, `mirrored?` | Scrubs entry `entry`'s timeline to `frame` (or the frame nearest `time` seconds; default 0): the window's cursor moves there and the preview poses the skeleton. Returns the frame's raw feature vector and its baked tags, bias and flags (below). |

`editor.create_asset { kind: "motion_database", path }` creates the file Content ▸ New writes.

The state (`editor.mm_db_state` and every method's reply):

```json
{ "open": true, "path": "/proj/assets/anim/hero.motiondb.ron", "dirty": false, "revision": 4,
  "comments_warning": null, "document": { "format_version": 2, "skeleton": "...", "schema": { ... },
  "tags": ["walk"], "mirror_pairs": [], "entries": [ ... ] },
  "valid": true, "issues": [], "checks": [{ "severity": "check", "code": "untagged_clip", "entry": 3, "message": "..." }],
  "undo_depth": 2, "redo_depth": 0,
  "rebuild": { "state": "done", "stale": false, "report": { "frames": 412, "dimensions": 27, "ranges": 6,
    "targets": 412, "untagged_frames": 0, "tag_frames": [{ "name": "walk", "frames": 61 }],
    "coverage": [{ "entry": 0, "clip": "anim/walk.clip", "frames": 61, "targets": 61, "mirrored": false }],
    "groups": [{ "group": "root_velocity", "weight": 1.0, "dims": 3, "min": -0.2, "max": 3.1 }],
    "kd": { "nodes": 31, "leaves": 16, "depth": 5 }, "warnings": [], "content_hash": "4f0c..." } },
  "live": { "enabled": true, "runs": 3, "error": null }, "cursor": { "entry": 0, "frame": 12, "mirrored": false },
  "toast": "rebuilt: 412 poses, 0 warnings" }
```

`issues[].span` names the span a `bad_span` issue is about (its row in the window selects that span's start
frame). `live.error` says why the background rebuild could not start (`no_project` without a project).

`rebuild.state` is `none`, `running` (with `job`), `done` (with `report`; `stale: true` once the document changed
after the rebuild started), `failed` (with `code` and `message`) or `cancelled`.

## Operations

| `op` | Fields | Does |
|---|---|---|
| `set_skeleton` | `path` | The skeleton asset path. |
| `set_root_joint` | `joint` | The root (character-frame) joint. |
| `set_forward` | `axis: [x, y, z]` | The root's forward axis (needs an x/z part). |
| `set_feet` | `feet` | The foot joints (no repeats). |
| `set_trajectory_times` | `times` | Future sample times, seconds, each above zero, at most 16. Per-sample weights follow the count. |
| `set_history_times` | `times` | Past sample times, at most 8. |
| `set_sample_rate` | `rate` | Frames per second, above zero. |
| `set_weight` | `group`, `value` | A group weight (`root_velocity`, `trajectory_position`, `trajectory_direction`, `foot_position`, `foot_velocity`, `trajectory_history`, `pose_position`, `pose_velocity`, `foot_contact`, `foot_phase`), clamped to `[0, 1000]`. |
| `set_normalization` | `mode` | `group_deviation`, `per_dimension` or `none`. |
| `set_sample_weight` | `list` (`trajectory` or `history`), `index`, `value` | One per-sample weight, clamped to `[0, 100]`. |
| `add_pose_joint` | `joint` | An extra pose joint (position and velocity, weight 1). |
| `remove_pose_joint` | `index` | |
| `set_pose_joint` | `index`, `position?`, `velocity?`, `weight?` | Weight clamped to `[0, 100]`. |
| `set_contacts` | `contacts` (`null` or `{ height_on, height_off, speed_on, speed_off, phase }`) | Contact labels; thresholds clamped to `>= 0`, each `*_off` raised to its `*_on`. |
| `add_mirror_pair` / `remove_mirror_pair` | `left`, `right` / `index` | Left/right joint pairs (a `mirrored` entry needs them). |
| `add_tag` | `name` | A tag in the table (at most 32). |
| `remove_tag` | `name` | Removes it from the table, every entry's chips and every tag span. |
| `rename_tag` | `name`, `to` | Renames it across the database: the table, the chips and the tag spans. |
| `add_clip` | `clip`, `index?` | A clip entry (not looping, no tags). |
| `remove_clip` | `index` | |
| `move_clip` | `from`, `to` | Reorders entries (the database lays frames out in entry order). |
| `set_clip` | `index`, `clip?`, `looping?`, `mirrored?`, `distance_matched?`, `trim?` | Per-clip flags; `trim: [start, end]` seconds, each clamped to `>= 0`. |
| `add_clip_tag` | `index`, `tag` | A chip; a tag the table lacks is added to it. |
| `remove_clip_tag` | `index`, `tag` | Removes the chip (the table keeps the tag). |
| `add_span` | `index`, `kind`, `range` or `frames` | Adds a span to entry `index`. `kind`: `"pass_through"`, `"unindexed"`, `{ "landing_bias": b }` (clamped to `[-100, 100]`) or `{ "tag": ["name", ...] }` (a tag the table lacks is added to it). `range: [start, end]` is clip seconds, `frames: [start, end]` database frames at the schema's sample rate (frame `k` is `k / rate` seconds, the builder's own frame times); both half-open, `0 <= start < end`. |
| `set_span` | `index`, `span`, `kind?`, `range?` or `frames?` | Moves, resizes or retypes span `span` of entry `index`. |
| `remove_span` | `index`, `span` | |

Overlaps (spec 52 § 2.5: "overlapping spans combine: biases add, masks OR, `unindexed` and `pass_through` are
sticky"): after `add_span` or `set_span`, a `pass_through`, `unindexed` or `tag` span that overlaps or touches
another span of the same kind (for `tag`, the same set of names) is merged with it into their union, which bakes
exactly the same frames; two `landing_bias` spans are kept apart because their biases add where they overlap.
Read the merged result in `document.entries[i].spans`.

Values are clamped to the runtime validator's ranges where it has one; what has no range (a non-finite number, a
time or rate not above zero, a list over its limit, an unknown name or index) is refused.

## Issues, checks and warnings

- `issues` are the validator's errors, from the runtime's own checks (`FeatureSchema::validate` against a
  skeleton made of the names the source uses, `TagTable::new`, `validate_spans`, the mirror and trim rules).
  Codes: `no_skeleton`, `no_clip`, `no_clips`, `bad_sample_rate`, `bad_trajectory_time`, `bad_history_time`,
  `bad_weight`, `sample_weight_count`, `bad_forward`, `duplicate_joint`, `too_many_dimensions`, `bad_contacts`,
  `too_many_tags`, `bad_tag_name`, `unknown_tag`, `no_mirror_table`, `bad_span`, `bad_source`. Whether the
  skeleton has a joint is checked by the rebuild.
- `checks` are the editor's authoring checks (the database builds): `untagged_clip` (an entry with no tag while
  the table has tags: no tag filter ever picks it) and `zero_weight` (a group in the layout with weight 0).
- `rebuild.report.warnings` are the runtime builder's warnings, unchanged (`missing_root_motion`, spec 52 § 2.4),
  each with its clip range, clip name and source entry.

## Span timeline, preview and live stats

Each clip in the window has a strip: a ruler of the clip's database frames, one row per span, and a bottom row.

| Do | Mouse | Keyboard (click the strip first) |
|---|---|---|
| Add a span | Drag on the bottom row (the kind is the **New span** choice; bias or tag beside it) | N adds a one-frame span at the cursor; **Add at cursor** |
| Move a span | Drag its body | Select the body (E), then Left/Right one frame, Shift ten |
| Resize a span | Drag an edge (snaps to frames) | Select an edge (E cycles start, end, body), then Left/Right one frame, Shift ten; the start/end frame fields |
| Select a span | Click it | Up/Down |
| Delete a span | Right-click it, **Delete span** | Delete (or Backspace); **Delete span** |
| Scrub | Click or drag the ruler | Left/Right with no span selected; Escape clears the selection |

Each edit is one `add_span`, `set_span` or `remove_span` op and one undo step. A moved edge snaps to a frame time;
an edge you did not move keeps its exact seconds.

The preview under the strip shows the frame under the cursor: the skeleton posed at the frame's clip time (the
animation editor's preview drawing), whether the frame is a jump target, its landing bias and tags (the runtime's
bake), and its raw feature vector by group. The vector is the runtime builder's own row for that frame: the entry is
built alone with `MotionDatabaseSource::build`, and a frame's raw features depend only on its own clip.
Normalization, which mixes all clips, is applied by the full rebuild. For a `mirrored` entry, **mirrored copy**
shows the mirrored frames' features (the pose is drawn unmirrored).

`editor.mm_db_scrub` returns:

```json
{ "entry": 0, "frame": 12, "frames": 61, "time": 0.4, "mirrored": false,
  "features": [0.0, 0.0, 1.2, ...],
  "groups": [{ "group": "root_velocity", "start": 0, "values": [0.0, 0.0, 1.2] }, ...],
  "meta": { "tags": ["walk"], "bias": -0.5, "pass_through": false, "unindexed": false, "target": true },
  "joints": 24,
  "spans": [{ "span": 0, "kind": { "landing_bias": -0.5 }, "range": [0.0, 0.2], "frames": [0, 6] }] }
```

`meta` is `null` when the entry's tags or spans do not build (the issue list says why); the features and pose
are still shown.

**Live** (on by default): once the document has been unchanged for 0.4 s, the editor starts the rebuild job
(`mm_db_rebuild:<n>`, a worker thread), so the warnings list and the stats follow your edits. A live rebuild of an
older revision is cancelled first. Clicking a warning or a problem row scrolls to its clip and puts the cursor on
the frame it names (a `bad_span` problem: the span's start; a build warning: the clip's first frame).

**Autosave**: an unsaved database is autosaved with the other documents (kind `motion_db`, its canonical text).
After a crash, the recovery list offers it; restoring opens it at its original path, unsaved, with the window shown.

## Refusal codes

Refusals are `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `missing_param` | `editor.mm_db_open` without `path`; `editor.mm_db_edit` without a non-empty `ops` array. |
| `bad_path` | Not a `.motiondb.ron` path. `path_outside_project` comes from the document-path rule. |
| `not_found` | The file cannot be read. |
| `bad_source` | The file does not parse, or its `format_version` is newer than 2. |
| `unsaved_changes` | Opening over a dirty document without `discard: true`. |
| `no_database_open` | Edit, save or rebuild with nothing open. |
| `save_failed` | The write failed. |
| `bad_op` | An op that does not parse (unknown `op` or field). |
| `too_many` | More than 512 ops, or a list over its limit. |
| `bad_value`, `bad_index`, `unknown_group`, `unknown_tag`, `duplicate` | The op refused (the message names its position). |
| `rebuild_running` | A rebuild already runs. |
| `no_rebuild` | `action: "cancel"` with none running. |
| `bad_action` | `action` other than `run`, `start`, `cancel` or `live`. |
| `missing_param` | `editor.mm_db_scrub` without `entry`; `action: "live"` without `enabled`. |
| `bad_index` | `editor.mm_db_scrub` for an entry that does not exist (also the `set_span`/`remove_span` op code). |
| `bad_frame` | `editor.mm_db_scrub` past the entry's last frame, or `mirrored: true` for an entry that is not mirrored. |
| `bad_value` | `editor.mm_db_scrub` with both `frame` and `time`, or a negative `time` (also the op code). |
| `unknown_skeleton`, `unknown_clip`, `build_failed` | `editor.mm_db_scrub` could not load or build the entry (the message says what). |
| `no_project` | A rebuild with no project open (it reads the cooked skeleton and clips). |
| `job_start_failed` | The worker thread did not start. |

A failed rebuild (`rebuild.state: "failed"`) has `code` `unknown_skeleton`, `unknown_clip`, `clip_non_finite`,
`clip_skeleton` or `build_failed` (the message starts with the runtime error's code).

## Dogfood

```json
{ "method": "editor.create_asset", "params": { "kind": "motion_database", "path": "assets/anim/hero.motiondb.ron" } }
{ "method": "editor.mm_db_open", "params": { "path": "assets/anim/hero.motiondb.ron" } }
{ "method": "editor.mm_db_edit", "params": { "ops": [
  { "op": "set_skeleton", "path": "characters/hero.glb#skeleton/Armature" },
  { "op": "set_feet", "feet": ["foot_l", "foot_r"] },
  { "op": "add_clip", "clip": "characters/hero.glb#clip/walk" },
  { "op": "set_clip", "index": 0, "looping": true },
  { "op": "add_clip_tag", "index": 0, "tag": "walk" } ] } }
{ "method": "editor.mm_db_rebuild", "params": {} }
{ "method": "editor.mm_db_save", "params": {} }
```

Spans and scrubbing:

```json
{ "method": "editor.mm_db_edit", "params": { "ops": [
  { "op": "add_span", "index": 0, "kind": { "landing_bias": -0.5 }, "frames": [0, 6] },
  { "op": "add_span", "index": 0, "kind": "pass_through", "range": [0.8, 1.0] } ] } }
{ "method": "editor.mm_db_scrub", "params": { "entry": 0, "frame": 3 } }
{ "method": "editor.mm_db_edit", "params": { "ops": [ { "op": "set_span", "index": 0, "span": 0, "frames": [0, 9] } ] } }
{ "method": "editor.mm_db_rebuild", "params": {} }
```

Read `issues` before saving, `rebuild.report.warnings` after rebuilding, `rebuild.report.coverage` to see
which clips contribute jump targets, and `editor.mm_db_scrub`'s `meta.target` to check that a span excludes the
frames you meant.
