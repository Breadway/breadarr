use std::path::{Component, Path as FsPath, PathBuf};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::{
    AddMovieRequest, AddMovieResponse, AddSeriesRequest, AddSeriesResponse, EpisodeSummary,
    MediaItemDetail, MediaItemSummary, SearchNowResult,
};
use breadarr_shared::Config;
use rusqlite::{params, OptionalExtension};

use crate::api::AppState;
use crate::metadata;

pub async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<MediaItemSummary>>, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.kind, m.title, m.year, m.monitored,
                    (SELECT count(*) FROM episode e WHERE e.media_item_id = m.id) AS episode_count,
                    (SELECT count(*) FROM episode e WHERE e.media_item_id = m.id AND e.monitored = 1 AND e.has_file = 0) AS missing_count
             FROM media_item m ORDER BY m.title",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MediaItemSummary {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                year: row.get(3)?,
                monitored: row.get::<_, i64>(4)? != 0,
                episode_count: row.get(5)?,
                missing_count: row.get(6)?,
            })
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(Json(rows))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<MediaItemDetail>, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let row = conn
        .query_row(
            "SELECT kind, title, year, monitored, root_folder FROM media_item WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    let Some((kind, title, year, monitored, root_folder)) = row else {
        return Err((StatusCode::NOT_FOUND, format!("no media_item {id}")));
    };

    let mut stmt = conn
        .prepare(
            "SELECT id, season_number, episode_number, title, air_date, monitored, has_file
             FROM episode WHERE media_item_id = ?1 ORDER BY season_number, episode_number",
        )
        .map_err(internal)?;
    let episodes = stmt
        .query_map(params![id], |row| {
            Ok(EpisodeSummary {
                id: row.get(0)?,
                season_number: row.get(1)?,
                episode_number: row.get(2)?,
                title: row.get(3)?,
                air_date: row.get(4)?,
                monitored: row.get::<_, i64>(5)? != 0,
                has_file: row.get::<_, i64>(6)? != 0,
            })
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;

    Ok(Json(MediaItemDetail {
        id,
        kind,
        title,
        year,
        monitored,
        root_folder,
        episodes,
    }))
}

pub async fn add(
    State(state): State<AppState>,
    Json(req): Json<AddSeriesRequest>,
) -> Result<Json<AddSeriesResponse>, (StatusCode, String)> {
    let Some(tvdb) = &state.tvdb else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "tvdb.api_key is not configured".into(),
        ));
    };

    let root_folder = constrain_root_folder(&req.root_folder, &state.config.default_root_folder())?;

    // Fetch before taking the lock — a sync MutexGuard can't be held across
    // an `.await` point.
    let episodes = tvdb.episodes(&req.tvdb_id).await.map_err(internal)?;

    let media_item_id = {
        let conn = state.conn.lock().await;
        metadata::insert_series(
            &conn,
            &req.tvdb_id,
            &req.title,
            req.year.map(|y| y as u32),
            &req.aliases,
            &root_folder,
            1,
            &episodes,
        )
        .map_err(internal)?
    };

    Ok(Json(AddSeriesResponse { media_item_id }))
}

pub async fn add_movie(
    State(state): State<AppState>,
    Json(req): Json<AddMovieRequest>,
) -> Result<Json<AddMovieResponse>, (StatusCode, String)> {
    let root_folder = constrain_root_folder(&req.root_folder, &state.config.movies_root_folder())?;
    let conn = state.conn.lock().await;
    let media_item_id = metadata::insert_movie(
        &conn,
        &req.tmdb_id,
        &req.title,
        req.year.map(|y| y as u32),
        &root_folder,
        2,
    )
    .map_err(internal)?;
    Ok(Json(AddMovieResponse { media_item_id }))
}

pub async fn monitor(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_monitored(&state, id, true).await
}

pub async fn unmonitor(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_monitored(&state, id, false).await
}

async fn set_monitored(
    state: &AppState,
    id: i64,
    monitored: bool,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let rows = conn
        .execute(
            "UPDATE media_item SET monitored = ?1 WHERE id = ?2",
            params![monitored as i64, id],
        )
        .map_err(internal)?;
    if rows == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no media_item {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn monitor_episode(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_episode_monitored(&state, id, true).await
}

pub async fn unmonitor_episode(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_episode_monitored(&state, id, false).await
}

async fn set_episode_monitored(
    state: &AppState,
    episode_id: i64,
    monitored: bool,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let rows = conn
        .execute(
            "UPDATE episode SET monitored = ?1 WHERE id = ?2",
            params![monitored as i64, episode_id],
        )
        .map_err(internal)?;
    if rows == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no episode {episode_id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Toggles every episode in one season and the `season.monitored` flag
/// when a season row exists (`insert_series` writes those). A missing
/// season row is not an error — episodes still update.
pub async fn monitor_season(
    State(state): State<AppState>,
    Path((media_item_id, season_number)): Path<(i64, i64)>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_season_monitored(&state, media_item_id, season_number, true).await
}

pub async fn unmonitor_season(
    State(state): State<AppState>,
    Path((media_item_id, season_number)): Path<(i64, i64)>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_season_monitored(&state, media_item_id, season_number, false).await
}

async fn set_season_monitored(
    state: &AppState,
    media_item_id: i64,
    season_number: i64,
    monitored: bool,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let rows = conn
        .execute(
            "UPDATE episode SET monitored = ?1 WHERE media_item_id = ?2 AND season_number = ?3",
            params![monitored as i64, media_item_id, season_number],
        )
        .map_err(internal)?;
    // Season row is best-effort — older libraries (or movies) may have none.
    let _ = conn.execute(
        "UPDATE season SET monitored = ?1 WHERE media_item_id = ?2 AND season_number = ?3",
        params![monitored as i64, media_item_id, season_number],
    );
    if rows == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no episodes in media_item {media_item_id} season {season_number}"),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a media item (and, via cascade, its episodes/aliases/releases)
/// from the library. Deliberately does not touch anything on disk — the
/// same "stop managing this, don't delete the user's files" default
/// Sonarr/Radarr use.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let rows = conn
        .execute("DELETE FROM media_item WHERE id = ?1", params![id])
        .map_err(internal)?;
    if rows == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no media_item {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes an episode's imported file — both the row *and* the actual file
/// on disk — and clears `has_file`, freeing the episode to be grabbed again
/// by a future search. Unlike `delete` above, this deliberately does touch
/// disk: the whole point is "this specific file is wrong/broken," and
/// leaving it in place would (a) mislead the user into thinking they still
/// have a good copy, and (b) block a future re-grab outright — `import_one`
/// refuses to overwrite an existing file with an equal-or-lower-scoring one
/// (see its dest-collision check), so a bad file that happened to score
/// high would silently reject its own replacement forever if left on disk.
pub async fn delete_episode_file(
    State(state): State<AppState>,
    Path(episode_id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let files = tracked_files(
        &conn,
        "SELECT id, path FROM episode_file WHERE episode_id = ?1",
        episode_id,
    )?;
    if files.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no file tracked for episode {episode_id}"),
        ));
    }
    for (file_id, path) in files {
        delete_file_and_clear(&conn, &path, file_id)?;
    }
    conn.execute(
        "UPDATE episode SET has_file = 0 WHERE id = ?1",
        params![episode_id],
    )
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Movie counterpart to `delete_episode_file` — movies have no `has_file`
/// column of their own (`movie_needs_grab` derives "has a file" purely from
/// `episode_file` row existence), so clearing the row is the entire state
/// reset needed.
pub async fn delete_movie_file(
    State(state): State<AppState>,
    Path(media_item_id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let files = tracked_files(
        &conn,
        "SELECT id, path FROM episode_file WHERE media_item_id = ?1 AND episode_id IS NULL",
        media_item_id,
    )?;
    if files.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no file tracked for media_item {media_item_id}"),
        ));
    }
    for (file_id, path) in files {
        delete_file_and_clear(&conn, &path, file_id)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Shared cleanup: removes the file from disk (tolerating "already gone" —
/// nothing left to do, not an error) and deletes its `episode_file` row by
/// that row's own primary key — never by `episode_id`/`media_item_id`
/// alone, which a TV show shares across every one of its episode rows and
/// would otherwise risk wiping an entire show's tracked files instead of
/// the one the caller actually looked up.
fn tracked_files(
    conn: &rusqlite::Connection,
    sql: &str,
    id: i64,
) -> Result<Vec<(i64, String)>, (StatusCode, String)> {
    let mut stmt = conn.prepare(sql).map_err(internal)?;
    let files = stmt
        .query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(files)
}

fn delete_file_and_clear(
    conn: &rusqlite::Connection,
    path: &str,
    file_id: i64,
) -> Result<(), (StatusCode, String)> {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to delete {path}: {e}"),
            ));
        }
    }
    conn.execute("DELETE FROM episode_file WHERE id = ?1", params![file_id])
        .map_err(internal)?;
    Ok(())
}

/// Manual "search now" for a single show/movie's whole current backlog —
/// bypasses the background loop's per-cycle budget and due-ness cadence
/// (those exist to pace unattended, indefinite operation; a single
/// user-triggered request doesn't need throttling against itself).
///
/// Dispatched to the background loop via a channel rather than run inline
/// here: the search pipeline (`execute_search_targets` and everything it
/// calls) holds a `Connection`/`&dyn ReleaseSource` across `.await` points,
/// which makes it `!Send` — fine for the background loop (part of the root
/// future, never `Send`-constrained) but not callable directly from an axum
/// handler (whose future axum requires to be `Send`). Routing through the
/// background loop's own already-running instance also means this reuses
/// its already-loaded title-matcher/sources instead of paying to construct
/// fresh ones per request.
pub async fn search_now(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<SearchNowResult>, (StatusCode, String)> {
    let Some(tx) = &state.background_tx else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "background loop is not running (qbit.base_url is not configured)".into(),
        ));
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::api::BackgroundRequest::SearchNow {
        media_item_id: id,
        reply: reply_tx,
    })
    .await
    .map_err(internal)?;
    let stats = reply_rx.await.map_err(internal)?.map_err(internal)?;

    Ok(Json(SearchNowResult {
        targets: stats.targets,
        searched: stats.searched,
        grabbed: stats.grabbed,
        errors: stats.errors,
        source_exhausted: stats.source_exhausted,
    }))
}

/// Movie candidate picker: `episode_id` is always `None` for a movie
/// target, matching every other movie/episode split in this codebase.
pub async fn list_candidates(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<breadarr_shared::dto::ReleaseCandidate>>, (StatusCode, String)> {
    fetch_candidates_via_background(&state, id, None).await
}

pub async fn grab_candidate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<breadarr_shared::dto::GrabCandidateRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    grab_candidate_via_background(&state, id, None, req).await
}

/// Episode candidate picker — resolves the episode's own `media_item_id`
/// first (every other candidate/search-target function needs it), same
/// pattern as `set_episode_monitored`.
pub async fn list_episode_candidates(
    State(state): State<AppState>,
    Path(episode_id): Path<i64>,
) -> Result<Json<Vec<breadarr_shared::dto::ReleaseCandidate>>, (StatusCode, String)> {
    let media_item_id = episode_media_item_id(&state, episode_id).await?;
    fetch_candidates_via_background(&state, media_item_id, Some(episode_id)).await
}

pub async fn grab_episode_candidate(
    State(state): State<AppState>,
    Path(episode_id): Path<i64>,
    Json(req): Json<breadarr_shared::dto::GrabCandidateRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let media_item_id = episode_media_item_id(&state, episode_id).await?;
    grab_candidate_via_background(&state, media_item_id, Some(episode_id), req).await
}

async fn episode_media_item_id(
    state: &AppState,
    episode_id: i64,
) -> Result<i64, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    conn.query_row(
        "SELECT media_item_id FROM episode WHERE id = ?1",
        params![episode_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)?
    .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no episode {episode_id}")))
}

async fn fetch_candidates_via_background(
    state: &AppState,
    media_item_id: i64,
    episode_id: Option<i64>,
) -> Result<Json<Vec<breadarr_shared::dto::ReleaseCandidate>>, (StatusCode, String)> {
    let Some(tx) = &state.background_tx else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "background loop is not running (qbit.base_url is not configured)".into(),
        ));
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::api::BackgroundRequest::FetchCandidates {
        media_item_id,
        episode_id,
        reply: reply_tx,
    })
    .await
    .map_err(internal)?;
    let candidates = reply_rx.await.map_err(internal)?.map_err(internal)?;
    Ok(Json(candidates))
}

async fn grab_candidate_via_background(
    state: &AppState,
    media_item_id: i64,
    episode_id: Option<i64>,
    req: breadarr_shared::dto::GrabCandidateRequest,
) -> Result<StatusCode, (StatusCode, String)> {
    let Some(tx) = &state.background_tx else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "background loop is not running (qbit.base_url is not configured)".into(),
        ));
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::api::BackgroundRequest::GrabCandidate {
        media_item_id,
        episode_id,
        source_id: req.source_id,
        raw_title: req.raw_title,
        link: req.link,
        guid: req.guid,
        reply: reply_tx,
    })
    .await
    .map_err(internal)?;
    reply_rx.await.map_err(internal)?.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// After `~/` expand and lexical normalize, the path must stay under
/// `allowed_root`. `..` components and absolute paths outside that root
/// are rejected with 400.
fn constrain_root_folder(
    requested: &str,
    allowed_root: &FsPath,
) -> Result<String, (StatusCode, String)> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "root_folder must not be empty".into(),
        ));
    }
    let expanded = Config::expand_path(requested);
    if expanded
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "root_folder must not contain '..'".into(),
        ));
    }
    let candidate = if expanded.is_absolute() {
        normalize_lexically(&expanded)
    } else {
        normalize_lexically(&allowed_root.join(expanded))
    };
    let root = normalize_lexically(allowed_root);
    if !candidate.starts_with(&root) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("root_folder must be under {}", root.display()),
        ));
    }
    Ok(candidate.to_string_lossy().into_owned())
}

fn normalize_lexically(path: &FsPath) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
