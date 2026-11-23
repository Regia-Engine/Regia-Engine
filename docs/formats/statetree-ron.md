# .statetree.ron: AI state tree (v2)

Spec: ADR-1565 (`docs/decisions.md`), `docs/specs/12-assets.md` § 4a (versioning).
Code: `regia_ai::state_tree` (`StateTreeDef`, `StateTree`). Example: [`statetree.example.ron`](statetree.example.ron).
Guide: `book/agents/state-tree.md`.

A `.statetree.ron` declares a hierarchy of states. When a state is active its tasks run; transitions leave it on a
tick, an event or completion. Version **2** (spec 38, ADR-1639); version 1 files still load (v2 only adds optional fields, so the v1 to v2
migration rewrites the version number and nothing else: `regia_ai::state_tree::migrate_ron_v1_to_v2`). Any other
`format_version` is rejected before the body is parsed. The shape is hashed by `cargo xtask check-formats`. `//` comments are read and
dropped on save.

## Schema

```
StateTreeDef {
    format_version: u32,             // 2 (1 is read as 2)
    description: String = "",
    root: String,                    // name of the root state
    seed: u64 = 0,                   // generator seed for Weighted selection
    state_key: String = "",          // blackboard string key that mirrors the active leaf name; "" = off
    ambient: [String] = [],          // v2: BtTask paths ticked first on every tick while the tree runs
    includes: [IncludeDef] = [],     // v2: trees inlined at compile time (<= 4 deep, no cycles)
    states: [StateDef],              // <= 4096, unique non-empty names, any order
}
StateDef {
    name: String,
    children: [String] = [],         // child names, in selection order; each state has at most one parent
    selection: First | Utility | Weighted = First,
    enter: Condition = Always,       // the state can only be entered while this holds
    tasks: [String] = [],            // registered BtTask type paths, run while the state is active
    transitions: [TransitionDef] = [],
    weight: f32 = 1.0,               // for a Weighted parent; <= 0 is never picked
    utility: Option<UtilityDef> = None,   // for a Utility parent; none scores 0
    enter_set: {String: Literal} = {},    // v2: written to the blackboard (key order) when the state is entered
    fallback: Stay | NextEnterable = Stay, // v2: when a transition into this state finds nothing below it
}
Literal = Bool(bool) | Int(i64) | Float(f32) | String(String)
IncludeDef { prefix: String, path: String }     // states become `prefix/<name>`
UtilityDef { key: String, scale: f32 = 1.0, bias: f32 = 0.0 }      // score = bias + scale * blackboard number
TransitionDef {
    trigger: Tick | Event(String) | Succeeded | Failed | Completed,
    condition: Condition = Always,
    priority: i32 = 0,               // higher wins
    target: State(String) | Next | NextEnterable | Parent | Stay | Succeed | Fail,   // v2: NextEnterable, Parent, Stay
    hold: f32 = 0.0,                 // v2: seconds trigger+guard must hold, in fixed steps; not on Event triggers
}
Condition =
    Always | Key(String) | Has(String)
  | Compare { key: String, op: Lt | Le | Eq | Ne | Ge | Gt, value: f32 }
  | Text { key: String, value: String }
  | Not(Condition) | All([Condition]) | Any([Condition])         // nesting depth <= 16
```

`Key` is true for a boolean `true`; a missing key or a value of the wrong type makes every leaf test false.
`Compare` reads the number losslessly (`Blackboard::try_get_float`) and is false for a missing or non-numeric key.

## Validation

`StateTree::compile` / `compile_with` reject: a version other than 1 or 2, no states, more than 4096 states, an empty or duplicate name,
an unknown root, child or target, a state with two parents (or the root as a child), a state the root cannot reach,
an unregistered task path, `Next` on a state without a next sibling, a condition nested deeper than 16, a
non-finite weight, scale, bias or comparison constant, an empty event name, an empty `enter_set` key, a negative or non-finite `hold`, a `hold` on an `Event` trigger
(`HoldOnEvent`), `Parent` on the root (`ParentOfRoot`), a missing, cyclic or over-deep include (`IncludeMissing`,
`IncludeCycle`, `IncludeDepth`). A bad definition builds no tree.

## Includes

`compile_with` takes a loader (project-relative path to RON text). Each include's states are copied in with every
state name, child, `State` target and the root prefixed `prefix/`; the included tree's `seed`, `state_key`, `ambient`
and `description` are ignored, its own includes are expanded first, and its `format_version` may be 1 or 2. The
merged definition (no `includes`) is what `StateTree::def()` returns.
