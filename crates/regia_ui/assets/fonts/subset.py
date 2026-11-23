#!/usr/bin/env python3
"""Regenerate the engine's bundled default font subsets (M7 UI round 5).

The engine embeds one subset per script family so the default font set covers
Latin, Arabic, CJK (incl. kana and Hangul) and emoji on every platform —
including Android, where neither fontdb nor cosmic-text loads any system font
(`regia_ui::text` installs the bundled families into the fallback chain).

Sources (all SIL Open Font License 1.1; licence texts sit next to this script):

  Family              Source
  ------------------  --------------------------------------------------------
  Inter               https://github.com/google/fonts/tree/main/ofl/inter
                      (already shipped as-is, not subsetting here)
  Noto Sans SC        https://github.com/google/fonts/tree/main/ofl/notosanssc
  Noto Sans KR        https://github.com/google/fonts/tree/main/ofl/notosanskr
  Noto Naskh Arabic   https://github.com/google/fonts/tree/main/ofl/notonaskharabic
  Noto Color Emoji    https://github.com/googlefonts/noto-emoji (the CBDT/CBLC
                      bitmap font; the repo no longer commits binaries — any
                      NotoColorEmoji.ttf works, e.g. /usr/share/fonts/noto/)

Charsets:
  - Han: the top 2000 of Jun Da's "Modern Chinese character frequency list"
    (lingua.mtsu.edu/chinese-computing/statistics/char; ~97.1 % of modern
    running text), plus Japanese shinjitai variants carried over from the
    previous gallery subset, plus every gallery/localization character.
  - Kana: the full hiragana/katakana blocks + CJK punctuation + fullwidth
    forms (Noto Sans SC carries all of them).
  - Hangul: the 2350 precomposed syllables of KS X 1001:1998 (the Wansung
    repertoire), generated directly from the EUC-KR encoding, plus the
    Hangul compatibility jamo block.
  - Arabic: the full Arabic block and neighbours as far as the font covers.
  - Emoji: a curated common-UI set plus the regional indicators for common
    flags (GSUB ligatures produce the flag glyphs).

Usage:
    python3 subset.py --sources DIR --out DIR
    # DIR must contain (download first, see the URLs above):
    #   NotoSansSC-VF.ttf, NotoSansKR-VF.ttf, NotoNaskhArabic-VF.ttf,
    #   CharFreq.csv (Jun Da modern list, ?Which=MO), NotoColorEmoji.ttf
    #   (or pass --emoji PATH, or put one in /usr/share/fonts/noto/)

Requires Python 3.9+ and fonttools >= 4.38 (pyftsubset's library API).
Run once at maintenance time; the generated .ttf files are committed.
"""

from __future__ import annotations

import argparse
import os
import sys
from fontTools import ttLib
from fontTools.subset import Options, Subsetter, load_font, save_font
from fontTools.varLib.instancer import instantiateVariableFont

# --- Must-cover strings (gallery samples + localized UI strings) -------------
# The real guard is the engine test `samples/ui_gallery/tests/font_coverage.rs`
# (it shapes the live gallery strings and asserts no glyph resolves to .notdef);
# this list makes the generator fail loudly when a needed character is missing.
GALLERY_STRINGS = [
    "The quick brown fox jumps over the lazy dog.",
    "مرحبا بالعالم — النص العربي",
    "مرحبا بالعالم — النص العربي مع التشكيل.",
    "你好世界 — 日本語のかなと漢字",
    "你好世界 — 日本語のかなと漢字、한국어.",
    "Regia Engine UI 🚀🎮✨",
    "Regia Engine UI 🚀🎮✨ — flags 🇺🇸🇯🇵",
    # en/ar/zh localization files (samples/ui_gallery/assets/localization)
    "UI Gallery Theme Language Widgets Text (RTL / CJK / emoji) Virtualised list"
    " Start Toggle theme Cycle language Volume Loading Name Music Item",
    "معرض الواجهة السمة اللغة العناصر نص (عربي / صيني / إيموجي) قائمة افتراضية"
    " ابدأ تبديل السمة تبديل اللغة الصوت التحميل الاسم الموسيقى عنصر",
    "界面图库 主题 语言 控件 文本（RTL / 中文 / 表情） 虚拟列表 开始 切换主题 切换语言"
    " 音量 加载中 名称 音乐 项目",
]

# Japanese shinjitai / legacy variants kept from the previous gallery subset
# (they are not in the simplified-Chinese frequency list).
VARIANT_SUPPLEMENT = "漢語儲動戻搖擬桿條標測滾籤終虛設試載進鈕開頁項體"

# Curated common-UI emoji (single codepoints; regional indicators appended).
EMOJI = [
    0x1F680, 0x1F3AE, 0x2728, 0x1F980,                      # gallery + tests
    0x2B50, 0x2705, 0x274C, 0x2714, 0x2716, 0x2753, 0x2757,  # stars / checks
    0x2764, 0x1F494, 0x1F49D,                               # hearts
    0x26A0, 0x2699, 0x2708, 0x270C, 0x2744, 0x27A1, 0x2B06, 0x2B07, 0x2B05,
    0x231A, 0x23F0, 0x23F3, 0x25B6, 0x23E9, 0x303D,
    0x26BD, 0x26BE, 0x26C5, 0x2600, 0x2615, 0x26F5, 0x26FD, 0x26EA,
    0x1F600, 0x1F601, 0x1F602, 0x1F603, 0x1F604, 0x1F605, 0x1F609, 0x1F60A,
    0x1F60D, 0x1F612, 0x1F613, 0x1F618, 0x1F61C, 0x1F621, 0x1F622, 0x1F625,
    0x1F628, 0x1F62D, 0x1F631, 0x1F633, 0x1F637, 0x1F642, 0x1F644, 0x1F64C,
    0x1F64F,
    0x1F442, 0x1F44B, 0x1F44C, 0x1F44D, 0x1F44E, 0x1F44F, 0x1F450, 0x1F46B,
    0x1F48B, 0x1F64A, 0x1F648, 0x1F476,
    0x1F493, 0x1F495, 0x1F496, 0x1F4A1, 0x1F4AA, 0x1F4AF,
    0x1F3A7, 0x1F3B1, 0x1F3BC, 0x1F3C0, 0x1F3C6, 0x1F3AF, 0x1F389, 0x1F3E2,
    0x1F3ED, 0x1F3F3, 0x1F308,
    0x1F4B0, 0x1F4B5, 0x1F4BB, 0x1F4F1, 0x1F50D, 0x1F510, 0x1F511, 0x1F512,
    0x1F513, 0x1F514, 0x1F516, 0x1F525, 0x1F527, 0x1F528, 0x1F531, 0x1F534,
    0x1F535, 0x1F53A, 0x1F53B, 0x1F5D1, 0x1F5DE, 0x1F5E1, 0x1F5E8, 0x1F5FA,
    0x1F4C1, 0x1F4C4, 0x1F4D6, 0x1F4DD, 0x1F4E6, 0x1F4E7, 0x1F4EA, 0x1F4F9,
    0x1F4FB,
    0x1F337, 0x1F338, 0x1F339, 0x1F340, 0x1F331, 0x1F34E, 0x1F355, 0x1F354,
    0x1F35E, 0x1F370, 0x1F36B, 0x1F377, 0x1F375, 0x1F431, 0x1F436, 0x1F41D,
    0x1F41F,
]
# Regional indicators for common flags: US JP GB CN KR DE FR IT ES IN BR.
FLAG_RIS = [0x1F1E6 + ord(c) - 0x41 for c in "USJPGBCNKRDFEISAB"]


def ksx1001_hangul() -> list[int]:
    """The 2350 precomposed Hangul syllables of KS X 1001:1998, from the EUC-KR encoding."""
    out: list[int] = []
    for hi in range(0xB0, 0xC9):
        for lo in range(0xA1, 0xFF):
            try:
                ch = bytes([hi, lo]).decode("euc_kr")
            except UnicodeDecodeError:
                continue
            if 0xAC00 <= ord(ch) <= 0xD7A3:
                out.append(ord(ch))
    return out


def read_frequency_list(path: str, count: int) -> list[str]:
    """Top `count` han characters from Jun Da's modern frequency list (GB18030 CSV)."""
    text = open(path, "rb").read().decode("gb18030")
    chars = [
        line.split("\t")[1]
        for line in text.splitlines()
        if line.strip() and not line.startswith("/*")
    ]
    return [c for c in chars if len(c) == 1][:count]


def make_options() -> Options:
    opts = Options()
    opts.layout_features = ["*"]  # keep GSUB/GPOS: Arabic joining, flag ligatures
    opts.name_IDs = ["*"]
    opts.name_languages = ["*"]
    opts.glyph_names = False
    opts.hinting = False
    opts.recalc_bounds = True
    opts.recalc_average_width = True
    opts.drop_tables += ["DSIG", "VHEA", "VMTX"]  # vertical metrics unused by the UI
    return opts


def instance(src: str, weight: float = 400.0) -> ttLib.TTFont:
    """Pin a variable font to one weight with clean static-instance names."""
    font = ttLib.TTFont(src)
    instantiateVariableFont(font, {"wght": weight}, inplace=True, updateFontNames=True)
    return font


def build_sc_charset(freq: str) -> set[int]:
    charset: set[int] = set(range(0x20, 0x7F))            # ASCII
    charset |= set(range(0x3000, 0x3100))                  # CJK punctuation + kana
    charset |= set(range(0xFF01, 0xFF65))                  # fullwidth + halfwidth stops
    charset |= {ord(c) for c in read_frequency_list(freq, 2000)}
    charset |= {ord(c) for c in VARIANT_SUPPLEMENT}
    charset |= {ord(c) for c in GALLERY_STRINGS_JOIN()}
    return charset


def GALLERY_STRINGS_JOIN() -> str:
    return "".join(GALLERY_STRINGS)


def build_kr_charset() -> set[int]:
    charset: set[int] = set(range(0x3131, 0x318F))         # compatibility jamo
    charset |= set(ksx1001_hangul())
    charset |= {0x20, 0x2E, 0x2C, 0x2010, 0x2011, 0x2014}  # stops used between words
    return charset


def build_arabic_charset() -> set[int]:
    charset: set[int] = set(range(0x20, 0x7F))
    charset |= set(range(0x0600, 0x0700))                  # Arabic
    charset |= set(range(0x0750, 0x0780))                  # Arabic Supplement
    charset |= set(range(0x08A0, 0x0900))                  # Arabic Extended-A
    charset |= set(range(0xFB50, 0xFE00))                  # Presentation Forms-A
    charset |= set(range(0xFE70, 0xFF00))                  # Presentation Forms-B
    charset |= set(range(0x2010, 0x2028))                  # dashes/quotes/ellipsis
    charset |= {0x00A0, 0x200C, 0x200D}                    # NBSP, ZWNJ, ZWJ
    return charset


def build_emoji_charset() -> set[int]:
    charset: set[int] = set(EMOJI) | set(FLAG_RIS)
    charset |= {0x20, 0xA0, 0x200D, 0xFE0F}                # space, NBSP, ZWJ, VS16
    return charset


def assert_covered(paths_and_charsets: list[tuple[str, set[int]]]) -> None:
    """Fail loudly when a must-cover string would still resolve to .notdef."""
    union: set[int] = set()
    for _, cs in paths_and_charsets:
        union |= cs
    latin_ext = {0x20, 0x2014, 0x2026} | set(range(0x30, 0x3A))  # Inter always covers these
    for text in GALLERY_STRINGS:
        for ch in text:
            cp = ord(ch)
            if cp in union or cp in latin_ext:
                continue
            raise SystemExit(
                f"must-cover character {ch!r} (U+{cp:04X}) is in no generated charset; "
                "extend the charsets above"
            )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--sources", required=True, help="dir with the downloaded source fonts")
    ap.add_argument("--out", required=True, help="output dir (this assets/fonts dir)")
    ap.add_argument("--emoji", default=None, help="path to a CBDT NotoColorEmoji.ttf")
    args = ap.parse_args()

    src = args.sources
    out = args.out
    os.makedirs(out, exist_ok=True)

    sc_font = instance(os.path.join(src, "NotoSansSC-VF.ttf"))
    kr_font = instance(os.path.join(src, "NotoSansKR-VF.ttf"))
    ar_font = instance(os.path.join(src, "NotoNaskhArabic-VF.ttf"))

    emoji_path = args.emoji or os.environ.get("NOTO_COLOR_EMOJI_TTF")
    if emoji_path is None:
        for cand in ("/usr/share/fonts/noto/NotoColorEmoji.ttf",):
            if os.path.exists(cand):
                emoji_path = cand
                break
    if emoji_path is None or not os.path.exists(emoji_path):
        raise SystemExit("no NotoColorEmoji.ttf source (pass --emoji or set NOTO_COLOR_EMOJI_TTF)")
    emoji_font = ttLib.TTFont(emoji_path)

    freq = os.path.join(src, "CharFreq.csv")
    charsets = []
    outputs = []

    cs = build_sc_charset(freq)
    charsets.append(("NotoSansSC", cs))
    outputs.append(("NotoSansSC-400-subset.ttf", sc_font, cs))

    cs = build_kr_charset()
    charsets.append(("NotoSansKR", cs))
    outputs.append(("NotoSansKR-400-subset.ttf", kr_font, cs))

    cs = build_arabic_charset()
    charsets.append(("NotoNaskhArabic", cs))
    outputs.append(("NotoNaskhArabic-400-subset.ttf", ar_font, cs))

    cs = build_emoji_charset()
    charsets.append(("NotoColorEmoji", cs))
    outputs.append(("NotoColorEmoji-subset.ttf", emoji_font, cs))

    assert_covered(charsets)

    total = 0
    for name, font, cs in outputs:
        path = os.path.join(out, name)
        # pyftsubset wants a font loaded through its own options for save()
        opts = make_options()
        loaded = load_font_src(font, opts)
        ss = Subsetter(options=opts)
        ss.populate(unicodes=sorted(cs))
        ss.subset(loaded)
        save_font(loaded, path, opts)
        sz = os.path.getsize(path)
        total += sz
        print(f"{name}: {sz:,} bytes ({sz / 1024:.0f} KiB), {len(cs)} requested codepoints")
    print(f"total added: {total:,} bytes ({total / 1024:.0f} KiB)")
    return 0


def load_font_src(font: ttLib.TTFont, opts: Options) -> ttLib.TTFont:
    """Adapt an in-memory TTFont for the subsetter (round-trip through bytes)."""
    import io

    buf = io.BytesIO()
    font.save(buf)
    buf.seek(0)
    return ttLib.TTFont(buf)


if __name__ == "__main__":
    sys.exit(main())
