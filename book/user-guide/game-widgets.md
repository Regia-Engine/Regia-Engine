# Game widgets

Most multiplayer and action games need the same few screens. `regia_ui::game_widgets` has them ready: a lobby list, a
scoreboard, a radar and minimap, world markers, and target brackets with lead. Each is a component on a UI node, or a
`.ui.ron` widget for designers, and each takes its data from the game. They draw with the normal theme colors. The net
game template uses all of them: start from `regia new --template net-game`, or read `samples/net_game/src/hud.rs`.
For agents: [Game widgets (agents)](../agents/ui-game-widgets.md).

## In the UI designer

The designer's preset list has `lobby_list`, `scoreboard`, `radar`, `minimap` and `marker_layer`. The preview fills them
with invented rows, so you can see the layout before there is a match. In a `.ui.ron` file they look like this:

```ron
(
    version: 10,
    root: (
        children: [
            (widget: marker_layer(())),
            (
                style: (size: (Points(200.0), Points(200.0))),
                widget: radar((range: 75.0)),
            ),
            (
                style: (size: (Points(240.0), Points(240.0))),
                widget: radar((world_rect: (-50.0, -50.0, 50.0, 50.0), map_image: 77)),
            ),
            (
                widget: scoreboard((
                    title: "Scores",
                    columns: [(key: "tags", title: "Tags", width: 64.0)],
                    hold_action: "Scores",
                )),
            ),
            (
                style: (size: (Points(520.0), Points(360.0))),
                widget: lobby_list((title: "Matches", create: "loc:lobby-create")),
            ),
        ],
    ),
)
```

A word in a label may be `"loc:<key>"`, a localization key. `docs/formats/ui-ron.md` lists every field. A map image is an
image id, as in the `image` field of any node.

The game does three things for these to come alive: it puts `HudFocus` on the player's pawn, `RadarBlip`, `WorldMarker`
or `TargetLock` on what should show, and returns standings from its rules.

## Scoreboard

Rules return their standings from one method, and a board shows them. The rules read whatever holds the scores: on a client,
the replicated components.

<!-- ignore-reason: a fragment of a game's rules; Player and Stats are the game's own components -->
```rust,ignore
impl GameRules for MyRules {
    fn on_player_joined(&mut self, _: &mut RulesCtx, _: Entity) -> Option<SpawnRequest> { None }

    fn scoreboard(&mut self, ctx: &mut RulesCtx) -> Option<ScoreSheet> {
        let mut players = ctx.world.query::<(&Player, &Stats)>();
        Some(ScoreSheet {
            columns: vec![ScoreSheetColumn::new("kills", "Kills")],
            rows: players
                .iter(ctx.world)
                .map(|(p, s)| ScoreSheetRow {
                    id: p.id,
                    name: p.name.clone(),
                    team: Some(p.team),
                    local: p.local,
                    values: vec![ScoreCell::Int(s.kills)],
                })
                .collect(),
            teams: Vec::new(),
        })
    }
}
```

A board groups teams with their totals (`group_by_team`), sorts by a column (a click on a heading sorts, again flips), and
breaks ties by name, then id, so two clients show one order. `Scoreboard::held("Scores")` shows it while that input action
is held. A game without `GameRules` sets `from_rules: false` and writes `table` itself.

A game on the match core writes no `scoreboard` method: `MatchSessionRules` (on the server) and `ReplicatedMatchRules`
(on a client) already return one row per player with kills, deaths, assists, objective points and score, under each
team's total ([Game modes](game-modes.md#server-authoritative-games)).

## Lobby list

With `regia_net`'s `LobbyPlugin`, a `LobbyList` shows the compatible lobbies and refreshes itself. Its rows join on a click.
In a lobby it shows the members and their ready marks, a ready button, a leave button and, for the owner, a start button
that opens once all are ready. `LobbyListBridge` holds the choices: the data keys it shows as name and detail, the query, how
often to refresh, and where "start match" sends everyone (`server`; without one the app's `NetSession` hosts). A game with
its own server browser sets `manual: true`, writes the rows, and reads `LobbyListRequest`.

## Radar and minimap

<!-- ignore-reason: lines from a HUD setup system; hud, pawn, enemy and MAP_IMAGE are the game's own -->
```rust,ignore
spawn_radar(&mut commands, hud, Radar::with_range(75.0), [200.0, 200.0]);
spawn_radar(&mut commands, hud, Radar::minimap([-50.0, -50.0, 50.0, 50.0]).with_map(MAP_IMAGE, [0.0; 4]), [240.0, 240.0]);
commands.entity(pawn).insert(HudFocus);
commands.entity(enemy).insert(RadarBlip { clamp_to_edge: true, ..RadarBlip::dot(RED) });
```

North is world −Z, at the top. A radar centers on `follow`, else the `HudFocus` entity, else `center`; `rotate: true` turns
it so the player's heading points up. With a `map`, a radar shows a window of the image that scrolls with the center and a
minimap the whole image. The UI has no rotated image, so a radar with a map stays north up. Blips come from `RadarBlip`
components; `layer` and `layer_mask` choose which radar shows which. When there are more than `max_blips` the nearest win.
The entity at a radar's center draws no blip of its own; on a minimap it draws one like any other. A round radar with a map
needs a square node, or the image stretches. In `.ui.ron`, a radar with no `size` is 200 px square and a `marker_layer` with
no `size` fills its parent.
Moving blips cost no layout pass.

## World markers

A `MarkerLayer` node over the screen shows every `WorldMarker` where its entity is, through the same camera the frame draws
with. With `clamp_to_edge` an off-screen marker pins to the screen edge along its bearing, also when it is behind the camera.
`fade` fades it with distance; `offset` lifts it above the head. A layer under `UiRoot::camera` follows that camera, for
split screen.

A marker with `plate: Some(WorldPlate::default())` is instead a name plate in the world, on the world-space UI: it faces the
camera and scene geometry can hide it. Each plate has its own texture, so use them for a few things, and overlay markers for
many.

## Target brackets with lead

A `TargetLock` frames its entity with four corners sized from `radius_m`, never smaller than `min_px`, with a label and the
distance. With a `projectile_speed` it also draws a pip where to aim, so a projectile fired now meets the target. Velocities
come from `velocity` and `shooter_velocity`, or are estimated from the motion over about a tenth of a second. The projectile
inherits the shooter's velocity, and `gravity` raises the aim point. A shot that cannot catch the target draws no pip.

## Limits

A board draws at most 128 lines and 8 columns, a list 64 rows, a radar 1024 blips, a layer 256 markers and 32 brackets. Past
those the nearest (or the first in order) win. All of it runs only while such a widget exists.
