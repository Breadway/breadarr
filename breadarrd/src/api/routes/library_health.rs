use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::{
    CodecCount, DuplicateGroup, FlaggedFile, LibraryHealthReport, LibrarySummary,
};
use rusqlite::Connection;

use crate::api::AppState;

/// Every flag category shares the same shape (id/title/episode-label/path),
/// resolved through the same TV-or-movie join every time — `episode_id` is
/// set for a TV file (join via `episode`→`media_item`) and NULL for a movie
/// file (join `media_item` directly via `episode_file.media_item_id`), the
/// same nullable-episode_id convention used throughout this schema.
const FLAGGED_FILE_SELECT: &str = "
    SELECT ef.id,
           COALESCE(mi_direct.title, mi_ep.title) AS media_title,
           CASE WHEN ef.episode_id IS NOT NULL
                THEN 'S' || printf('%02d', e.season_number) || 'E' || printf('%02d', e.episode_number)
                ELSE NULL END AS episode_label,
           ef.path
    FROM episode_file ef
    JOIN media_file_probe p ON p.episode_file_id = ef.id
    LEFT JOIN media_item mi_direct ON mi_direct.id = ef.media_item_id
    LEFT JOIN episode e ON e.id = ef.episode_id
    LEFT JOIN media_item mi_ep ON mi_ep.id = e.media_item_id
    WHERE ";

fn fetch_flagged(conn: &Connection, where_clause: &str) -> rusqlite::Result<Vec<FlaggedFile>> {
    let sql = format!("{FLAGGED_FILE_SELECT}{where_clause}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(FlaggedFile {
            episode_file_id: row.get(0)?,
            media_title: row.get(1)?,
            episode_label: row.get(2)?,
            path: row.get(3)?,
        })
    })?;
    rows.collect()
}

fn fetch_duplicate_groups(conn: &Connection) -> rusqlite::Result<Vec<DuplicateGroup>> {
    let mut groups = Vec::new();

    let mut stmt = conn.prepare(
        "SELECT m.title,
                'S' || printf('%02d', e.season_number) || 'E' || printf('%02d', e.episode_number),
                group_concat(ef.path, '||')
         FROM episode_file ef
         JOIN episode e ON e.id = ef.episode_id
         JOIN media_item m ON m.id = e.media_item_id
         GROUP BY ef.episode_id
         HAVING count(*) > 1",
    )?;
    groups.extend(
        stmt.query_map([], |row| {
            let paths: String = row.get(2)?;
            Ok(DuplicateGroup {
                media_title: row.get(0)?,
                episode_label: row.get(1)?,
                paths: paths.split("||").map(str::to_string).collect(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?,
    );

    let mut stmt = conn.prepare(
        "SELECT m.title, group_concat(ef.path, '||')
         FROM episode_file ef
         JOIN media_item m ON m.id = ef.media_item_id
         WHERE ef.episode_id IS NULL
         GROUP BY ef.media_item_id
         HAVING count(*) > 1",
    )?;
    groups.extend(
        stmt.query_map([], |row| {
            let paths: String = row.get(1)?;
            Ok(DuplicateGroup {
                media_title: row.get(0)?,
                episode_label: None,
                paths: paths.split("||").map(str::to_string).collect(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?,
    );

    Ok(groups)
}

fn fetch_summary(conn: &Connection) -> rusqlite::Result<LibrarySummary> {
    let (total_files, total_size_bytes): (i64, i64) = conn.query_row(
        "SELECT count(*), COALESCE(sum(size_bytes), 0) FROM episode_file",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let probed_files: i64 = conn.query_row("SELECT count(*) FROM media_file_probe", [], |row| {
        row.get(0)
    })?;

    let mut stmt = conn.prepare(
        "SELECT video_codec, count(*) FROM media_file_probe
         WHERE video_codec IS NOT NULL GROUP BY video_codec ORDER BY count(*) DESC",
    )?;
    let by_video_codec = stmt
        .query_map([], |row| {
            Ok(CodecCount {
                codec: row.get(0)?,
                count: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Resolution buckets are computed against `height` directly rather than
    // reusing `flag_under_quality` (which is only a <1080p boolean) — this
    // view wants the full breakdown, not just the pass/fail split.
    let bucket = |where_clause: &str| -> rusqlite::Result<i64> {
        conn.query_row(
            &format!(
                "SELECT count(*) FROM media_file_probe WHERE height IS NOT NULL AND {where_clause}"
            ),
            [],
            |row| row.get(0),
        )
    };
    let sd_count = bucket("height < 720")?;
    let hd_720p_count = bucket("height >= 720 AND height < 1080")?;
    let full_hd_1080p_count = bucket("height >= 1080 AND height < 2160")?;
    let uhd_4k_count = bucket("height >= 2160")?;

    let with_subs: i64 = conn.query_row(
        "SELECT count(*) FROM media_file_probe WHERE flag_no_subtitles = 0",
        [],
        |row| row.get(0),
    )?;
    let pct_with_subtitles = if probed_files > 0 {
        (with_subs as f64 / probed_files as f64) * 100.0
    } else {
        0.0
    };

    Ok(LibrarySummary {
        total_files,
        total_size_bytes,
        probed_files,
        by_video_codec,
        sd_count,
        hd_720p_count,
        full_hd_1080p_count,
        uhd_4k_count,
        pct_with_subtitles,
    })
}

pub async fn library_health(
    State(state): State<AppState>,
) -> Result<Json<LibraryHealthReport>, (StatusCode, String)> {
    let conn = state.conn.lock().await;

    let report = LibraryHealthReport {
        corrupt_files: fetch_flagged(
            &conn,
            "p.corruption_status IN ('probe_failed','decode_failed')",
        )
        .map_err(internal)?,
        under_quality_files: fetch_flagged(&conn, "p.flag_under_quality = 1").map_err(internal)?,
        no_subtitle_files: fetch_flagged(&conn, "p.flag_no_subtitles = 1").map_err(internal)?,
        no_english_audio_files: fetch_flagged(&conn, "p.flag_no_english_audio = 1")
            .map_err(internal)?,
        non_english_default_audio_files: fetch_flagged(
            &conn,
            "p.flag_non_english_default_audio = 1",
        )
        .map_err(internal)?,
        duplicate_groups: fetch_duplicate_groups(&conn).map_err(internal)?,
        summary: fetch_summary(&conn).map_err(internal)?,
    };

    Ok(Json(report))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TV episode file (id=1, under-quality) and a movie file (id=2,
    /// no English audio) with `media_file_probe` rows already populated —
    /// exercises both halves of the nullable-episode_id join every query in
    /// this file relies on.
    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Some Show', 2020, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 2, 5, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, 1, NULL, '/tmp/show.mkv', 1000, 'none')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime, height, video_codec,
                corruption_status, flag_under_quality, flag_no_subtitles, flag_no_english_audio, flag_non_english_default_audio)
             VALUES (1, datetime('now'), 1000, 0, 480, 'h264', 'probe_ok', 1, 0, 0, 0)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (2, 'movie', 'Some Movie', 2019, 1, 2, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (2, NULL, 2, '/tmp/movie.mkv', 2000, 'none')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime, height, video_codec,
                corruption_status, flag_under_quality, flag_no_subtitles, flag_no_english_audio, flag_non_english_default_audio)
             VALUES (2, datetime('now'), 2000, 0, 1080, 'hevc', 'probe_ok', 0, 1, 1, 0)",
            [],
        )
        .unwrap();

        conn
    }

    #[test]
    fn fetch_flagged_resolves_titles_for_both_tv_and_movie_files() {
        let conn = seeded_conn();

        let under_quality = fetch_flagged(&conn, "p.flag_under_quality = 1").unwrap();
        assert_eq!(under_quality.len(), 1);
        assert_eq!(under_quality[0].media_title, "Some Show");
        assert_eq!(under_quality[0].episode_label.as_deref(), Some("S02E05"));

        let no_english = fetch_flagged(&conn, "p.flag_no_english_audio = 1").unwrap();
        assert_eq!(no_english.len(), 1);
        assert_eq!(no_english[0].media_title, "Some Movie");
        assert_eq!(no_english[0].episode_label, None);
    }

    #[test]
    fn fetch_duplicate_groups_finds_both_tv_and_movie_duplicates() {
        let conn = seeded_conn();
        // A second file for the same TV episode, and a second for the same movie.
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (3, 1, NULL, '/tmp/show-dup.mkv', 1000, 'none')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (4, NULL, 2, '/tmp/movie-dup.mkv', 2000, 'none')",
            [],
        )
        .unwrap();

        let groups = fetch_duplicate_groups(&conn).unwrap();
        assert_eq!(groups.len(), 2);
        let tv_group = groups
            .iter()
            .find(|g| g.media_title == "Some Show")
            .unwrap();
        assert_eq!(tv_group.paths.len(), 2);
        let movie_group = groups
            .iter()
            .find(|g| g.media_title == "Some Movie")
            .unwrap();
        assert_eq!(movie_group.paths.len(), 2);
    }

    #[test]
    fn fetch_duplicate_groups_ignores_files_without_duplicates() {
        let conn = seeded_conn();
        assert!(fetch_duplicate_groups(&conn).unwrap().is_empty());
    }

    #[test]
    fn fetch_summary_buckets_resolutions_and_computes_subtitle_percentage() {
        let conn = seeded_conn();
        let summary = fetch_summary(&conn).unwrap();

        assert_eq!(summary.total_files, 2);
        assert_eq!(summary.total_size_bytes, 3000);
        assert_eq!(summary.probed_files, 2);
        assert_eq!(summary.sd_count, 1); // the 480p show episode
        assert_eq!(summary.full_hd_1080p_count, 1); // the 1080p movie
                                                    // One of the two probed files (the movie) has flag_no_subtitles=1.
        assert_eq!(summary.pct_with_subtitles, 50.0);
    }

    #[test]
    fn fetch_summary_handles_an_empty_library_without_dividing_by_zero() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let summary = fetch_summary(&conn).unwrap();
        assert_eq!(summary.total_files, 0);
        assert_eq!(summary.pct_with_subtitles, 0.0);
    }
}
