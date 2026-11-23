# .logic.ron: logic graph (v2)

Spec: `docs/specs/102-logic-graph.md` § 3 (ADR-3355, phase 1 as built: ADR-7201), `docs/specs/12-assets.md`
§ 4a (versioning). Code: `regia_ai::logic` (`LogicGraphDef`, `LogicGraph`). Example:
[`logic.example.ron`](logic.example.ron). Guide: `book/agents/logic-graph.md`.

A `.logic.ron` is a flat list of event → condition → action rules, the graph's locals, its typed entity and
world variables and the seed of its timer jitter. Version 1 held every event arm of spec 102's later phases,
so phase-1 and phase-2 builds read files written for them and reject the then-unbuilt features at compile time
(`unsupported_feature`) rather than at parse time; since phase 3 (ADR-7268) every arm and the `world:` list
compile. Version **2** (v0.7, format batch 2, ADR-8900, P-177) adds the optional `entity_vars` and
`world_vars` lists. A v1 file reads as v2 (its body is a valid v2 body; only the number moves); a version 0
or a version newer than this build reads is rejected before the body is parsed. The shape is hashed by
`cargo xtask check-formats`; `regia upgrade-project` rewrites a v1 file's `format_version` number and keeps
every other byte. `//` comments are read and dropped on save.

A v0.6 build (which reads only v1) refuses a v2 file with `unsupported_version`: "logic graph format_version 2
is not supported (this build reads version 1)".

## Schema

```
LogicGraphDef {
    format_version: u32,              // 1
    description: String = "",
    seed: u64 = 0,                    // SplitMix64 seed of the timer jitter
    locals: [LocalDef] = [],          // <= 64, unique non-empty names without `/`
    world: [String] = [],             // declared world blackboards (at most 16; non-empty, no `/`, unique)
    entity_vars: [EntityVarDef] = [], // v2: <= 64, unique non-empty names without `/`; not written when empty
    world_vars: [WorldVarDef] = [],   // v2: <= 64, a declared board, unique per board; not written when empty
    rules: [RuleDef] = [],            // <= 256, unique non-empty names (a rule's identity on hot reload)
}
LocalDef { name: String, default: Literal }
EntityVarDef { name: String, default: Literal }               // the entity key `name` / `entity/name`
WorldVarDef { board: String, name: String, default: Literal } // the key `world/<board>/<name>`
RuleDef {
    name: String,
    event: LogicEvent,
    priority: i32 = 0,                // higher runs first on the same event; ties in declaration order
    cooldown_ticks: u32 = 0,          // fixed steps after a fire during which the rule matches but does not act
    condition: Condition = Always,    // the state-tree Condition, depth <= 16, keys scoped (below)
    actions: [ActionDef] = [],        // <= 16, run in order
}
ActionDef {
    task: String,                     // logic::SetBlackboard | logic::EmitEvent | logic::EnableEntity |
                                      // logic::DisableEntity | any registered BtTask path
    set: { String: Binding } = {},    // <= 32 entries
}
LogicEvent =
    Start
  | Timer(every_ticks: u32 /* >= 1 */, jitter_ticks: u32 = 0)
  | BlackboardChanged(key: String)
  | EntityEvent(String /* non-empty */)
  | OnSequenceMarker(sequence: String, marker: String)   // inbox event sequence_marker:<sequence>#<marker>
  | OnNetMessage(message: String)                        // inbox event net_message:<message>
  | InputAction(action: String)                          // inbox event input_action:<action>
Binding = Literal(Literal) | BbRef(String /* scoped key */)
Literal = Bool(bool) | Int(i64) | Float(f32 /* finite */) | String(String)
Condition = Always | Key(String) | Has(String) | Compare(key, op: Lt|Le|Eq|Ne|Ge|Gt, value: f32)
          | Text(key, value: String) | Not(Condition) | All([Condition]) | Any([Condition])
```

## Keys and scopes

`local/x` names a graph local, `entity/x` or a bare `x` the entity's `Blackboard`, `world/<board>/x` the
world blackboard `<board>`, which the graph must declare in `world:`. Any other `/` prefix, or an undeclared
board, is `unknown_scope`. Condition keys, `BbRef`s, `BlackboardChanged` keys and
the literal `key` of `logic::SetBlackboard` are checked at compile time; a key read through a `BbRef` at
run time is checked when the action runs (a bad one fails that action).

## Semantics that the format fixes

- **Timers** count fixed steps from the graph's start tick (tick 1). Period *p* of an arm is due on tick
  `p * every_ticks + draw % (jitter_ticks + 1)`, where `draw` is the next output of one SplitMix64 stream per
  graph instance seeded with `seed`: one draw per period, taken when the period is scheduled (at the start
  tick for period 1, in arm order; then when the previous period fires, arms in order, periods ascending).
- **BlackboardChanged** fires once per tick for a key written since the graph's last tick, whether or not
  the value changed (writes through `Blackboard::insert`/`remove` are counted; a raw write into
  `Blackboard::values` is seen only when the value changes). The start tick takes the baseline.
- **Event order** in a tick: `Start`, timers, `BlackboardChanged` (key order), then the inbox (FIFO, at most
  64; the oldest overflow is dropped and traced). Events emitted during the tick wait for the next one.
- **Built-in action parameters**: `logic::SetBlackboard { key, op: "set" | "increment" | "append", value }`,
  `logic::EmitEvent { event, target? }`, `logic::EnableEntity { target? }`, `logic::DisableEntity { target? }`
  (`target` is a `BbRef` to an entity value; unknown parameters are `bad_value`). A registered task's `set`
  bindings are written into the entity blackboard before each of its ticks.

- **Typed entity and world variables** (v2): a declared variable's type is its default's literal kind. When
  the graph starts (its first tick, and again after a restart) each declared default is written into the
  entity's `Blackboard` (one is added when the entity has none) or the world board, unless the key is already
  there; the seed comes before the `BlackboardChanged` baseline, so it fires no rule. At compile time a
  built-in `logic::SetBlackboard` with a literal `key` naming a declared variable must keep its type: `set`
  of a literal of another kind, `increment` of a non-number or `append` to a non-string variable is
  `bad_value`. A world variable whose board is not in `world:` is `unknown_scope`; an empty or `/` name, a
  non-finite float default or more than 64 of either list is `bad_value`; a repeated name is
  `duplicate_name`.

## Compile errors

`parse`, `unsupported_version`, `too_many_rules`, `too_many_actions`, `condition_too_deep`, `duplicate_name`,
`unknown_task`, `unknown_scope`, `bad_value`, `bad_op`, `missing_param`, `unsupported_feature` (see
`LogicGraphError::reason` and `book/agents/logic-graph.md`).
