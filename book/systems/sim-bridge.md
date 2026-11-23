# Networking and the sim bridge

Some games keep their authoritative state outside the engine: a deterministic lockstep or
rollback simulation, a server-authoritative model, a simulation written as a separate library and
shared with a dedicated server. Regia Engine supports this as a first-class pattern, the **sim bridge**
(`regia_framework::sim_bridge`, `docs/specs/25-sim-bridge-and-do27.md` § 3): the simulation owns
the truth and its own clock; Regia Engine is the presentation layer that sends it input and draws
what it reports.

> **Networking status.** Regia Engine ships its own client-server networking crate, `regia_net`
> (since v0.2.5; replication, prediction, interpolation, lag compensation, lobbies and a
> dedicated-server host followed through v0.2.10). Start at [Networking](../user-guide/networking.md)
> and [Build a networked game](../tutorials/networked-game.md).
> The sim bridge is the other path: a game that keeps its own netcode inside an external
> simulation connects that simulation to the engine through the bridge; this is how
> *Direct Order 27* works. The two do not exclude each other.

## How it fits

Every frame, `SimBridgePlugin`:

1. reads the `BridgeInput` resource (movement axes and action edges your input systems wrote)
   and calls `ExternalSim::submit_input`;
2. calls `ExternalSim::read_snapshot` and **reconciles** entities: a new sim id spawns an entity
   (with `SimId`, `SimKind`, `Team` and whatever its kind registers), a known id gets its
   `Transform` and `SimExtras` rewritten, a vanished id is despawned;
3. calls `ExternalSim::drain_events` and sends each event as a `SimEvent<E>` message, and routes
   it through the data-driven `EventRouter`.

Nothing downstream of the bridge ever writes simulation state: authority stays with the sim. The
bridge copies the snapshot pose into `Transform`. `to_regia_pose` returns its inputs unchanged,
is not called by reconcile, and is not a hook. A sim converts inside `read_snapshot`.

## Implementing a sim

```rust
use std::time::Duration;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::sim_bridge::{
    BridgeInput, EventRouter, ExternalSim, KindRegistry, SimBridgePlugin, SimEntityView, SimEvent,
    SimSnapshot, SnapshotTick,
};

/// One drone in the snapshot.
struct Drone {
    id: u64,
    position: Vec3,
}

/// What the sim reports each frame.
#[derive(Default)]
pub struct DroneSnapshot {
    drones: Vec<Drone>,
}

impl SimSnapshot for DroneSnapshot {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for drone in &self.drones {
            f(SimEntityView {
                id: drone.id,
                kind: "drone",
                team: 0,
                translation: drone.position,
                rotation: Quat::IDENTITY,
            });
        }
    }
}

/// A toy authoritative sim: drones drift in the direction the player pushes. The bridge keeps
/// it as a resource.
#[derive(Resource)]
pub struct DroneSim {
    tick: u64,
    push: Vec3,
    positions: Vec<Vec3>,
    events: Vec<String>,
}

impl ExternalSim for DroneSim {
    type Snapshot = DroneSnapshot;
    type Event = String;

    fn submit_input(&mut self, input: &BridgeInput) {
        self.push = Vec3::new(input.move_axes.x, 0.0, -input.move_axes.y);
    }

    fn read_snapshot(&mut self, _frame_time: Duration, out: &mut DroneSnapshot) -> SnapshotTick {
        self.tick += 1;
        for p in &mut self.positions {
            *p += self.push * 0.05;
        }
        if self.tick == 1 {
            self.events.push("sim_started".into());
        }
        out.drones = self
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| Drone { id: i as u64 + 1, position: *p })
            .collect();
        SnapshotTick(self.tick)
    }

    fn drain_events(&mut self, out: &mut Vec<String>) {
        out.append(&mut self.events);
    }

    fn event_kind(event: &String) -> String {
        event.clone()
    }
}

/// Reacts to sim events in game code.
fn on_sim_event(mut events: MessageReader<SimEvent<String>>) {
    for event in events.read() {
        info!("sim event: {}", event.0);
    }
}

pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        let sim = DroneSim {
            tick: 0,
            push: Vec3::ZERO,
            positions: vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0)],
            events: Vec::new(),
        };
        let kinds = KindRegistry::from_ron(r#"(kinds: { "drone": (presentation_scripts: []) })"#)
            .unwrap_or_default();
        let router = EventRouter::from_ron(r#"(routes: { "sim_started": [Toast(text: "Sim online")] })"#)
            .unwrap_or_default();
        app.add_plugins(regia_scene::ScenePlugin)
            .add_plugins(SimBridgePlugin::new(sim).kinds(kinds).events(router))
            .add_systems(Update, on_sim_event);
    }
}
```

Your input systems write `BridgeInput` (`move_axes` and the names of action `edges` pressed this
frame) from the action layer ([Input](../user-guide/input.md)); the bridge hands it to the sim.
In a real game the sim runs on its own thread (or is a client of a server) and `read_snapshot`
copies out the latest state, interpolating to the frame time if it wants to.

## Kinds and presentation scripts

`config/kinds.ron` (a `KindRegistry`) says what each kind of sim entity looks like:

```ron
(kinds: {
    "warhull": (prefab: Some("prefabs/units/warhull.prefab.ron"),
                presentation_scripts: ["my_game::scripts::HullPresenter"]),
})
```

Presentation scripts are ordinary [scripts](../user-guide/gameplay.md) attached on spawn. They read
`SimExtras`, the extra `(name, value)` pairs the snapshot reported for that entity
(`SimSnapshot::extras_for`: damage, lean, turret yaw, a team color), and drive meshes,
animation and effects. The bridge records the `prefab` path; instantiating that prefab on spawn,
and `death_prefab` on vanish, is **planned** (`docs/specs/25` § 3). Today a game attaches meshes
to new `SimKind` entities itself (as `samples/sim_bridge` does).

## Routing events

`config/events.ron` (an `EventRouter`) maps an event kind to reactions without code:

```ron
(routes: {
    "explosion": [CameraShake(amplitude: 0.3, frequency_hz: 18.0, duration_secs: 0.6, falloff: 2.0),
                  Toast(text: "Hull breach")],
    "artillery": [SoundCue(cue: "boom_far", max_distance: Some(800.0))],
    "footstep":  [SoundCue(cue: "step")],
})
```

The bridge applies every reaction. `CameraShake` (needs `CameraPlugin`) shakes the camera and
`Toast` raises a `ToastRaised` message the runtime UI shows. On a client build `Vfx` spawns the
named effect from the `VfxScene` library, `Decal` places the named `DecalLibrary` prefab
(`scorch`, `rut`, a loaded `.decal.ron`) facing up into the `DecalPool`, `SoundCue` plays the
named audio cue (spatially at the event's place when it has one, flat when it has none) and
`Announcer` plays a line from the named announcer bank (rotation, pre-emption
and ducking as in [audio](../user-guide/audio.md)). `Vfx` and `Decal` need to know where the event happened:
implement `ExternalSim::event_place` to return `EventPlace::At(position)` or
`EventPlace::Entity(sim_id)` (the entity's current position). An event without a place skips its
placed reactions.

A placed `SoundCue` plays through the engine's default emitter, which carries **50 m**: past that the
play is refused (counted as `sounds_refused`, no voice starts). Give a far-carrying event its own
range with `max_distance` (meters), as `artillery` does above; leave it out and the 50 m default
applies. A value that is not finite and positive falls back to the default. An unplaced cue plays
flat and ignores the field.

Variations (effect seed, decal spin, cue pick) come from a seed derived from the
snapshot tick, the event's index and its kind, so a replay plays the same ones. Unknown names,
unplaced events and missing subsystems are counted in `RoutedReactionStats` (the agent's
`sim.counters` `reactions`), never fatal. A server build (no `client` feature) counts them as
`no_subsystem`. For anything else, read `SimEvent<E>` in a system.

## Frame-rate independence

A simulation must advance on its own clock, never on the presentation frame rate. `SimCounters`
records snapshot ticks, input calls and drained events; the agent methods `sim.counters` and
`sim.check_framerate_independence` run the game at two frame rates and compare the sim-side
rates, so a sim that accidentally ties itself to rendering fails a test
([agent guide: gameplay](../agents/gameplay.md)).

## A threaded host

`ExternalSim` leaves threading to the sim. When the sim is a plain fixed-step simulation, let
`regia_framework::sim_host::ThreadedSimHost` run it: it steps the sim on its own thread and
publishes the last **two** frames as one pair, so the presentation never waits for a tick and can
blend between them. Implement `FixedStepSim` and hand the host to the bridge:

<!-- ignore-reason: a sketch over the game's own MySim and MySnapshot types and an app built elsewhere; the compiled end-to-end example is sim_host::tests::a_threaded_host_drives_the_sim_bridge -->
```rust,ignore
use regia_framework::sim_bridge::{BridgeInput, SimBridgePlugin};
use regia_framework::sim_host::{FixedStepSim, SimHostSettings, ThreadedSimHost};

impl FixedStepSim for MySim {
    type Frame = MySnapshot; // implements SimSnapshot + Clone for the bridge
    type Event = MyEvent;
    fn step(&mut self, input: &BridgeInput) { /* one fixed tick */ }
    fn tick(&self) -> u64 { self.tick }
    fn write_frame(&self, out: &mut MySnapshot) { /* overwrite every field */ }
    fn drain_events(&mut self, out: &mut Vec<MyEvent>) { out.append(&mut self.events) }
}

let host = ThreadedSimHost::start(MySim::new(), SimHostSettings::default())?; // 60 Hz
app.insert_resource(host.handle()); // sim.host_status, sim.host_hold, sim.host_step
app.add_plugins(SimBridgePlugin::new(host));
```

- **The pair.** `host.published()` returns `prev`, `curr`, both ticks, both publish times and a
  sequence, always from the same publish. While the sim steps, `curr_tick` is `prev_tick + 1`;
  `alpha(now, delay)` places a frame drawn `delay` behind the clock between them (one tick is the
  usual delay). The bridge reads the pair through `read_pair` and blends by each kind's policy
  ([Handedness and blending](#handedness-and-blending)); `read_snapshot` copies `curr`.
- **Holding.** `handle.set_held(true)` pauses: the host publishes a *still* pair (`prev` and
  `curr` the same frame), drops the input it is offered and keeps its grants for later.
  `SimHostSettings::start_held` boots held (a lobby preloading a match).
- **Clocks.** `HostPacing::Realtime { tick, catch_up }` steps on the wall clock. A host that falls
  behind runs at most `catch_up` late ticks back to back, then drops the rest of the debt (counted
  as `debt_drops` and `ticks_dropped`), so a hitch never bursts the sim forward. `HostPacing::Manual`
  steps only ticks granted with `handle.grant_ticks(n)`: tests and render-outs step exactly.
- **A stale presentation.** With `client_stale_after: Some(d)`, the host steps only while
  `handle.stamp_client()` was called within `d` (call it once per presented frame), so a loading
  stall does not run the match unseen.
- **Input and events.** Axes replace, edges queue in order until the next tick. Events go to a
  bounded queue (`event_cap`, oldest dropped and counted).
- **Stopping.** Dropping the host stops and joins its thread; `into_sim()` also hands the sim
  back, for a hot-reload unload that keeps the sim's state. A sim that panics stops the host;
  waiters get `SimHostError::SimPanicked` instead of hanging.

Agents read and drive the host with `sim.host_status`, `sim.host_hold`, `sim.host_step`,
`sim.host_snapshot` and `sim.host_restore` ([agent guide: framework](../agents/framework.md)).

### Lockstep pacing

A lockstep game must step exact agreed frames, never merged input. Implement `LockstepSim` (one
`step_inputs(tick, &inputs)` per agreed tick) and `regia_detsim::state::DetState` for the sim, and
start the host with `HostPacing::Lockstep`:

<!-- ignore-reason: a sketch over the game's MySim and a net seat built elsewhere; the compiled end-to-end test is sim_host::tests::lockstep_pacing_steps_each_frame_once_in_order -->
```rust,ignore
use regia_framework::sim_host::{HostPacing, LockstepSim, SimHostSettings, ThreadedSimHost};

impl LockstepSim for MySim {
    type Inputs = MyFrame; // what one agreed tick carries, such as N224's LockstepFrame<I>
    fn step_inputs(&mut self, tick: u64, inputs: &MyFrame) { /* exactly one tick */ }
}

let settings = SimHostSettings {
    pacing: HostPacing::Lockstep { tick: Some(regia_framework::sim_host::DEFAULT_TICK) },
    ..SimHostSettings::default()
};
let (host, feed) = ThreadedSimHost::start_lockstep(MySim::new(), settings)?;
// On the net seat's thread, for every agreed frame:
feed.push_frame(tick, frame)?;
```

The host steps the frame for the next tick only, each once and in order. A missing frame stalls
the host until it arrives (`frame_stalls`); a duplicate or an old frame is refused with
`SimHostError::FrameOutOfOrder`, and one more than `frame_queue_cap` ticks ahead with
`FrameQueueFull`. Bridge input offered to a lockstep host is dropped (counted in `edges_dropped`):
the frames carry the input. With `tick: None` the host wakes once per grant instead of on the
clock, which tests and replays use to step exactly.

`handle.catch_up(n)` lets each wake step up to `n` queued frames back to back: a late joiner or a
resynced seat reaches the live tick faster than real time, but never more than `n` ticks per wake
(spec 120 § 7.4). Each caught-up tick is still published. When the host reaches a tick whose frame
has not arrived it is at the live edge, and it returns to one tick per wake on its own.

### Snapshots and restore

A host started with `start_lockstep`, or `start_with_snapshots` for a merged-input sim that
implements `DetState`, saves and loads the sim **on its own thread**, between two steps:

<!-- ignore-reason: `handle`, `send_to_seat` and `bytes` are the game's; the compiled test is sim_host::tests::snapshot_then_restore_republishes_and_resumes_bit_identically -->
```rust,ignore
// Now, or when the sim reaches a tick (answered right after that tick's step).
let snapshot = handle.request_snapshot(tick)?.wait()?;
send_to_seat(seat, &snapshot.bytes);

// On the seat: load the bytes, then push the frames after snapshot.tick.
let restored_tick = handle.restore(bytes)?.wait()?;
```

`restore` goes through `regia_detsim::state::restore_with`, so the sim resumes bit for bit; it
replaces the sim, drops queued frames up to the restored tick and publishes a *still* pair at the
restored tick (the presentation snaps there instead of blending from the old timeline). Damaged or
foreign bytes are refused (`SimHostError::RestoreFailed`) and the sim keeps running as it was. A
held host stays held. `HostReply::wait_timeout` bounds the wait.

### FP environment

A lockstep sim must step under the IEEE default floating-point environment on every peer. A thread
inherits its creator's environment, and an audio or video library may have turned on flush-to-zero
there. The host thread therefore calls `regia_detsim::fpu::ensure_default_fp_env()` before the first
step (`SimHostStatus::fp_env_reset` says whether it had to), and again every `fp_check_every` steps
(every step in debug builds, `DEFAULT_FP_CHECK_EVERY` = 600 in release builds) in case foreign code
running on the sim thread changed it; each change is put back and counted (`fp_env_changed`).

### Suspend

`SimBridgePlugin` adds `hold_sim_host_on_suspend` to the `Lifecycle` schedule: when the OS suspends
the app (Android: home button, screen off) a `SimHostHandle` resource is held, so the sim never
steps unseen, and `Resumed` releases it unless the game had held the host itself. An app that hosts
a sim without the bridge adds the system and `init_resource::<SuspendHold>()` itself.

## Handedness and blending

`SimBridgeSettings` (insert it, or build the plugin with `.settings(...)`) sets how the bridge turns
the sim's frames into poses (ADR-8456). The default changes nothing: a right-handed sim that
publishes one snapshot is drawn exactly as read, bit for bit.

<!-- ignore-reason: a sketch over the game's own sim, kinds file and app; the compiled end-to-end tests are sim_blend::bridge_tests -->
```rust,ignore
use std::time::Duration;
use regia_framework::sim_blend::SimBridgeSettings;
use regia_framework::sim_coords::SimHandedness;

app.add_plugins(
    SimBridgePlugin::new(host)
        .kinds(KindRegistry::load(Path::new("config/kinds.ron"))?)
        .settings(SimBridgeSettings {
            handedness: SimHandedness::LeftHandedReflectX, // the sim is left-handed
            teleport_distance: Some(10.0),                 // a 10 m jump in one tick snaps
            presentation_delay: Duration::from_nanos(16_666_667), // draw one tick behind
        }),
);
```

- **Handedness.** A sim whose world is left-handed (+X east, +Y up, +Z north) draws mirrored when
  placed unchanged. `SimHandedness::LeftHandedReflectX` reflects X once at the boundary, for every
  snapshot pose and every `EventPlace::At`: positions `(−x, y, z)`, rotations by quaternion
  conjugation `(w, x, y, z) → (w, x, −y, −z)` (a proper rotation, never a negative scale) and yaw
  `θ → −θ`. The sim must not convert again. Picking rays go back with the same function (the
  reflection is its own inverse: `SimHandedness::inv`). The helpers are in
  `regia_framework::sim_coords` (`pos`, `dir`, `quat`, `yaw`, `rendered_xz`, `seat_delta`).
- **The winding flip.** Placed meshes (glTF, cooked) are right-handed already and are not mirrored;
  only their placement is converted. A mesh the sim authors in its own coordinates and draws with an
  identity transform (baked world geometry) goes through `handedness.mirror_mesh(&mut cpu_mesh)`:
  positions, normals and tangents are reflected, the tangent sign flips and every triangle's winding
  is reversed (`mirror_mesh_winding`), so back-face culling and lighting stay right.
- **Blend policy per kind.** A sim that publishes its last two frames (`ExternalSim::read_pair`;
  `ThreadedSimHost` does) is drawn between them. Each kind in `config/kinds.ron` picks a `blend`
  (optional; `Lerp` when left out):

  ```ron
  (kinds: {
      "hull": (prefab: Some("prefabs/hull.prefab.ron"), presentation_scripts: []),
      "door": (prefab: None, presentation_scripts: [], blend: Hold),
      "marker": (prefab: None, presentation_scripts: [], blend: Snap),
  })
  ```

  `Lerp` lerps the position and slerps the rotation, and snaps to the newest pose past
  `teleport_distance`. `Snap` always draws the newest pose. `Hold` draws the older frame until the
  blend reaches the newest one: no in-between poses, but in time with the lerped kinds.
- **The delay.** The blend weight places a frame drawn `presentation_delay` behind the newest
  publish between the two frames (`PublishedFrames::alpha`). A still pair (a held host), and a
  manually paced host, draw the newest frame.

Agents read and change these with `sim.bridge_settings` and `sim.set_bridge_settings`
([agent guide: framework](../agents/framework.md#sim-bridge-presentation)).

## Presentation physics

Debris, ragdolls and explosion pushes are presentation: they run on the engine's physics next to
the sim and never feed back into it. The engine piece is
`regia_framework::sim_bridge::presentation` (ADR-8356). It is not on the lockstep closure.

- **Units are kinematic hulls.** `PresentationPhysicsPlugin { hulls, debris_lifetime_s }` and
  `HullShapes` map a sim kind to any `ColliderShape`, not only a box. The bridge writes the
  snapshot pose into `Transform` in `PreUpdate`; physics pushes a kinematic body's `Transform` as
  its next pose in every `FixedUpdate`, so the hull moves exactly where the sim says, with the
  velocity of that move. Debris resting on a deck rides along through friction. Physics never
  writes a kinematic body's `Transform` back.
- **A dynamic body with a sim id is refused.** Before the physics step the module removes that
  body, logs a warning, and increments `PresentationPhysicsStats::refused_writes`. The snapshot
  `Transform` stays. A hull kind is attached again as kinematic. Debris without a sim id stays
  dynamic.
- **The ground is the drawn ground.** With the `client` feature (it links `regia_terrain`),
  `PresentationGround::from_terrain(&TerrainMap, Option<&HolesMask>)` builds the collider from
  `TerrainMap::collision_heightfield` and lists the hole cells the draw skips. Insert the resource
  before `Startup`. The collider sits at the map's origin.
- **Debris has no sim id.** A `DebrisBurst` message spawns chunks through `DebrisSpawner`. The
  lifetime is `debris_lifetime_s` (30 simulated seconds unless you set it). The bridge only
  rewrites entities in its `SimEntityMap`, so the chunks stay dynamic.

Agents read `sim_bridge.presentation_stats` (hull bodies, debris alive, refused writes, and the
spawn counters). See [Presentation physics (agents)](../agents/presentation-physics.md).

Keep anything that must agree across machines out of this layer: physics is deterministic per
platform, but presentation bodies are not part of the sim's state, so two clients may scatter
debris differently. A sim event (`"explosion"`) is the usual trigger; route it to a system that
writes a `DebrisBurst` at `ExternalSim::event_place`.

`samples/sim_bridge` keeps the windowed demo (`PresentationPhysicsDemo`, the sample terrain
constants, `build_cube_mesh`) and calls the engine plugin (`sim_bridge --presentation-physics`).
The three promises live in `crates/regia_framework/tests/presentation_physics.rs`: a chunk dropped
on a bridged hull rests on its deck and rides with it when the snapshot pose moves, a chunk over a
hole falls through while one beside it rests, and the bridge never writes the `Transform` of an
entity without a sim id. `a_dynamic_body_with_a_sim_id_is_refused` holds the refusal rule.

## Recording input

`regia_framework::input_recording::InputRecordingPlugin` records and replays the per-frame input
of deterministic games; `samples/arena` uses it so a seeded bot match replays identically. That
is the building block for replays and lockstep tests.

## Hot reload and the sim

The sim lives in a library that is **not** hot reloaded: editing it needs an editor restart, like
engine code. The presentation side (scripts, systems) reloads normally
([Hot reload](../user-guide/hot-reload.md)).

The complete reference is `samples/sim_bridge`: three hover discs from a mock sim, presentation
scripts reading extras, chase/overhead/free cameras, timers, tweens, live settings, touch controls
on Android, a sound on the first sim event, and (with `--presentation-physics`) the presentation
physics above.
