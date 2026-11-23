# Radial menu

`regia_ui` has no per-widget protocol methods; an agent drives the wheel through the same input paths a
player uses, and reads the result from the wheel's component and its messages. User guide:
[Radial menu](../user-guide/radial-menu.md).

## Drive it

- **Open.** `RadialMenu::open()`, `set_held(true)` (hold mode), `toggle()` (toggle mode) or the `open_action`
  entry of `VirtualButtons`. In a running app, `play.inject_input` of the bound action does the last one.
- **Point.** Stick: `GamepadAxisChanged` for `RightStickX` / `RightStickY` (or write `RadialSticks`). Mouse:
  `UiPointer::cursor` (the wheel needs two cursor samples; the first only records the resting position). Touch:
  `UiPointer::touches` and `active_touch` (the drag is measured from the first sample).
- **Confirm or cancel.** Release the open control, lift the finger or button, or `UiNavActions` `submit`
  and `cancel` (gamepad `A` and `B`).

## Read it

`RadialMenu::selection` (`None`, `Center`, `Sector(n)`), `open`, `progress` (`0..=1`), `phase()`; the
`RadialEvent` message stream (`Opened`, `Hover(selection)`, `Select { selection, id }`, `Cancel`, `Closed`); and the sector chips,
which are `RadialChip` nodes under the wheel (count = shown sectors + centre).

To see it: open the `ui_gallery` sample's Wheel page (key `4`) and `seeing.screenshot` while the wheel is
held. Check it worked: with eight sectors and the stick at `(1, 0)` the selection is `Sector(2)`; at `(0, 0)`
it is `None` (or `Center` with a centre action); jittering across a boundary by less than 12 % of a sector never
changes it.

The pure math (`RadialGeometry::select`) needs no app: `select(current, [x, y])` returns the sector or `None`.

This UI has no dedicated agent method, so there are no protocol parameters or error codes to list.
Workflow: open the Wheel page in `ui_gallery`, inject the configured open action with
`play.inject_input`, inject the pointing input, then inspect the selection/message stream and capture
the open wheel.

Captured failing input call (lavapipe; `actions` must be an array):

```console
$ regia agent call play.inject_input '{"actions":"bad"}' --project samples/arena
error: method call failed: `actions` must be an array (-32602): {"code":"F0002","expected":"an array","hint":"check the method's parameters in the agent guide","param":"actions"}
```
