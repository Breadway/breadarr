use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::{MaxedSearchTarget, StalledGrab, StuckReport};

use crate::api::AppState;

/// Deliberately earlier than importer::mod's own `STALL_THRESHOLD_HOURS`
/// (72h); this report exists to surface a stuck grab *before* the
/// daemon's own auto-fail kicks in, not just repeat it after the fact.
const STALLED_GRAB_HOURS: f64 = 48.0;

/// Matches the point where the search-driven loop's exponential backoff
/// (`6h * 2^(count-1)`, capped at 168h) saturates; see `scheduler::
/// DUE_CLAUSE`. A target still at this count with no successful grab has
/// been failing its search every single time for at least a week.
const MAXED_SEARCH_COUNT: i64 = 6;

const MAXED_SEARCH_QUERY: &str = "
    SELECT ss.media_item_id, m.title, ss.search_count, ss.last_searched_at
    FROM search_state ss JOIN media_item m ON m.id = ss.media_item_id
    LEFT JOIN episode e ON e.id = ss.episode_id AND e.media_item_id = m.id
    WHERE ss.search_count >= ?1 AND ss.last_result != 'grabbed'
      AND m.monitored = 1
      AND ((m.kind = 'series' AND e.monitored = 1 AND e.has_file = 0
            AND (e.air_date IS NULL OR e.air_date <= date('now')))
           OR (m.kind = 'movie' AND ss.episode_id IS NULL))
      AND NOT EXISTS (
          SELECT 1 FROM episode_file f
          WHERE (ss.episode_id IS NOT NULL AND f.episode_id = ss.episode_id)
             OR (ss.episode_id IS NULL AND f.media_item_id = ss.media_item_id))
      AND NOT EXISTS (
          SELECT 1 FROM release r
          WHERE r.media_item_id = m.id AND r.status IN ('grabbed', 'downloading')
            AND ((ss.episode_id IS NULL AND r.episode_id IS NULL)
                 OR r.episode_id = ss.episode_id
                 OR (r.episode_id IS NULL AND r.season_number = e.season_number)))
    ORDER BY ss.search_count DESC";

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

    let mut stmt = conn.prepare(MAXED_SEARCH_QUERY).map_err(internal)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maxed_search_report_only_includes_missing_monitored_aired_targets() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO media_item (id, kind, title, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'show', 1, 1, '/show'),
                    (2, 'movie', 'missing movie', 1, 1, '/missing'),
                    (3, 'movie', 'acquired movie', 1, 1, '/acquired'),
                    (4, 'movie', 'unmonitored movie', 0, 1, '/disabled');
             INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file, air_date)
             VALUES (1, 1, 1, 1, 1, 0, '2020-01-01'),
                    (2, 1, 1, 2, 1, 1, '2020-01-01'),
                    (3, 1, 1, 3, 0, 0, '2020-01-01'),
                    (4, 1, 1, 4, 1, 0, '2999-01-01'),
                    (5, 1, 1, 5, 1, 0, '2020-01-01');
             INSERT INTO episode_file (episode_id, media_item_id, path, size_bytes)
             VALUES (5, NULL, '/show/five.mkv', 1), (NULL, 3, '/acquired/movie.mkv', 1);
             INSERT INTO search_state (media_item_id, episode_id, search_count, last_result, last_searched_at)
             VALUES (1, 1, 6, 'no_viable', '2020-01-01'),
                    (1, 2, 6, 'no_viable', '2020-01-01'),
                    (1, 3, 6, 'no_viable', '2020-01-01'),
                    (1, 4, 6, 'no_viable', '2020-01-01'),
                    (1, 5, 6, 'no_viable', '2020-01-01'),
                    (2, NULL, 6, 'no_viable', '2020-01-01'),
                    (3, NULL, 6, 'no_viable', '2020-01-01'),
                    (4, NULL, 6, 'no_viable', '2020-01-01');"
        ).unwrap();
        let mut stmt = conn.prepare(MAXED_SEARCH_QUERY).unwrap();
        let ids: Vec<i64> = stmt
            .query_map([MAXED_SEARCH_COUNT], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![1, 2]);
        conn.execute("UPDATE media_item SET monitored = 0 WHERE id = 1", [])
            .unwrap();
        let ids: Vec<i64> = stmt
            .query_map([MAXED_SEARCH_COUNT], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![2]);
        conn.execute_batch(
            "UPDATE media_item SET monitored = 1 WHERE id = 1;
             INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs)
             VALUES (1, 'test', 'rss', 'http://localhost', 60);
             INSERT INTO release (media_item_id, raw_title, source_id, guid, status, grabbed_at, season_number)
             VALUES (1, 'show S01', 1, 'pack', 'grabbed', datetime('now'), 1),
                    (2, 'movie', 1, 'movie', 'downloading', datetime('now'), NULL);"
        ).unwrap();
        let ids: Vec<i64> = stmt
            .query_map([MAXED_SEARCH_COUNT], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            ids.is_empty(),
            "active season packs and movies are already being resolved"
        );
    }
}
