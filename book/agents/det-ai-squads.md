# Deterministic squads and mode bots

[Motor and combat](det-ai-combat.md) move a unit and pick its shots. This page adds the two layers on top:

- the **wingman layer**: formations, a commander's orders turned into per-unit plans, the low-hull and low-ammo
  reports and the repair runs;
- the **mode bots**: goal orders for king of the hill, capture the flag, free-for-all and arena deathmatch,
  hooked to the [match rules](match-rules.md).

Both live in `regia_detsim::ai` (re-exported as `regia_ai::det`); the hooks live in `regia_framework::rules::bots`
(ADR-8554). All of it is lockstep code: the same inputs give the same bits on every peer, and
`cargo xtask det-scan` checks it. The code has the same bits as Direct Order 27's sim (copied under ADR-8012), so
a game moving its AI onto Regia keeps its checksums. A game's resource economy stays in the game.

## Formations

`wingman_slot(formation, index, anchor, yaw, close)` is wingman `index`'s slot on a leader at `anchor` facing
`yaw` (forward is `(sin yaw, cos yaw)`). Every slot sits behind the leader or clear of its fire lane.

| Formation | Shape |
|---|---|
| `Line` | Abreast, 12 m apart, one spacing behind the leader |
| `Wedge` | A V: wings 14 m out at the lane edge, files 16 m apart |
| `Column` | Single file, 16 m apart |
| `Loose` | A deterministic scatter behind the leader (no exact spacing) |

`close` halves the spacing. `assign_formation_slots(ids, ...)` slots a group by sorted entity id, so the same
order on the same ids gives the same slots in any caller order. `level_formation_anchor(..., &ground)` moves the
anchor to the nearest level spot (searched in fixed rings) when a slot would land more than 4 m above or below
the leader's ground; `ground(x, z)` is your height function. `spacing_error` and `slot_clears_leader_lane` are
the acceptance checks.

## Orders to plans

`WingmanCommand` is the vocabulary: `Follow`, `Advance`, `Flank`, `AttackTarget`, `AttackAtWill`, `Escort`,
`Hold`, `DefendPoint`, `DefendEntity`, `Patrol`, `CollectScrap`, `Regroup`, `Retreat`, `PickupPilot`, `CeaseFire`,
`WeaponsFree` and `Stance`. Your own order type implements `WingmanOrder` (return `None` for orders your sim
steers itself); `WingmanCommand::from_team_directive` maps a broadcast `TeamDirective` (attack, follow, defend,
retreat, go to, attack at) for units with no order of their own. Follow maps to `EscortCommander`: `plan` escorts the
team's commander hull (`is_commander`), else the lowest-id allied hull, and holds where the unit stands when it is
the commander or has no ally. To escort a specific entity, send `Escort { leader }` yourself.

`plan(&unit, &command, &world, slot_index)` resolves one unit's command into a `WingmanPlan`:

| Field | Meaning |
|---|---|
| `steer` | `SteerGoal { point, arrive_m, anchor_vel }`: where to go; `anchor_vel` is a moving leader's velocity |
| `fire` | `FireGate::WeaponsFree`, `CeaseFire` or `TargetOnly(id)` |
| `callout` | A `CalloutId`; `name()` is a stable key (`ack`, `engaging`, `hull_critical`, ...) for your line table |
| `option_bias` | The [tactics](det-ai-tactics.md) option this command favors, or `None` |

`route(&command, &ids, &world)` resolves a group (or one unit), slotting by sorted id. A Retreat walks to the
team's Re-up point (`reup_point`) and otherwise 40 m away from the nearest enemy; an unset Hold (the origin)
holds where the unit stands.

`drive(&unit, &plan, &world, &board, &coord, los, &mut rng)` turns a plan into an `AiUnitOutput`: the allied
keep-out first, then the Re-up override (a unit at 30 % hull or 20 % ammo walks to its Re-up point under any
command, still firing), then `arrive_moving` toward the goal. The fire gate goes through `fire_intent`, so a
friendly in the shot cone (`fire_blocked`) always holds the shot.

`report(&unit)` names the callout a unit should make: `HullCritical` at 15 % hull, `NeedRepair` at 30 %,
`LowAmmo` at 20 % ammo. `repair_goal` and `ammo_goal` give the nearest repair or ammo point for those runs; equal
distances break on the point's position, so the slice order of `points` never changes the run.

```rust,wrap
use regia_detsim::ai::{
    AiRng, AiUnitView, AiWorld, CoordState, FireGate, OpenField, SightingsBoard, WingmanCommand,
    WingmanFormation, drive, route,
};

let mut leader = AiUnitView::new(1, 1, [0.0, 0.0, 0.0]);
leader.is_commander = true;
let world = AiWorld {
    tick: 0,
    world_seed: 7,
    units: vec![leader, AiUnitView::new(2, 1, [30.0, 0.0, -5.0]), AiUnitView::new(3, 1, [-30.0, 0.0, -5.0])],
};
let command = WingmanCommand::Follow { leader: 1, formation: WingmanFormation::Line, close: false };
let plans = route(&command, &[3, 2], &world);
let goal = plans[&2].steer.expect("a slot").point;
assert!(goal[2] < 0.0); // behind the leader
assert_eq!(plans[&2].fire, FireGate::WeaponsFree);

let me = &world.units[1];
let mut rng = AiRng::tick_stream(world.world_seed, world.tick, me.id);
let board = SightingsBoard::new(3, 400.0);
let out = drive(me, &plans[&2], &world, &board, &CoordState::default(), OpenField, &mut rng);
assert!(out.steer.is_some()); // heads for its slot
```

## Mode bots

The bot layer reads one small view, `Objectives { hill, flags, tick, world_seed }`, and a `BotBody` per living
bot (id, team, position, hull, level, flag carrier). Each order is a `ModeOrder { goal, focus_enemy, koth, ctf }`.

| Mode | Order |
|---|---|
| King of the hill (`koth_order`) | Contest: drive to this bot's ring slot (8 m, ranked by id); Hold once inside; Rotate 60 m out along its radial below the level's retreat hull |
| Capture the flag (`ctf_order`, `assign_ctf_roles`) | By id: attack, defend, escort, repeating. The attacker takes the enemy flag and runs it to its own base; the defender holds (or returns) its flag; the escort stands 10 m off the carrier on the home side |
| Free-for-all and arena (`ffa_order`) | Hunt the nearest enemy in the level's acquire range, offset 6 m sideways by id parity; else the nearest pickup in the level's search radius; else hold |

`mode_order` gives one bot's order, `mode_orders` every living bot's for a snapshot. Difficulty only changes
decisions: `reaction_ticks`, `acquire_range_m`, `aim_for`/`hit_chance`, `retreat_hull_pct` and `pickup_radius_m`
come from the level's `DifficultyProfile`. `ScriptedSim` is a headless 2v2 over the orders (move 2 m a step,
flat damage), with a `digest` for determinism checks.

Every order reads its slices as sets: two peers holding the same contacts, allies or pickups in a different slice
order get the same orders. The hostile focus keeps a 1 cm tie band (within 1 cm the lower id wins) and scans the
contacts in id order; equal pickup distances break on the pickup's position. Passing slices in entity-id order is
still the cheap path (no copy).

### Hooking the match rules

`regia_framework::rules::bots` reads the [match rules](match-rules.md) board. Per tick, in the same order on
every peer:

```rust,wrap
use regia_framework::rules::bots::{AiLevel, BotHooks, CtfRole, contacts_of};
use regia_framework::rules::mode::MatchMode;
use regia_framework::rules::objective::{Contestant, ObjectiveBoard, ObjectiveSettings};

let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
board.place_flags(&[1, 2], 100.0, 900).expect("two flags");
let units = [
    Contestant { id: 1, team: 1, position: [100.0, 0.0, 0.0], alive: true }, // on team 2's flag
    Contestant { id: 3, team: 2, position: [100.0, 0.0, 400.0], alive: true },
];
board.ctf_tick(1, &units); // unit 1 picks the flag up

let hooks = BotHooks { level: AiLevel::Medium, world_seed: 7, free_for_all: false };
let bots = hooks.bodies(&board, &units, |_| 1.0); // your hull fractions
let orders = hooks.orders(&board, 1, &bots, &contacts_of(&bots), &[]);
let (_, carrier) = orders[0];
assert_eq!(carrier.ctf, Some(CtfRole::Attack));
assert_eq!(carrier.goal, [-100.0, 0.0, 0.0]); // run it home
```

`bot_mode(mode, free_for_all)` maps a `MatchMode` to its bot set (team deathmatch is the arena set; `Custom`
returns `None`, so a game's own mode keeps its own bots). `contacts_of` is "every bot sees every other"; a game
with [sighting boards](det-ai-perception.md) calls `mode_order` per bot with its team's contacts instead.

## Reading it from an agent

Plans and orders are your sim's state, so the engine has no protocol method for them, as for the
[combat layer](det-ai-combat.md). Drive the match itself through the `rules.match_*` methods of the
[match rules](match-rules.md), and expose a tick's plans or orders through your own agent method or debug
overlay.

## Tests

- `cargo test -p regia_detsim --test ai_wingman`: Direct Order 27's wingman acceptance (spacing within ±20 %
  standing and while the leader moves, every slot clear of the leader's lane, no friendly fire into the player's
  line, every command resolving on the first tick through the real motor, ten escorts settling clear, Re-up
  routing, fighting withdrawals).
- `cargo test -p regia_detsim --lib ai::` runs the wingman and mode-bot unit tests, including
  `scripted_sim_digests_match_the_pins` (the cross-arch pins).
- `cargo test -p regia_framework --test match_bots`: `mode_bot_hooks_route_a_ctf_carrier` (the rules pick up a
  flag, the bots route its carrier home, the rules score it), a king-of-the-hill team sent to the hill, and the
  mode mapping.
