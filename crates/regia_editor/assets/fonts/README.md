# Editor fonts (desktop editor only)

The PiercingXX brand faces the editor themes use (`regia_editor::theme`, BRAND-GUIDE section 4,
ADR-8585). They are compiled into the editor with `include_bytes!`; nothing outside
`regia_editor` reads them, so no runtime crate or game build carries them.

| File | Face | Version | Upstream file | sha256 | Licence |
|---|---|---|---|---|---|
| `JetBrainsMono-wght.ttf` | JetBrains Mono, variable (wght 100-800, default 400) | 2.304 | `JetBrainsMono[wght].ttf` | 662a196d58f1183bf2d77428b6d5283fe3f45161ab021bea4036bc98e5cac016 | OFL-1.1 (`OFL-JetBrainsMono.txt`) |
| `SpaceMono-Regular.ttf` | Space Mono Regular | 1.003 | `SpaceMono-Regular.ttf` | 95837e182baeeada83368f7748db28357f0a1b75c6b84ff7065b5edf933c8e18 | OFL-1.1 (`OFL-SpaceMono.txt`) |
| `Geist-wght.ttf` | Geist, variable (wght 100-900, default 400) | 1.900 | `Geist[wght].ttf` | b34955a09f71d9ac4b67e54cedb1825d3d5264c7992dbbafa0802080d617e241 | OFL-1.1 (`OFL-Geist.txt`) |

Copied unmodified from the owner's brand repository (`piercingxx-branding/ui/fonts-src`, whose
`SOURCES.md` records the upstream releases: JetBrains/JetBrainsMono v2.304, vercel/geist-font
v1.9.2, googlefonts/spacemono); the sha256 values above match that file. The square brackets of the
upstream variable-font names are replaced with `-wght` so the paths need no shell quoting. All three
are SIL Open Font License 1.1 with no Reserved Font Name. The editor renders the variable faces at
their default instance, weight 400 (the brand's regular weight).
