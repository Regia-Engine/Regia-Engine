# Accessibility review (UI and input)

Status: **draft for owner sign-off** (lane sprint-accessibility-review, 2026-11-05, ADR-2646). This is the review
behind the v1.0 checklist row "Accessibility (UI and input) reviewed" (`docs/versions.md`). The lane does not tick
that row; the owner does, after reading this page.

## Scope and method

The engine (Regia Engine on `integ/v035` plus this lane) is checked against two public lists:

- **Game Accessibility Guidelines** (GAG), the *basic* and *intermediate* sets:
  <https://gameaccessibilityguidelines.com/basic/>, <https://gameaccessibilityguidelines.com/intermediate/>.
- **WCAG 2.2**, the level A and AA success criteria that apply to a game's menus and HUD:
  <https://www.w3.org/TR/WCAG22/>.

Guidelines are paraphrased here, never quoted; follow the links for the wording. Many guidelines are design
decisions that only a game can make (difficulty, tutorials, language). Those rows say what the engine gives the
game to make them easy. The GAG list was compiled from the published sets as the reviewer knows them; the lane
could not fetch the pages while writing, so the owner should check the list and levels against the live pages
(the site revises them).

Each row gives one of:

- **Free**: a game gets it without writing code (it may still need a setting exposed in its menu).
- **API**: the engine provides it; the game must call or configure something (named).
- **Game**: a design choice the engine cannot make; what the engine offers is noted.
- **Partial**: some of it exists; the gap is named.
- **Not supported**: nothing in the engine; the gap is named.

Evidence is a `file:line`, a test name, an agent method or a book page. Paths are relative to the repository
root. Items new in this lane are marked *(new)*.

## 1. Game Accessibility Guidelines: motor

| Guideline (paraphrased) | Level | Status | What the engine provides | Evidence | Gap |
|---|---|---|---|---|---|
| Let players remap or reconfigure controls | Basic | API | Runtime rebinding of keys, buttons and axes, saved at once to `user://input_overrides.ron`, named profiles, a ready rebind screen (`RebindPanel`). | `crates/regia_input/src/rebind.rs` (`InputRebinder`, `InputOverrides`, `save_profile`); test `profiles_save_list_load_and_become_active` (`crates/regia_input/tests/rebind_axes.rs`); `input.rebind*`; `book/agents/input-rebind.md` | None for single-player. |
| Make controls as simple as possible, or offer a simpler scheme | Basic | Game | Contexts, chords and triggers let a game ship a reduced scheme; input assists *(new)* turn holds into toggles and mashing into holds. | `crates/regia_input/src/config.rs` (`Trigger`, `InputContext`); `crates/regia_input/src/assist.rs` | Scheme design is the game's. |
| Every part of the UI is reachable with the same input as gameplay | Basic | Free | Keyboard and gamepad focus navigation with directional moves, tab order, a navigation lint for unreachable nodes and dead ends; sliders step with left/right. | `crates/regia_ui/src/focus_graph.rs:85` (`FocusGroup`), `:1186` (`nav_move`), `:416` (`NavIssueKind`); tests `dpad_moves_focus_through_a_menu_and_a_b_confirm_and_back` (`crates/regia_ui/tests/suite/gamepad_nav.rs`), `lint_finds_unreachable_dead_ends_and_bad_links`; `ui.nav_*`; `book/agents/ui-navigation.md` | None. |
| Let players adjust control sensitivity | Basic | API | Per-action scale and response curves (`Modifier::Scale`, `ResponseCurve`); per-device axis invert and dead zone through rebinding. `GameSettings.input.sensitivity` and `invert_y` scale the actions named in `InputConfig.look_actions` (mouse and gamepad). An empty list leaves both fields for the game to read. | `crates/regia_input/src/config.rs` (`LookSettings`, `InputConfig::look_actions`); tests `look_sensitivity_and_invert_y_scale_mouse_and_gamepad_when_the_action_is_named`, `unset_look_actions_leave_sensitivity_and_invert_y_unread` (`crates/regia_input/tests/look_settings.rs`) | Closed (gap 4, ADR-3464). |
| Support more than one input device | Basic | Free | Keyboard, mouse, gamepads (gilrs on desktop, a native Android backend), touch, virtual touch controls, per-player device assignment. | `crates/regia_input/src/gamepad.rs:541` (`poll_android_gamepads`), `crates/regia_input/src/touch_controls.rs:158` (`TouchControls`); tests `face_shoulder_codes_map`, `output_drives_the_injection_like_a_virtual_control`, `per_player_device_assignment_filters_sources` (`crates/regia_input/tests/actions.rs`); `book/user-guide/input.md` | None. |
| Make interactive elements large and well spaced, especially on touch screens | Basic | Partial | A 44 UI px `size.touch-min` theme token; the theme linter flags smaller controls in the `comfortable` density. | `crates/regia_ui/src/theme/lint.rs:38` (`LintKind::TouchTarget`); test `touch_targets_under_the_minimum_are_flagged_in_comfortable_only`; `ui.theme_lint` | A lint, not a layout floor; the `compact` density is exempt (gap 12). |
| Avoid or offer alternatives to holding buttons down | Intermediate | API *(new)* | Per-action `PressMode::Toggle`: one press latches the action on, the next turns it off, on any device. Persisted in `GameSettings.accessibility.input_assists`, with per-player and per-gamepad overlays. | `crates/regia_input/src/assist.rs` (`PressMode`, `InputAssists`); tests `toggle_mode_latches_on_a_press_and_releases_on_the_next` (`crates/regia_input/tests/assists.rs`), `a_players_toggle_changes_only_that_players_stream`; `input.assists`; `book/agents/input-assists.md` | The game must list which actions its menu offers as toggles. Per-player overlays closed (gap 9, ADR-3464). |
| Avoid repeated inputs (button mashing, rapid quick-time presses) | Intermediate | API *(new)* | Per-action `repeat`: while held, the action fires again after a delay and then at an interval, so holding replaces mashing. | `crates/regia_input/src/assist.rs:66` (`RepeatRate`); tests `repeat_turns_one_long_hold_into_regular_presses`, `a_long_frame_fires_one_repeat_not_a_burst` | None engine-side. |
| Allow for input with tremor (ignore short accidental presses; a delay between accepted inputs) | Intermediate | API | Per-action `min_hold_secs`: presses shorter than the minimum never reach the action. Per-action `cooldown_steps`: a press that starts inside that many evaluations after an accepted press is ignored. | tests `a_minimum_hold_ignores_bumps`, `cooldown_steps_drop_presses_inside_the_gap` (`crates/regia_input/tests/assists.rs`), `cooldown_steps_drop_a_press_that_starts_inside_the_gap` (`crates/regia_input/src/assist.rs`) | Closed (gap 9, ADR-3464). |
| Do not require precise timing; offer a slower game speed | Intermediate | API | `Time::time_scale` and `Time::paused` slow or pause gameplay while the UI runs on real time. | `crates/regia_app/src/time.rs:90`; tests `time_scale_scales_delta`, `pause_and_suspend_freeze_gameplay_time`; `play.set_time_scale` | Not a player setting; the game exposes it. |
| Keep accuracy-dependent targets still; avoid precise pointer work | Intermediate | Game | Activation happens on release over the same node it was pressed on (a slipped press cancels); sliders step without dragging; a software cursor for gamepads. | `crates/regia_ui/src/interaction.rs:604`; test `left_right_changes_a_focused_slider_and_a_does_not_jump_it`; `crates/regia_ui/src/soft_cursor.rs:40` | No direct test of release-to-activate (gap 13). |
| Keep touch gestures simple; offer alternatives to multi-touch | Intermediate | Partial | Gesture recognizer and virtual controls. | `crates/regia_ui/src/gesture.rs:143`; `book/agents/ui-touch.md` | Nothing enforces a non-gesture alternative; game design. |
| Assist options (aim assist, assisted steering, skips) | Intermediate | Not supported | None. | searched `aim_assist`, `auto_aim`, `difficulty` | Genre-specific; listed as a framework gap (gap 14). |
| Haptics can be turned off or down | (common practice) | API *(new)* | `Gamepads::set_rumble_scale` scales every rumble request, 0 = off; `PhoneVibration` scales `Haptics::vibrate` the same way. Both persist in `GameSettings.accessibility`. | `crates/regia_input/src/gamepad.rs` (`set_rumble_scale`, `PhoneVibration`); tests `the_rumble_scale_turns_every_request_down_or_off`, `phone_vibration_scales_amplitude_and_desktop_accepts_it`, `haptics_sets_phone_vibration_scale` (`crates/regia_agent/src/input_haptics.rs`); `input.haptics` | Closed for gamepad rumble and phone vibration (gap 6, ADR-3464). |

## 2. Game Accessibility Guidelines: cognitive

| Guideline (paraphrased) | Level | Status | What the engine provides | Evidence | Gap |
|---|---|---|---|---|---|
| Use an easily readable default font size | Basic | Free | Theme type scale; a device text floor (12 px on Steam Deck). | `crates/regia_ui/src/scale.rs:100` (`HANDHELD_MIN_TEXT_PX`), `:106` (`min_text_px`) | Desktop floor is 0 (the game's theme decides). |
| Use simple, clear language and text formatting | Basic | Game | Localization tables, keys and plural rules. | `book/user-guide/localization.md` | Writing is the game's. |
| Include tutorials; remind players of objectives and controls | Basic / Intermediate | Game | Binding labels and glyphs for the current device (`binding_label`, `LastUsedDevice`) so on-screen prompts follow rebinding. | `crates/regia_input/src/rebind.rs` (`binding_label`, `source_label`) | None engine-side. |
| Start the game without deep menus | Basic | Game | — | — | Game design. |
| Let players progress text at their own pace | Intermediate | Partial | Caption display time follows a player reading speed (`CaptionOptions::reading_cps`). | `crates/regia_ui/src/captions/options.rs:116` | Advancing dialogue on input is the game's; no engine-wide "wait for input" option was reviewed. |
| Offer a way to turn off or reduce background movement | Intermediate | API | `reduced_motion` makes UI tweens, theme fades and motion presets jump to their end, and sets the effective camera-shake scale and head-bob scale to 0 and forces motion blur off. | `crates/regia_framework/src/settings.rs` (`AccessibilitySettings::reduced_motion`, `SceneMotion`); tests `reduced_motion_jumps_transforms_and_caps_fades_at_motion_fast` (`crates/regia_ui/src/motion.rs`), `scene_motion_scale_defaults_and_reduced_motion_zeroes_shake_and_bob`, `player_shake_scale_scales_the_pose_and_leaves_authored_gain`, `reduced_motion_scale_stops_handheld_sway_and_bob`, `reduced_motion_scale_stops_the_walk_bob`, `motion_blur_follows_the_player_and_reduced_motion_forces_it_off` | Closed for shake, handheld sway and bob, walk bob, and motion blur (gap 2, ADR-3464). Field-of-view kicks stay authored. |
| Practise without failure; skip non-core sections; adjustable difficulty | Intermediate | Game | — | — | Game design. |
| Ensure all settings are saved | Basic (general) | Free | `GameSettings` written atomically to `user://settings.ron` (rate-limited, again on exit); corrupt files boot on defaults. Master, Music, Sfx, Voice and Ambience volumes and their mutes persist. | `crates/regia_framework/src/settings.rs` (`SettingsPersistence`, `AudioBusSettings`); tests `defaults_persist_and_reload`, `corrupt_file_boots_on_defaults`, `voice_ambience_and_mutes_round_trip_and_apply_live` | **Android**: persistence is off until the shell sets `SettingsPath`. World and Ui buses stay runtime-only (gap 5 closed for Voice, Ambience and the mutes, ADR-3464). |

## 3. Game Accessibility Guidelines: vision

| Guideline (paraphrased) | Level | Status | What the engine provides | Evidence | Gap |
|---|---|---|---|---|---|
| Do not carry essential information by color alone | Basic | Game + API *(new)* | `render.color_vision` **simulate** shows the game as a player with protanopia, deuteranopia or tritanopia sees it, so a designer can check. | `crates/regia_render/src/color_vision.rs:137` (`ColorVisionFilter`); tests `red_and_green_collapse_to_one_hue_for_protan_and_deutan_but_not_tritan`, `the_published_matrices_keep_white_and_gray`; `book/agents/color-vision.md` | The UI is not filtered (gap 8). |
| Offer a color-blind option | (common practice) | API *(new)* | **compensate** mode shifts colors the player confuses into ones they can tell apart; persisted in `GameSettings.accessibility.color_vision`. | test `compensation_separates_a_pair_the_simulation_confuses`; `crates/regia_render/src/color_vision.rs:351` (`apply_color_vision`) | The UI is not filtered; no visual check on a real scene yet (lead). |
| High contrast between text/UI and background | Basic | Free | WCAG contrast computation; the theme linter checks text at 4.5:1 and boundaries at 3:1; captions refuse colors under 4.5:1; a `high_contrast` setting switches themes. | `crates/regia_ui/src/theme/color_ops.rs:124` (`contrast_ratio`); `crates/regia_ui/src/captions/options.rs:57` (`MIN_CONTRAST`); tests `contrast_ratio_matches_wcag_extremes_and_symmetry`, `the_players_high_contrast_setting_picks_the_high_contrast_theme`; `ui.accessibility` | None for the UI. No contrast option for the 3D scene (game). |
| Let players adjust the font size | Intermediate | Free | `text_scale` 0.5 to 3.0, re-shapes and re-lays out all text; persisted. | `crates/regia_ui/src/scale.rs:194`; test `text_scale_is_clamped_and_the_device_floor_still_applies` (`crates/regia_ui/tests/accessibility.rs`); `ui.accessibility` | **No whole-UI scale** setting (icons, controls) (gap 7). |
| Screen-reader support for menus | Intermediate | API | Accessibility tree from widgets (roles, names, values, states), live regions, announcements; the `accesskit` feature bridges to AT-SPI (Linux), UI Automation (Windows), NSAccessibility (macOS) and TalkBack (Android, ADR-3454). | `crates/regia_ui/src/a11y.rs:219` (`Accessible`), `:1412` (`ScreenReader::announce`); tests `the_tree_reads_like_the_screen` (`crates/regia_ui/tests/a11y_tree.rs`), `announcements_change_the_live_region_label`; `ui.a11y_*`; `book/user-guide/accessibility.md` | Off by default; Android TalkBack adapter is in (ADR-3454; a device check is still open); **no Steam Deck game-mode path**; not yet run against a real reader (gap 3). |
| Avoid placing essential temporary information outside the player's eye line | Intermediate | Game | Anchors and safe areas. | `book/agents/ui-anchors.md` | Layout is the game's. |
| Choice of crosshair or cursor color and size | Intermediate | Not supported | The soft cursor has speed and dead zone only. | `crates/regia_ui/src/soft_cursor.rs:40` | No reticle helper (gap 10). |
| Adjustable field of view | Intermediate | API | `AccessibilitySettings.fov_override_deg` replaces the gameplay camera's field of view (clamped 1 to 170 degrees). `None` keeps the camera's value. Shots and the sequencer keep theirs. | `crates/regia_framework/src/settings.rs` (`SceneMotion`); tests `player_fov_override_clamps_and_none_keeps_the_authored_value`, `player_fov_override_replaces_the_gameplay_camera_fov` | Closed for the gameplay camera (gap 2, ADR-3464). |
| Use surround sound / spatial audio | Intermediate | Free | Spatial audio, HRTF, occlusion. | `book/agents/audio.md` | None. |

## 4. Game Accessibility Guidelines: hearing

| Guideline (paraphrased) | Level | Status | What the engine provides | Evidence | Gap |
|---|---|---|---|---|---|
| Separate volume controls (effects, speech, music) | Basic | API | Bus tree Master, Music, Sfx, World, Ui, Voice, Ambience with volume and mute at runtime. Master, Music, Sfx, Voice and Ambience volumes and mutes persist in `GameSettings.audio` and apply on load and on change. | `crates/regia_framework/src/settings.rs` (`AudioBusSettings`); tests `audio_bus_settings_default_to_full_volume_and_apply_live`, `voice_ambience_and_mutes_round_trip_and_apply_live`; `audio.set_bus` | Closed (gap 5, ADR-3464). World and Ui stay runtime-only. |
| Subtitles for all important speech | Basic | Free | `SubtitleMode::{Off, Dialogue, All}`, caption presenter, speaker labels. | `crates/regia_framework/src/settings.rs:491`; tests `labels_follow_the_option` (`crates/regia_ui/src/captions/tests.rs`), `subtitle_mode_derives_from_subtitles_and_keeps_it_in_sync`; `captions.*`; `book/agents/dialogue-subtitles.md` | None. |
| Present subtitles clearly (size, background, line length) | Basic | Free | `CaptionOptions`: text scale, background type and opacity, colors (refused under 4.5:1), position; at most 2 lines (3 at large text) and 62 % of the width. | `crates/regia_ui/src/captions/options.rs:96`, `:299` (`CaptionPolicy`) | No character-count limit (the width limit stands in). |
| Do not carry essential information by sound alone; caption important sounds | Basic / Intermediate | Free | `SubtitleMode::All` adds sound captions (`[door slams]`) that never evict speech. | test `sound_captions_never_evict_speech_and_need_caption_mode` | **No direction indicator** on sound captions (gap 11). |
| Follow the system captions preference | (common practice) | Partial | First run follows the OS captions setting. | `crates/regia_framework/src/settings.rs:528`; test `a_first_run_follows_the_system_captions_preference` | Windows only; Linux and macOS report nothing (gap 11). |
| Stereo/mono toggle | Intermediate | API *(new)* | `Audio::set_mono_output` downmixes to `(L + R) / 2` before the master limiter; persisted in `GameSettings.accessibility.mono_audio`. | `crates/regia_audio/src/audio/mix.rs:95`, `crates/regia_audio/src/limiter.rs:274`; tests `mono_sums_both_channels_into_each_and_off_is_bit_exact`, `mono_works_with_the_limiter_bypassed_and_silences_non_finite_first`; `audio.mono`; `book/agents/audio-mono.md` | None. |
| Visual alternative to voice chat (text chat, transcription) | Intermediate | Partial | Lobby text chat. | `crates/regia_net/src/lobby/plugin.rs:439` (`send_chat`); `lobby.chat` | **No speech-to-text** for voice chat (gap 15). |

## 5. Game Accessibility Guidelines: speech and general

| Guideline (paraphrased) | Level | Status | What the engine provides | Evidence | Gap |
|---|---|---|---|---|---|
| Speech input is never required | Intermediate | Free | The engine has no speech-input path; voice chat is optional and push-to-talk by default. | `book/agents/voice-chat.md` | None. |
| Avoid flickering images and repetitive patterns | Basic | Not supported | Nothing measures or limits flashes. | searched `photosensitiv`, `strobe`, `flash` | **No flash analysis or limiter** (gap 1). |
| Offer a choice of difficulty | Basic | Game | Curve tables can drive difficulty ramps. | `crates/regia_framework/src/curve_table.rs` | Game design. |
| Describe accessibility features in-game and on the store page | Basic | Game | This page and `book/user-guide/accessibility.md` list what a game built on the engine can claim. | — | Store text is the game's. |
| Settings changeable during play | (common practice) | Free | Every setting applies live (`SettingsChanged`). | `crates/regia_framework/src/settings.rs` (`apply_and_persist_settings`); test `accessibility_assists_reach_input_audio_and_render_live` *(new)* | None. |
| Language choice | (common practice) | Free | `GameSettings.language`, audio language, right-to-left layout. | `crates/regia_ui/src/style.rs:389` (`locale_is_rtl`); test `rtl_text_dir_lays_a_row_out_right_to_left` | None. |

## 6. WCAG 2.2, level A and AA, as they apply to game UI

Criteria about web documents only (page titles, link purpose, multiple ways to find a page, redundant form entry,
authentication) are left out.

| Criterion | Level | Status | Evidence | Gap |
|---|---|---|---|---|
| 1.1.1 Non-text content has a text alternative | A | API | `Accessible::with_label` / `with_label_key`, `Accessible::decorative()`; test `the_tree_reads_like_the_screen` | Icon buttons need a label from the game (checklist in `book/user-guide/accessibility.md`). |
| 1.3.1 Info and relationships | A | Free | Roles from widgets, label children, dialogs; `crates/regia_ui/src/a11y.rs:55` | None. |
| 1.3.4 Orientation not locked unless essential | AA | Game | Android manifest orientation is a project choice (`book/user-guide/android.md`) | Game choice. |
| 1.4.1 Color is not the only means of conveying information | A | Game + API *(new)* | `render.color_vision` simulate for the 3D view; theme states use more than color | UI not filtered by the simulation (gap 8). |
| 1.4.3 Text contrast at least 4.5:1 | AA | Free | Theme lint and caption checks (above) | None. |
| 1.4.4 Text resizes to 200 % without loss | AA | Free | `text_scale` up to 3.0 with re-layout | No overflow test at 200 % across the default theme (gap 7). |
| 1.4.11 Non-text contrast at least 3:1 | AA | Free | Theme lint boundary check; focus ring tokens | None. |
| 1.4.13 Content on hover or focus is dismissable and persistent | AA | Partial | Tooltips; `book/agents/ui-tooltips.md` | Not reviewed in depth. |
| 2.1.1 Everything works from a keyboard | A | Free | Focus navigation and nav lint (above) | None. |
| 2.1.2 No keyboard trap | A | Free | Test `closed_modal_scope_does_not_trap_focus` (`crates/regia_ui/src/focus.rs:767`); modals are deliberate traps that close with Back | None. |
| 2.1.4 Single-key shortcuts can be turned off or remapped | A | API | Rebinding covers gameplay; UI shortcuts registry (`ui.shortcuts`) | Remapping of UI shortcuts not reviewed. |
| 2.2.1 Time limits can be adjusted | A | API | `Time::time_scale` | Game exposes it. |
| 2.2.2 Moving content can be paused or stopped | A | API | `reduced_motion` for UI, and for camera shake, handheld sway and walk bob, and motion blur | Field-of-view kicks stay authored (gap 2 closed for the listed effects, ADR-3464). |
| 2.3.1 No more than three flashes a second | A | Not supported | — | Gap 1. |
| 2.4.3 Focus order is meaningful | A | Free | Tab order rules; test `tab_order_puts_positive_indices_first_and_skips_negative_ones` | None. |
| 2.4.7 Focus visible | AA | Free | `FocusRingPolicy` (`crates/regia_ui/src/focus.rs:138`); test `the_ring_uses_the_focus_ring_tokens_and_follows_radius_plus_offset` | The default shows the ring only during keyboard or gamepad navigation; `Always` is the option. |
| 2.4.11 Focused item not hidden | AA | Partial | Scroll views scroll to focus | Not tested against overlays. |
| 2.5.1 Path and multi-point gestures have single-pointer alternatives | A | Partial | Gestures optional | Not enforced (game). |
| 2.5.2 Pointer cancellation (activate on release) | A | Free | `crates/regia_ui/src/interaction.rs:604` | No direct test (gap 13). |
| 2.5.3 Visible label is part of the accessible name | A | Free | Name from the node's own text or label children | None. |
| 2.5.7 Dragging has a single-pointer alternative | AA | Free | Sliders step by keys, gamepad and screen-reader raise/lower; test `raise_and_lower_step_a_slider_by_its_own_step_and_stop_at_the_ends` | None. |
| 2.5.8 Target size at least 24 by 24 CSS px | AA | Partial | 44 UI px touch-min lint in `comfortable` | Compact density exempt (gap 12). |
| 3.1.1 Language of the UI is known | A | Free | Locale reported to the tree and `ui.accessibility` | None. |
| 3.2.1 / 3.2.2 Focus or input does not change context unexpectedly | A | Free | Activation needs a press; dropdowns open on press | None. |
| 3.3.1 Input errors are identified in text | A | Partial | `Invalid` marker reaches the tree (`crates/regia_ui/src/a11y.rs:860`) | No error-message link on text fields; no test that `invalid` reaches a reader (gap 13). |
| 4.1.2 Name, role, value | A | Free | Accessibility tree and bridge (above) | Steam Deck game mode (gap 3). Android TalkBack is ADR-3454. |
| 4.1.3 Status messages | AA | API | Live regions, `ScreenReader::announce`; test `announcements_queued_in_one_frame_keep_only_the_newest_up_to_the_cap` | None. |

## 7. Ranked gaps

Ranked by player harm, then by how cheaply the engine can close them. Items 1 to 4 should be closed or explicitly
accepted before v1.0.

1. **Photosensitivity: dev-time flash check built; runtime limiter still open** (GAG flicker, WCAG 2.3.1).
   `regia_core::flash` analyzes a sequence of sRGB frames (general flash and saturated-red flash, one-second
   windows, a 10 degree field scaled from 341×256 px on a 1024×768 screen). Teams run it before release with
   `seeing.flash_check` or `cargo xtask flash-check` (ADR-3804). It is an aid, not a certification. Tests:
   `full_screen_black_white_at_2_hz_passes_and_at_4_hz_fails`,
   `a_region_below_the_area_threshold_passes_and_above_it_fails`,
   `flicker_between_two_bright_levels_passes_the_darker_image_rule`,
   `a_saturated_red_flash_fails_while_the_same_luminance_change_in_gray_passes`,
   `a_smooth_fade_over_one_second_passes`,
   `the_same_square_wave_at_30_and_60_fps_has_the_same_verdict`,
   `a_sliding_window_catches_a_burst_that_straddles_two_seconds`.
   The runtime limiter for engine effects (lightning, muzzle flash, explosions) is not built: it needs
   `regia_render` / `regia_vfx` changes. CPU work, no hardware.
2. **Scene motion** *(closed, ADR-3464).* `AccessibilitySettings` gained `camera_shake_scale` (0 to 1, default 1),
   `motion_blur` (`Option<bool>` on accessibility, not graphics), `fov_override_deg` (degrees, clamped 1 to 170,
   `None` keeps the camera's value) and the reduced-motion path into the 3D scene. `reduced_motion` sets the
   effective shake scale and the head-bob scale to 0 and forces motion blur off, without writing the stored shake
   scale. The shake pose is multiplied in `ShakeSensor::effective_gain_scaled`; authored `gain` is not rewritten.
   Framework effects that animate on their own: camera shake, handheld sway and breathing, and the walk bob honor
   it. Field-of-view kicks (rig fov nodes, shot camera fov, sequencer `camera.fov`) stay authored; the gameplay
   driver replaces fov only when the override is set, and shots and the sequencer are not overridden. `Some(true)`
   motion blur only sets the allow flag; a velocity target is still required. Tests:
   `scene_motion_scale_defaults_and_reduced_motion_zeroes_shake_and_bob`,
   `motion_blur_follows_the_player_and_reduced_motion_forces_it_off`,
   `player_fov_override_clamps_and_none_keeps_the_authored_value`,
   `player_shake_scale_scales_the_pose_and_leaves_authored_gain`,
   `reduced_motion_scale_stops_handheld_sway_and_bob`,
   `reduced_motion_scale_stops_the_walk_bob`,
   `player_fov_override_replaces_the_gameplay_camera_fov`,
   `settings_set_writes_scene_motion_and_bus_fields`.
3. **Screen reader on Steam Deck game mode**, and no run against a real reader yet (Orca, Narrator, NVDA,
   VoiceOver, or TalkBack on a phone). Android TalkBack is ADR-3454; a device check is still open. Steam Deck
   game mode has no screen reader (desktop mode is AT-SPI). Needs devices.
4. **Look sensitivity and invert** *(closed, ADR-3464).* `InputConfig.look_actions` names the actions
   `sensitivity` and `invert_y` scale (mouse deltas and gamepad sticks, after the bindings combine and before
   injection). An empty list, the default and older input files, leaves both fields for the game to read. Tests:
   `look_sensitivity_and_invert_y_scale_mouse_and_gamepad_when_the_action_is_named`,
   `unset_look_actions_leave_sensitivity_and_invert_y_unread`,
   `look_and_vibration_scales_copy_when_they_change_and_a_host_value_survives_defaults`.
5. **Per-bus volume persistence** *(closed, ADR-3464).* Voice and Ambience volumes and the mutes of Master, Music,
   Sfx, Voice and Ambience are saved in `GameSettings.audio` and applied on load and on change. Older settings
   files load full volume and mutes off (`serde` defaults). `GameSettings` is a player data file (`user://settings.ron`),
   not a frozen content format under `docs/specs/12-assets.md` § 4a, and it has no `format_version`. World and Ui
   stay runtime-only. Tests: `voice_ambience_and_mutes_round_trip_and_apply_live`,
   `settings_set_writes_scene_motion_and_bus_fields`.
6. *(closed: gamepad rumble intensity, `input.haptics`.)* **Phone vibration** *(closed, ADR-3464).*
   `AccessibilitySettings.vibration_scale` (0 to 1, default 1) scales `PhoneVibration::vibrate`. Reduced motion
   does not change it. Tests: `phone_vibration_scales_amplitude_and_desktop_accepts_it`,
   `haptics_sets_phone_vibration_scale`,
   `look_and_vibration_scales_copy_when_they_change_and_a_host_value_survives_defaults`.
7. **Whole-UI scale** (not only text) and a 200 % text overflow test across the default theme.
8. **Color-vision filter does not reach the UI**: it works through the 3D view's LUT; the UI pass would need the
   same transform (a `regia_ui` render change).
9. **Input assists for local multiplayer and an input cool-down** *(closed, ADR-3464).* `ActionAssist.cooldown_steps`
   is the minimum number of evaluations between accepted presses (0 to 600). `InputAssists.players` and
   `gamepads` overlay the default set (player, then gamepad, then default). `input.assists` takes `player` or
   `gamepad`, not both. Tests: `cooldown_steps_drop_a_press_that_starts_inside_the_gap`,
   `player_and_gamepad_maps_overlay_the_default` (`crates/regia_input/src/assist.rs`),
   `cooldown_steps_drop_presses_inside_the_gap`, `a_players_toggle_changes_only_that_players_stream`
   (`crates/regia_input/tests/assists.rs`), `assists_set_cooldown_and_a_per_player_override`
   (`crates/regia_agent/src/input_assists.rs`).
10. **Reticle / crosshair helper** with player color and size.
11. **Caption direction indicators**, and system captions preference on Linux and macOS.
12. **Touch target minimum is a lint**, and the `compact` density is exempt.
13. **Tests missing for behavior that exists**: release-to-activate (pointer cancellation) and the `invalid` state
    reaching the screen-reader tree; no error-message relation for text fields.
14. **Assist options** (aim assist, steering assist, skip) as framework helpers: genre-specific, lowest priority
    for the engine.
15. **Speech-to-text for voice chat**: needs a speech model; out of scope before v1.0.

## 8. Closed in this lane

- Hold versus toggle, minimum hold time and auto-repeat per action (`regia_input::InputAssists`, `input.assists`).
- Mono audio (`Audio::set_mono_output`, `audio.mono`).
- Color-vision simulation and compensation (`regia_render::color_vision`, `render.color_vision`).
- Gamepad rumble intensity (`Gamepads::set_rumble_scale`, `input.haptics`).
- All four persisted in `GameSettings.accessibility` and applied live.
- Scene motion (ADR-3464): camera-shake scale, motion blur, gameplay field-of-view override, and `reduced_motion`
  on shake, handheld sway and walk bob, and motion blur. Tests named on gap 2.
- Look sensitivity and Y invert on the actions `InputConfig.look_actions` names. Tests named on gap 4.
- Voice and Ambience volumes and the five bus mutes, persisted and applied. Tests named on gap 5.
- Phone vibration scale (`PhoneVibration`, `input.haptics`). Tests named on gap 6.
- Per-player and per-gamepad input assists, and an input cool-down. Tests named on gap 9.

## 9. What the owner is asked to decide

- Accept this review as the v1.0 "Accessibility (UI and input) reviewed" evidence, with gaps 1 to 4 as v1.0
  blockers or as accepted known issues.
- Whether the GAG list here matches the live site closely enough, or should be re-checked line by line.
