# Cutscenes

A cutscene is a [sequence player](sequence-player.md) played with the game around it held:
gameplay input locked, the HUD hidden, letterbox bars, a skip action, and a hand-back to gameplay
at the end. Code: `regia_framework::sequencer::cutscene` (ADR-2948). The actors a cutscene needs can
come from the sequence itself: see *Spawnable bindings* on the sequence player page.

## The flow

1. **Start.** `cutscene.start` (or `start_cutscene` / `CutsceneCommand::Start` from Rust) saves the
   enabled input contexts of each listed player and leaves only the cutscene context
   (`Cutscene` by default) enabled, releases toggle latches, hides the UI nodes tagged `HudLayer`
   with a listed name (`"*"`: every tagged node; each gets its own display back later), and plays
   the player from the start. A sequence with spawnables pre-rolls first (phase `preroll`).
2. **Play.** The letterbox bars slide in over `letterbox_secs` on a UI root of their own (sort
   order 900). Bind the skip action (`cutscene_skip` by default) in the cutscene context.
3. **Skip.** The skip action skips on a press (a button already held when the cutscene starts must be let go first), or once held for `skip.hold_secs` (`skip_held` in
   the state shows the progress). With `skip.fade_secs` the picture fades to black (phase
   `skip_fade_out`), the jump happens at full black, and it fades back in. The jump moves the player
   to its last frame: every marker in between arrives, in the order playback would have fired it,
   as `MarkerReached { skipped: true }`; the spawnables of the last frame exist and the others are
   gone; the last values are applied; running dialogue lines stop (`dialogue::stop_line`) and the
   subtitle queue is cleared. So gameplay that reacts to markers ends in the same state as if the
   cutscene had played through.
4. **Hand-back.** When the player finishes (played or skipped), it is stopped: the camera takeover
   blends back to gameplay (over `hand_back_blend` when given), spawnables without `keep_on_stop`
   are despawned and, with `restore_on_stop`, bound properties get their prior values. Input and
   the HUD come back on that frame (phase `hand_back`), the bars slide out, and `CutsceneEnded`
   goes out.
5. **Overlap.** Two cutscenes may run at once (on two players). The first one to lock a player's
   input or hide a HUD node saves what was there; the last one to end gives it back.
6. **Stop.** `cutscene.stop` ends it at once, also during the pre-roll: everything above comes
   back and `CutsceneEnded { stopped: true }` goes out. Stopping or despawning the player from
   elsewhere does the same.

## Methods

```json
{ "method": "cutscene.start", "params": { "player": 4294967301, "letterbox": 0.1,
  "hide_hud": ["hud"], "skip": { "hold_secs": 0.5, "fade_secs": 0.3 }, "hand_back_blend": 1.0 } }
{ "method": "cutscene.skip", "params": {} }
{ "method": "cutscene.stop", "params": {} }
{ "method": "cutscene.state", "params": {} }
```

| Parameter (`cutscene.start`) | Default | Range |
|---|---|---|
| `player` | the only sequence player | an entity with a `SequencePlayer` |
| `lock_input` | `true` | |
| `players` | `[0]` | up to 8 local player numbers |
| `input_context` | `"Cutscene"` | non-empty |
| `letterbox` | `0.12` | `0..=0.45` of the screen height per bar; `0` draws none |
| `letterbox_secs` | `0.4` | `0..=60` |
| `hide_hud` | `["*"]` | up to 64 `HudLayer` names |
| `skip` | `{ enabled: true, action: "cutscene_skip", hold_secs: 0, fade_secs: 0 }` | times `0..=60` |
| `hand_back_blend` | the player's `blend_out` | `0..=60` |
| `stop_dialogue_on_skip` | `true` | |

The state (`cutscene.state { player? }`, also returned by the other three) has `running`,
`phase` (`preroll`, `playing`, `skip_fade_out`, `hand_back`), `skipped`, `skip_held`, `fade`
(`0` clear to `1` black), `letterbox` (`0..=1` of the way in), `input_locked`, `hud_hidden`,
`start_tick` (net starts), and the player's `time`, `duration`, `status`, `prerolling` and
`markers_skipped`. Without `player` it lists every running cutscene with the `started` and
`skipped` counts. `cutscene.skip` adds `began` (`false` when it was already skipping).

Error codes: `cutscene_no_player`, `cutscene_running`, `cutscene_not_running`,
`cutscene_skip_disabled`, `cutscene_bad_settings`, `cutscene_several`, `bad_entity`.

## Multiplayer

Actors are not replicated: every peer plays the cutscene on its own player.
`NetCutscenes` (a resource) maps a name to the local player:

<!-- ignore-reason: `NetCutscenes` is behind `regia_framework`'s `net` feature, which the book's compile check does not enable; `world` and `player` come from the game -->
```rust,ignore
world.resource_mut::<NetCutscenes>().register("intro", player, CutsceneSettings::default());
```

A start names a tick; each peer starts the cutscene on that tick of its session clock
(`NetCutscenes::set_tick`, written by the net glue). A peer that is already past the tick (a late
joiner) skips to `(tick - start) / tick_hz` seconds, with the markers before it delivered as
skipped, and lands where the others are. Each start (name and tick) plays once per peer, however
often it is announced. With the `net` feature, add
`regia_framework::sequencer::cutscene::net::NetCutscenePlugin` on every process of the session (it
registers the `CutsceneStart` message, which is part of the protocol); the server calls
`NetCutsceneServer::announce(name, start_tick)`, every peer gets the start, and peers that join
while it is announced get it too (`NetCutsceneServer::end` stops that).

## From Rust

<!-- ignore-reason: statements of a game system (`world` and `player` come from it); the book's compile check takes items only -->
```rust,ignore
use regia_framework::sequencer::{CutsceneSettings, SkipSettings, start_cutscene};

start_cutscene(world, player, CutsceneSettings {
    skip: SkipSettings { hold_secs: 0.5, fade_secs: 0.3, ..SkipSettings::default() },
    ..CutsceneSettings::default()
})?;
```

Read `CutsceneEnded` to continue the game, and `MarkerReached::skipped` to tell a marker that was
skipped over from one that was shown.
