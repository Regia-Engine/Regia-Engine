# 38 — State trees

Status: proposed (ADR-1578). Plan item G7, target v0.4.0. Crate: `regia_ai` (module `state_tree`); agent surface in
`regia_agent`; authoring in `regia_editor` (State Tree Editor panel, `regia_ai::state_tree_edit`, ADR-3342,
`book/agents/state-tree-editor.md`).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: v1 is **implemented** on `origin/integ/ext2` (ADR-1565: `regia_ai::state_tree::{def, runtime, mod}`,
`docs/formats/statetree-ron.md`, `book/agents/state-tree.md`); this spec records what v1 already does (§ 2) and the
gaps a v2 closes (§ 3 onward).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-gameplay-state-trees` (it carries open
close-paraphrase review items; this spec was re-derived from the built v1 design and Regia's own needs, and none of
its wording or term lists is kept).

## 1. Purpose and scope

Behavior trees (`regia_ai::tree`, spec 22 § T2.2) decide *what to do next* by re-walking from the root. A state tree
is for behavior best described as *being in a state* with explicit reasons to leave it (a guard that is idle, alert,
chasing; a door; a boss with phases). Both share the `Blackboard`, `BtTask` and `BtTaskRegistry`, so one task
library serves both. This spec covers (a) a precise record of v1 as built, and (b) the v2 additions: per-state
blackboard writes on entry, held (debounced) transitions, ambient tasks, more destinations, reusable sub-trees by
inclusion, hot reload, debug controls, and a small agent surface. Out of scope: § 11.

## 2. Baseline: v1 as built (ADR-1565, spec records gaps)

- Asset `.statetree.ron`, `StateTreeDef { format_version: 1, description, root: String, seed: u64, state_key: String,
  states: Vec<StateDef> }` (≤ 4096 states). `StateDef { name, children, selection: First | Utility | Weighted, enter:
  Condition, tasks: Vec<String>, transitions: Vec<TransitionDef>, weight: f32, utility: Option<UtilityDef> }`.
  `TransitionDef { trigger: Tick | Event(String) | Succeeded | Failed | Completed, condition: Condition, priority:
  i32, target: State(String) | Next | Succeed | Fail }`. `Condition` = `Always | Key | Has | Compare | Text | Not | All |
  Any` (depth ≤ 16). Full schema: `docs/formats/statetree-ron.md`.
- `StateTree::compile(&def, &BtTaskRegistry)`; `StateTree::tick(&mut BtContext)`. ECS: `StateTree` (not reflected),
  reflected `StateTreeStatus { status, active_path, ticks, trace }` and `StateTreeInbox { events, dropped }` (`dropped`: ADR-5905).
  `AiPlugin` ticks every tree in `FixedUpdate`/`AiStep` after behavior trees (system span `state_tree_tick`).
- Tick order: queued events (each takes at most one transition; an unmatched event is dropped), tasks of every
  active state root to leaf, then tick/completion transitions (at most one per tick). Candidate order: higher
  `priority`, then deeper state, then declaration order; a candidate whose target cannot be entered is skipped
  (trace `rejected`). Entering re-enters the target itself even if active, enters inactive ancestors, then descends
  by `selection` (`Utility` = `bias + scale * blackboard number`, ties to the earlier child; `Weighted` = seeded
  SplitMix64 draw). Leaving resets the tasks that ran, leaf first. `MAX_PENDING_EVENTS` = 64, trace = 64 lines.
  A finished tree ignores ticks until `StateTree::restart`.
- Agent: only generic `scene.get_entity` / `scene.set_property` on `StateTreeStatus` and `StateTreeInbox`;
  no methods of its own, no error codes.

Gaps (what v1 cannot do): tasks take no parameters; no timing on transitions; no always-running tasks that publish
data before selection; no `Parent` destination; no reuse of one tree inside another; definition edits need a full
restart; no debug controls; validation cannot be run on text without building an entity. § 3-§ 8 close these.

## 3. Decisions (ADR-1585)

- **Task parameters** stay out of `BtTask` (a signature change would touch behavior trees). A state instead has
  `enter_set`, a map of blackboard key to literal written when the state becomes active, so tasks read their
  configuration from the blackboard as they already do.
- **Work that runs for the whole life of the tree** (publishing data before selection, tree-wide tasks) is one concept,
  **ambient tasks**: ordinary `BtTask`s ticked every tick before anything else while the tree runs.
- **Sub-trees are inlined at compile time** (`include`), not run as separate frames. Deterministic, no second
  active chain, and the agent's `active_path` stays a single list.
- **Hot reload** keeps the active path when every active state still exists by name under the same parent chain from
  the root (ADR-1639 (6)); otherwise restart.
- **No callback subscriptions, request queue, or event payload types** in v2. Events stay named strings; a task that needs a
  value reads the blackboard. (A new need ships as a v3 field with a migrator.)

## 4. Format v2 (`.statetree.ron`, `format_version: 2`)

v2 is a superset of v1: every v1 field keeps its name and meaning; new fields default so a migrated file behaves
identically. Frozen-format rule (`docs/specs/12-assets.md` § 4a): bump `STATE_TREE_FORMAT_VERSION` to 2, add a
migrator `v1 -> v2` (sets `format_version: 2`, adds nothing else), register it so `regia upgrade-project` rewrites
`.statetree.ron` files, keep v1 readable through the migrator, and re-record the hash with
`cargo xtask check-formats --update` (lead runs it). Files with a newer version are refused with the existing
`StateTreeError::UnsupportedVersion`.

New fields:

```ron
StateTreeDef(
  format_version: 2,
  root: "Root",
  ambient: ["game::TrackThreat"],                 // BtTask paths, ticked first each tick, started with the tree
  includes: [ (prefix: "Combat", path: "ai/combat.statetree.ron") ],
  states: [
    (name: "Idle",
     enter_set: { "mood": String("calm") },       // literals written on entry (before tasks run)
     transitions: [
       (trigger: Tick, condition: Compare(key: "threat_dist", op: Lt, value: 12.0),
        hold: 0.25, target: State("Alert")),
       (trigger: Event("hit"), target: Parent),
       (trigger: Event("noise"), target: Stay),   // consumes the event, changes nothing
     ]),
    (name: "Search", fallback: NextEnterable, ...),
  ],
)
```

- `StateDef.enter_set: BTreeMap<String, Literal>` (default empty); `Literal` is a new serde enum `Bool(bool) | Int(i64) |
  Float(f32) | String(String)` converting into `BbValue` (`BbValue` itself is reflect-only, and vec/entity literals are
  not authorable). Applied in key order when the state is entered,
  after `enter` passes and before its tasks' first tick. Keys must be non-empty (`BadValue` otherwise).
- `TransitionDef.hold: f32` (seconds, default 0): trigger and condition must both hold continuously for this long,
  measured in fixed steps (`ceil(hold / step)` ticks; built as `max(1, ceil(hold / step - 0.001))`, ADR-1639 (3), so
  an f32 rounding step does not make 0.25 s at 1/60 s 16 ticks); the counter resets when either stops holding or the owning
  state is left. A held transition counts as matching from the tick the counter is reached for as long as both keep
  holding (ADR-1809: one rejected or outranked on that tick fires later). Applies to
  `Tick`, `Succeeded`, `Failed` and `Completed`; an `Event` trigger with `hold > 0` is a compile error.
  Non-finite or negative values are `BadValue`.
- `Target` gains `Parent` (re-enter the owning state's parent and descend by its selection; error if the owning
  state is the root) and `Stay` (match, consume the trigger, no state change; useful to swallow an event so a
  lower-priority transition does not take it). `Next` keeps v1 meaning: the next sibling in declaration order.
  `NextEnterable` is the first later sibling whose `enter` condition and descent succeed.
- `StateDef.fallback: Fallback = Stay | NextEnterable` (default `Stay`): when entering this state by a transition
  target fails below it (its children offer nothing to enter), `NextEnterable` tries the following siblings
  instead of rejecting the transition. The start descent has no fallback.
- `StateTreeDef.ambient: Vec<String>`: ambient tasks run before event handling, in list order, every tick; their
  result is ignored. They are `reset` on restart or finish, so each run starts them fresh (built form, ADR-5779: the
  task objects are constructed once at compile, like state tasks, not on every start). Unregistered
  path: `UnknownTask`.
- `StateTreeDef.includes: Vec<IncludeDef { prefix: String, path: String }>`: at compile, the states of the
  referenced tree are copied in with every state name (and every child, target and `root` reference inside it) prefixed
  `prefix/`. The included root is reachable as `prefix/<its root>`; its `seed`, `state_key` and `ambient` are ignored
  and `format_version` must be 1 or 2 (older versions migrate in memory). Compile takes a loader
  (`&dyn Fn(&str) -> Option<String>`); depth ≤ 4, cycles are an error. Includes are expanded before name checks, so
  all v1 validation applies to the merged tree. The merged definition is what `StateTree::def()` returns.
- New compile errors (`StateTreeError` variants, appended): `IncludeMissing(String)`, `IncludeCycle(String)`,
  `IncludeDepth`, `HoldOnEvent(String)`, `ParentOfRoot(String)`. Their agent reasons are the snake_case names
  (`include_missing`, `include_cycle`, `include_depth`, `hold_on_event`, `parent_of_root`); v1 errors map to
  `parse`, `unsupported_version`, `empty`, `too_many_states`, `empty_name`, `duplicate_name`, `unknown_state`,
  `multiple_parents`, `unreachable`, `unknown_task`, `no_next_sibling`, `condition_too_deep`, `bad_value`.
  ADR-4543 adds `bad_behavior_tree` for a task path `bt:<file>.bt.ron` that cannot be loaded or compiled.

## 5. Rust API additions (`regia_ai::state_tree`)

```rust
impl StateTree {
    /// Compile with includes resolved through `load` (path -> RON text). `compile` stays as v1: a definition with
    /// includes fails there with `IncludeMissing` (built form, ADR-5779).
    pub fn compile_with(def: &StateTreeDef, registry: &BtTaskRegistry, load: &dyn Fn(&str) -> Option<String>)
        -> Result<Self, StateTreeError>;
    /// Replace the definition in place (hot reload): keep the active path and event queue when every active
    /// state name still exists in `next` and the new tree compiles; otherwise exit all and return to NotStarted.
    pub fn reload(&mut self, next: StateTree, ctx: &mut BtContext<'_>) -> ReloadOutcome; // Kept | Restarted
    pub fn pause(&mut self); pub fn resume(&mut self); pub fn is_paused(&self) -> bool;
}
#[derive(Component, Reflect, Default)] #[reflect(Component, Default)]
pub struct StateTreeDebug { pub disabled: Vec<String>, pub pause_on_enter: Vec<String> }
```

- `pause`: `tick` returns immediately (no timers, tasks or events advance; queued events keep waiting).
  `StateTreeStatus` gains `paused: bool` and `held: Vec<String>` (transition labels with a running `hold` counter);
  both new fields at the end; this is a host-visible change (struct literals), recorded in the release fragment.
  (`StateTreeStatus` is reflected, not serde-serialized, so there is no `#[serde(default)]` to add; built form,
  ADR-5779.) The ECS system leaves a paused tree's `StateTreeInbox` alone, so events sent during a pause wait there
  in order and none is dropped (ADR-5779). While paused (or while a layer parks the agent) the inbox keeps at most
  `MAX_PENDING_EVENTS` (64) names, the newest, in order: each fixed step the oldest past that are dropped and counted
  in `StateTreeInbox.dropped` (a new `u64` field at the end, never reset), and an inbox's first drop logs one
  warning. So a long pause cannot grow the inbox without bound, and what it keeps fits the tree's queue on resume
  (ADR-5905).
- `StateTreeDebug` (dev builds only, `cfg(debug_assertions)`; absent in shipping): a name in `disabled` makes selection and entry skip that state (as if its
  `enter` failed); a name in `pause_on_enter` calls `pause()` at the end of the tick in which that state was
  entered. Never persisted in the asset.
- Hot reload is host-driven: the editor or a game system compiles the new RON text with `compile_with` and calls
  `reload` (agents use `statetree.reload`, § 7). `regia_ai` gains no asset dependency; the include loader is a
  closure the host supplies. No new crate edge (`cargo xtask check-layers`).

## 6. Per-tick order in v2

1. If paused: return. 2. Ambient tasks tick, list order. 3. Queued events as v1 (one transition each).
4. State tasks root to leaf as v1. 5. Held-transition counters advance for every active state's transitions whose
trigger and condition hold (reset the others). 6. Tick/completion transition pool as v1, with `hold` honored;
at most one transition per tick. 7. Apply `StateTreeDebug` pause requests; write `state_key` and the status.
`enter_set` writes happen during step 3 or 6 as part of applying the winning transition, before the next tick's
tasks. Nothing depends on hash order (`BTreeMap` everywhere).

## 7. Agent surface (`book/agents/state-tree.md`, existing page extended; `book/COVERAGE.md` rows added)

The generic `scene.*` path stays. Added methods (class; params -> result; `path` is project-relative and read through the project asset root):

| Method | Class | Params | Result |
|---|---|---|---|
| `statetree.compile` | read | `text` or `path`, optional `includes: bool` (default true) | `{ ok, states, errors: [{reason, state?, detail}] }` |
| `statetree.control` | play | `entity`, `op: "pause"\|"resume"\|"restart"` | `{ status, paused }` |
| `statetree.reload` | play | `entity`, `text` or `path` | `{ outcome: "kept"\|"restarted" }` (compile errors as `reason`) |
| `statetree.debug` | play (dev) | `entity`, `disabled?: [name]`, `pause_on_enter?: [name]` | `{ disabled, pause_on_enter }` |

Errors use the neighbors' convention: `F0002` with `data.reason`: `entity_not_found`, `no_state_tree`, `bad_op`,
`state_not_found` (debug name unknown), `dev_only` (shipping build), `missing_param`, plus the compile reasons of
§ 4. Without a `StateTree` on the entity, `control` and `debug` answer `no_state_tree`. The page gains: the v2
fields, a worked guard example, how to read a trace to find why a transition did not fire, the reason list.

## 8. Tests and acceptance (tests first)

Unit tests beside the module; scenario tests in `crates/regia_ai/tests/state_tree_v2.rs` (built form, ADR-5779:
every v2 scenario is a unit test in `crates/regia_ai/src/state_tree/tests.rs`, agent tests in
`crates/regia_agent/src/state_tree.rs`, allocation budgets in `crates/regia_ai/tests/state_tree_alloc.rs`). Fixed steps, no sleeps,
seeds fixed, temp dirs through `regia_test_utils::unique_temp_dir`.

1. **Migration.** A v1 file migrates to v2 with byte-stable output after the version line; `regia upgrade-project`
   test fixture; v3 refused; v1 behavior identical (re-run the existing v1 scenario tests against a migrated copy).
2. **`enter_set`.** Values appear in the blackboard before the first tick of the entered state's tasks; keys apply in
   sorted order; leaving does not remove them.
3. **`hold`.** A `Tick` transition with `hold: 0.25` at a 1/60 s step fires on exactly step 15; dropping the
   condition on step 10 resets it; leaving and re-entering the state resets it; `hold` on `Event` is a compile error.
4. **Destinations.** `Parent` re-enters the parent and descends by its selection; root `Parent` is `parent_of_root`;
   `Stay` swallows an event so a lower-priority transition does not fire; `NextEnterable` skips a sibling whose
   `enter` fails, `Next` does not; `fallback: NextEnterable` vs `Stay` when the target's children offer nothing.
5. **Ambient.** Ambient tasks tick before events; a value they write is visible to the first selection's `enter`
   conditions; they reset on restart.
6. **Includes.** Inlined names are prefixed; targets inside the included tree resolve; missing, cyclic (A includes
   B includes A) and depth 5 includes give the three errors; the merged `def()` round-trips through RON.
7. **Hot reload.** Same names: path kept, ticks continue, queue kept (`Kept`); a renamed active state: `Restarted`;
   a reload that fails to compile (through `statetree.reload`) leaves the old tree running and reports the compile reason.
8. **Pause and debug.** Paused tree advances nothing for 100 ticks and then resumes with the same trace as an
   unpaused control run (identical, not shifted: a paused tick is not counted, ADR-1639 (4)); `disabled` skips a state; `pause_on_enter` pauses at the end of that tick.
9. **Determinism.** Same asset, seed, blackboard writes and events over 2,000 ticks: identical `StateTreeStatus`
   (including trace) in two runs.
10. **Loader fuzz.** `cargo fuzz` target on `StateTreeDef::from_ron` + `compile_with` (never panics).
11. **Agent (rule 8).** Each method: success, missing parameter, each reason; dogfood script: spawn a guard, send
    `heard` through `StateTreeInbox`, `play.wait_until` on `StateTreeStatus.active_path`, pause with `statetree.control`.

Acceptance: `cargo test -p regia_ai -p regia_agent`; `cargo xtask check-formats --update` (lead) then `check-formats`;
`cargo xtask agent-coverage`; `cargo xtask check-layers`; `cargo xtask manual` (lead).

## 9. Budgets and platforms

1,000 trees of 6 states with `Tick` guards on blackboard compares: under 1 ms per fixed step on one desktop core in
(asserted as allocation counts with a counting allocator, never wall time: zero heap allocations per
tick in steady state; trace lines are written only when something happens). Android aarch64: same code. No
platform-specific code; all arithmetic is compares, a linear score, and the SplitMix64 draw, so results match on
Linux, Windows, Android (macOS testing). Span `state_tree_tick` stays per system run.

## 10. Provenance

Hierarchical state machines with guarded transitions: Harel, "Statecharts: a visual formalism for complex systems"
(1987); utility selection: Mark, "Behavioral Mathematics for Game AI" (2009); SplitMix64: Steele, Lea, Flood, "Fast
splittable pseudorandom number generators" (2014). Shared row: `docs/provenance.md`, "AI / state trees (spec 38)";
original code. This spec's own record: behavior only
from the public-sources set area above; Regia names and ordering are Regia's own.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 10a. As built: panel authoring parity (sprint w9-anim-authoring)

- The State Tree Editor panel (`crates/regia_editor/src/state_tree_editor/panel.rs`) now edits the tree-level fields
  (`set_tree`: description, seed, state key), the ambient tasks (`set_ambient`), the includes (`add_include`,
  `set_include`, `remove_include`, and **List under** a selected state, a `reparent_state` of the included root read
  from the file, since an include's states are unreachable until listed) and a state's utility score (`set_state`
  with `utility` or `clear_utility`). Every control applies the shared `regia_ai::state_tree_edit` ops as one undo
  step, so a save writes the canonical bytes `ai.statetree_edit` writes. An include that leads back to the open
  file is an `include_cycle` error and the save is refused with `invalid_tree`. No format change. Tests:
  `state_tree_editor::authoring_tests`.

## 11. Out of scope

A graphical editor for the asset (the asset is hand/agent editable; the State Trees panel of ADR-2030 debugs a running tree, it does not author one). Amended by ADR-3342: the State Tree Editor panel and the `ai.statetree_edit` / `ai.statetree_validate` file methods now author the asset (same format, version 2). ADR-4543 adds a behavior-tree node that runs a state tree and a state-tree task that runs a behavior tree. Callback subscriptions, buffered transition requests, event payloads, per-tree schemas,
scheduled sleeping ticks, parallel sub-trees, debug recording to file/replay, network replication, task inputs bound
to anything other than blackboard keys, event bubbling beyond the ancestors of an active state (v1 already matches events on every
active state).

### Owner decisions

None.
