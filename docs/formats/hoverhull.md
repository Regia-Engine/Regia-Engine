# .hoverhull.ron: hover hull v1

Library: `regia_framework::det_hover` (`HoverHullAsset::{from_ron, to_ron, validate, into_parts}`,
`migrate_hoverhull`).
Check without stepping: agent method `detsim.hover_hull_check`. Guide page: `book/agents/det-hover.md`.
Decision: ADR-8512 (handshake bytes: ADR-8632).

A `.hoverhull.ron` holds the tuning of one hover hull: a display name, the `HoverParams` numbers, an
optional `HoverLimits` block, an optional flight-mode name and an optional note. The text is parsed
on the host. A lockstep match does not compare the text. It compares the hull handshake,
`HoverHullParts::handshake_bytes` (built by `regia_detsim::hover::hull_handshake_bytes`, ADR-8632):
273 bytes, everything the hull step reads from the file.

| Bytes | Content |
|---|---|
| 0..180 | `params`: `HoverParams::to_le_bytes`, four little-endian raw `f32` bits per `HoverParams::FIELD_RANGES` field, in that table's order |
| 180 | `flight`: 0 for `air_time`, 1 for `clamped` |
| 181..273 | `limits`: four little-endian raw `f32` bits per `HoverLimits::FIELD_RANGES` field, in that table's order |

An omitted `limits` block or `flight` is filled with the limits and mode the host steps before the
bytes are written, so the bytes are what the match steps. `-0.0` stays `-0.0`. Because every number
and the mode the step reads are in those bytes, a float token that two hosts parse differently, or
a peer whose file has other limits or another flight mode, fails the handshake before the first
tick (`hull_handshake_mismatch` names the first field that differs) and cannot desync. A host that
steps its own `ClimbTuning` (not a `FlightMode::climb`) or a non-default world compares those
itself (`WorldPhysics::to_le_bytes` for the world). A format built on a hull appends its own bytes
after these 273 instead of writing a second digest. The file's v1 shape is unchanged; the
handshake is computed on the host after the parse.

Every `params` and `limits` field is optional and defaults to `HoverParams::DEFAULT_HULL` or
`HoverLimits::DEFAULT`, not `0.0`. An omitted `limits` block means the host keeps its own limits.
The loader accepts named fields only and turns on RON `implicit_some`, so `ride_height: 1.5` and
`Some(1.5)` are the same, and a `limits` block may be `(...)` or `Some((...))`. A file may also
start with `#![enable(implicit_some)]`. A positional tuple is `parse_error`.

```ron
(
    format_version: 1,
    name: "scout",
    params: (ride_height: 1.5),
    limits: Some((hop_apex_m: 2.0)),
    flight: Some("clamped"),
    notes: Some("artist note"),
)
```

| Field | Meaning |
|---|---|
| `format_version` | `1`. Any other value fails with `bad_version`. There is no older layout; `migrate_hoverhull` rewrites v1 in canonical form. |
| `name` | Display name. Empty or all whitespace fails with `bad_name`. |
| `params` | `HoverParams` fields. A missing field keeps `DEFAULT_HULL`. A bad number fails with that field's code (`not_finite`, `negative`, `zero_rate`, `out_of_range`, `inverted_range`) and names the field. |
| `limits` | `None` (omit the block) or a `HoverLimits` block. A missing field inside the block keeps `HoverLimits::DEFAULT`. A bad number names `limits.<field>`. |
| `flight` | `None`, or `FlightMode::from_name`: `"air_time"` or `"clamped"`. Anything else is `bad_flight`. |
| `notes` | Free text a tool may show. The step does not read it. |

`regia upgrade-project` knows the suffix `.hoverhull.ron` and leaves a v1 file as it is.

`detsim.hover_hull_check` takes exactly one of `text` (at most 1 MiB) or `path` (a project file, at
most 1 MiB). Success carries the resolved hull, `handshake_hex` (the 273 handshake bytes, with an
omitted `limits` block or `flight` filled with `HoverLimits::DEFAULT` and `air_time`, the engine
defaults) and `bytes_hex` (the 180 `params` bytes only). Compare `handshake_hex`; `bytes_hex`
alone cannot see `limits` or `flight`. JSON numbers are not the handshake: JSON cannot keep `-0.0`.
The call's JSON-RPC code stays `F0002`; `format_code` is one of `parse_error`, `bad_version`, `bad_name`, `bad_flight`, `not_finite`, `negative`,
`zero_rate`, `out_of_range`, `inverted_range`.
