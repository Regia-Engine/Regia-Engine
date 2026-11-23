# Net characters (agents)

Characters in the predicted schedule of [net prediction](net-prediction.md): `regia_framework::net_character` and
`regia_framework::character::step_character` (spec 73 §§ 6.6, 6.7, 9; ADR-1905). Guide topic `net-character`. The module is
behind the `net` feature of `regia_framework`, which the `net` feature of `regia` and the agent server turn on.

## Setting it up

```rust,ignore
app.add_plugins((NetPlugin { mode }, CharacterPlugin, NetCharacterPlugin));

// The server spawns it; the client gets a copy.
commands.spawn((
    Replicated { transform: TransformReplication::Movement, ..default() },
    Transform::from_translation(start),
    NetCharacter::default(),
    NetPredicted,
    NetOwner(peer),
));

// Each fixed tick on the client, before NetSet::Predict.
fn write_input(mut q: Query<&mut InputCommand<CharacterNetInput>>, look: Res<Look>, axes: Res<Axes>) {
    for mut command in &mut q {
        command.value = CharacterNetInput {
            move_dir: axes.stick,
            yaw: look.yaw,
            pitch: look.pitch,
            jump: axes.jump_pressed,
            crouch: axes.crouch,
            sprint: axes.sprint,
        };
    }
}
```

- `NetCharacter` requires `Character`, `CharacterMovement`, `CharacterInput`, `CharacterState`, `NetCharacterBase` and
  `NetMontagePlay`, and marks the entity `CharacterSteppedExternally`, so the movement system of `CharacterPlugin` leaves
  it alone. Values a prefab sets stay. Both sides must hold the same `CharacterMovement` tuning: it does not travel.
- Use `TransformReplication::Movement` or `Full`. The comparison includes the rotation, and `PositionOnly` never delivers it.
- A character with no `NetOwner` belongs to the server and reads its own `CharacterInput` there (AI writes it).
- `CharacterNetInput` is quantized: `move_dir` 8 bits per axis (zero is exact), `yaw` and `pitch` 16 bits. `move_dir` is in the
  frame of `yaw` (`x` right, `y` forward); `CharacterNetInput::from_character_input` converts a world-space
  `CharacterInput`. The step runs on `quantized()` on both sides, so the client predicts with what the server receives. A
  missing command repeats the stick and clears `jump`. A stick axis of `-128`, which the encoder never writes, is refused
  with `character_input_range` (`CODE_MOVE_RANGE`), as vehicles refuse theirs with `vehicle_input_range`.
- A game with richer input implements `CharacterCommand` on its own `NetInput` type and calls `install_net_character::<I>`
  and `add_montage_visuals` instead of adding the plugin. `character_input()` must return the values as they travel.
- `step_character(world, collision, entity, &input, dt)` is the movement step on its own. It writes the entity's `Transform`,
  `CharacterState`, `CharacterMovement::mode` and clears `CharacterInput::jump`. `MoveResult::ground_entity` names what the
  character stands on.

## What counts as a correction

One tolerance covers the character's `Transform`, `CharacterState` and `NetCharacterBase`: translation 0.01 m, rotation 0.01 rad,
velocity 0.05 m/s, `time_in_air`, `coyote_time_left` and `jump_buffer_time_left` 1/240 s, `mode`, `grounded`, `jumps_used` and
`crouched` exact, `landed_this_frame` not compared. `NetResimulating` makes `RapierCharacterQuery` skip push impulses, which the
first run applied and the server applies for real.

## Moving bases

A tick-driven mover (`TickDrivenMotion`) predicts exactly and is still the right choice for anything a game can script.

Anything else a character stands on, such as a raft on physics, is a proxy on the client, some ticks behind the server. Client
and server disagree about where a rider is in the world and agree about where it is on the raft, so when the ground has a
`NetId`, both sides keep `NetCharacterBase { base, local, stepped_at, stepped_yaw }`:

- At the start of every step the rider is put back on the base, using the base's pose as that side knows it. On a re-run tick
  the base stands still at its latest known pose. This replaces the collision backend's carry (`CharacterAnchored`).
- Prediction compares `local`, not the world position, so a raft the client sees late is no correction. `net.prediction`
  reports the error on the base in `max_error_m`.
- The rider is drawn on the raft as the client sees it.
- A `Transform` write by something else (knockback, `net.force_correction`) moves `local` by the same amount.
- Jumping off, landing on, or losing a base are real differences: the world frames differ by the raft's lag. Expect a
  correction, smoothed by `NetVisualOffset`. Over `snap_distance_m` it snaps.

A dynamic body on a proxy turns kinematic on the client, so rapier takes its pose from the replicated `Transform` and riders
stand on it. The base's rotation carries the rider's position, not its facing.

## Root motion

Frame-time root motion cannot be re-run, so a networked character takes it only from `NetMontagePlay`:

```rust,ignore
// In a predicted system ordered before net_character_step::<I>.
if vault_pressed && !play.is_playing() {
    play.start("vault", 0, cx.tick(), 1.0);
}
```

- The montage comes from the rig's `MontagePlayer` library (`RootMotionDriven::rig`, or the character) on both sides. A montage
  that is not there logs `net.montage_not_in_library` and moves nothing.
- The step samples `MontagePlayback` at index `tick - start_tick`, follows the asset's section links and rate curve, and maps the
  motion with `Transform`s only. Re-run ticks produce the same bits.
- The character needs a `RootMotionDriven` that is not `Off`. Input drives it while nothing plays.
- Root motion is at full weight, so a vault covers the clip's whole distance. Warp windows do not apply, because warp targets
  are live per-rig state. To change section from predicted code, call `start` again at that tick.
- The rig shows the play state after the predicted schedule: a start the server refused is stopped, a late one begins at the
  time the montage has reached. Clients run this for proxies too.

## Methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.characters` | read | — | `{ characters: [{ entity, net_id, role, position, velocity, grounded, mode, base, base_local, montage }] }` |

`role` is `authority`, `predicted` or `proxy`. `base` is a `NetId` or null, and `base_local` is the position on it (the world
position without a base). `montage` is `{ name, section, start_tick, rate }` or null.

To dogfood a misprediction on a raft: read `net.characters` on the server for the rider's `entity`, call
`net.force_correction { entity, offset: [1, 0, 0] }` there, then `wait_until` on the client until `net.prediction` shows one
more correction and `max_error_m` close to 1.

## Errors

`F0002` with `data.reason`:

| Reason | Meaning |
|---|---|
| `net_not_running` | `NetCharacterPlugin` (or `install_net_character`) is not installed in this process |

Log targets: `net.character_input_refused` (another input type is registered), `net.montage_not_in_library`,
`net.montage_unresolved`.
