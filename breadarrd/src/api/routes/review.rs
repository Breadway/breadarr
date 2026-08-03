use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::ReviewQueueEntry;

use crate::api::AppState;
use crate::scheduler;

pub async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<ReviewQueueEntry>>, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT rq.id, rq.raw_release_title, m.title, rq.confidence, rq.status, rq.created_at
             FROM review_queue rq LEFT JOIN media_item m ON m.id = rq.candidate_media_item_id
             WHERE rq.status = 'pending'
             ORDER BY rq.created_at DESC",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ReviewQueueEntry {
                id: row.get(0)?,
                raw_release_title: row.get(1)?,
                candidate_media_title: row.get(2)?,
                confidence: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    Ok(Json(rows))
}

pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let Some(qbit) = &state.qbit else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "qbit.base_url is not configured".into(),
        ));
    };

    let prep = {
        let conn = state.conn.lock().await;
        scheduler::prepare_review_approval(&conn, id).map_err(internal)?
    };
    let prepared = match prep {
        scheduler::ApprovalPrep::Ready(p) => p,
        scheduler::ApprovalPrep::NotPending => {
            return Err((StatusCode::CONFLICT, "review item is not pending".into()))
        }
        scheduler::ApprovalPrep::MissingGrabData => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "review item has no stored link/source to grab".into(),
            ))
        }
        scheduler::ApprovalPrep::CouldNotResolveEpisode => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "could not resolve which episode this release is".into(),
            ))
        }
        scheduler::ApprovalPrep::NotMonitoredOrAlreadyHave => {
            return Err((
                StatusCode::CONFLICT,
                "episode is not monitored or is already downloaded".into(),
            ))
        }
    };

    let torrent_hash = match scheduler::grab_prepared_approval(qbit, &state.qbit_category, &prepared).await {
        Ok(hash) => hash,
        Err(e) => {
            // The grab errored outright (not just "added but no hash
            // captured" — `finalize_review_approval` below handles that
            // case and still runs to completion). `prepare_review_approval`
            // already claimed this row into `approved` before we got here;
            // without releasing it back to `pending`, a transient
            // qBittorrent error would strand the review permanently
            // unapprovable with nothing ever recorded for it.
            let conn = state.conn.lock().await;
            let _ = scheduler::release_review_claim(&conn, id);
            return Err(internal(e));
        }
    };

    {
        let conn = state.conn.lock().await;
        scheduler::finalize_review_approval(
            &conn,
            &prepared,
            &state.qbit_category,
            torrent_hash.as_deref(),
        )
        .map_err(internal)?;
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn reject(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    scheduler::reject_review(&conn, id).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
