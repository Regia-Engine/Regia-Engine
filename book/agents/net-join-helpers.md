# Join helpers (agents)

LAN announce filtering, address join codes and the store-ticket hook (spec 120 § 14, ADR-7954, ADR-7989,
ADR-8502). No new
agent method. A host sees a refused ticket on the existing `net.status` client state. A late store refusal is
`disconnected:kicked`.

## LAN announce

`discovery::DiscoveryConfig` sets the four-byte magic and the UDP port. The defaults are `RGNA` and UDP 7900,
the same values `ServerAnnounce::encode` and `ServerAnnounce::decode` have always used. `encode_with` and
`decode_with` take another magic. A datagram with a different magic decodes as `Ok(None)`, so two announcers
on one port do not list each other.

`LanSocket` is the UDP announcer. It binds IPv4 `0.0.0.0` with `SO_REUSEADDR` set before `bind`, so two
sockets may share the port, and it publishes a broadcast. A loopback test publishes to `127.255.255.255`,
which stays on the loopback interface and is copied to every socket on that port. A unicast to `127.0.0.1`
is delivered to only one of them. `SO_REUSEPORT` is not set. Each socket then keeps only its own magic.
`LanSocket::poll` returns each announce with the address it came from. The announce carries only
`game_port`, so the host to join is `SocketAddr::new(from.ip(), announce.game_port)`. One poll reads at most
`LAN_POLL_MAX_DATAGRAMS` (256) datagrams, kept or skipped; a flood on the public port waits for the next poll
instead of holding the caller's thread.
`LanAnnouncer` on a `DiscoveryHub` is the in-process form of that filter. A listener that polls can publish
past 1,024 datagrams. A listener that never polls stops receiving; the others still do.

| Code | When |
|---|---|
| `lan_announce_invalid` | `LanSocket::publish` could not encode the announce. |
| `lan_send_failed` | The UDP send failed. |

## Address join codes

`mint_address_join`, `encode_address_join` and `decode_address_join` carry a host address, a port, a nonce and
an expiry. They sit beside the six-character relay codes and use the same alphabet, so a typed relay code is
not read as an address. The default lifetime is 12 hours (`ADDRESS_JOIN_TTL_DEFAULT_S`). The caller passes
`now_unix_s`: the code is valid while `now < expires_at` and expired when `now >= expires_at`.

The code is not a secret. The host password stays the access control. An IPv6 scope id is not stored.
The nonce and the expiry are advisory: only the decoding client checks them. The host never sees them,
so a host cannot revoke a code. Do not put the nonce in the join token.

| Code | When |
|---|---|
| `address_code_invalid` | The text is not an address code, the checksum fails (a one-character typo), the layout version is unknown, or the address family does not match. |
| `address_code_expired` | `now_unix_s` is at or past the code's expiry. |

## Store ticket

`NetGame::join_ticket` returns the ticket bytes. The default is an empty slice, so existing games are unchanged.
`NetServer::set_ticket_check` installs a `JoinTicketCheck`. The server calls `verdict` after the password gate,
after `validate_join`, and after the live-peer, per-address and capacity checks, immediately before the peer
is added. No hook means a ticket does not deny the join. A wrong password is `bad_password` and the hook is
not called. A full server denies with `full` and does not open a store session.

A refusal keeps `DenyReason::InvalidJoin` on the wire (no new deny byte, wire version stays 2) and puts
`ticket_refused` in the deny detail. `ClientEnd::as_str` returns `ticket_refused` for that denial, so
`net.status` reports the client state `disconnected:ticket_refused`.

`TicketVerdict::Pending` means the ticket parsed and the peer may enter. The server stores the ticket and
calls `end_ticket` when that peer leaves, and when admission still fails after the verdict. `take_refusals`,
read at the end of every `NetServer::receive` (so an ECS loop that calls only `receive` and `flush` applies it
too), or `NetServer::finish_ticket(peer, false)`, removes the peer with `DisconnectReason::Kicked`.
That client state is `disconnected:kicked`. `finish_ticket(peer, true)` leaves the peer in place.

With a resume grace (`ServerConfig::resume_grace_ms`), a player that times out is held dormant and keeps its
store session for the resume. The session ends when the hold ends. A late refusal for a dormant player ends
the hold. A resume that brings a new ticket ends the old session; a resume with the ticket its peer holds keeps
that session and the hook is not called again. A join with no resume token whose ticket equals a dormant hold's
(a restarted process) ends that hold and its session first, then the hook runs.

| Code | When |
|---|---|
| `ticket_refused` | The installed `JoinTicketCheck` refused `NetGame::join_ticket` at join time. Client state `disconnected:ticket_refused`. |
| `kicked` | A pending ticket was refused later (`take_refusals` or `finish_ticket(peer, false)`). Client state `disconnected:kicked`. |
| `full` | The server or the per-address cap is full. The ticket hook does not run. |
| `bad_password` | The host password did not match. The ticket hook does not run. |

Steam implements the hook as `regia_steam::SteamJoinTicketCheck`. Tests use `SteamJoinTicketCheck::allow`, an
in-memory list, or `SteamAuthSessions` over a fake `SteamAuthApi`, and make no network call. The live path
(`LiveSteamTickets`, `SteamAuthSessions` on the Steamworks client) is compiled only with the `steamworks`
feature. `BeginAuthSession` ok is `Pending`. The host must call Steam `run_callbacks`; this crate does not.
`ValidateAuthTicketResponse` with any error is a later kick while that session is open; a response that
arrives after the session ended is ignored, so it cannot kick a rejoin. The server calls `end_ticket` when the
peer leaves, which calls `EndAuthSession` if that ticket is still the open session for its Steam id.
`DuplicateRequest` for a session the hook holds is a client that restarted on a new address: the old session
ends, the old peer is kicked (`kicked`), and the new ticket begins again as `Pending`. `DuplicateRequest` for a
session the hook does not hold is `ticket_refused`. `cargo test` does not run the live callback. Until the
callback, a pending peer is in the game.

Join data is at most 512 bytes (`MAX_JOIN_DATA_BYTES`). A Steam session ticket is about 240 bytes. A game
that puts other bytes in the same join blob can overflow that budget and the join fails. The cap is unchanged.

Since ADR-10069 (KI-NT-047) the hook installs on every server type:

| Server | Install | The client's ticket |
|---|---|---|
| `NetServer` | `NetServer::set_ticket_check` | `NetGame::join_ticket` of the join data |
| `NetLink` server | `NetLink::set_ticket_check(Arc<dyn JoinTicketCheck>)` (false while idle or on a client) | `NetGame::join_ticket` of the data given to `NetLink::connect_with_join` (sent again with every resume) |
| `NetSession` host (listen or dedicated) | `NetSession::set_join_ticket_check(Some(..))`, before or while hosting; every later host keeps it | `ConnectOptions::auth` (the hello's `auth` bytes; empty when none) |
| Dedicated `ServerHost` | `ServerHost::set_join_ticket_check` before `start` (hands it to the game's session) | as `NetSession` |

On a `NetSession` the verdict runs after the password (a wrong one is `bad_password` and never reaches the hook) and
before the authenticator, on every hello from the transport, a resume's too (a forged reconnect token cannot skip
it). In-process loopback and simulated clients are not checked. `Refuse` answers `auth_failed` with the detail
`ticket_refused`; a late refusal kicks the player with `kicked: ticket_refused`; `end_ticket` runs when the player is
gone (not while it waits in grace), when a link with a verdict is refused or drops before admission, and for the old
ticket when a resume brings other bytes. `SessionServer::has_ticket_check` says whether one is installed. One
`Arc` may serve several servers (`JoinTicketCheck` is implemented for `Arc<T>`).

`SteamAuthSessions` matches each `ValidateAuthTicketResponse` to the oldest begin of that Steam id still waiting for
its first answer (Steam answers begins in order; KI-NT-048, ADR-10069): a refusal that belongs to a session a restart
replaced, or to one that ended before its answer came, does not kick the session after it; that session's own later
refusal still does. A ticket-cancelled answer (`SteamAuthSessions::on_validate_answer` with
`SteamAuthAnswer::Cancelled`; the live callback passes it) that arrives after a restart replaced, or a leave ended, a
session whose first answer was already in, while the new session waits for its own, is the old ticket's cancel and is
dropped. The rule assumes Steam answers every begun session once, in begin order.
