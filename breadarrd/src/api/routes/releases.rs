use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::ReleaseSummary;

use crate::api::AppState;

pub async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<ReleaseSummary>>, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT r.id, m.title, r.raw_title, r.score, r.status, r.grabbed_at
             FROM release r JOIN media_item m ON m.id = r.media_item_id
             ORDER BY r.grabbed_at DESC LIMIT 200",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ReleaseSummary {
                id: row.get(0)?,
                media_title: row.get(1)?,
                raw_title: row.get(2)?,
                score: row.get(3)?,
                status: row.get(4)?,
                grabbed_at: row.get(5)?,
            })
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(Json(rows))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
