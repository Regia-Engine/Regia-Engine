# Steam Deck

The Deck is a first-class Regia Engine target: a 7" 1280×800 handheld that runs native Linux games in
the Steam Linux Runtime ([Steam](steam.md), [Packaging](packaging.md)). This page has two parts:
what the engine does for the Deck, and the **test steps the owner runs on the real device**.

## What the engine does

| Area | Behavior |
|---|---|
| Render preset | `RenderProfilePreset::Deck`: 1280×800, temporal upscale from 70 %, 40 fps default cap. Measured on a Steam Deck on 2026-10-14 ([Auto quality](auto-quality.md)): the lab scene holds the cap with the GPU at about 10 ms of the 25 ms budget. |
| Frame caps | `DeckFrameCap`: 30, 40, 45, 60 fps. `DeckFrameCapSetting` (written by your settings menu) is applied to `FramePacing` on a Deck. The engine never paces above the display's refresh, so a lower SteamOS limiter wins instead of fighting the engine cap. `DeckFrameCap::refresh_for(panel)` names the refresh each cap runs at (LCD 40–60 Hz, OLED 45–90 Hz; 30 fps runs at 60 Hz, 40 fps at 80 Hz on the OLED). |
| UI scale | `ScaleMode::HANDHELD` (reference 1280×800): a UI authored at that size is scale 1.0 on the Deck and grows when docked. |
| Text size | `UiDevicePolicy.min_text_px` (12 px on a Deck, detected from Steam's `SteamDeck=1`): no text renders smaller, whatever the author set. Set the resource yourself to raise it (a "UI size" option). |
| Safe area | The Deck panel has none. `UiDevicePolicy.title_safe` adds an optional margin (fraction of the window) for TV overscan to roots with `SafeAreaPadding`. |
| Controller UI | D-pad and left stick move focus (stick repeats while held), A confirms, B cancels or closes the modal, LB/RB step through the focus order, left/right change a focused slider. `UiInputMode` says whether the last input was the pointer or navigation, so you draw the focus ring and button prompts only for navigation. Nothing needs the mouse: Steam Input's gamepad and keyboard-mouse templates both feed the same `UiNavActions`. |
| Sleep | winit reports no suspend on desktop Linux, so the engine watches `CLOCK_BOOTTIME` against `CLOCK_MONOTONIC`. A gap of 2 s or more is a sleep: the engine runs the same suspend → resume pair Android uses (surfaces dropped and recreated, audio output suspended and restarted, gameplay time not advanced, frame pacing restarted) and counts it in `SystemSleepStats`. The log says `system woke after N s asleep`. |

### Deck readiness helpers (ADR-368)

| Area | Behavior |
|---|---|
| Starting defaults | `DeckRenderDefaults::select(panel, power)`: 1280×800, render scale 0.7, the TSR upscaler, 40 fps on the LCD panel and 45 fps on the OLED panel. On battery below 15 % it starts at 30 fps with TAAU at scale 0.6. `DeckUpscaler::Fsr3` is the player's alternative. |
| Power and thermal hints | `ThermalGovernor` reads the battery and the hottest hwmon sensor every 300 frames and publishes `DeckPowerPolicy`: warm (80 °C) caps at 40 fps, hot (90 °C) at 30 fps, low battery at 30 fps. Two samples to step down, six samples 5 °C lower to step back up. The player's cap is only ever lowered. `render_scale_factor` is for the host to multiply into the render scale. The thresholds are first guesses. |
| Glyphs | `ActiveGlyphs` follows the connected pad's name (`ControllerKind::from_device_name`); the Deck's own virtual pad keeps the Deck glyphs. It reports `Steam Virtual Gamepad` through the engine's gamepad backend (`Microsoft X-Box 360 pad 0` through evdev), so on a Deck a pad that reads as Xbox or generic keeps the Deck glyphs (v0.2.5 showed Xbox glyphs for it; ADR-1750). `GlyphFamily::label(button)` gives the printed label and `texture_key(button)` the texture path `glyphs/<xbox\|playstation\|switch\|deck>/<key>`, where `<key>` is the label lowercased, spaces as `_`, `+` as `plus` and `-` as `minus` (`L Stick Press` becomes `l_stick_press`). |
| Keyboard | UI text widgets write `TextFieldFocus { request: Some(KeyboardRequest) }` on focus and `None` on blur; `drive_keyboard` shows or dismisses the floating keyboard through the backend. The real backend does not implement it yet (the call reports `Unavailable`). |
| Resume | After every resume the keyboard state is cleared, the governor's counters restart and the frame cap is applied again. |

### Deck prep 2: prompts, presets, 16:10 UI and FSR 1 (ADR-526)

| Area | Behavior |
|---|---|
| Action prompts | `action_prompts(&config, context, action, family)` gives one `Prompt { label, texture_key }` per gamepad control bound to the action, in binding order. Keyboard and mouse bindings are skipped. A stick axis draws the stick (`glyphs/<family>/left_stick`), a trigger axis draws the trigger, and both D-pad axes draw `glyphs/<family>/dpad`. Draw the texture when it loaded and the label otherwise. |
| Glyph sets | A complete family has 20 textures: 17 buttons, both sticks and the D-pad. `glyph_set_keys(family)` lists them. `missing_glyphs(asset_root, family, &GLYPH_EXTENSIONS)` names the ones your asset folder lacks (`.png`, `.ktx2` or `.webp`), so a test can fail before a player sees an Xbox letter on a Deck. |
| Performance presets | `DeckPerfSettings { preset, panel, drive_frame_cap }`, from `REGIA_DECK_PRESET` by default. Choosing a preset writes its cap into `DeckFrameCapSetting`; the player can still pick another cap. `DeckPerfState` holds the effective budget. Copy it onto your `RenderProfile` when it changes (below). |
| Thermal and battery | The preset's budget goes through the ADR-368 governor: the cap only goes down, and the render scale is multiplied by `render_scale_factor` (never below 0.5). |
| UI at 1280×800 | On a handheld (`UiDevicePolicy::HANDHELD`, `fit_reference: true`), a `ScaleWithScreen` root never scales past the size at which its reference fits the window. A UI authored for 1920×1080 runs at scale 0.667 on the 16:10 panel, full width, with 80 px to spare vertically. It is no longer clipped by 71 px at the sides. Text still has the 12 px floor. Desktops keep the authored blend. |
| Sleep and resume | Besides the ADR-213 and ADR-368 resets, the Steam Input bridge releases every held action and UI button on suspend and on resume, and starts a fresh Steam Input session ([Steam](steam.md)). |

The presets (LCD panel; on the OLED panel Balanced and FSR 1 run at 45 fps). The power limits are
starting points for the Deck pass, not measurements:

| Preset (`REGIA_DECK_PRESET`) | Cap | Render scale | Upscaler | Shadows | AO / contact | Lights (shadowed / visible) | Power limit hint |
|---|---|---|---|---|---|---|---|
| `battery` | 30 | 0.6 | TAAU | 2 × 1024 | off / off | 4 / 64 | 8 W |
| `balanced` (default) | 40 | 0.7 | TSR | 2 × 2048 | on / on | 8 / 96 | 12 W |
| `performance` | 60 | 0.6 | TSR | 2 × 2048 | on / off | 8 / 96 | default |
| `fsr1` | 40 | 1.0 of a 960×600 output | TAAU (TAA at scale 1) | 2 × 2048 | on / on | 8 / 96 | 12 W |

SteamOS sets the APU's power limit (quick-access menu, Performance, TDP limit). The game cannot set
it. A preset only decides how much work the game asks for, so that its cap holds under the hinted
limit. Battery saver is the starting preset below 15 % battery (`DeckPerfPreset::default_for`).

Copy the knobs onto the render profile when `DeckPerfState` changes:

```rust,ignore
fn apply_deck_perf(state: Res<DeckPerfState>, mut profile: ResMut<MyRenderProfile>) {
    if !state.is_changed() {
        return;
    }
    let k = state.knobs;
    let p = &mut profile.0; // a regia_render::RenderProfile
    p.render_scale = k.render_scale;
    p.temporal_upscaler = match k.upscaler.name() {
        "tsr" => TemporalUpscaler::Tsr,
        "fsr3" => TemporalUpscaler::Fsr3,
        _ => TemporalUpscaler::Taau,
    };
    p.csm_cascade_count = k.csm_cascade_count;
    p.csm_resolution = k.csm_resolution;
    p.ssao = if k.ssao { SsaoMode::HalfRes } else { SsaoMode::Off };
    p.contact_shadows = k.contact_shadows;
    p.bloom = k.bloom_mip_count > 0;
    p.bloom_mip_count = k.bloom_mip_count;
    p.local_shadow_max_lights = k.local_shadow_max_lights;
    p.max_visible_lights = k.max_visible_lights;
}
```

#### The FSR 1 profile (gamescope scaling)

The Deck's FSR 1 is gamescope's, not the engine's. The game renders a smaller 16:10 frame, and
gamescope upscales the whole output, UI included, to the panel with AMD's FSR 1 filter.

1. In Game Mode, open the game's Steam properties and set the game resolution to **960×600**.
   960×600 is 75 % of the panel per axis, close to FSR 1's "Ultra Quality" ratio of 1.3×. 1024×640
   saves less GPU time and looks sharper; 800×500 and 640×400 save more and look softer.
2. In the quick-access menu (Performance), set the scaling filter to **FSR** and choose a sharpness.
3. Add `REGIA_DECK_PRESET=fsr1 %command%` to the game's launch options. This step is optional.
   Without it the engine still fits the render scale.
4. In Desktop Mode, launch through gamescope instead:
   `gamescope -w 960 -h 600 -W 1280 -H 800 -F fsr -f -- %command%`. Older gamescope builds spell the
   filter `-U`.

What the engine does:
- It detects gamescope (`GAMESCOPE_WAYLAND_DISPLAY`) and sees a window smaller than the panel.
  `DeckDisplay::compositor_upscales` is then true.
- It raises the render scale so the frame is not scaled down twice. Balanced at 960×600 renders
  560 lines, as it would on the panel, instead of 420 lines that gamescope would stretch again.
- The `fsr1` preset renders the output at scale 1 with TAA. FSR 1 wants anti-aliased, low-noise
  input (AMD's FSR 1 guide).

The cost is that the UI is upscaled too. Text is softer, but bigger: the 12 px floor applies in
game pixels, so floor-sized text draws 16 px tall on the panel. Prefer `balanced` with the engine's
own TSR for sharp UI. Use `fsr1` when the GPU, not the CPU, holds the frame rate back.

### Deck 3: controller layouts and preset validation (ADR-1332)

| Area | Behavior |
|---|---|
| Controls screen | `layout_sheet(&config, context, family)` gives the rows of a "Controls" screen for one input context: one row per bound control, in a fixed order (face buttons, shoulders, triggers, each stick before its click, the D-pad, Start, Select, the system button), each with the family's prompt and the actions on it. `layout_sheets(&config, family)` does every context, highest priority first. `LayoutSheet::to_text` prints it. |
| Controller-only check | `check_layout(&config, &LayoutCheck)` reports `no_gamepad_binding` (error: an action with key or mouse bindings and no gamepad binding), `reserved_control` (the Steam / Guide / PS / Home button: the system menu takes it), `control_conflict` (two actions on one control with nothing to tell them apart) and, with `glyph_root`, `missing_glyphs`. List deliberate keyboard-only actions in `keyboard_only_ok`. |
| Glyphs on a Deck | A Deck starts on the Deck's glyphs before any pad reports in. Steam's virtual pad (named like an Xbox pad, or `Steam Virtual Gamepad`) keeps them; a DualSense, Switch pad or Steam Controller switches to its own (`ControllerKind::for_connected_pad`). The SDL names `PS4 Controller` and `PS5 Controller` now read as PlayStation. While the Steam Input bridge is active, the controller type Steam Input reports wins (`follow_steam_input_glyphs`): it knows the physical pad behind the virtual one. The real backend does not report the type yet, so for now the name decides on a real Deck. |
| D-pad prompts | An action bound to both D-pad axes gets one D-pad prompt, not two. |
| Preset validation | `vfx_lab --deck-presets` measures every performance preset on the device and judges each one (below). |

Run the check before a build goes to Valve's review, and in CI:

```console
$ regia steam layout --family deck --allow-keyboard-only Console --glyphs assets --check
Menu (deck)
  A  Accept
  B  Back
Gameplay (deck)
  A            Jump
  X            Reload, Interact
  LT           Aim
  RT           Fire
  Left Stick   Move
  Right Stick  Look
  Menu         Pause
  keyboard or mouse only: Console
no issues
```

The Deck's back grips (L4, R4, L5, R5) and trackpads are not part of the standard layout: players bind them to your
actions in Steam Input's configurator, through the in-game actions file (`regia steam input-manifest`).

A UI confirms with the south button and cancels with the east one on every pad (the positions, not the letters). On a
Nintendo pad that is B to confirm and A to go back, and the prompts say so, because the labels follow the position.

### Deck Verified checklist (Valve's public criteria, our status)

| Criterion | Status |
|---|---|
| Full controller support, default Steam Input template with glyphs | Engine: navigation, no mouse-only widgets, `regia steam layout --check` finds actions with no gamepad binding, `missing_glyphs` finds incomplete glyph sets. Host: ship the controller configuration and use the platform's glyphs. |
| All UI reachable and operable with the controller | Engine: yes (above). Host: put a `Focusable` on every interactive node and a `DefaultFocus` on every screen. |
| Text legible at the default font size on the 7" panel | Engine: 12 px floor. Host: judge on the device. |
| On-screen keyboard for text entry | Steam: `ShowFloatingGamepadTextInput` ([Steam](steam.md)). Host: call it when a text field takes focus in navigation mode. |
| Default resolution 1280×800 (or 16:9) | `Deck` preset. |
| Runs without a launcher, EULA or anti-cheat prompt needing a mouse | Host. |
| Survives sleep and resume | Engine: sleep detection above. **Needs the device test below.** |
| Runs in the Steam Linux Runtime 3.0 | Proven on a PC (`reports/steam-ship.md`); the Deck run is below. |

## The owner's test run

You need one binary, `vfx_lab`, built for the Steam runtime (glibc 2.31), and the Deck in Desktop
Mode with a terminal. Nothing is installed; the run writes one folder.

```console
$ chmod +x vfx_lab
$ ./vfx_lab --deck-check --out ~/deck-check          # 5 minutes, windowed 1280×800, 40 fps cap
```

It shows a lit ground, a sky and the full effect set cycling, 48 boxes moving on rings and a slow
camera path. At the end it prints one line (frames, average fps, p50 and p99 frame time, missed
deadlines, peak temperature) and the folder holds:

| File | What |
|---|---|
| `deck_check.json` | Frame statistics, GPU time per pass, the Deck sensors every 5 s (APU and GPU temperatures, GPU power, clocks, GPU busy, battery charge and power), OS and kernel, whether Game Mode is active, the screenshot list. |
| `summary.json` | The engine's own session record ([Telemetry](telemetry.md), spec 28): percentiles, hitches, memory, the quit timings (`exit`). `summary.partial.json` is the last 60 s snapshot; ignore it when `summary.json` exists. |
| `deck_*_NNNpct.png` | Four screenshots at 3 %, 30 %, 60 % and 95 % of the run. |

The run and its frame statistics start after the first frame: the first frame compiles the preset's pipelines (about
1 s on the Deck, 4 s with an empty shader cache) and is not counted (ADR-7387).

Do these runs, in this order, each into its own folder:

1. **Desktop Mode, defaults** (above). The reference run.
2. **Game Mode.** Add `vfx_lab` as a non-Steam game, launch options `--deck-check --fullscreen --out /home/deck/deck-check-gm`, and start it from the library. This is the real shipping condition: gamescope, the SteamOS frame limiter, the battery.
3. **Sleep.** Repeat run 2 with `--minutes 6`. About two minutes in, press the power button, wait 30 seconds, wake the Deck. The picture must come back, effects must keep playing, and there must be sound if you have sound on (the lab has none: check that no error dialog appears). The log must contain `system woke after`. Frame times right after the wake may show one long frame; the report's `max_ms` shows it.
4. **Caps** (one minute each, Game Mode or desktop): `--minutes 1 --cap 30`, `--cap 45`, `--cap 60`, and `--cap 0` (uncapped, to see the headroom). With the SteamOS limiter set to the same value, and once with it lower than the engine cap (for example limiter 30, `--cap 40`), frame times should stay even.
5. **Quit.** Each run quits through the clean exit; `summary.json` carries the step times. Note any run where the screen stayed black or the Deck needed a long wait after the quit.

6. **Presets** (about 12 minutes, after the runs above). Plug in the charger, close other apps, then in Game Mode
   (launch options `--deck-presets --fullscreen --out /home/deck/deck-presets`) or in Desktop Mode:

   ```console
   $ ./vfx_lab --deck-presets --fullscreen --out ~/deck-presets            # every preset, 2 minutes each
   $ ./vfx_lab --deck-presets --presets fsr1 --out ~/deck-presets-fsr1     # Game Mode at 960×600, scaling filter FSR
   ```

   It runs `vfx_lab --deck-check --preset <name>` for `battery`, `balanced`, `performance` and `fsr1` in turn, each in
   its own folder, with a 30-second pause between runs (`--cooldown`), then writes `deck_presets.json` and
   `deck_presets.md` and prints the table. On an OLED Deck add `--panel oled`; the report also says which panel the GPU
   names, and warns when the two differ. For the `fsr1` run set the game resolution to 960×600 and the scaling filter
   to FSR first (above); outside gamescope it warns `no_gamescope`.

Copy the folders back:

```console
$ tar czf deck-check.tgz -C ~ deck-check deck-check-gm
$ scp deck-check.tgz <your PC>:
```

### What happens with the numbers

The `steam_deck` row of [Auto quality](auto-quality.md) is **measured** (2026-10-14, Deck OLED: APU 0932, board Galileo, 90 Hz panel; this page said LCD until the 2026-10-29 pass. SteamOS 3.8.16, wall power, Plasma Wayland, windowed): five minutes of `vfx_lab --deck-check` at the Deck preset held the 40 fps cap with p50 25.0 / p95 27.3 / p99 29.1 ms, 0.52 % missed deadlines and a 53 °C peak. The GPU frame averaged 10.3 ms (forward 3.5, post 2.7, TAAU 1.5, shadows 1.0). Uncapped, the lab scene runs at 46 fps with the GPU still at 10 ms. That is two vblanks of the OLED's 90 Hz panel, not headroom (see the 2026-10-29 pass below). The lab scene is lighter than a game map, so repeat the run with your own content; if the 40 fps cap is not held, the preset's render scale or shadow settings change in `profile.rs` with the reason in the commit message, and the run is repeated.

## The Deck pass over SSH

The same checks, driven from a build machine over SSH while the Deck sits in Desktop Mode. The script is
`docs/evidence/deck/2026-11-05/deck-pass.sh` (the 2026-10-29 copy is the one the v0.2.5 pass ran). Everything goes under `/home/deck/regia-test`; nothing is installed
and no SteamOS, Steam or desktop setting changes. The script refuses to start while Game Mode (gamescope) or a
Steam game is running. During a window run it wakes the panel (a blanked panel gets no frame callbacks, which
changes the numbers) and holds a screensaver inhibition until the run ends, the way a video player does.

The binaries are a plain `cargo build --release` on any build machine whose glibc is not newer than the Deck's
(SteamOS 3.8 has glibc 2.41). Check with `objdump -T` as in the next section. A newer host does not work even for
plain Rust code: glibc 2.43 and 2.44 gave new symbol versions to libm functions (`atan2f`, `acosf`, `sinh`, `cosh` and
others), so a release build on a glibc 2.44 host needs `GLIBC_2.44` and does not start on the Deck. Copy them over:

```console
$ cargo build --release -p showcase -p regia_cli -p vfx_lab -p input_lab -p arena
$ cargo build --release -p ui_gallery --example ui_shots
$ cargo test --release -p stream_world --test acceptance --no-run   # prints the test binary's path
$ ssh <host> 'mkdir -p /home/deck/regia-test/bin'
$ scp target/release/{showcase,regia,vfx_lab,input_lab,arena} target/release/examples/ui_shots \
      docs/evidence/deck/2026-11-05/deck-pass.sh <host>:/home/deck/regia-test/bin/
$ scp target/release/deps/acceptance-<hash> <host>:/home/deck/regia-test/bin/stream_world_acceptance
$ scp -r samples/showcase/example_scene <host>:/home/deck/regia-test/scene
```

To compare against Xwayland, set `BACKEND=x11`. `TAG` keeps that run's results in their own folders:
`systemd-run --user --collect -E BACKEND=x11 -E TAG=x11 ...`.

Then run the pass. Start it as a transient user unit: a background job started over SSH is killed with the
SSH session when the connection closes. The unit is removed when it ends.

```console
$ ssh <host> 'systemd-run --user --collect --unit=regia-deck-pass \
      -p StandardOutput=append:/home/deck/regia-test/pass.log -p StandardError=append:/home/deck/regia-test/pass.log \
      bash /home/deck/regia-test/bin/deck-pass.sh all'
$ ssh <host> 'systemctl --user is-active regia-deck-pass; tail /home/deck/regia-test/pass.log'
```

Use `append:`, not `file:`: `file:` writes from the start of an existing log without truncating it, so a second unit
leaves the old lines mixed with the new. The whole pass takes about 60 minutes. A single phase runs the same way (`deck-pass.sh showcase 40 45`).

| Phase | What it does | Output under `out/` |
|---|---|---|
| `info` | OS, kernel, board (`Jupiter` is the LCD Deck, `Galileo` the OLED), glibc, the binaries' newest GLIBC symbol and `ldd`, `regia --version`, `vulkaninfo --summary`, the panel modes, power supplies, input devices. | `info.txt` |
| `hello` | Starts `showcase --fullscreen --render-profile Deck` and calls `agent.hello`, `loop.get`, `render.get_profile` and `telemetry.get_last_summary` through `regia agent call`. | `hello/` |
| `showcase [CAP...]` | One showcase run. For each cap (default 30 40 60), `loop.set { max_fps }`, a 20 s warm-up, then 300 s measured. It records a telemetry summary every 10 s and the sensors every 5 s: temperatures, APU slow and fast PPT, GPU clock and busy, VRAM and GTT, battery, fan, process and system CPU, RSS. Each cap ends with a window screenshot. At the end come `seeing.memory_report`, the session summary and the log. | `showcase/cap<N>_*.{jsonl,txt,png}` |
| `knobs` | One showcase run at a 45 fps cap (`KNOB_CAP`). It measures 60 s each (`KNOB_SECS`) of the Deck preset, render scale 0.588 and 0.5 (`render.set_upscaler { quality }`), and the DesktopLow and MobileHigh presets. Shows which setting fits the frame into the cap. | `knobs/<setting>_*` |
| `deckcheck` | `vfx_lab --deck-check --fullscreen` for 5 minutes at the default cap, then 1 minute each at 30, 45 and 60 fps and uncapped. | `deck-check/` |
| `memory` | The showcase at a 45 fps cap for 60 s, with `SteamDeck=1` (as Steam launches a game) and without: `render.get_profile`, `render.memory_pressure`, `render.memory_budget {"tier":"deck"}` (the `Deck` preset is DO27's MEDIUM tier), `seeing.memory_report`, and the kernel's VRAM and GTT use. | `memory-steam/`, `memory-desktop/` |
| `streambench` | stream_world's teleport bench (`bin/stream_world_acceptance`) at 1280×800 with the Deck's streaming budgets, the VRAM bound and residency wait off and on, `RUNS` (2) times each. Each JSON line carries the fill numbers and the VRAM numbers a bound is chosen from. | `streambench/` |
| `arena` | The arena sample fullscreen for 3600 frames (`ARENA_FRAMES`) with the sensors, then its headless budget probe (8 bots, 200 crates, about 20,000 particles). | `arena/` |
| `renderout` | A headless showcase and two `render.out_start` jobs: 60 PNG frames at 1280×800, then 60 at 1920×1080 with 2× supersampling, polled with `render.out_progress` until done. | `renderout/` |
| `uishots` | `ui_shots` renders the widget gallery and the HUD at 1280×800 (and 1080p, 1440p, 4K) on the Deck's GPU, with the 40 fps showcase frame as the HUD's backdrop. | `ui-shots/` |

The built-in controls cannot be pressed over SSH. With someone holding the Deck, run
`ssh <host> '/home/deck/regia-test/bin/input_lab --list-gamepads 60'` and press every button, stick, trigger
and the D-pad in turn. It prints one line per press, and a summary line with every button and axis it saw. The
engine names the built-in controls `Steam Virtual Gamepad`.

`SECS` and `WARM` change the measured and warm-up seconds. Copy the results back with
`scp -r <host>:/home/deck/regia-test/out .` and remove the folder when you are done:
`ssh <host> 'rm -rf /home/deck/regia-test'`.

### Results of the 2026-10-29 pass (v0.2.5, Deck OLED, Desktop Mode)

The numbers, the method and the files are in `docs/evidence/deck/2026-10-29/summary.md`. In short:

- v0.2.5 as tagged runs the showcase with the `Deck` profile (render scale 0.7) at 30 fps at every cap on the
  desktop. On Wayland each frame waited for the compositor before its CPU work started, so CPU and GPU added up to
  23.7 ms and landed on the third 90 Hz vblank.
- With that fixed, a focused window overlaps frames, and the same preset holds 30, 40, 45, 60 and 90 fps with no
  hitches. 45 fps has a p99 of 22.8 ms and the APU draws 6.3 W. 60 fps draws 7.5 W, and 90 fps draws 12.9 W with the
  edge at 71 °C and the fan at 4300 rpm.
- Caps 40 and 60 do not divide the OLED's 90 Hz. Their frame times are even at the engine, which does not prove even
  motion on the panel. Use 30, 45 or 90 on the desktop.
- `gpu_ms` follows the clock: 16.9 ms at 683 MHz, 10.7 ms at 1599 MHz. A reading at a low cap overstates the frame's
  cost.
- RSS was 250 MB, and engine VRAM 232 MB.
- `vfx_lab --deck-check` fullscreen, with the same fix, holds its 40 fps cap: p50 25.0, p99 25.9 ms, 0.52 % missed.
  Uncapped it reaches 87.9 fps. As tagged it stopped at 45.7.
- The first launch takes 15 s to the first frame with an empty pipeline cache, and 1.8 s afterwards.
- With `SteamDeck=1` the 12 px text floor applies. The sample HUD's soldier cards then ellipsize the names and
  clip one role line.
- ADR-1750 lists what to change and what still needs someone at the device: the buttons, Game Mode, sleep and
  battery runs.

### Results of the 2026-11-05 pass (v0.2.15, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-05/summary.md` and ADR-2952. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 16.9 and 12.4 ms and no
  hitches, the same as v0.2.5 with its Wayland fix. RSS is 300 to 325 MB, about 60 MB more than v0.2.5, and flat within
  each run.
- The `deck` memory tier (DO27 MEDIUM: 5 GiB RAM, 3 GiB VRAM) has room to spare. The showcase uses 330 MB and 238 MB, and the
  Deck's Vulkan device-local heap is 6 GiB.
- stream_world fills a 2 km teleport in 1.57 s with no holes, with the VRAM bound and residency wait off or on. Its cells
  carry no VRAM estimate, so the bound's default still waits for a world with cooked textures (ADR-2952 item 3).
- The agent hello, render-out, the arena smoke run, the UI shots and `vfx_lab --deck-check` all pass.
- A game that names no render preset gets Desktop High on a Deck unless it runs the quality detector, which picks `Deck`.
  Set `render_profile = "Deck"` or use the detector. (Fixed in v0.2.16, below.)
- The memory report no longer files the UI glyph atlas (10.5 MB) under `shadows`.

### Results of the 2026-11-06 pass (v0.2.16, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-06/summary.md` and ADR-3289. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.8, 16.9 and 12.4 ms and no
  hitches, the same as v0.2.15. `vfx_lab --deck-check`, memory, streaming, render-out, the arena and the UI shots pass.
- **A game that names no render preset now gets `Deck` on a Deck** (`RenderProfilePreset::runtime_default()`): when Steam
  sets `SteamDeck=1`, or from the DMI board name (`Valve Jupiter` or `Valve Galileo`) when it does not. That includes its
  40 fps cap: the arena sample, which names no preset, now draws 4.9 W on the Deck instead of 13.9 W. `render.get_profile`
  shows `runtime_default`. Cooks still use `platform_default()` (Desktop High), so a cook does not depend on where it runs.
- Build the Deck binaries on a host with glibc 2.41 or older (see above).

### Results of the 2026-11-07 pass (v0.2.17, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-07/summary.md` and ADR-3956. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 16.9 and 12.5 ms and no
  hitches, the same as v0.2.16. `vfx_lab --deck-check`, memory, streaming, render-out, the arena and the UI shots pass.
- The runtime default holds on v0.2.17: the arena, which names no preset, renders `Deck` at its 40 fps cap.

### Results of the 2026-11-10 pass (Regia Forest Demo 0.2.23 release candidate, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-10/summary.md` and ADR-5452. In short:

- The shipped (no-agent) build: `./run-demo.sh --check` exits 0, the window opens fullscreen, picks `Deck` by detection
  and draws the forest at its 40 fps cap; closing the window exits 0; it opens no socket and writes no agent endpoint.
- The agent-on twin of the same commit: agent ready in 0.9 s, `pipelines_outside` 0 on a cold cache, the gamepad
  actions move and turn the camera, screenshots are written, `quit.request` exits 0.
- The demo benchmark on `Deck` holds 30, 40 and 60 fps on all four views (p99 at most 36.2, 27.5 and 19.1 ms, no
  hitches). The low dynamic-GI tier costs 1.9 ms of GPU time on the demo scene (the `Deck` preset ships with it off);
  with it on, the demo still holds 40 fps.
- Keep the screen on while a test runs: with the panel blanked, every frame waits a second for the compositor.

#### Owner steps: Game Mode and sleep (not done over SSH)

Both need someone at the Deck: Game Mode needs a Steam change (a non-Steam shortcut), and nothing can wake the Deck
from SSH after a suspend.

1. In Desktop Mode, extract the release candidate: `tar xf regia-demo-0.2.23-linux.tar` (it makes
   `regia-deck-0.2.23/`).
2. Open Steam, **Games → Add a Non-Steam Game to My Library → Browse**, set the file filter to all files, pick
   `regia-deck-0.2.23/run-demo.sh`, and add it.
3. **Return to Gaming Mode**, find `run-demo.sh` under Non-Steam in the library, and press Play. Expect the loading screen
   ("REGIA FOREST DEMO - LOADING"), then the forest, with "Picked Deck" in the panel and about 40 fps in the overlay.
4. Controller: the left stick moves, the right stick looks, the shoulders and triggers fly up and down, View toggles
   the panel, Y takes a screenshot, Start runs the benchmark (the panel's status line shows each view).
5. **Sleep:** with the demo running, press the power button once, wait 30 s, press it again. The demo must come back
   drawing (no black or frozen frame, no crash) and the controller must still work.
6. Quit from the Steam button (**Exit game**). You should land back in the library.
7. Check `~/.local/share/RegiaEngine/showcase/logs/`: the newest session log is there and no `panic-*` file was written.
   If it is easy, send the session log: it records the sleep (`regia lifecycle` lines) and whether Steam set
   `SteamDeck=1`.
8. Repeat step 5 once in Desktop Mode with `./run-demo.sh --fullscreen` from Konsole. Remove the shortcut from the
   library afterwards if you want.
`vfx_lab --deck-validate <folder>` repeats the judgment on a folder copied back, on any machine. Each preset gets
`pass`, `warn` or `fail`:

| Code | Verdict | When |
|---|---|---|
| `missing_report` | fail | The preset's folder has no readable `deck_check.json`. |
| `wrong_preset` | fail | The report is another preset's run (or names none, for any preset but `balanced`). |
| `legacy_report` | warn | The report names no preset (a run from before `--preset`): judged as `balanced`. |
| `wrong_cap` | fail | The run was capped at another rate than the preset's cap, or uncapped. |
| `hidden_frames` | fail | Frames ran with the window hidden (screen blanked or covered): the run is not valid. |
| `cap_missed` | fail | The average frame rate is under 97 % of the cap. |
| `missed_deadlines` | fail | More than 2 % of frames took over 1.25 × the frame period. |
| `hitches` | warn | More than 0.2 % of frames took over 50 ms. |
| `hot` | warn | A sensor reached 90 °C, the governor's hot step. |
| `window_size` | warn | The window was more than 8 px from the preset's game resolution (use `--fullscreen` or Game Mode). |
| `no_gamescope` | warn | The `fsr1` run was not under gamescope, so gamescope's FSR 1 pass is not in the numbers. |
| `panel_mismatch` | warn | The knobs were chosen for one panel and the GPU names the other. |
| `over_power_hint` | warn | The mean amdgpu power is over the preset's power-limit hint. |
| `no_power_samples` | warn | The preset has a hint but the run has no power samples. |
| `order_gpu_time` | warn | `battery` or `performance` took more GPU time per frame than `balanced` (5 % slack). |
| `order_power` | warn | A cheaper preset drew more power than a dearer one (`battery` < `balanced` < `performance`, 5 % slack). |

The exit code is 1 when a preset fails. The power checks are warnings only: whether the amdgpu `power1_average` reading
is the package power the TDP slider limits is not confirmed yet. In the 2026-10-14 run it averaged 4.1 W with the GPU
busy reading 0 % and a 200 MHz clock, which looks like an idle reading, so treat it with care until the next Deck pass.

The `steam_deck` row of [Auto quality](auto-quality.md) is **measured** (2026-10-14, Deck LCD, SteamOS 3.8.16, wall power, Plasma Wayland): five minutes of `vfx_lab --deck-check` at the Deck preset held the 40 fps cap with p50 25.0 / p95 27.3 / p99 29.1 ms, 0.52 % missed deadlines and a 53 °C peak. The GPU frame averaged 10.3 ms (forward 3.5, post 2.7, TAAU 1.5, shadows 1.0). Uncapped, the lab scene runs at 46 fps with the GPU still at 10 ms, so the frame is not GPU bound. The lab scene is lighter than a game map, so repeat the run with your own content; if the 40 fps cap is not held, the preset's render scale or shadow settings change in `profile.rs` with the reason in the commit message, and the run is repeated.

### Results of the 2026-11-10 v047 pass (integration line, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-10-v047/summary.md` and ADR-5565. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 17.0 and 12.5 ms and no
  hitches, the same as v0.2.17. `vfx_lab --deck-check`, memory, render-out and the arena pass.
- The Forest demo (`showcase --demo`) picks `Deck` by detection, builds no pipeline outside the warm queue on a cold
  cache, and holds 30, 40, 45 and 60 fps on all four benchmark views with no hitches. It does not hold 90 (79.7 to 88.8
  fps on the views): the forest needs 11 to 12.8 ms of GPU time a frame. The preset's own cap is 40.
- `showcase SCENE --fullscreen --exit-after-frames N` now exits by itself (exit code 0), like the headless run and the
  windowed demo.
- To measure headroom on the `Deck` preset, set a large cap (`loop.set {"max_fps": 10000}`): `{"max_fps": null}` clears
  your cap and the preset's 40 fps cap applies again.

### Results of the 2026-11-11 v048 pass (integration line, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-11-v048/summary.md` and ADR-5858. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 16.9 and 12.4 ms and no
  hitches, the same as v047. `vfx_lab --deck-check`, memory, render-out and the arena pass.
- The Forest demo holds 30, 40, 45 and 60 fps on all four benchmark views. It does not hold 90 (GPU-bound, as in v047),
  and its `approach` view varies from 68 to 79 fps between runs there, so compare 90 fps rows over two or more runs. The
  preset's own cap is 40.

### Results of the 2026-11-12 v049-dx12 pass (integration line, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-12-v049dx12/summary.md` and ADR-7387. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 17.0 and 12.5 ms and no
  hitches, the same as v048. Memory, render-out and the arena pass.
- The Forest demo holds 30, 40, 45 and 60 fps on all four benchmark views and does not hold 90, as in v048.
- `vfx_lab --deck-check` on this line counted its start-up frame as a frame of the run (average 39.5 fps and a 3.9 s
  maximum at the 40 fps cap). With the ADR-7387 fix it reads 40.0 fps, p99 25.1 ms, 0.5 % missed.

### Results of the 2026-11-13 v050rc2 pass (integration line, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-13-v050rc2/summary.md` and ADR-7539. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 16.9 and 12.4 ms and no
  hitches, the same as v049-dx12. `vfx_lab --deck-check` (40.0 fps at the 40 cap), memory, render-out and the arena pass.
- The Forest demo holds 30, 40, 45 and 60 fps on all four benchmark views and does not hold 90, as in v049-dx12.
- The line's final head (`7c61f9da9`) was run again with shorter windows: the same rows pass, and render-out writes
  byte-identical frames.

### Results of the 2026-11-14 v050rc5 pass (integration line, Deck OLED, Desktop Mode)

The numbers are in `docs/evidence/deck/2026-11-14-v050rc5/summary.md` and ADR-7688. In short:

- The showcase with the `Deck` preset holds 30, 40, 45, 60 and 90 fps, with p99 33.6, 25.2, 22.6, 17.0 and 12.6 ms and no
  hitches, the same as v050rc4. `vfx_lab --deck-check` (40.0 fps at the 40 cap), agent hello, memory, the arena and
  render-out pass; render-out writes the same frames as v050rc4.
- The Forest demo holds 30, 40, 45 and 60 fps on all four benchmark views and does not hold 90, as in v050rc4. In the
  first run three views at 40 and 45 dropped while another job's SSH session loaded the Deck (every stall was the
  compositor's image handback); two more cold runs with nothing else on the Deck held every cap on every view.

## Building the test binary

On a build machine with `cargo-zigbuild` and zig 0.14+:

```console
$ cargo zigbuild --release -p vfx_lab --bin vfx_lab --target x86_64-unknown-linux-gnu.2.31
$ objdump -T target/x86_64-unknown-linux-gnu/release/vfx_lab | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1
```

The last line must not exceed `GLIBC_2.31`.

Related: [Steam](steam.md), [Packaging](packaging.md), [UI](ui.md), [Auto quality](auto-quality.md).
