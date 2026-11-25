# Match rules

<!-- index: Match rules -->
<!-- index: Rule set -->
<!-- index: Playlist -->

Panel id: `match_rules`. The **Match Rules** tab edits a project's `.rules.ron` rule sets and `.playlist.ron` playlists, and play-in-editor uses those files when a match runs. The level's Game Rules picker lives on [World settings](world-settings.md) and reads the same library. Depth: [Match rules in the editor](../../agents/editor-match-rules.md), [Authoring modes](../game-modes.md).

The tab is not in the built-in dock. **View ▸ Reset layout** closes it. On [World settings](world-settings.md), **Edit rules…** opens it. `editor.rules_open` opens the tab and the file. An agent can also add the tab with `editor.layout_apply {"open": "match_rules"}`.

Rule sets and playlists are files. Their edits sit on the editor undo stack and do not mark the level dirty.

## Edit a rule set

1. Under **Rule set**, type a project path that ends in `.rules.ron` and press **Open or create**. Create starts from the file stem, a skeleton (`tdm`, `ffa`, `ctf`, `koth`, `dom` or `custom`) and an optional parent.
2. If the open rule set has unsaved edits, the tab asks you to save or discard before replacing it. An agent passes `discard: true` or gets `match_rules_dirty`.
3. The form lists the file fields (`id`, `name_key`, `skeleton`, `extends`, `teams`, `win_conditions`, `options`) and then each session section (`flow`, `phases`, `objectives`, `teams`, `respawn`, `zones`). An empty field inherits the parent, or the engine default at the root.
4. Problems appear on the rows (`ruleset_option_range`, `ruleset_unknown`, `ruleset_cycle` and the other rule-set codes). **Save** is refused while one is listed (`match_rules_save_blocked`) and writes nothing.
5. **Save** writes the file by a temporary file and a rename, then reloads the project library so the Game Rules picker lists the id. A rule set with no edits is not rewritten, so comments and layout stay.

## Edit a playlist

1. Under **Playlist**, open or create a `.playlist.ron`.
2. **Add row**, remove, and reorder. Each row names a map, a mode and options. A row whose mode names no rule set and no skeleton is a warning (`playlist_mode_unknown`), not a save blocker: that mode belongs to the game.
3. A row option the schema does not declare, or a value out of range, blocks the save (`playlist_save_blocked`). An empty playlist cannot be saved.

## Game Rules on the level

World Settings shows `LevelSettings.game_rules` as a combo: registered rules types (including `DefaultRules` and `MatchSessionRules`, plus a game crate's types after one play session) and the project's rule sets. A name in neither list stays as typed and is marked custom. The warning code is `game_rules_unregistered`. On load, that level uses `DefaultRules`.

## Play

While a play session runs, the match controls start, pause, resume, step and end the match, add or remove a bot seat, and advance the playlist row. With no session the control answers `match_control_not_playing`. The same actions are `editor.match_control`.

## Actions

| Action | Control | Agent method |
|---|---|---|
| Read the panel | The tab | `editor.match_rules_state` |
| Open or create a rule set | **Rule set**, **Open or create** | `editor.rules_open` |
| Edit one field | The form | `editor.rules_set_field` |
| Save the rule set | **Save** | `editor.rules_save` |
| Edit the playlist | **Playlist** | `editor.playlist_edit` |
| Drive the match in play | The match strip | `editor.match_control` |

## Keyboard

The path fields and buttons are focusable. Undo is Ctrl+Z, including rule-set and playlist edits.

## Known limits

- A `.rules.ron` over 256 KiB, or a file that does not parse, is left out of the library. `editor.match_rules_state` lists those files under load errors. They do not appear in the picker.
- A field the form cannot represent (`match_rules_field_unsupported`) has to be edited as text.
- Playlist rows stop at 1024. A playlist file over 256 KiB is not opened.
- This tab does not place a public match, a lobby or a relay. It authors the files and drives the match inside play-in-editor.

## See also

[World settings](world-settings.md), [Play-in-editor](play-in-editor.md), [Net](net.md), [Editor guide](index.md).
