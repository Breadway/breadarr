use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;

/// Fribb/anime-lists cross-references AniDB anime entries (roughly one per
/// TV season/cour) to TVDB/TMDB ids and season numbers — this is what
/// Sonarr itself relies on for anime scene-numbering, and unlike
/// manami-project/anime-offline-database it actually carries TVDB/TMDB ids
/// (verified: the offline-database has none at all).
const SOURCE_URL: &str =
    "https://raw.githubusercontent.com/Fribb/anime-lists/master/anime-list-full.json";

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(default)]
    anidb_id: Option<i64>,
    #[serde(default)]
    tvdb_id: Option<i64>,
    #[serde(default)]
    themoviedb_id: Option<serde_json::Value>,
    #[serde(default)]
    season: Option<SeasonField>,
    #[serde(default)]
    episode_offset: Option<OffsetField>,
}

#[derive(Debug, Deserialize)]
struct SeasonField {
    tvdb: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OffsetField {
    tvdb: Option<i64>,
}

/// Downloads the current Fribb/anime-lists dataset and upserts it into
/// `anime_mapping` (TV) and `anime_tmdb_movie` (movies). Returns the number
/// of `anime_mapping` entries processed (movie ids aren't counted the same
/// way — one entry can contribute several).
pub async fn refresh(conn: &mut Connection, client: &reqwest::Client) -> Result<usize> {
    let bytes = client
        .get(SOURCE_URL)
        .send()
        .await
        .context("anime-lists download failed")?
        .error_for_status()
        .context("anime-lists download returned an error status")?
        .bytes()
        .await
        .context("anime-lists download body read failed")?;

    let entries: Vec<Entry> =
        serde_json::from_slice(&bytes).context("anime-lists response was not valid JSON")?;

    let tx = conn.transaction()?;
    let mut written = 0;
    for e in &entries {
        // `themoviedb_id` carries either a `tv` id (a single number) or a
        // `movie` id list (rereleases/split cuts can give one AniDB entry
        // several TMDB movie ids) — never both in practice, but nothing
        // guarantees that, so both are checked independently rather than
        // assuming one implies the absence of the other.
        let tv_tmdb_id = e
            .themoviedb_id
            .as_ref()
            .and_then(|v| v.get("tv"))
            .and_then(|v| v.as_i64());
        for movie_id in e
            .themoviedb_id
            .as_ref()
            .and_then(|v| v.get("movie"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_i64())
        {
            tx.execute(
                "INSERT OR IGNORE INTO anime_tmdb_movie (tmdb_id) VALUES (?1)",
                params![movie_id],
            )?;
        }

        // ~63% of entries have no AniDB cross-reference at all (AniList/MAL/Kitsu-only
        // listings) — irrelevant to a table keyed on anidb_id, skip them.
        let Some(anidb_id) = e.anidb_id else {
            continue;
        };

        let season_number = e.season.as_ref().and_then(|s| s.tvdb);
        let episode_offset = e.episode_offset.as_ref().and_then(|o| o.tvdb).unwrap_or(0);

        tx.execute(
            "INSERT INTO anime_mapping (anidb_id, tvdb_id, tmdb_id, season_offset, episode_offset)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(anidb_id) DO UPDATE SET
                tvdb_id = excluded.tvdb_id,
                tmdb_id = excluded.tmdb_id,
                season_offset = excluded.season_offset,
                episode_offset = excluded.episode_offset",
            params![
                anidb_id,
                e.tvdb_id,
                tv_tmdb_id,
                season_number,
                episode_offset
            ],
        )?;
        written += 1;
    }
    tx.commit()?;

    Ok(written)
}

/// Resolves an absolute anime episode number to a (season, episode) pair
/// for a known TVDB series.
///
/// One TVDB season is often stitched together from several AniDB cours,
/// each its own `anime_mapping` row with its own `episode_offset` — e.g.
/// tvdb_id 366263 (Ascendance of a Bookworm) has cours starting at absolute
/// episodes 1, 15, 27, 37 (offsets 0, 14, 26, 36). The matching cour is the
/// one with the largest offset that's still below the target absolute
/// episode (offset = episode count preceding that cour, so
/// `local_episode = absolute - offset`).
pub fn resolve_absolute_episode(
    conn: &Connection,
    tvdb_id: i64,
    absolute_episode: u32,
) -> Result<Option<(u32, u32)>> {
    let mut stmt = conn.prepare(
        "SELECT season_offset, episode_offset FROM anime_mapping
         WHERE tvdb_id = ?1 AND season_offset IS NOT NULL",
    )?;
    let rows = stmt
        .query_map(params![tvdb_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let absolute = absolute_episode as i64;
    let best = rows
        .into_iter()
        .filter(|&(_, offset)| offset < absolute)
        .max_by_key(|&(_, offset)| offset);

    Ok(best.map(|(season, offset)| (season as u32, (absolute - offset) as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE anime_mapping (
                anidb_id INTEGER PRIMARY KEY, tvdb_id INTEGER, tmdb_id INTEGER,
                season_offset INTEGER, episode_offset INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        // Mirrors real data observed for tvdb_id 366263: one TVDB season
        // stitched from four AniDB cours starting at absolute eps 1/15/27/37.
        for (anidb_id, offset) in [(1, 0), (2, 14), (3, 26), (4, 36)] {
            conn.execute(
                "INSERT INTO anime_mapping (anidb_id, tvdb_id, season_offset, episode_offset)
                 VALUES (?1, 366263, 1, ?2)",
                params![anidb_id, offset],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn resolves_first_cour() {
        let conn = seeded_conn();
        assert_eq!(
            resolve_absolute_episode(&conn, 366263, 5).unwrap(),
            Some((1, 5))
        );
    }

    #[test]
    fn resolves_later_cour_using_its_offset() {
        let conn = seeded_conn();
        // absolute ep 20 falls in the cour starting at 15 (offset 14)
        assert_eq!(
            resolve_absolute_episode(&conn, 366263, 20).unwrap(),
            Some((1, 6))
        );
    }

    #[test]
    fn returns_none_for_unmapped_series() {
        let conn = seeded_conn();
        assert_eq!(resolve_absolute_episode(&conn, 999999, 1).unwrap(), None);
    }
}
