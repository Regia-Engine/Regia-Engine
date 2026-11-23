# XR snap-turn hysteresis regression recipe

This recipe covers the internal `SnapTurn` primitive and its existing simulator surface. It adds no
agent method. For an application with `regia_xr` installed, use the existing calls documented in
[XR (OpenXR seam)](xr.md):

1. Call `xr.drive { "action": "session_state", "state": "focused" }`.
2. Call `xr.drive { "action": "input", "name": "turn", "sub": "right", "value": [0.6, 0] }`, then inspect
   `xr.get.actions` and `xr.get.snaps` after the fixed update. The exact threshold is inclusive.
3. Continue holding at or above threshold and observe the snap count stays unchanged. Move below
   `0.3`, then hold the opposite direction at `-0.6`; inspect the count and rig yaw in `xr.get`.
4. An undeclared input name returns the existing lowercase error code `xr_action_unknown`. The
   simulator-only input drive on a non-simulator backend returns `xr_simulator_only`.

The integration test `comfort_hysteresis_model` exercises the `SnapTurn` and `rig_turn` primitives
directly without a headset or runtime loader. These primitives are internal-only behavior (they have
no dedicated agent method); the agent recipe above checks the already exposed simulator actions.

The independent table consumes cooldown at the start of each fixed step. While latched, a sample
strictly below the rearm magnitude clears the latch. A snap fires only while unlatched, at or above
the threshold, and once the cooldown is zero. Each trace uses a fresh instance and constant,
validated settings. Position is compared within `1e-5` meters; composed head orientation uses a
quaternion dot error tolerance of `2e-6`.
