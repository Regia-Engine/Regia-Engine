# .tags.ron: gameplay tag table (v1)

Spec: ADR-1463 (`docs/decisions.md`), `docs/specs/12-assets.md` § 4a (versioning).
Code: `regia_framework::tags` (`TagTable`, `TagRegistry`). Example: [`tags.example.ron`](tags.example.ron).
Guide: `book/agents/gameplay-tags.md`.

A `.tags.ron` lists the hierarchical gameplay tags a game uses. Declaring `Combat.Damage.Fire` also
declares its parents `Combat.Damage` and `Combat` (implicit tags). Version **1**; a newer version is
rejected before the body is parsed (v1 has no migration steps). The shape (this table plus the JSON/RON form
of `TagQuery`) is hashed by `cargo xtask check-formats`. `//` comments are read and dropped on save.

## Schema

```
TagTable {
    version: u32,                    // 1
    description: String = "",
    tags: [TagDecl],
}
TagDecl {
    name: String,                    // 1..=8 segments of [A-Za-z0-9_-], 1..=48 bytes each, whole name <= 192 bytes
    description: String = "",
}
TagQuery =                           // embedded in assets that match tags; tags are name strings
    Has(String) | HasExact(String)
  | AllOf([TagQuery]) | AnyOf([TagQuery]) | NoneOf([TagQuery])    // depth <= 16, nodes <= 1024
```

A tag id is the 64-bit FNV-1a hash of the name's UTF-8 bytes. Ids never appear in files.

## Validation

An invalid name, a duplicate name, two names with the same id, or a newer version is an error; a table with
problems builds no registry (`TagTable::problems` lists them all). A tag used in an asset that the table does
not declare is reported by `TagRegistry::check_names`, `check_container` and `check_query`.
