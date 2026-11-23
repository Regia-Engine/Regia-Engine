# Host check (agents)

`net.host_check` reports the check a host runs before hosting (spec 120 § 10.7, ADR-8370): the port
mapping its hosting flow got from the router, whether a stranger reaches the mapped port, and how
many clients its uplink carries. A dedicated server started with `--relay` runs the check once,
after the relay registered it, and publishes it under the label `server_host` once a second with
its relay report (`net.relay_status` carries the same object as `host_check`).

What runs:

- **Port mapping**: PCP (RFC 6887), NAT-PMP (RFC 6886) and UPnP IGD ask the default gateway at once
  (found in the routing table on Linux, Android, Windows and macOS, ADR-8893: a platform that hides the
  table, or no IPv4 default route, gives `port_map_unavailable`);
  the first grant wins, a later grant is deleted again, the winner is renewed at half its lifetime
  (two hours asked) and deleted when the host stops. The mapped address is announced to the relay
  as a `mapped` candidate (see `nat.candidates` in [Relay status](net-relay.md)). With `--relay
  local` nothing is mapped (`mapping_error` `port_map_off`).
- **Port-forward self-check**: the host asks the relay (relay protocol 3) to send one probe to the
  mapped address from a relay port the host never sent to. The probe arrives on the game socket
  only through the mapping.
- **Capacity**: the host sends 1,200-byte measurement packets to the relay with a bounded startup
  (256 KiB/s, then 2.885 × the delivery rate per round trip, until the delivery rate stops growing
  by 25 % for three rounds; at most 3 s and 16 MiB) and takes the largest delivery rate as the
  uplink. Clients = uplink ÷ per-client budget (the session's `peer_bytes_per_second`, 256 KiB/s by
  default). Not run against a relay on this machine.

| Method | Params | Result |
|---|---|---|
| `net.host_check` | `host` (optional label) | `{ hosts: [ check ] }` |

Each check:

| Field | Meaning |
|---|---|
| `label`, `relay`, `join_code` | The label it was published under, the relay, the join code (or `null`). |
| `mapping` | `mapper` (`pcp`, `nat_pmp` or `upnp`), `external` (the public address clients use), `internal_port`, `lifetime_s` (0: a permanent UPnP mapping), `renewals`; `null` without a mapping. |
| `mapping_error` | Why there is no mapping: `port_map_refused` (a gateway answered and refused), `port_map_unavailable` (no gateway answered, or none was found), `port_map_lapsed` (renewals failed until the lifetime ran out; the host keeps trying), `port_map_off` (a local relay), `port_map_disabled` (the dedicated host ran with `--no-port-map`, N-19, ADR-10069); `null` while the race runs or with a mapping. |
| `reachable` | `checking`; `reachable` (the probe arrived); `unreachable` (the relay sent it four times and nothing arrived within 1 s of the last); `no_mapping`; `relay_cannot_probe` (the relay has no probe port); `relay_no_answer` (four requests, no acknowledgement); `relay_too_old` (a relay older than protocol 3). |
| `uplink_bytes_per_second` | The measured uplink toward the relay; `null` until a sample. |
| `uplink_end` | `running`, `full_pipe` (the estimate is the uplink), `capped` (time or byte cap first: the uplink is at least the estimate), `relay_no_answer`, `relay_too_old`; `null` when the measurement does not run. |
| `per_client_bytes_per_second` | The budget the estimate divides by. |
| `estimated_clients` | Clients the uplink carries at that budget; `null` without an uplink sample. |
| `done` | Every part has its answer. |

```json
{ "hosts": [ { "label": "server_host", "relay": "192.0.2.1:7800", "join_code": "K7QX2M",
  "mapping": { "mapper": "pcp", "external": "198.51.100.20:7777", "internal_port": 7777,
               "lifetime_s": 7200, "renewals": 0 },
  "mapping_error": null, "reachable": "reachable", "uplink_bytes_per_second": 2481000,
  "uplink_end": "full_pipe", "per_client_bytes_per_second": 262144, "estimated_clients": 9,
  "done": true } ] }
```

## Errors

| Code | When |
|---|---|
| `host_check_bad_params` | The params carry a key other than `host`, or `host` is not a non-empty string. (Params that are not an object are refused by the protocol with `F0002` before the method runs.) |
| `host_check_unknown` | No host that ran the check has the label in `host`. `{}` lists them. |

## Reading it

- `reachable` with a mapping: clients can connect straight to `external`; the relay is only a
  fallback.
- `unreachable` with a mapping: the router granted the mapping but something in front of it (a
  second NAT, a carrier-grade NAT, a firewall) drops inbound UDP. Clients still join by code through
  hole punching or the relay.
- Limit: the relay has one IP. Behind a NAT that filters by address only (not port), the probe also
  passes through the NAT's own state toward the relay, so `reachable` then means "a new client from
  the relay's address would get in", which a second relay address would be needed to tell apart
  (RFC 5780 § 4.4).
- `estimated_clients` is the uplink alone: CPU, the per-host relay caps
  (`caps.max_clients_per_host` in `net.relay_status`) and the game's own rate can bind first.
