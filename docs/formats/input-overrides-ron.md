# `input_overrides.ron` and input profiles (format_version 2)

Owner: `regia_input::rebind` (`InputOverrides`). The player's rebound bindings, kept in `user://input_overrides.ron`
(`OVERRIDES_PATH`) and applied over the game's `InputConfig`. Named profiles (`user://input_profiles/<name>.ron`,
`save_profile` / `load_profile`) are the same struct.

```ron
(
    format_version: 2,
    contexts: {
        "gameplay": {
            "jump": (action: "jump", input: Key("KeyJ")),
        },
    },
    slots: {
        "gameplay": {
            "fire": {
                Gamepad: [(action: "fire", input: GamepadButton(RightTrigger))],
            },
        },
    },
)
```

| Field | Meaning |
|---|---|
| `format_version` | 2. Absent reads as 1; a newer file is refused (so it is not overwritten) |
| `contexts` | Context → action → one `Binding` that replaces every binding of the action |
| `slots` | v2: context → action → device slot (`All`, `KeyboardMouse`, `Gamepad`) → bindings that replace the action's bindings on those devices only; merged after `contexts`; omitted while empty |

A `Binding` is the `InputConfig` binding: `action`, `input` (`Key`, `Keys2D`, `MouseDelta`, `MouseButton`,
`GamepadButton`, `GamepadStick`, `GamepadAxis`), optional `modifiers` and `triggers`.

**Versioning.** v1 had no `format_version` and no `slots`; it is a subset of v2, so `InputOverrides::from_ron` reads it
as it stands and the next save writes v2. `regia upgrade-project --write` does the same for a project copy. Registered in
`cargo xtask check-formats` (`input-overrides`, the binding enums included) since ADR-2316.
