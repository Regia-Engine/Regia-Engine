# `manifest-<platform>.json.sig` (release manifest signature, format 1)

Owner: `regia_release_chunks` (ADR-8687; registered as `release-signature` by ADR-8846). One
JSON document next to each release manifest. `regia release sign <dir> --key <path> --date
YYYY-MM-DD` writes it; `regia release verify` checks it, and requires it with
`--require-signature`. `regia upgrade-project` lists files ending `.json.sig` that read as this
format and leaves v1 in place; a `.json.sig` file of another tool is skipped.

Example (`docs/formats/release-signature.example.json`: the writer's output for the manifest
`{"format":1,"platform":"demo","version":"1.0.0","chunks":[],"links":[]}` named
`manifest-demo.json`, the date `2026-11-18` and the throwaway Ed25519 seed of 32 bytes of 7;
never a real key):

```json
{
  "format": 1,
  "algorithm": "ed25519",
  "key_id": "0871f3aabc26e458",
  "public_key": "ed25519:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
  "date": "2026-11-18",
  "manifest_sha256": "ea64e8da8d01443a89c41ce240e0e03fb0072b50d60b9e92fce3820507ef0aef",
  "signature": "80d137a76ac8dbd6b3feffea20d62f5d01e4c3ccbbfde049453172b39e0adfd0f862720c11710191390978eaba2920739544ba1a540d420d787f07150c84730d"
}
```

| Field | Meaning |
|---|---|
| `format` | 1. A newer document is refused with `release_signature_newer` by the canonical reader; `regia release verify` reports `manifest_signature`. |
| `algorithm` | `ed25519` (RFC 8032). Anything else is `manifest_signature`. |
| `key_id` | Lowercase hex of the first 8 bytes of BLAKE3 of the 32-byte public key. |
| `public_key` | `ed25519:` and 64 lowercase hex digits. |
| `date` | `YYYY-MM-DD`, an argument of `regia release sign`; no clock is read. |
| `manifest_sha256` | SHA-256 of the manifest bytes, 64 lowercase hex digits. |
| `signature` | 128 lowercase hex digits: Ed25519 over `regia-release-manifest-v1\0` followed by `file <name>\nsha256 <hex>\ndate <YYYY-MM-DD>\n`. |

Unknown fields are refused. Files are at most 8192 bytes. The writer emits pretty JSON (two-space
indent), fields in the order above and a final newline; `canonical_signature_json` writes that
form back. The schema fingerprint `cargo xtask check-formats` hashes is
`format,algorithm,key_id,public_key,date,manifest_sha256,signature`.

Frozen: a field change bumps `format`, with a migrator, an `upgrade-project` row, a corpus file
(`crates/regia_agent/tests/fixtures/format-corpus/release-signature/`), the fuzz tests of the
reader (`crates/regia_release_chunks/tests/fuzz.rs`, named by the fuzz-target registry's
`release-signature` row; ADR-10356) and the `format-hashes.ron` row.
