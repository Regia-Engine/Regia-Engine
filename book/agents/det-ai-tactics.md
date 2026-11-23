# Deterministic tactics, difficulty and aim

The [deterministic AI core](det-ai-core.md) schedules and scores; [perception](det-ai-perception.md)
says what a unit knows. This page is how a unit commits to a tactic, how sharp it is, and how it
shoots. `regia_detsim::ai` (re-exported as `regia_ai::det`) has the hysteresis decision loop, the
Easy/Medium/Hard difficulty profiles and the aim and fire module (ADR-8462). All of it is lockstep
code: the same inputs give the same bits on every peer, and `cargo xtask det-scan` checks it.

The loop, the profiles and the aim math have the same bits as Direct Order 27's sim (copied under
ADR-8012), so a game moving its AI onto Regia keeps its checksums.

## Committing to a tactic

`catalogue(&thresholds)` returns the twelve options (`OptionId::ALL` order): Push, Hold, Strafe,
Dogfight, Kite, Retreat, Regroup, Defend, CollectScrap, Escort, Flank and FocusFire. Their curve
knees come from the profile's `Thresholds` (for example Push turns on around `push_force_ratio`), so
two levels score the same situation with the same curves, only shifted.

`Hysteresis` is the core's decision policy. A unit keeps its option unless:

- a challenger beats it by `switch_margin` (0.15) on `min_hold_ticks` (5) decisions in a row;
- the host invalidates it (`decide(.., invalidated = true)`: the target died, the order was cancelled);
- it has been held `max_hold_ticks` (40) decisions, when the best option is taken whatever the margin.

The first decision commits the best score. Of equal best scores the last wins; no scores commit Hold.
The counters count decisions, so on the default 10-tick cadence 5 decisions are 50 ticks.

```rust,wrap
use regia_detsim::ai::{
    AiCore, DifficultyProfile, Hysteresis, Inputs, OptionId, catalogue, decide,
};

let hyst = Hysteresis::default();
let mut core = AiCore::new(0xD1FF, hyst.cadence(), hyst);
let profile = DifficultyProfile::medium();
let options = catalogue(&profile.thresholds);
let units = [3u32, 13, 23];

for tick in 0..100 {
    core.decide(tick, &units, |_unit, _rng, scores| {
        // The host fills the inputs from its sim (perception, influence, hull, ammo).
        let inputs = Inputs { hull_pct: 0.9, ammo_pct: 1.0, local_force_ratio: 0.7, ..Inputs::default() };
        profile.score_options(&options, &inputs, scores);
    });
}
assert!(core.decision_for(3).is_some());

// An order invalidates the option: commit the new best at once, outside the schedule.
let chosen = decide(&hyst, core.decision_entry(3), &[(OptionId::Retreat, 0.9)], true);
assert_eq!(chosen, OptionId::Retreat);
```

## Difficulty is decision quality

`DifficultyProfile::for_level(AiLevel::Easy | Medium | Hard)` returns one plain-data profile. It has
no hull, ammunition or damage field: a Hard bot plays the same unit better, never with better
numbers.

| Field | Easy | Medium | Hard |
|---|---|---|---|
| `reaction_delay_ticks` | 36 | 21 | 12 |
| `perception` (trust, decay, LOS re-check, forget) | 30, 0.90, 24, 0.5 | 60, 0.96, 12, 0.35 (`PerceptionParams::default()`) | 120, 0.99, 6, 0.2 |
| `aim` (lead error share, tracking lag, spread mrad) | 1.0, 20, 35 | 0.5, 8, 18 | 0.2, 3, 8 |
| `repertoire` | Push, Hold, Regroup | + Strafe, Kite, Retreat | every option |
| `coordination` (focus fire, flank, stagger) | none | focus fire | all three |
| `thresholds` (push ratio, retreat hull, outnumbered, aggression, caution) | 1.0, 0.30, 2.0, 1.1, 0.8 | 1.3, 0.20, 2.0, 1.0, 1.0 | 1.5, 0.35, 2.0, 0.9, 1.2 |
| `scrap` (routing, search radius m) | no, 30 | no, 45 | yes, 60 |

`profile.allows(id)` is the repertoire plus the team abilities (Flank needs `coordination.flank`,
FocusFire needs `coordination.focus_fire`). `profile.score_options(&catalogue, &inputs, out)` scores
the allowed options, multiplies Push by `aggression` and Retreat/Defend by `defence` (`bias`), and
clamps to 0..1. `AiLevel::parse` reads "easy", "medium" (or "normal") and "hard"; `as_str` writes
them. The profile is serde, so a lobby can store a seat's level or a custom profile.

## Aim and fire

The fire gate reads a `ShooterView` (position, `weapons_free`, `stance`, hardpoints, the mortar range)
and a `FireTarget` (entity id, position, velocity, unit or structure, range). A hardpoint slot is an
`AmmoSlotView`: its `WeaponRole` (Direct, Mortar, Seeker), projectile speed, ammo (`None` for energy
weapons) and range band, filled from your weapon table.

`fire_gate(&shooter, &target, &profile.aim, &mut rng, los, &friendlies)` returns `None` (hold fire)
when the shooter is not weapons-free or holds fire, no usable slot fits, the line of sight (any
`LineOfSight`) is blocked, or a friendly is in the 10° shot cone. Otherwise it returns a
`FireIntent { slot, target, aim_error }`. Slot choice (`choose_slot`, `choose_slot_with`):

1. A mortar in band beyond `mortar_min_range_m` (90 m by default), against a structure only.
2. A direct slot in band.
3. A seeker in band, against a unit only.

A dry slot is never used. `aim_error` is the human-like miss: `lead` is the level's share of the
target's travel during the shot's flight, `tracking_lag_ticks` is the settle time, and `spread_mrad`
is the level's cone times a 0.5..1.5 draw from the unit's `AiRng` stream (the only random input).

```rust,wrap
use regia_detsim::ai::{
    AiRng, AmmoSlotView, AmmoView, DifficultyProfile, FireTarget, OpenField, ShooterView,
    WeaponRole, fire_gate, lead_point,
};

let cannon = AmmoSlotView { role: WeaponRole::Direct, projectile_speed: 120.0, ammo: Some((50, 50)), band: [0.0, 150.0] };
let shooter = ShooterView::new([0.0, 0.0, 0.0], AmmoView { slots: vec![Some(cannon)] });
let target = FireTarget { id: 9, pos: [0.0, 0.0, 100.0], vel: [10.0, 0.0, 0.0], is_unit: true, range: 100.0 };
let mut rng = AiRng::tick_stream(0xD1FF, 40, 1);

let shot = fire_gate(&shooter, &target, &DifficultyProfile::hard().aim, &mut rng, OpenField, &[]);
assert_eq!(shot.map(|s| s.slot), Some(0));
// Where to point the barrel: the intercept of a 120 m/s shell and the crossing target.
let aim = lead_point(shooter.pos, target.pos, target.vel, 120.0);
assert!(aim[0] > 5.0);
```

Other helpers: `lead_point` (three fixed iterations of the intercept in plan), `ballistic_angle` and
`ballistic_aim` (the shallow mortar arc by bisection; 45° when the target is out of reach),
`seeker_can_lock` (units only, inside the band) and `nearest_in_band(&board, team, pos, band)` (the
nearest known enemy of a sighting board within a band). None of them writes a heading: firing never
fights the steering.

## Reading it from an agent

The decisions, profiles and fire intents belong to your sim, so the engine has no protocol method
for them. `Decision`, `DifficultyProfile`, `FireIntent` and `AimError` are serde, and
`AiCore::rows(next_tick)` gives each unit's committed option: expose them through your own agent
method or debug overlay.

## Tests

`cargo test -p regia_detsim --test ai_tactics` runs the hysteresis-in-the-core, profile filter and
aim-ordering tests and `tactics_and_aim_bits_match_the_pinned_hashes` (the cross-arch pins);
`cargo test -p regia_detsim --test bot_ai` runs Direct Order 27's bot tests (fire only in range, firing
keeps the hull's yaw, a straight 300 m approach); `cargo test -p regia_detsim --lib ai::` runs the
ported tactics, difficulty and aim unit tests.
