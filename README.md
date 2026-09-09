# breadarr

A single-daemon Rust replacement for the Sonarr + Radarr + Prowlarr stack: one process, one database, and a small, opinionated set of behaviors instead of a fully general, endlessly-pluggable indexer/automation platform. If Sonarr/Radarr/Prowlarr's flexibility is more than you need and you're fine with a small hardcoded set of sources and a terminal UI in exchange for a much lighter footprint, this is built for you.

- **`breadarrd`**: the daemon. Watches sources, matches releases to your library, scores and grabs candidates, imports completed downloads, probes them for real ground-truth quality, and refreshes Jellyfin. Runs as a `systemd --user` service.
- **`breadarr-tui`**: a terminal client (ratatui) that talks to `breadarrd`'s local HTTP API. No web UI, on purpose.

## Distribution

breadarr is a **homelab** product. It is distributed through bakery (`bakery install breadarr`) and is **not** baked into the BOS ISO. It is not a GTK/desktop-shell app, has no bos-settings panel, and does not subscribe to or emit bread events. bread's `KNOWN_APPS` list reserves the `"arr"` app id for a possible future integration; this repo does not emit on that id.

## Why this exists

Sonarr + Radarr + Prowlarr is three separate services, three databases, three web UIs, and a lot of setup surface for one workflow: watch for releases, pick the best one, download it, file it correctly, tell Jellyfin. breadarr collapses that into a single daemon, with a few specific problems solved directly rather than configured around:

- **Wrong default audio track**: a common pattern on some trackers is "Italian track 1, English track 2." breadarr detects this after download and remuxes the English track to default via `mkvmerge`, instead of scoring the release down or grabbing something worse. The same fix can be swept across the *existing* library after the fact (`remux-backlog`), not just applied to new grabs.
- **Anime numbering**: absolute episode numbers get resolved to season/episode via a bundled AniDB↔TVDB↔TMDB mapping, and anime is allowed to fall back to Japanese-only audio when no English release exists (every other content gate requires English audio).
- **Fuzzy title matching**: release titles are matched to your monitored library via a local ONNX embedding model (`all-MiniLM-L6-v2`, CPU-only, no GPU dependency), not exact-string matching. Low-confidence matches land in a review queue instead of silently grabbing the wrong show. A token-overlap sanity gate also blocks the model's one known failure mode (see [Known limitations](#known-limitations)) from reaching the review queue at all, not just from auto-matching.
- **Library normalization**: a library-scan mode matches existing folders to TVDB/TMDB and renames them down to a clean `Title (Year)` form, stripping release-group tags, quality markers, and season/episode cruft that shouldn't be in a folder name.
- **Season packs**: a batch release (a whole season, a multi-episode range) is grabbed and split correctly: each file inside is matched to its own tracked episode by re-parsing its own filename, then imported individually, with quality compared per-episode against whatever's already owned.
- **Media intelligence**: every library file gets `ffprobe`d on import (and, incrementally, in the background for the existing library), so breadarr knows its actual resolution/codec/audio/subtitle makeup, not just what the release title claimed, and can flag files that don't match up.
- **Quality upgrades keep happening after import**: a separate, slow-cadence background cycle periodically re-checks already-owned, monitored episodes/movies against what's currently searchable, and re-grabs when a candidate beats the current file's score by more than a configurable margin (repacks/propers always supersede regardless of margin). The existing hardlink-swap-on-import logic handles the actual file replacement, so this is purely a new trigger, on its own interval and budget (`upgrade_*` in `[sources]`) so it never competes with missing-content search for request budget. Its limited per-cycle budget is spent on `media_file_probe`-flagged files first (verified under-quality, missing subtitles, non-English default audio, or a probe/decode failure) before it's spent uniformly across everything else owned.
- **Manual release picker**: for the cases auto-grab and the review queue don't handle well, press `c` on a selected show/episode/movie in the TUI to see the same scored (or gate-rejected, with the reason) candidate list the automatic pipeline would have seen, and grab one by hand.

## Sources

- **nyaa.si** (RSS): anime TV, polled continuously. Full-auto, no request budget concerns (it's a plain RSS feed).
- **apibay.org** (a community JSON API mirror of The Pirate Bay's search): the primary search-driven source for general TV and movies. Unlike 1337x this is a genuine machine-readable API, needs no HTML scraping, and its search actually ranks by relevance rather than pure seeder count, which matters a lot for titles made of common words.
- **torrents-csv** (JSON DHT-dump search): first general-content fallback when TPB fails or returns nothing. Same hash-to-magnet grab as TPB; covers movies and TV.
- **YTS** (JSON `list_movies` API via `yts.lt`; `yts.mx` no longer resolves): movie-only fallback after TPB. Each hit expands into one candidate per quality so the scorer sees 720p/1080p/2160p separately.
- **1337x** (scraped HTML, via community mirrors): last-resort general-content fallback after the JSON sources. 1337x's main domain is Cloudflare-protected and has a ban history, so requests are round-robined across mirrors with automatic cooldown/backoff on failures, jittered between searches, and rate-limit responses are honored explicitly.
- **nyaa.si search mode**: anime movies specifically route here instead of the general-content chain, since nyaa is the safe, official-RSS-interface target and gives materially better results for anime content.

Search-driven sources share one per-cycle request budget (default: 5 searches per 30-minute cycle). General content tries TPB, then YTS (movies) / torrents-csv, then 1337x; anime movies go to nyaa search. Kept conservative since 1337x has a ban history and the goal is steady backlog clearing, not maximum throughput. A whole cycle failing outright backs off the *next cycle's* interval (1h to 2h to 4h, capped), on top of each source's own per-mirror cooldowns.

## Setup

Install via bakery (`bakery install breadarr`) on a homelab host, or build from this repo. breadarr is not on the BOS ISO.

1. Copy `config.example.toml` to `~/.config/breadarr/breadarrd.toml` and fill in:
   - qBittorrent WebUI URL/credentials
   - Jellyfin URL + API key
   - TVDB API key ([thetvdb.com](https://thetvdb.com/dashboard/account/apikeys), "Fan/Personal" tier)
   - TMDB API Read Access Token ([themoviedb.org](https://www.themoviedb.org/settings/api), v4 auth, not the shorter v3 key)
   - Optionally, a Gotify-shaped `notifications.webhook_url` for push notifications, and `daemon.api_token` if `listen_addr` will ever be bound to more than loopback.
2. Install `mkvtoolnix-cli` and `ffmpeg` on the host (breadarr runs natively, not in Docker).
3. Install the systemd user unit (`packaging/systemd/breadarrd.service`) to `~/.config/systemd/user/`, then:
   ```
   loginctl enable-linger $USER   # so it runs without an active login session
   systemctl --user daemon-reload
   systemctl --user enable --now breadarrd
   ```
4. Point `breadarr-tui` at the same config (it reads `daemon.listen_addr` to find the API) and run it.

## Using the TUI

`Tab` / `Shift+Tab` cycle Library / History / Review / Add / Stuck / Calendar / Health / Profiles; `1`–`8` jump straight to a tab. `j`/`k` or arrows move, `g`/`G` jump to first/last, `PgUp`/`PgDn` page, `Enter` opens detail or runs a search, `Esc` backs out. `?` is the full key list.

- **Review**: `a` approves and `r` rejects a pending low-confidence title match. The tab title badges the pending count. Check this periodically, especially early on.
- **Library**: `/` incrementally filters the list by title; `f`/`o` cycle kind-filter and sort. With an item's detail open: `s` triggers an immediate search-now pass for that item's backlog; `n` jumps to the next missing monitored episode; `m`/`e`/`S` toggle monitored on the show/episode/season respectively; `x` (confirm with a second `x`) removes the item from tracking without touching files on disk; `d` (confirm with a second `d`) deletes a bad imported file from disk and clears its tracking, freeing the episode/movie to be re-grabbed on the next cycle, the redownload path for a file that turned out to be wrong or broken; `c` fetches the manual release picker for the selected episode/movie, `Enter` grabs the highlighted candidate, `Esc` cancels. Movies (no episode list) show a summary pane with the same keys.
- **Stuck**: surfaces grabs that look stalled (no download progress advancing, or missing from qBittorrent) before the daemon's own auto-fail timers would catch them. `Enter` jumps to that show.
- **Calendar**: upcoming/recently-aired episodes in a roughly week-either-side window. `Enter` opens the matching episode.
- **Health**: daemon cycle outcomes plus the library-health report (see below), scrollable.
- **Profiles**: quality-profile weight axes; open a profile and edit a weight in place.

## Operational notes

- `curl http://127.0.0.1:7879/health` is liveness-only (is the process up). Cycle outcomes live at `/health/detail` (requires `Authorization: Bearer <token>` when `daemon.api_token` is set).
- `curl http://127.0.0.1:7879/library/health` (or the TUI's Health tab) reports corrupt files, under-1080p files, missing-English-audio files, non-English-default-audio files, missing-subtitle files, duplicate-file groups, and library-wide summary stats (codec breakdown, resolution distribution, subtitle coverage %), all derived from `ffprobe` data, not release-title claims.
- The API is plaintext HTTP: there is no TLS. Binding `listen_addr` off loopback requires a non-empty `daemon.api_token`.
- If `daemon.api_token` is set, every route except `/health` requires `Authorization: Bearer <token>`.
- Library normalization is a manual, explicit action (not run automatically against your files): `breadarrd debug-scan-tv <root-dir>` / `breadarrd debug-scan-movies <root-dir>`.
- `breadarrd remux-backlog` sweeps the whole library for files with a non-English default audio track and applies the same track-promotion fix used automatically on fresh imports: a one-time (or occasional) pass against files that predate the fix, or were imported before breadarr started tracking them.
- `breadarrd probe-library` backfills `ffprobe` data for the whole library in one run (the running daemon does this incrementally, a bounded batch per hour, so it doesn't stall the grab/import/search cycles; this command is for getting it all done immediately instead).
- `breadarrd verify-library` runs the expensive full-decode corruption check (`ffmpeg -xerror`, actually decoding every frame) against every file whose cheap header probe succeeded but hasn't been decode-verified yet. This is opt-in and can take minutes per file, so, unlike `probe-library`, it's never run automatically by any ticker; run it by hand (or on a cron) whenever you want a real, not-just-header-parseable confirmation the library is intact.
- Other `debug-*` subcommands are diagnostics for exercising one piece of the pipeline directly: run `breadarrd <name>` with no args to see its usage. Currently: `debug-qbit-add`, `debug-qbit-list`, `debug-jellyfin-refresh`, `debug-tvdb-add`, `debug-tvdb-search`, `debug-anime-map-refresh`, `debug-match-title`, `debug-grab-cycle`, `debug-import-cycle`, `debug-1337x-search`, `debug-scan-tv`, `debug-scan-movies`, `debug-search-show` (a manually-triggered, unthrottled search pass over one already-tracked title's whole backlog), `debug-reconcile-report` (dry-run of the disk-reconciliation pass, safe to run against a freshly-restored or otherwise suspect database before trusting the hourly ticker with it unattended).
- `breadarrd transcode-library` backfills AV1 transcode over existing library files (requires `[transcode] enabled = true`). `breadarrd retranscode-oversized` re-encodes already-AV1 files that landed larger than the current ceiling. `breadarrd relink-orphaned-files` reattaches episode files on disk that tracking lost.
- The embedding model (~90MB) downloads automatically on first run.
- The database is backed up (with its WAL/SHM sidecars) to `<db-dir>/backups/` on every daemon startup, keeping the 5 most recent copies: there's no migration framework, so this stands in for the pre-upgrade backup Sonarr/Radarr do on every schema change.
- An hourly background pass reconciles tracked `episode_file` paths against what's actually on disk: a file renamed or transcoded in place (e.g. Tdarr converting codec/container) gets its path repaired rather than being wrongly treated as deleted; a file genuinely gone gets cleared from tracking so it becomes searchable again. A circuit breaker refuses to touch anything if an anomalous fraction of the library looks missing at once (the classic false signal of an offline mount), rather than mass-clearing tracking for files that are actually still there.

## Known limitations

- No subtitle generation yet: a Whisper-based reimplementation of an existing external tool is planned (see [Roadmap](#roadmap)), with the schema (`episode_file.subtitle_status`) already reserved for it.
- No Sonarr/Radarr API compatibility shim yet, so tools expecting that API (e.g. Overseerr/Seerr) can't integrate directly; still a future idea (see [Roadmap](#roadmap)).
- Quality-profile weights are loaded and editable (Profiles tab). `min_seeders` and the group denylist are still hardcoded defaults, not user-configurable.
- The title-matching embedding model (all-MiniLM-L6-v2) doesn't actually discriminate between unrelated romanized-Japanese titles: it clusters any two romaji strings as "similar foreign text" regardless of content (verified live: several unrelated anime auto-matched at >0.85 confidence against a handful of "attractor" shows with zero real relation). A token-overlap gate (`MIN_TOKEN_OVERLAP` in `matcher/mod.rs`) blocks this from both auto-matching *and* reaching the review queue, but the underlying model limitation is a workaround, not a fix.
- The search loop's per-cycle budget is intentionally conservative; raising it trades faster backlog clearing for more request volume against 1337x specifically, which has a ban history. The upgrade-search loop shares the same underlying sources and the same conservatism applies.

## Roadmap

Everything above is shipped and running. This section is the honest, disciplined version of "what would this become if taken all the way": grounded in subsystems that already exist, not a wishlist. Nothing here contradicts the project's two foundational design choices (a small hardcoded set of sources, not a general indexer-plugin architecture; a TUI, not a web UI); anything that would require reconsidering either is flagged as such.

### Medium-term (closes the loop on data already being collected)

- **Tdarr hand-off.** `media_file_probe` already has `video_codec`, `container_bitrate`, `video_bitrate`, and `hdr` per file: everything needed to generate a "these files are still H.264/high-bitrate and are good AV1-transcode candidates" list without re-scanning the filesystem. Whether that's a report the user acts on manually or a direct trigger into the existing Tdarr pipeline is a judgment call for whenever this is built, but the *data* side of this is already sitting in the database, unused.
- **Smart-library auto-upgrade daemon.** The combination of the quality-upgrade loop above, corruption detection, and duplicate-group detection (`library/health`'s `duplicate_groups`) is most of what's needed for a low-touch "keep the library clean" background process: re-grab flagged files, quarantine/remove confirmed-corrupt ones, resolve duplicate groups down to the single best-scoring file. This is a composition of pieces above, not a new subsystem; the risk is entirely in getting the "don't touch a file a human hasn't implicitly signed off on" judgment calls right, which argues for notification-first (as the webhook infrastructure already supports) before any auto-delete behavior.
- **Tracker/release-group reliability scoring.** `torrent_fetch` is a permanent, unopinionated log of every torrent ever grabbed, independent of what happened to it afterward: cross-referencing it against `release.status` (`imported` vs `failed`) and `media_file_probe`'s post-import quality flags would let a release-group or per-tracker track record feed back into `scoring/score.rs` as a real weighted axis, instead of only the static `group_allowlist`/`group_denylist` lists that exist today. This is genuinely unexploited surface area: the raw data already exists and nothing reads it.
- **Mining the raw ffprobe JSON.** `media_file_probe.raw_ffprobe_json` is kept verbatim specifically so nothing has to be re-probed when a new use is found for a field that doesn't have its own column yet (chapter markers, encoder tags, less-common stream metadata). The near-term backlog above doesn't need it; whatever comes after does, and it's already there.

### Longer-term (larger, still-plausible extensions)

- **Learning from review-queue decisions.** Every approve/reject in the review queue is already a labeled example of "was this match actually correct." Logging outcomes (not just acting on them) and periodically comparing auto-tuned per-library confidence thresholds against the fixed `AUTO_MATCH_CONFIDENCE`/`MIN_TOKEN_OVERLAP` constants in `matcher/mod.rs` is a plausible way to let the matcher get measurably better over time for *this* library's actual title vocabulary: anime is the library segment most exposed to the token-overlap workaround today, so it's also the segment most likely to benefit first. This is a genuine judgment call (a wrong auto-tune silently degrades match quality with no review-queue visibility into it happening), not a slam dunk; worth prototyping as an offline analysis of logged decisions before it's ever allowed to write back to live thresholds.
- **Seerr/Overseerr compatibility shim, then real request fulfillment.** A Sonarr/Radarr v3-API-compatible shim is the integration path Seerr's client code expects. Once that shim exists, the natural next step isn't just protocol compatibility: it's tracking *fulfillment* end-to-end (a request maps to a `media_item`, which maps to `release`/`event_history` rows that already record exactly when and how it was grabbed and imported), giving a requester real status instead of Seerr's own best-effort polling.
- **Whisper subtitle generation.** Fully speced already: reimplement the existing external Python/Whisper tool's exact behavior, "Full Subtitles" (everything transcribed) and "Foreign Parts" (segments where a translate-pass diverges from the transcribe-pass, via text-similarity diff at a 0.80 threshold) SRT tracks, muxed with `mkvmerge`, but gated and on-demand (triggered post-import only when a file lacks subtitles and has non-English/fallback audio), unlike the external tool's full-library batch sweep. `episode_file.subtitle_status` is already wired through the schema for this. The open technical questions are a Rust equivalent of Python's `difflib.SequenceMatcher` for the similarity diff, and which Whisper-in-Rust GPU backend story actually works well across the range of consumer GPU hardware this runs on in practice (CUDA/Metal/Vulkan-centric crates like `whisper-rs` have no strong story for e.g. Intel Arc or other OpenVINO-friendly hardware).

### Deliberately not on this list

A general indexer-plugin system (Prowlarr-style) and a web UI were both explicit, considered rejections early in this project, not gaps: a hardcoded, well-understood set of sources and a terminal-only client are the point, not a limitation to eventually fix. Nothing above should be read as walking either decision back; if a future need ever seriously challenges one of them, that deserves its own explicit reconsideration, not a quiet reversal buried in a roadmap item.

## License

MIT: see `LICENSE`.
