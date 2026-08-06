pub mod anime_map;
pub mod tmdb;
pub mod tvdb;

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{params, Connection};

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
/// `MutexGuard` across an `.await` point — this convenience wrapper is only
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
    conn.execute(
        "INSERT INTO media_item (kind, title, year, tvdb_id, monitored, quality_profile_id, root_folder)
         VALUES ('series', ?1, ?2, ?3, 1, ?4, ?5)",
        params![
            title,
            year,
            tvdb_series_id.parse::<i64>().ok(),
            quality_profile_id,
            root_folder
        ],
    )?;
    let media_item_id = conn.last_insert_rowid();

    for alias in aliases {
        conn.execute(
            "INSERT INTO alias (media_item_id, text, source) VALUES (?1, ?2, 'tvdb')",
            params![media_item_id, alias],
        )?;
    }

    let mut seasons_seen = HashSet::new();
    for ep in episodes {
        // TVDB's "season 0" is a catch-all for specials — recaps, shorts,
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
            conn.execute(
                "INSERT OR IGNORE INTO season (media_item_id, season_number, monitored) VALUES (?1, ?2, ?3)",
                params![media_item_id, ep.season_number, monitored],
            )?;
        }
        conn.execute(
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

    Ok(media_item_id)
}

/// Inserts a movie into the library. No fetch step needed (unlike
/// `add_series`/`insert_series`) — TMDB's movie search result already
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
    conn.execute(
        "INSERT INTO media_item (kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
         VALUES ('movie', ?1, ?2, ?3, 1, ?4, ?5)",
        params![
            title,
            year,
            tmdb_movie_id.parse::<i64>().ok(),
            quality_profile_id,
            root_folder
        ],
    )?;
    Ok(conn.last_insert_rowid())
}
