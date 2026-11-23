# DO27 switch package for v0.7

This page is for Direct Order 27 (DO27), the game that is moving its engine-general code onto Regia Engine. It is
the v0.7 switch package: what DO27 switches to, what changes in the tunnel query, what DO27 re-cooks for format
batch 2, and what Regia promises after the tag. It links the pages that hold the detail and does not restate them:

- [Adopting engine features](adopting-features.md): how to adopt a row (turn it on beside the host's code, run the
  check, delete the host's code in the change that makes the engine feature the default).
- [DO27 host checklist](do27-host-checklist.md): field-by-field host changes.
- [DO27 interface contracts](do27-interface-contract.md): the data DO27 and the engine exchange.
- [Tunnels Stage B: the switch note for DO27](tunnels-stage-b-switch.md): the tunnel query's switch.

**The bar for v0.7 is "ready for DO27 to switch", not "DO27 has switched"** (owner, 2026-11-19). DO27 is paused until
the v0.7 tag. Its own pin runs and its sim-version bump are v0.7.x work (section (d)).

Every Regia name on this page was checked against the code on the v0.7 integration line (integ-v07c, 71773be0c) on
2026-11-20, name by name. DO27's side is described from the absorb plan and the lane reports only.

## (a) What DO27 switches to

How to read the tables. One row per DO27 component in the absorb plan's switch table (absorb plan § 6), plus the
absorb-gaps rows (ids in brackets) the plan briefed later. "Regia API" names the crate path and main types; "Format"
is the name in `docs/formats/format-hashes.ron` (or "none": the system reads no file of its own); "Parity test" is a
test that proves the behavior DO27 relies on (the plan's acceptance test where it exists on the line); "State" is
**ready** (built and merged on this line), **ready, checks owed** (merged; a platform leg or a gate check is still
owed, named), **partial** (what is missing) or **not on this line** (the lane that owns it). Adopt a row with the
steps in [Adopting engine features](adopting-features.md#how-to-adopt-a-row).

### Determinism foundation

| DO27 component | Regia API | Format | Parity test | State |
|---|---|---|---|---|
| `math.rs`, the clippy list and the scanner (F-01) | `regia_detsim::math` (`sin`, `atan2`, `hypot`, ...), `ensure_default_fp_env`, `FpEnvWatch`; `crates/regia_detsim/clippy.toml`; `cargo xtask det-scan` | none | `canonical_atan2_matches_the_edge_vector_golden`, `canonical_atan2_is_correctly_rounded_on_the_published_hard_cases`, `spot_values_are_libm_0_2_16`, `det_scan_flags_a_planted_powi_and_hash_iteration`, `ftz_guard_resets_a_set_mxcsr` | ready. `MATH_BITS_VERSION` is 1, and version 1 **is** the `atan2` canonicalization (lane v07-det-atan2-canon, landed; it is not pending work): `math::atan2` is correctly rounded, so its bits differ from DO27's old wrapper (libm `atan2f` with canonical inputs and output) by one ulp exactly where libm is not correctly rounded — e.g. `atan2(8.681013, -8809090.0)` moved `0x40490fd6` to `0x40490fd7` — and from bare libm also on signed zeros and NaNs. DO27 re-records once the checksums, replays and goldens that call `math::atan2`, sends `MATH_BITS_VERSION 1` in the handshake, and calls `atan2_libm` where bytes must match the old bits; Stage A keeps DO27's bits (atan2 canon (`docs/releases/v0.1.28-v07-det-atan2-canon.md`)) |
| The CROSS_ARCH golden (F-02, F-03) | `regia_detsim::golden` (`DetScenario`, `DetRegistry`, `GoldenMode`), also `regia_test_utils::det_golden`; `cargo xtask det-golden` with its legs | `regia-det-golden-1` | `a_flipped_bit_names_the_first_diverging_tick`, `planted_simd_scenario_differs_only_on_aarch64` | ready, checks owed: the qemu and Android legs. DO27's own golden is not a registered scenario: `do27_steering_golden_through_the_lockstep_driver_matches_its_sha` needs DO27's sim and is ignored here (DO27 runs it in its pin runs, section (d)) |
| `desync_hash.rs` and its fold; the NET_DESYNC2 float rule (F-04, F-05) | `regia_detsim::hash` (`Mix`, `canonical_f32_bits`, `SystemFold`, `DesyncHash`, `SystemRing`, `diverged_systems`); ECS side `EcsDesyncHash`, `DesyncHashPlugin`; agent `sim.desync_rows` | none | `identical_runs_never_diverge`, `every_system_injection_is_named_at_its_tick`, `negative_zero_and_nan_hash_canonically` | ready |
| `entity_ids.rs`, the n² passes, ENTITY_CAP (F-06) | `regia_detsim::ids::DetIdAllocator`; `regia_detsim::broadphase` (`DetBroadphase`, `BroadphaseConfig`, `FloorRule`) | none | `broadphase_matches_the_n2_pair_order_bit_for_bit`, `stress_210_entity_equivalent_determinism`, `free_list_is_first_in_first_out` | ready |

### Netcode and lockstep

| DO27 component | Regia API | Format | Parity test | State |
|---|---|---|---|---|
| The renet transport; NET_SECURE interim | `NetServer`, `NetClient` (sealed links: `CryptoMode`, `SecureLink`, `ServerKey`) | none (wire version 6) | `nothing_readable_crosses_a_sealed_link_and_the_control_shows_the_test_can_see`, `the_defaults_refuse_a_plain_remote_peer` | ready, checks owed: the transport lane's preflight |
| `HostLockstep` and reconnect (N224, N225) | `regia_net::lockstep` (`LockstepHost`, `LockstepSeat`, `LockstepConfig`, `DelayPolicy`, `SeatId`); `regia_net::reconnect::Reconnector`; agent `net.lockstep` | none | `four_seats_on_bad_wifi_for_five_minutes_never_stall_over_two_ticks_from_one_lost_datagram`, `a_seat_reconnects_after_15_seconds_and_is_in_step_within_5_seconds_with_the_same_seat_id` | ready, checks owed: one Android run |
| Desync votes and the probe; NET_DESYNC2 interim (N226, N227) | `regia_net::desync` (`DesyncReport`, `DesyncBundle`, `DesyncReplay`, `locate`); votes `Voter`, `DesyncVerdict`, `TiePolicy`; agent `net.desync`, `net.desync_inject` | `desync` (`*.desync.ron`) | `an_observer_breaks_a_two_seat_tie`, `an_observer_host_probes_both_diverged_seats`, `an_injected_desyncs_bundle_replays_offline_to_the_same_tick_and_subsystem` | ready, checks owed (the diagnose lane's checks) |
| Re-host (N301) | `regia_net::lockstep::HostMigration` (`MigrationConfig`, `MigrationRoster`, `rank_survivors`); agent `net.migration` | none | `two_migrations_in_a_row`, `every_seat_ranks_the_survivors_the_same_way` | partial: 12 of 13 tests green on the lane; the 13th waits on a rerun. Migration as net-soak cases [NL-01]: not on this line (lane lockstep-migration-scenarios) |
| Lockstep replay and the replay library [NL-02] | `regia_net::lockstep::replay` (`LsReplayWriter`, `LsReplayReader`, `ReplayAudit`, `ReplayLibrary`); `regia net replay-audit`; agent `net.replay_audit`, `net.replays` | `lsreplay` (`.lsreplay`) | `replay_audit_names_a_flipped_input_tick`, `library_rotation_keeps_the_newest_n` | ready, checks owed: `check-formats --update` at the batch 2 freeze |
| A server-hosted lockstep match [NL-03] | `regia_net::lockstep_server::LockstepServer`, `LockstepServerSettings`, `LockstepHostGame` | `server-toml` (its `[lockstep]` section) | `headless_observer_host_runs_40_seats_without_ecs`, `the_observer_counts_in_the_quorum` | ready |
| `host_check`, `port_check` (N229) | `regia_net::host_check` (`PortForwardCheck`, `UplinkProbe`, `estimate_clients`, `HostCheckReport`); `race_port_mappers`; agent `net.host_check` | none | `the_self_check_reports_reachable_and_then_unreachable_when_the_mapping_is_removed`, `the_capacity_estimate_on_listen_uplink_is_within_20_percent` | ready, checks owed: the hosting lane's preflight |
| ir-hub, dormant [NL-04, NL-05, NL-06] | `LobbyBackend`; `regia_net::lobby::hub` (`HubLobbyBackend`, `HubServer`, `HubHost`, `InviteCard`); the `regia-lobby-hub` binary, shipped by `regia package --server`; agent `lobby.hub_status`, `lobby.hub_join_status` | `hub-toml` | `two_clients_find_each_other_through_the_hub_without_steam`, `invite_card_carries_no_host_address`, `a_forged_welcome_is_refused`, `hub_soak_1000_presences_bounded_memory` | ready, checks owed: the binary lane's clippy, rustdoc and package staging |
| ir-log's second logger and the report path (NL-07, Q-11) [NL-08] | `regia_core::log::session::PeerLog` (`hash_trace`); `ReportSourceDirs`, `ReportSources`; `regia_crash::outbox::Outbox`; agent `report.outbox_status` | none (the outbox's ledger is not a frozen format) | `two_peers_hash_trace_lines_diff_to_the_first_split`, `report_sources_lists_logs_dir`, `second_upload_sends_only_new_tails`, `a_crash_marker_resends_the_session_once` | ready, checks owed: the log-extras lane's platform matrix and allocation counts |
| The lockstep bot soak [NL-09] | `lockstep-mix` profiles of `regia_net_soak` (`InputMixKind`); `cargo xtask net-soak --two-host` | none | `lockstep_mix_40_seats_desync_zero_1_min`; `lockstep_mix_40_seats_desync_zero_10_min` (long, run with `--ignored`) | ready |

### Simulation building blocks

| DO27 component | Regia API | Format | Parity test | State |
|---|---|---|---|---|
| Pathing grid, search (S-01, S-02) | `regia_nav_det` (also `regia_nav::det`): `NavGrid`, `Stamp`, `SizeClass`, `MoveClass`, `GridGround`; `regia_nav_det::search` (`plan`, `plan_layered`, `Scratch`); agent `sim.grid_flow` | none (built from the ground at run time) | `stamp_api_matches_the_class_stamps_bit_for_bit`, `grid_bits_match_the_pinned_hashes`, `two_runs_give_the_same_hash` | ready, checks owed (the search lane's checks) |
| Grid agents [S-03] | `regia_nav_det::agent` (`DetAgents`, `DetMover`, `KinematicMover`, `DetAgentWorld`); agent `nav.det_agents` | none | `agents_follow_through_a_custom_mover`, `two_runs_give_the_same_hash` | ready |
| ORCA (S-04) | `regia_detsim::avoid` (`orca_velocity`, `OrcaBody`, `OrcaSettings`, `OrcaSolver`) | none | `orca_bits_match_on_aarch64`, `head_on_pair_passes_on_opposite_sides` | ready |
| Crowd lanes [S-05] | `regia_nav_det::crowd::Crowd` (`register_choke`, `register_tunnel_chokes`), `CrowdSettings` | none | `crowd_lanes_file_through_a_registered_choke`, `crowd_bent_slots_pull_back_from_a_cliff` | ready |
| Navmesh (S-06) | `regia_nav_det::navmesh::NavMesh` (`bake`, `bake_id`, `find_path`) | `nav-det-navmesh` (`<map>.detnav`) | `bake_bytes_identical_twice`, `navmesh_bake_id_pinned_on_every_arch`, `canonical_float_bytes_match_do27_rule` | ready, checks owed (the navmesh lane's checks) |
| Hover and the handling rig (S-07, S-08) | `regia_detsim::hover` (`step`, `step_world`, `HoverParams`, `HoverState`, `FlightMode`); `regia_detsim::handling` (`measure`, `Report`); agent `detsim.hover_step`, `detsim.hover_probe`, `detsim.hover_hull_check` | `hoverhull` (`.hoverhull.ron`, read by the framework host) | `libm_1000_tick_vector_matches_committed_hash`, `rig_reports_the_feel_targets` | ready, checks owed: two aarch64 legs. The committed vector is Regia's own default hull (ADR-8164), not DO27's hull: DO27 re-records its hull's vector in its pin runs |
| AI core, perception, aim and difficulty (S-09 to S-11) | `regia_detsim::ai` (also `regia_ai::det`): `AiRng`, `AiSchedule`, `UtilityCatalogue`, `InfluenceGrid`, `SightingsBoard`, `LineOfSight`, `PerceptionIndex`, `DifficultyProfile`, `lead_point`, `fire_gate` | `ai-det-utility` (`.detutility.ron`) | `ai_schedule_slices_by_entity_id`, `rng_streams_diverge_by_seed_and_entity`, `los_trait_sees_through_an_open_mouth`, `auto_fire_only_fires_within_range` | ready |
| Motor, combat, wingman and mode bots (S-12, S-13) | `regia_detsim::ai::motor` (`arrive`, `steer_to`), `ai::combat`, `WingmanFormation`, `ai::mode_bots` (`mode_orders`); `regia_rules::bots::BotHooks`; mortar arcs `regia_detsim::aim::AimSolution`; agent `detsim.aim_solve` | none | `fire_gate_never_fires_out_of_band`, `formation_spacing_stays_within_20_percent`, `mode_bot_hooks_route_a_ctf_carrier` | ready |
| Modes and match flow (S-14) | `regia_rules` (also `regia_framework::rules`): `MatchSession`, `MatchFlow`, `MatchMode`, `ObjectiveBoard`, `Playlist`, `RuleSet`; agent `rules.match_setup`, `rules.match_input`, `rules.match_step` | `rules-ron`, `playlist-ron`, `match-result` | `ready_up_starts_on_an_agreed_tick`, `match_rules_ctf_koth_3600_matches_the_committed_hash`, `host_pause_freezes_the_tick_and_releases_it` | ready |
| Series, map vote, end stats, spectator vision [S-15] | `regia_rules::series::SeriesState`, `map_vote::MapVote`, `WinCondition`, `MatchEndLedger`, `vision::sees_team`; agent `rules.series`, `rules.vote_cast`, `rules.end_stats` | inside `rules-ron` | `best_of_three_ends_at_two_wins_and_carries_between`, `tally_picks_the_most_voted_option` | ready |
| `fog.rs` (S-16) | `regia_detsim::fog` (`SightTable`, `FogRules`, `FogWorld`, `TeamVision`, `FogGrid`, `FogMemory`); `regia_framework::fog_view::FogViewSim`; `regia_nav_det::fog_los::GridLineOfSight`; agent `fog.status`, `fog.set_viewer` | none | `a_unit_outside_every_friendly_sight_radius_is_hidden`, `hidden_unit_never_reaches_the_viewer_snapshot` | ready |
| Gait, sim harness (S-17, S-18) | `regia_animation::gait` (`step_gait`, `GaitParams`, `GaitGround`); `regia_detsim::harness::SimHarness` | `gait` (`.gait.ron`); `regia-det-golden-1` | `tripod_keeps_three_feet_planted`, `same_seed_same_snapshot_after_n_ticks` | ready, checks owed (both lanes' checks) |
| The announcer line id (S-19) | today `Audio::announce` and `Announcer` (bank-level lines) | none | `higher_priority_pre_empts_lower` | not on this line: the line-id API is on lane announcer-external-line (unpushed) |

### Tunnels and world geometry

| DO27 component | Regia API | Format | Parity test | State |
|---|---|---|---|---|
| The tunnel sim (layout, query, `TunnelsState`, hover ground, sense), Stage A (W-01 to W-05) | `regia_world_geom::compat` (no feature): `TunnelLayout`, `TunnelsState` (`support`, `move_floor`, `place_floor`, `confine`, `underground`), `compat::surface::column`, `sample_move_floor`, `hear_range_m` | DO27's own `do27-tunnels/1` export, read by `parse_tunnels` (not a Regia frozen format) | `switchbacks_mesh_is_byte_identical_from_the_baked_description`, `step_ok_matches_the_t3_acceptance`, `stage_a_bits_are_the_same_with_and_without_stage_b` | ready: no checksum change |
| The same, native: Stage B (W-06 to W-13, W-29) | `WorldGeom` (`from_cooked`, `floors_at`, `support`, `covered_at`, `place_floor`, `push_disc`, `segment`), `GeomState::state_hash`, behind `stage_b`; agent `terrain.surface_at`, `terrain.mesh_layers` | `terrain-mesh-layers-cooked` | `two_peer_tunnel_drive_checksums_equal_x86_aarch64`, `native_mouth_hop_at_full_speed_le_5cm_per_tick` | partial: see section (b). The D4 column bar waits for an owner ruling, `native_resident_geometry_fits_the_sim_budget` is red (a cook-estimate check, not the memory bar), aarch64 legs owed. DO27's bump is v0.7.x |
| `ruins.rs` crater, `terrain_deform.rs` (W-12) | `GroundEdit` (`Crater`, `Berm`), `GroundEdits`, `GeomState::apply_ground_edit`, `DirtyRect`, feeding `TerrainHeightEdits` and the nav dirty rect; Stage A keeps `CraterBook` | `terrain-mesh-layers-cooked` (the ground lattice) | `crater_over_a_covered_tube_moves_its_floor_by_zero`, `same_ops_twice_give_identical_grids_and_dirty_lists`, `dirty_rect_reaches_terrain_height_edits_nav_and_the_index_generation` | ready, checks owed: aarch64 legs |
| Collapse rubble (W-13) | Blockers: `GeomState::set_blocker`, `BlockerId`, `BlockerStatus`, `section_open`, `cell_route`; agent `terrain.blockers` | `terrain-mesh-layers-cooked` (blocker plugs) | `a_blocker_closes_and_reopens_in_one_generation`, `two_peers_checksum_equal_across_a_close_and_reopen` | ready |
| The on-foot grid, `pilot_mesh` (W-03, W-30) | Stage A `compat::onfoot::PilotGrid`; native `regia_world_geom::mover` (`move_disc`, `move_capsule`, `dressing_grid`, `body_probe`) | `terrain-mesh-layers-cooked` (native) | `cells_at_span_cap_is_reported`, `rubble_pile_is_a_sim_floor`, `drawn_solid_within_half_meter_of_wall` | ready, checks owed: the mover lane's aarch64 legs and the walk re-pin |
| Centerline tunnel nav (W-04, W-20) | `regia_nav_det::tunnel_bake::TunnelNavMeshBake`, `TunnelLayer`; layered: `LayeredBake`, `LayerState::sync`, `NavMesh::det_layered_bake`, `cook_layered` | `nav-det-navmesh`, `nav-cell` | `a_collapse_closes_its_passage_and_clearing_reopens_it`, `two_hulls_pass_in_every_passage`, `junction_never_wedges`, `layered_tunnel_runs_give_the_same_hash` | ready, checks owed: det-scan, Windows, Android and qemu legs of the layered nav |
| `cam_clip`, `camera_clip` (W-26) | `regia_world_geom::interior::InteriorProfile` (`step`, `place`), `CameraInteriorParams`, `mouth_blend`; agent `camera.interior_state` | `terrain-mesh-layers-cooked` | `junction_pass_has_no_one_frame_snap`, `cam_rel_eye_under_a_ceiling_stays_in_the_tube` | ready, checks owed: the Mac and Windows runs |
| Zone boxes, exposure ease, reverb (W-24, W-25) | `regia_world_geom::zones` (`ExposureEase`, `section_volumes`); `regia_world_geom::portal_sound::PortalSoundMap`; `regia_audio::geom_sound::GeomSoundField`; agent `render.get_exposure`, `audio.probe_zones` | `terrain-mesh-layers-cooked` (zones, portals) | `drive_through_every_mouth_steps_le_half_ev`, `ease_uses_frame_time_not_wall_time`, `gunfire_around_a_bend_comes_from_the_mouth_direction` | ready |
| Mouth rects, hero holes, the half-texel remap (W-10) | `regia_world_geom::cut` (`cut_mouth`, `cut_lattice`, `LatticeCut`); `NamedHole`, `HolesMask::named_hole` | `terrain-holes`, `terrain-mesh-layers-cooked` | `visual_grid_shift_is_the_half_texel_at_the_origin`, `mouth_holes_match_the_render_mask`, `rings_are_the_cut_boundary` | ready |
| `tunnel_bake.py` and the four drifting files (W-07 to W-09) | `regia_world_geom::tunnel_bake` (`bake`, `check_consistency`), `regia_world_geom::cook::cook`, `PassagesDocument::import_do27_json`; `regia terrain passages`, `regia terrain cook`, `regia terrain check-layers`; the `regia cook` pass `cook_level_job`; agent `terrain.bake_report` | `terrain-mesh-layers` (`.meshlayers.ron`, `.passages.ron`) in, `terrain-mesh-layers-cooked` out | `do27_json_imports_to_the_same_canonical_mesh`, `switchbacks_cuts_35_sections`, `triangle_count_drift_fails_cook`, `render_collision_nav_agree` | ready, checks owed: the GPU plate review |
| Backdrop ring, cliff shells (P-09, W-32) [P-09] | `regia_terrain::backdrop::BackdropRing`, `build_visual_map`, `extend_visual_map`; agent `terrain.backdrop_status` | none (extends the in-memory map) | `backdrop_seam_gap_zero` | ready for the ring. Cliff shells: not on this line (lane cliff-shells-as-layers) |

### Presentation

| DO27 component | Regia API | Format | Parity test | State |
|---|---|---|---|---|
| The `present.rs` sim thread (P-01) [P-02] | `regia_framework::sim_host::ThreadedSimHost` (`FixedStepSim`, `SimHostSettings`); `ExternalSim`; `BlendPolicy`; `SimHandedness`, `mirror_mesh_winding`; agent `sim.host_status` | none | `host_never_publishes_a_torn_frame`, `x_reflection_flips_winding` | ready, checks owed: Android, Windows and preflight of both lanes |
| The `bank.rs` mirror, LOD bands, fade, keys (P-03) [P-04] | `regia_render::draw_list` (`DrawList`, `DrawKey`, `KeyedDraw::with_lod_chain`); `ShadowCast::Local`; agent `render.draw_list` | none | `drawlist_9k_keyed_draws_spawn_no_entities`, `drawlist_item_crossfades_like_an_entity` | ready, checks owed: the core lane's preflight and Windows |
| The CPU HUD raster [P-05, P-06] | `regia_quick::draw2d` (`Draw2d`, `Draw2dList`, `Draw2dTiles`, `DirtyRun`, `HudLayer`); sample `samples/hud_draw2d`; agent `quick.draw2d_stats` | none | `draw2d_matches_the_golden_hud_tile`, `hud_partial_redraw_matches_a_full_redraw` | partial: the 2 ms HUD paint bar is a bench row, not an asserted test; the Deck and Pixel numbers are owed |
| `fast_quit`, `single_instance`, `stack_dump` (P-07, P-08) | `regia_platform::fast_exit` (`ExitMode`, `request_fast_exit`), `SingleInstanceLock`, `alt_menu`; `regia_crash::stack_dump::write_hang_report` | none | `fast_quit_hides_within_one_frame`, `second_instance_refused`, `hang_report_has_every_thread` | ready, checks owed: native Windows for the stack dump and the Alt menu |
| Foot plant, node clips, the rig driver (P-10, P-11) [P-12] | `regia_animation::foot_ik::plant::FootPlanter`, `PlantLock`; `CookedClipBody::RawNodes`; `regia_animation::node_driver::NodeDriver`; agent `anim.foot_plant_simulate`, `anim.node_drivers` | `cooked-clip`, `node-tracks` | `planted_foot_never_slides_over_1cm`, `plain_node_channel_animates`, `hinge_hits_its_limit` | ready, checks owed (the foot-plant and node-track lanes' checks) |
| The observer camera, photo mode (P-13) [P-14] | `regia_framework::spectator::SpectatorMode`; `regia_framework::photo_mode::PhotoMode`; agent `spectator.set`, `photo.enter`, `photo.capture` | none | `orbit_mode_keeps_target_centered`, `photo_camera_never_leaves_the_leash` | ready |
| `ultra_low.rs`, the VRAM budget (10-01, 10-02) | `QualityTier::VeryLow`, `ProfileName`; `regia_render::memory_budget` (`BudgetTier::IntegratedLow`, `MemoryBudget::for_tier`) | none | `very_low_is_desktop_low_drawn_at_a_third_and_reads_as_desktop_low`, `intel_n_series_built_in_graphics_pick_very_low`, `integrated_low_is_the_floor_row` | ready, checks owed: the profile-names lane's clippy and preflight |

### Content and tools

| DO27 component | Regia API | Format | Parity test | State |
|---|---|---|---|---|
| The `map_load.rs` switch; MAP_SWITCH_MEM interim (Q-01) | `regia_core::load_switch` (`LoadSwitch`, `CancelToken`); `regia_framework::level_switch` (`LevelSwitch`, `LevelSwitchPlugin`); agent `level.switch`, `level.switch_status` | `scene` (`.level.ron`) | `switch_cancels_old_load_rss_bounded`, `a_second_request_cancels_the_first_and_cancel_keeps_the_open_level` | ready, checks owed: the lane's preflight and Windows |
| Square padding (Q-05) | `TerrainMap::with_world_depth`, `world_depth` | runtime only | `_4100x1540_map_samples_without_padding`, `rectangular_selection_skips_the_overhang_and_clamps_straddling_vertices` | partial: the level file has no world-depth field yet (scene 14 carries `sample_layout` only), so the host still sets the depth after loading |
| The scatter re-bake for rock colliders (Q-08) | `PlacementOrigin`, `WorldAnchor`, `place_set_with_origin`, `bake_scenery_colliders`; agent `terrain.scatter_colliders` | `scatter` | `backdrop_extension_moves_no_rock`, `world_anchored_colliders_do_not_move_when_the_terrain_grows` | ready |
| The `replay_engine` v4 to v3 rewrite (Q-04) | `regia_framework::replay_sim::ReplaySim` skips unknown records | DO27's `.do27replay` versions 2 to 4 (`docs/formats/do27replay.md`) | `v4_file_with_unknown_record_plays` | ready, checks owed: the lane's allocation counts and Windows |
| `cook_project.sh`: sidecar `.meta`, LOD classes, the id manifest, roots (Q-06, C-02) [C-01] | `gltf_sidecars`, `regia_asset_pipeline::lod_classes` (`LodClassTable`); `regia assets` `ids export`/`ids import`, `regia cook --ids`, `export_asset_ids`; `regia_asset_pipeline::overlay_roots`; agent `assets.ids_export`, `cook.overlay_roots` | `meta`, `asset-ids` | `sidecar_image_gets_meta`, `lod_class_table_reaches_cook`, `recook_keeps_ids_so_patch_ships_one_pak`, `missing_buffer_filled_from_second_root` | ready |
| `pack_diet.py`: closure and diet rules (C-03) [C-04] | `ContentMode::Closure`; `regia_asset_pipeline::cook_diet` (`DietRules`, `plan_diet`, `drop_finest_lods`); agent `assets.diet_report` | `asset-registry` | `unreferenced_asset_not_shipped`, `lod_dropped_over_1_5m_triangles` | ready, checks owed: the diet lane's rustdoc and Windows |
| The glTF lint (C-05, C-06) | `regia assets lint-materials` (with `--fix`), `regia assets lint-geometry`; `lint_materials`, `lint_gltf` | none (glTF sources) | `orm_with_metallic_1_flagged_and_fixed`, `stretched_uv_coincident_pair_flagged` | ready, checks owed (both lint lanes) |
| The kit check (C-07) [C-08] | `regia assets contract-check`; `DeliveryContract`, `check_delivery`, `write_vendor_report`; agent `assets.check_contract` | `delivery-contract` | `wrong_up_axis_fails_contract`, `missing_socket_named_in_report` | ready |
| The package audit (C-09) [C-10] | `regia package` runs `regia_cli::package_hygiene` (`check_project_tree`, `prepare_stage`, `Policy`, `StageRules`, `PathScrub`) | none | `planted_dev_flag_string_fails_package`, `build_path_in_json_scrubbed` | ready, checks owed (package-hygiene-a) |
| Parity (C-11) [C-12] | `regia_asset::access_trace` (`REGIA_ASSET_TRACE`); `regia package parity` (`package_parity::run`); agent `assets.access_trace`, `package.parity` | none | `trace_lists_a_missed_open`, `file_missing_from_package_reported` | ready, checks owed (asset-access-trace) |
| Triage and the perf gate (C-13) [C-14] | `regia logs triage`, `regia logs perf-gate`; `regia_agent::log_triage::triage`, `perf_gate`; agent `logs.triage`, `logs.perf_gate` | none | `split_upload_stitched_in_order`, `p99_over_threshold_fails_gate` | ready |
| Release chunks, verify, diff, sign (C-15) [C-16, C-17] | `regia engine chunk` (`plan`, `cut`, `pack`, `verify`); `regia release` (`verify --deep`, `diff`, `sign`, `upload-plan`); `regia_release_chunks` (`cut_tree`, `verify_release`, `diff_release`, `sign_release`, `UploadBackend`); agent `release.verify`, `release.sign_status` | `release-chunks`; the `release-signature` sidecar (spec 12 § 4a; its `format-hashes.ron` row lands at the batch 2 freeze) | `chunk_never_exceeds_cap`, `flipped_byte_fails_deep_verify`, `tampered_manifest_signature_refused` | ready, checks owed: the signature's format row (release-sign-hosting) |
| Terrain Python: operators A (C-18) | `regia_terrain::ops::features` (`terrace`, `crater`, `flatten_pad`, `domain_warp`); procgen nodes; agent `terrain.apply_operator` | none (heights in memory) | `flatten_pad_is_flat_within_1cm`, `chained_ops_apply_in_order_and_unwind_in_reverse` | ready |
| Terrain Python: operators B, pit, ramps, curvature [C-19] | none on this line | none | none on this line (the plan's haul-ramp test is on the lane branch) | not on this line (lane terrain-ops-b) |
| Terrain import, the minimap (C-20, W-27) | `regia terrain source-import`, `regia terrain overview-bake`; `TerrainSource::from_json`, `from_png`, `bake_overview`, `regia_world_geom::overview::bake_directory`; agent `terrain.inspect_source`, `terrain.bake_overview` | `terrain-source-tiles` | `zero_datum_png_imports_to_meters`, `minimap_bake_twice_identical_bytes` | ready, checks owed: Windows |
| `fpc_collect` (C-21) | `regia package --pipeline-caches` (`package_pipeline_caches::collect`) | the pipeline cache (`FPCH_VERSION`) | `fpc_from_bundle_merged_for_its_driver` | ready, checks owed: Windows |
| The play gate (C-22) [C-23] | `regia play-gate` (`replay`, `check`, `run`); `KeyTrack`, `run_gate` | `key-track` | `track_replays_keys_and_expects_log_lines`, `installed_player_passes_track` | ready |
| `provenance.rs`, the pack doctor (C-24) | `regia assets provenance-import` (`import_provenance`); the pack-doctor rule is `regia gen verify`; agent `assets.provenance_import` | `meta` (writes v2) | `voice_row_survives_migration` | ready |
| `ir-script`; LUA_SANDBOX interim (C-25) | `regia_script_lua` authority split: `ScriptAuthority`, `HostCommandRules`, `HostCommand`, `with_authority`, `drain_host_commands` | `luamod` (`luamod.ron`) | `presentation_script_cannot_enqueue_host_commands`, `a_host_snapshot_hook_can_queue_a_command` | ready |
| Package containers (C-26) | `regia package --container` (`steam-sniper`, `debian12`), `ContainerProfile` | none | `container_profile_builds_glibc_floor` | ready, checks owed: the containers lane's agent tests, Windows, Android and preflight |

### What is not ready on this line

Everything else in the tables above is merged on this line. These rows are not, or not in full; each names the lane
that owns it, and none blocks DO27 from switching the rest:

- Lockstep host migration: one of 13 migration tests waits on a rerun; migration as net-soak cases [NL-01] is lane
  lockstep-migration-scenarios.
- Stage B: the D4 column bar (owner ruling), the cook-estimate check, aarch64 legs (section (b)).
- The announcer line id (S-19): lane announcer-external-line.
- Terrain operators B [C-19]: lane terrain-ops-b. Cliff shells (W-32): lane cliff-shells-as-layers.
- The level file's world depth (Q-05): the host sets it after loading until it lands in a scene format step.
- The HUD's 2 ms paint bar [P-06]: measured by a bench, not asserted; Deck and Pixel numbers owed.

## (b) The tunnel query: Stage B

[Tunnels Stage B: the switch note for DO27](tunnels-stage-b-switch.md) is the switch note. It says what changes when
DO27 moves from Stage A (`regia_world_geom::compat`, DO27's own arithmetic, no checksum change) to Stage B (the native
query behind the `stage_b` feature): the calls DO27 replaces, the two version numbers its handshake adds
(`QUERY_BITS_VERSION`, `MATH_BITS_VERSION`), the state hash it folds into its checksum, the pins it re-records in the
same bump, the bars measured per device, and the fallback if a bar is still missed at the 11-30 checkpoint (v0.7 ships
Stage A as the default). This page does not copy it.

On this line `QUERY_BITS_VERSION` is 8 and `MATH_BITS_VERSION` is 1. If either moves before DO27's bump, DO27 sends the
new value.

## (c) Format batch 2: what DO27 re-cooks

Format batch 2 is the one set of format steps in v0.7 (owner-approved 2026-11-19, freeze Sat 11-28). The upgrade
guide for the whole batch is `docs/guide/upgrade-v07.md`, written by the freeze lane (v07-fmt-batch2-freeze) when it
runs the single `check-formats --update`; it covers every project. The table below is DO27's part of it: the formats
DO27's content uses whose version moved since v0.6.0, and what DO27 runs for each. Versions are the ones in
`docs/formats/format-hashes.ron` on this line; each row links the lane's release notes.

| Format | v0.6.0 → v0.7 | What moved | What DO27 runs | Release notes |
|---|---|---|---|---|
| `nav-cell` | 2 → 3 | `NAV_CELL_FORMAT_VERSION`: conservative rasterization (a cell takes every triangle that overlaps it). Same bytes, new height meaning; navmeshes change at triangle edges and thin geometry now blocks or carries agents | **Re-cook** the streamed levels' navigation (`regia cook`). A v1 or v2 cell still loads with a warning that names the re-cook; a build older than v0.7 refuses a v3 cell (`nav_cell_version`). No `regia upgrade-project` row: the cell is cooked output | nav cell v3 (`docs/releases/v0.1.28-w11-fmt-nav-cell-v3.md`) |
| `terrain-mesh-layers-cooked` | 1 → 2 | The tunnel index section of every `<map>.meshlayers`: fixed-width ground counts behind a hashed head, so **every cooked map's `content_hash` changes** even where the cooked value does not | `regia upgrade-project --write` brings a v1 file to v2 with the same geometry, then `regia cook` rebuilds it from source. ADR-10346 invalidates the migrated cache identity and revisions the cook key, so filled skylight mouths receive the current cuts even if their chunk LOD files survive or the package was already upgraded. See the [switch note](tunnels-stage-b-switch.md#what-do27-re-cooks). Anything DO27 stored with a hash takes the new one | index build (`docs/releases/v0.1.28-w11-index-build.md`) |
| `terrain-index` | 2 → 3 | `TERRAIN_INDEX_FORMAT_VERSION`: the cooked terrain index (`terrain/index`) can hold a Z sample spacing unequal to X and cooked masks | `regia upgrade-project --write` rewrites each v1 or v2 index as v3 (`terrain_index_migration_sources`); a full `regia cook` writes v3 too. A v2 index still loads | terrain index v3 (`docs/releases/v0.1.28-v07-fmt-terrain-index-v3.md`) |
| `scene` | 13 → 14 | TerrainMap spacing: `SCENE_FORMAT_VERSION` 14 adds `LevelTerrain::sample_layout` (`TerrainSampleLayout`: `VertexCentered`, the default, or `CellCentered`) | `regia upgrade-project --write` (a v13 level's terrain block gains `sample_layout: VertexCentered`; nothing moves). **Re-cook** only a level switched to `CellCentered`: its `<map>.meshlayers`, scatter, navigation and GI heightfield move by up to half a cell. A host that builds `LevelTerrain` or `LevelGround` literals adds the field | TerrainMap spacing (`docs/releases/v0.1.28-v07-fmt-terrainmap-spacing.md`) |
| `gpu-mesh` | 2 → 3 | `GPU_MESH_VERSION`: GPU-ready layout for skinned and morphing meshes in desktop paks | **Re-cook** desktop paks (`regia cook --ship`). Older paks load unchanged; a v0.6 runtime refuses a v3 entry, so ship the runtime and the paks together. `GpuMeshView` gains `extras` | GPU mesh v3 (`docs/releases/v0.1.28-v07-fmt-gpu-mesh-v3.md`) |
| `decal` | 2 → 3 | `DECAL_FORMAT_VERSION`: decal prefabs (`.decal.ron`) gain `surface` (the footstep surface name, `DecalPrefab`) | `regia upgrade-project --write` rewrites v1 and v2 prefabs (version number only, with backups). **No re-cook**: `.decal.ron` is read as source. A host that spawns decals through `DecalPool::spawn_with` calls `set_footstep_surface` to keep footsteps | decal v3 (`docs/releases/v0.1.28-v07-fmt-decal-v3.md`) |

Run `regia upgrade-project` without `--write` first: it lists every file it would change. Then run it with `--write`,
then `regia cook` (and `regia cook --ship` for the paks). The revision-aware input key forces the first
mesh-layer source cook to rebuild legacy skylight cuts (ADR-10346); `--clean` is no longer required.
A second unchanged cook writes nothing, and a second `--write` changes nothing.

Two more batch 2 steps touch DO27 without a re-cook. The net wire is version 6 (`REGIA_NET_WIRE_VERSION`): every
peer of a session runs the same Regia build. The rules formats (`rules-ron`, `playlist-ron`, `match-result`) are
new in v0.7, so DO27 has no old files to migrate; the gait and other v0.7 rows are listed in the upgrade guide.

## (d) After the tag: the v0.7.x promise

DO27 is paused until the v0.7 tag, so two pieces of the switch are DO27's own work on v0.7.x, not part of the tag:

- **DO27's own pin runs.** DO27 re-records its own pins on its own content (the tunnel-fight lockstep checksum, the
  predicted-hull drive, the determinism fuzz fixtures, the replay hash, the lockstep checkpoints; the list is in the
  [switch note](tunnels-stage-b-switch.md#the-statement-adr-8538)) and runs its play gate on the v0.7 tag.
- **The sim-version bump.** The Stage B switch is one sim-version bump, outside an alpha week (spec 121 § 14 rule 3).
  DO27 picks the week.

**Regia's side: a 48 h fix turnaround** (owner, 2026-11-19). An engine bug that blocks DO27's pin runs or its
sim-version bump on v0.7.x gets a fix on the v0.7.x line within 48 h of the report. A report names the Regia tag, the
failing check (a test name, an agent method and its answer, or a `regia` command and its output) and, for a pin, the
expected and measured values. A fix that would move a frozen format or a Regia pin says so in its release notes and
waits for DO27's go before it merges. A missed bar is not a bug: it goes to the owner for a ruling with the measured
number (the [switch note](tunnels-stage-b-switch.md#if-a-bar-is-missed-the-fallback) lists the open ones).
