# destruction_lab

Destruction level 2 end to end: pre-fractured chunks, the server decides breaks, clients play them.

- A wall and a small tower, cut from boxes by the fracture tool when the lab starts (`kit/mod.rs`; the
  repo holds no binary assets for them).
- A dedicated server and two clients in one process. Datagrams go from app to app through each
  process's `NetNode`, the in-memory path of the destruction network tests.
- `DestructionPlugin` and `DestructionNetPlugin` on all three, `ExplosionPlugin` on all three,
  `DestructionDrawPlugin` and `ExplosionFxPlugin` on client 0.
- A script: a blast in front of the wall, a second one behind it while the chunks fly, one at the foot
  of the tower.

## Run it

```sh
cargo run -p destruction_lab --example lab                       # a window on client 0
cargo run -p destruction_lab --example lab -- --headless --ticks 120
cargo test -p destruction_lab                                    # needs lavapipe or a GPU for the agent test
```

The library cannot link the fracture tool, because a sample's runtime stays free of pipeline code. So
the kit is cut in the test and the example, and the library takes it as a `LabKit`.

Client 0 runs the show. Each of its fixed ticks runs one frame of the server and of client 1
(`drive_lab`), so `play.pause` and `play.step` hold all three in lockstep.

## What to copy

- `process` in `src/lib.rs`: the plugins of a process, server or client.
- `LabBlast` in `src/wire.rs`, `announce_blasts` and `hear_blasts`: how a blast reaches the clients. The
  break events are the engine's job; telling clients about the explosion itself is the game's.
- `dress_roots` in `src/view.rs`: a client places nothing, so it dresses the instances that arrive.

## Ask it things

The agent listens on client 0.

```sh
regia agent call --project samples/destruction_lab lab.fire '{"center":[-4,1.2,1],"preset":"barrel"}'
regia agent call --project samples/destruction_lab lab.report
regia agent call --project samples/destruction_lab physics.destruction_playback
```

`lab.report` shows all three processes. See the guide:
[destruction over the network](../../book/agents/destruction-net.md#a-sample-to-copy).

## Limits

- Client 0 runs one lab tick per frame, whatever the frame rate: this is a lab, not a real-time game.
- Everything crosses at once. Debris poses match between the clients because they run the same ticks
  from the same stream. Over a real link the chunks leave with the same velocities and land where each
  client's physics puts them.
- Both clients share the process with the server, so this says nothing about bandwidth; the numbers
  for that are in `destruction_net_perf`.
- Headless, particles spawn and advance only on a rendered frame, and the engine drops spawn jobs that wait
  too long for one. Take `seeing.screenshot` within a few frames of a blast to see its fire; a window has
  no such limit. Shots of one paused moment show the same fire however far the physics stepped.
- The fireball is sized by the crash effect's scale law, whose smallest charge is 50 kg of TNT equivalent, so every blast in
  the lab draws a fireball about 12 m wide. In a window it hides the tower for a few seconds after a blast.
  The dark cluster in the middle of a fresh fireball is its embers, which spread out over the next frames.
