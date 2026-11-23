# Inertialized transitions

A state-machine transition can **cross-fade** (the default) or **inertialize**. A cross-fade
evaluates the old and the new state for the whole `duration` and blends them. A short cross-fade
between two different poses still pops: the blend moves the pose, and its velocity, too fast. An
inertialized transition switches to the new state at once and evaluates only that state, at full
weight. At the switch it records, per joint, how far the shown pose is from the new state's pose
and how fast that gap is changing. It adds this offset back on top of the new state and decays it
to zero over `duration`. Nothing jumps, the motion keeps its momentum, and while the transition
runs it costs one state evaluation instead of two.

## Turning it on

Set `blend` on a transition. `duration` becomes the decay time:

```ron,animgraph
(
    format_version: 2,
    parameters: [("Air", Bool(false)), ("Dead", Bool(false)), ("FireWeight", Float(0.0))],
    root: Layer((
        base: StateMachine((
            initial: "Loco",
            states: [
                ("Loco", Clip((clip: "run", looping: true, speed: 1.0, sync_group: None))),
                ("Air", Clip((clip: "jetpack_hover", looping: true, speed: 1.0, sync_group: None))),
                ("Land", Clip((clip: "land", looping: false, speed: 1.0, sync_group: None))),
                ("Death", Clip((clip: "death", looping: false, speed: 1.0, sync_group: None))),
            ],
            transitions: [
                (from: Some("Loco"), to: "Air", conditions: [Bool(param: "Air", value: true)],
                 duration: 0.15, curve: Smooth, interruptible: true, blend: Inertialize),
                (from: Some("Air"), to: "Land", conditions: [Bool(param: "Air", value: false)],
                 duration: 0.15, curve: Linear, interruptible: true, blend: Inertialize),
                (from: Some("Land"), to: "Loco", conditions: [ClipFinished],
                 duration: 0.2, curve: Smooth, interruptible: false),
                (from: None, to: "Death", conditions: [Bool(param: "Dead", value: true)],
                 duration: 0.2, curve: Linear, interruptible: true, blend: InertializeSpring),
            ],
        )),
        layers: [(
            graph: Clip((clip: "aim_fire", looping: true, speed: 1.0, sync_group: None)),
            mask: (joints: ["spine_01"], include_children: true),
            mode: Override,
            weight_param: "FireWeight",
            weight_inertialize: 0.15,
        )],
    )),
)
```

| `blend` | What it does |
|---|---|
| `Crossfade` (default; also what an omitted `blend` means) | Blends both states with `curve` over `duration`. |
| `Inertialize` | Only the target state plays. The offset decays with a quintic polynomial (David Bollo, GDC 2018). It ends exactly at `duration` and never overshoots. If the old pose was moving *away* from the new one, that part of its velocity is dropped, so the pose never swings further out than either state. |
| `InertializeSpring` | Only the target state plays. The offset decays as a critically damped spring (Daniel Holden's spring inertialization). Velocity carries over completely, even when it points away. The spring can overshoot a little and leaves < 0.5 % of the offset at `duration`, which is dropped. |

`curve` is ignored by the inertialized modes. Good starting points are 0.15–0.3 s. An
inertialized transition can be longer than the cross-fade it replaces, because the new state is
already fully in charge from the first frame.

**Interruptions.** When another transition fires while one is running, an inertialized
transition starts again from the pose that is on screen and from its velocity, so an interrupted
transition does not pop either. A cross-fade that interrupts an inertialized transition keeps the
old offset decaying on top. A cross-fade that interrupts a cross-fade still drops the old source
state (the v1 behaviour).

**Layer weights.** `weight_inertialize` (seconds, default 0) on a layer eases weight changes the
same way. When gameplay snaps `FireWeight` from 0 to 1, the upper body moves into the aim pose
over 0.15 s, continuing whatever motion it had, instead of snapping. With 0 the weight follows the
parameter directly.

## What else to know

- **Everything after the state machine still applies.** The offset is added to the state
  machine's output, so layers with bone masks, two-bone IK, foot IK, look-at and root motion work
  unchanged. A `BlendSpace2D` with a sync group can be a target state.
- **Only the target state runs during the transition.** It fires the notifies and curves and
  supplies the root motion from the first frame. The old state stops at the switch.
- **Old files load unchanged.** Version 1 files have no `blend`, so they load as cross-fades
  through the 1 → 2 migrator. Regia Engine writes `blend` and `weight_inertialize` only when they are
  not the default.
- **Cost.** A machine that has at least one inertialized transition copies its output pose once a
  frame (the velocity history). On the switch frame the target state is evaluated twice, once as
  a side-effect-free probe one frame back, to get its velocity. While the transition runs, only
  one state is evaluated.

## In the editor and for agents

The graph editor's transition inspector has a **Blend** row (Crossfade / Inertialize /
Inertialize (spring)). The state-machine canvas labels inertialized arrows `inertial`, and spring
ones `inertial~`. Layers have a **Weight inertialize (s)** slider. During play-in-editor, the debug
overlay and `anim.get_state` show the running transition's mode and the offset still decaying
(`book/agents/inertialization.md`).

Method and sources: `docs/provenance.md` ("Inertialized state-machine transitions"), ADR-203.
