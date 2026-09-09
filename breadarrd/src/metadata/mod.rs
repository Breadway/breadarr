pub mod anime_map;
pub mod tmdb;
pub mod tvdb;

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesSearchResult {
    pub external_id: String,
    pub name: String,
    pub year: Option<u32>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MovieSearchResult {
    pub external_id: String,
    pub title: String,
    pub year: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeInfo {
    pub season_number: u32,
    pub episode_number: u32,
    pub absolute_number: Option<u32>,
    pub title: Option<String>,
    pub air_date: Option<String>,
}

/// Inserts a series and its full episode list into the library, using a
/// TVDB series id as the source of episode data. Returns the new
/// `media_item.id`.
///
/// Split into a fetch step and a write step (see [`insert_series`]) rather
/// than one function that both awaits and holds `conn`, because a caller
/// using `Mutex<Connection>` (e.g. an axum handler) can't hold a sync
/// `MutexGuard` across an `.await` point: this convenience wrapper is only
/// safe for callers with an owned, unshared `Connection` (e.g. the debug
/// CLI commands).
#[allow(clippy::too_many_arguments)]
pub async fn add_series(
    conn: &Connection,
    tvdb: &tvdb::TvdbClient,
    tvdb_series_id: &str,
    title: &str,
    year: Option<u32>,
    aliases: &[String],
    root_folder: &str,
    quality_profile_id: i64,
) -> Result<i64> {
    let episodes = tvdb.episodes(tvdb_series_id).await?;
    insert_series(
        conn,
        tvdb_series_id,
        title,
        year,
        aliases,
        root_folder,
        quality_profile_id,
        &episodes,
    )
}

/// TVDB/TMDB ids are positive integers. `"0"` and non-numeric strings
/// must not be stored: `0` would collide under the unique external-id
/// indexes the same way a parse-failure `unwrap_or_default()` used to.
pub(crate) fn parse_external_id(raw: &str) -> Option<i64> {
    raw.parse().ok().filter(|&id| id > 0)
}

fn existing_media_item_id(conn: &Connection, column: &str, value: i64) -> Result<Option<i64>> {
    debug_assert!(column == "tvdb_id" || column == "tmdb_id");
    let sql = format!("SELECT id FROM media_item WHERE {column} = ?1 ORDER BY id ASC LIMIT 1");
    conn.query_row(&sql, params![value], |row| row.get(0))
        .optional()
        .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
pub fn insert_series(
    conn: &Connection,
    tvdb_series_id: &str,
    title: &str,
    year: Option<u32>,
    aliases: &[String],
    root_folder: &str,
    quality_profile_id: i64,
    episodes: &[EpisodeInfo],
) -> Result<i64> {
    let tvdb_id = parse_external_id(tvdb_series_id);
    if let Some(tvdb_id) = tvdb_id {
        if let Some(existing) = existing_media_item_id(conn, "tvdb_id", tvdb_id)? {
            return Ok(existing);
        }
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO media_item (kind, title, year, tvdb_id, monitored, quality_profile_id, root_folder)
         VALUES ('series', ?1, ?2, ?3, 1, ?4, ?5)",
        params![title, year, tvdb_id, quality_profile_id, root_folder],
    )?;
    let media_item_id = tx.last_insert_rowid();

    for alias in aliases {
        tx.execute(
            "INSERT INTO alias (media_item_id, text, source) VALUES (?1, ?2, 'tvdb')",
            params![media_item_id, alias],
        )?;
    }

    let mut seasons_seen = HashSet::new();
    for ep in episodes {
        // TVDB's "season 0" is a catch-all for specials: recaps, shorts,
        // and often a tie-in movie that's frequently already tracked as
        // its own separate `media_item` (verified live: Chainsaw Man's
        // season 0 included "Chainsaw Man – The Movie: Reze Arc", which
        // already exists as its own movie entry). Monitoring these by
        // default means the library never actually "completes" and the
        // missing-episode count is inflated with content the user was
        // never trying to acquire as episodes in the first place. Regular
        // seasons keep the previous default of monitored.
        let monitored = i64::from(ep.season_number != 0);
        if seasons_seen.insert(ep.season_number) {
            tx.execute(
                "INSERT OR IGNORE INTO season (media_item_id, season_number, monitored) VALUES (?1, ?2, ?3)",
                params![media_item_id, ep.season_number, monitored],
            )?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO episode
                (media_item_id, season_number, episode_number, absolute_number, title, air_date, monitored, has_file)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                media_item_id,
                ep.season_number,
                ep.episode_number,
                ep.absolute_number,
                ep.title,
                ep.air_date,
                monitored,
            ],
        )?;
    }

    tx.commit()?;
    Ok(media_item_id)
}

/// Inserts a movie into the library. No fetch step needed (unlike
/// `add_series`/`insert_series`): TMDB's movie search result already
/// carries everything a movie needs (title, year); there's no separate
/// episode-list call the way a series has.
pub fn insert_movie(
    conn: &Connection,
    tmdb_movie_id: &str,
    title: &str,
    year: Option<u32>,
    root_folder: &str,
    quality_profile_id: i64,
) -> Result<i64> {
    let tmdb_id = parse_external_id(tmdb_movie_id);
    if let Some(tmdb_id) = tmdb_id {
        if let Some(existing) = existing_media_item_id(conn, "tmdb_id", tmdb_id)? {
            return Ok(existing);
        }
    }

    conn.execute(
        "INSERT INTO media_item (kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
         VALUES ('movie', ?1, ?2, ?3, 1, ?4, ?5)",
        params![title, year, tmdb_id, quality_profile_id, root_folder],
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::init(&conn).unwrap();
        conn
    }

    fn ep(season: u32, episode: u32) -> EpisodeInfo {
        EpisodeInfo {
            season_number: season,
            episode_number: episode,
            absolute_number: None,
            title: Some(format!("E{episode}")),
            air_date: Some("2020-01-01".into()),
        }
    }

    #[test]
    fn parse_external_id_rejects_zero_and_garbage() {
        assert_eq!(parse_external_id("550"), Some(550));
        assert_eq!(parse_external_id("0"), None);
        assert_eq!(parse_external_id("-12"), None);
        assert_eq!(parse_external_id("not-a-number"), None);
        assert_eq!(parse_external_id(""), None);
    }

    #[test]
    fn insert_series_writes_seasons_and_episodes_together() {
        let conn = conn();
        let id = insert_series(
            &conn,
            "12345",
            "Show",
            Some(2020),
            &["Alias".into()],
            "/tv/Show",
            1,
            &[ep(1, 1), ep(1, 2), ep(2, 1)],
        )
        .unwrap();

        let seasons: i64 = conn
            .query_row(
                "SELECT count(*) FROM season WHERE media_item_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let episodes: i64 = conn
            .query_row(
                "SELECT count(*) FROM episode WHERE media_item_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let aliases: i64 = conn
            .query_row(
                "SELECT count(*) FROM alias WHERE media_item_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seasons, 2);
        assert_eq!(episodes, 3);
        assert_eq!(aliases, 1);
    }

    #[test]
    fn insert_series_rolls_back_when_an_episode_insert_fails() {
        let conn = conn();
        conn.execute(
            "CREATE TRIGGER fail_episode BEFORE INSERT ON episode
             BEGIN SELECT RAISE(ABORT, 'boom'); END",
            [],
        )
        .unwrap();

        let err = insert_series(
            &conn,
            "99",
            "Show",
            None,
            &["A".into()],
            "/tv/Show",
            1,
            &[ep(1, 1)],
        );
        assert!(err.is_err());

        let items: i64 = conn
            .query_row("SELECT count(*) FROM media_item", [], |r| r.get(0))
            .unwrap();
        let seasons: i64 = conn
            .query_row("SELECT count(*) FROM season", [], |r| r.get(0))
            .unwrap();
        let aliases: i64 = conn
            .query_row("SELECT count(*) FROM alias", [], |r| r.get(0))
            .unwrap();
        assert_eq!(items, 0);
        assert_eq!(seasons, 0);
        assert_eq!(aliases, 0);
    }

    #[test]
    fn insert_series_is_idempotent_on_tvdb_id() {
        let conn = conn();
        let first = insert_series(
            &conn,
            "12345",
            "Show",
            None,
            &[],
            "/tv/Show",
            1,
            &[ep(1, 1)],
        )
        .unwrap();
        let second = insert_series(
            &conn,
            "12345",
            "Other Title",
            None,
            &[],
            "/tv/Other",
            1,
            &[],
        )
        .unwrap();
        assert_eq!(first, second);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM media_item WHERE tvdb_id = 12345",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_movie_is_idempotent_on_tmdb_id() {
        let conn = conn();
        let first = insert_movie(&conn, "550", "Fight Club", Some(1999), "/movies", 2).unwrap();
        let second = insert_movie(&conn, "550", "Fight Club 2", Some(2000), "/movies", 2).unwrap();
        assert_eq!(first, second);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM media_item WHERE tmdb_id = 550",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
