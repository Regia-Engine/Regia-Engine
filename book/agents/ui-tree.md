# UI tree view and data grid (agents)

ADR-273 W10, ADR-1842. Code: `regia_ui::tree`.

`UiTree` is a tree of labelled nodes with an expanded set, one selection and keyboard navigation. Node ids are preorder
indexes. Everything starts collapsed. The visible rows (a node and its depth, shown only when every ancestor is expanded)
are written into the `ListItems` of the same entity by `sync_tree_lists`, so a virtualised `ListView` shows them: a tree of
10,000 nodes costs one list row per visible entry. Rows read `[+] label` (collapsed branch), `[-] label` (expanded branch)
or `    label` (leaf: four spaces in place of the marker), indented four spaces per level.

Rules: collapsing a node whose descendant is selected moves the selection to that node; `select` expands the ancestors so
the row is visible; `collapse_all` moves a hidden selection to its top-level ancestor. Keys: Down and Up move one visible
row; Right expands a collapsed branch, else enters its first child; Left collapses an expanded branch, else goes to the
parent; Home and End jump. `UiTree::can_drop` refuses dropping a node onto itself or into its own subtree, and
`UiTree::drop_position` turns the pointer's height in a row into before (top quarter), inside or after (bottom quarter),
the indicator a drag shows.

`DataGrid` is the table model: columns (title, width), rows of cells and a stable sort by one column (numbers compare
numerically and come first, text compares case-insensitively; sorting the same column again flips the direction).

## Methods

- `ui.tree { entity, action?, ... }`:
  - `query` (default): `{ nodes, visible, selected, rows }`; `offset` and `limit` page the rows (most 500).
  - `set { tree: [{ label, children? }] }`: builds the tree on the node (adds an empty `ListItems` when missing).
  - `expand`, `collapse`, `toggle`, `select { node }`; `expand_all`; `collapse_all`; `key { key }`.

  Mutating; permission `edit_scene`.

```json
{"method": "ui.tree", "params": {"entity": 4294967310, "action": "set", "tree": [{"label": "Level", "children": [{"label": "Sun"}]}]}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_action` | `action` is not one the method knows |
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `no_tree` | the entity has no tree; create one with `set` |
| `bad_tree` | `tree` is not an array of `{ label, children? }`, or has too many nodes or levels |
| `unknown_node` | `node` is missing or not a node id of the tree |
| `bad_key` | `key` is not up, down, left, right, home or end |

`entity` is required integer entity bits. `action` defaults to `query`; `set` accepts nested `{ label, children? }`
objects up to 200,000 nodes and 128 levels. `node` is a preorder integer index. `offset` defaults to 0 and `limit`
defaults to 100, capped at 500 rows. `key` accepts `up`, `down`, `left`, `right`, `home` or `end`.

## Failure example

Captured CLI output from `regia agent call ui.tree '{"entity":4294967310}'`:

```text
error: method call failed: the entity does not exist (-32001): {"code":"unknown_entity","hint":"take entity ids from a ui.query result"}
```

## Workflow

Call `set` on a list node with the tree data, then `query` to get preorder node ids and visible rows. Expand the
desired branch, select its node id, and query again to confirm that selection and visibility changed.
