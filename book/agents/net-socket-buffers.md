# UDP socket buffers (agents)

`net.socket_buffers` binds a new `127.0.0.1:0` UDP socket, asks the kernel for receive and send
buffer sizes, and optionally marks packets with DSCP EF (ADR-7998). The call then drops that
socket. It does not read or change the live game socket.

| Method | Params | Result |
|---|---|---|
| `net.socket_buffers` | `recv_bytes`, `send_bytes`, `dscp_ef` | `{ requested_recv, requested_send, confirmed_recv, confirmed_send, meets_request, below_half, dscp_ef_applied, read_back, dont_fragment, local_addr }` |

- `recv_bytes` is the `SO_RCVBUF` request in bytes. Optional. Default `2097152` (2 MiB). Allowed range `1..=67108864`.
- `send_bytes` is the `SO_SNDBUF` request in bytes. Optional. Default `524288` (512 KiB). Same range.
- `dscp_ef` is a boolean. Optional. Default `true`. When true, the socket sets the DSCP EF mark `0xB8` (codepoint 46, RFC 3246 and RFC 2474) on the bound family: `IP_TOS` for IPv4, `IPV6_TCLASS` for IPv6. A stack that refuses the mark leaves the socket up.
- `requested_recv` and `requested_send` are the sizes this call asked for.
- `confirmed_recv` and `confirmed_send` are what `getsockopt` reported. On Linux the kernel returns twice the granted size, clamped by `rmem_max` and `wmem_max`. They are `0` when the read-back failed.
- `meets_request` is true when both confirmed sizes are at least the request.
- `below_half` is true when either confirmed size is strictly below half the request (integer division). That is a warning on the socket, not a failed call.
- `dscp_ef_applied` is true when the bound family accepted the mark and the read-back is `0xB8`, or the set succeeded and the read-back itself failed.
- `read_back` is true when both buffer reads succeeded.
- `dont_fragment` is true when the socket's packets carry don't-fragment (ADR-8893): the platform option was set and reads back on. Linux and Android use `IP_MTU_DISCOVER` = `IP_PMTUDISC_PROBE` (`IPV6_MTU_DISCOVER` on IPv6), Windows `IP_DONTFRAGMENT` (`IPV6_DONTFRAG`), macOS `IP_DONTFRAG` (`IPV6_DONTFRAG`). Every `UdpTransport` sets it, so a datagram larger than the path is refused (`EMSGSIZE`) or dropped by a router instead of being fragmented, and a path MTU probe that is acknowledged was carried whole. `false` means the platform refused the option: IPv4 datagrams may then be fragmented and the probed path MTU can read high.
- `local_addr` is the loopback address the OS assigned, for example `127.0.0.1:41234`.

`{}` and JSON `null` use the defaults. `UdpTransport::bind` on a game socket uses the same defaults. Pass `bind_with` or `from_socket_with` from Rust when a game wants other sizes. `from_socket` overwrites options already set on the socket.

A 2 MiB request is often clamped on Linux, because `rmem_max` is commonly about 208 KiB. The doubled grant can then be below half of 2 MiB. The socket still binds, and the process logs the diagnostic line at warn. Raising `rmem_max` and `wmem_max`, or requesting a smaller size, clears the warning.

## Errors

| Code | When |
|---|---|
| `socket_buffers_bad_params` | `net.socket_buffers` was called with a non-object, an unknown field, a size that is not an integer, a size outside `1..=67108864`, or a `dscp_ef` that is not a boolean. The dispatcher may answer `F0002` first for a wrong JSON type or a size outside the declared range. |
| `socket_buffers_bind` | The loopback socket could not bind `127.0.0.1:0`, or the bound socket has no local address. A refused buffer or DSCP sockopt is not this error. |

Neither code panics. A refused sockopt is reported in `dscp_ef_applied`, `read_back`, `meets_request` and `below_half`.
