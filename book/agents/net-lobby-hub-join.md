# Lobby hub joins and player identity (agents)

The self-hosted lobby hub (`regia_net::lobby::hub`, [Lobby hub status](net-lobby-hub.md)) also
brokers how a member reaches the lobby owner's game session, and binds every hub session to a
player key (ADR-8607). The hub never carries game datagrams.

## Player identity

A player is an X25519 key pair (`regia_net::crypto::PlayerIdentity`; `HubClientConfig::identity`,
a fresh one per connection when unset). The client's `Hello` carries a proof: one Noise `X`
message to the hub's key whose payload is the sealed hub link's handshake hash, so it fits that
one link only. The hub records the proven `PlayerKey` as the session's identity:

- a `Hello` whose proof does not verify on its link (made for another link, another hub, or
  garbage) is refused with `hub_identity_mismatch`, and no session exists;
- a proof whose static or ephemeral key is the zero key or another low-order X25519 point is
  refused the same way (`verify_player_proof` returns `weak_key`, ADR-8665): the X25519 output of
  such a key is all zeros for every hub, so the proof shows no private key. The hub never records
  `PlayerKey([0; 32])` for a session, and `HubServer::connect_player` refuses that key with
  `hub_identity_mismatch`;
- a `Welcome` that names another key than the client's is refused by the client with
  `hub_identity_mismatch` (its `HubLinkState` ends with that code);
- one key holds one handle: the same key under another handle is `hub_identity_in_use`; under
  the same handle it gets its session back (a reconnect; the old link is dropped);
- a lobby mute holds on the muted player's key: leaving and joining again, or a new session under
  the same key, stays muted.

## Join brokering

1. The owner offers its game socket's reach: `HubLobbyBackend::offer_reach(lobby, ReachOffer)`
   with its candidates (a port mapping as a `mapped` candidate), its NAT class when known, the
   relay, the relay's join code and the `JoinTicket` (game port and pinned server key).
2. A member asks: `ask_reach(lobby, ReachOffer { candidates, nat, .. })`.
3. The owner approves: `approve(lobby, member, true)`, or `HubClientConfig::auto_approve`.
4. The hub sends both sides a `JoinPlan` with the other's candidates; the path is, in order:
   `mapped` (the owner's mapping, dialed directly), `relay` (both NAT classes known and no direct
   pair can work by the RFC 4787 model: the relay allocation at once, no checks), else `punch`
   (paced connectivity checks through the relay's introduction, the relay winning at 1.5 s).
5. The member runs the plan with `HubReach` on its game socket and connects its endpoint to the
   address and key `take_socket` gives; then it reports the path: `report_reach(lobby, path,
   punch_attempts)` (`HubReach::outcome`). Both sides see `LobbyEvent::Reached { lobby, member,
   path }`.

`lobby.hub_join_status` reports what the hub saw:

| Method | Params | Result |
|---|---|---|
| `lobby.hub_join_status` | `hub` (optional label) | `{ hubs: [ joins report ] }` |

Each report:

| Field | Meaning |
|---|---|
| `label` | The label it was published under (`HubHost::publish`). |
| `joins` | At most 256 rows, lobby then member order: `lobby`, `member` (decimal strings), `handle`, `identity` (first 8 hex digits of the player key), `platform` (the member's `platform` member data, or `null`), `nat` (`full_cone`, `port_restricted`, `symmetric`, or `<mapping>_<filtering>`; `null` when not told), `state` (`asked`, `planned`, `reached`, `denied`), `planned` and `path` (`mapped`, `punch`, `relay`, or `null`), `punch_attempts` (checks the member sent), `relayed`. |
| `paths` | `{ mapped, punch, relay }`: reached joins per path. |
| `identity_mismatches` | Hellos refused `hub_identity_mismatch`. |
| `identity_in_use` | Hellos refused `hub_identity_in_use`. |

```json
{ "method": "lobby.hub_join_status", "params": {} }
{ "method": "lobby.hub_join_status", "params": { "hub": "hub" } }
```

## Errors

| Code | When |
|---|---|
| `hub_bad_params` | Params are not an object, carry a key other than `hub`, or `hub` is not a non-empty string. |
| `hub_unknown` | No report has that label; call with `{}` to list them. |

## Hub codes

In `refused`, `HubLobbyBackend::last_error` and `HubLinkState::Ended`:

| Code | Meaning |
|---|---|
| `hub_identity_mismatch` | A `Hello` whose player proof does not match its sealed link, or a `Welcome` naming another player key. |
| `hub_identity_in_use` | This player key already holds a session under another handle. |
| `hub_no_reach` | An ask before the owner offered, or an offer with no mapping, candidate or relay. |
| `hub_reach_invalid` | More than 8 candidates, a join code that is not one, or a ticket over 128 bytes; a plan `HubReach` cannot run. |
| `hub_no_ask` | An approval or a report for a member with no open ask or plan. |
| `hub_reach_denied` | The owner denied the ask. |

`hub_key_mismatch` also ends a `HubReach` whose relay ticket names another server key than the
owner's ticket from the hub link, or names a key when the owner's ticket names none: a plan with
no server key never adopts the relay's key (ADR-8665).
