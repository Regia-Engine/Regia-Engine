# Delegated hub sessions (agents)

Codec generation 2 of the self-hosted lobby hub (spec 89 § 16.2, ADR-10378). The handshake
protocol stays 1, so a v0.7.0 client still reaches a v0.7.1 hub and a v0.7.1 client still reaches
a v0.7.0 hub. Version-2-only calls are refused. They do not disconnect the peer.

No new agent method. Registering a server, revoking a binding, creating a delegated lobby,
setting relay-only, setting lobby bans and closing a lobby run in v0.7.1 (see
[Behavior](#behavior-nl-07-adr-10384)); only the client-side `join::set_bans` helper still returns
`hub_not_ready`. The new codes are counted in the existing `lobby.hub_status` `refused` map
([Lobby hub status](net-lobby-hub.md)).

| Code | When |
|---|---|
| `hub_needs_wire_v2` | A version-2-only call and the other side is still wire 1. The hub replies `Refused` and keeps the link. A client that saw a hub game version other than `"2"` returns this locally and sends nothing. |
| `hub_not_ready` | The call is on the wire and this build does not run it yet. The link stays up. |

`HubLobbyBackend::note_hub_game_version` records the hub's probe text. `"2"` is codec generation 2.
Empty, `"2.0"`, and any other text stay on wire 1.

`plans_for` honors `relay_only` (NL-08, ADR-10391). A listen lobby with the flag clear still plans mapped, then punch, then relay. See [Relay-only plans](#relay-only-plans-nl-08).

Fuzz target `net.lobby_hub_msg` includes the new messages and still includes the old ones. A count
above its cap (8 owner proofs, 32 bytes of game version, 256 server bans, 64 lobby bans) and a
grace above 24 h are refused before a `Vec` is grown from that count.

## Example

`lobby.hub_status` is unchanged. A refused version-2 call adds a key:

```json
{ "method": "lobby.hub_status", "params": {} }
```

```json
{ "hubs": [{ "refused": { "hub_needs_wire_v2": 1, "hub_not_ready": 1 } }] }
```

## Behavior (NL-07, ADR-10384)

`register_server`, `revoke_binding`, `create_delegated`, `close_lobby` and `set_reach_mode` now run.
`HubServer::set_bans` and the host's `SetBans` replace the lobby ban set. `join::set_bans` still
returns `hub_not_ready`. No new agent method is registered. Two report fields carry the new state.

`lobby.hub_servers` is `HubReport.servers`: each dedicated server's key, capacity, load, protocol,
game version, `relay_only`, owner count and grace. No address and no owner-key list.

`lobby.hub_bindings` is the local list from `HubLobbyBackend::bindings`: proofs this player built
with `bind_server` (server key, serial, expiry, proof bytes). It is not a hub-wide dump.

A binding is one Noise pattern `X` message (`Noise_X_25519_ChaChaPoly_BLAKE2s`) to the hub static
key. The prologue is `regia.lobby_hub.server_binding v1`. The payload is 49 bytes: version `1`,
the server key, expiry and serial. A server presence proof uses prologue
`regia.lobby_hub.server_presence v1` and the hub link's handshake hash. A low-order static or
ephemeral key is `hub_binding_invalid`. Expiry `0` does not expire. Any other expiry must be at
most 366 days ahead.

An ownerless `RegisterServer` (no owner proofs) may `create` a public lobby. The card's owner
handle is the server session. `CreateDelegated` needs a proven player, a live binding and an
online server. The stored session address stays empty. `regia.server` in that spec data is
`hub_address_in_data`. The same refusal covers an address spelling on any lobby or member value
at `set_data`, `set_member_data` and create: a socket address, `udp:` or `udp://` in any ASCII
case plus one, a bracketed IPv4 or an IPv6 with a zone and a port, and `steam:` in any ASCII case
plus digits. A bare address, a hostname and a port alone are ordinary text. A handle that is a
spelling is stored as a stand-in, so presence, directory cards, room member names and invites do
not carry it. A listen confirm stores a flag and no address. Members are told
`SessionConfirmed`. A wire-1 peer is sent `ServerSet` of `None` instead.

The dedicated server, not the player owner, sends `ReachOffer` and `Approve`. The member's ticket
names that server's `ServerKey`. The owner's address is not copied into the member's plan, the
room or presence. The password gate, the server's `bans.ron` and `JoinTicketCheck` stay the game
server's job before it calls `Approve`. `AskReach` refuses a banned key with `hub_banned`, and a
full dedicated server with `hub_server_full`, before any candidate is copied. `Approve` rechecks
the operator ban, the lobby ban, and, on a dedicated lobby, the server-seat ban and
`load >= capacity`. Any of those is `ReachDenied` and no plan, including a ban that landed while
the ask was open.

When the owning player disconnects, a delegated lobby enters grace. It does not emit
`OwnerChanged` and it does not make the server the owner. `grace_ms` of `0` has no deadline.
`None` on the client API is 600_000 ms. Listen lobbies still hand off with `OwnerChanged`.

`RevokeBinding` records `(player key, server key, serial)` in memory and in `revocations.ron`
(format 1, temp file plus rename). The next `CreateDelegated` for that serial is
`hub_binding_revoked`. Lobbies that already exist stay up. A wire-1 directory page omits dedicated
lobbies and server cards. A wire-1 join of a dedicated lobby is `hub_needs_wire_v2` and the link
stays up.

| Code | When |
|---|---|
| `hub_binding_unbound` | The server has no live binding for this caller, or it is ownerless. |
| `hub_binding_expired` | The binding's expiry is in the past. |
| `hub_binding_revoked` | This owner revoked this serial. |
| `hub_binding_not_owner` | The caller is not a bound owner of this server. |
| `hub_binding_invalid` | The proof, payload, owner list or horizon is not valid. |
| `hub_server_offline` | The server is not online. |
| `hub_server_in_use` | Another link already holds this server key. The old link is dropped. |
| `hub_server_invalid` | Capacity, load, visibility or grace is not valid. |
| `hub_server_full` | `AskReach` on a dedicated lobby whose `load >= capacity` and `capacity > 0`. Members included. No candidate bytes. |
| `hub_not_session` | A player called offer or approve on a delegated lobby. |
| `hub_not_a_player` | A server session called `CreateDelegated`, or an ownerless server called a player-only create. |
| `hub_ask_rate_limited` | More than 4 asks for one player key, or 16 for one source address, in 10 seconds. Neither window is stamped. |
| `hub_ask_inflight` | More than 2 open asks for one key, or 64 on the hub. Replacing an open ask does not take a second slot. |
| `hub_banned` | The player key is on the operator set, the lobby set, or the server's published list. Checked at `AskReach`, before any candidate is copied. `Approve` checks the operator set and the lobby set again, and a ban that landed while the ask was open is `ReachDenied` with no plan. |
| `hub_address_in_data` | Lobby or member data is an address spelling, or `regia.server` is set. The spelling includes a socket address, `udp:` or `udp://` in any ASCII case, a bracketed address with a port, and `steam:` in any ASCII case plus digits. A dedicated confirm is the same refusal. A listen confirm stores a flag only. |
| `hub_lobby_closed` | A non-member asked reach on a lobby that is not joinable. A member of a closed listen lobby may still ask. |
| `hub_lobby_full` | A non-member asked reach on a lobby already at `max_members`. |
| `hub_revoke_full` | The revocation set is at 4096 entries. This serial was not stored. |
| `hub_draining` | The hub is draining. New registers and new lobbies are refused. Online servers stay. |
| `hub_needs_wire_v2` | A version-2-only call, or a join of a dedicated lobby, while the peer is still wire 1. |
| `hub_not_ready` | `join::set_bans`. `HubServer::set_bans` and the host's `SetBans` replace the lobby ban set. The link stays up. |

## Relay-only plans (NL-08)

When `relay_only` is set on the `plans_for` call, on the owner's `ReachOffer`, or on the member's
`ReachOffer`, the only success path is the relay. The owner must have offered a relay and a join
code. Otherwise approve is `hub_no_reach` and the ask stays `asked`. Neither plan carries a
candidate or a mapped address. The member's ticket is rewritten to an unspecified address
(`0.0.0.0`, port 0) with the same server key. The relay address and the join code stay. The
owner's ticket stays empty.

`JoinPath::relay_first` is that member's path. It sends no punch introduction and no connectivity
check. `RelayRegistrar::relay_only`, called before the first update, rewrites the host token the
same way, sends `Register` with no candidates and with the hide-address byte, and the relay then
publishes no address in `HostInfo`, `Allocated` or `ClientJoined`. A ticket from
`JoinTicket::from_game_socket` is not relay-only by itself: that is the punch-capable host path.

`set_reach_mode` stores the lobby flag. The listen owner, a delegated lobby's owning player or its
bound server, or an ownerless lobby's server session may set or clear it. Approve passes that
stored flag, so it hides every member. An offer that sets `ReachOffer.relay_only` hides that
exchange, including the member's address on `ReachAsk`. The dedicated host's `--relay-only` calls
`RelayRegistrar::relay_only` and skips the router mapping and the host check. `join::set_bans`
still returns `hub_not_ready`. No new agent method. `hub_no_reach` is the existing code.

## Key rotation and the relay process (NL-11)

`regia-lobby-hub` polls a `RelayServer` on the same process tick as the hub when `hub.toml`
`relay.enabled` is set (`format_version` 2, ADR-10397). The relay port is 7812. It is not the hub
listen port. A fresh package writes `relay.enabled = true`. `regia upgrade-project` rewrites a
version-1 file to version 2 and writes `relay.enabled = false`, so an existing hub does not open
the relay port until an operator turns it on. Version 1 still loads. `[bans] path` is a list of
player keys (one 64-digit hex line each; a missing file is an empty set). `[log] dir` is where
the process appends `hub.log`. The process logs the ban count, not the keys.

SIGTERM or SIGINT stops new sessions and new relay allocations. Refresh of an allocation already
issued continues. The process exits once those allocations are gone, or once 30 seconds of the
process clock have passed, whichever comes first. A handshake during the drain is refused with
`hub_draining` and allocates no session. That peer can stay on the link. Live and dormant
sessions hold the exit. A refused link is waited for, and never past the 30 second window
(ADR-10419). A second signal exits at once. The drain does not add a relay wire
code: a new `Allocate` is the existing full reply. `GET /health` and `lobby.hub_status` report
`relay_allocations`, `servers_online` and `draining`. HTTP 503 while draining. The packaged
`regia-hub.service` sets `TimeoutStopSec` to 45 seconds (that 30 second window plus 15,
ADR-10419), so systemd does not send SIGKILL before the process exits 0. The dedicated-server
unit stays at 10 seconds.

The process holds one Noise static key. Rotation is two processes, not two keys in one handshake.
Bring the new key up on a new port, ship a game config whose pin list tries the new hex key and
then the old one, wait the adoption window, stop the old process, and drop the old hex key from
the next game config. The private key stays in `key_path`, outside any project. The hex public
key is what a game ships. Overlap longer than 7 days is an operator choice, not a wire field.

| Code | When |
|---|---|
| `hub_key_mismatch` | The client pinned a hub key this process does not hold. The link ends during the handshake. No session is allocated. |
