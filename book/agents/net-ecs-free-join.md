# ECS-free hub join (agents)

A game that runs its own simulation can use `regia_net` with `default-features = false` and no
`ecs` feature, and still join a lobby through the self-hosted hub (ADR-10376). Guide topic
`net-ecs-free-join`. The hub itself is [Lobby hub joins and player identity](net-lobby-hub-join.md).
The core build is [A server without the ECS](../user-guide/networking.md#a-server-without-the-ecs).

This page adds no agent method. The `net.*` and `lobby.*` methods read an app, so they need `ecs`.
Without it, read `NetServer::status` and `peer_rows` on the server you drive. The error codes below
are the ones those paths already return. They are lowercase ASCII with digits and `_`.

## What compiles without the ECS

`cargo tree -p regia_net --no-default-features -e normal,build` must not link `regia_ecs`,
`regia_app`, `regia_scene`, `regia_asset` or `regia_platform` (also `regia_audio` and `bevy_ecs`,
which `cargo xtask check-net-core` already forbids). `-e features` selects feature edges, not the
crates the library links.

Available in that build:

| Item | Role |
|---|---|
| `server_host::JoinTicket`, `RelayRegistrar`, `relay_split` | The owner's game socket registers with the relay and deposits the ticket (game address and `ServerKey`). |
| `server_host::JoinPath`, `JoinSocket` | Punch, then the relay, on a datagram socket. `relay_first` skips checks when the broker chose the relay. |
| `server_host::TestClock` | A clock the test advances. `HubClock::manual` is the hub client's clock. Step them together. |
| `lobby::hub::HubReach` | Runs a `JoinPlan` on a socket it owns. |
| `lobby::hub::JoinPlanDriver` | The same plan when the game keeps the socket. |

`HostGame` and `ServerHost` stay behind `ecs`. An `ecs` build calls the same helpers.

## Caller-owned socket

`JoinPlanDriver::start(local, &plan, client_key)` binds the plan as if the game socket were at
`local`. `client_key` is `server_host::fresh_allocation_key`.

Each frame:

1. `push_datagram(from, bytes)` for every datagram the game socket received. `false` means it was
   not queued: longer than `MAX_DATAGRAM_BYTES` (refused whole, never cut) or the inbound side
   already holds `PLAN_QUEUE` (4096). Either one counts in `dropped`.
2. `advance(now_ms)` with the game's clock.
3. `poll_send` until it returns `None`, and send each datagram on the game socket. A full outbound
   queue makes the plan's socket return `WouldBlock` and counts a drop.
4. Once `state` is `Ready`, `take_endpoint` returns the endpoint socket, the address to connect it
   to, and the `ServerKey` to pin. The key is the plan's (the owner's ticket from the hub), including
   on the relay path. Call it once.
5. Keep pushing and polling. The endpoint and a relay control half that is still running share the
   queues.

`outcome` is `(Mapped, 0)`, `(Punch, checks_sent)` or `(Relay, checks_sent)` once the path has
settled. Report it with `HubLobbyBackend::report_reach` after the endpoint is connected, the same
as `HubReach`.

## Error codes

No code was added.

| Code | When |
|---|---|
| `hub_reach_invalid` | `start` refused the plan: mapped with no address, or punch or relay with no relay and join code. |
| `hub_key_mismatch` | The relay's ticket names a different `ServerKey` than the plan, or names one when the plan names none. |
| `join_code_invalid` | The join code is not a join code. |
| `join_relay_invalid` | The relay address is not usable. |
| `join_relay_bind_failed` | The relay path could not bind. |
| `join_refused` | The relay refused the join. |
| `join_ticket_invalid` | The relay's answer was not a ticket. |
| `join_timeout` | The path gave up. |
| `relay_too_old` | The relay is older than this join path speaks. |
| `relay_full` | The relay has no room. |
| `relay_no_allocation` | The relay allocated no port. |

`ReachState::Failed` carries that code. `take_endpoint` returns `None` until `Ready`, and after it
has been taken.

## Tests

`cargo test -p regia_net_soak --no-default-features --test ecs_free_join` builds `regia_net` without
`ecs` (the soak kit depends on it with `default-features = false`; a const assert fails the build
if `ECS_FEATURE` is still set) and runs one `JoinPlan` on a caller-owned socket over `SimNat` and
`TestClock` for each path:

- mapped: an open host with a mapped candidate, a symmetric member, no relay allocation;
- punch: a port-restricted host and a full-cone member, connectivity checks sent;
- relay: both symmetric, no checks, a relay allocation, the sealed handshake pinned to the owner's
  `ServerKey`.

The same target runs when the soak kit's `ecs` feature is on, so the driver stays green for ECS
callers. `HubReach`'s own tests are unchanged. `cargo test -p regia_net --no-default-features` is
not this proof: that crate's dev-dependencies unify `ecs` back on.
