# Secondary rigid debris

A break can ask for more secondary rigid bodies than a frame should spawn. `physics.debris_budget`
is the cap (ADR-3929). It does not spawn bodies. The caller spawns only the granted count.

The public break message is `DestructionBroke`, a `BreakEvent`. That event carries no requested
debris count, so this budget does not read it. Pass `break_id` yourself. `BreakEvent.seq` is a
suitable id. `0` is a valid id.

## Caps

| Cap | What it limits |
|---|---|
| `per_break` | Bodies one break may spawn, added up across frames. |
| `per_frame` | Bodies every break may spawn together until `reset_frame`. |

Both are integers from 1 to 4294967295. Calls are taken in order. A request is granted the
minimum of the request, the room left on that break, and the room left in the frame. The refused
part is added to `dropped`. A request with no room left returns `spawned` 0 and still counts as
dropped. `reset_frame` restores the frame allowance. It does not clear per-break totals or
`dropped`.

With caps 4 and 6, three pushes of 3 for one break spawn 3, then 1, then 0, and `dropped` is 5.
After `reset_frame` the frame cap is 6 again. That break is still full. A new break can spawn 4.

Per-break totals stay until the budget resource is dropped.

## Method

| Method | Permission | What it does |
|---|---|---|
| `physics.debris_budget` | play | Read or set the caps, restore the frame allowance, admit one request. |

Parameters, all optional:

| Parameter | Meaning |
|---|---|
| `per_break` | The per-break cap. On an existing budget, omitting it keeps the current cap. |
| `per_frame` | The per-frame cap. Same rule. Installing a budget needs both. |
| `reset_frame` | `true` restores the frame allowance after the caps are applied. |
| `break_id` | The break. Set together with `count`. |
| `count` | How many bodies the caller wants. An integer from 0 to 4294967295. |

A call is checked before anything changes. Then caps are applied, then `reset_frame`, then one
push when both `break_id` and `count` are set. `null` on an optional parameter means absent.

Result: `{ installed, per_break, per_frame, frame_spawned, frame_remaining, dropped, spawned, break_id, break_spawned, digest }`.
`spawned` is the grant for this call, or `null` when the call did not push. `digest` is a decimal
string, the same for two runs that made the same calls. Without a budget, `installed` is `false`
and the counters are 0.

```json
{"method": "physics.debris_budget", "params": {"per_break": 4, "per_frame": 6}}
{"method": "physics.debris_budget", "params": {"break_id": 1, "count": 3}}
{"method": "physics.debris_budget", "params": {"reset_frame": true, "break_id": 2, "count": 4}}
```

## Errors

| Code | When |
|---|---|
| `debris_budget_bad_cap` | `per_break` or `per_frame` is present and is not a finite integer from 1 to 4294967295: zero, negative, a fraction, too large, or not finite. Nothing changes. |
| `F0003` | A push (`break_id` and `count`) before the caps are set. Nothing is installed. |
| `F0002` | `break_id` or `count` is set without the other, `count` or `break_id` is not an integer in range, `reset_frame` is not a boolean, or only one cap is passed when no budget exists yet. |

`debris_budget_bad_cap` is `data.code` on an invalid-params error (`-32602`).
