# glossary: localization glossary v1

Files: `assets/localization/glossary.locglossary.ron`.
Library: `regia_ui::loc_glossary` (`parse_glossary`, `glossary_findings`, `build_memory`,
`apply_memory`). Commands: `regia loc validate`, `regia loc export`. Decision: ADR-3359.
Spec: `docs/specs/36-localization-exchange-dialogue-subtitles.md` § 3.

The glossary lists product terms and their approved translation per locale. `regia loc validate`
warns (`loc_glossary_mismatch`) when a message uses a term but its translation in that locale
does not use the approved translation. The translation memory is derived from the project's own
translations: on export, an untranslated entry whose source pattern was translated before is
offered as a note, and an exact match pre-fills the translation with the `regia-review` flag.

```ron
(
    format_version: 1,
    terms: [
        (term: "Save", translations: { "de": "Speichern", "fr": "Enregistrer" }, note: "verb, not noun"),
        (term: "quick save", translations: { "de": "Schnellspeicherung" }),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. A newer version fails with a version error. |
| `terms` | The term list, in file order. |
| `term` | The source-language term; non-empty. |
| `translations` | The approved translation per locale tag (`"de"`, `"fr-CA"`); a lookup falls back to the tag's language part. Values are non-empty. |
| `note` | Optional note for translators. |

Term matching is case-insensitive on word boundaries ("save" matches "Save" and "quick save",
not "saved"), so a glossary warning means the terminology actually differs, not that the
inflected form differs.

Migration: none at v1. The file is optional; a project without it gets no glossary warnings
and exports without translation-memory notes.
