# `.bt.ron` (behavior tree definition, v2)

Owner: `regia_ai::tree` (`BehaviorTreeDef`, reflected and serde). A flat node arena plus the root index; children are
indices into `nodes`, so a hand-authored file stays readable. `BehaviorTree::compile` checks the graph (child indices in
bounds, no node reachable twice, so no cycle and no second parent; a `condition` has no children) and resolves leaf
tasks in the `BtTaskRegistry`.

```ron
(
    root: 0,
    nodes: [
        (name: "root", kind: selector, children: [1, 2], key: "", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "sees", kind: condition, children: [], key: "enemy_visible", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "rest", kind: cooldown, children: [3], key: "", seconds: 2.0, times: None, task: "", policy: require_all),
        (name: "patrol", kind: leaf, children: [], key: "", seconds: 0.0, times: None, task: "game::Patrol", policy: require_all),
    ],
)
```

| Field | Meaning |
|---|---|
| `root` | Index of the root node |
| node `name` | The node's name in status and errors |
| node `kind` | `selector`, `sequence`, `parallel`, `inverter`, `condition`, `compare`, `cooldown`, `repeat`, `timeout`, `service`, `state_tree`, `leaf` |
| node `children` | Child indices (composites: any; decorators: one; leaves, conditions, compares and state_tree nodes: none) |
| node `key` | `condition`: the blackboard key read as a bool. `compare`: the left-hand key. An abort watches this key |
| node `seconds` | `cooldown`, `timeout`: the interval (ignored elsewhere; a `service` task runs every tick) |
| node `times` | `repeat`: the count (`None` repeats for ever) |
| node `task` | `leaf` and `service`: the registered task type path. `state_tree`: a `.statetree.ron` path |
| node `policy` | `parallel`: `require_all` or `require_one` |
| `format_version` | `2`. Absent in a v1 file, which still loads |
| node `abort` | `none` (default, omitted), `self`, `lower_priority`, `both`. Condition, compare and decorators |
| node `op` | `compare`: `eq` (default, omitted), `ne`, `lt`, `le`, `gt`, `ge`, `is_set`, `in_range` |
| node `value_kind` | `compare`: `bool` (default, omitted), `int`, `float`, `tag`, `entity` |
| node `literal` / `literal_hi` | `compare`: the constant, or the inclusive bounds of `in_range` |
| node `rhs_key` | `compare`: a second blackboard key. Empty means `literal` is the right-hand side |

The fields in the example above are required. `format_version` and the compare/abort fields are optional and
omitted when they are the default, so a v1 line stays byte-for-byte the line in the example.

**Versioning.** `BT_FORMAT_VERSION` is 2 (ADR-4543). A file with no `format_version` is v1 and loads unchanged.
`regia upgrade-project` calls `regia_ai::migrate_bt_ron`: it inserts `format_version: 2` (or rewrites an explicit
`1`) and keeps every other byte. A file already at v2 is returned unchanged. A newer version is refused.
Registered in `cargo xtask check-formats` (`bt`) since ADR-2316.

**Writing.** The editor and `ai.bt_edit` write the canonical layout (`regia_ai::bt_edit::to_canonical_ron`):
`format_version` when it is 2 or higher, then `root`, then one line per node with the v1 fields in the order of
the table and the v2 fields only when they are not the default. `times` is `None` or `Some(n)`. Editing and
validation: `regia_ai::bt_edit` (ADR-2408, ADR-4543).

A `compare` of the wrong type is the validator code `type_mismatch` (a constant whose kind is not `value_kind`,
or `in_range` on a non-numeric kind). `bad_compare` is a missing side, both a constant and `rhs_key`, a
non-finite float, or a range whose low bound is above its high bound. At runtime a missing key or a stored
value of another type makes the comparison false. It does not panic.

`abort` other than `none` re-checks the node when `key` (and `rhs_key`, for a compare) changes, and aborts the
running branch through the task's `reset`. `self` aborts this branch when the check fails. `lower_priority`
aborts a selector's running lower-priority sibling when the check passes. `both` does both.
