# Capability markers

`agent.hello` lists named markers in `capabilities`, next to `reflection`, `events`, `assets`, `render`
and the others. `agent.schema` repeats them as `capability_markers`, each with a one-line description.
Ask for a marker instead of probing parameter names; a build that lacks a feature does not list it.

| Marker | Present when | Means |
|---|---|---|
| `shot_fade_layer` | always | Shot fades (`fade_in`, `fade_out`, `fade_color`) are drawn by the engine in layer `"fade"` above the UI. `render.sequence` reports `fade_layer`; the render-out manifest has a `fade` block. |
| `render_out_aov_id` | always | `render.out_start` `aovs` accepts `"id"`: `id.instance` and `id.class`, with the manifest's `aov.ids`. |
| `render_out_aov_depth` | always | `aovs` accepts `"depth"` (channel `Z`). |
| `render_out_aov_motion` | always | `aovs` accepts `"motion"` (channels `motion.u`, `motion.v`). |
| `render_out_aov_material` | always | `aovs` accepts the material AOVs `"normal"`, `"normal_view"`, `"roughness"`, `"metalness"`, `"base_color"` and `"material_id"`; `aov_format` accepts `"png"` and `"png16"` for the 0..1 ones. |
| `shot_rotation_keys` | always | `camera.set_path` accepts `rotation` keys and `.shots.ron` stores them. `agent.hello` also reports `shots_ron_version: 3` (v3 lets path and look-at keys pass a shot's end, ADR-8640). |
| `render_out_audio` | the world has an `Audio` resource | `render.out_start` `audio` writes a WAV mixdown. Without an `Audio` resource the call answers `F0013`. |
| `render_out_audio_stems` | the world has an `Audio` resource | `audio.stems` writes one WAV per bus. |
| `editor_tools` | the editor hosts the agent | `editor.tool_*` methods and `<id>.tool.toml` manifests; see [editor tools](editor-tools.md). |
| `configurator_contracts_v1` | always | `configurator.contract_check` and `configurator.contract_schema` validate the `jewelry.<name>/1` product viewer contracts; see [configurator contracts](configurator-contracts.md). |
| `configurator_runtime_v1` | always | `configurator.open_product`, `configurator.options`, `configurator.set_pick`, `configurator.build` and `configurator.resolve` run the product configurator (ADR-2534); see [Product configurator](configurator.md). |
| `viewer_view_pack_v1` | always | `viewer.view_simulate` and `viewer.pack_verify`: the product viewer drag view and packages; see [viewer view](viewer-view.md). |

```json
{ "method": "agent.hello", "params": {} }
```

Check `capabilities` for the marker you need. The editor's own methods also appear in `methods`
(`editor.tool_call`, `editor.tool_panel`), but the marker is the stable name.

A host that embeds the agent adds its own marker with `AgentExtension::add_capability(name, description)`.

`name` and `description` are static strings supplied by the host; the method replaces the description
when the same name is added again. The method does not validate marker names or return an error for a
malformed name. The marker is reported as supplied, so hosts should use stable lowercase ASCII names.
There is no rejection code for a bad name in this implementation.

Workflow: call `agent.hello`, check that `capabilities` contains the required marker, and only then
call the method that marker advertises. For example, require `render_out_audio` before sending audio
options to `render.out_start`; if it is absent, omit that option.

Captured failing call (lavapipe; render-out timing parameters omitted):

```console
$ regia agent call render.out_start '{}' --project samples/arena
error: method call failed: give `frames` or `duration_s` (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```
