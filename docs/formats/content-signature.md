# Content manifest signature (`content-manifest.ron.sig`, format `content-signature` v1)

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md` § 11. Decision record: ADR-2710. Guide:
`book/agents/content-trust.md`.

A detached Ed25519 signature (RFC 8032) of a hosted content manifest, written by `regia assets sign`
next to the manifest and uploaded with it: `<base_url>/<build_id>/content-manifest.ron.sig`. The
manifest format is unchanged.

```ron
(
    format_version: 1,
    algorithm: "ed25519",
    key_id: "9f2c41d07ab3e55c",
    signature: "<128 lowercase hex digits>",
)
```

- The signed message is the ASCII bytes `regia-content-manifest-v1` and one zero byte, followed by the
  manifest file's exact bytes as served. The prefix keeps a manifest signature from ever verifying as a
  signature over anything else.
- `key_id` is the first 8 bytes of the BLAKE3 hash of the 32-byte public key, in hex. It picks the
  trusted key; it is not trusted by itself (the signature is checked under that key).
- At most 4096 bytes are read. Any other `format_version`, algorithm, key id or signature shape is
  `content_signature_invalid`.

Version rules: a shape change bumps `format_version`. Signatures are made per upload, so there is no
migrator: re-sign with the current `regia`.
