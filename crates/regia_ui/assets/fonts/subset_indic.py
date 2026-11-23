#!/usr/bin/env python3
"""Regenerate the Hebrew, Devanagari and Thai default-font subsets (ADR-1842, UI parity L06).

Sources (all SIL Open Font License 1.1; licence texts sit next to this script):

  Noto Sans Hebrew      https://github.com/google/fonts/tree/main/ofl/notosanshebrew
  Noto Sans Devanagari  https://github.com/google/fonts/tree/main/ofl/notosansdevanagari
  Noto Sans Thai        https://github.com/google/fonts/tree/main/ofl/notosansthai

Usage:
    python3 subset_indic.py --sources DIR --out DIR
    # DIR holds NotoSansHebrew-VF.ttf, NotoSansDevanagari-VF.ttf, NotoSansThai-VF.ttf (the
    # variable fonts named "NotoSans<Script>[wdth,wght].ttf" upstream).

Each font is pinned to wght=400, wdth=100 and subset to its script's blocks. GSUB and GPOS are
kept whole: Devanagari conjuncts and mark positioning and Thai mark stacking live there, and
dropping them would leave glyph soup instead of text. Requires fonttools >= 4.38.
"""

from __future__ import annotations

import argparse
import os
from fontTools import ttLib
from fontTools.subset import Options, Subsetter, load_font, save_font
from fontTools.varLib.instancer import instantiateVariableFont

COMMON = {0x20, 0xA0, 0x200C, 0x200D, 0x25CC} | set(range(0x2010, 0x2028)) | set(range(0x30, 0x3A))

SCRIPTS = {
    "Hebrew": (
        "NotoSansHebrew-400-subset.ttf",
        set(range(0x0590, 0x0600)) | set(range(0xFB1D, 0xFB50)),
    ),
    "Devanagari": (
        "NotoSansDevanagari-400-subset.ttf",
        set(range(0x0900, 0x0980)) | set(range(0xA8E0, 0xA900)) | {0x1CD0, 0x1CD2, 0x1CD3, 0x1CF2},
    ),
    "Thai": (
        "NotoSansThai-400-subset.ttf",
        set(range(0x0E00, 0x0E80)),
    ),
}


def make_options() -> Options:
    opts = Options()
    opts.layout_features = ["*"]
    opts.name_IDs = ["*"]
    opts.name_languages = ["*"]
    opts.glyph_names = False
    opts.hinting = False
    opts.recalc_bounds = True
    opts.recalc_average_width = True
    opts.drop_tables += ["DSIG", "VHEA", "VMTX"]
    return opts


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--sources", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    for script, (out_name, block) in SCRIPTS.items():
        font = ttLib.TTFont(os.path.join(args.sources, f"NotoSans{script}-VF.ttf"))
        instantiateVariableFont(font, {"wght": 400.0, "wdth": 100.0}, inplace=True, updateFontNames=True)
        cmap = font.getBestCmap()
        wanted = {cp for cp in (block | COMMON) if cp in cmap}
        opts = make_options()
        sub = Subsetter(opts)
        sub.populate(unicodes=sorted(wanted))
        sub.subset(font)
        path = os.path.join(args.out, out_name)
        save_font(font, path, opts)
        print(f"{out_name}: {len(wanted)} code points, {os.path.getsize(path) // 1024} KiB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
