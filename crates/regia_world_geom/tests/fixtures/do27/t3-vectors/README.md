# T3 reference vectors (data)

Portions copied from Direct Order 27 (spec/maps/t3-vectors/, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
Mark: as-is (data; byte-identical to the tag's blobs). Read by `tests/t3_vectors.rs`.

| File | Source | Blob | SHA-256 |
|---|---|---|---|
| `switchbacks_mesh.json` | spec/maps/t3-vectors/switchbacks_mesh.json | b2f515be | 10b8c8c276dab1d1dc762f20f7802ddcb161abd6ca1eec89296ca974e3ca606e |
| `cinder_steps_mesh.json` | spec/maps/t3-vectors/cinder_steps_mesh.json | c638da26 | debdee24cff297f5cdbe89178556ee97bcc132ab2d9f35dd4e62657a909780f6 |
| `four_decks_mesh.json` | spec/maps/t3-vectors/four_decks_mesh.json | 355fb857 | 936ec2cec6268808756c8adbdf645cf1e1a51c72ed8c2832ee1e71e2c113c188 |
| `switchbacks_canyon.json` | spec/maps/t3-vectors/switchbacks_canyon.json | 1ca7d4f8 | 7417c5b12cc5bfa6a396277f5f1b006cd7f1cf57efe132a22d6b17b7e9ad1b25 |

Format `do27-t3-vectors/1` (a foreign format name, kept as data): the input mesh in integer millimetres (the
mesh is built from `(mm as f32) / 1000.0`) and, per query XZ, every free span bottom up and `at` rows (the
index of the span `Column::span_at(y)` answers). The `source` strings inside the files name the source's
module paths; they are data, not instructions. A changed vector is a sim change: these files are never
refreshed here, only re-copied from a later tag.
