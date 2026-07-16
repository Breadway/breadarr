use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::CalendarEntry;

use crate::api::AppState;

/// How far back/forward "upcoming" spans — matches the common Sonarr
/// calendar default window (a week either side) rather than an arbitrary
/// wider range, since the point is "what's relevant right now."
const DAYS_PAST: i64 = 7;
const DAYS_FUTURE: i64 = 21;

pub async fn calendar(
    State(state): State<AppState>,
) -> Result<Json<Vec<CalendarEntry>>, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT e.media_item_id, m.title, e.season_number, e.episode_number, e.title,
                    e.air_date, e.monitored, e.has_file
             FROM episode e JOIN media_item m ON m.id = e.media_item_id
             WHERE e.air_date IS NOT NULL
               AND date(e.air_date) BETWEEN date('now', ?1) AND date('now', ?2)
             ORDER BY e.air_date ASC",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(
            [format!("-{DAYS_PAST} days"), format!("+{DAYS_FUTURE} days")],
            |row| {
                Ok(CalendarEntry {
                    media_item_id: row.get(0)?,
                    media_title: row.get(1)?,
                    season_number: row.get(2)?,
                    episode_number: row.get(3)?,
                    title: row.get(4)?,
                    air_date: row.get(5)?,
                    monitored: row.get::<_, i64>(6)? != 0,
                    has_file: row.get::<_, i64>(7)? != 0,
                })
            },
        )
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(Json(rows))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
