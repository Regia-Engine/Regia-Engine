# Squads, attack tokens, formations and the AI tick budget

This page is about AI at the scale of a whole encounter: agents that act as a group, take turns attacking
one target, stand in distinct places, and do not all think on every fixed step. `AiSquadPlugin`
(`regia_ai::squad`) adds the group features and `AiPlugin` already holds the tick budget. Both are off until
you ask for them: without `AiSquadPlugin`, or with no `AiTickBudget` that is `enabled`, trees tick exactly as
they did before, step for step.

Everything here is counted in fixed steps and never timed, so two runs from the same state agree bit for bit.

| Piece | Goes on | Agent methods |
|---|---|---|
| `AiSquad`, `AiSquadMember`, `AiDead` | the squad entity, its members | `ai.squads`, `ai.squad_set` |
| `AiAttackTokens`, `AiTokenHolder` | the target, the holders | `ai.squads`, `ai.squad_set` |
| `AiFormation` | the squad entity | `ai.squads`, `ai.squad_set` |
| `AiTickBudget`, `AiImportance`, `AiTickStats` | a resource, an agent, a resource | `ai.tick_budget_get`, `ai.tick_budget_set` |

`ai.query_impls` lists the custom generators and tests [spatial queries](spatial-queries.md) may name.

## Squads

A squad is an entity with `AiSquad`, which brings a `Blackboard` the members share. An agent joins by getting
`AiSquadMember::new(squad)` and leaves by losing it. The squad entity must exist first.

<!-- ignore-reason: fragment of a game's setup: `app` and `world` come from the game -->
```rust,ignore
app.add_plugins((AiPlugin, AiSquadPlugin));
let squad = world
    .spawn(AiSquad::new("alpha").with_role(SquadRole::new("flanker", 2)))
    .id();
let sergeant = world.spawn((Blackboard::new(), AiSquadMember::new(squad).with_rank(5))).id();
```

**The leader rule.** The member with the highest rank leads; on equal ranks, the one that joined first. An
election runs when a squad with no leader gains a member and when the leader leaves or dies. It does not run
otherwise, so a new member with a higher rank does not depose a living leader, and writing a member's `rank` elects
nobody. Inserting `AiSquadMember` over an existing one is leaving and rejoining: the member goes to the end of the join
order, and if it led, a new leader is elected. `set_squad_leader` picks a leader by hand, until it goes.

**Death.** A despawn counts, and so does inserting `AiDead` on an agent that stays in the world (a corpse, a
ragdoll). Both take effect at once through component hooks, whichever system caused them: the leader, the roles,
the formation slots and the attack tokens have changed before the next line of the code that did the killing.
Removing `AiDead` puts the member back in its squad, at the end of the join order.

**Role slots.** Each `SquadRole` has a name and a capacity. With `auto_fill` (the default of `SquadRole::new`)
free slots are filled when a slot or a member becomes free: roles in declared order, each taking roleless
members by the leader rule's order. `claim_squad_role` and `release_squad_role` do it by hand. `set_squad_role` adds a
role or changes its capacity, taking the role from the latest holders if there are too many.

What the members can read:

| Blackboard | Key | Value |
|---|---|---|
| each member's | `squad_is_leader` | bool |
| each member's | `squad_role` | string, absent with no role |
| the squad's | `squad_leader` | entity, absent with no leader |

The leaves `SquadRead` and `SquadWrite` copy a key between the squad's blackboard and the agent's. Register a
configured one under a name your tree uses:

<!-- ignore-reason: registration lines inside a game's task-registry setup -->
```rust,ignore
SquadWrite::new("my_target", "shared_target").register(&mut registry, "game::TellSquad");
SquadRead::new("shared_target", "target").register(&mut registry, "game::HearSquad");
```

## Attack tokens

`AiAttackTokens::new(2)` on a target means at most two agents hold a token on it at once. Whoever holds one
attacks; the rest circle, take cover or do something else.

**Who gets a token.** An agent that is refused is remembered as a *waiter* until it stops asking
(`waiter_ttl_steps`, 30 fixed steps by default) or cancels. A free token goes to the waiters by priority, highest
first, then in the order they first asked; someone asking out of turn is refused and joins the line. So an
agent that just gave a token back and asks again waits behind everyone already waiting, and agents of equal
priority take turns. With `preempt` on (off by default), a waiter first in line also takes a token from the
lowest-priority holder, the latest granted of those, when its own priority is strictly higher.

**Automatic release.** A holder that dies or is despawned frees its tokens at once, and a target that dies or is
despawned frees every holder. A despawned or dead waiter stops holding up the line. A holder keeps its token while it
lives, including while it is parked, and removing a tree from an agent does not reset the tree's leaves.

**With a tick budget**, keep `waiter_ttl_steps` above `cadence + lateness` of the agents that wait, or a slow agent loses
its place each time it expires. A slow agent at the head of the line also holds a free token until its next tick.

| Leaf (`regia_ai::...`) | Does | Returns |
|---|---|---|
| `AcquireAttackToken` | asks for a token on the entity in blackboard key `target` | `Success` once held, `Running` while waiting, `Failure` with no target, no pool, or a dead agent |
| `AcquireAttackTokenScoped` | the same, and gives it back when the node resets: a `Timeout` that fires, the end of a `Parallel`, a new `Repeat` iteration, leaving the state it runs in | as above |
| `ReleaseAttackToken` | gives back every token the agent holds | always `Success` |
| `HasAttackToken` | checks for a token on `target`, so a behavior tree can notice a preemption mid-attack (a state tree does not tick a finished task again, so there it answers once, on entering the state) | `Success` or `Failure` |

State trees use the same task types, by the same names. `AcquireAttackToken::register` configures another key, a
priority (literal or from a blackboard key) or `wait: false`.

**A sequence that fails part way keeps its token, with either leaf.** A `Sequence` that fails does not reset its
children, so `Sequence(Acquire, Attack, Release)` skips the release when `Attack` fails, and the token stays held for as
long as the tree does something else. Put a release on the other path:
`Selector(Sequence(Acquire, Attack, Release), Sequence(Release, Patrol))`. In a state tree, put the scoped leaf in the
state that attacks; use the plain one when the token must outlive the state that acquired it, and release it in a later
state.

## Formation slots

`AiFormation` on the squad entity gives each member a place around a target or the leader. Every
`interval_steps` fixed steps (6 by default), and on the next step after a member joins or leaves or the leader changes, the shape's candidate
places are walked in a fixed order. Each is projected onto the navigation mesh with `find_nearest`, optionally
checked for a path from the anchor (`require_path`), and dropped when it has no mesh point or lands within
`min_separation` of a place already accepted. So two slots next to a wall do not collapse onto one spot: outer
rings and later rows make up the numbers. With no `NavMesh` resource the places are used as they are.

| Shape | Places |
|---|---|
| `Ring { radius, spacing }` | rings around the anchor, the first at `radius`, more `spacing` beyond; fixed to the world axes, so slots do not sweep as the anchor turns |
| `Line { spacing, row_length }` | rows behind the anchor, centered on its heading |
| `Wedge { spacing }` | a V opening backward from the anchor |

**Assignment.** A member keeps its slot while the slot stays valid. The others, in join order, take the nearest
free slot of the innermost ring or row with room, the earlier candidate on a tie. A resolve always looks at whole
rings and rows, so two members around a target pick from the whole ring instead of crowding its first two places.
A slot index goes to at most one member. A member with no slot left is in `unplaced`. A `Leader` anchor takes no
slot, and an anchor that is gone or dead empties the slots.

A slotted member's own blackboard gets `formation_slot` (a vec3) and `formation_slot_index` (an integer); both go
away when it has no slot, when it leaves, and when the formation or the squad goes away. Its tree walks there with
whatever move leaf it uses. `slot_key` renames them: insert a new `AiFormation` to change it.

## The tick budget

`AiTickBudget` caps how many behavior trees and state trees tick per fixed step. It covers those two only:
`UtilityPlugin`, `GoapPlugin` and `PerceptionPlugin` have budgets of their own.

<!-- ignore-reason: resource values only; the surrounding app is the game's -->
```rust,ignore
world.insert_resource(AiTickBudget {
    enabled: true,
    max_trees_per_step: 64,
    tiers: vec![
        AiTickTier { max_distance: 30.0, cadence_steps: 1, max_trees_per_step: 0 },
        AiTickTier { max_distance: 80.0, cadence_steps: 4, max_trees_per_step: 0 },
        AiTickTier { max_distance: f32::MAX, cadence_steps: 16, max_trees_per_step: 0 },
    ],
    focus: Some(player),
});
```

Each step:

1. A tree is *due* when the steps since its last tick reach its tier's cadence. The tier comes from the agent's
   distance to the focus: the budget's, or its own `AiImportance::focus`. The last tier also takes everything
   beyond it, and any agent with no focus or no position. A tree seen for the first time is due at once, and an
   `AiImportance::always_on` agent is due every step.
2. Due trees are taken in this order: trees overdue by a whole cadence, always-on agents, then the rest; within each,
   latest first, then by nearer tier, then in the order the scheduler first saw them, up to the tier's and the step's
   limits. Always-on ignores tier limits and does not count against them, but the step's limit holds for everyone, so
   always-on agents that alone fill it still give way to a tree overdue by a whole cadence.
3. A tree that ticks gets `dt` = the steps since its last tick times the fixed step, so a timer inside it runs at
   the same speed at any cadence. Cadence 1 sees exactly the fixed step.

A tree that is due and misses its turn stays due, is later on the next step, and goes earlier. That is the round
robin. A parked agent keeps no clock: it comes back with one step of time. A state tree that is paused or finished
cannot do any work, so it uses no budget. An agent with `AiDead` keeps ticking: stopping a dead agent's tree is the game's
job (remove the tree or park the agent).

A state-tree `hold` keeps its seconds when an agent changes cadence: its counter is re-expressed in the new `dt`.

**When is every tree on its cadence?** When the demand fits (the sum over tiers of `trees / cadence` is at most
`max_trees_per_step`) and each cadence divides the next. Then, with 10 trees a step, 10 near agents at cadence 2
and 90 far ones at cadence 18 all tick exactly on cadence. When the demand does not fit, the limits still hold and
every tree still takes its turn, later than asked. `AiTickStats` counts that (`late_ticks_total`,
`max_late_steps`), so watch it when sizing a budget.

## Agent methods

| Method | Permission | Params | Result |
|---|---|---|---|
| `ai.squads` | read | `{ squad? }` | `{ step, squad }`, or with no `squad` `{ step, squads, token_pools }`, in entity order |
| `ai.squad_set` | play | `{ squad \| create, name?, roles?, add?, remove?, leader?, role?, blackboard?, formation?, token_pool? }` | `{ squad, token_pool }` |
| `ai.tick_budget_get` | read | `{ entity? }` | `{ budget, stats, agent }` |
| `ai.tick_budget_set` | play | `{ enabled?, max_trees_per_step?, tiers?, focus? }` | `{ budget }` |
| `ai.query_impls` | read | `{}` | `{ generators, tests, providers, matcher }` |

`ai.squad_set` fields, applied in this order: `roles` adds or changes `{ name, capacity, auto_fill? }`; `remove`
holds entity ids; `add` holds entity ids or `{ entity, rank? }`; `leader` is a member; `role` is `{ member, name }`
(a null `name` takes the role away); `blackboard` maps keys to a bool, number, string, `[x, y, z]`,
`{ "entity": id }` or null to remove; `formation` is
`{ anchor: "leader" | { "entity": id }, shape: { kind: "ring", radius, spacing } | { kind: "line", spacing, row_length } | { kind: "wedge", spacing }, interval_steps?, min_separation?, require_path?, slot_key?, extents? }`
or null to remove; `token_pool` is `{ target, capacity, preempt?, waiter_ttl_steps? }` and can stand alone with no
`squad` when no other squad field is given. An entity in `add` that is already a member keeps its place and only changes
rank, if one is given. Parameters are checked before anything changes, except `role_full`, which is only known once the
roster has changed. An agent with `AiDead` counts as no member.

`ai.tick_budget_set` changes the fields named and keeps the others. `tiers` lists `{ max_distance, cadence_steps,
max_trees_per_step? }`, nearest first and ascending, and only the last may leave out `max_distance`. A null `focus`
clears it. `agent` in `ai.tick_budget_get` is `{ entity, tier, distance, always_on, behavior_tree_age, state_tree_age }`.

```json
ai.squad_set { "create": true, "name": "alpha", "roles": [{ "name": "flanker", "capacity": 2 }],
               "add": [4294967296, { "entity": 4294967297, "rank": 5 }],
               "token_pool": { "target": 4294967298, "capacity": 2 } }
ai.tick_budget_set { "enabled": true, "max_trees_per_step": 10, "focus": 4294967299,
                     "tiers": [{ "max_distance": 30.0, "cadence_steps": 1 }, { "cadence_steps": 16 }] }
ai.tick_budget_get { "entity": 4294967296 }
→ { "budget": { "enabled": true, "max_trees_per_step": 10, ... },
    "stats": { "steps": 120, "trees": 100, "ticks_last_step": 10, "deferred_last_step": 0, ... },
    "agent": { "entity": 4294967296, "tier": 1, "distance": 44.0, "always_on": false,
               "behavior_tree_age": 3, "state_tree_age": null } }
```

### Error codes

| Code | `data.reason` | Meaning |
|---|---|---|
| `ai_squads_unavailable` | | `AiSquadPlugin` is not installed |
| `ai_tick_budget_unavailable` | | `AiPlugin` is not installed |
| `ai_query_unavailable` | | `SpatialQueryPlugin` is not installed (`ai.query_impls`) |
| `F0002` | `bad_params` | the parameters are not an object |
| `F0002` | `missing_param` | `ai.squad_set` with no `squad` and no `create`, unless `token_pool` stands alone |
| `F0002` | `bad_entity` | an entity id is not a number, or `add` / `remove` is not an array |
| `F0002` | `bad_create`, `bad_name`, `bad_rank` | `create` is not a boolean or comes with `squad`; `name` is not a string; a rank is not a whole number |
| `F0002` | `bad_role` | a `roles` row or the `role` object is malformed |
| `F0002` | `role_full` | the role's slots are all taken |
| `F0002` | `bad_blackboard`, `bad_formation`, `bad_tokens` | that parameter is malformed, or a number that must be above 0 is not |
| `F0002` | `nothing_to_set` | `ai.tick_budget_set` names no field |
| `F0002` | `bad_enabled`, `bad_max_trees`, `bad_tiers`, `bad_focus` | that `ai.tick_budget_set` field is malformed |
| `F0003` | `entity_not_found` | a named entity does not exist |
| `F0003` | `not_a_squad`, `not_a_member`, `unknown_role` | the entity has no `AiSquad`; the agent is not (or will not be) a member; the squad has no such role |
| `F0003` | `not_a_pool` | the `token_pool` target has no `AiAttackTokens` once the pool change ran |

## From Rust

<!-- ignore-reason: fragment of a game system: `world`, `agent`, `target` and `priority` come from the caller -->
```rust,ignore
use regia_ai::attack_tokens::*;

match acquire_token(world, agent, target, priority) {
    outcome if outcome.is_held() => { /* attack */ }
    TokenOutcome::Denied => { /* in line: ask again next tick */ }
    _ => { /* no pool, or the agent is dead */ }
}
release_token(world, agent, target);
set_token_capacity(world, target, 1); // evicts the lowest priorities
```

The spans are `ai_tick_plan` and `ai_squad_step`. See [spatial queries](spatial-queries.md) for custom generators
and tests, [AI behavior trees](ai.md) for the trees themselves and [AI perception](ai-perception.md) for what agents
know about their targets.

## Formation assets

A `.formation.ron` file (format version 1, registry `ai-formation`) names the slots instead of one of the three
code shapes. Each slot is an offset in the anchor's frame (x to the right, z backward), an optional role tag, and
the file adds a spacing scale and a fill order. The scale multiplies every offset. An empty fill order keeps
declaration order; otherwise it lists every slot name once, and a lower rank wins a distance tie.

`ai.formation_set` with `shape` set to `ring`, `line` or `wedge` installs that built-in preset (the first ring,
the first line row, or the first four wedge rows) and assigns by cost. `asset` is a path to a `.formation.ron`
file. Pass one of the two, not both. `ai.formation_get` reads the shape, the asset path, the slots and who stands
where.

A slot with a role tag takes only a member whose `AiSquadMember.role` equals that tag. An untagged slot takes
anyone. Assignment is the minimum total horizontal distance: the Hungarian method at 64 members or fewer, and a
greedy pass of `(distance, member id, slot index)` above that. The same members in another order get the same
slots. The sticky rule on this page still applies when no custom asset is set.

A file is refused when it does not parse, names another version, has a non-finite number, repeats a slot name or
offset, is empty, has more than 1024 slots, or has a fill order that is not a permutation of the names. The agent
codes are `unknown_squad`, `unknown_shape` and `invalid_asset` (the message carries `parse`,
`unsupported_version`, `non_finite`, `duplicate_slot`, `too_many_slots`, `empty`, `bad_value` or `bad_fill`).
`shape` and `asset` together, or neither, is `F0002`.

## Moving in formation

On a `NavCrowd`, `set_agent_group` stores a leader and a slot offset on a follower. The leader keeps following
its corridor. A follower steers toward the leader plus that offset, rotated by the leader's heading and projected
onto the mesh. It matches the leader's speed, catches up when the slot is ahead, and slows when it is ahead of
the slot.

When the projected slot is off the mesh, or the leader's corridor is narrower than the shape, that follower
queues in a column behind the leader along the corridor. The column clears once the corridor is wide enough and
the slot is back on the mesh. The `ai_formation` gizmo group draws the slots, the member-to-slot lines and the
column lines.
