# Net rejections per peer (agents)

`net.rejections` answers "what did each peer have refused, and why" (KI-NT-025, ADR-10077). It lists every
refusal by lowercase refusal code, per peer, for peers that are still connected and for peers that already left
(a peer removed for its refusals is usually the one you ask about). It reads two sources:

- **Transport** (`endpoints`): every `ServerEvent::Rejected` a published `NetServer` raised: a message that did
  not decode, an input over its cap, a flood, a broken reliable stream. A client endpoint lists no rows.
- **Replication runtime** (`runtime`): every refusal the server world's `NetNode::reject` decided (spectator input,
  server-only message types, malformed voice). `null` on a client world and on a world without the net model.

A `ServerBridge` host also receives the runtime's refusals as `ServerEvent::Rejected` from `pump_in` (KI-NT-024),
so one event stream carries both; this method is the counted view of the same refusals.

| Method | Params | Result |
|---|---|---|
| `net.rejections` | `endpoint?`, `peer?` | `{ endpoints: [{ label, role, peers: [{ peer, total, kinds }] }], runtime: { total, dropped, peers: [{ peer, total, kinds }] } \| null }` |

- `endpoint` limits the answer to one published endpoint (labels as `net.status` lists them). Optional.
- `peer` limits the rows to one peer id. Optional. A peer with no refusals has no row.
- `peers[].kinds` maps each refusal code (`message_malformed`, `input_malformed`, `input_too_long`,
  `unknown_lane`, `spectator_input`, `server_only_message`, `voice_malformed`, `flood`, `fragment_too_long`,
  `reassembly_over_cap`, ...; `RejectKind::as_str`) to its count; `total` is their sum.
- `runtime.total` counts every refusal the runtime decided; `runtime.dropped` counts the ones its log of 1,024
  dropped before the host took them (the per-peer counts keep them).
- Rows are kept for the 256 peers whose newest refusal is newest; an older peer's row is dropped first.

Read-only; it changes nothing.

## Example

```json
{"jsonrpc": "2.0", "id": 1, "method": "net.rejections", "params": {"endpoint": "server"}}
```

```json
{"jsonrpc": "2.0", "id": 1, "result": {
  "endpoints": [{"label": "server", "role": "server", "peers": [
    {"peer": 4, "total": 1, "kinds": {"flood": 1}},
    {"peer": 7, "total": 2, "kinds": {"message_malformed": 2}}
  ]}],
  "runtime": {"total": 1, "dropped": 0, "peers": [
    {"peer": 2, "total": 1, "kinds": {"server_only_message": 1}}
  ]}
}}
```

## Errors

| Code | When |
|---|---|
| `net_unknown_endpoint` | No published endpoint has the `endpoint` label. `net.status` lists the labels. |
| `net_rejections_bad_peer` | `peer` is not an integer from 0 to 4294967295. |

## Related

- [Networking](../user-guide/networking.md): `ServerEvent::Rejected` and the refusal codes.
- `net.status` lists the runtime's newest 16 refusals; `net.peers` shows each peer's malformed score.
