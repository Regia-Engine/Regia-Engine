# QUIC datagrams (agents)

`net.quic_status` reports the optional QUIC datagram socket (ADR-3970). The socket carries RFC 9221
unreliable datagrams on one `127.0.0.1` connection. Regia's reliable channel, acks and replication stay
in `regia_net`; this socket does not replace them. The Cargo feature `net-quic` is off unless the crate
is built with it.

| Method | Params | Result |
|---|---|---|
| `net.quic_status` | — | `{ enabled, bound, datagrams_sent, datagrams_lost, last_error }` |

- `enabled` is `false` when this build has no `net-quic` feature. The other fields are then `false`, `0`, `0` and `null`.
- `bound` is `true` while a QUIC datagram socket is up.
- `datagrams_sent` counts datagrams the connection accepted. A datagram this socket drops on purpose is not counted here.
- `datagrams_lost` counts local drops and datagrams QUIC refused, including injected drops and short receive buffers.
- `last_error` is the newest of `quic_bind`, `quic_handshake` and `quic_closed`, or `null`.

Listeners and clients bind only `127.0.0.1:0`; the OS chooses each local port. A client dials the listener's assigned loopback port.

The counters add up every socket in the process. A host that wants one socket reads that socket's own snapshot.

With the feature off, the response is:

```json
{ "enabled": false, "bound": false, "datagrams_sent": 0, "datagrams_lost": 0, "last_error": null }
```

## Errors

| Code | When |
|---|---|
| `quic_bad_params` | `net.quic_status` was called with anything other than `{}` or JSON `null`. |
| `quic_bind` | The bind address was not `127.0.0.1:0`, the dial address was not `127.0.0.1` with a nonzero port, the socket could not bind, the payload was larger than 2048 bytes or over the QUIC datagram limit (the connection's current payload limit; the connection stays up), the peer or this endpoint has QUIC datagrams off, the receive buffer was too short, or the destination was not the connected peer. |
| `quic_handshake` | The QUIC handshake failed, including a certificate other than the one pinned for that connection. A handshake that gets no response uses this code when the idle timer fires. |
| `quic_closed` | The connection has stopped taking datagrams. |

`quic_bind`, `quic_handshake` and `quic_closed` are returned by the socket as I/O errors whose text starts with the code. `quic_bad_params` is the agent method only. None of them panics.

See [Networking (agents)](net.md) for the session that runs on top of the socket.

## Path and receive edges (ADR-7946)

QUIC starts at a 1200-byte path MTU and probes for larger paths. Endpoint packets
reserve at most 1156 payload bytes (less when the peer allows fewer), leaving room
for QUIC framing even if discovery falls back to 1200. A refused over-limit send
counts in `datagrams_lost`; a smaller retry remains possible. The connection layer
shrinks uncommitted work without advancing its packet sequence. Reliable fragment
IDs already published and encryption nonce counters are never rewound.

An oversized receive increments `datagrams_lost` and sets `last_error` to
`quic_bind`, then continues draining. It does not abort the endpoint receive loop.
A socket call yields after 1024 oversized datagrams to bound hostile work.

Verify with `cargo test -p regia_net --features net-quic --lib quic_edges` and
`cargo test -p regia_net --test handshake_quic_edges`. These use deterministic
payload queues and an in-memory socket whose limit changes at a chosen send;
there are no sleeps or wall-clock assertions. Read `net.quic_status` before and
after an oversized receive: `bound` stays true, `datagrams_lost` increases, and
later valid datagrams still reach the session. The agent response shape and
error-code vocabulary are unchanged.
