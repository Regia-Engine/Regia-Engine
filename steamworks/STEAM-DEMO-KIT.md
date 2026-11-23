# Steam release kit for the forest demo

The free v0.5 forest demo, working name **Regia Forest Demo**, must be *ready to release* on Steam by
2026-11-22: built, packaged, depots staged and the store kit complete. Going live follows Valve's own
timeline afterwards (see [Earliest live date](#earliest-live-date)). This page is the owner's checklist, in order.
Everything that needs an account, a payment, tax details or an upload credential is the owner's step; nothing in the
repository logs in, uploads or stores a credential.

The working name lives in one place, `steamworks/DEMO_NAME.txt`. Every file the kit generates (VDF build
descriptions, store texts, upload notes, the player README) takes the name from there, so a rename is one edit to that
file. The prose on this page and in the book says "the demo" except where it quotes the current name.

ADR-4899, ADR-5002. Tools: `cargo xtask demo-package` (builds the package, book/user-guide/demo-package.md) and
`cargo xtask steam-demo-upload` (stages it for steamcmd, book/agents/steam-demo-upload.md). Templates:
`steamworks/`. This page lives in `steamworks/` (not `docs/releases/`) because the uploader's test compiles it in and
CI must run when it changes (ADR-5002). Evidence for the 2026-11-09 corrections: `docs/evidence/v05-steam-demo-gaps.md`
and the readiness audit (`<scratch>`).

## Valve's rules this kit follows

Read on Valve's public pages on 2026-11-09 (readiness audit, no partner login). Valve changes these pages; open the
link before you pay or submit.

| # | Valve rule | Value used here | Source |
|---|---|---|---|
| 1 | Steam Direct product fee | USD 100 per product, not refundable; recouped after USD 1,000 adjusted gross revenue (a free product never earns that, so it is not recouped) | <https://partner.steamgames.com/steamdirect>, <https://partner.steamgames.com/doc/gettingstarted/appfee> |
| 2 | New-partner waiting period | **21 days** between paying the app fee and the first release (for a partner's first few titles) | <https://partner.steamgames.com/steamdirect> |
| 3 | "Coming Soon" visibility | The store page is public as Coming Soon for at least 2 weeks before release; it can only be posted after the store review passes | <https://partner.steamgames.com/doc/store/coming_soon> |
| 4 | Store page review and build review | Two separate reviews. Plan 3 to 5 business days each and submit the page at least 7 business days before it should go public (the Steam Direct overview says 1 to 5 days) | <https://partner.steamgames.com/doc/store/review_process> |
| 5 | The page lists only what the build does | Every feature on the page must be in the build at launch | <https://partner.steamgames.com/doc/store/review_process> |
| 6 | Content survey | Required; sets the content descriptors, holds the AI-content questions (about content that ships), and gives the German age rating (unrated games are hidden in Germany) | <https://partner.steamgames.com/doc/gettingstarted/contentsurvey> |
| 7 | Store graphics, client icons | Sizes in [Graphics](#graphics); capsules show the product name and no other text | <https://partner.steamgames.com/doc/store/assets/standard>, <https://partner.steamgames.com/doc/store/assets/libraryassets>, <https://partner.steamgames.com/doc/store/assets/community> |
| 8 | Screenshots | At least 5 gameplay shots, 16:9, 1920 x 1080 or larger; mark at least 4 suitable for all ages | <https://partner.steamgames.com/doc/store/assets/standard> |
| 9 | Trailer | Required to release: up to 1920 x 1080, 30 or 60 fps, H.264 + AAC `.mp4` preferred, 16:9 | <https://partner.steamgames.com/doc/store/trailer> |
| 10 | Short description | Plain text, one line, no dates, a few hundred characters (the kit keeps it under 300) | <https://partner.steamgames.com/doc/store/page/description> |
| 11 | Free product | Select "This is a free product" when the app is created; a free product has no price and no in-app purchases. The paid price grid starts at USD 0.99, so "free" is not a price | <https://partner.steamgames.com/doc/store/freetoplay>, <https://partner.steamgames.com/doc/store/pricing> |
| 12 | Demo app type | A Steam "Demo" app hangs off a base game; there is no Regia base game, so this demo is its own free product (owner question 1) | <https://partner.steamgames.com/doc/store/application/demos> |
| 13 | Uploading | steamcmd `+login <account> +run_app_build <app_build.vdf> +quit`; Steam Guard on the first login; `SetLive` cannot set the default branch live | <https://partner.steamgames.com/doc/sdk/uploading> |
| 14 | Linux runtime | Pick the runtime per branch under Installation > Linux Runtime, matching what the binary was built against. The default for a native game is scout on a soldier container (glibc 2.28), which cannot run this binary (it needs glibc 2.29 and 2.30 symbols): pick **Steam Linux Runtime 3.0 (sniper)**, glibc 2.31, which carries every library the binary uses | <https://partner.steamgames.com/doc/store/application/platforms/linux>, <https://github.com/ValveSoftware/steam-runtime>, evidence row 8 |
| 15 | Redistributables | Tick the runtimes the build needs on Installation > Redistributables; Steam installs them before the first launch | <https://partner.steamgames.com/doc/features/common_redist> |
| 16 | Controller support | "Full" means pad prompts that match the device on screen, every part playable with a pad, no launcher that needs a mouse; this build is **partial** | <https://partner.steamgames.com/doc/features/steam_controller/getting_started_for_devs> |
| 17 | Steam Deck | Deck compatibility review is Valve's and not a release requirement; the page names the Deck only after the Deck check passes on this package | <https://partner.steamgames.com/doc/steamhardware/compat> |

## Questions for owner

0. **The scene on the store page.** The demo ships `samples/showcase/example_scene`: cone trees, rocks, a capsule
   standing in for a vehicle on a simple heightfield. Valve's screenshots must show the product (rule 8), so the store
   page will show that scene. The renders in `<scratch>` are honest pictures of what ships.
   If the demo should carry real forest content before 11-22, that is a content job to schedule now; the kit reruns
   unchanged on a new scene (`render-shots.sh`, `compose-capsules.py`, `make-icons.py`).
1. **Standalone free app, or wait for a base game?** Steam's "Demo" app type hangs off a base game. Regia has no game
   on Steam, so the kit assumes the demo is its own free product: one USD 100 fee that a free product never recoups
   (rules 1, 11, 12). The depots and the kit are the same either way; only the app id changes.
2. **AI disclosure in the content survey (rule 6).** The survey asks about content that ships and that players see
   or hear (art, sound, text). The scene's 12 assets are hand-written or generated by the engine's own code, listed
   with their authors and licences in `asset_manifest.json`. The engine's code was written with AI agents; the public
   page says development tools are not what the section is about. Your answer to give.
3. **The name.** "Regia Forest Demo" is a working name; change `steamworks/DEMO_NAME.txt` and rerun the lead's
   staging steps if you pick another. Type the same text on the partner site.
4. **Capsule lettering.** The kit uses a plain bold face as a review draft. Approve it or supply another; the lead
   reruns `compose-capsules.py`.
5. **Sample licence.** `LicenseRef-Regia-Sample` (every scene asset) still needs its legal paragraph from counsel.
   A custom EULA on the partner site is optional; add one only if counsel wants it.

## Owner steps, in order

Everything here is yours: it needs your account, money, identity or approval. The lead's steps (marked **lead**) slot
in where they are needed; each says what the lead hands you.

### A. Before anything is uploaded

1. **Partner paperwork** (O1): sign in to the Steamworks partner site and finish the legal entity, bank, tax interview
   and identity check if they are not done. Valve describes this as 15 to 30 days and releases nothing before it.
2. **Answer the questions above** (O14, O3, O12, O13): the scene (0), standalone or demo (1), the name (3), lettering
   (4), the sample licence (5). A "no" to question 0 starts a content job and a re-render before step 9.
3. **Pay the Steam Direct fee** (O2) for one product. Write the date in [Dates](#dates): the 21-day wait (rule 2)
   counts from it.
4. **Create the app** (O3): type the name from `steamworks/DEMO_NAME.txt`, and select **"This is a free product"**
   (rule 11). Do not enter a price, and add no in-app purchases.
5. **Depots and package** (O4): Installation > Depots. Name two depots; set the first to Windows, 64-bit, and the
   second to Linux + SteamOS, 64-bit. Publish, then check both depot ids are in the app's store package (Associated
   Packages & DLC); a depot missing there installs nothing. Give the lead the **app id** and the **two depot ids**
   (not secrets).

### B. Lead stages the build (lead, as soon as the ids exist)

L1. **lead**: build the package on the release candidate (book/user-guide/demo-package.md). Binaries built here:

```sh
cargo xtask demo-package --targets windows,linux --app-id <APP_ID>
cargo xtask demo-package --verify target/demo-package
```

   Prebuilt shipping binaries need their build ids (ADR-5002), and both must come from one commit, the tagged one.
   For v0.5.0 the binaries are built at the tag (Linux in the bullseye container, Windows with cargo-xwin) and passed
   with `--from-*` and `--git-* <tag commit>`: the exact commands and their measured times are in
   `docs/releases/v0.5.0-runbook.md` § 6 (steps 11 and 13). Do not reuse the old v0.2.20 inputs under
   `<scratch>`: they predate ADR-5419 and carry the agent protocol, which
   `cargo xtask demo-notices --verify` refuses.

L2. **lead**: stage for steamcmd (hashes re-checked, scripts and store texts written, nothing uploaded):

```sh
cargo xtask steam-demo-upload --app <APP_ID> --depot-win <WIN_DEPOT_ID> --depot-linux <LINUX_DEPOT_ID> \
  --package <STAGED> --dry-run
```

   It prints two commands and writes `target/steam-demo-upload/UPLOAD-COMMAND.txt` and `store/` (the three store texts
   with each depot's install size). Hand the owner that folder and the kit's graphics folder. Upload from Linux so
   `run-demo.sh` keeps its executable bit.

### C. Upload (you, about 10 minutes)

6. **steamcmd** (O6): install it (<https://developer.valvesoftware.com/wiki/SteamCMD>). A build account with only
   "Edit App Metadata" and "Publish App Changes To Steam" for this app is enough.
7. Run the **preview** command from `UPLOAD-COMMAND.txt` (checks scripts and content, uploads nothing). Type your
   account name in place of `<your-steam-build-account>`; steamcmd asks for the password and the Steam Guard code.
8. Run the **upload** command. The build appears under SteamPipe > Builds. `SetLive` is empty, so it is live nowhere.

### D. Installation settings (you, clicks)

The menu names are the partner site's as Valve's public pages give them; look for the same setting if a name moved.

9. **Launch options** (O5), Installation > General, two entries:

    | OS | Executable | Arguments |
    |---|---|---|
    | Windows, 64-bit | `showcase.exe` | `--demo content\scene` |
    | Linux + SteamOS, 64-bit | `run-demo.sh` | (none) |

    Steam starts in the install folder. `run-demo.cmd` (Windows) and `run-demo.sh` do the same outside Steam.
10. **Redistributables** (O7): tick **Visual C++ 2015-2022 Redistributable (x64)**. The Windows binary imports
    `VCRUNTIME140.dll` and no other Visual C++ DLL (evidence row 13).
11. **Linux runtime** (O8): Installation > Linux Runtime (it appears once step 9's Linux entry exists): select
    **Steam Linux Runtime 3.0 (sniper)** (rule 14). Do not leave the default.
12. **Test on a private branch** (O6): SteamPipe > Builds, set the build live on a password-protected beta branch,
    install it from your Steam client on Windows (and on Linux or the Deck if you have one) and start it once.

### E. Store page (you, copy and paste, about 1 hour)

13. **Texts** (O9): Store page > Basic info, paste from the lead's `store/` folder:
    - `short-description.txt` into the short description;
    - `about.bbcode` into "About this game" (Steam markup);
    - `system-requirements.txt` into the Windows and SteamOS + Linux requirement fields, line by line. Leave the
      recommended fields empty (nothing is measured there yet; see [System requirements](#system-requirements)).
    - Supported languages: English (interface; no voice).
    - Genre and tags: Simulation; suggested tags Exploration, Relaxing, Nature, Walking Simulator, Benchmark.
      Do not tick "Free to Play" as a genre: the free-product choice in step 4 is what makes it free.
    - Controller support: **Partial Controller Support** (rule 16; evidence row 11).
    - Do not tick Steam Deck, cloud, achievements or the overlay as features: this build uses none of them.
14. **Graphics** (O10): Store page > Graphical assets and the client icons: upload the files from
    `<scratch>` (see [Graphics](#graphics)), the trailer and its 1920 x 1080 poster frame
    (lane v05-demo-trailer, `<scratch>`, after your approval). Mark at least four screenshots
    suitable for all ages.
15. **Content survey** (O11): no violence, no nudity or sexual content, no gambling, no user content, no online
    interaction, no in-app purchases. The AI section: question 2. Submit it early: the German rating comes from it.
16. **Store review** (O15): complete the store checklist and press "Mark As Ready For Review" at least 7 business days
    before you want the page public (rule 4). When it passes, press "Post as Coming Soon" at once: the 2-week clock
    (rule 3) starts then. Write the dates in [Dates](#dates).

### F. Build review (you); release is not this milestone

17. **Build review** (O15): set the tested build live on the **default** branch, complete the release checklist and
    submit the build for review (rule 4).
18. **Do not press release.** The 2026-11-22 goal is "ready": page approved or in review, build uploaded and in review.
    Release when the 21-day wait and the 2-week Coming Soon period have both passed, on the date you choose.

### Lead's checks before handing the kit over

- L3. `cargo xtask demo-package --verify <STAGED>` exits 0; `steam-demo-upload ... --dry-run` exits 0 and
  `steam-upload-plan.json` lists both depots with their build ids.
- L4. The demo starts from the staged folders: `content/linux/run-demo.sh --check` and, on Windows (the min-spec Windows laptop),
  `content\windows\showcase.exe --demo content\scene --check`, exit 0. (Windows: done for 0.2.20 on 2026-11-10, see
  L6; and for the 0.2.23 release candidate zip `01ca8a6a...` on 2026-11-10, with the `WINDOWS-CHECK.md` steps run on
  the min-spec Windows laptop by script, input through the agent on the twin, no physical pad: Low by detection, nothing built outside
  the warm queue, no socket, exit 0. The first launch on a cold
  driver took 79 s to the forest there, the second 4.2 s: `docs/evidence/v05-demo-rc-the min-spec Windows laptop.md`, ADR-5578.)
- L5. **Steam Deck** (audit row 12, open): when the Deck answers, run `DECK-CHECK.md` from the staged folder on this
  package's `regia-demo-<version>-linux.tar`. Until it passes, the page does not name the Deck.
- L6. **Visual C++ on a clean machine** (audit row 13, open): one launch of `content\windows\showcase.exe --demo
  content\scene --check` on a Windows image that never had the VC++ 2015-2022 runtime (a fresh VM or a reset PC; a new
  user profile is not enough, the runtime lives in System32). Expect a "VCRUNTIME140.dll was not found" error without
  it and exit 0 after installing `vc_redist.x64.exe`. That is what step 10 makes Steam do.
  - Done so far (lane v05-windows-close, 2026-11-10, the min-spec Windows laptop): the 0.2.20 zip (SHA256 `79a8a458...`) unpacked into
    a new folder, `--check` run from `cmd` with `PATH` set to the Windows folders only: exit 0. That machine has the
    runtime (v14.51) in System32, so this is L4, not L6. The exe imports eight `VCRUNTIME140.dll` functions
    (`__C_specific_handler`, `__CxxFrameHandler3`, `__current_exception`, `__current_exception_context`, `memcmp`,
    `memcpy`, `memmove`, `memset`); every one is in the first 2015 runtime (14.0), so any installed 2015-2022 runtime
    is new enough.
  - Owner steps, option A (a fresh Windows VM, nothing installed after setup): copy the zip in, unpack it with
    Explorer, open `cmd` in `regia-windows-0.2.20`, run `where vcruntime140.dll` (expect "Could not find"), then
    `showcase.exe --demo content\scene --check` (expect the "VCRUNTIME140.dll was not found" dialog), then run
    `vc_redist.x64.exe /install /quiet /norestart` (Microsoft's "latest supported Visual C++ Redistributable" page)
    and the `--check` line again (expect `showcase: loaded SceneData ...` and exit 0, `echo %ERRORLEVEL%`).
  - Owner steps, option B (Windows Sandbox on a Windows 11 Pro PC; turning the feature on is a system change, so it is
    your call): Settings > Optional features > Windows Sandbox, reboot, start Windows Sandbox, paste the zip in, then
    the same commands as option A. `--check` needs no GPU. Close the Sandbox to discard it.
- L7. **Physical gamepad** (audit row 11, open): one pass with a real pad on the min-spec Windows laptop: fly, open the panel with
  Select, move between its controls with the D-pad, change a slider, screenshot with North, benchmark with Start.
  Full controller support also needs the pad's buttons named on screen; until then the page says partial.
- L8. Graphics: `python3 compose-capsules.py shots kit ../../GitHub/Rust-engine/steamworks/DEMO_NAME.txt` and
  `python3 make-icons.py kit` in `<scratch>` after any scene, look or name change; both check
  their sizes.

## Earliest live date

The binding rules: 21 days after the fee (rule 2) and 2 weeks of public Coming Soon after the store review (rule 3).
US Thanksgiving is Thursday 2026-11-26; treat 11-26 and 11-27 as non-business days for Valve's reviews.

| Fee paid | Store page submitted | Coming Soon posted | Earliest release |
|---|---|---|---|
| Mon 2026-11-09 | Tue 11-10 | Fri 11-14 (fast review, 1 to 5 days) | **Mon 2026-11-30** (the 21-day line binds; the 2 weeks end Sat 11-28) |
| Mon 2026-11-09 | Tue 11-10 | Wed 11-18 (7 business days) | **Wed 2026-12-02** (2 weeks after Coming Soon) |
| Mon 2026-11-16 | Tue 11-17 | Fri 11-27 to Wed 12-02 | Mon 12-07 at the earliest (21 days), later if Coming Soon posts after 11-23 |

Add about a week for one rejected review and its resubmission. An owner who has already released other titles on
Steam is past the new-partner wait; then only the Coming Soon rule binds.

## Dates

Fill these in as they happen (the lead keeps the table above in step with them).

| Event | Date |
|---|---|
| Fee paid | |
| Store page submitted / passed | |
| Coming Soon public | |
| Build uploaded (build id) | |
| Build review submitted / passed | |
| Released | |

## Package

The staged package the lead hands over: `<scratch>`, rebuilt 2026-11-09 on the build server
by lane v05-steam-demo-gaps from the v0.2.20 shipping inputs (Windows b394124ea, Linux 924493f56) with step L1's
command, the player README, the build ids and no tester launcher. `--verify` ok; `steam-demo-upload --dry-run` with
stand-in ids ok; `content/linux/run-demo.sh --check` exit 0.

| Archive | Bytes | BLAKE3 | SHA256 |
|---|---|---|---|
| `regia-demo-0.2.20-windows.zip` | 100,124,562 | `3b7b4417f8e3cc2741a4109bd37538aa1c4757ecee2d4d0043400d981c847e36` | `79a8a4580aa87db00e2061e09fccf937eacd270ec50e39f0010fa7d0ecd1b97d` |
| `regia-demo-0.2.20-linux.tar` | 276,309,504 | `175e16fd50fa2156ca9fb286caa089f28a53efd028d3fd0dc0c2ea2e4cb3ea2a` | `eba5f056220f4f749404480daba7d80b33b30c5aeaec60b3064494df814653e5` |

Depots: Windows 20 files, 100,121,106 bytes (101 MB on the store page); Linux 20 files, 276,290,020 bytes (277 MB).
This package replaces the coldlaunch lane's (`8326f8d7...` / `07d43a63...`): the binaries are the same, the README,
manifests and Windows file list changed. The Windows run on the min-spec Windows laptop and the Deck check (L4, L5) have not been
repeated on these archives. Rebuild on the release candidate once the app id exists (step L1).

## Graphics

Rendered from the demo through the agent protocol ([shot list](#shot-list)); the files are in
`<scratch>` (not in the repository). `cargo xtask steam-demo-upload` writes the same table
to `store/graphics.txt` (one list in code, `STORE_ASSETS`, and a test keeps this table equal). The file extension is
the format Valve asks for.

| Asset | Size (px) | Required | File |
|---|---|---|---|
| Header capsule | 920 x 430 | yes | `header-capsule.png` |
| Small capsule | 462 x 174 | yes | `small-capsule.png` |
| Main capsule | 1232 x 706 | yes | `main-capsule.png` |
| Vertical capsule | 748 x 896 | yes | `vertical-capsule.png` |
| Page background | 1438 x 810 | no | `page-background.png` |
| Library capsule | 600 x 900 | yes | `library-capsule.png` |
| Library header | 920 x 430 | yes | `library-header.png` |
| Library hero | 3840 x 1240 | yes | `library-hero.png` |
| Library logo | 1280 x 720 | yes | `library-logo.png` |
| App icon (JPG) | 184 x 184 | yes | `app-icon.jpg` |
| Shortcut icon (PNG) | 512 x 512 | yes | `shortcut-icon.png` |
| Screenshot (5 or more) | 1920 x 1080 | yes | `screenshot-01.png` to `screenshot-06.png` |

The library logo is the name on a transparent background (no scene); the capsules carry the name over a scene plate.
The library hero carries no text (Valve overlays the logo on it). The two client icons are one picture: the right
square of `screenshot-03.png` (hour 17, the low sun and long shadows), no text, made by `make-icons.py`.
`community-icon.png` (184 x 184 PNG, the earlier render) stays in the folder as a spare; upload `app-icon.jpg`.

## Shot list

The scene's sun clock runs at day 80 and latitude 46 degrees: the sun sets just after 18:00 and the scene has no
night lighting (21:30 renders the ground black), so the shots stay between 07:00 and 17:00.

Every picture comes from the shipped scene (`samples/showcase/example_scene`) in `showcase --demo`, driven by the
agent protocol (book/agents/showcase-demo.md), Ultra tier, headless so the size is exact. One shot per row; `hour` is
the sun clock, `weather` the preset of `demo.set_weather`, `fly` a `demo.fly` call before the shot.

| File | Size | Tier | Hour | Weather | Camera | Purpose |
|---|---|---|---|---|---|---|
| `screenshot-01.png` | 1920 x 1080 | Ultra | 9.5 | clear | start view | Morning light through the trees |
| `screenshot-02.png` | 1920 x 1080 | Ultra | 13.0 | clear | `fly` forward 240 frames, rise 0.2 | Noon, wide view |
| `screenshot-03.png` | 1920 x 1080 | Ultra | 17.0 | clear | `fly` forward 120, look yaw 0.004 x 120 | Low evening sun |
| `screenshot-04.png` | 1920 x 1080 | Ultra | 15.0 | overcast, rain 0.6, wind 8 | `fly` forward 60 | Overcast sky (no rain is visible in this build) |
| `screenshot-05.png` | 1920 x 1080 | Ultra | 7.2 | clear | start view | Early morning, low sun |
| `screenshot-06.png` | 1920 x 1080 | High | 11.0 | clear | start view, `demo.show controls` | Meant to show the panel; the rendered frame shows the hillside only |
| `header-capsule.png` plate | 920 x 430 | Ultra | 17.0 | clear | as screenshot-03 | Capsule plate |
| `small-capsule.png` plate | 462 x 174 | Ultra | 17.0 | clear | as screenshot-03 | Capsule plate (name must stay readable) |
| `main-capsule.png` plate | 1232 x 706 | Ultra | 9.5 | clear | as screenshot-01 | Capsule plate |
| `vertical-capsule.png` plate | 748 x 896 | Ultra | 9.5 | clear | start view, look pitch up | Capsule plate |
| `page-background.png` | 1438 x 810 | Ultra | 13.0 | overcast | as screenshot-02 | Background (Steam darkens it) |
| `library-capsule.png` plate | 600 x 900 | Ultra | 17.0 | clear | start view | Library plate |
| `library-header.png` plate | 920 x 430 | Ultra | 17.0 | clear | as screenshot-03 | Library plate |
| `library-hero.png` | 3840 x 1240 | Ultra | 9.5 | clear | as screenshot-02, no panel | Library hero, no text |
| `community-icon.png` | 184 x 184 | Ultra | 17.0 | clear | crop of screenshot-03 | Spare icon (superseded by `app-icon.jpg`) |
| `shortcut-icon.png`, `app-icon.jpg` | 512 x 512, 184 x 184 | Ultra | 17.0 | clear | right square of screenshot-03 | Client icons (`make-icons.py`) |

How they were made (2026-11-09, the test machine RTX 5080 under its card lock, dev-profile `showcase` and `regia` from the
release-kit branch): `<scratch> <bin dir> samples/showcase/example_scene <out>`
starts one headless `showcase --demo --headless --tier Ultra --size WxH --project-dir <fresh dir>` per row and drives
it with `regia agent call ... --project <fresh dir>`: `demo.set_quality`, `demo.show {"page": "hidden"}`,
`demo.set_time_of_day`, `demo.set_weather` (`transition_s` 0), `demo.fly` when the row has one, 90 settle frames
(polled through `demo.state.frame`), `demo.screenshot`, `quit.request`. Then
`compose-capsules.py <shots> <kit> steamworks/DEMO_NAME.txt` sets the name over the plates, draws the library logo
and checks every size, and `make-icons.py <kit>` writes the two client icons. Rerun all three after any change to the
scene, the look or the name.

## Store page draft

The text is in `steamworks/store/` (templates; `{{DEMO_NAME}}` is replaced by the working name, and
`{{INSTALL_MB_WINDOWS}}` / `{{INSTALL_MB_LINUX}}` by each depot's size, when `steam-demo-upload` runs). No comparisons
with other products, no "first", "unique" or "best", no other engine named, and nothing this build does not show:
no night, rain, snow or storm, no Steam Deck, no full controller support (a test checks the rendered text). Put a claim
back only with the evidence: rain or night when a shot shows it, the Deck when L5 passes, full controller support when
the panel names the pad's buttons and L7 passes.

## System requirements

Only measured hosts (evidence row 9):

- **Windows minimum**: the min-spec Windows laptop MiniBook X (Intel N150, 4 cores, Intel Graphics, 16 GB) running this package's zip
  (coldlaunch run2): Low tier chosen by GPU class, 17 fps steady in the 1280 x 720 window (15-16 fps on the first run).
  "4 GB RAM": peak working set 978 MB and 654 MB of GPU shared memory on the same PC (v0.2.15 `vfx_lab`, perf-log
  round "N150 min-spec pass"), which leaves room for Windows.
- **SteamOS + Linux minimum**: the GTX a test machine, `xtask showcase-table` of v0.2.17 at 2560 x 1440 (perf-log round
  "Linux GPU sweep of v0.2.17"): Low 15.2 to 24.2 ms median per view (about 40 to 65 fps), High 28.0 to 40.6 ms.
  The rig's CPU was not reachable to record; the processor line stays generic. Integrated GPUs on Linux and the
  Steam Deck are not measured on this package.
- **Recommended** (both): left empty. The earlier RTX 3080 line was the engine's GPU table, not a run; fill it after
  a `cargo xtask showcase-table` row on the a test machine (Windows) or a re-run on the GTX a test machine with this package.
