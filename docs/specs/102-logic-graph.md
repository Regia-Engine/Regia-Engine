# 102 — Logic graph

Status: proposed (ADR-3355). Target: v0.4.0 (Tier 2 wave, after M11; not on DO27's critical path).
**As built (phase 1, 2026-11-12, ADR-7201):** P1 exists — `regia_ai::logic` (`LogicGraphDef`,
`LogicGraph`, `LogicInbox`, `LogicGraphStatus`, `LogicAuthority`), `.logic.ron` v1
(`docs/formats/logic-ron.md`) and `logic.compile/control/emit/reload/status`
(`book/agents/logic-graph.md`); ADR-7201 records where the build refines this spec.
**As built (phase 2, 2026-11-12, ADR-7224):** P2 exists — `regia_ai::logic_edit` (`LogicGraphDoc`,
`LogicOp`, `validate`), the `Panel::LogicGraph` egui-snarl canvas (`regia_editor::logic_graph_editor`), the
`x.logic.layout.ron` position sidecar and `editor.logic_open/state/edit/save/validate`; ADR-7224 records the
node model, lowering and reload-on-save rules. Of phase 4, the breakpoints are built (P4.1/P4.2:
`LogicGraphDebug`, `logic.debug`, `logic.control step`; ADR-7224 (9)); the allocation-free steady state
(P4.3) is built since phase 4.3 (below).
**As built (phase 3, 2026-11-12, ADR-7268):** world blackboards (`LogicWorldBlackboards`, a declared
`world:` list compiles; `logic.world`, `logic.world_set`), the three arms as inbox events
(`sequence_marker:<seq>#<marker>`, `input_action:<action>`, `net_message:<id>`), the presentation flag
(`BtTaskRegistry::mark_presentation`) and, in the umbrella crate (`regia::logic_bridge`, feature
`logic` — not `regia_framework`, which cannot depend on `regia_ai`), `logic::PlaySequence`,
`logic::PlayCue`, `logic::CallLua`, the marker / input / Lua / net routes and the `NetRole` authority.
ADR-7268 records each refinement (untargeted `regia.emit` reaches `LogicModHost` entities; the net
stamp is the `net_message_tick` blackboard key).
**As built (phase 4.3, 2026-11-12, ADR-7282):** the allocation-free steady state — the trace is a ring of
typed records formatted on read, `LogicGraphStatus` refreshes in place (`trace_mirror`), and
`logic::SetBlackboard` / task parameters / watches write in place (`Blackboard::assign`, `append_str`);
`crates/regia_ai/tests/logic_graph_alloc.rs` asserts zero allocations for 100 × 25 over 1,000 steps. The
wall-clock half of acceptance 3 is an `#[ignore]`d hand-run test; its `docs/perf-log.md` number is not
recorded yet (quiet-host measurement for the lead).
**As built (finish, 2026-11-15, w11-logic-graph-finish):** all four phases exist. The editor hooks: Content ▸
**New ▸ Logic graph** (`NewAssetKind::LogicGraph`, `editor.create_asset { kind: "logic_graph" }`) writes the panel's
empty v1 graph, and the asset-open dispatcher's `logic_graph` row (`.logic.ron` → `editor.logic_open`, panel
`Logic Graph`) opens it from a Content Browser double click, an Output Log link and `editor.open_asset`
(`logic_graph_editor::tests::content_new_logic_graph_opens_edits_saves_and_the_runtime_runs_it`,
`asset_open::tests`). The P4 "agent dogfood script in CI" is `crates/regia_editor/tests/logic_graph_dogfood.rs`, run
by the gate as `cargo test -p regia_editor --test logic_graph_dogfood`. Acceptance, test by test: P2.1
`logic_edit::tests::p2_1_a_definition_round_trips_exactly_and_resaves_byte_identical`,
`logic_graph_editor::tests::p2_1_doc_to_snarl_to_doc_preserves_graph_and_layout`; P2.2
`p2_2_wiring_lowers_to_the_expected_condition_trees`, `p2_2_pin_type_checks_refuse_bad_wires`; P2.3
`p2_3_every_op_is_one_undo_step_and_matches_the_agent_path`; P2.4
`p2_4_each_issue_code_appears_in_the_panel_list_and_logic_validate`; P2.5
`p2_5_live_activity_variables_and_reload_on_save`; P3.1-P3.5 `crates/regia/tests/logic_integrations.rs` (`p3_1_*`,
`p3_2_*`, `p3_3_*`, `p3_4_*`, `p3_5_*`) and `regia_ai` `logic::p3_tests`; P4.1/P4.2 `logic::debug_tests::p4_1_*`,
`p4_2_*` and the dispatcher's `debug_breakpoint_step_and_resume_through_the_dispatcher`; P4.3
`crates/regia_ai/tests/logic_graph_alloc.rs`; P4.4 the agent page `book/agents/logic-graph.md` and its
`book/COVERAGE.md` rows (`cargo xtask agent-coverage`, `editor_method_snapshot`). User guide:
`book/user-guide/editor-guide/logic-graph.md`.
Before that, the v0.5.0 burndown audit (2026-11-10, ADR-5609) found nothing built.
Crate: `regia_ai` (new module `logic`, next to `state_tree` and `tree`); editor panel in `regia_editor`;
agent surface in `regia_agent`; the Lua bridge in `regia_script_lua` adds one task family.

## 1. Purpose and scope

A **logic graph** is a visual, node-based way for a designer (not a programmer) to wire gameplay:
*when event E happens, if condition C holds, run actions A1, A2, …*. It is the event → condition →
action (ECA) model edited as a node graph: event nodes on the left, condition nodes in the middle,
action nodes on the right, wires defining the rules.

Regia already has three complementary behavior systems, and this spec deliberately builds on them
instead of adding a fourth silo:

- **State trees** (spec 38) decide *which state an agent is in*, with guarded transitions.
- **Behavior trees** (spec 22 § T2.2) re-walk a task tree every tick to decide *what to do next*.
- **Lua mods** (spec 84) are sandboxed code for modding, not for per-level designer wiring.

What is missing is the layer a level designer reaches for first: *"when the player enters this
trigger, if the door is unlocked, play the creak sequence and open the door."* Today that is Rust
code or a Lua mod. The logic graph fills that gap; it does not replace any of the three systems
(§ 12).

The designer authors one `.logic.ron` asset per graph in a new **Logic Graph** editor panel
(egui-snarl node canvas, the same widget as the material and procedural graph editors), or
entirely through the agent protocol (spec 23: text-first, self-describing, every capability
agent-accessible). A graph instance runs as a component on an entity, ticked in `FixedUpdate`.

This spec covers: the runtime model and why it is its own interpreter compiled from a flat rule
list; events, conditions, actions and how Rust and Lua register node behavior; variables and
blackboard scopes; the editor UX; hot reload; the `.logic.ron` frozen format; determinism and
networking; debugging; performance budgets; the agent surface; and a phased plan with acceptance
tests. Rule 8 (agent-native) applies from phase 1.

Cleanroom: the design derives from Regia's own state-tree/behavior-tree runtimes and from public
literature (§ 11). No proprietary engine source or terms; "logic graph" is the product name.

## 2. Runtime model: a compiled rule interpreter, not a state tree and not a BT

**Decision: a graph compiles to its own small interpreter.** `LogicGraphDef` (plain serde data) →
`LogicGraph::compile(&def, &BtTaskRegistry)` → a validated, resolved rule list held in a flat
arena, deliberately mirroring how `BehaviorTreeDef` compiles (`crates/regia_ai/src/tree.rs`), so
the definition stays `Reflect` + serde and the compiled form owns no asset data.

Rejected alternatives, with reasons:

- **Compile to a state tree.** State-tree semantics are hierarchical states with selection
  (`First | Utility | Weighted`), `enter` conditions and state targets. A logic graph is a *flat*
  set of independent event rules; forcing it through state selection would make the designer model
  "rules that are always armed" as fake states and would drag AI-only concepts (utility, weighted
  draws) into level scripting. The two systems stay separate but share machinery (below).
- **Compile to a behavior tree.** BTs are per-agent decision logic with `Running` continuation
  semantics re-walked from the root each tick. Event-driven one-shot rules (open the door *once*
  when triggered) fit ECA firing, not a re-walked task tree. Long-running activity that *does*
  fit a BT (patrol, chase) is referenced from an action, not inlined.
- **Interpret the document directly.** The document is compiled and validated up front (unknown
  task paths, dangling wires, bad values are load-time errors, mirroring `StateTreeError`), so the
  per-tick path touches only resolved indices — this is what keeps the steady-state tick
  allocation-free.

What the interpreter *does* reuse, unchanged, from the existing AI runtimes:

| Machinery | Source | Reuse |
|---|---|---|
| `Blackboard`, `BbValue` | `regia_ai` | variable storage in all three scopes (§ 5) |
| `Condition` enum (`Always/Key/Has/Compare/Text/Not/All/Any`) | `regia_ai::state_tree::def` | the leaf condition vocabulary (§ 4.2); scope resolution is new logic-runtime code |
| `BtTaskRegistry` | `regia_ai` | the action library: any registered `BtTask` is an action (§ 4) |
| `AiTickBudget` (ADR-2709) | `regia_ai::tick_budget` | shared per-tick budget, as a new `TreeKind::Logic` (§ 2) |
| Fixed-step tick order, spawn order | `AiPlugin` (spec 38 § 6) | determinism (§ 8) |
| Inbox + bounded trace + status component pattern | spec 38 v2 | events, observability, debugging (§ 4, § 9) |

Module placement: `regia_ai::logic`, not a new crate. The rule evaluator, blackboards, condition
set and task registry all live there today; a separate `regia_logic` crate would duplicate the
registry plumbing and add a layer edge for no gain. `regia_ai` keeps its `default-features = false`
`regia_framework` dependency and gains **no** sequencer, audio, net or script dependency:
presentation actions are registered from `regia_framework` through the task registry at app
composition (§ 4.3), and the Lua call action is registered by the umbrella crate `regia`, which
already optionally depends on both `regia_ai` and `regia_script_lua` (§ 4.4) — the alternative, a
`("regia_script_lua", "regia_ai")` layer allowlist entry, is rejected to keep same-layer edges
minimal.

Tick order per fixed step (one system, span `logic_graph_tick`, run after behavior trees and state
trees in the same `AiStep` set, sharing `AiTickBudget` as `TreeKind::Logic`):

1. If paused: return (no timers, inbox or actions advance).
2. Advance `Timer` event arms that are due this tick and enqueue their events into this tick's
   queue.
3. Detect `BlackboardChanged` arms by checking the watched keys' write counters (§ 4.1) and
   enqueue one coalesced event per key, in key order.
4. Append the inbox snapshot: everything pushed to `LogicInbox` since the previous tick's drain,
   in FIFO order (`MAX_PENDING_EVENTS` = 64, same bound as state trees; overflow drops the oldest
   with a trace line, never panics). Events pushed **during** this tick (by an action's
   `EmitEvent`, by `logic.emit`, by gameplay code) are not in the snapshot; they are delivered in
   step 4 of the **next** tick. A self-emitting rule pair (A on X emits Y, B on Y emits X) therefore
   advances exactly one hop per tick, like the state tree's one-transition-per-tick rule
   (`crates/regia_ai/src/state_tree/runtime.rs`), never ping-pongs inside one step, and cannot
   spin past the event cap.

   **Step 4b.** Tick each suspended chain (§ 4.3) in (priority, declaration) order — the same order rules
   evaluate in — one task tick per chain. A chain whose rule fires in step 5 starts and takes
   its first task tick in the same step, immediately after that rule's evaluation.
5. For each queued event in order — timers, then `BlackboardChanged` in key order, then the inbox
   FIFO — evaluate rules in deterministic order: `priority` descending, then declaration order.
   A rule matches when its event arm equals the event and its condition tree evaluates true
   (§ 4.2). A matched rule fires its action chain (§ 4.3); an action that fails stops the chain
   and writes a trace line (`action_failed`); the remaining rules still evaluate.
6. Refresh `LogicGraphStatus` (§ 9).

A rule fires at most once per event per tick; a rule with no inbox event arm (event = `Start` or a
`Timer`/`BlackboardChanged` arm) is evaluated once per tick from the step-2/3 queue instead of the
inbox drain. Nothing depends on hash order (`BTreeMap` everywhere, like spec 38 § 6). Per-tick work
is bounded by construction: at most `MAX_PENDING_EVENTS` (64) events × `MAX_RULES` (256) rule
evaluations = 16,384, before any action ticks.

## 3. The document (`LogicGraphDef`, `.logic.ron` v1)

```ron
LogicGraphDef(
  format_version: 1,
  description: "Gatehouse door logic",
  seed: 7,                                  // Timer jitter RNG seed (SplitMix64, § 4.1)
  locals: [ (name: "opened_count", default: Int(0)), (name: "door_speed", default: Float(2.0)) ],
  world:   [ "gatehouse" ],                 // declared world blackboards (non-empty is
                                           // `unsupported_feature` until P3, § 13)
  rules: [
    ( name: "open_door",
      event: EntityEvent("trigger_enter"),   // § 4.1
      priority: 10,
      cooldown_ticks: 0,
      condition: All([ Has("door_ref"), Not(Key("locked")) ]),
      actions: [
        ( task: "logic::SetBlackboard", set: { "key": Literal(String("local/opened_count")),
                                               "op": Literal(String("increment")) } ),
        ( task: "game::OpenDoor", set: { "open_door/speed": BbRef("local/door_speed") } ),
      ]),
    ( name: "raise_alarm",
      event: EntityEvent("trigger_enter"),
      priority: 20,
      condition: Key("locked"),
      actions: [ ( task: "logic::EmitEvent", set: { "event": Literal(String("alarm")),
                                                    "target": BbRef("entity/door_ref") } ) ]),
  ],
)
```

This example uses only P1 tasks and the real `Condition`/`Binding` shapes: leaf conditions are
`Always`, `Key("name")`, `Has("name")`, `Compare { key, op, value: f32 }`, `Text { key, value }`,
`Not/All/Any` (the existing enum in `crates/regia_ai/src/state_tree/def.rs` — note `Compare`
compares a *number* against an `f32`, so a boolean guard is `Key`/`Not(Key(..))`, not
`Compare(.., Bool(..))`), and a binding is `Literal(Bool(..)|Int(..)|Float(..)|String(..))` or
`BbRef("scope/key")` (§ 4.3). P1 acceptance test 1 parses this document verbatim.

Limits (compile errors past them): `MAX_RULES` = 256, `MAX_ACTIONS_PER_RULE` = 16, condition depth
≤ 16 (`MAX_CONDITION_DEPTH`, reused), `MAX_LOCALS` = 64, action `set` map ≤ 32 entries. Rule and
local names are unique non-empty strings; `MAX_PENDING_EVENTS` = 64.

`cooldown_ticks` (default 0): after a rule fires, it cannot fire again until that many fixed steps
have passed; the counter resets on restart. A rule still *matches* during its cooldown (the trace
shows the match) but does not run its actions, so `rule_matched` and fire counts stay meaningful.

The compiled `LogicGraph` component is attached to an entity together with the `Blackboard` it
reads (usually the same entity's). **One entity hosts at most one `LogicGraph`** — graph,
locals, inbox and status components all derive from the single `LogicGraph` insert, so a second
graph on one entity would silently replace the first with no error. Graphs that need to share an
entity's lifecycle compose through child entities (one graph per child), and graphs that need to
talk to each other use entity events (`EmitEvent` targeting an entity, § 4.3) or declared world
blackboards (§ 5). Every `logic.*` agent method therefore addresses a graph by its entity alone
(§ 10).

## 4. Events, conditions, actions

### 4.1 Events (the left column)

Event arms are a closed, format-versioned enum, **complete in the v1 schema** so later phases
never re-shape a frozen file (spec 12 § 4a):

| Arm | Fires when | Deterministic because |
|---|---|---|
| `Start` | the graph instance starts (spawn or restart) | fixed step |
| `Timer { every_ticks, jitter_ticks }` | every `every_ticks` fixed steps (first fire after one period); optional seeded jitter 0..=`jitter_ticks` | ticks + SplitMix64 RNG (below), never wall-clock |
| `BlackboardChanged { key }` | the key was **written** at least once since the graph's last tick (a same-value write counts — it is a write counter, not a value diff), coalesced to one event per key per tick | writes happen in tick order |
| `EntityEvent(name)` | an event with this name is pushed to the graph's `LogicInbox` (below) — by gameplay code, a task, a state-tree transition, `logic.emit` (editor/agent), a Lua mod's `regia.emit` (§ 4.4) or another graph's `EmitEvent` action | inbox is FIFO, drained in order |
| `OnSequenceMarker { sequence, marker }` | a playing sequence bound to this entity reaches `marker` | sequenced sim time |
| `OnNetMessage { message }` | a registered net message names this graph's entity as target (stamped at `NetSet::Apply`) | message application order is a pure function of the packets received (spec 70 § 8) |
| `InputAction { action }` | the input system fires bound action `action` on a locally controlled entity | input → fixed-step mapping (spec 11) |

`OnSequenceMarker`, `OnNetMessage` and `InputAction` parse and compile in v1 but their semantics
land in P3: a graph that **uses** one before its phase is a compile error `unsupported_feature`
(the arm is in the schema, the feature is not). This keeps one frozen format version across P1–P4
— no mid-programme v2, no migrator, no silent meaning change for files authored at P1.

`Timer` jitter RNG, pinned: the graph seed feeds a SplitMix64 stream (the algorithm the state
tree pins, spec 38 § 9–10), one stream per graph instance. For timer arm *i* at period *p* the
jitter is `draw(i, p) % (jitter_ticks + 1)`, where `draw(i, p)` is the stream's (*k*-th) output in
the fixed order (arm index, then period). Draws happen whether or not the budget skipped ticks
(timers count **fixed steps**, not graph ticks — a timer due on a step the cadence skips
fires at the graph's next tick: a cadence-16 graph's `Timer(60)` fires at tick 64, the trace
records due-tick 60 and the jitter draw is unchanged; § 2 and P1.12). Two graphs with the same seed and timer arms draw identically; different
seeds differ.

`LogicInbox { events: Vec<String> }` is the same plain-string inbox pattern as `StateTreeInbox`
(spec 38): all sources above push named events. A payload convention (a task that needs a value
writes the blackboard first, then emits) keeps events as plain strings in v1 — the same decision
spec 38 § 3 records for state trees. There is deliberately **no separate `Manual` arm**: a
host-raised event (`logic.emit`, editor button) is an `EntityEvent` with the chosen name, and
rules match it like any other named event.

v2 candidates (not in the v1 schema, each ships as a format bump with a migrator): `Chance`
conditions.

### 4.2 Conditions (the middle column)

Rule conditions use the existing `Condition` enum vocabulary (`Always/Key/Has/Compare/Text/Not/
All/Any`, depth ≤ 16), evaluated against the graph's scopes (§ 5) with the graph entity's context.
**Scope resolution is new logic-runtime code**, not the state tree's evaluator: the state tree's
`Condition::eval(&Blackboard)` reads one blackboard with literal keys
(`crates/regia_ai/src/state_tree/def.rs`), which has no notion of `local/` or `world/` prefixes.
The logic runtime adds one stated rule — a key's scope is named by its prefix; a bare key is
entity scope; there is no stack and no shadowing (`world/gatehouse/x` names exactly one store).
Leaf evaluation itself (what `Key`/`Has`/`Compare`/`Text` mean per value) reuses the existing
`Condition::eval` against the resolved scope's `Blackboard`. No new condition *kinds* in v1 —
the set that drives state-tree transitions is exactly the set a designer needs for guards, and
every addition is a format-versioned change from here on.

### 4.3 Actions (the right column)

An action is `{ task: String, set: BTreeMap<String, Binding> }`, where `Binding` is
`Literal(Bool|Int|Float|String)` (spec 38 § 4's `Literal`) or `BbRef("scope/key")`. This reuses
spec 38 § 3's decision — *task parameters stay out of the `BtTask` signature*; tasks read their
configuration from the blackboard — so any registered task works as an action; but **not
"unchanged"**: the multi-tick contract below is new behavior the task library was written against
trees for, and v1 action chains rely on it.

On fire, the chain runs as a **suspended per-rule-fire state machine** (there is no "scratch
namespace" — `BtContext` is only `{world, entity, dt}` and tasks read the entity's `Blackboard`
through `blackboard()`/`blackboard_mut()`):

1. The named `BtTask` is constructed once at compile time. On chain start its `reset` runs (the
   same `BtTask::reset` trees call before reuse, `crates/regia_ai/src/task.rs`), so a re-fired
   rule never inherits a previous fire's task state.
2. Before each task tick, the bindings are applied to the **entity's `Blackboard`** — exactly
   like the state tree writes `enter_set` literals into the real blackboard —: literals under
   their `set` key, `BbRef`s resolved against the scopes (§ 5) and the *value* copied under the
   `set` key. Written keys persist after the chain finishes (as `enter_set` keys do); a task that
   latches state removes its keys in `reset`.
   Bindings write under their raw `set` keys with no prefix or namespacing: a binding
   overwrites a gameplay blackboard key of the same name, and every chain-tick write fires
   `BlackboardChanged` arms watching that key (§ 4.1). Name action keys after their task
   (`open_door/speed`) or keep them in `locals:` (§ 5) when they are graph state, not entity
   state; an action must not write a key gameplay code owns unless overwriting it is the point.
3. The task is ticked **once per graph tick** (every fixed step at cadence 1) with a normal `BtContext`. `Running` suspends the
   chain until the next tick the graph runs. Task ticks get the budget's `dt = age * step` of
   the graph's tick, as trees do (§ 2, P1.12): a chain suspended across budget-skipped ticks
   resumes with the accumulated `dt` of the step it restarts on, not one fixed step.
   `Success` advances to the next action; `Failure`
   stops the chain with an `action_failed` trace line. The rule's remaining bookkeeping
   (cooldown, fire counts) updates when the chain *starts*, not when it finishes.
4. Only one chain per rule runs at a time; a second fire of the same rule while its chain is
   suspended is dropped with a trace line (`chain_busy`) — the multi-graph case this used to
   collide with does not exist (§ 3, one graph per entity), but a rule can re-fire from its own
   emitted event one tick later.

Because ticking is one-per-step with `reset` on start, existing multi-tick tasks (move, wait,
`test::SucceedAfter`-style) behave as designers expect: they advance one step per tick and resume
correctly on re-fire. Ticking "until `Success`" inside one step is **not** the contract — that
would either loop forever on a `Running` task or consume many `dt`s in one step.

Built-in action tasks shipped in v1 (task paths; registration site in parentheses):

- `logic::SetBlackboard` — write/increment/append a value at `scope/key` (the locals and
  cross-graph glue). (`regia_ai`, P1.)
- `logic::EmitEvent` — push a named event to this graph's inbox, or to a **target entity's**
  inbox by entity id (resolved from a binding, e.g. a `door_ref` blackboard value); world inboxes
  are v2. (`regia_ai`, P1.)
- `logic::EnableEntity` / `logic::DisableEntity` — toggle a target entity's activity flag.
  (`regia_ai`, P1.)
- `logic::PlaySequence` — start a sequencer sequence (`.seq.ron`) bound to a target entity
  through the existing `SequencePlayer` bindings (spec 22 § T2.6). (`regia_framework`, behind its
  `client` feature — the sequencer player is client-only — registered into the same
  `BtTaskRegistry` at app composition; `regia_ai` gains no sequencer dependency. P3.)
- `logic::PlayCue` — play a sound cue asset (`.cue.ron`). (`regia_audio`-adjacent; defined and
  registered with the other presentation actions, P3.)

The first three are *sim* actions (they write gameplay state). The last two are *presentation*
actions (they only drive local sight/sound); the distinction matters under networking (§ 8).

### 4.4 Registering new node behavior

**From Rust:** any `BtTask` is already an action — `#[derive(BtTask)]` + `BtTaskRegistry::register`
(for configured tasks, `register_factory`), exactly as behavior trees and state trees register
leaves today. The editor palette and agent `list_types` enumerate them from the registry; no
second registry is introduced. Games register their tasks (`game::OpenDoor` above) in app
composition, as today.

**From Lua:** mods *raise* graph events with the existing `regia.emit`, and P3 adds an optional
entity target to it — **`regia.emit(name, value, target?)`**, where `target` is an entity the mod
holds (the `EntityHandle` mods already receive from world ops); with no `target` the event stays
on today's host queue — the one `LuaMods::drain_emitted` drains (`crates/regia_script_lua/src/host.rs`) —
unchanged, and only a targeted emit routes into the targeted entity's `LogicInbox`. This is a **spec 84 amendment**
(the `regia.emit` signature and its `EmittedEvent { mod_id, name, value }` record gain a target),
recorded here so P3 changes spec 84 with the spec-84 owner rather than treating the API as free.
The host-side bridge routes drained `EmittedEvent`s into `LogicInbox` of the targeted entity.

The `CallLua { mod_id, function }` task family — a configured task whose `set` map is passed to
the Lua function as a table and whose return (`true`/`false`/`nil`) maps to `Success`/`Failure` —
is **registered by the umbrella crate `regia`** (its `lua` feature already optionally depends on
`regia_script_lua`, and `regia_ai` is a dependency it can enable), not by `regia_script_lua`
itself: both crates sit in `Layer::Gameplay`, where a same-layer `regia_script_lua → regia_ai`
edge would need an xtask allowlist entry for a dependency that today does not exist, failing
`cargo xtask check-layers` on day one. Umbrella registration keeps `Layer::Gameplay` untouched.
`CallLua` runs its function through the sandbox's existing journaling dispatcher
(`LuaMods::fire`/`run_chunk`/`call_tick`, spec 84): the call is journaled like a mod callback, it
runs inside the existing instruction budget and memory cap, and a Lua error fails only that
action (trace line), never the graph or the mod — the same isolation rule mods already follow.
This keeps `regia_ai` free of any script dependency while giving modders both directions.

## 5. Variables and blackboard scopes

Three scopes, all `BTreeMap`-ordered, resolved by the prefix rule of § 4.2 (explicit prefix names
the scope; bare key = entity scope; no shadowing):

| Scope | Storage | Lifetime |
|---|---|---|
| `local/` | the graph instance's locals, held inside the `LogicGraph` component (a `Blackboard` seeded from `locals:` defaults) | the component instance |
| `entity/` (bare keys) | the entity's existing `Blackboard` component | the entity |
| `world/<name>/` | a named global blackboard (`LogicWorldBlackboards` resource) declared in the asset's `world:` list | the world/level |

The locals live inside the single graph component — a second `Blackboard`-typed component on the
entity would collide with the entity's own (§ 3, one graph per entity), and keeping locals in the
component means hot reload (§ 7) carries them without a second component swap.

`Condition` keys and `BbRef`s name a scope explicitly (`local/x`, `world/gatehouse/x`) or bare for
entity scope. World blackboards are how separate graphs coordinate (the gatehouse graph sets
`world/gatehouse/alarm`; the patrol graph reads it). A graph may only touch world blackboards it
declares (compile error otherwise; a non-empty `world:` list is `unsupported_feature` until P3
implements world scope); declaration is the audit hook. The live debug view (§ 9) shows all three
scopes per instance.

## 6. Editor UX

A new **Logic Graph** panel in `regia_editor` (docked like the material graph and procgen panels;
the `Panel` enum lives in `crates/regia_editor/src/shell.rs`, which gains the `Panel::LogicGraph`
variant):

- **Canvas:** egui-snarl (the one node-graph widget in the tree, per `matgraph_editor` and
  `procgen`). Three columns by convention — events left, conditions middle, actions right — with a
  palette grouped the same way. A rule reads as one event node wired through zero or more
  condition nodes (chained wires = `All`; a condition node with a mode pin can be switched to
  `Any`/`Not` composition, mirroring the `Condition` enum) into one or more action nodes wired in
  execution order.
- **Document model:** `LogicGraphDoc` is pure data (graph + `layout: BTreeMap<node,[f32;2]> +
  per-node inspector state), no egui in the model — the pattern `MatGraphDoc` and `ProcGraphDoc`
  established, so the agent protocol edits the same document.
- **Edits:** all structural edits flow through a headless op list (`logic_edit::LogicOp`, mirroring
  `regia_ai::bt_edit::BtOp`): add/remove/move node, wire/unwire, edit `set` binding, rename, set
  priority/cooldown. The panel translates gestures to ops; ops are one undo step each through the
  shared `UndoStack`; the same ops back the agent methods (§ 10). Validation runs on a document
  copy after every op and lists issues with stable codes (§ 3 limits, `unknown_task`,
  `dangling_wire`, `unknown_scope`, `duplicate_name`, `condition_too_deep`, `bad_value` — the
  neighbors' snake_case convention).
- **Authoring without the panel:** the asset is hand/agent-editable RON; `logic.compile` validates
  text without spawning anything (spec 38 § 7 pattern).

## 7. Hot reload

Same contract as state-tree hot reload (spec 38 § 5, ADR-1565): a host (editor watcher or agent)
compiles new text with `LogicGraph::compile` and calls `LogicGraph::reload(next, ctx)` — `reload`
takes an **already-compiled** graph (as the state tree's does), so a def that fails to compile
never reaches `reload`: the old graph keeps running and the compile error is reported (P1.8). The
swap happens at the start of the graph's next tick. Locals survive by name (new locals take their
defaults, removed locals vanish); the inbox and per-rule fire counts are kept; `ReloadOutcome` is
`Kept | Restarted` — `Restarted` when running state cannot carry over: a `Start`-armed rule must
re-run, or a rule with a suspended action chain was removed/renamed (restart is the explicit
opt-out of keeping). The editor's play-in-editor live edit reloads on save through the same path,
and a `LogicReloadRecord` log (bounded) mirrors `StateTreeReloadRecord`. Rule-level identity is
the rule *name*, so a designer's live session is not reset by graph-layout edits that leave names
alone.

## 8. Determinism and networking

- **Offline determinism** follows spec 70 § 8: offline play stays deterministic under
  `enable_fixed_step`. The logic-graph additions: timers in fixed steps (SplitMix64 jitter, § 4.1),
  one queue with a stated order per tick (timers, `BlackboardChanged` in key order, inbox FIFO,
  same-tick emits next tick — § 2), (priority, declaration) rule order, `BTreeMap` everywhere.
  Acceptance test: identical def, seed, blackboard writes and event sequence over 2,000 fixed
  steps produce byte-identical `LogicGraphStatus` including trace, in two runs (the harness style
  of spec 38 § 8 test 9 and spec 70 § 10 test 1).
- **Networking** follows spec 70's authority model (§ 5 Authority, § 5.3 Roles). v1 adds **no net
  messages**: gameplay effects of a graph flow through components that already replicate (door
  state, health, transforms). The authority seam is a resource, not a crate dependency:
  `regia_ai` defines `LogicAuthority` — a predicate `fn(Entity, &World) -> bool`, defaulting to
  "always authoritative" (the offline case) — and the net wiring (umbrella crate `regia`, `net`
  feature) installs a check built on `NetRole` (`has_authority`, spec 70 § 5.3). `regia_ai` keeps
  its optional `regia_net` feature out of this path.
- **Sim vs presentation split.** Actions declare themselves sim (write gameplay state) or
  presentation (only drive local sight/sound; § 4.3). On a proxy (`NetRole::Proxy`) the graph
  still ticks — presentation actions run so the remote door creaks and its sequence plays — but
  sim actions are skipped (they would be overwritten by replication anyway); on the authority peer
  everything runs. Predicted copies run both, and must not write replicated state except through
  existing replicated components — the branch rule spec 70 § 5.3 already states ("authority runs
  the rule, the locally controlled copy plays the instant feedback, proxies only show").
- P3 implements the `OnNetMessage` arm (already in the v1 schema, § 4.1): message application
  stays "a pure function of the packets received" (spec 70 § 8) by stamping inbox events with the
  net tick at `NetSet::Apply` (spec 70 § 8.1).

## 9. Debugging

`LogicGraphStatus` (reflected component, refreshed every tick, mirrors `StateTreeStatus`):
`running/paused`, per-rule `{ name, fires, last_fired_tick, cooldown_remaining }`, a bounded
locals snapshot (≤ 64 entries), and a bounded trace (`MAX_TRACE_ENTRIES` = 64) with kinds
`event_received, rule_matched, rule_rejected, action_started, action_failed, chain_busy,
reload_kept, reload_restarted, inbox_overflow`. "Refreshed" is the state tree's change-checked
`refresh` precedent (`crates/regia_ai/src/state_tree/mod.rs`), not a blind rewrite: a steady
graph reuses the previous strings and vectors and allocates nothing (§ 13 P4.3's
allocation-free steady-state claim covers the status write as well as the tick).

`LogicGraphDebug` (dev builds only, `cfg(debug_assertions)`, absent in shipping — the
`StateTreeDebug` pattern): `pause_on_rule: Vec<String>` and `pause_on_event: Vec<String>`, applied
at the end of the tick in which they trigger; plus `disabled: Vec<String>` rules that never match.
Never persisted in the asset.

The panel's live view (read through the PIE dispatcher, like the state-tree panel's `LiveSource`)
shows the graph coloring by last-tick activity: which event fired, which rules matched or were
rejected (hover gives the trace reason), each variable's current value, and breakpoint hits with
resume/step controls. This is a debugger *and* the authoring surface, so live values sit next to
the nodes they belong to.

## 10. Agent surface (rule 8)

New methods (classes per spec 23; every mutating method takes `dry_run` where an edit is involved
and returns the undo id):

| Method | Class | Params | Result |
|---|---|---|---|
| `logic.compile` | read | `text` or `path` | `{ ok, rules, errors: [{reason, rule?, detail}] }` |
| `logic.control` | play | `entity`, `op: "pause"\|"resume"\|"restart"` | `{ status, paused }` |
| `logic.emit` | play | `entity`, `event` | `{ queued }` |
| `logic.reload` | play | `entity`, `text` or `path` | `{ outcome: "kept"\|"restarted" }` |
| `logic.debug` | play (dev) | `entity`, `pause_on_rule?`, `pause_on_event?`, `disabled?` | `{ .. }` |
| `logic.status` | read | `entity` | `LogicGraphStatus` JSON |
| `editor.logic_open` / `logic_save` | edit | `path` | doc handle / save result |
| `editor.logic_edit` | edit | `path`, `ops: [LogicOp]` | `{ issues, undo_id }` |
| `editor.logic_validate` | read | `path` or `text` | issues list |

Error reasons (agent `F0002 data.reason`, snake_case): `entity_not_found`, `no_logic_graph`,
`bad_op`, `rule_not_found`, `unknown_task`, `dangling_wire`, `unknown_scope`, `duplicate_name`,
`condition_too_deep`, `too_many_rules`, `too_many_actions`, `bad_value`, `unsupported_feature`,
`unsupported_version`, `dev_only`, `missing_param`, plus parse errors. Implementation adds
`book/agents/logic-graph.md` and the `book/COVERAGE.md` rows (both in phase 1/2 acceptance,
below).

## 11. Provenance

ECA rule model: Widom & Ceri, *Active Database Systems: Triggers and Rules for Advanced Database
Processing* (Morgan Kaufmann, 1996) — the event–condition–action shape and the term; guarded
transitions and utility/weighted selection stay where they live (spec 38, Harel 1987 statecharts;
Mark 2009). Observer/event-queue structure: Nystrom, *Game Programming Patterns* (public online
book). Node-graph editor UX follows the in-tree egui-snarl panels (matgraph, procgen); no new
widget. Row added to `docs/provenance.md` (design record for this spec; implementation rows land
with the code). Original design and names are Regia's own.

## 12. Out of scope

Visual scripting of arbitrary code (no loops, functions or custom node authoring from the editor —
reach for Lua mods or Rust); per-rule `Any`-of-events (compose with two rules); replication of the
graph asset itself (content distribution already handles assets); event payloads beyond the
blackboard convention (a v2 field with a migrator if a real need appears); migration of graphs from
other engines; replacing behavior trees, state trees or Lua mods (a graph *calls* them).

## 13. Phased plan and acceptance tests

Sizes follow the milestone scale of `docs/05-milestones.md` (S < a week, M ≈ 1–2 weeks,
L ≈ 3–4 weeks of one lane). Tests are fixed-step/counter-based; temp paths through
`regia_test_utils::unique_temp_dir`; no sleeps or wall-clock budgets (the lane rules).

### P1 — Runtime core (M; the MVP)

Scope: `regia_ai::logic` def + compile + interpreter; event arms `Start`, `Timer`,
`BlackboardChanged`, `EntityEvent` (`OnSequenceMarker`/`OnNetMessage`/`InputAction` parse in the
v1 schema and fail compile with `unsupported_feature` until P3, § 4.1); condition evaluation over
local/entity scopes (a non-empty `world:` list is `unsupported_feature` until P3); built-in
actions `SetBlackboard`, `EmitEvent`, `EnableEntity`, `DisableEntity`; `LogicInbox` with
same-tick-emit-next-tick delivery (§ 2); `LogicGraphStatus` (change-checked refresh, § 9) +
bounded trace; `reload`; the `LogicAuthority` resource (§ 8, default always-authoritative);
agent methods `logic.compile/control/emit/reload/status`; a stand-in `game::OpenDoor` task (a no-op `BtTask`
registered under that exact path in tests) so the § 3 example compiles verbatim; `.logic.ron` v1 registered in the
format registry (lead runs `check-formats --update`) with `logic_schema_fingerprint()` and
`logic_migration_sources()` (the state-tree precedent, `crates/regia_ai/src/state_tree/def.rs`),
the `regia upgrade-project` row, and the `docs/formats/logic-ron.md` page. This phase alone is
usable: an agent or a text-editing designer can author and run graphs.

Acceptance tests:

1. **Compile and limits.** A def past each limit (257 rules, 17 actions per rule, condition depth
   17, duplicate or empty names, unknown task path, unknown scope) fails with the exact codes of
   § 3/§ 10; a valid def compiles with resolved rule order (priority, declaration); the § 3
   example parses and compiles verbatim.
2. **Timers.** `every_ticks: 60` fires at exactly ticks 60 and 120; jitter draws come from the
   graph seed (two same-seed runs match, different seeds differ) with the § 4.1 draw schedule;
   a paused graph fires nothing; a same-value write to a watched key fires `BlackboardChanged`.
3. **BlackboardChanged.** Two writes to one key in one tick produce one event; two keys produce
   events in key order; a write from an action in the same tick is visible to the watch next tick.
4. **Inbox.** Events drain FIFO; cap 64, the 65th drops the oldest with an `inbox_overflow`
   trace line; an event no rule matches is dropped with a trace line; a rule fires at most once
   per event; events emitted during the drain (an `EmitEvent` action, `logic.emit`) are delivered
   next tick, and a two-rule self-emit loop (A on X emits Y, B on Y emits X) advances exactly one
   hop per tick without event loss.
5. **Ordering.** Two rules on the same event: higher `priority` wins the trace order; a tie falls
   to declaration order; a failing action in the first rule does not stop the second rule.
6. **Conditions.** `All/Any/Not/Compare/Key/Has/Text` against local and entity scopes, including
   `BbRef` dereferences in bindings; a false condition yields `rule_rejected`, no action runs;
   `local/x` and bare `x` resolve to different stores without shadowing.
7. **Action chains.** Actions run in wire order; `set` literals and resolved `BbRef`s land in the
   entity blackboard before each tick (the `enter_set` precedent); a multi-tick task
   (`test::SucceedAfter`-style) suspends its chain and resumes one step per tick; `reset` runs on
   re-fire so a task that latched state in a previous fire starts clean; a `Failure` stops that
   chain only.
8. **Hot reload.** A name-preserving edit reloads `Kept` (locals by name, inbox and fire counts
   kept); removing an active rule resets its counters, and a graph whose suspended chain or
   `Start` rule cannot carry over reloads `Restarted`; a def that fails to compile leaves the old
   graph running and reports the reason (it never reaches `reload`).
9. **Determinism.** Same def, seed, blackboard writes and event sequence for 2,000 fixed steps:
   byte-identical `LogicGraphStatus` including trace, in two runs.
10. **Agent.** Each `logic.*` method: success, missing parameter, every documented reason; a
    dogfood script (spawn a graph entity, `logic.emit`, `play.wait_until` on the status,
    `logic.control pause/resume`).
11. **Fuzz.** `cargo fuzz` target over `LogicGraphDef::from_ron` + `compile` never panics.
12. **Budget.** Graphs register with `AiTickBudget` as `TreeKind::Logic`; a graph deferred by the
    budget runs on its next scheduled tick with the skipped steps accumulated into `dt`
    (`dt = age * step`, the state-tree cadence rule), and a chain suspended across those ticks
    resumes on the same `dt`; a timer due on a skipped step fires at that next tick with the
    trace recording its due-tick and the jitter draw unchanged (§ 4.1), a
    `BlackboardChanged` write during skipped steps coalesces into one event at the graph's next
    tick, and per-tick rule evaluations stay within the § 2 bound.
13. **Cooldown.** A rule with `cooldown_ticks: 120` matches during its cooldown but runs its
    actions at most once per 120 fixed steps; restart resets the counter.

### P2 — Editor canvas (M-L)

Scope: `LogicGraphDoc` (pure data), the egui-snarl Logic Graph panel, palette, wiring gestures
(dropped-wire menus as in the procgen panel), inspector, live issues list, `LogicOp` headless
edits shared with the agent, `UndoStack` integration, live activity coloring (event fired /
matched / rejected per node), `editor.logic_open/edit/save/validate`, `Panel::LogicGraph`.

Acceptance tests:

1. **Round trip.** Doc → snarl → doc preserves graph and layout; re-saving an unchanged asset is
   byte-identical (canonical RON).
2. **Wiring.** Event→action directly compiles to condition `Always`; a condition chain compiles to
   `All`; a condition node set to `Not`/`Any` in the inspector changes the compiled tree; dangling
   wires are issues, not panics.
3. **Ops and undo.** Every `LogicOp` through the panel is one undo step and round-trips through
   `editor.logic_edit` with identical results (panel and agent share the op path).
4. **Validation.** Each § 6 issue code appears in the panel list and in `editor.logic_validate`.
5. **Live view.** In play-in-editor, a fired rule colors its nodes; the variable table shows all
   three scopes; a reload on save keeps live state per § 7.

### P3 — Integrations (M)

Scope: `logic::PlaySequence` and `logic::PlayCue` presentation actions, defined and registered
from `regia_framework` (client) / the audio side into the shared `BtTaskRegistry` at app
composition (§ 4.3) — not in `regia_ai`; the semantics of the `OnSequenceMarker`,
`OnNetMessage` and `InputAction` event arms already in the v1 schema (`unsupported_feature` until
now, § 4.1): `OnNetMessage` inbox events stamped at `NetSet::Apply` (spec 70 § 8.1) so message
application stays a pure function of the packets received; world-scope blackboards (a non-empty
`world:` list compiles from here on); the `CallLua` task registered by the umbrella crate `regia`
and the `regia.emit(name, value, target?)` spec-84 amendment with its → `LogicInbox` routing
(§ 4.4); the `NetRole`-based `LogicAuthority` check installed by `regia`'s net wiring (§ 8).

Acceptance tests:

1. **Sequencer.** A rule starts a `.seq.ron` on a bound target; markers during playback raise
   `OnSequenceMarker` events into the same graph.
2. **Audio.** `PlayCue` plays the named `.cue.ron` asset through the existing cue path.
3. **Lua.** A `CallLua` action passes its `set` map as a table; a Lua error fails the action
   (trace line) and leaves the mod enabled and the graph running; `regia.emit(name, value,
   target)` from a mod raises the named event in the targeted entity's graph inbox (no target =
   today's host queue that `LuaMods::drain_emitted` drains, unchanged).
4. **World scope.** Two graphs on different entities coordinate through a declared world
   blackboard; an undeclared world scope is a compile error.
5. **Networking.** In the spec 70 determinism harness (offline and listen-server host, spec 70
   § 10) graph traces match; on a `NetRole::Proxy` copy sim actions do not run while presentation
   actions do; net message application order is a pure function of the packets received.

### P4 — Debugging and performance polish (S)

Scope: `LogicGraphDebug` breakpoints (`pause_on_rule`, `pause_on_event`, `disabled`), step,
the live value view (§ 9), allocation-free steady-state assertion, budget/perf numbers recorded
in `docs/perf-log.md`, `book/agents/logic-graph.md` + `book/COVERAGE.md` rows, the agent dogfood
script in CI.

Acceptance tests:

1. A `pause_on_rule` breakpoint pauses at the end of the firing tick; step advances exactly one
   tick; resume continues.
2. A `disabled` rule never matches; `pause_on_event` triggers on the named inbox event only.
3. Perf: 100 graph instances × 25 rules of compares over 1,000 ticks — zero heap allocations in
   steady state (counting allocator) and within the `AiTickBudget` slice on one desktop core;
   Android aarch64 runs the same test.
4. Coverage: `cargo xtask agent-coverage` and the book coverage test pass with the new page.

As built (ADR-7282): acceptance 3 is `crates/regia_ai/tests/logic_graph_alloc.rs` — zero allocations over
1,000 steps after 200 warm-up steps, on the plain schedule and under an `AiTickBudget` whose slice holds all
100 graphs (asserted by the budget's counters: all tick, none deferred or late); the time bar (median step
under one 60 Hz fixed step) is the `#[ignore]`d `a_hundred_graphs_tick_inside_one_fixed_step`, because gated
tests carry no wall-clock bars. Android aarch64 builds the test (`cargo ndk -t arm64-v8a -P 28 test -p regia_ai
--test logic_graph_alloc --no-run`; the default API level fails to link the `regia_web` dev-dependency's
`-laaudio`) and runs it: the phase-4.3 lane ran it on the TEST Pixel 9 Pro (`3 passed; 0 failed; 1 ignored`,
`reports/sprint-post05w4-logic-graph-p4-alloc.md`). `logic::trace_tests` keeps the trace text identical to the
phase-1 runtime's. Acceptance 4's dogfood script is `crates/regia_editor/tests/logic_graph_dogfood.rs` (finish
note in the header).

### Owner decisions

1. Product name "Logic Graph" and panel title — confirm or rename.
2. Priority: phase 2 (editor canvas) vs agent-only authoring first — agent-only is cheaper and
   spec-23-native; the canvas is what the owner asked a designer to use. Recommended: P1 runtime
   with full agent surface, P2 canvas immediately after.
3. Whether `OnNetMessage` (phase 3) must land in the same release as the rest, or can follow.

## 14. Match rules nodes (audit gap GR-7.6)

Designers script mode events visually: "when the flag is captured, open the gate". No format change:
the five tasks register through the umbrella crate's bridge (`regia::logic_bridge`, § 4.4), and match
events reuse the `EntityEvent` arm (§ 4.1) under the name `match.<kind>`.

### 14.1 Tasks

`LogicBridgePlugin` registers these in every build (they need only `regia_framework`). All five are
sim actions (§ 8): a peer without authority skips them, and a network client (`NetMode::Client`)
refuses them with a warning on the `regia::logic` target and a failed action, because the match
belongs to the host. A bad or missing parameter, or a refusal from the session, is a warning and a
failed action (`action_failed` in the trace); nothing changes.

| Task | `set` parameters | Does |
|---|---|---|
| `rules::AddScore` | `points` (integer; negative takes away), and `team`, or `team` + `seat` (slot), or `entity` (holds `TeamId` + `LedgerSlot`) | `team` alone: the team's board score (`MatchSession::add_team_score`, clamped to `0..=u32::MAX`, no event, no limit check). A seat: its ledger score (`MatchSession::add_seat_score`), and a `player_scored` event with reason `scripted`. Refused for a neutral team, an empty seat, or a match that is over. |
| `rules::EndMatch` | `team` (default `0`, no winner), `reason` (default `"ended"`; any `EndReason` word: `ended`, `forfeit`, `draw`, `time_limit`, `rounds_won`, `capture_limit_reached`, `hill_score_limit_reached`) | `MatchSession::report_condition`: with every phase setting off the match ends now; with the phase machine on the condition lands on its next step. Refused when the match is already over. |
| `rules::SetTeam` | `team`, `entity` (default: the graph's entity) | A seated entity changes seat through `assign_team` (the session's `teams.switch` policy applies; the default `never` refuses), then `retag_moved_seats` moves its `TeamId` and `LedgerSlot`. An unseated entity only gets the new `TeamId`. |
| `rules::SetSpawnEnabled` | `entity` (a `PlayerStart`; default: the graph's entity), `on` (default `true`) | `on: false` adds `PlayerStartDisabled`, which `default_spawn_point` skips; `on: true` removes it. When every start is disabled the first start is used anyway. |
| `rules::LockObjective` | `id` (the objective's entity id), `on` (default `true`) | `ObjectiveBoard::set_locked`: a locked flag takes no touches (no pickup, no touch return) and does not capture while carried; carrier death drops still happen and drop checks and the return timer still send it home. A locked hill scores no points. Locks survive a round start and a new match clears them. Refused for an unknown id. |

The locks are core state (`ObjectiveBoard::locked`, sorted ids). The field is skipped in serde
while empty and hashed by the proof scenario only when non-empty, so a board that never locks keeps
its serialized and golden bits (S-14 behavior unchanged).

### 14.2 Match events

`route_match_events` runs in `PostUpdate` (once per frame, like the sequence-marker route) and takes
every `MatchEvent` the session adapter (`MatchSessionPlugin`) sent in the frame's fixed steps. Each becomes the inbox event `match.<kind>`, where
`<kind>` is the same word `rules.match_step` reports as `kind`:

`flag_taken`, `flag_dropped`, `flag_returned`, `flag_captured`, `hill_point`, `match_over`,
`paused`, `resumed`, `launch_scheduled`, `launch_cancelled`, `launched`, `seat_left`,
`phase_changed`, `round_over`, `flag_auto_returned`, `player_scored`, and (ADR-8731, the v0.7
roll-up) `respawn_at`, `eliminated`, `zone_captured`, `zone_point`, `zone_activated`,
`wave_started`, `wave_spawn`, `wave_cleared`, `coop_ended`, `racer_checkpoint`, `racer_lap`,
`racer_missed_checkpoint`, `racer_wrong_way`, `racer_finished`, `racer_dnf`, `zone_phase`,
`zone_damage`, `team_eliminated`, `victory`, `surrender_vote`, `surrender_failed`, and (ADR-8881)
`row_skipped`.

It goes to:

1. every entity marked `MatchEventListener` (the level's game-mode entity) that has a logic graph;
2. the entities the event involves: the unit that took, dropped, returned or captured a flag (the
   contestant id is an entity index, matched among entities with `TeamId`), the flag's
   `ObjectiveEntity` (the hill's for `hill_point`; the zone's or the race checkpoint's for their
   events), and the pawn holding the seat of a seat event (`player_scored`, `seat_left`,
   `respawn_at`, `eliminated`, the `racer_*` events, `surrender_vote`; `TeamId` + `LedgerSlot`).
   Each target hears an event once.

Before the event lands, its fields are written into the target's entity blackboard (the payload
convention of § 4.1) under per-kind keys `match_<kind>_<field>` (`match_flag_captured_team`). Keys
are per kind because one step often sends several events: a capture by a bound carrier is followed
by that seat's `player_scored`, and generic keys would let the second overwrite the fields the
`match.flag_captured` rule reads.

| Field | Value |
|---|---|
| `team` | the scoring, taking or winning team; the seat's team; the flag's own team for a drop or return |
| `seat` | the seat's slot |
| `score` | the score after the event (board score, or the seat's ledger score) |
| `objective` | the objective id |
| `unit` | the unit entity |
| `reason` | the end reason, score reason, auto-return cause, phase, launched mode, or who paused |
| `round` | the round |
| `tick` | the match tick (the flow step for flow events) |

A field the event does not carry is removed, so a key never shows another event's value. When one
frame sends several events of one kind to one entity (several `player_scored` on the state
entity), the keys of that kind hold the last one's fields. Graphs take
the events on their next tick. Order `MatchRulesSet` after `AiStep` (as the acceptance tests do) so
a rules task lands in the same step's session step; the bridge does not impose that order, because
a game may already order its damage between the two.

The logic graph editor lists the tasks under Actions (the registry's task paths) and each event as
`Match: <kind>` under Events.

### 14.3 Acceptance tests

`cargo test -p regia --features logic,net --test logic_rules_nodes`:
`a_logic_graph_opens_a_gate_on_match_flag_captured`,
`add_score_task_scores_on_the_host_and_is_refused_on_a_client`,
`end_match_task_ends_the_match_with_its_reason`,
`set_spawn_enabled_task_removes_a_point_from_selection`, plus the lock, team and seat-score
cases. The core side is `cargo test -p regia_rules --test scripted_rules`.
