use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::{MaxedSearchTarget, StalledGrab, StuckReport};

use crate::api::AppState;

/// Deliberately earlier than importer::mod's own `STALL_THRESHOLD_HOURS`
/// (72h) — this report exists to surface a stuck grab *before* the
/// daemon's own auto-fail kicks in, not just repeat it after the fact.
const STALLED_GRAB_HOURS: f64 = 48.0;

/// Matches the point where the search-driven loop's exponential backoff
/// (`6h * 2^(count-1)`, capped at 168h) saturates — see `scheduler::
/// DUE_CLAUSE`. A target still at this count with no successful grab has
/// been failing its search every single time for at least a week.
const MAXED_SEARCH_COUNT: i64 = 6;

pub async fn stuck(
    State(state): State<AppState>,
) -> Result<Json<StuckReport>, (StatusCode, String)> {
    let conn = state.conn.lock().await;

    let mut stmt = conn
        .prepare(
            "SELECT r.id, r.media_item_id, m.title, r.raw_title, r.grabbed_at
             FROM release r JOIN media_item m ON m.id = r.media_item_id
             WHERE r.status = 'grabbed'
               AND (julianday('now') - julianday(r.grabbed_at)) * 24.0 > ?1
             ORDER BY r.grabbed_at ASC",
        )
        .map_err(internal)?;
    let stalled_grabs = stmt
        .query_map([STALLED_GRAB_HOURS], |row| {
            Ok(StalledGrab {
                release_id: row.get(0)?,
                media_item_id: row.get(1)?,
                media_title: row.get(2)?,
                raw_title: row.get(3)?,
                grabbed_at: row.get(4)?,
            })
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;

    let review_queue_depth: i64 = conn
        .query_row(
            "SELECT count(*) FROM review_queue WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
        .map_err(internal)?;

    let mut stmt = conn
        .prepare(
            "SELECT ss.media_item_id, m.title, ss.search_count, ss.last_searched_at
             FROM search_state ss JOIN media_item m ON m.id = ss.media_item_id
             WHERE ss.search_count >= ?1 AND ss.last_result != 'grabbed'
             ORDER BY ss.search_count DESC",
        )
        .map_err(internal)?;
    let maxed_out_search_targets = stmt
        .query_map([MAXED_SEARCH_COUNT], |row| {
            Ok(MaxedSearchTarget {
                media_item_id: row.get(0)?,
                media_title: row.get(1)?,
                search_count: row.get(2)?,
                last_searched_at: row.get(3)?,
            })
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;

    Ok(Json(StuckReport {
        stalled_grabs,
        review_queue_depth,
        maxed_out_search_targets,
    }))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
