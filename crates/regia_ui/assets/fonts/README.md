# Bundled default fonts

The engine embeds a default font set so unpinned UI text renders Latin, Arabic,
CJK (incl. kana and Hangul) and colour emoji on **every** platform — Android included,
where neither fontdb nor cosmic-text loads any system font. `regia_ui::text` seeds
cosmic-text's per-script fallback chain with these families
(`new_with_locale_and_db_and_fallback`), and pins the sans-serif default to Inter.

All fonts are SIL Open Font License 1.1; licence texts sit next to this file.
Regenerate the subsets with `python3 subset.py` (see its docstring for sources).

| File | Family | Licence | Source | Size |
|---|---|---|---|---|
| `Inter-Regular/Bold/Italic.ttf` | Inter | OFL-1.1 | google/fonts `ofl/inter` | 412/420/417 KiB (as-is) |
| `NotoNaskhArabic-400-subset.ttf` | Noto Naskh Arabic | OFL-1.1 | google/fonts `ofl/notonaskharabic`, instanced to wght=400, full Arabic block incl. joining GSUB | 142 KiB |
| `NotoSansSC-400-subset.ttf` | Noto Sans SC | OFL-1.1 | google/fonts `ofl/notosanssc`, instanced to wght=400 (its VF default is Thin/100 — the M7 round-4 "Thin" subset was the un-instanced default), subset: Jun Da top-2000 han (97.1 % of running text) + full kana + CJK punctuation + fullwidth forms + gallery strings | 827 KiB |
| `NotoSansKR-400-subset.ttf` | Noto Sans KR | OFL-1.1 | google/fonts `ofl/notosanskr`, instanced to wght=400, subset: all 2350 KS X 1001:1998 Hangul syllables + compatibility jamo | 429 KiB |
| `NotoSansHebrew-400-subset.ttf` | Noto Sans Hebrew | OFL-1.1 | google/fonts `ofl/notosanshebrew`, instanced to wght=400 wdth=100, subset: Hebrew block + presentation forms, all layout tables kept (`subset_indic.py`, ADR-1842) | 16 KiB |
| `NotoSansDevanagari-400-subset.ttf` | Noto Sans Devanagari | OFL-1.1 | google/fonts `ofl/notosansdevanagari`, same instancing, subset: Devanagari + extended + Vedic marks, GSUB/GPOS kept for conjuncts and marks (`subset_indic.py`) | 162 KiB |
| `NotoSansThai-400-subset.ttf` | Noto Sans Thai | OFL-1.1 | google/fonts `ofl/notosansthai`, same instancing, subset: Thai block, GSUB/GPOS kept for mark stacking (`subset_indic.py`) | 20 KiB |
| `NotoColorEmoji-subset.ttf` | Noto Color Emoji | OFL-1.1 | `googlefonts/noto-emoji` CBDT bitmap font (e.g. `/usr/share/fonts/noto/NotoColorEmoji.ttf`), subset: ~138 common UI emoji + regional indicators for common flags (GSUB ligatures) | 655 KiB |

Added subset bytes (before the Hebrew, Devanagari and Thai rows): **2 052 KiB** (the engine already shipped 1 159 KiB of Inter).
Not covered by the default set (projects bundle their own): other Indic scripts, rare han,
uncommon emoji. `specs/18` § 2 lists those as engine
goals for later tiers.

## Why subsetting must keep GSUB/GPOS

`subset.py` runs pyftsubset with `--layout-features='*'`: Arabic contextual joining
(init/medi/fina/isol) and emoji flag ligatures (regional-indicator pairs) live in GSUB,
and dropping the tables would break shaping while the cmap still resolves — glyph
soup instead of tofu, worse to debug. `tests/font_coverage.rs` in `samples/ui_gallery`
guards all of this: it shapes every gallery sample and localized string with the
embedded set only and fails on any `.notdef`.
