# Deterministic motor and combat

[Tactics](det-ai-tactics.md) decide what a unit wants to do and how well it aims. This page turns that
decision into movement and shots. `regia_detsim::ai` (re-exported as `regia_ai::det`) has the motor (the
steering primitives and the hover energy manager), the combat composition (target priority, the fire
seam, the Re-up rule and the escort V) and the read view they share (ADR-8516). All of it is lockstep
code: the same inputs give the same bits on every peer, and `cargo xtask det-scan` checks it.

The motor and the composition have the same bits as Direct Order 27's sim (copied under ADR-8012), so a
game moving its AI onto Regia keeps its checksums.

## The read view

Each tick your sim builds an `AiWorld { tick, world_seed, units }` with one `AiUnitView` per unit and
structure, in entity-id order. Start from `AiUnitView::new(id, team, pos)` and fill what your sim knows:

| Field | What it is |
|---|---|
| `is_unit`, `on_foot` | A hull (`false`: a structure); crew on foot (allies do not keep out of it) |
| `pos`, `yaw`, `vel` | The body state; forward is `(sin yaw, cos yaw)` |
| `hull_pct`, `max_hull` | Hull left (0..1) and full hull points |
| `target_value` | How much a shooter values this target (your table: deciding structures first, scouts last) |
| `weapon_dps` | All mounted weapons' damage per second, summed in slot order |
| `reup` | `ReupRole::Repair` or `Supply` for a Re-up point, else `None` |
| `ammo` | The hardpoints (S-11's `AmmoSlotView`: role, projectile speed, ammo, band) |
| `hover` | `HoverView::from_params(&hover_params)`: top speed, yaw rate at speed, braking |
| `stance`, `weapons_free`, `level` | Fire orders and the difficulty level |
| `radius`, `is_commander` | Collision radius; the team's commander hull (escorts form on it) |
| `mortar_min_range_m` | Beyond this range a mortar shells a structure (90 m by default) |

A unit's output is an `AiUnitOutput { steer, fire, callout }`. `steer` is a `SteerCommand { throttle,
strafe, steer }`: the three hover inputs a player drives, each -1..1. The motor never writes a yaw, so
aim never fights steering.

## The motor

| Primitive | Does |
|---|---|
| `arrive(&unit, goal, park_m)` | Full throttle, easing off over 25 m; parked (all zero) inside `park_m`; brakes at -1 when the hull would coast through the park circle |
| `arrive_moving(&unit, slot, park_m, anchor_vel)` | Matches the anchor's velocity plus a pull toward the slot; `arrive` when the anchor is stopped |
| `pursue(&unit, pos, vel, Pursuit::{Pure, Lead, Lag}, time)` | Chases a point (current, ahead or behind the target); bleeds throttle to 0.3 when the nose must swing 90° |
| `orbit(&unit, pos, radius, dir)` | Faces the target and strafes round it, throttle holding the radius |
| `kite(&unit, pos, band_edge)` | Faces the target, strafes, and holds `band_edge` metres |
| `evade(&unit, threat)` | Full throttle and a hard turn away from the threat's side |
| `formation_slot(&unit, anchor, yaw, right, forward)` | `arrive` at a slot in the anchor's frame |
| `energy_throttle(&unit, share)` | The throttle that eases forward speed to `share` of top speed |

A hull below 3 m/s gets no turn demand from `pursue`, `orbit` or `kite`, and `steer_to` has a 0.03 rad
deadband: a parked or jammed hull never spins in place.

## Combat composition

`unit_output(&unit, &world, &board, &coord, los, decision, &mut rng)` expands a unit's committed option
(from [tactics](det-ai-tactics.md)) into steer and fire:

1. **Re-up.** At or below 30 % hull, or 20 % on any finite ammo slot (`reup_needed`), the unit retreats
   whatever it decided, toward `reup_point` (the nearest repair point when the hull is low, supply when
   dry), else the allies' centroid, else a break turn. It keeps firing while it falls back.
2. **Formation.** Regroup and Escort form a staggered V on the commander hull (or the lowest-id ally),
   one slot per escort by entity id, after `separate_from_allies` pushes the hull clear of the keep-out
   (`ally_keepout_m`: both radii plus 2 m from the commander; no overlap from other allies).
3. **Target.** `target_priority` scores `value / time_to_kill + threat × proximity`, doubles the score
   of a target at zero hull (finish kills), puts targets inside a weapon band first, drops sightings
   older than the level's trust window, and lets the team's trusted focus (`CoordState::set_focus`) win
   outright. Ties go to the lower entity id.
4. **Fire.** Once the level's reaction delay has passed since the first sighting, and the sighting is
   still trusted, the S-11 fire gate picks the slot: a mortar arc at a structure beyond the mortar range,
   a direct slot in band, a seeker at a unit in band.
5. **Steer.** Push and FocusFire lead-pursue; Hold parks at 85 % of the chosen weapon's far band edge;
   Strafe, Dogfight and Kite orbit; Flank drives 40 m along `flank_bearing`; Retreat withdraws.

`combat_run(&world, &board, &coord, los, &decisions)` runs every unit in world order, each with its own
`AiRng::tick_stream(world_seed, tick, id)`, and returns the outputs to apply after all decisions.

```rust,wrap
use regia_detsim::ai::{
    AiLevel, AiRng, AiUnitView, AiWorld, AmmoSlotView, AmmoView, CoordState, KnownEnemy, OpenField,
    OptionId, SightSource, Sighting, SightingsBoard, ThreatEstimate, WeaponRole, unit_output,
};

let cannon = AmmoSlotView { role: WeaponRole::Direct, projectile_speed: 120.0, ammo: None, band: [0.0, 150.0] };
let mut me = AiUnitView::new(1, 1, [0.0, 0.0, 0.0]);
me.ammo = AmmoView { slots: vec![Some(cannon)] };
me.weapon_dps = 60.0;
me.level = AiLevel::Hard;
let foe = AiUnitView::new(10, 2, [0.0, 0.0, 60.0]);
let world = AiWorld { tick: 30, world_seed: 7, units: vec![me.clone(), foe] };

// Team 1 saw unit 10 at tick 0 and still sees it.
let mut board = SightingsBoard::new(3, 400.0);
board.by_team[1].push(KnownEnemy {
    entity: 10,
    sighting: Sighting {
        entity: 10, last_pos: [0.0, 0.0, 60.0], last_vel: [0.0; 2], last_seen_tick: 30,
        first_seen_tick: 0, confidence: 1.0, source: SightSource::DirectLos,
    },
    threat: ThreatEstimate { dps: 60.0, range_band: [0.0, 150.0], can_reach_me: true, incoming: true },
});

let mut rng = AiRng::tick_stream(world.world_seed, world.tick, me.id);
let out = unit_output(&me, &world, &board, &CoordState::default(), OpenField, Some(OptionId::Push), &mut rng);
assert!(out.steer.is_some_and(|s| s.throttle > 0.0)); // closes on the foe
assert_eq!(out.fire.map(|f| f.target), Some(10)); // and shoots it
```

`fire_intent` is the same target choice and gates without the steer (for a layer that moves the unit
itself), with an optional `restrict` id ("attack my target"). `TeamTargets::build` resolves every team's
targets once per tick; the `_from` forms (`unit_output_from`, `fire_intent_from`, `target_priority_from`)
take it so a large crowd does not resolve the board once per unit.

## Reading it from an agent

The views and outputs belong to your sim, so the engine has no protocol method for them. `AiWorld`,
`AiUnitView`, `AiUnitOutput`, `SteerCommand`, `Target` and `CoordState` are serde: expose a tick's
outputs through your own agent method or debug overlay.

## Tests

`cargo test -p regia_detsim --test ai_combat` runs Direct Order 27's combat tests (band discipline,
Hard hits twice Easy, the hover duel, push and retreat, the group push, the escort V, target priority,
the Re-up rule, fighting withdrawals, the reaction gate) and four more: the mortar arc at a far
structure, the energy manager, the default table row for a contact out of view, and
`motor_and_combat_bits_match_the_pinned_hashes` (the cross-arch pins). `cargo test -p regia_detsim --lib
ai::` runs the ported motor tests and the combat unit tests.
