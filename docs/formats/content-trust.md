# Content trust record (`content-trust.ron`, format `content-trust` v1)

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md` § 11. Decision record: ADR-2710. Guide:
`book/agents/content-trust.md`.

Written by `regia cook` / `regia package` next to `content-manifest.ron` when `Regia.toml` lists
`[content] trusted_keys`, removed when it does not, and staged into the package's `content/` (the
Android package extracts it with the manifest). A running game reads it at startup: with it, a manifest
fetched from a host is adopted only after its detached signature (`content-manifest.ron.sig`,
`docs/formats/content-signature.md`) verifies under one of these keys.

```ron
(
    format_version: 1,
    policy: Required,
    keys: [
        "ed25519:3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
    ],
)
```

- `policy`: `Required` (a manifest without a signature is refused with `content_signature_missing`) or
  `Optional` (it is accepted and reported `unsigned`; a signature that is present must still verify).
- `keys`: one or two Ed25519 public keys, `ed25519:` and 64 hex digits (what `regia assets keygen`
  prints). Two keys let a studio rotate: ship a build trusting the old and the new key, then sign with the
  new one.

Version rules: a shape change bumps `format_version`. The file is derived data: a recook writes the
current version. A newer, broken or invalid file (no key, three keys, a key that does not parse) is
logged and makes the game refuse every fetched manifest (`content_signature_invalid`): a game must never
fall back to trusting unsigned content because its trust record did not read. No migrator is needed.
