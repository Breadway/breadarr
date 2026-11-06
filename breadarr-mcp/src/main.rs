// breadarr-mcp is an MCP stdio server exposing breadarrd's HTTP API to agents.
// Protocol: newline-delimited JSON-RPC 2.0 on stdin/stdout (MCP stdio
// transport). Logs go to stderr; stdout carries only protocol frames.
//
// Configuration: same file as breadarrd/breadarr-tui
// (`~/.config/breadarr/breadarrd.toml`, `daemon.listen_addr` for the base
// URL and `daemon.api_token` for auth), with env overrides:
//   BREADARR_URL        e.g. http://127.0.0.1:7879 (overrides listen_addr)
//   BREADARR_API_TOKEN  (overrides daemon.api_token)
//
// Destructive tools are deliberately narrow: `remove_media` (stop managing,
// files on disk untouched, same as the daemon) requires confirm=true, and
// there is no file-delete tool in this v1.

use std::env;

use breadarr_shared::dto::ReleaseCandidate;
use breadarr_shared::{Config, DaemonClient};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MAX_TEXT_CHARS: usize = 15_000;

async fn dispatch(client: &DaemonClient, cfg: &Config, name: &str, args: serde_json::Map<String, Value>) -> Value {
    match name {
        "library_list" => h_library_list(client, args).await,
        "library_detail" => h_library_detail(client, args).await,
        "search_catalog" => h_search_catalog(client, args).await,
        "add_series" => h_add_series(client, cfg.clone(), args).await,
        "add_movie" => h_add_movie(client, cfg.clone(), args).await,
        "search_now" => h_search_now(client, args).await,
        "movie_candidates" => h_movie_candidates(client, args).await,
        "episode_candidates" => h_episode_candidates(client, args).await,
        "grab_movie_candidate" => h_grab_movie_candidate(client, args).await,
        "grab_episode_candidate" => h_grab_episode_candidate(client, args).await,
        "review_list" => h_review_list(client, args).await,
        "review_approve" => h_review_approve(client, args).await,
        "review_reject" => h_review_reject(client, args).await,
        "releases_list" => h_releases_list(client, args).await,
        "stuck_report" => h_stuck_report(client, args).await,
        "calendar" => h_calendar(client, args).await,
        "library_health" => h_library_health(client, args).await,
        "health_detail" => h_health_detail(client, args).await,
        "set_media_monitored" => h_set_media_monitored(client, args).await,
        "set_episode_monitored" => h_set_episode_monitored(client, args).await,
        "remove_media" => h_remove_media(client, args).await,
        _ => err_text(format!("unknown tool: {name}")),
    }
}

fn tools_list() -> Vec<Value> {
    vec![
        tool(
            "library_list",
            "List tracked shows and movies (id, kind, title, year, monitored, episode/missing counts). Filter by kind or a case-insensitive title substring.",
            obj_schema(
                vec![
                    ("kind", prop("string", "Filter: 'series' or 'movie'. Omit for both.")),
                    ("query", prop("string", "Case-insensitive title substring filter. Omit for all.")),
                    ("monitored_only", prop("boolean", "Only return monitored items.")),
                ],
                vec![],
            ),
        ),
        tool(
            "library_detail",
            "Full detail for one tracked item: metadata plus its episode list (id, season/episode numbers, monitored, has_file). Movies have no episode rows.",
            obj_schema(
                vec![("media_id", prop("integer", "media_item id from library_list"))],
                vec!["media_id"],
            ),
        ),
        tool(
            "search_catalog",
            "Search TVDB (kind series) or TMDB (kind movie) for the external id needed by add_series/add_movie. Returns external_id, title, year per hit. This searches the catalog, not your library.",
            obj_schema(
                vec![
                    ("q", prop("string", "Title to search for")),
                    ("kind", prop("string", "'series' (default) or 'movie'")),
                ],
                vec!["q"],
            ),
        ),
        tool(
            "add_series",
            "Track a new TV series. Find tvdb_id via search_catalog first. Aliases default to empty (daemon recovers TVDB aliases itself). root_folder defaults to the configured series root.",
            obj_schema(
                vec![
                    ("tvdb_id", prop("string", "TVDB series id from search_catalog")),
                    ("title", prop("string", "Series title")),
                    ("year", prop("integer", "First-air year, omit if unknown")),
                    ("root_folder", prop("string", "Library folder. Omit for the configured default.")),
                    ("aliases", prop_array("Alternate titles that should also match this series")),
                ],
                vec!["tvdb_id", "title"],
            ),
        ),
        tool(
            "add_movie",
            "Track a new movie. Find tmdb_id via search_catalog with kind movie. root_folder defaults to the configured movies root.",
            obj_schema(
                vec![
                    ("tmdb_id", prop("string", "TMDB movie id from search_catalog")),
                    ("title", prop("string", "Movie title")),
                    ("year", prop("integer", "Release year, omit if unknown")),
                    ("root_folder", prop("string", "Library folder. Omit for the configured default.")),
                ],
                vec!["tmdb_id", "title"],
            ),
        ),
        tool(
            "search_now",
            "Trigger an immediate unthrottled search pass over one item's whole backlog. Slow (up to minutes): same 600s allowance as the TUI.",
            obj_schema(
                vec![("media_id", prop("integer", "media_item id"))],
                vec!["media_id"],
            ),
        ),
        tool(
            "movie_candidates",
            "Scored (or gate-rejected, with reason) release candidates for a movie: the manual picker. Slow, runs a real search.",
            obj_schema(
                vec![
                    ("media_id", prop("integer", "media_item id of the movie")),
                    ("limit", prop("integer", "Max candidates 1-50, default 15")),
                ],
                vec!["media_id"],
            ),
        ),
        tool(
            "episode_candidates",
            "Scored (or gate-rejected, with reason) release candidates for one episode: the manual picker. Slow, runs a real search. Use library_detail to find the episode id.",
            obj_schema(
                vec![
                    ("episode_id", prop("integer", "episode id from library_detail")),
                    ("limit", prop("integer", "Max candidates 1-50, default 15")),
                ],
                vec!["episode_id"],
            ),
        ),
        tool(
            "grab_movie_candidate",
            "Grab one manual-picker candidate for a movie. Copy source_id/raw_title/link/guid verbatim from a movie_candidates entry.",
            obj_schema(
                vec![
                    ("media_id", prop("integer", "media_item id of the movie")),
                    ("source_id", prop("integer", "source_id from the candidate")),
                    ("raw_title", prop("string", "raw_title from the candidate")),
                    ("link", prop("string", "link/magnet from the candidate")),
                    ("guid", prop("string", "guid from the candidate")),
                ],
                vec!["media_id", "source_id", "raw_title", "link", "guid"],
            ),
        ),
        tool(
            "grab_episode_candidate",
            "Grab one manual-picker candidate for an episode. Copy source_id/raw_title/link/guid verbatim from an episode_candidates entry.",
            obj_schema(
                vec![
                    ("episode_id", prop("integer", "episode id")),
                    ("source_id", prop("integer", "source_id from the candidate")),
                    ("raw_title", prop("string", "raw_title from the candidate")),
                    ("link", prop("string", "link/magnet from the candidate")),
                    ("guid", prop("string", "guid from the candidate")),
                ],
                vec!["episode_id", "source_id", "raw_title", "link", "guid"],
            ),
        ),
        tool(
            "review_list",
            "Pending low-confidence title matches (id, raw release title, candidate library title, confidence). Check periodically; approve or reject by id.",
            obj_schema(vec![], vec![]),
        ),
        tool(
            "review_approve",
            "Approve a pending review item by id: grabs the release.",
            obj_schema(
                vec![("review_id", prop("integer", "review_queue id from review_list"))],
                vec!["review_id"],
            ),
        ),
        tool(
            "review_reject",
            "Reject a pending review item by id: drops it without grabbing.",
            obj_schema(
                vec![("review_id", prop("integer", "review_queue id from review_list"))],
                vec!["review_id"],
            ),
        ),
        tool(
            "releases_list",
            "Recent grab history (media title, raw release title, score, status), newest first.",
            obj_schema(
                vec![("limit", prop("integer", "Max rows 1-200, default 50"))],
                vec![],
            ),
        ),
        tool(
            "stuck_report",
            "Why content is missing: stalled grabs (grabbed >48h, pre-dating the daemon auto-fail), review queue depth, and search targets failing so long their backoff maxed out.",
            obj_schema(vec![], vec![]),
        ),
        tool(
            "calendar",
            "Episodes airing roughly a week back to three weeks out (media id/title, season/episode, air date, monitored, has_file).",
            obj_schema(vec![], vec![]),
        ),
        tool(
            "library_health",
            "ffprobe-derived library health: summary stats plus the first 10 paths per flag category (corrupt, under-1080p, missing subtitles, non-English audio, non-English default audio, duplicates).",
            obj_schema(vec![], vec![]),
        ),
        tool(
            "health_detail",
            "Daemon cycle status: last grab/import/search/upgrade/transcode outcomes plus search_halted (backoff ceiling hit). Liveness only is GET /health on the daemon itself.",
            obj_schema(vec![], vec![]),
        ),
        tool(
            "set_media_monitored",
            "Monitor or unmonitor a whole show/movie by media id.",
            obj_schema(
                vec![
                    ("media_id", prop("integer", "media_item id")),
                    ("monitored", prop("boolean", "true to monitor, false to unmonitor")),
                ],
                vec!["media_id", "monitored"],
            ),
        ),
        tool(
            "set_episode_monitored",
            "Monitor or unmonitor a single episode by episode id.",
            obj_schema(
                vec![
                    ("episode_id", prop("integer", "episode id from library_detail")),
                    ("monitored", prop("boolean", "true to monitor, false to unmonitor")),
                ],
                vec!["episode_id", "monitored"],
            ),
        ),
        tool(
            "remove_media",
            "Stop tracking a show/movie (daemon cascade-deletes its episodes/releases; files on disk are NOT touched, same as Sonarr/Radarr default). Requires confirm=true.",
            obj_schema(
                vec![
                    ("media_id", prop("integer", "media_item id")),
                    ("confirm", prop("boolean", "Must be true to delete")),
                ],
                vec!["media_id", "confirm"],
            ),
        ),
    ]
}

// ---------- handlers ----------

async fn h_library_list(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let items = match client.list_media().await {
        Ok(v) => v,
        Err(e) => return err_text(format!("library_list failed: {e:#}")),
    };
    let kind = opt_str(&args, "kind").to_lowercase();
    let query = opt_str(&args, "query").to_lowercase();
    let monitored_only = opt_bool(&args, "monitored_only").unwrap_or(false);
    let out: Vec<Value> = items
        .into_iter()
        .filter(|m| {
            if !kind.is_empty() && m.kind.to_lowercase() != kind {
                return false;
            }
            if monitored_only && !m.monitored {
                return false;
            }
            if !query.is_empty() && !m.title.to_lowercase().contains(&query) {
                return false;
            }
            true
        })
        .map(|m| {
            json!({
                "id": m.id, "kind": m.kind, "title": m.title, "year": m.year,
                "monitored": m.monitored, "episode_count": m.episode_count,
                "missing_count": m.missing_count,
            })
        })
        .collect();
    ok_json(&json!({"total": out.len(), "items": out}))
}

async fn h_library_detail(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "media_id") else {
        return err_text("media_id is required");
    };
    match client.media_detail(id).await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("library_detail failed: {e:#}")),
    }
}

async fn h_search_catalog(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let q = opt_str(&args, "q");
    if q.is_empty() {
        return err_text("q is required");
    }
    let kind = opt_str(&args, "kind");
    let kind = if kind.is_empty() { "series" } else { &kind };
    let res = if kind.eq_ignore_ascii_case("movie") {
        client.search_movies(&q).await
    } else if kind.eq_ignore_ascii_case("series") {
        client.search_series(&q).await
    } else {
        return err_text("kind must be 'series' or 'movie'");
    };
    match res {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("search_catalog failed: {e:#}")),
    }
}

async fn h_add_series(
    client: &DaemonClient,
    cfg: Config,
    args: serde_json::Map<String, Value>,
) -> Value {
    let tvdb_id = opt_str(&args, "tvdb_id");
    let title = opt_str(&args, "title");
    if tvdb_id.is_empty() || title.is_empty() {
        return err_text("tvdb_id and title are required (use search_catalog first)");
    }
    let root_folder = opt_str(&args, "root_folder");
    let root_folder = if root_folder.is_empty() {
        cfg.default_root_folder().to_string_lossy().into_owned()
    } else {
        root_folder
    };
    let aliases = opt_str_list(&args, "aliases");
    let req = breadarr_shared::dto::AddSeriesRequest {
        tvdb_id,
        title,
        year: opt_i64(&args, "year"),
        aliases,
        root_folder,
    };
    match client.add_series(&req).await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("add_series failed: {e:#}")),
    }
}

async fn h_add_movie(
    client: &DaemonClient,
    cfg: Config,
    args: serde_json::Map<String, Value>,
) -> Value {
    let tmdb_id = opt_str(&args, "tmdb_id");
    let title = opt_str(&args, "title");
    if tmdb_id.is_empty() || title.is_empty() {
        return err_text("tmdb_id and title are required (use search_catalog with kind movie first)");
    }
    let root_folder = opt_str(&args, "root_folder");
    let root_folder = if root_folder.is_empty() {
        cfg.movies_root_folder().to_string_lossy().into_owned()
    } else {
        root_folder
    };
    let req = breadarr_shared::dto::AddMovieRequest {
        tmdb_id,
        title,
        year: opt_i64(&args, "year"),
        root_folder,
    };
    match client.add_movie(&req).await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("add_movie failed: {e:#}")),
    }
}

async fn h_search_now(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "media_id") else {
        return err_text("media_id is required");
    };
    match client.search_now(id).await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("search_now failed: {e:#}")),
    }
}

async fn h_movie_candidates(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "media_id") else {
        return err_text("media_id is required");
    };
    match client.movie_candidates(id).await {
        Ok(v) => ok_json(&json!(project_candidates(&v, clamp_limit(opt_i64(&args, "limit"), 15)))),
        Err(e) => err_text(format!("movie_candidates failed: {e:#}")),
    }
}

async fn h_episode_candidates(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "episode_id") else {
        return err_text("episode_id is required");
    };
    match client.episode_candidates(id).await {
        Ok(v) => ok_json(&json!(project_candidates(&v, clamp_limit(opt_i64(&args, "limit"), 15)))),
        Err(e) => err_text(format!("episode_candidates failed: {e:#}")),
    }
}

async fn h_grab_movie_candidate(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(media_id) = req_i64(&args, "media_id") else {
        return err_text("media_id is required");
    };
    let Some(c) = candidate_from_args(&args) else {
        return err_text("source_id, raw_title, link and guid are all required (copy from movie_candidates)");
    };
    match client.grab_movie_candidate(media_id, &c).await {
        Ok(()) => ok_text("grabbed"),
        Err(e) => err_text(format!("grab_movie_candidate failed: {e:#}")),
    }
}

async fn h_grab_episode_candidate(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(episode_id) = req_i64(&args, "episode_id") else {
        return err_text("episode_id is required");
    };
    let Some(c) = candidate_from_args(&args) else {
        return err_text("source_id, raw_title, link and guid are all required (copy from episode_candidates)");
    };
    match client.grab_episode_candidate(episode_id, &c).await {
        Ok(()) => ok_text("grabbed"),
        Err(e) => err_text(format!("grab_episode_candidate failed: {e:#}")),
    }
}

async fn h_review_list(client: &DaemonClient, _args: serde_json::Map<String, Value>) -> Value {
    match client.review_queue().await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("review_list failed: {e:#}")),
    }
}

async fn h_review_approve(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "review_id") else {
        return err_text("review_id is required");
    };
    match client.approve_review(id).await {
        Ok(()) => ok_text("approved and grabbed"),
        Err(e) => err_text(format!("review_approve failed: {e:#}")),
    }
}

async fn h_review_reject(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "review_id") else {
        return err_text("review_id is required");
    };
    match client.reject_review(id).await {
        Ok(()) => ok_text("rejected"),
        Err(e) => err_text(format!("review_reject failed: {e:#}")),
    }
}

async fn h_releases_list(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let limit = clamp_limit(opt_i64(&args, "limit"), 50).max(1) as usize;
    match client.releases().await {
        Ok(v) => {
            let total = v.len();
            let items: Vec<Value> = v
                .into_iter()
                .take(limit.min(200))
                .map(|r| {
                    json!({
                        "id": r.id, "media_title": r.media_title, "raw_title": r.raw_title,
                        "score": r.score, "status": r.status, "grabbed_at": r.grabbed_at,
                    })
                })
                .collect();
            ok_json(&json!({"total": total, "items": items}))
        }
        Err(e) => err_text(format!("releases_list failed: {e:#}")),
    }
}

async fn h_stuck_report(client: &DaemonClient, _args: serde_json::Map<String, Value>) -> Value {
    match client.stuck().await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("stuck_report failed: {e:#}")),
    }
}

async fn h_calendar(client: &DaemonClient, _args: serde_json::Map<String, Value>) -> Value {
    match client.calendar().await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("calendar failed: {e:#}")),
    }
}

async fn h_library_health(client: &DaemonClient, _args: serde_json::Map<String, Value>) -> Value {
    match client.library_health().await {
        Ok(v) => ok_json(&json!({
            "summary": v.summary,
            "counts": {
                "corrupt_files": v.corrupt_files.len(),
                "under_quality_files": v.under_quality_files.len(),
                "no_subtitle_files": v.no_subtitle_files.len(),
                "no_english_audio_files": v.no_english_audio_files.len(),
                "non_english_default_audio_files": v.non_english_default_audio_files.len(),
                "duplicate_groups": v.duplicate_groups.len(),
            },
            "corrupt_files": first_n(&v.corrupt_files, 10),
            "under_quality_files": first_n(&v.under_quality_files, 10),
            "no_subtitle_files": first_n(&v.no_subtitle_files, 10),
            "no_english_audio_files": first_n(&v.no_english_audio_files, 10),
            "non_english_default_audio_files": first_n(&v.non_english_default_audio_files, 10),
            "duplicate_groups": first_n(&v.duplicate_groups, 10),
        })),
        Err(e) => err_text(format!("library_health failed: {e:#}")),
    }
}

async fn h_health_detail(client: &DaemonClient, _args: serde_json::Map<String, Value>) -> Value {
    match client.health_detail().await {
        Ok(v) => ok_json(&serde_json::to_value(&v).unwrap_or(Value::Null)),
        Err(e) => err_text(format!("health_detail failed: {e:#}")),
    }
}

async fn h_set_media_monitored(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "media_id") else {
        return err_text("media_id is required");
    };
    let Some(monitored) = req_bool(&args, "monitored") else {
        return err_text("monitored must be true or false");
    };
    let res = if monitored {
        client.monitor(id).await
    } else {
        client.unmonitor(id).await
    };
    match res {
        Ok(()) => ok_text(if monitored { "monitored" } else { "unmonitored" }),
        Err(e) => err_text(format!("set_media_monitored failed: {e:#}")),
    }
}

async fn h_set_episode_monitored(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "episode_id") else {
        return err_text("episode_id is required");
    };
    let Some(monitored) = req_bool(&args, "monitored") else {
        return err_text("monitored must be true or false");
    };
    let res = if monitored {
        client.monitor_episode(id).await
    } else {
        client.unmonitor_episode(id).await
    };
    match res {
        Ok(()) => ok_text(if monitored { "monitored" } else { "unmonitored" }),
        Err(e) => err_text(format!("set_episode_monitored failed: {e:#}")),
    }
}

async fn h_remove_media(
    client: &DaemonClient,
    args: serde_json::Map<String, Value>,
) -> Value {
    let Some(id) = req_i64(&args, "media_id") else {
        return err_text("media_id is required");
    };
    if !matches!(args.get("confirm"), Some(Value::Bool(true))) {
        return err_text("refusing without confirm=true (files on disk are kept, tracking is deleted)");
    }
    match client.delete_media(id).await {
        Ok(()) => ok_text("removed from tracking (files on disk kept)"),
        Err(e) => err_text(format!("remove_media failed: {e:#}")),
    }
}

// ---------- arg / projection helpers ----------

fn opt_str(args: &serde_json::Map<String, Value>, key: &str) -> String {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_owned()
}

fn opt_i64(args: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    }
}

fn opt_bool(args: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn req_i64(args: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    opt_i64(args, key)
}

fn req_bool(args: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    opt_bool(args, key)
}

fn opt_str_list(args: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn clamp_limit(v: Option<i64>, default: i64) -> i64 {
    let n = v.unwrap_or(default);
    n.clamp(1, 200)
}

fn candidate_from_args(args: &serde_json::Map<String, Value>) -> Option<ReleaseCandidate> {
    let link = opt_str(args, "link");
    let guid = opt_str(args, "guid");
    let raw_title = opt_str(args, "raw_title");
    let source_id = opt_i64(args, "source_id")?;
    if link.is_empty() || guid.is_empty() || raw_title.is_empty() {
        return None;
    }
    Some(ReleaseCandidate {
        raw_title,
        link,
        guid,
        source_id,
        source_name: String::new(),
        seeders: None,
        leechers: None,
        size_bytes: None,
        score: None,
        rejected_reason: None,
        resolution: None,
        is_repack: false,
        is_season_pack: false,
    })
}

fn project_candidates(cands: &[ReleaseCandidate], limit: i64) -> Value {
    let items: Vec<Value> = cands
        .iter()
        .take(limit.max(1) as usize)
        .map(|c| {
            json!({
                "raw_title": c.raw_title, "source_name": c.source_name,
                "source_id": c.source_id, "score": c.score,
                "rejected_reason": c.rejected_reason, "seeders": c.seeders,
                "leechers": c.leechers, "size_bytes": c.size_bytes,
                "resolution": c.resolution, "is_repack": c.is_repack,
                "is_season_pack": c.is_season_pack,
                "link": c.link, "guid": c.guid,
            })
        })
        .collect();
    json!({"total": cands.len(), "items": items})
}

fn first_n<T: serde::Serialize>(v: &[T], n: usize) -> Vec<Value> {
    v.iter()
        .take(n)
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect()
}

// ---------- MCP framing ----------

fn ok_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": clip(text)}]})
}

fn ok_json(v: &Value) -> Value {
    let s = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into());
    ok_text(&s)
}

fn err_text(msg: impl Into<String>) -> Value {
    let msg = msg.into();
    json!({"content": [{"type": "text", "text": clip(&msg)}], "isError": true})
}

fn clip(s: &str) -> String {
    if s.len() > MAX_TEXT_CHARS {
        format!("{}\n...(truncated, {} chars total)", &s[..MAX_TEXT_CHARS], s.len())
    } else {
        s.to_owned()
    }
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    json!({"name": name, "description": desc, "inputSchema": schema})
}

fn obj_schema(props: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    let map: serde_json::Map<String, Value> = props.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    let mut s = json!({"type": "object", "properties": map});
    if !required.is_empty() {
        s["required"] = json!(required);
    }
    s
}

fn prop(kind: &str, desc: &str) -> Value {
    json!({"type": kind, "description": desc})
}

fn prop_array(desc: &str) -> Value {
    json!({"type": "array", "description": desc, "items": {"type": "string"}})
}

fn daemon_client() -> anyhow::Result<(DaemonClient, Config)> {
    let cfg = Config::load()?;
    let base_url = env::var("BREADARR_URL").unwrap_or_else(|_| format!("http://{}", cfg.daemon.listen_addr));
    let token = env::var("BREADARR_API_TOKEN").unwrap_or_else(|_| cfg.daemon.api_token.clone());
    Ok((DaemonClient::new(base_url.trim_end_matches('/'), &token)?, cfg))
}

fn setup_client() -> Result<(DaemonClient, Config), String> {
    match daemon_client() {
        Ok(v) => Ok(v),
        Err(e) => Err(format!("breadarr config/client setup failed: {e:#}")),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("breadarr-mcp {} up", env!("CARGO_PKG_VERSION"));
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("dropping undecodable frame: {e}");
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        if id.is_null() {
            continue; // notification: nothing to answer
        }
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => {
                let version = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05")
                    .to_owned();
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "breadarr-mcp", "version": env!("CARGO_PKG_VERSION")},
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools_list()}),
            "tools/call" => {
                let name = req
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = req
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                match setup_client() {
                    Ok((c, cfg)) => dispatch(&c, &cfg, name, args).await,
                    Err(e) => err_text(e),
                }
            }
            other => {
                let frame = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {other}")}});
                send(&mut stdout, &frame).await?;
                continue;
            }
        };
        let frame = json!({"jsonrpc": "2.0", "id": id, "result": result});
        send(&mut stdout, &frame).await?;
    }
    Ok(())
}

async fn send(stdout: &mut tokio::io::Stdout, frame: &Value) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(frame)?;
    s.push('\n');
    stdout.write_all(s.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_passes_short_strings_through() {
        assert_eq!(clip("hello"), "hello");
    }

    #[test]
    fn clip_truncates_long_strings_with_total() {
        let long = "x".repeat(MAX_TEXT_CHARS * 2);
        let out = clip(&long);
        assert!(out.contains("truncated"));
        assert!(out.contains(&long.len().to_string()));
        assert!(out.len() < long.len());
        assert!(out.starts_with(&long[..MAX_TEXT_CHARS]));
    }

    #[test]
    fn clamp_limit_bounds_and_defaults() {
        assert_eq!(clamp_limit(None, 15), 15);
        assert_eq!(clamp_limit(Some(0), 15), 1);
        assert_eq!(clamp_limit(Some(500), 15), 200);
        assert_eq!(clamp_limit(Some(7), 15), 7);
    }

    #[test]
    fn remove_media_requires_confirm_true() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // No daemon needed: the guard fires before any HTTP call.
            let cfg = Config::default();
            let client = DaemonClient::new("http://127.0.0.1:9", "").unwrap();
            let mut args = serde_json::Map::new();
            args.insert("media_id".into(), json!(123));
            let out = h_remove_media(&client, args.clone()).await;
            assert_eq!(out.get("isError"), Some(&json!(true)));
            args.insert("confirm".into(), json!(false));
            let out = h_remove_media(&client, args.clone()).await;
            assert_eq!(out.get("isError"), Some(&json!(true)));
            let _ = cfg;
        });
    }

    #[test]
    fn candidate_from_args_rejects_partials() {
        let mut args = serde_json::Map::new();
        args.insert("source_id".into(), json!(1));
        args.insert("raw_title".into(), json!("t"));
        assert!(candidate_from_args(&args).is_none());
        args.insert("link".into(), json!("magnet:?x"));
        args.insert("guid".into(), json!("g"));
        let c = candidate_from_args(&args).unwrap();
        assert_eq!(c.source_id, 1);
        assert_eq!(c.raw_title, "t");
    }

    #[test]
    fn tools_list_has_add_and_grab_tools() {
        let names: Vec<String> = tools_list()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        for want in [
            "library_list",
            "search_catalog",
            "add_series",
            "add_movie",
            "search_now",
            "movie_candidates",
            "grab_movie_candidate",
            "grab_episode_candidate",
            "review_approve",
            "remove_media",
        ] {
            assert!(names.contains(&want.to_owned()), "missing tool {want}");
        }
    }

    #[test]
    fn every_tool_schema_is_a_valid_object_schema() {
        for t in tools_list() {
            let schema = t.get("inputSchema").expect("tool without inputSchema");
            assert_eq!(schema.get("type"), Some(&json!("object")));
            assert!(schema.get("properties").and_then(Value::as_object).is_some());
        }
    }
}
