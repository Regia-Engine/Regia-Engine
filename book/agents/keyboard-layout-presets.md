# Keyboard layout presets

`input.keyboard_layout` reads a preset's 30 letter-block labels, physical codes and
positional movement source. It requires `read` permission and works without an
`InputPlugin` or window. It does not change the host's bindings.

```json
{"method":"input.keyboard_layout","params":{"layout":"colemak"}}
```

The result has `layout` (canonical lowercase name), `layouts` (`qwerty`, `colemak`,
`azerty`), `keys` (30 `{label, physical_key}` rows in physical row order), and
`movement` (`{"Keys2D":{"up":"KeyW","down":"KeyS","left":"KeyA","right":"KeyD"}}`).
Names and ASCII letter labels ignore case; physical codes are case-sensitive.

| Preset | Up / down / left / right labels | Example label → physical code |
|---|---|---|
| `qwerty` | W / S / A / D | E → `KeyE` |
| `colemak` | W / R / A / S | F → `KeyE`, N → `KeyJ`, ; → `KeyP` |
| `azerty` | Z / S / Q / D | Z → `KeyW`, Q → `KeyA`, M → `Semicolon` |

Colemak means the original layout, not Colemak-DH. AZERTY means classic French,
not Belgian or revised French AZERTY. The utility covers the main letter block
only. Digits, modifiers, Backspace, CapsLock, function keys, shifted characters and
AltGr remain explicit physical bindings. In particular, it does not implement the
optional Colemak CapsLock-to-Backspace OS remap. Text entry still uses the platform's
logical key and text-input events.

Errors: `keyboard_layout_bad_params` for a missing, non-string or unknown layout,
or extra parameters. The dispatcher can also reject an overlong layout name
(limit: 32 bytes) with the universal `F0002` error. No fallback silently selects
another layout.

## Construct and verify a binding

Hosts use `regia_input::KeyboardLayout::{Qwerty,Colemak,Azerty}`. `physical_key`
returns an `Option<KeyCode>` for a label; `key_label` returns the preset label for
a physical key. `keys` enumerates the same pairs, `movement_keys` constructs an
`InputSource::Keys2D`, and `ALL`, `name`, `parse` support a host's layout selector.

```rust,wrap
use regia_input::{ActionKind, Binding, InputConfig, KeyCode, KeyboardLayout};

let layout = KeyboardLayout::Colemak;
let input = InputConfig::new()
    .action("Move", ActionKind::Axis2D)
    .binding(Binding::new("Move", layout.movement_keys()));
assert_eq!(layout.physical_key("F"), Some(KeyCode::new("KeyE")));
```

For an existing host, read the preset, then use physical codes in the established
[rebinding workflow](input-rebind.md): start `input.rebind` for the intended action
and feed `play.inject_input` physical key events. `input.rebind_state` verifies the
stored physical binding. For example, Colemak F uses
`{"type":"key","key":"KeyE","text":"f","state":"pressed"}`; send its release
as `key: "KeyE"` too. `play.wait_until` and `play.expect` can then verify the action's
host-visible effect as described in [Testing](testing.md). Existing custom
bindings and saved profiles are never rewritten merely by querying a layout.

Automated checks: `regia_input --test keyboard_layout_presets` pins the mappings,
round trips, invalid labels and physical press/release behavior with changing
logical labels; `regia_agent --lib keyboard_layout` exercises the real dispatcher
and malformed requests. No screenshots or golden images are needed for this query.
