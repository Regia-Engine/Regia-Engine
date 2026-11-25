# Combat and abilities

<!-- index: Health -->
<!-- index: Damage -->
<!-- index: Gameplay tag -->
<!-- index: Ability -->
<!-- index: Attribute -->
<!-- index: Gameplay effect -->
<!-- index: Montage -->
<!-- index: Motion warping -->
<!-- index: Cue -->

Combat in Regia is three layers you can adopt one at a time. A hit can be a
`DamageEvent` on `Health`. The swing the player sees is a montage. Numbers
that stack, expire, and go on cooldown are attributes, effects, and abilities
(`docs/specs/53-abilities-attributes-effects.md`). This page
is the procedure. Presentation details stay on
[Montages](../../user-guide/montages.md) and
[Gameplay](../../user-guide/gameplay.md).

Two ability types exist and they do not share data. `regia_ai::abilities`
is a small set of named slots with a cooldown and a resource cost, ticked
with AI. `regia_framework::abilities` is the spec 53 model: attribute sets,
effects, grants, and cues. Use the second one for player combat. A behavior
tree decides when to activate. It does not replace the grant.

## A hit that changes health

The baseline, already in the framework, is enough for a sword and a health bar.

1. Add the damage systems (`regia_framework::damage`). `Health` holds
   `current` and `max`. A `DamageEvent` names the target, the amount, the
   source, and a position. `DamageSet` applies it in `FixedUpdate`.
2. Read `Died` or `DiedEvent` when `current` reaches zero. The third-person
   template respawns the player after a delay and saves which enemies are
   dead. See [Third Person template](../../user-guide/third-person-template.md).
3. Put gameplay tags on the entity when rules need them (`Status.Immune`,
   `Combat.Damage.Fire`). `TagContainer::has` matches a child tag.
   `has_exact` does not. The format is on
   [Gameplay](../../user-guide/gameplay.md#gameplay-tags).

That path does not apply stacking, durations, or costs. Add spec 53 when a
buff, a damage-over-time effect, or a cooldown must be data.

## Show the swing

1. Author a `.montage.ron` with sections (windup, swing, recover) on a slot
   such as `full_body` or an upper-body slot. The asset shape is on
   [Montages](../../user-guide/montages.md).
2. Start it from the attack input. A combo is a section jump: a second press
   inside the window sets the next section instead of restarting the montage.
3. When the hit should land on a point the game chooses (the enemy, a ledge),
   add a motion-warp window. Warping follows the authored timeline. A section
   jump during the window skips warping for that frame.
4. Apply damage from a notify or from a fixed-step timer you own, not from
   the render frame. The same tick then hits the same way on a replay.

The template's combo is ordinary game code (`game/src/combat.rs`): three
swings, 0.9 s to chain, the third swing hits hardest. Replace that with a
montage when the motion has to be authored.

## Grant an ability

`AbilityPlugin` (`regia_framework::abilities`) advances effects and ability
state only in `FixedUpdate`. Without the plugin, every `abilities.*` agent
method answers `{ "available": false }`.

1. Write an attribute set (`.attrs.ron`), an effect (`.effect.ron`), and an
   ability (`.ability.ron`). Field lists are in `docs/formats/attrs-ron.md`,
   `docs/formats/effect-ron.md`, and `docs/formats/ability-ron.md`.
2. Add `AbilityPlugin`. Load the assets into `AbilityAssets` by path.
3. Put the attribute set on the fighter and grant the ability. From the agent
   protocol the same steps are `abilities.add_attributes` and
   `abilities.grant`. A full call sequence (grant, activate, cooldown, a burn
   effect) is on [Abilities (agents)](../../agents/abilities.md).
4. Activate from input or from `abilities.signal` with a gameplay tag. A
   refusal is a normal result (`started: false` and a `cause` such as
   `on_cooldown`), not a protocol error.
5. Cues (`CueEvent`) are presentation only. Play a montage, a sound, or a
   camera shake from the cue. The cue must not change health, tags, or
   cooldowns. Those already changed in the fixed step that emitted the cue.

`regia_ai::abilities::Abilities` stays available for an AI slot that only
needs a cooldown. Do not store spec 53 grants in it.

Network prediction of abilities is reserved and not part of this version.
Spec 53 § 2.8 keeps an intent sequence number and does not replicate the
book. Do not describe a predicted ability as shipping.

## What an enemy needs

Pathing and "when to swing" are not the ability system.

- A navmesh agent or a behavior-tree `MoveTo` brings the enemy into range.
  See [Navigation and AI](../../user-guide/navigation-ai.md).
- The tree's leaf calls the same grant activation the player's input calls.
- GOAP picks a sequence of action names. It does not deal damage. See
  [GOAP](../../user-guide/goap.md).

## Destruction is not damage

Breaking a wall is [destruction level 2](../../user-guide/physics.md):
pre-fractured chunks and bonds. A gameplay hit can also queue
`physics.destruction_damage`, or an explosion can do both
(`gameplay.explode` with `destruction: true`). Destruction level 3, fracturing
a mesh at runtime, is not in this version. It is parked after v1.0.

## Finished check

You are done when all of these hold:

- One `DamageEvent` lowers `Health.current` by that amount on the next fixed
  step, and a second event in the same step applies in order.
- At zero health the entity raises `Died` once, and your respawn or ragdoll
  runs from that message.
- The attack montage plays on the press and does not restart if the player
  holds the button. A combo window chains sections.
- With `AbilityPlugin`, `abilities.activate` on a fresh grant returns
  `started: true`, and the same call while the cooldown tag is held returns
  `started: false` with `cause` `on_cooldown`.
- A timed effect changes `Attributes.current` and restores it when the effect
  ends. `abilities.inspect` shows the breakdown.
- A cue handler plays a sound or a montage and does not write `Health`.

The template proves the health path: `cargo test -p my_game` includes a swing
that kills a grunt and a load that brings it back. Ability checks are
`abilities.check` and `abilities.inspect` after `play.step`.

## When it does not work

- Damage never lands. `DamageSet` is not in the schedule, or the event's
  target entity has no `Health`.
- The cooldown never ends. Something else grants the cooldown tag, so the
  count stays above zero. Tags from effects use `TagCounts`. The authored
  `TagContainer` is not cleared by the ability system.
- The montage and the hit disagree. Damage was applied on the render frame
  instead of the fixed step that owns the notify.
- `abilities.grant` says the plugin is unavailable. `AbilityPlugin` is not
  installed on that app.

## Where to read next

- [Montages](../../user-guide/montages.md) and
  `docs/specs/34-montage-extensions.md` for sections,
  warping, and sync groups.
- [Abilities (agents)](../../agents/abilities.md) for every method, refusal
  cause, and error string.
- [UI and audio](ui-and-audio.md) for the health bar, the hit sound, and captions.
- [Cloth](../../user-guide/cloth.md) if a cape should react to the swing. Cloth
  does not read the ragdoll unless you hand it the shapes.
