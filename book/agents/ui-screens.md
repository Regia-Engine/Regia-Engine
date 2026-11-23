# UI screens and page history (agents)

Guide topic: `agent.guide {"topic": "ui-screens"}`. Gap W18, ADR-273; integrated by ADR-1563.

A screen is any node with `UiScreen { id, mode, transition }`, usually a full-window
child of one `UiRoot`. `ScreenStack` is the history, bottom first. Only the top screen
is shown (`Display::None` hides the rest) and only the top receives input: it holds an
`InputLayer` with the screen's `mode`, so push/pop restores the previous visibility
and input mode through the layer stack. The control that had focus when that screen
was covered is focused again if it is still focusable and not disabled; otherwise the
layer's default focus stands.

<!-- ignore-reason: `ScreenError` does not implement `std::error::Error`, so the examples' `?` has no harness to run in -->
```rust,ignore
push_screen(world, title)?;
push_screen(world, options)?;
pop_screen(world, None)?; // back to title, focus and UiOnly restored
show_screen(world, title)?; // pops everything above title, or pushes it
```

`show_screen` returns whether the visible screen changed. Despawning a stacked screen
(or removing its `UiScreen`) prunes it on the next frame and focus recovers instead of
locking. `ScreenChanged { pushed, popped, top }` tells transition drivers what happened;
`screen_list(world)` is the read side (id plus `visible`, bottom first).

## Transitions

The switch itself is instant so layout is untouched. `ScreenTransition` (`instant`,
`fade`, `slide`) is metadata: read it on `ScreenChanged` and drive the animation with
`ui.tween` (for example a fade on `Opacity`). Reduced motion jumps to the end value.
Error codes are `unknown_entity`, `not_a_screen`, `already_on_stack`, `empty_stack`
and `not_on_top` (use `show_screen` to jump back).

## `ui.screens`

`ui.screens { action?, entity? }` (`play` permission) reads and drives the stack. The answer is
always the stack after the call: `screens` (bottom first, `entity`, `id`, `visible`), `depth`,
`top`, `top_id` and the `focused` node.

```json
{ "method": "ui.screens" }
→ { "screens": [ { "entity": 4294967310, "id": "title", "visible": false },
                 { "entity": 4294967312, "id": "options", "visible": true } ],
    "depth": 2, "top": 4294967312, "top_id": "options", "focused": 4294967313, "applied": false }
{ "method": "ui.screens", "params": { "action": "pop" } }
→ { "screens": [ { "entity": 4294967310, "id": "title", "visible": true } ], "depth": 1,
    "top": 4294967310, "top_id": "title", "focused": 4294967311, "popped": 4294967312 }
```

- `list` (the default) changes nothing and answers `applied: false`.
- `push` needs `entity` (a node with `UiScreen`); the previous top is hidden.
- `pop` removes the top screen (or `entity`, which must be the top) and answers `popped`.
- `show` needs `entity`: it pops back to a stacked screen or pushes a new one, and answers
  `changed` (false when it was already on top).

Errors (lowercase codes in `error.data.code`): `unknown_entity` (no such entity), `not_a_screen`
(no `UiScreen` on it), `already_on_stack` (push of a stacked screen: use `show`), `empty_stack`
(pop with nothing stacked) and `not_on_top` (pop of a buried screen: pop the top or `show` it).

## Recipes

- *Does Back return to the title with focus on Play?* `ui.screens { action: "push", entity:
  options }`, then `ui.screens { action: "pop" }`: `top_id` is `title` and `focused` is the Play
  button.
- *Wait for a menu to open*: `play.wait_until` cannot read the stack (it is a resource without
  reflection); poll `ui.screens` until `top_id` names the screen.


## Opt-in back handling

`ScreenStack::set_back_pop(true)` opts the stack into Escape/gamepad B popping. The default is
false. A `ScreenBack::Pop` or `ScreenBack::Ignore` component overrides the default for one screen.
Only the top screen owning the active input layer can pop; an overlaid modal, game-only input
mode, disabled screen or input-rebinding capture keeps the back press from popping it.
The existing pop path restores focus on the previous screen, skipping disabled descendants.
No `.ui.ron` field is introduced here; screen definition authoring belongs to the format lane.

`ui.screens` adds `action:"set_back"` with `back:"pop"|"ignore"`. With `entity`, this sets a
per-screen override; without it, it sets the stack default. `action:"back"` exercises the
active layer's policy and returns `changed` and `applied` (false when nothing popped).
The stack result now includes `back`; every screen row includes its override `back`, null when
it inherits the stack default. Existing entity errors still apply. Invalid `action` or `back`
uses the shared invalid-parameter error `F0002`.

```json
{"method":"ui.screens","params":{"action":"set_back","back":"pop"}}
{"method":"ui.screens","params":{"action":"set_back","entity":4294967312,"back":"ignore"}}
{"method":"ui.screens","params":{"action":"back"}}
```
