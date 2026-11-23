# Steam

Methods for the shared Steam connection and the Steam Input bridge: all read-only except `steam.set_deck_preset`, which
changes the Deck performance preset (permission `play`). It needs `regia_steam::SteamPlugin` in the app;
the plugin registers it on the `AgentExtension`.

| Method | Params | Result |
|---|---|---|
| `steam.status` | — | `{ plugin, state, running, offline, user_id, app_id, overlay_open, steam_deck }` |
| `steam.input` | — | `{ plugin, enabled, active, controllers, action_set, driven_actions, ui_down }` |
| `steam.input_manifest` | — | `{ plugin, ok, manifest?, error? }` |
| `steam.deck` | — | `{ plugin, detected, cap_fps, power_cap_fps, power_reason, render_scale_factor, controller, glyph_family, keyboard_open, keyboard_failures }` |
| `steam.glyphs` | `family?`, `action?`, `context?` | `{ plugin, family, controller, buttons: [{ button, label, texture_key }], context?, action?, prompts? }` |
| `steam.deck_perf` | — | `{ plugin, detected, preset, presets, panel, frame_cap_fps, render_scale, upscaler, csm_cascade_count, csm_resolution, ssao, contact_shadows, bloom_mip_count, local_shadow_max_lights, max_visible_lights, tdp_hint_watts, game_resolution, gamescope, output, compositor_upscales, output_16_10 }` |
| `steam.set_deck_preset` | `preset` | `{ plugin, preset, previous }` |
| `steam.layout` | `family?`, `context?`, `keyboard_only_ok?` | `{ plugin, ok, family, sheets: [{ context, rows: [{ control, label, texture_key, actions }], keyboard_only }], issues: [{ severity, code, context, action, control, message }], errors, error? }` |

- `plugin` is `false` (and nothing else is present) when the app has no `SteamPlugin`.
- `state` is `Ready`, `Unavailable` (Steam not running, feature off, Android) or `Restarting`.
- `running` is `state == Ready`. `offline` means Steam runs in offline mode (not logged on).
- `user_id` is the 64-bit Steam id as a string, or `null` when not signed in.
- `overlay_open` tells you why gameplay input is suspended: the Steam overlay is up.

On a CI box without Steam, `steam.status` answers `running: false`; tests that need a connected
state inject `MockSteam` (see [Steam](../user-guide/steam.md)).

## Steam Input

- `steam.input`: `enabled` is `SteamInputSettings::enabled`. `active` is true once Steam Input
  initialized with Steam ready. `controllers` is what Steam Input reports. `action_set` is the
  upper-case set chosen from the player's top-priority context, `driven_actions` the Regia Engine
  actions currently held from Steam, `ui_down` the UI actions (`ui_up`, `ui_accept`, ...) down.
  With the bridge off the result is `enabled: false, active: false`.
- `steam.input_manifest`: the generated in-game actions file for the live `InputConfig`.
  `ok: false` with an `error` text when the config cannot be expressed (a 3D action, or two actions
  mapping to one Steam name); a missing `InputConfig` is also reported there. Neither is a protocol
  error.
- Check a Deck run: `steam.input` should show `controllers >= 1` and the expected `action_set`;
  press A and read `ui_down`.

## Deck readiness (`steam.deck`)

`steam.deck` is read-only and reports what the Deck-specific systems decided (ADR-368):

- `detected` is the Deck flag. Off a Deck every system below is idle and the fields keep their defaults.
- `cap_fps` is the player's chosen cap; `power_cap_fps` is the ceiling the thermal governor allows (60 means no
  limit). The frame loop paces at the lower of the two.
- `power_reason` is `none`, `low_battery` or `thermal`. `render_scale_factor` is a multiplier the host applies to
  the render scale (1.0 means unchanged).
- `controller` is the controller kind the UI last saw (`Unknown`, `SteamDeck`, `SteamController`, `Xbox`,
  `PlayStation`, `Switch`, `Generic`). `glyph_family` is the glyph directory to draw: `xbox`, `playstation`,
  `switch` or `deck`.
- `keyboard_open` tells you whether the floating keyboard is up; `keyboard_failures` counts refused show requests.

`plugin` is `false` (and nothing else is present) when the app has no `SteamPlugin`. There are no error codes.

## Prompts, presets and FSR 1 (ADR-526)

- `steam.glyphs` lists the label and texture key of all 17 standard buttons for a glyph family
  (`xbox`, `playstation`, `switch`, `deck`; default: the family the UI shows now). With `action`
  it also returns `prompts`: one entry per distinct gamepad control bound to the action in
  `context` (default: the player's top enabled context, else `Gameplay`), in binding order.
  Keyboard and mouse bindings have no prompt; an unknown action or context gives `prompts: []`.
  A stick axis shares the stick's glyph (`glyphs/<family>/left_stick`), a trigger axis the trigger's.
  An unknown `family` is a protocol error (`F0002`, invalid params). A present `family`,
  `action` or `context` that is not a string is the same error. Omit the field, or pass JSON
  null, to take the default.
- `steam.deck_perf` reports the performance preset in force and its effective budget: the preset's
  knobs after the thermal and battery governor, fitted to the output size. `tdp_hint_watts` is the
  SteamOS power limit the preset is meant to hold its cap under (`null` = the default limit). It is
  a hint and is never applied. `gamescope` is true under gamescope. `output` is the game's window
  size (`null` until the first resize). `compositor_upscales` is true when gamescope scales a
  smaller output up to the panel (the FSR 1 profile). Off a Deck, `detected` is false and the
  knobs stay at Balanced.
- `steam.set_deck_preset` switches the preset (`battery`, `balanced`, `performance`, `fsr1`;
  permission `play`). On a Deck the new budget applies on the next frame; off a Deck only the stored `preset` changes and the knobs stay at Balanced. The preset's frame cap is written
  into the player's cap; the player can still pick another cap afterwards. An unknown or missing
  `preset` is `F0002`.

Check a Deck run:

1. Call `steam.set_deck_preset {"preset":"battery"}`, step a frame, then read `steam.deck_perf`:
   `frame_cap_fps` is 30 and `render_scale` is 0.6.
2. For the FSR 1 profile, start the game at 960×600 with gamescope's FSR filter on, then read
   `steam.deck_perf`: `compositor_upscales` is true, `output_16_10` is true, and with `fsr1` the
   `render_scale` is 1.
3. Plug in a DualSense, then call `steam.glyphs {"action":"Jump"}`: `family` is `playstation`.
   On the Deck's own controls it is `deck`.

There are no new error codes.

<!-- depth-2:method-reference -->

These calls were sent to a headless editor, which does not register the steam methods, so each answer is unknown method (`F0008`). A process that registers them and has no Steam resource answers success with `plugin` false instead.


## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `steam.deck`

Deck readiness: detection, frame cap, power policy, controller glyph family, keyboard state.

*Read, does not mutate.*

No parameters.

Result: `{ plugin, detected, cap_fps, power_cap_fps, power_reason, render_scale_factor, controller, glyph_family, keyboard_open, keyboard_failures }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture.

```json,ignore
steam.deck {}
→ { "message": "unknown method steam.deck", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.deck -->

### `steam.deck_perf`

Deck performance preset in force: frame cap, render budget, power hint, gamescope (FSR 1) scaling.

*Read, does not mutate.*

No parameters.

Result: `{ plugin, detected, preset, presets, panel, frame_cap_fps, render_scale, upscaler, csm_cascade_count, csm_resolution, ssao, contact_shadows, bloom_mip_count, local_shadow_max_lights, max_visible_lights, tdp_hint_watts, game_resolution, gamescope, output, compositor_upscales, output_16_10 }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture.

```json,ignore
steam.deck_perf {}
→ { "message": "unknown method steam.deck_perf", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.deck_perf -->

### `steam.glyphs`

Controller prompt glyphs: every button's label and texture key for a family, and the prompts of an action.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `family` | string | no | xbox, playstation, switch or deck; default the family the UI shows now |
| `action` | string | no | an input action; its gamepad bindings become `prompts` |
| `context` | string | no | the input context of `action`; default the player's top enabled context, else Gameplay |

Omitting action returns the button list without action prompts; context only applies when action is supplied.

Result: `{ plugin, family, controller, buttons: [{ button, label, texture_key }], context?, action?, prompts? }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture.

```json,ignore
steam.glyphs {}
→ { "message": "unknown method steam.glyphs", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.glyphs -->

### `steam.input`

The Steam Input bridge: enabled, active, controllers, chosen action set, held actions, UI actions down.

*Read, does not mutate.*

No parameters.

Result: `{ plugin, enabled, active, controllers, action_set, driven_actions, ui_down }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture.

```json,ignore
steam.input {}
→ { "message": "unknown method steam.input", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.input -->

### `steam.input_manifest`

The Steam Input in-game actions file generated from the live InputConfig.

*Read, does not mutate.*

No parameters.

Result: `{ plugin, ok, manifest?, error? }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture.

```json,ignore
steam.input_manifest {}
→ { "message": "unknown method steam.input_manifest", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.input_manifest -->

### `steam.set_deck_preset`

Switch the Deck performance preset (battery, balanced, performance, fsr1); on a Deck the budget applies next frame, off a Deck only the stored preset changes.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `preset` | string | yes | battery, balanced, performance or fsr1 |

Result: `{ plugin, preset, previous }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture.

```json,ignore
steam.set_deck_preset {"preset": "balanced"}
→ { "message": "unknown method steam.set_deck_preset", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.set_deck_preset -->

### `steam.status`

The shared Steam connection: running, offline, user id, app id, overlay state, Deck flag.

*Read, does not mutate.*

No parameters.

Result: `{ plugin, state, running, offline, user_id, app_id, overlay_open, steam_deck }`.

No error codes beyond the universal set.

With the extension handler registered but its Steam resource absent, the call succeeds with `plugin` false. A host that never registered the extension returns `F0008`, as in this capture. It does not return an F-code for that.

```json,ignore
steam.status {}
→ { "message": "unknown method steam.status", "code": -32601, "data": { "code": "F0008", "hint": ..., "suggestion": "stream.status" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19127 chars).

<!-- capture:steam.status -->

## Workflow: see whether Steam input and the Deck preset are live

1. `steam.status`. `F0008` means this host did not register the Steam extension. If its handler is registered but the Steam resource is absent, it succeeds with plugin false. A present resource with running false means Steam is unavailable; offline mode is reported separately by offline.
2. When `plugin` is true, `steam.input` for the actions the manifest bound, and `steam.glyphs` with `family` and an `action` for the prompt labels.
3. `steam.deck` says whether the process is on a Deck. `steam.deck_perf` is the preset in force.
4. `steam.set_deck_preset` with `battery`, `balanced`, `performance` or `fsr1`. Off a Deck only the stored preset changes. An unknown name is `F0002`.
5. `steam.input_manifest` checks the manifest file the game ships. `ok` false carries the manifest error in the success object.

<!-- /depth-2:method-reference -->

## Controller layout (`steam.layout`, ADR-1332)

`steam.layout` is read-only. It shows what a "Controls" screen shows, and checks that the game can be played on a
controller alone (the controller part of Valve's Deck Verified review).

- `family` is the glyph family the labels are for (`xbox`, `playstation`, `switch`, `deck`; default: the family the UI
  shows now, which is `deck` on a Deck before any other pad reports in).
- `sheets` has one entry per input context, highest priority first, or only `context` when you pass it. Each `rows`
  entry is one bound control, in the layout order (face buttons, shoulders, triggers, each stick before its click, the
  D-pad, Start, Select, the system button). `control` is the family-neutral id (`south`, `left_trigger`, `left_stick`,
  `left_stick_click`, `dpad_up`, `dpad`, `start`, `mode`, ...). `actions` are the actions on it, in binding order.
  `keyboard_only` lists the actions this context binds to keys or the mouse but to no usable gamepad control.
- `issues` are sorted errors first. `errors` counts them. The issue codes:

| Code | Severity | Meaning |
|---|---|---|
| `no_gamepad_binding` | error | The action has key or mouse bindings in the context and no gamepad binding: a Deck player cannot do it. Pass the action in `keyboard_only_ok` when that is deliberate (a debug console key). |
| `reserved_control` | warning | The action is on the system button (Steam on the Deck, Guide, PS, Home). The system menu takes that button, so it does not count as the action's gamepad binding. |
| `control_conflict` | warning | Two or more actions share one control with plain triggers (no hold, tap, pulse or chord). One press fires all of them. |
| `missing_glyphs` | warning | Only from `regia steam layout --glyphs <dir>`: a family's glyph textures are incomplete. |

- A missing `InputConfig` is `ok: false` with an `error` text, not a protocol error. An unknown `family` or `context`,
  or a `keyboard_only_ok` that is not a list of strings, is `F0002` (invalid params). There are no new error codes.
- The same report from the command line: `regia steam layout [--family deck] [--context NAME] [--glyphs assets]
  [--allow-keyboard-only ACTION]... [--check]`. `--check` makes an error fail the command, for CI.

Check a build:

1. Call `steam.layout {"family":"deck"}`. `errors` must be 0 before the game goes to the Deck review.
2. Read the `Gameplay` sheet: every action you expect on the pad has a row, and `keyboard_only` is empty or holds only
   your debug keys.
3. Connect a DualSense and call `steam.layout` without `family`: the labels change to Cross, Circle, L1, ...

## The net_game client's Steam line (overlay check)

The `net_game` sample's windowed client built with `--features steamworks` (or `--features steam` on the mock
backend) runs `SteamPlugin` and shows a HUD text node named `steam_overlay` (ADR-7317,
`book/user-guide/steam-real-run.md` § 3). No new method: read the state with `steam.status` and the line with
`ui.query`.

1. `steam.status {}`: `overlay_open` is true while the Steam overlay is up; `running` false means no Steam client.
2. `ui.query {"name": "steam_overlay"}`: the one node's `text` holds `Steam (<backend>): running, app <id>`,
   `overlay OPEN` or `overlay closed (Shift+Tab opens it)`, `last: OverlayOpened (opened N, closed M)` (or
   `OverlayClosed`, or `no overlay message yet`), and `gpu: <adapter> (<backend>)` once the window has a device.

The mock test `cargo test -p net_game --features steam --test steam_overlay` injects `OverlayActivated` and checks
both reads.

## The overlay trace and the cursor hold (ADR-7349)

No new method. Two engine behaviors for the overlay check:

- **Cursor hold.** While the overlay is open, `SteamPlugin` sets `regia_platform::CursorRelease`: the window layer
  shows a free cursor and passes no game grab to the OS until `OverlayClosed` (or a shutdown), then applies the
  game's current `Window::cursor_grab` / `cursor_visible` once. `SteamSettings::release_cursor_on_overlay = false`
  turns it off. `regia_platform::CursorCalls` counts the `set_cursor_grab` / `set_cursor_visible` calls the window
  layer made.
- **Trace.** `REGIA_STEAM_OVERLAY_TRACE=<file>` writes a line per `GameOverlayActivated` callback (repeats included),
  `OverlayOpened`/`OverlayClosed`, window focus change, Shift/Tab key event, cursor wish/hold/OS call, and a
  heartbeat once a second; the header names the overlay injection facts (`overlay_renderer_preloaded`,
  `vk_overlay_layer`). Format and how to read it: `book/user-guide/steam-real-run.md` § 3.

The `net_game` client's line now starts with `STEAM OVERLAY: OPEN (opened N, closed M)` or
`STEAM OVERLAY: closed (...)`; read it with `ui.query {"name": "steam_overlay"}` as before.

Tests: `cargo test -p regia_steam --test plugin the_overlay` (hold through re-grabs and focus churn, opt-out, trace
contents), `cargo test -p regia_platform --features window --lib a_cursor_hold` (no grab call while held),
`cargo test -p net_game --features steam --test steam_overlay` (headline row, Shift+Tab never shows the scoreboard).
