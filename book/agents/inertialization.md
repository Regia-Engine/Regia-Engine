# Inertialized transitions: author, observe, verify

Inertialized transitions are a per-transition `blend` mode of the anim-graph state machine
(`Crossfade` | `Inertialize` | `InertializeSpring`, `book/user-guide/inertialization.md`).

## Author

`editor.anim_transition` takes `blend` (default `Crossfade`). `duration` is the decay time:

```json
{"method": "editor.anim_transition", "params": {"machine": 1, "from": "Air", "to": "Land",
  "conditions": [{"Bool": {"param": "Air", "value": false}}], "duration": 0.15,
  "blend": "Inertialize"}}
→ { "index": 3, "transitions": [ { "from": "Air", "to": "Land", ..., "blend": "Inertialize" } ] }
```

Every transition in the response carries `blend`, including `"Crossfade"` (the file omits the
default). Switch an existing transition with `op: "set", index`. A layer's weight easing is the
layer's `weight_inertialize` field (seconds; edit it with `editor.anim_set_node` on the layer
node's `layers` data).

## Observe

`anim.get_state` has a `transition` object for the first state machine while a transition runs:

```json
{ "entity": 4294967310 }
→ { ..., "state": "Land",
    "transition": { "machine": "root/base", "from": "Air", "to": "Land", "progress": 0.4,
                    "blend": "inertialize", "inertial_offset": 0.21 }, ... }
```

`blend` is `crossfade`, `inertialize` or `inertialize_spring`. `inertial_offset` is the largest
joint rotation offset (radians) still added on top of the target state. It falls to 0 as the
transition settles. It can stay above 0 after a cross-fade interrupts an inertialized transition;
in that case `transition` has only `machine`, `blend` and `inertial_offset`. With no transition
running, `transition` is `null`.

In Rust and in the editor's preview, `regia_animation::debug::graph_debug` reports the same
values per machine: `NodeDebugState::StateMachine { blend, inertial_offset, .. }`. While a
transition is inertialized, the source state's node is inactive and has weight 0.

## Verify a transition

1. `anim.set_param { entity, name: "Air", value: false }`.
2. Step (`play.step`) and poll `anim.get_state` until `transition.blend == "inertialize"`, then
   until `transition` is `null` (settled after `duration`).
3. Capture frames across the switch (`seeing.screenshot` each step) and compare them by eye. An
   inertialized switch shows no single-frame jump.

The numeric proof lives in `crates/regia_animation/tests/inertialize.rs`. The test checks that
the velocity jump at the switch, including on interruption, shrinks with the frame time, while a
0.05 s cross-fade's does not. `regia_asset_pipeline`'s `inertialize_capture` example plots a
joint's angle and angular speed through walk → aim, run → land and run → death on DO27's pilot
rig, and times many characters with each mode.
