# `.navlanes.ron`, format version 2

Authored crowd lanes (`regia_nav::lanes::NavLaneSet`, ADR-1934; v2 lane preference and opposing flow, ADR-2133). Game code loads it with `NavLaneSet::parse` and
`NavLaneSet::graph`, then inserts the `NavLaneGraph` resource; agents send the text to `nav.lanes_set`
(`book/agents/navigation-lanes.md`).

```ron
(
    format_version: 2,
    join_distance: 0.25,
    lanes: [
        (points: [(0.0, 0.0, 0.0), (20.0, 0.0, 0.0)], width: 3.0, next: [1, 2], opposite: Some(3)),
        (points: [(20.0, 0.0, 0.0), (20.0, 0.0, 30.0)], speed: 1.2, user_id: 7, weight: 3.0),
        (points: [(20.0, 0.0, 0.0), (45.0, 0.0, 10.0), (60.0, 0.0, 10.0)], flags: 2),
        (points: [(20.0, 0.0, 0.0), (0.0, 0.0, 0.0)], width: 3.0),
    ],
)
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `format_version` | `u32` | required | 1 or 2. A newer version is the error `nav_lane_version`; 0 is `nav_lane_parse`. |
| `join_distance` | `f32` | `0.0` | A lane also leads to every other lane starting within this many metres of its end. 0 is off. |
| `lanes` | list | required | At most 65536 lanes. A lane's index in the list is its id in `next` and at runtime. |
| `points` | list of `(f32, f32, f32)` | required | The centre line, walked first to last. 2 to 4096 points, finite, no two consecutive equal. |
| `width` | `f32` | `2.0` | Full width in metres, `> 0`. Agents stay inside it. |
| `speed` | `f32` | `1.4` | Speed limit in metres per second, `> 0`. |
| `flags` | `u16` | `1` | Filter bits; an agent uses lanes sharing a bit with its own. |
| `user_id` | `u64` | `0` | The game's id; `set_lane_enabled` acts on every lane with the id. |
| `next` | list of `u32` | `[]` | Indices of the lanes this one leads to. Empty, a lane is a dead end. |
| `weight` | `f32` | `1.0` | v2. Preference, finite and `> 0`: a wanderer at a junction picks among the open lanes in proportion to their weights, and a route's cost through the lane is its time divided by the weight. |
| `opposite` | `Option<u32>` | `None` | v2. The lane running the other way over the same strip. Mutual: naming one side pairs both. Agents of the two lanes see each other as oncoming and keep right to pass. Each end must lie within half the wider width of the other lane's start and the lengths within 10 %. |

A lane is one-way; a two-way pavement is two lanes: side by side, or over one strip paired with `opposite`. Any
problem with a lane is `nav_invalid_lane`. With every weight 1 and no pairing, a crowd steps exactly as with v1 data.

Migration note: v1 → v2 (ADR-2133) only adds the optional `weight` and `opposite`: `regia_nav::migrate_nav_lanes`
checks the file and rewrites its version number, keeping comments and order, and `regia upgrade-project` runs it
(`navlanes`). The loader reads v1 and v2 as they are. `cargo xtask check-formats` holds the shape.
