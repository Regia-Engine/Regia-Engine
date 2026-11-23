# Safety: permissions, read-only mode, audit and provenance

The owner stays in control (specs/23 § 5). The project gates the protocol with
`<project>/.regia/agent.toml`; every call lands in an audit log; every authored change is stamped
with provenance and listed for review.

## Permission profiles

```toml
# .regia/agent.toml
read_only = false                     # true: every mutating call is denied (F0012)
permissions = ["read", "edit_scene", "assets", "play"]
rate_limit_per_second = 50            # mutating calls (1..=10000)
heavy_rate_limit_per_second = 60      # cost "heavy" calls, read-only ones too (1..=10000)
output_roots = ["/srv/capture"]       # extra directories render.out_start may write under
replay_roots = ["/srv/replay-store"]  # extra directories replay.load may read from (never written)
```

- `read` — every query method (always allowed).
- `edit_scene` — scene edits and live settings writes.
- `assets` — loading and reloading assets.
- `play` — pause/step/resume, time scale, input injection.

Default (no file): the full working set above. A file that exists but does not parse or cannot be read fails closed (logged as an error, never fatal): `read_only` with the `read` permission only. A denied call
answers `PERMISSION_DENIED` with `error.data.code = "F0012"` and a hint that names the permission.
`output_roots` lists absolute directories that `render.out_start` may write under, in addition to the project root, the
system temp dir (see [Render-out](render-out.md)). The same roots bound the files `demo.inspect`,
`demo.seek_plan` and `replay.load` read (ADR-1769). `replay_roots` lists absolute directories `replay.load` may
read from on top of those (ADR-1965): a replay store kept outside the project goes here, not in `output_roots`,
because `output_roots` also says where the engine may write. A host adds replay roots of its own with the
`regia_agent::ReplayRoots` resource (the showcase host's `--replay-root DIR` flag). Unknown keys are ignored for forward compatibility. The editor-side confirmations (`package`,
`delete`, `bless`) arrive with the editor surface.

## Request paths

Since v0.5.1 (ADR-7871), the process's working directory is **not** an additional allowed root.
Starting a game from a home directory therefore does not grant access to that directory when a
project root is supplied. Configure `output_roots` or `replay_roots` explicitly for folders outside
the project and temp roots. With neither an asset-server root nor `AgentSettings::project_dir`,
the working directory still serves as the project root itself. Every agent method resolves a
project-relative path against that one root, in that order (ADR-8798: `ai.bt_edit`, `ai.statetree_edit`,
`anim.montage_edit`, `anim.montage_slot_edit`, `audio.loopgen_render`, `level.export_bake` and the project reads of
`logic.*`, `statetree.*`, `vfx` systems and `fuzz.run` used to fall back to the working directory when only
`project_dir` was set). A write path whose deepest existing part cannot be resolved (the project root itself, or
a dangling symlink) is refused, not let through. The temporary file of every atomic write beside a project file
(`x.bt.ron.new`, `.x.<pid>.tmp` and the like) is created fresh: a file or symbolic link already at that predictable
name is removed (a link itself, never its target) and the temp is opened with exclusive create, so a link planted
there in a cloned project never receives the bytes (ADR-10066; `audio.loopgen_render` did this since ADR-3206).

File paths in requests are confined (ADR-1816). No method, under any rule, reaches a path under a directory named
`goldens` (`golden_path_denied`, [Golden images](goldens.md)). Two rules, named in each method's page:

- **Project-relative** (levels for `level.storage`/`level.convert`, `functest` levels and scripts, prefabs and
  materials under `assets/`, `editor.create_asset`, the import folder, procedural graphs, `configurator.contract_check`
  `package_dir`): no absolute path, no
  drive or UNC prefix (`C:x`, `\\server\share`), no `:`, no `..` (nor a segment of only dots and spaces, such as
  `.. ` or `...`), no NUL, no device name (`CON`, `NUL`, `COM1`, ...), and the file (or its deepest existing parent) may not
  lead out of the project through a symlink.
- **Allowed roots** (the `render.out_start` rule): a relative path joins the project root and keeps the lexical rules
  above (no drive or UNC prefix, no `:`, no `..`, no NUL, no device name); an absolute one must lie under the project
  root, the system temp dir or an `output_roots` entry, with no `..` and no NUL; the deepest existing
  parent is checked with symlinks resolved. Used by `render.out_start` `out`, `render.out_job`'s paths, `abilities.validate`,
  `abilities.add_attributes`, `content.set_source`, every `out_dir`, `editor.save_level`, `editor.set_reference`,
  `editor.view_asset`, `editor.thumbnails`, `editor.spawn_gltf_hierarchy`, `editor.collision_layer`,
  `editor.play_sound`, `editor.model_report`, `editor.model_extensions`, `editor.theme_panel_open`/`_save`, the animation editor's paths,
  `editor.bake_set`/`bake_gi` `out`, `editor.procgen_capture` `out`, and `photo.capture` `directory` (also under the
  session pictures folder; ADR-8795). `demo.inspect`, `demo.seek_plan` and
  `replay.load` read under the same roots; `replay.load` also under `replay_roots` and the host's `ReplayRoots`.
- **Documents** (ADR-7388): the project-relative rule for a relative path; an absolute one must lie under the project
  root or the system temp dir (not `output_roots`, which widen outputs only); the deepest existing
  parent is checked with symlinks resolved, and a network or device form outside both is refused before the filesystem
  is touched. Used by every path an editor tool opens, reads or saves: `editor.save_level` `path`,
  `editor.set_reference` `path`, `editor.ui_designer_open`/`_save`, `editor.ui_anim_open`/`_save`,
  `editor.conversation_open` (`path`, `project`)/`_save`, `editor.mode2d_set` `map`/`rules`, `editor.tilemap_save`,
  `editor.bt_*`, `editor.statetree_*`, `editor.matgraph_*` (and an instance's `parent`), `editor.theme_panel_open`/`_save`,
  the animation editor's paths, the mesh-source paths, `editor.lightmap_*` `source`, and the read-only inspectors
  `editor.usd_inspect`, `ply_inspect`, `stl_inspect`, `psd_inspect`, `fbx_inspect`, `obj_inspect`, `bvh_inspect` and
  `svg_report`, and `net.sim_clients` `recording`. Saves also check the document's suffix (`.ui.ron`, `.uianim.ron`, `.conv.ron`, `.tilemap.ron`, ...). A
  refusal lists `allowed_roots: ["project", "temp_dir"]`. Paths a level or UI document holds (a `TileMap` map, a UI
  component's `use`) follow the project-relative rule.

A refused path answers `F0002` with `data.reason = "path_outside_project"` and `data.path_reason` naming the rule:
`path_absolute`, `path_prefix`, `path_parent`, `path_nul`, `path_device`, `path_trailing` (a segment ends in a dot
or a space, which Windows can drop when it opens the file), `path_remote` (a network or device form outside every root, refused before the
filesystem is touched), `path_empty`, `path_escapes` (outside every root, or through a symlink) or `path_io` (the root
cannot be resolved). An allowed-roots refusal also lists
the kinds of root the parameter may lie under in `data.allowed_roots` (`project`, `temp_dir`,
`output_roots`, and `replay_roots` for `replay.load`); it never names a host path. Methods with their own reason code
keep it (`procgen_bad_path`). `editor.import` `sources`, `editor.new_project` and `editor.open_project` take paths
from anywhere by design, and so does `editor.open_kit` (inspect a Tincal kit folder before
importing it; the kit reader confines its reads to the folder; `editor.fbx_inspect`/`obj_inspect` did too before
ADR-7388: a `Read` method no longer reaches outside the project and the temp dir, so import a model or copy it there first), `editor.bake_gi` `scene` (a
flag-like `-...` is refused), and `configurator.open_product` `root` (a content directory, never a network or device form; every path below it is checked lexically, see
[the configurator page](configurator.md)). `replay.load` and `demo.*` are not among them: since ADR-1769 (v0.2.8) they read only
under the allowed roots above (before, they took a path from anywhere).

## Audit log

Every call — success, error or deferred — appends one JSON line to
`<project>/.regia/agent/audit.jsonl`:

```json
{ "seq": 1, "frame": 621, "unix_ms": 1730000000621, "method": "scene.spawn",
  "params_hash": "9e107d9d372bb682", "outcome": "ok", "error_code": null,
  "duration_us": 412, "client": "claude" }
```

The params hash is FNV-1a over the params (compact — params can be huge). Where the log lives is in
`project.info` → `audit_log`. `outcome` is `ok`, `error`, `deferred`, `expired` (the caller already got `F0006`,
so the call was not run) or `board` (a `render.out_progress` the connection answered from the render-out
progress board while the app thread was inside a render-out step, so the app did not run it; ADR-2582).

The log is owner-only on Unix and on Windows (an owner-only DACL; a log an earlier version left readable by other
accounts is rewritten owner-only on open, ADR-5489), and at 64 MiB it moves to `audit.1.jsonl` and starts over; one previous file is kept. A
project is often a cloned repository, so a symbolic link planted at `audit.jsonl` is replaced, never appended through,
and a linked agent folder turns the log off (also at rotation: if the new log cannot be opened after the move, the log
stops rather than growing `audit.1.jsonl`). The token and `endpoint.json` get the same care: a linked agent folder is
refused, and a client reads the token only from a small regular file of token characters.

## Provenance of agent-made changes

Every successful authored edit (scene edits, settings writes) is stamped and readable through
`agent.changes` — oldest first, one page at a time. `since` is exclusive (records with a greater `seq`);
`limit` caps the page (default 200; `0` counts as 1 and anything over 1000 as 1000, ADR-7454). `next` is the
`since` of the following page: the last returned `seq`, or the newest `seq` in the log when the page is empty.
Walk the whole log with `since: 0`, then `since: next`, until a page comes back empty:

```json
agent.changes {}
→ { "records": [{ "seq": 1, "frame": 700, "utc_unix_ms": 1730000000700,
    "method": "scene.set_property", "target": "entity 17 Transform.translation.y",
    "summary": { "before": 0.0, "after": 2.0, "applied": true },
    "tool": "agent", "client": "claude", "model": "sonnet", "session": "pid-4242",
    "reviewer": "pending" }], "next": 1 }
```

The log keeps the newest 10,000 records (`seq` keeps increasing). `dry_run` previews and failed calls are not changes and are not stamped. The dispatcher skips that stamp for an `edit_scene` call with `dry_run: true`. A method in another permission class does not preview unless its own handler reads `dry_run`; `nav.add_obstacle` inserts the box either way. Asset-file provenance
(`.meta` records, staged review, owner confirmation before shipping) is the editor surface's half of
this rule.

## Trust model

The token is the outer gate, and `build`/`play` amount to code execution (specs/23 § 5): grant them
only to agents you would trust with a shell on the machine, or run the app in a container. Mutating
calls are rate-limited per second (`F0007`).
