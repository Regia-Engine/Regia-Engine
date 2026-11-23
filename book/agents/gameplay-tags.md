# Gameplay tags

A gameplay tag is a hierarchical name such as `Combat.Damage.Fire`. A game gives an entity a set of tags
(`TagContainer`), asks questions of it (`TagQuery`), and lists the tags it uses in one file
(`game.tags.ron`) so that a typo is an error instead of a tag that never matches (ADR-1463). The code is
`regia_framework::tags`; add `TagPlugin` to an app to get the `TagRegistry` resource, the reflected
`TagContainer` component and the `TagsChanged` message.

## Names

A name is 1 to 8 segments joined by `.`. A segment is 1 to 48 bytes of `A-Z a-z 0-9 _ -`. A whole name is at
most 192 bytes. Names are case-sensitive. `Combat.Damage` is the parent of `Combat.Damage.Fire`; a shared
prefix without a dot (`Combat.DamageType`) is not a child.

A tag's id is the 64-bit FNV-1a hash of the name's UTF-8 bytes (offset basis `0xcbf29ce484222325`, prime
`0x100000001b3`), the same in every process and release. Ids are shown as 16 hex digits by `tags.list`.
Everything saved or sent to an agent uses the name, never the id. Two names with the same id are refused
(`Collision`); rename one.

## Matching

| Question | `Combat.Damage.Fire` held | `Combat.Damage` held |
|---|---|---|
| `Has("Combat.Damage")` | yes (a held child counts) | yes |
| `Has("Combat.Damage.Fire")` | yes | no (a held parent is not its child) |
| `HasExact("Combat.Damage")` | no | yes |

`AllOf`, `AnyOf` and `NoneOf` combine queries and nest (16 levels, 1024 nodes). With no children `AllOf` and
`NoneOf` are true and `AnyOf` is false.

## The tag table

```ron
(
    version: 1,
    description: "Combat tags",
    tags: [
        // Damage kinds.
        (name: "Combat.Damage.Fire", description: "Fire damage"),
        (name: "Status.Burning"),
    ],
)
```

Declaring a tag declares its parents (`Combat.Damage`, `Combat`) as implicit tags; declare a parent yourself
to give it a description. A name declared twice, an invalid name, or a newer `version` is an error. The
validator reports every problem at once.

## Methods

- `tags.list { prefix?, implicit? }` lists the declared tags: `name`, `id`, `depth`, `description`,
  `implicit`.
- `tags.get { entity }` returns `tags` (what the entity holds), `expanded` (plus implied parents) and
  `undeclared`. An entity without a container holds no tags.
- `tags.set { entity, tags, mode?, allow_undeclared?, dry_run? }` changes an entity's tags. `mode` is
  `replace` (default), `add`, `remove` (exactly those tags) or `remove_branch` (those tags and everything
  below). The container is written, and `TagsChanged` raised, only when the result differs from what the
  entity held; `applied` says whether it was. `dry_run` reports without writing.
- `tags.query { entity, query }` runs a query given in JSON: `{"Has": "Combat.Damage"}`,
  `{"HasExact": "..."}`, `{"AllOf": [..]}`, `{"AnyOf": [..]}`, `{"NoneOf": [..]}`. It returns `matches`,
  the entity's `tags` and any `undeclared` names the query mentions.
- `tags.validate {}` lists every tag held by a container in the world that the table does not declare.

```json
{ "method": "tags.set", "params": { "entity": 4294967296, "tags": ["Combat.Damage.Fire"], "mode": "add" } }
{ "method": "tags.query", "params": { "entity": 4294967296,
  "query": { "AllOf": [{ "Has": "Combat.Damage" }, { "NoneOf": [{ "Has": "Status.Immune" }] }] } } }
```

## Errors

Bad parameters answer `F0002` with a lowercase code in the message, in brackets:

| Code | Meaning |
|---|---|
| `bad_prefix` | `prefix` is not a string. |
| `bad_implicit` | `implicit` is not a boolean. |
| `bad_mode` | `mode` is not `replace`, `add`, `remove` or `remove_branch`. |
| `bad_tags` | `tags` is missing, not an array of strings, or holds more than 256 names. |
| `bad_tag_name` | A name breaks the naming rules (empty segment, bad character, too deep or long). |
| `undeclared_tag` | A valid name that the tag table does not declare (pass `allow_undeclared` to accept it). |
| `bad_allow_undeclared` | `allow_undeclared` is not a boolean. |
| `bad_dry_run` | `dry_run` is not a boolean. |
| `bad_query` | `query` is missing, not an object, malformed, or over the depth or node limit. |

`F0003` means the app has no `TagPlugin` (add `regia_framework::tags::TagPlugin`) or the entity does not
exist.

## In game code

```rust,wrap
use regia_framework::tags::{Tag, TagContainer, TagPlugin, TagQuery, TagsChanged};

// app.add_plugins(TagPlugin);
let mut held = TagContainer::from_names(["Combat.Damage.Fire"])?;
assert!(held.has(Tag::new("Combat.Damage")?));
assert!(!held.has_exact(Tag::new("Combat.Damage")?));
let immune = TagQuery::from_ron(r#"AnyOf([HasExact("Status.Immune"), Has("Armor.Fire")])"#)?;
held.matches(&immune);
```

`TagsChanged { entity, added, removed }` is a message raised in `PostUpdate` (set `TagsSet`) after a
container is edited. Read it with a `MessageReader`.

To check a tag table against the assets that use it, build a registry with `TagRegistry::load(path)` and
call `check_names`, `check_container` or `check_query`; each returns the undeclared or invalid names with the
source you gave. Tags counted by stack (two sources granting the same tag) are not modeled: a container is a
set.
