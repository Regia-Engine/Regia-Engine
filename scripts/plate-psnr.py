#!/usr/bin/env python3
"""PSNR / MAE between two plate directories (same file names).

  plate-psnr.py <ref-dir> <other-dir>
      Print MAE / PSNR per plate.
  plate-psnr.py <ref-dir> <other-dir> --vendor intel_arc [--floors scripts/plate-floors.json]
      Also check each plate against the vendor's floor; exit 1 when a plate is below its floor or
      a plate with a floor is missing from either directory. A vendor with no floors exits 2
      (not gated yet), so a gate script can tell "unmeasured" from "failed".
  plate-psnr.py --self-test
"""
import json
import os
import sys
import tempfile

import numpy as np
from PIL import Image

DEFAULT_FLOORS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "plate-floors.json")


def psnr_of(x, y):
    d = x - y
    mse = (d * d).mean()
    return float("inf") if mse == 0 else 10 * np.log10(255 * 255 / mse), d


def load(path):
    return np.asarray(Image.open(path).convert("RGB"), dtype=np.float64)


def compare(a, b):
    """{plate name: psnr dB} plus the printed lines for every png both directories hold."""
    out, lines = {}, []
    for f in sorted(os.listdir(a)):
        if not f.endswith(".png") or "diff" in f or not os.path.exists(os.path.join(b, f)):
            continue
        x, y = load(os.path.join(a, f)), load(os.path.join(b, f))
        if x.shape != y.shape:
            lines.append(f"{f}: size differs {x.shape} vs {y.shape}")
            out[f[:-4]] = float("-inf")
            continue
        p, d = psnr_of(x, y)
        out[f[:-4]] = p
        lines.append(f"{f}: MAE {np.abs(d).mean():.3f} PSNR {p:.1f} dB px>8 {100*(np.abs(d).max(axis=2)>8).mean():.2f}%")
    return out, lines


def check(measured, floors):
    """Failure strings: a plate under its floor, or a floored plate that was not measured."""
    fails = []
    for plate, floor in sorted(floors.items()):
        if plate not in measured:
            fails.append(f"{plate}: missing (floor {floor:.1f} dB)")
        elif measured[plate] < floor:
            fails.append(f"{plate}: {measured[plate]:.1f} dB under the floor {floor:.1f} dB")
    return fails


def self_test():
    rng = np.random.default_rng(7)
    base = rng.integers(0, 256, (32, 32, 3), dtype=np.uint8)
    noisy = np.clip(base.astype(np.int16) + rng.integers(-6, 7, base.shape), 0, 255).astype(np.uint8)
    with tempfile.TemporaryDirectory() as t:
        ra, rb = os.path.join(t, "ref"), os.path.join(t, "other")
        os.makedirs(ra)
        os.makedirs(rb)
        for name, img in (("p_same", base), ("p_noisy", noisy)):
            Image.fromarray(base).save(os.path.join(ra, name + ".png"))
            Image.fromarray(img).save(os.path.join(rb, name + ".png"))
        measured, _ = compare(ra, rb)
    assert measured["p_same"] == float("inf")
    assert 30 < measured["p_noisy"] < 45, measured
    assert check(measured, {"p_same": 60.0, "p_noisy": 25.0}) == []
    fails = check(measured, {"p_noisy": 60.0, "p_gone": 20.0})
    assert len(fails) == 2 and "missing" in fails[0] and "under the floor" in fails[1], fails
    with open(DEFAULT_FLOORS) as fh:
        data = json.load(fh)
    assert data["vendors"], "floor file has no vendors"
    for vendor, plates in data["vendors"].items():
        assert plates and all(v > 0 for v in plates.values()), vendor
    print("plate-psnr self-test ok")


def main(argv):
    if argv[:1] == ["--self-test"]:
        self_test()
        return 0
    args, opts = [], {}
    it = iter(argv)
    for a in it:
        if a in ("--vendor", "--floors"):
            opts[a[2:]] = next(it, None)
        else:
            args.append(a)
    if len(args) != 2:
        print(__doc__)
        return 64
    measured, lines = compare(args[0], args[1])
    print("\n".join(lines))
    vendor = opts.get("vendor")
    if vendor is None:
        return 0
    with open(opts.get("floors") or DEFAULT_FLOORS) as fh:
        floors = json.load(fh)["vendors"].get(vendor)
    if not floors:
        print(f"no PSNR floors for vendor {vendor!r}: not gated (measure it, then add a row to plate-floors.json)")
        return 2
    fails = check(measured, floors)
    for f in fails:
        print("FAIL " + f)
    print(f"plate floors for {vendor}: {'FAIL' if fails else 'PASS'} ({len(floors)} plates)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
