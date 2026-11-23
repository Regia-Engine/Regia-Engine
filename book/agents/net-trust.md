# Net trust: known hosts, rekeying and NAT rebinding (agents)

Guide topic `net-trust`. ADR-2416, on top of the transport encryption of [net-crypto](net-crypto.md) (ADR-1901). Three things
keep a sealed `regia_net` link trustworthy over a long session:

| Feature | What it does | Where |
|---|---|---|
| Trust on first use | A client pins the server's key the first time it meets a host, and refuses a different key later | `regia_net::known_hosts`, `LinkOptions::known_hosts` |
| Rekeying | Each direction moves to a fresh key after a number of records or bytes | `RekeyPolicy` on `ServerConfig::rekey` and `ClientConfig::rekey` |
| NAT rebinding | A client whose public address changes keeps its session once the new address answers a probe (path validation, ADR-8299); a client follows a server whose address changes the same way | `NetServer`, `NetClient`, always on |

## Trust on first use

Without a pin, a client accepts any server key: it resists a listener, not an attacker in the middle (net-crypto). A known-hosts
store closes most of that gap the way SSH clients do: the first connection to a host records the key, and every later connection
must show the same key.

- Turn it on per link: `LinkOptions { known_hosts: Some(<user dir>/known_hosts.ron), host_id: "...", .. }`. The file format is
  [known-hosts-ron](../../docs/formats/known-hosts-ron.md).
- The host id is the server address unless the game passes a stable name (`host_id`), for servers that move between addresses.
- First use: the link connects, records the key, and emits `LinkEvent::HostKeyPinned { host, key, saved }`. Show the key to the
  player if the game cares; `saved: false` means the file could not be written and the pin lasts for this connection only.
- Later: a server with the stored key connects quietly. Another key ends the attempt with `LinkEvent::Ended { reason:
  "net_host_key_changed" }`, before any password or token leaves the client, and the store keeps the old key.
- An explicit pin (`LinkOptions::server_key`) wins over the store; the store is neither read nor written.
- A loopback link left plain by `encryption: auto` has no key, so nothing is recorded.
- A damaged or newer store refuses the connection (`net_known_hosts_unreadable`, `net_known_hosts_newer`) and is never overwritten.

What it does not protect: the very first connection. An attacker in the middle at that moment gets their key pinned. A key the
player got from a trusted channel (a join code, the store page) still belongs in `server_key`.

A server that lost its `server.key` shows a new key to every client that pinned the old one. Tell players, or have them run
`net.forget` (or the game's equivalent) for that host.

## Rekeying

Each direction of a sealed link switches to the next key after `RekeyPolicy { after_records, after_bytes }` (defaults: about a
million records or 1 GiB per key; `0` turns a limit off; `RekeyPolicy::NEVER` turns rekeying off). The next key is the Noise
`REKEY` of the current one, so no message is exchanged. The sender writes its key epoch in the record header; the receiver follows
on the first record that authenticates under the next key, keeps the previous key for records the network reordered across the
switch, and catches up over up to 16 keys whose records were all lost. Record counters run on across keys, so one replay window
covers the whole session. Nothing here reads a clock.

`net.crypto` reports `rekeys`: key switches on the live sealed links, both directions summed. Both sides of a quiet link report
the same number.

## NAT rebinding

Home routers and mobile networks change a client's public address or port mid-session; a carrier-grade NAT may put the phone
behind an address that already holds many players. The server finds the peer by its session token (one map probe) and treats a
record from a new address as a **candidate** only if, cheapest check first:

1. the new address has trial decryptions left this second for this peer (8, `REBIND_ATTEMPTS_PER_S`, counted per peer and
   address: forgeries from elsewhere cannot spend the budget of the address the client really moved to),
2. no other peer holds the address, and it is not banned,
3. its counter is newer than any record the peer sent (a late copy of an old record cannot start a move), and
4. it authenticates under that peer's session key (the address proves nothing).

A candidate's record is authentic, so its content is delivered, but **the peer does not move yet** (ADR-8299, RFC 9000 § 8.2 and
§ 9.3, no wire change): the server keeps sending to the validated address and sends one ping packet, a *probe*, to the
candidate alone (transport v2 acknowledges a lone ping at once). When a record from the candidate acknowledges the probe's
packet number, the client has shown it
receives traffic there and the peer moves; its round-trip estimate restarts on the new path. Until then the server sends the
candidate at most three times the bytes it received from it (§ 8.1 anti-amplification). An unanswered probe is resent at most
every 100 ms (`PATH_PROBE_RESEND_MS`).

So an on-path observer that copies a fresh record and races it from its own address (soak finding F1) never moves the peer: its
copy is delivered once, the probe goes to the observer, and the client's real records keep the peer where it is. If the client's
NAT really moves it while someone races every record, the client's own copies arrive as duplicates from the new address; a
duplicate of one of the last 64 records (`DUPLICATE_CANDIDATE_RECORDS`) still makes the address a candidate (nothing in it is
delivered twice), and its acknowledgment of the probe moves the peer there.

- A move needs no waiting interval: a phone switching back from cellular to Wi-Fi is followed within a round trip plus a probe.
  (`REBIND_MIN_INTERVAL_MS` is kept for source compatibility and no longer applied.)
- `max_peers_per_ip` limits joins only. A peer that moves keeps its one slot, whatever address block it moves into.
  On a server that keeps resume tickets (`resume_grace_ms` above 0), a player that leaves holding its ticket (a goodbye,
  or a timeout into dormancy) keeps its slot of the block for the grace: its return with the ticket from the same block
  is never counted as a new peer, and a fresh join counts the held slots too, borrowing at most one. A block so holds
  at most `max_peers_per_ip` + 1 peers through joins, however its players come and go (ADR-8733); `net.status` totals
  `held_slot_returns` counts the returns. A return spends its hold only once its client confirms the accept, so a
  retry after a lost accept is the same peer, and the ticket from another block leaves the hold alone (ADR-8883).
- Plain links rebind the same way, with the session token and a newer packet sequence in place of the tag, with one
  difference (ADR-10077, KI-NT-018): nothing authenticates a plain packet, so a candidate's packets are **held**, not
  applied, until a packet from there answers the probe (at most 16 packets and 24 KiB per candidate; over that they are
  dropped and counted in `rebinds_refused`). Then the held packets are applied in arrival order. A forger who copied the
  token from the wire cannot inject anything from an address it picks; a spoofer of the peer's own address still can (a
  plain link has no key: use encryption off loopback).
- The client does the same for the server: an authentic, newest record from a new server address makes it a candidate, the
  client probes it, and follows when it answers (a server whose NAT moved its port, a multi-homed host).

The peer keeps its id, its reliable channel and its replication state.

`net.status` totals: `rebinds` (validated moves), `rebinds_refused` (records with a peer's token from a new address that were not
let in, including duplicates that did not validate) and `path_probes`. `net.peers` rows carry the same per peer: `rebinds`,
`rebinds_refused`, `path_probes` and `path_candidate` (the newest address waiting for validation, or null). Steady
`rebinds_refused` growth with no real moves means someone is sending them.

A NAT rebinding costs about one round trip (ADR-8422). Any probe sent to a candidate validates it when answered (a resend does
not forget the earlier one), an address that has authenticated a record is opened without the per-address trial budget, and
while the validated path is silent and the candidate sends the peer's newest records, each data packet is also copied to the
candidate (copies and probes together at most three times what it sent). Copies never validate anything: only a probe number
sent to the candidate alone does. A move that changes only the port keeps the round-trip estimate. `path_mirrored` (totals and
rows) counts the copies; a lockstep seat whose NAT rebinds keeps receiving frames while its new port validates.

Limit: an attacker who can both read and forward traffic in both directions is a relay; it can carry the probe to the client and
the answer back, and the peer then moves to it, exactly as through a NAT. It still cannot read sealed traffic or join the session,
and when it stops relaying, the client's next record from its real address starts a validated move back.

## Resumption tickets and replay (wire 6, ADR-8858)

A resumption ticket ([net-link](net-link.md)) is an address token as well: it names the address block (one IPv4 address,
one IPv6 /64) the server had proved for its client. A stateless ticket still opens after it was used, so what makes it
single use is the peer's **epoch**: every accept mints the next epoch into its ticket, and the client's first
authenticated packet on a connection confirms that accept's epoch. From then on every older ticket is refused from every
address (`resume_epoch`); until then the older ticket still names the peer, which is the lost-accept rule of ADR-8535.
The server keeps two counters per peer for this and no table of tickets. The rules for a replayed first flight (nothing
is admitted before the client's first sealed record under the new keys; flights are ordered by the client's own count,
never by arrival; a byte-identical replay costs no curve work twice) are designed in ADR-8858 and built with the
server's admission, which this build does not have yet.

## Agent methods

- `net.known_hosts {}`. Read-only. Returns `{ path, hosts: [{ host, key, source }] }`, `source` is `first_use` or `manual`.
- `net.trust { host, key }`. Pins `key` (64 hex digits) for `host`, replacing an earlier key. Returns `{ host, key, previous, path }`.
  Mutating; permission `play`.
- `net.forget { host }`. Removes the host, so its next connection is a first use. Returns `{ host, forgotten, previous, path }`.
  Mutating; permission `play`.

The store is always `user://known_hosts.ron`; the methods take no path.

```json
{"method": "net.known_hosts", "params": {}}
{"method": "net.trust", "params": {"host": "203.0.113.7:7777", "key": "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"}}
{"method": "net.forget", "params": {"host": "203.0.113.7:7777"}}
```

| Code | Where | Meaning |
|---|---|---|
| `net_host_key_changed` | `LinkEvent::Ended`, `session.state` `detail` | The host proved a key that is not the one stored for it |
| `net_bad_host_id` | `net.trust`, `net.forget`, `NetLink::connect` | Empty, longer than 256 bytes, or with control characters |
| `net_bad_server_key` | `net.trust` | `key` is not 64 hex digits |
| `net_known_hosts_full` | `net.trust`, first use | The store holds 4096 hosts |
| `net_known_hosts_newer` | all | The file's `format_version` is newer than this build reads |
| `net_known_hosts_no_store` | all methods | The app has no `user://` directory |
| `net_known_hosts_unreadable` | all, `NetLink::connect`, `session.connect` | The file is damaged; it is left as it is |
| `net_known_hosts_write_failed` | `net.trust`, `net.forget` | The file could not be written |

## Check it yourself

```text
net.forget { "host": "127.0.0.1:7777" }
(connect a NetLink client with known_hosts set, encryption required)
net.known_hosts {}            -> hosts[0].source == "first_use", key == net.crypto server_key
(restart the server without its server.key, connect again)
expect LinkEvent::Ended reason == "net_host_key_changed"
net.trust { "host": "127.0.0.1:7777", "key": "<the new key>" }   -> the next connection succeeds
```

## Cost

Rekeying costs one 32-byte encryption per switch per direction; records keep their size (the epoch rides in the counter field's top
byte). A rebind attempt costs one decryption at most, eight per peer and address per second; a probe is one empty packet,
at most every 100 ms per candidate and never more than three times the bytes the candidate sent. The known-hosts store is read once per connect and
written once per first use.
