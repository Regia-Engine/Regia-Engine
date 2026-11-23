# Relay status (agents)

`net.relay_status` reports the relay reports a process published (spec 120 § 10.6, ADR-8183). A
dedicated server started with `--relay` publishes one under the label `server_host` once a second:

- with `--relay local` (role `host+relay`): the host's registration and the relay it runs, with its
  counters, caps, refusals by reason and every allocation;
- with `--relay HOST:PORT` (role `host`): the host's registration only (relay address, join code,
  relayed clients, confirmed allocations). The remote relay's own counters are on its console
  (`relay` command).

A game or tool that runs its own relay publishes with `NetAgentState::publish_relay(label,
RelayServer::report())`.

| Method | Params | Result |
|---|---|---|
| `net.relay_status` | `relay` (optional label) | `{ relays: [ report ] }` |

Each report:

| Field | Meaning |
|---|---|
| `label`, `role` | The label it was published under; `relay`, `host` or `host+relay`. |
| `relay` | The relay's address. |
| `join_code` | The host's join code, or `null` before the relay gave one. |
| `relayed_clients` | Clients the relay announced to this host (`ClientJoined`). |
| `confirmed_allocations` | Of those, allocations whose host side the relay confirmed from the game socket. |
| `runs_relay` | Whether this process runs the relay (the fields below are present only then). |
| `protocol`, `lobbies`, `allocations`, `ports`, `ports_free`, `hosts`, `most_clients_of_one_host`, `forwarded`, `refreshed` | The relay's `RelayStatus`. |
| `caps` | `allocation_ports`, `max_clients_per_host` (40), `max_allocations_per_ip` (8), `allocation_bytes_per_second` (4 MiB/s), `host_bytes_per_second` (64 MiB/s), `relay_bytes_per_second` (0 = none). |
| `refusals` | By reason: `full` (a cap answered `Full`), `permission` (a datagram to an allocation port from an address with no permission), `refresh_key` (a `Refresh` whose key matched neither side), `version` (`VersionTooOld`/`VersionTooNew`), `rate_limited` (control requests over the per-source limit), `allocation_rate`, `host_rate`, `relay_rate` (forwarding over a bandwidth cap). |
| `allocation_list` | One row per allocation: `port`, `host`, `client`, `bytes_to_host`, `bytes_to_client`. |
| `nat` | A host registered from its game socket (ADR-8340, spec 120 § 10.7): `candidates` (`kind` `host`, `mapped`, `srflx`, `prflx` or `relay`, and `addr`) it announced to the relay; `sessions` (introductions the relay delivered: one per client that joined by code through a protocol 3 relay), `active_sessions`, `checks_sent` and `checks_answered` (connectivity checks), `checks_dropped` (no such session, malformed), `nominated` (sessions whose client nominated a direct pair), `not_nominated` (ended without one: those clients went through the relay or gave up). Absent for a relay and for a registrar on a socket of its own. |

```json
{ "relays": [ { "label": "server_host", "role": "host+relay", "relay": "127.0.0.1:7800",
  "join_code": "K7QX2M", "relayed_clients": 16, "confirmed_allocations": 16, "runs_relay": true,
  "protocol": 3, "allocations": 16, "ports": 64, "ports_free": 48, "hosts": 1,
  "most_clients_of_one_host": 16, "forwarded": 912345, "refreshed": 16,
  "caps": { "allocation_ports": 64, "max_clients_per_host": 40, "max_allocations_per_ip": 8,
            "allocation_bytes_per_second": 4194304, "host_bytes_per_second": 67108864,
            "relay_bytes_per_second": 0 },
  "refusals": { "full": 0, "permission": 0, "refresh_key": 0, "version": 0, "rate_limited": 0,
                "allocation_rate": 0, "host_rate": 0, "relay_rate": 0 },
  "allocation_list": [ { "port": 50000, "host": "203.0.113.7:40000", "client": "198.51.100.4:5000",
                         "bytes_to_host": 1200345, "bytes_to_client": 7004512 } ] } ] }
```

## Errors

| Code | When |
|---|---|
| `relay_bad_params` | The params carry a key other than `relay`, or `relay` is not a non-empty string. (Params that are not an object are refused by the protocol with `F0002` before the method runs.) |
| `relay_unknown` | No report has the label in `relay`. `{}` lists the labels. |

Reading `nat`: on a healthy host most `sessions` end `nominated`; a host whose clients all end
`not_nominated` sits behind a NAT that maps per destination (symmetric) or a firewall that drops
inbound UDP, and every client plays through the relay (see `allocations`).

## Withdrawing a join code

`RelayRegistrar::revoke_code(now_ms)` withdraws the host's code (N-19, ADR-10069): the registrar stops renewing and
sends a `Register` with a lifetime of 0 until the relay confirms (`code_revoked()`); `join_code()` is `None` at once.
The relay removes the lobby and answers `not_registered`; a later join by the code ends `join_refused`
(`unknown_code`). Players already joined stay. A relay built before this change clamps the 0 to one second, so the
code lapses within it. A host that wants a code again makes a new registrar (a new code).

## Join by code: the client's codes

A client that joins with `ConnectTarget::JoinCode` (`net.connect` or `NetSession::connect`) asks a
protocol 3 relay to introduce it to the host (both sides get each other's candidates); both send
paced connectivity checks from their game sockets (one per 50 ms per session) and the handshake runs
on the best pair that answered. From 500 ms the client also takes a relay allocation of its own; the
relay path wins at 1.5 s if no direct pair completed. A protocol 2 relay skips the introduction (the
client tries the host's address only, then the relay). `NetSession::join_route` names the path: `resolving`, `direct`,
`direct_chosen`, `relayed`, `failed`. When it fails, the session ends with one of these codes in
`transport_error`:

| Code | When |
|---|---|
| `join_timeout` | The relay never answered the `Lookup`. |
| `join_refused` | The relay refused (`unknown_code`, a wrong bind key, ...). |
| `join_ticket_invalid` | The lobby's token is not a join ticket of this engine. |
| `relay_too_old` | The direct path failed and the relay speaks protocol 1 (no allocation per client). Update the relay. |
| `relay_full` | The direct path failed and the relay refused an allocation at a cap (`max_clients_per_host`, the per-IP cap or the pool). |
| `relay_no_allocation` | The direct path failed and the relay never confirmed an allocation. |
| `join_relay_invalid` | A bare code with no relay set (`NetSession::set_relay` or `CODE@HOST:PORT`). |

See [Dedicated server (agents)](net-dedicated-server.md) and the operator page
[Dedicated server](../user-guide/dedicated-server.md#5-join-codes-and-a-relay).
