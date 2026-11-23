# Asset push into a launched game (`assets.push`)

A game the editor launched (**Launch standalone**, `book/agents/editor-launch.md`) keeps up with the
editor's asset edits: every asset the editor's live cooker rebuilds while the game runs is pushed into
the game over the agent protocol and swapped in place there (P-301, ADR-7836). The Output Log's launch
bar counts them (**pushed N**; failures as **N failed**, with the last reason on hover), and
`editor.launch_status` reports `pushed`, `push_failed` and `push_error`.

The editor connects the way any agent client does. A standalone launch gives the game its own agent
directory, `<project>/.regia/launch/agent-<id>`, in `REGIA_AGENT_DIR` (so the game's endpoint never
replaces the editor's own in `<project>/.regia/agent`); the editor reads the endpoint and token the
game wrote there and calls `assets.push` on a worker thread, so the editor frame never waits on the
game. A failed call drops the connection and the next push reconnects. The directory is removed when
the launch ends.

What is pushed:

| Change | Push | In the game |
|---|---|---|
| A cooked asset the live cooker rebuilt (texture, mesh, material, and every other cooked kind) | `asset`: the id, with the `.fckd` bytes | Written into the cooked cache under the id's own file name and hot-reloaded: handles stay valid and `AssetReloaded` fires (GPU copies, colliders and pipelines rebuild as for any hot reload). |
| A UI document (`.ui.ron`) under the project's `assets/` | `source`: its project-relative path | Written there; `SourceAssetChanged` fires and the UI reloads it. |
| An engine shader (`.wgsl` in the engine's shader directory) | `shader`: its file name | Validated and staged as a shader edit; renderers rebuild their pipelines. Nothing is written. |

## The method

`assets.push { asset | source | shader, data }` — exactly one target; `data` is the bytes as standard
base64, at most **16 MiB** decoded. Permission class: **Assets**.

- `asset`: a cooked asset id (or a path in the game's manifest). The bytes go to
  `<cache>/<id>.fckd` atomically (a temporary file in the same directory, then a rename). From then on
  the id is served from the cache **over any mounted pak**, so a packaged game sees the push too; this
  lasts until the game restarts. The answer carries `loaded` and `generation` (the asset's hot-reload
  count).
- `source`: a project-relative path that must lie under `assets/`: no leading `/`, no drive, no `.` or
  `..` part. The file is written atomically there.
- `shader`: a plain file name ending `.wgsl` (ASCII letters, digits, `_`, `-`, `.`). The text must be
  UTF-8 and pass validation, or nothing changes and `push_shader_invalid` carries the diagnostic.

Identical bytes answer `changed: false` (nothing is written; a cooked asset is still reloaded).

```json
{ "method": "assets.push", "params": { "asset": "0d7c1f6e-2b9a-4c3e-8f51-6a2d9e4b7c10", "data": "<base64>" } }
→ { "kind": "asset", "target": "0d7c1f6e-…", "bytes": 5120, "changed": true, "path": "…/0d7c1f6e-….fckd",
    "loaded": true, "generation": 1 }
```

## Error codes

| Code | When |
|---|---|
| `push_no_target` | Not exactly one of `asset`, `source` and `shader` was given. |
| `push_bad_data` | `data` is missing or not standard base64, or a shader's text is not UTF-8. |
| `push_too_large` | The decoded bytes are over 16 MiB. |
| `push_bad_path` | `source` leaves `assets/`, or `shader` is not a plain `.wgsl` file name. |
| `push_unknown_asset` | `asset` is neither an asset id nor a path in the game's manifest. |
| `push_no_server` | The game has no asset server. |
| `push_io` | Writing the file failed; nothing changed. |
| `push_shader_invalid` | The shader did not validate; the previous shaders stay live. |

## Limits and what is not pushed yet

- An **Android** launch is pushed to through its dev transport (ADR-8472, `book/agents/editor-launch.md`
  "Device transport"): the launch forwards a host port to the game's agent port and writes the
  endpoint into its agent directory, so the same pusher reaches the device. Before a batch that holds
  cooked assets the editor cooks the project for `android` (incremental: only changed sources) and
  pushes the device-format bytes from the `android` cooked cache; UI documents and engine shaders push
  as for standalone. A failed device cook counts each cooked push of the batch as failed with the
  cook's reason. The fast-iterate launch that reads assets from the editor host (P-300) is not built.
- An asset over 16 MiB is not pushed (the launch bar counts it as failed with the reason); relaunch
  to see it.
- Levels are not pushed: a level change in the editor is a play-copy matter (relaunch).

## Testing it

`cargo test -p regia_agent --lib assets_push` drives the method through the dispatcher: a pushed cooked
image replaces the live one behind the same handle, identical bytes answer `changed: false`, a UI
document is written under `assets/` and raises `SourceAssetChanged`, and every refusal answers its
code. `regia_asset`'s `a_pushed_asset_wins_over_the_pak_that_holds_its_id` shows the cache override
over a mounted pak, including for a payload reader made before the push.
`cargo test -p regia_editor --test editor_launch a_rebuilt_asset_is_pushed_into_the_running_standalone_game`
launches a fake game, checks its `REGIA_AGENT_DIR`, and answers the editor's `assets.push` from a real
loopback agent server advertised in that directory.
