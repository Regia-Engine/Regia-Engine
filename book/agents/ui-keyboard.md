# UI on-screen keyboard (agents)

ADR-273 I11, ADR-1842. Code: `regia_ui::keyboard`.

The platform layer raises the Android soft keyboard, and enables the desktop IME, when the primary window's `ime_enabled`
flag turns on. The UI now sets that flag from focus: in `auto` mode (default) it is on while the focused node is a
`TextInput` and off otherwise, so tapping a text field brings the keyboard up and moving focus away dismisses it. `off`
never raises it (a console build with its own keyboard screen); `on` keeps it up. The flag is only written when it changes.

The IME policy is logic over focus and the primary window flag; it does not require a device. For a device check, use
the test Pixel 9 Pro (`adb -s 54101FDAP0015H`): focus a text field, then move focus to a button and confirm the flag
turns off.

## Methods

- `ui.keyboard { mode? }`: stores the mode when given and reports `{ mode, wanted, window_ime }` (`window_ime` is the primary
  window's flag, or `null` without a window). `wanted` and `window_ime` are the last sync
  `sync_on_screen_keyboard` applied. A mode change is stored on this call and applied on the next UI tick, so
  the reply can still show the previous `wanted` and window flag. Mutating; permission `edit_scene`.

```json
{"method": "ui.keyboard", "params": {"mode": "auto"}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_mode` | `mode` is not auto, off or on |

`mode` is an optional string; without it the call only reports (the policy starts in `auto`). `window_ime` is `null` when there is no primary window. The method
returns `wanted` separately from the platform window flag so a headless check can verify policy without a device.

## Failure example

Captured CLI output from `regia agent call ui.keyboard '{"mode":"sometimes"}'`:

```text
error: method call failed: `mode` must be auto, off or on (-32602): {"code":"bad_mode","hint":"auto follows text focus"}
```

## Workflow

Read the current policy with `regia agent call ui.keyboard`. Set `mode` to `auto`, focus a `TextInput`, and read again:
`wanted` should be true. Move focus to a non-text node and read once more; `wanted` should be false. Use `off` to keep a
game's own keyboard screen in control.
