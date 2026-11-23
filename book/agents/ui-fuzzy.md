# UI fuzzy search (agents)

ADR-273 W17, ADR-1842. Code: `regia_ui::fuzzy`.

`fuzzy_match(query, candidate)` and `fuzzy_rank(query, candidates, limit)` rank labels for palettes and pickers. Every
query character must appear in the candidate in order; case and whitespace in the query are ignored. The score favors a
match at the start of the candidate, at word starts (after a separator or a lowercase-to-uppercase step) and runs of
consecutive characters; skipped characters cost a little. When the query, with its whitespace removed, has the same number of characters as the candidate, the score gets a bonus.
Ties keep the input order, so the same input always gives the same ranking. Positions are character indexes, for
highlighting. Scoring one label is linear in its length. A positive `limit` keeps that many best rows and does not
sort the rest. A search box reranks when its query, item list, or limit changes, not while the caret blinks.

## Methods

- `ui.fuzzy { query, items: [string], limit? }` ranks `items` (at most 10000) and returns
  `{ query, count, matches: [{ index, item, score, positions }] }`, best first. `limit` defaults to 20; `0` returns all.
  Not mutating; permission `read`.

```json
{"method": "ui.fuzzy", "params": {"query": "fb", "items": ["fabric", "foo_bar"]}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_query` | `query` is missing or not a string |
| `bad_items` | `items` is not an array of strings, or has more than 10000 entries |
| `bad_limit` | `limit` is not a whole number from 0 to 10000 |

`query` is a required string; `items` is a required array of strings (maximum 10,000). `limit` is an optional integer
from 0 through 10,000, defaults to 20, and `0` returns every match.

## Failure example

Captured CLI output from `regia agent call ui.fuzzy '{"query":"x","items":[],"limit":-1}'`:

```text
error: method call failed: `limit` must be a whole number from 0 to 10000 (-32602): {"code":"bad_limit","hint":"0 returns every match"}
```

## Workflow

Pass the visible labels as `items` with the player's text as `query`. Use each result's `index` to select the original
item and `positions` to highlight matched characters; increase `limit` or set it to `0` when the caller needs more
results.
