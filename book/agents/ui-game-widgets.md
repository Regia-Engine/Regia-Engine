# Game widgets (agents)

ADR-2958. Code: `regia_ui::game_widgets`, driven by `regia_framework::game_ui`. Guide topic: `agent.guide {"topic":
"ui-game-widgets"}`. For people and designers: [Game widgets](../user-guide/game-widgets.md).

Five methods read and steer the widgets games keep rebuilding: a lobby list, a scoreboard, a radar and minimap, world
markers, and target brackets with lead. Each answers in the coordinates the widget drew with, so a test can compare
what is on screen with what the world says, without a screenshot. Every method validates all its parameters before it
changes anything. Without `entity` a method addresses the widget with the lowest entity bits; `ui.query` finds the
others by `name`. All five need the `edit_scene` permission, also to read, and every call counts against the limit of 50
mutating calls per second (`F0007`), so poll them less often than that.

## `ui.lobby_list`

`ui.lobby_list { entity?, action?, lobby? }`

`action` is `query` (the default), or one of what the widget's buttons send: `refresh`, `create`, `join` (needs `lobby`,
a decimal string or an integer), `leave`, `ready`, `unready` and `launch`. They go out as `LobbyListRequest` messages.
`regia_framework::game_ui` answers them from `regia_net::Lobbies`, the same state [`lobby.*`](net-lobbies.md) drives,
on a later frame, so watch the reply with `play.wait_until`. A failure shows in `status` and `error`, with the codes of
`lobby.*` (`lobby_full`, `lobby_not_found`, `not_owner`, `net_not_running`, ...).

```json
{"method": "ui.lobby_list", "params": {"action": "join", "lobby": "42"}}
```

Reply: `{ entity, rows: [{ id, name, detail, players, max_players, joinable }], current: { id, name, max_players, members:
[{ id, name, ready, owner, local }], is_owner, can_launch, launched } | null, status, error, manual, driver, sent }`.
`status` is `idle`, `busy` or `error`. An error stays until the next request or a success; the periodic lists do not
clear it, and only a list that fails reports through it. `driver` names what answers requests; `manual` means the game fills the rows itself.
While `current` is set the widget shows the lobby instead of the list. Lobby ids are 64-bit, so they are strings.

## `ui.scoreboard`

`ui.scoreboard { entity?, action?, column?, columns?, rows? }`

`query` (the default) reads the board. `sort` orders by `column`, and the same column again flips, as a heading click
does. `show` and `hide` set `visible`; a board also shows while its `hold_action` is held. `set` writes `columns`
(`[{ key, title?, width? }]`, at most 8) and/or `rows` (`[{ id, name?, team?, local?, values? }]`, values being integers
or strings, at most 1024) and stops the board following the rules (`from_rules: false`).

Reply: `{ entity, title, shown, visible, hold_action, from_rules, group_by_team, sort: { column, descending } | null,
columns, lines, dropped }`. `lines` is the draw order: `{ kind: "team", team, name, totals }` and `{ kind: "player", id,
name, team, local, values }`. Sort ties go by name, then id, so every client shows one order. The board draws at most 128
lines; `dropped` says how many did not fit. A `set` or `sort` answers with its own result; `shown` and `dropped` update
on the next frame.

## `ui.radar`

`ui.radar { entity?, action?, range?, world_rect?, rotate?, center?, layer_mask?, max_blips? }`

`query` (the default) reads it; `set` changes it: `range` makes it a radar of that many metres, `world_rect` (`[min_x,
min_z, max_x, max_z]`) a minimap of that area, with a rectangular outline; giving both is an error. `max_blips` is 0 to 1024.

Reply: `{ entity, mode, range, world_rect, shape, rotate, has_map, center, heading, size, blips: [{ source, node, label,
world, pos, inside }], dropped, default_max_blips }`. `pos` is the blip's center in UI px from the widget's top-left, `world`
its `[x, z]`, `inside` false for a blip pinned to the rim. North (world −Z) is up unless `rotate` turns it: `heading` is in
radians clockwise from north. The nearest blips win when there are more than `max_blips`; `dropped` counts the rest. `blips` is in entity order. The
entity at a radar's center has no blip; on a minimap it has one like any other.

## `ui.world_markers`

`ui.world_markers { entity?, action?, target?, position?, label?, color?, size?, offset?, clamp_to_edge?, show_distance?,
fade?, icon?, plate? }`

`query` (the default) reads the markers of a marker layer. `add` puts a `WorldMarker` on `target`, or on a new anchor at
`position`. `plate` (`{ size?, ppm?, fade?, depth_test? }`) makes it a world-space name plate instead: billboarded and
depth-tested, it shows in `plates`, and [`ui.targets`](ui.md) lists its panel. `remove` and `clear` take markers away;
an anchor made by `add` goes with its last marker.

```json
{"method": "ui.world_markers", "params": {"action": "add", "position": [0, 1, -10], "label": "Objective", "clamp_to_edge": true}}
```

Reply: `{ entity, camera, size, markers: [{ source, node, pos, on_screen, clamped, behind, distance, alpha, visible }],
plates: [{ root, source, position }], added }`. `pos` is in UI px on the layer. A marker off screen is pinned to the edge
along its bearing with `clamp_to_edge` (`clamped`), and hidden without it (`visible: false`). `behind` means behind the
camera. A layer with no camera shows nothing and says so with `camera: null`.

## `ui.target_bracket`

`ui.target_bracket { entity?, action?, target?, position?, radius?, speed?, gravity?, velocity?, shooter?, shooter_velocity?,
label?, color? }`

`query` (the default) reads the brackets. `lock` puts a `TargetLock` on `target` or on a new anchor at `position`, `release`
takes it away. `radius` is the target's size in metres, `speed` the muzzle speed in m/s (without it no lead is computed),
`gravity` the drop in m/s². `velocity` and `shooter_velocity` default to estimates from the motion. `shooter` defaults to the
`HudFocus` entity, else the camera.

```json
{"method": "ui.target_bracket", "params": {"action": "lock", "position": [0, 1, -100], "speed": 300, "velocity": [10, 0, 0]}}
```

Reply: `{ entity, brackets: [{ target, node, center, half_px, on_screen, distance, velocity, lead_world, lead_screen,
time_to_intercept, no_solution }], locked }`. `lead_world` is the point to aim at, `lead_screen` where the pip shows (`null`
when that point is off screen), `time_to_intercept` the flight time in seconds. The pip stays while the target itself is off screen (`on_screen: false`):
then only the frame and its label hide. `no_solution` means the projectile cannot
catch the target. The projectile inherits the shooter's velocity; gravity is countered by raising the aim point `½ g t²`.

### Every parameter is checked, under every action

`ui.world_markers` and `ui.target_bracket` read and check all of their parameters before the action runs, `query`
included (ADR-5957). A key the method does not take (`lable`, or `plate.sise` inside `plate`) or a value of the wrong JSON
type is refused with the generic `F0002` and `data.param` naming it; a value of the right type that `add` or `lock` would
refuse (`size: -1`, a two-number `position`, `color: "red"`) is refused with that action's code, even when the action is
`query`, `remove`, `clear` or `release` and never uses it. The server's `timeout_ms` is accepted on every call. A refused
call changes nothing: no anchor is spawned and no marker or lock moves.

## Errors

| Code | Class | Meaning |
|---|---|---|
| `bad_action` | invalid params | The method has no such `action`. |
| `bad_param` | invalid params | A parameter has the wrong shape or range; the message names it. |
| `bad_entity` | invalid params | `entity` is a number that is not entity bits (a value that is not a non-negative integer is the generic `F0002` with `param: "entity"`). |
| `bad_color` | invalid params | `color` is not `"#RRGGBB"`, `"#RRGGBBAA"` or a linear RGBA array. |
| `unknown_entity` | not found | The named entity does not exist. |
| `no_game_widget` | not found | No such widget in the app, on the node, or on the target. |
| `no_lobby_driver` | not found | Nothing answers lobby list requests: add `regia_net::LobbyPlugin`. |
| `no_world_position` | invalid params | The target has no `GlobalTransform`. |

## Testing with them

`samples/net_game/tests/hud.rs` drives all five in a running match over the real agent protocol. A pixel check needs a
real frame: `seeing.screenshot { "target": "offscreen", "window_layers": true }` draws the HUD, and
`samples/net_game/tests/render_smoke.rs` compares it with the same frame without the UI layers, at the places these methods
report, so it needs no golden image.
