# Look CI: the look check at every green tag

The merge gate proves a tag builds and its goldens hold. Goldens are small lavapipe frames of
small scenes, so a merge can still change how DO27's maps look and pass the gate. Look CI renders
the twelve UE parity plates on DO27's content at every green tag. It compares them with the
previous green tag and flags what changed.

## What one run does

For a tag `T` and the previous green tag `P`:

1. Renders the twelve parity plates (`look_r2 --set parity` framing, 960×540, converged) at
   Desktop High and at the Deck tier on a shared RTX 5090 (`LOOK_R2_TIER=desktop-high|deck`).
   The plates are `bore_twelve_{wide,mid,close}`, `switchbacks_{wide,mid,close}`,
   `tunnel_interior`, `cockpit`, `dusk_vista`, `set_piece_borehole`, `fireball_20m` and `smoke_80m`.
2. Runs `do27_chase --plates` (the six landscape plate cameras, 2560×1440, 180 frames) on the
   exclusive bench 5090 and keeps the per-frame CSV.
3. Compares `T` with `P` and with the UE plates. It writes sheets, heat maps, `compare.json` and a
   markdown summary.

`P` is rendered too if it has no render set yet. A finished render set is not rendered again,
so a second run only redoes the comparison (seconds).

## Pinned content

DO27's content changes between tags as well as the engine. If `T` and `P` were rendered on
different content, the content changes would be flagged as engine changes. This happened once:
one commit rendered at two times gave FLIP 0.64 on `tunnel_interior`, because DO27 had re-exported
the switchbacks map in between. So every run pins the content:

1. The script hashes the content drop: sha256 of every file, then one hash over the sorted list
   of paths and hashes. The first 12 hex digits are the snapshot's *fingerprint*.
2. It copies the drop once to `lookci/content/<fingerprint>/` on the build machines. Unchanged files are
   hard links to the previous snapshot. If the drop changes during the copy, the script retries.
3. Both tags render from that snapshot. Each render set records its fingerprint in `content`.
4. If `P`'s set was rendered on another snapshot, the script renders `P` again on this one into
   `lookci/<P>/c-<fingerprint>/`. The binaries of every rendered commit are cached in
   `lookci/bin/<sha>/`, so this does not rebuild `P`.

The summary names the snapshot. Content changes themselves are not flagged; a re-export shows up
as a re-rendered `P`.

## Flags

| flag | rule |
|---|---|
| frame changed | FLIP mean (or p95) of a plate against `P` > 3 × the noise floor between two renders of one tree. Floors: mean 0.004, p95 0.02 |
| UE drift | the plate's gap to the UE plate grows by > 10 percentage points of the UE value (mean luma, contrast, saturation), or by > 6° (hue) |
| GPU cost | a plate's GPU frame p50 rises by > 10 % |

A *frame changed* flag means "look at this plate", not "this is a regression". Intended look work
raises it too. The sheet shows whether the change is good. The UE-drift flags are the ones that
mean "further from UE than before".

Plate statistics are the look-r2 measures, computed on the displayed 8-bit frame:

- **luma:** BT.709 mean and median
- **contrast:** standard deviation of the luma
- **saturation:** mean HSV saturation
- **hue:** circular mean hue of the saturated pixels (S ≥ 0.35, V ≥ 0.12), undefined below 1 % of
  the frame
- **clipping:** per cent of pixels with any channel ≥ 250

UE plates are box-filtered to the render size. Every tag is measured by one build of `look_ci`.
A tag rendered by an older harness is therefore scored on the same definitions as a new one.

## The `look_ci` binary

```text
look_ci render  --out <dir> [--ue <ue plates dir>] [--width W] [--height H] [--frames N]
                                                        # 12 plates at LOOK_R2_TIER + metrics.json
look_ci metrics --dir <dir> [--ue <ue plates dir>]      # metrics.json for an existing plate dir
look_ci compare --this <set> [--prev <set>] [--ue <dir>] [--noise noise.json]
                [--this-label T] [--prev-label P] [--location text] [--md path]
                [--content <fingerprint>]
look_ci noise   --a <set> --b <set> --out noise.json    # FLIP + GPU spread of two identical runs
```

A *set* is a directory with `desktop-high/<shot>.png`, `deck/<shot>.png` and `timing.csv`.
`compare` writes into the set:

- `compare/<tier>/<shot>.png`: four 960×540 panels, **UE | previous | this | FLIP heat map**
  (black, magenta, orange, white as the error grows)
- `overview_<tier>.png`: all twelve rows at half size
- `compare.json` and `summary.md`

FLIP and SSIM come from `regia_render::compare::compare_srgb8`, which scores two display-encoded
RGBA8 frames.

## Running it

The orchestration script is `maintainer-scripts/lookci.sh`. It is not in git, because it drives
the build machines.

```text
lookci.sh <tag>                 # renders on the build server, summary -> maintainer-scripts/lookci/<tag>.md
lookci.sh --calibrate <tag>     # renders <tag> twice -> noise.json (the FLIP noise floor)
```

The output goes to `the build server:<build-dir>>/` (`<tag>/c-<fingerprint>/`
when the tag is rendered again on newer content). DO27's content snapshots and the UE plates are
copied to the build machines read-only. Never commit them or renders of them to this repository.
