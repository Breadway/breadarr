# Graph Report - breadarr  (2026-08-03)

## Corpus Check
- 46 files · ~90,608 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 605 nodes · 1557 edges · 19 communities
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 2 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `109b29ee`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- scheduler.rs
- api/mod.rs
- Connection
- execute_search_targets
- enumerate_search_targets
- rss.rs
- matcher/mod.rs
- parser/mod.rs
- config.rs
- transcode/mod.rs
- ffprobe.rs
- importer/mod.rs
- Result
- process_pending_grabs
- Connection
- import_one
- find_relinkable_episode_files
- fetch_candidates
- import_season_pack

## God Nodes (most connected - your core abstractions)
1. `import_one()` - 30 edges
2. `process_item()` - 30 edges
3. `TranscodeConfig` - 24 edges
4. `process_pending_grabs()` - 24 edges
5. `parse()` - 22 edges
6. `AppState` - 19 edges
7. `encode_and_verify()` - 18 edges
8. `fetch_candidates()` - 18 edges
9. `import_season_pack_file()` - 17 edges
10. `execute_search_targets()` - 17 edges

## Surprising Connections (you probably didn't know these)
- `import_one()` --references--> `TranscodeConfig`  [EXTRACTED]
  breadarrd/src/importer/mod.rs → breadarr-shared/src/config.rs
- `import_season_pack()` --references--> `TranscodeConfig`  [EXTRACTED]
  breadarrd/src/importer/mod.rs → breadarr-shared/src/config.rs
- `import_season_pack_file()` --references--> `TranscodeConfig`  [EXTRACTED]
  breadarrd/src/importer/mod.rs → breadarr-shared/src/config.rs
- `maybe_enqueue_transcode()` --references--> `TranscodeConfig`  [EXTRACTED]
  breadarrd/src/importer/mod.rs → breadarr-shared/src/config.rs
- `process_pending_grabs()` --references--> `TranscodeConfig`  [EXTRACTED]
  breadarrd/src/importer/mod.rs → breadarr-shared/src/config.rs

## Import Cycles
- None detected.

## Communities (19 total, 0 thin omitted)

### Community 0 - "scheduler.rs"
Cohesion: 0.07
Nodes (24): a_second_prepare_call_on_an_already_claimed_review_sees_not_pending(), count_monitored_missing_episodes_in_season_counts_correctly(), does_not_find_an_unmonitored_or_already_have_episode(), find_episode_id_ignores_monitored_and_has_file_state(), finds_a_monitored_missing_episode(), insert_pending_review(), movie_does_not_need_grab_once_it_has_a_file(), movie_does_not_need_grab_when_unmonitored() (+16 more)

### Community 1 - "api/mod.rs"
Cohesion: 0.09
Nodes (37): AppState, BackgroundRequest, constant_time_eq(), CycleRecord, CycleStatus, require_api_token(), router(), Connection (+29 more)

### Community 2 - "Connection"
Cohesion: 0.16
Nodes (37): ApprovalPrep, best_existing_movie_score(), best_existing_score(), best_existing_season_pack_score(), count_monitored_missing_episodes_in_season(), finalize_review_approval(), find_episode_id(), find_media_item_id_by_title() (+29 more)

### Community 3 - "execute_search_targets"
Cohesion: 0.24
Nodes (17): TitleMatcher, execute_search_targets(), GrabCycleStats, is_seen(), mark_seen(), QbitClient, run_grab_cycle(), run_search_cycle() (+9 more)

### Community 4 - "enumerate_search_targets"
Cohesion: 0.13
Nodes (30): build_movie_query(), build_tv_query(), cadence_stays_backed_off_at_a_high_search_count_instead_of_overflowing(), enumerate_search_targets(), enumerate_search_targets_excludes_owned_movies_includes_missing(), enumerate_search_targets_excludes_unaired_inflight_and_anime(), enumerate_search_targets_for_media_item(), enumerate_search_targets_for_media_item_routes_anime_movie_via_tmdb_table() (+22 more)

### Community 5 - "rss.rs"
Cohesion: 0.07
Nodes (29): parse_human_size(), RawReleaseItem, Option, String, urlencode(), accumulate_field(), build_search_url(), ItemFields (+21 more)

### Community 6 - "matcher/mod.rs"
Cohesion: 0.14
Nodes (17): download(), ensure_model(), MatchCandidate, MatchOutcome, queue_for_review(), Connection, Option, Path (+9 more)

### Community 7 - "parser/mod.rs"
Cohesion: 0.08
Nodes (42): a_genuine_season_only_pack_with_no_dash_episode_still_has_no_episode(), brackets_still_take_priority_over_a_coincidental_bare_year(), Codec, does_not_mistake_a_year_titled_movie_for_a_bare_year(), does_not_mistake_a_yyyy_mm_dd_date_for_an_episode_range(), does_not_panic_on_real_corpus(), does_not_panic_on_unparsable_manga_release(), extracts_a_bare_year_from_a_scene_style_movie_name() (+34 more)

### Community 8 - "config.rs"
Cohesion: 0.06
Nodes (56): Config, config_path(), DaemonConfig, default_1337x_mirrors(), default_anime_svtav1_max_threads(), default_anime_svtav1_preset(), default_av1_efficiency_factor(), default_config_has_expected_values() (+48 more)

### Community 9 - "transcode/mod.rs"
Cohesion: 0.09
Nodes (63): Arc, TranscodeConfig, BacklogCandidate, cfg(), claim_pending_jobs(), claim_pending_jobs_claims_nothing_when_already_at_the_cap(), claim_pending_jobs_enforces_live_action_and_anime_caps_independently(), claim_pending_jobs_respects_already_running_jobs_as_a_global_cap() (+55 more)

### Community 10 - "ffprobe.rs"
Cohesion: 0.12
Nodes (27): AudioStream, build_media_probe(), DecodeCheck, generate_clip(), generate_test_clip(), is_english(), MediaProbe, parse_frame_rate_fraction() (+19 more)

### Community 11 - "importer/mod.rs"
Cohesion: 0.10
Nodes (25): a_fresh_grab_is_not_stalled(), a_grab_untouched_past_the_threshold_is_stalled(), a_torrent_missing_past_the_grace_period_is_failed(), a_torrent_missing_within_the_grace_period_is_not_yet_failed(), fail_grab_reopens_the_release_for_search(), find_relinkable_episode_files_falls_back_to_the_unpadded_season_folder(), find_relinkable_episode_files_skips_an_ambiguous_match_rather_than_guessing(), media_item_with_root() (+17 more)

### Community 12 - "Result"
Cohesion: 0.15
Nodes (24): copy_via_temp_file(), copy_via_temp_file_writes_through_a_part_file_and_renames_into_place(), find_by_basename(), find_by_stem(), import_season_pack_file(), insufficient_space(), largest_video_file(), locate_video_file() (+16 more)

### Community 13 - "process_pending_grabs"
Cohesion: 0.20
Nodes (13): a_persistently_failing_import_escalates_to_failed_instead_of_retrying_forever(), does_not_import_a_torrent_while_it_is_still_being_physically_moved(), fail_grab(), fetch_pending_grabs(), ImportStats, PendingGrab, process_pending_grabs(), process_pending_grabs_routes_each_grab_by_torrent_state() (+5 more)

### Community 14 - "Connection"
Cohesion: 0.17
Nodes (16): ensure_probed(), ensure_probed_does_not_flag_a_wide_aspect_ratio_file_as_under_quality(), generate_dual_audio_clip(), grab_is_stalled(), grab_missing_past_grace(), probe_library(), probe_library_reports_probed_vs_skipped_up_to_date(), ProbeSweepReport (+8 more)

### Community 15 - "import_one"
Cohesion: 0.16
Nodes (14): a_drifted_root_folder_does_not_defeat_the_dest_collision_check(), a_strictly_better_release_replaces_the_existing_file_via_a_real_move(), an_upgrade_swap_sweeps_every_duplicate_episode_file_row_not_just_one(), ensure_probed_flags_a_low_resolution_file_as_under_quality(), ensure_probed_skips_reprobing_an_unchanged_file(), generate_test_clip(), import_one(), import_one_enqueues_a_transcode_job_when_transcode_is_enabled() (+6 more)

### Community 16 - "find_relinkable_episode_files"
Cohesion: 0.20
Nodes (13): collect_video_files(), deterministic_filename(), deterministic_movie_filename(), find_relinkable_episode_files(), find_relinkable_episode_files_finds_a_file_with_no_tracked_row(), has_cjk(), ProbeFields, relink_episode_files() (+5 more)

### Community 17 - "fetch_candidates"
Cohesion: 0.17
Nodes (12): fetch_candidates(), infer_has_english_audio(), is_anime(), load_quality_profile(), load_quality_profile_applies_a_stored_override(), load_quality_profile_uses_defaults_for_the_seeded_empty_weights_row(), passes_relevance_filter(), release_sort_key() (+4 more)

### Community 18 - "import_season_pack"
Cohesion: 0.43
Nodes (7): import_season_pack(), import_season_pack_fails_when_nothing_in_it_matches_a_tracked_episode(), import_season_pack_imports_every_file_and_marks_episodes_owned(), import_season_pack_skips_an_upgrade_locked_episode_even_with_a_lower_scoring_existing_release(), import_season_pack_skips_episodes_that_already_have_a_better_file(), SeasonPackImportOutcome, seeded_season_pack_conn()

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `TranscodeConfig` connect `transcode/mod.rs` to `config.rs`, `Result`, `process_pending_grabs`, `import_one`, `import_season_pack`?**
  _High betweenness centrality (0.462) - this node is a cross-community bridge._
- **Why does `SearchCycleStats` connect `execute_search_targets` to `scheduler.rs`, `api/mod.rs`?**
  _High betweenness centrality (0.316) - this node is a cross-community bridge._
- **Why does `AppState` connect `api/mod.rs` to `transcode/mod.rs`?**
  _High betweenness centrality (0.195) - this node is a cross-community bridge._
- **Should `scheduler.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.0666049953746531 - nodes in this community are weakly interconnected._
- **Should `api/mod.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.08527131782945736 - nodes in this community are weakly interconnected._
- **Should `enumerate_search_targets` be split into smaller, more focused modules?**
  _Cohesion score 0.12643678160919541 - nodes in this community are weakly interconnected._
- **Should `rss.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.06570048309178744 - nodes in this community are weakly interconnected._