# T3 edge-vector replacement (data)

Portions copied from Direct Order 27 (spec/maps/t3-vectors/cinder_steps_mesh.json, commit c06c17084), ADR-8012.
Mark: adapt (data, ADR-8246). A JSON file cannot hold this header, so it lives here, as for
`../../do27/t3-vectors/README.md`. Read by `tests/native_edge_vectors.rs`.

| File | Source | SHA-256 |
|---|---|---|
| `cinder_steps_mesh.json` | spec/maps/t3-vectors/cinder_steps_mesh.json (lines 1-4028) | 571116e2435474305e7345adce198791ab4c4cd4ad9912dc8c94cca3ed51c818 |

The triangles are unchanged integer-mm inputs. One column (original column 602, an exact shared edge) is
replaced by Regia's canonical-ownership answer; every height there changes by at most 1 mm. The file
records the change in `regia_changes` and carries `"provenance": "regia"`. The original vector file is
kept unchanged under `../../do27/t3-vectors/`.
