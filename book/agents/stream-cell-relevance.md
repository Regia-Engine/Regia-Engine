# Stream cell relevance

`stream.cell_relevance` reports, for one peer, the stream cells that peer has loaded, the margin, and how many replicated entities the `stream_cells` filter dropped on the last snapshot.

A replicated entity whose stream cell is not in that loaded set, and not inside the one-cell margin, is not relevant to the peer. The margin is Chebyshev distance on the cell grid: a loaded cell `(0, 0)` also keeps `(1, 0)` and `(1, 1)`, and drops `(2, 0)`. An entity with no stream cell stays relevant. That covers the always-loaded set, a menu, and the player pawn.

The filter only narrows. It never adds interest. A profile that already failed an earlier relevance stage stays irrelevant. `sent_to_all`, `owner_only` and `follow_parent` profiles are not stamped and are not passed through this filter, so their reach is unchanged.

## When it is on

A dedicated server registers the filter when an enabled streamed level is open. A disabled index opened through the low-level world API does not activate it. A client does not register it. With streaming off the filter is absent: a profile that names `stream_cells` does not drop anyone, and a far entity stays relevant.

While streaming is on, a server enables the relevance pipeline and stamps `custom_filter` to `stream_cells` on profiles that are not `sent_to_all`, `owner_only` or `follow_parent`. A profile that already names a filter runs that filter before the cell check. Both can remove interest. Drop counts count unique entities per snapshot; diagnostic queries do not increment them, and steps between snapshots preserve the last count. `Offline` counts as a server. A listen server built with the client plugins adds `StreamCellRelevancePlugin` itself.

A peer is narrowed only when it owns an enabled streaming source. The loaded set is the resident cells within that source's load radius. A peer with a source and no resident cell in range drops every entity that has a stream cell. So does a peer whose enabled source has no usable position (no `Transform`, or a NaN or infinite one): it fails closed with an empty loaded set until the source has a finite position. A peer with no entry is not narrowed.

## `stream.cell_relevance`

Read-only. Permission: Read. MCP tool: `stream_cell_relevance`.

| Param | Type | Required | Meaning |
|---|---|---|---|
| `peer` | integer | yes | Peer id. A connected peer, or a peer that owns a streaming source on this server. |

Result: `{ peer, loaded, margin, dropped }`. `loaded` is cell labels `"<x>_<z>"` in cell order. `margin` is `1`. `dropped` is how many entities the filter removed for that peer on the last published snapshot.

## Errors

All of these are `F0002`.

| Reason | When |
|---|---|
| (no reason) | `peer` is missing or not an integer. |
| `net_not_server` | This process is a client. The check runs before the streaming check. |
| `stream_not_enabled` | No streamed level is open. |
| `stream_cell_filter_off` | Streaming is on and the host did not install the filter. |
| `peer_unknown` | That id is not a connected peer and owns no streaming source. |

## Not in this build

Positions stay in the existing encoding. A cell-plus-offset position is a net change and is not part of this filter.
