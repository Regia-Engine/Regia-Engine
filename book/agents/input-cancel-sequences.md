# Input cancellation sequences (hardware-free replay)

This page records the deterministic CPU replay for multi-pointer cancellation. It uses only
`TouchInput` and focus messages in a headless `App`; it needs no Android device, window server,
or GPU.

## Replay

Run the focused regression and the complete crate suite:

```sh
cargo test -p regia_input --test device_batches cancel_then_duplicate_end_does_not_turn_into_a_tap
cargo test -p regia_input --test device_batches independent_pointer_cancellation_reuse_and_focus_resume_have_exact_action_edges
cargo test -p regia_input
```

The device batch replay tracks two IDs at once, moves the surviving pointer while the other is
cancelled, checks the cancelled stick action's `Completed` edge while the button action remains
pressed, cancels all pointers, reuses an ID after its terminal frame, then sends focus loss and
focus regain before a new touch. The duplicate-end replay sends `Cancelled`, `Ended`, and another
`Ended` in one batch and requires the tracked phase to remain `Cancelled`; it also checks that no
tap gesture is emitted.

`android_map` pure tests cover NaN, infinity, coordinates outside `[-1, 1]`, tracked versus
untracked `Moved`, other phases, malformed physical-key strings, overflowing numeric strings,
unknown Android keycodes, and the terminal reset of an untracked joystick report to `(0, 0)`.
Refused events return `None`; an unknown key or non-axis event is not guessed to be a joystick.
The exercised values flow through the normal `InputPlugin` update and action evaluation before
state assertions.

The existing agent read call is `input.touch_controls {}`. It reports the installed zones,
claims, held actions and tracked fingers; this lane adds no method or error code. The response
has no error for the valid empty-parameter request. This page documents the existing surface;
the replay tests themselves are local CPU tests rather than an agent RPC session.

## Result

On the pre-fix replay, cancellation followed by an end in the same batch changed the tracked
phase from `Cancelled` to `Ended` (assertion failure: `left: Some(Ended)`, `right:
Some(Cancelled)`). Cancellation now remains terminal for that batch, so a delayed end cannot
turn it into a tap. The Android mapping release test also failed before the fix with `left: None`
for an untracked cancelled joystick event; terminal reports now map to a centered stick so the
last non-zero axis cannot remain held. Assertions use exact action phases and pointer states over
manual frame steps, with no wall-clock waits. Touch mouse emulation also checks the `just_released`
edge after the last active pointer cancels.
