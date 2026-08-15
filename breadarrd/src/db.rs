use rusqlite::Connection;

/// How many recent backups `backup_before_open` keeps around — bounded so
/// a long-running daemon (restarted routinely by systemd on crash/update)
/// doesn't slowly fill the disk with an ever-growing pile of copies.
const MAX_BACKUPS: usize = 5;

/// Writes a consistent snapshot of the existing database to a timestamped
/// file under `<db-dir>/backups/`, then prunes old backups beyond
/// `MAX_BACKUPS`. A no-op if there's no existing database yet (fresh
/// install — nothing to back up). Uses `VACUUM INTO` so WAL state is
/// folded into one standalone file; a raw `fs::copy` of a live WAL
/// database can be torn. Sonarr/Radarr back themselves up before every
/// upgrade; breadarr has no migration framework to trigger that same
/// moment, so this runs on every startup instead.
pub fn backup_before_open(db_path: &std::path::Path) -> anyhow::Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let backup_dir = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("backups");
    std::fs::create_dir_all(&backup_dir)?;

    let stem = db_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("breadarr.db");
    // Millisecond precision (not just seconds) so two backups triggered
    // within the same second — restart-crash-loop territory — still sort
    // and dedupe correctly instead of the second one silently overwriting.
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ");
    let dest = backup_dir.join(format!("{timestamp}-{stem}"));

    let src = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Path is interpolated (VACUUM INTO does not bind `?` parameters);
    // single quotes in the path are doubled so the SQL string stays valid.
    let dest_sql = dest.to_string_lossy().replace('\'', "''");
    src.execute(&format!("VACUUM INTO '{dest_sql}'"), [])?;
    drop(src);

    prune_old_backups(&backup_dir, stem)?;
    Ok(())
}

fn prune_old_backups(backup_dir: &std::path::Path, stem: &str) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(backup_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(stem) && !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .collect();
    // Filenames are `<timestamp>-<stem>` with a sortable timestamp format,
    // so lexical order is chronological order.
    entries.sort_by_key(|e| e.file_name());
    if entries.len() > MAX_BACKUPS {
        for e in &entries[..entries.len() - MAX_BACKUPS] {
            let base = e.file_name().to_string_lossy().to_string();
            let _ = std::fs::remove_file(e.path());
            for sidecar_ext in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(backup_dir.join(format!("{base}{sidecar_ext}")));
            }
        }
    }
    Ok(())
}

/// Ad-hoc migration for a column introduced after the initial schema. No
/// migration framework exists (single production deployment, `CREATE TABLE
/// IF NOT EXISTS` is the only mechanism otherwise) — idempotent by simply
/// ignoring "duplicate column name", so it's safe to call on every startup
/// against both a fresh database and one that already has the column.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    ddl: &str,
) -> anyhow::Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}");
    match conn.execute(&sql, []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
            if msg.contains("duplicate column name") =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

pub fn init(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;

         CREATE TABLE IF NOT EXISTS quality_profile (
             id      INTEGER PRIMARY KEY,
             name    TEXT    NOT NULL UNIQUE,
             kind    TEXT    NOT NULL CHECK (kind IN ('movie','tv')),
             cutoff  REAL,
             weights TEXT    NOT NULL
         );

         CREATE TABLE IF NOT EXISTS media_item (
             id                  INTEGER PRIMARY KEY,
             kind                TEXT    NOT NULL CHECK (kind IN ('movie','series')),
             title               TEXT    NOT NULL,
             year                INTEGER,
             tvdb_id             INTEGER,
             tmdb_id             INTEGER,
             anidb_id            INTEGER,
             monitored           INTEGER NOT NULL DEFAULT 1,
             quality_profile_id  INTEGER NOT NULL REFERENCES quality_profile(id),
             root_folder         TEXT    NOT NULL
         );

         CREATE TABLE IF NOT EXISTS alias (
             id             INTEGER PRIMARY KEY,
             media_item_id  INTEGER NOT NULL REFERENCES media_item(id) ON DELETE CASCADE,
             text           TEXT    NOT NULL,
             source         TEXT    NOT NULL
         );

         CREATE TABLE IF NOT EXISTS season (
             id             INTEGER PRIMARY KEY,
             media_item_id  INTEGER NOT NULL REFERENCES media_item(id) ON DELETE CASCADE,
             season_number  INTEGER NOT NULL,
             monitored      INTEGER NOT NULL DEFAULT 1,
             UNIQUE(media_item_id, season_number)
         );

         CREATE TABLE IF NOT EXISTS episode (
             id               INTEGER PRIMARY KEY,
             media_item_id    INTEGER NOT NULL REFERENCES media_item(id) ON DELETE CASCADE,
             season_number    INTEGER NOT NULL,
             episode_number   INTEGER NOT NULL,
             absolute_number  INTEGER,
             title            TEXT,
             air_date         TEXT,
             monitored        INTEGER NOT NULL DEFAULT 1,
             has_file         INTEGER NOT NULL DEFAULT 0,
             UNIQUE(media_item_id, season_number, episode_number)
         );

         CREATE TABLE IF NOT EXISTS episode_file (
             id               INTEGER PRIMARY KEY,
             episode_id       INTEGER REFERENCES episode(id) ON DELETE CASCADE,
             media_item_id    INTEGER REFERENCES media_item(id) ON DELETE CASCADE,
             path             TEXT    NOT NULL,
             size_bytes       INTEGER NOT NULL,
             video_codec      TEXT,
             resolution       TEXT,
             audio_langs      TEXT,
             quality_score    REAL,
             subtitle_status  TEXT    NOT NULL DEFAULT 'none'
                              CHECK (subtitle_status IN ('none','not_needed','queued','done'))
         );

         CREATE TABLE IF NOT EXISTS source (
             id                  INTEGER PRIMARY KEY,
             name                TEXT    NOT NULL UNIQUE,
             kind                TEXT    NOT NULL CHECK (kind IN ('rss','scrape')),
             base_url            TEXT    NOT NULL,
             poll_interval_secs  INTEGER NOT NULL DEFAULT 300,
             enabled             INTEGER NOT NULL DEFAULT 1
         );

         CREATE TABLE IF NOT EXISTS release (
             id             INTEGER PRIMARY KEY,
             media_item_id  INTEGER NOT NULL REFERENCES media_item(id) ON DELETE CASCADE,
             episode_id     INTEGER REFERENCES episode(id) ON DELETE CASCADE,
             raw_title      TEXT    NOT NULL,
             source_id      INTEGER NOT NULL REFERENCES source(id),
             guid           TEXT    NOT NULL,
             score          REAL,
             torrent_hash   TEXT,
             status         TEXT    NOT NULL
                            CHECK (status IN ('grabbed','downloading','imported','upgraded','failed')),
             grabbed_at     TEXT    NOT NULL
         );

         CREATE TABLE IF NOT EXISTS anime_mapping (
             anidb_id       INTEGER PRIMARY KEY,
             tvdb_id        INTEGER,
             tmdb_id        INTEGER,
             season_offset  INTEGER,
             episode_offset INTEGER NOT NULL DEFAULT 0
         );

         -- Anime *movies'* TMDB ids, separate from `anime_mapping` above:
         -- one AniDB entry can map to several TMDB movie ids (rereleases,
         -- split cuts), which doesn't fit `anime_mapping`'s one-row-per-
         -- anidb_id shape (that shape exists for TV's season/episode-offset
         -- upsert semantics, meaningless for movies) — and nothing about a
         -- movie needs the anidb_id linkage anyway, only is-this-tmdb_id-
         -- an-anime-movie membership.
         CREATE TABLE IF NOT EXISTS anime_tmdb_movie (
             tmdb_id INTEGER PRIMARY KEY
         );

         CREATE TABLE IF NOT EXISTS review_queue (
             id                       INTEGER PRIMARY KEY,
             raw_release_title        TEXT    NOT NULL,
             candidate_media_item_id  INTEGER REFERENCES media_item(id) ON DELETE CASCADE,
             confidence               REAL    NOT NULL,
             link                     TEXT,
             source_id                INTEGER REFERENCES source(id),
             status                   TEXT    NOT NULL DEFAULT 'pending'
                                      CHECK (status IN ('pending','approved','rejected')),
             created_at               TEXT    NOT NULL
         );

         CREATE TABLE IF NOT EXISTS seen_guid (
             source_id  INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
             guid       TEXT    NOT NULL,
             seen_at    TEXT    NOT NULL,
             PRIMARY KEY (source_id, guid)
         );

         -- One row per monitored episode/movie the search-driven loop has
         -- attempted at least once. `episode_id` NULL means the row is for
         -- a movie (same nullable-episode_id convention as episode_file) —
         -- a plain UNIQUE(media_item_id, episode_id) can't express that,
         -- since SQLite treats NULLs as distinct, so two partial unique
         -- indexes stand in for it instead.
         CREATE TABLE IF NOT EXISTS search_state (
             id                INTEGER PRIMARY KEY,
             media_item_id     INTEGER NOT NULL REFERENCES media_item(id) ON DELETE CASCADE,
             episode_id        INTEGER REFERENCES episode(id) ON DELETE CASCADE,
             last_searched_at  TEXT    NOT NULL,
             search_count      INTEGER NOT NULL DEFAULT 1,
             last_result       TEXT    NOT NULL
                                CHECK (last_result IN ('grabbed','no_results','no_viable','error'))
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_search_state_episode
             ON search_state(episode_id) WHERE episode_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_search_state_movie
             ON search_state(media_item_id) WHERE episode_id IS NULL;

         -- Same shape as `search_state`, but for the separate upgrade-search
         -- cadence: already-imported episodes/movies periodically re-checked
         -- for a better release. Kept as its own table rather than reusing
         -- `search_state` so the two cadences (missing-content vs. upgrade)
         -- never interfere with each other's due-ness clock.
         CREATE TABLE IF NOT EXISTS upgrade_state (
             id                INTEGER PRIMARY KEY,
             media_item_id     INTEGER NOT NULL REFERENCES media_item(id) ON DELETE CASCADE,
             episode_id        INTEGER REFERENCES episode(id) ON DELETE CASCADE,
             last_checked_at   TEXT    NOT NULL,
             check_count       INTEGER NOT NULL DEFAULT 1,
             last_result       TEXT    NOT NULL
                                CHECK (last_result IN ('grabbed','no_results','no_viable','error'))
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_upgrade_state_episode
             ON upgrade_state(episode_id) WHERE episode_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS idx_upgrade_state_movie
             ON upgrade_state(media_item_id) WHERE episode_id IS NULL;

         -- A queryable decision log (grabbed/review_queued/imported/failed),
         -- separate from `release`'s current-state columns — added after
         -- a night of reconstructing why-did-it-do-that purely from
         -- journal logs. `media_item_id` has no FK so a row survives even
         -- after its media_item is deleted (the point is historical
         -- record, not live referential integrity).
         CREATE TABLE IF NOT EXISTS event_history (
             id             INTEGER PRIMARY KEY,
             media_item_id  INTEGER,
             episode_id     INTEGER,
             event_type     TEXT    NOT NULL
                            CHECK (event_type IN ('grabbed','review_queued','imported','failed')),
             detail         TEXT    NOT NULL,
             occurred_at    TEXT    NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_event_history_media_item
             ON event_history(media_item_id, occurred_at);

         -- Default profiles. Scoring reads these rows' `weights` on every grab.
         INSERT OR IGNORE INTO quality_profile (id, name, kind, weights)
             VALUES (1, 'Default TV', 'tv', '{}');
         INSERT OR IGNORE INTO quality_profile (id, name, kind, weights)
             VALUES (2, 'Default Movie', 'movie', '{}');

         -- One row per `episode_file`, ffprobe's ground-truth read of what's
         -- actually on disk — a deliberate companion table rather than more
         -- columns on `episode_file` (which already carries three vestigial,
         -- never-populated columns from an earlier attempt at this: don't
         -- compound that mistake). `probe_size_bytes`/`probe_mtime` are a
         -- freshness guard, not just metadata: `reconcile_missing_files`
         -- repairs a stale `episode_file.path` onto the *same row id* when
         -- a file is renamed or transcoded in place (e.g. Tdarr converting
         -- codec/container), so a probe keyed only to that row id would
         -- silently describe a file that no longer exists unless something
         -- notices the underlying bytes changed.
         CREATE TABLE IF NOT EXISTS media_file_probe (
             episode_file_id   INTEGER PRIMARY KEY
                               REFERENCES episode_file(id) ON DELETE CASCADE,
             probed_at         TEXT    NOT NULL,
             probe_size_bytes  INTEGER NOT NULL,
             probe_mtime       INTEGER NOT NULL,

             duration_secs       REAL,
             container_bitrate   INTEGER,
             video_codec         TEXT,
             width               INTEGER,
             height              INTEGER,
             video_bitrate       INTEGER,
             audio_codecs        TEXT,
             audio_langs         TEXT,
             default_audio_lang  TEXT,
             subtitle_langs      TEXT,

             corruption_status TEXT NOT NULL DEFAULT 'unknown'
                               CHECK (corruption_status IN
                                 ('unknown','probe_ok','probe_failed','decode_ok','decode_failed')),
             corruption_checked_at TEXT,

             flag_under_quality             INTEGER NOT NULL DEFAULT 0,
             flag_no_subtitles              INTEGER NOT NULL DEFAULT 0,
             flag_no_english_audio          INTEGER NOT NULL DEFAULT 0,
             flag_non_english_default_audio INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_probe_flags
             ON media_file_probe(flag_under_quality, flag_no_subtitles,
                                 flag_no_english_audio, flag_non_english_default_audio);
         CREATE INDEX IF NOT EXISTS idx_probe_corruption
             ON media_file_probe(corruption_status);

         -- Permanent audit trail of every torrent breadarr has ever handed
         -- to qBittorrent, independent of whatever later happens to the
         -- `release` row it's associated with (which can be deleted, or
         -- flipped between grabbed/failed/imported/upgraded over time). No
         -- foreign keys — same reasoning as `event_history`: the point is a
         -- durable historical record, not live referential integrity, so a
         -- row must survive its source/media_item being deleted.
         CREATE TABLE IF NOT EXISTS torrent_fetch (
             id             INTEGER PRIMARY KEY,
             torrent_hash   TEXT,
             name           TEXT    NOT NULL,
             size_bytes     INTEGER,
             category       TEXT,
             source_id      INTEGER,
             media_item_id  INTEGER,
             episode_id     INTEGER,
             status         TEXT    NOT NULL,
             fetched_at     TEXT    NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_torrent_fetch_hash ON torrent_fetch(torrent_hash);
         CREATE INDEX IF NOT EXISTS idx_torrent_fetch_fetched_at ON torrent_fetch(fetched_at);

         -- One row per file queued for AV1 transcoding, whether from the
         -- post-import async hook or the `transcode-library` backfill sweep.
         -- Persisted (not an in-memory queue) so a `pending`/`running` row
         -- left over from a daemon crash mid-encode just gets picked up
         -- again on the next tick instead of silently vanishing.
         CREATE TABLE IF NOT EXISTS transcode_job (
             id               INTEGER PRIMARY KEY,
             episode_file_id  INTEGER NOT NULL REFERENCES episode_file(id) ON DELETE CASCADE,
             status           TEXT NOT NULL DEFAULT 'pending'
                              CHECK (status IN ('pending','running','done','failed','skipped')),
             original_codec   TEXT,
             original_bytes   INTEGER,
             new_bytes        INTEGER,
             error            TEXT,
             queued_at        TEXT NOT NULL,
             finished_at      TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_transcode_job_status ON transcode_job(status);
         -- Prevents two jobs for the same file ever being active at once —
         -- closes the door on the same file getting encoded twice
         -- concurrently regardless of how it happened (a stray duplicate
         -- enqueue, a daemon-restart reset racing a still-alive backfill).
         CREATE UNIQUE INDEX IF NOT EXISTS idx_transcode_job_active_episode_file
             ON transcode_job(episode_file_id) WHERE status IN ('pending','running');

         CREATE INDEX IF NOT EXISTS idx_release_status ON release(status);
         CREATE INDEX IF NOT EXISTS idx_release_torrent_hash ON release(torrent_hash);
         CREATE INDEX IF NOT EXISTS idx_release_episode_id ON release(episode_id);
         CREATE INDEX IF NOT EXISTS idx_episode_file_episode_id ON episode_file(episode_id);
         CREATE INDEX IF NOT EXISTS idx_review_queue_status ON review_queue(status);
         CREATE INDEX IF NOT EXISTS idx_episode_air_date ON episode(air_date);",
    )?;

    // Progress watermark for stalled-download detection (added after the
    // initial `release` schema — see `add_column_if_missing`).
    add_column_if_missing(conn, "release", "last_seen_progress", "REAL")?;
    add_column_if_missing(conn, "release", "last_progress_at", "TEXT")?;
    // Counts consecutive `import_one` failures for a completed torrent —
    // without this, a persistent import error (a bad path remap, an
    // unreadable file) retried forever every cycle with no escalation,
    // permanently blocking the episode/movie from being re-searched via a
    // different release (`status` stays `grabbed`) while never actually
    // succeeding either.
    add_column_if_missing(
        conn,
        "release",
        "import_error_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Set only for season-pack grabs (`episode_id` stays NULL for these,
    // same convention as movies — but unlike a movie, one media_item can
    // have many *different* season packs, so episode_id-is-NULL alone
    // can't disambiguate "season 1 pack" from "season 2 pack" the way it
    // disambiguates "this movie" from nothing else. This column is that
    // disambiguator, used to scope existing-grab/upgrade comparisons and
    // in-flight checks to the right season.
    add_column_if_missing(conn, "release", "season_number", "INTEGER")?;

    // Extra per-file metadata beyond the original probe columns — kept as
    // its own batch of `add_column_if_missing` calls (rather than folded
    // into the original `CREATE TABLE`) since that table already exists on
    // deployed databases. None of this feeds any decision yet; the point is
    // to have it already sitting in the DB, ready for whatever uses it
    // later, rather than needing a second full-library re-probe when that
    // day comes.
    add_column_if_missing(conn, "media_file_probe", "frame_rate", "REAL")?;
    add_column_if_missing(
        conn,
        "media_file_probe",
        "default_audio_channels",
        "INTEGER",
    )?;
    // Best-effort classification (see MediaProbe::is_hdr) computed at
    // probe time from the raw color_transfer tag, which is also kept
    // verbatim below rather than only storing the reduced boolean.
    add_column_if_missing(
        conn,
        "media_file_probe",
        "hdr",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "media_file_probe", "color_transfer", "TEXT")?;
    add_column_if_missing(conn, "media_file_probe", "container_format", "TEXT")?;
    // The complete ffprobe JSON response, verbatim — the escape hatch for
    // anything not already worth its own column (chapters, encoder tag,
    // less-common stream metadata). See `ffprobe::MediaProbe::raw_json`.
    add_column_if_missing(conn, "media_file_probe", "raw_ffprobe_json", "TEXT")?;

    // Set once a file has been through a successful local AV1 transcode —
    // stops the upgrade cycle from treating it as still needing a bigger
    // HEVC/H264 release, since `best_existing_score` otherwise only ever
    // sees the stored `release.score` from original-grab time, which a
    // local re-encode never touches.
    add_column_if_missing(
        conn,
        "episode_file",
        "upgrade_locked",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    // Decided at enqueue time (path prefix match against
    // `transcode.anime_root_folders`, OR'd with the `anime_mapping`/
    // `anime_tmdb_movie` metadata check) and carried on the job row so
    // `claim_pending_jobs` can dispatch straight to the right encode
    // pipeline (`run_ffmpeg_encode_anime` vs `_live_action`) without
    // re-deriving it — the eligibility metadata lookups aren't available
    // from the job row's own columns alone (no media_item_id here).
    add_column_if_missing(
        conn,
        "transcode_job",
        "is_anime",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    // Lets a job re-encode a file that's already AV1 — normally
    // `encode_and_verify` treats "already AV1" as nothing-to-do and skips
    // the encode entirely, which is right for a fresh library scan but
    // wrong for deliberately re-transcoding a file that got mis-encoded
    // (e.g. the 476 files a real rate-control bug left *larger* than their
    // original — already AV1, so the normal backfill query skips them, but
    // they're exactly what a remediation pass needs to revisit). See
    // `find_oversized_av1_candidates`.
    add_column_if_missing(
        conn,
        "transcode_job",
        "force_reencode",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    ensure_indexes(conn)?;

    Ok(())
}

fn index_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// `CREATE UNIQUE INDEX IF NOT EXISTS` still errors when existing rows
/// violate uniqueness (IF NOT EXISTS only checks the index name). A dirty
/// production DB must still start, so uniqueness failures warn and skip.
fn create_unique_index_best_effort(conn: &Connection, name: &str, ddl: &str) -> bool {
    match conn.execute(ddl, []) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                index = name,
                error = %e,
                "skipping unique index; existing rows would violate it"
            );
            false
        }
    }
}

/// Delete extra `media_item` rows that share a non-NULL `tvdb_id`/`tmdb_id`,
/// keeping the lowest `id`. Extras with any `episode` or `episode_file`
/// rows are left in place — those are not safe to drop. Returns whether
/// every duplicate group was reduced to a single row.
fn dedupe_media_item_external_id(conn: &Connection, column: &str) -> anyhow::Result<bool> {
    debug_assert!(column == "tvdb_id" || column == "tmdb_id");
    let sql = format!(
        "SELECT {column}, MIN(id) FROM media_item
         WHERE {column} IS NOT NULL
         GROUP BY {column}
         HAVING COUNT(*) > 1"
    );
    let dupes: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut safe = true;
    let extra_sql = format!("SELECT id FROM media_item WHERE {column} = ?1 AND id != ?2");
    for (ext_id, keep_id) in dupes {
        let extras: Vec<i64> = {
            let mut stmt = conn.prepare(&extra_sql)?;
            let rows = stmt.query_map(rusqlite::params![ext_id, keep_id], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for extra_id in extras {
            let episode_count: i64 = conn.query_row(
                "SELECT count(*) FROM episode WHERE media_item_id = ?1",
                [extra_id],
                |row| row.get(0),
            )?;
            let file_count: i64 = conn.query_row(
                "SELECT count(*) FROM episode_file WHERE media_item_id = ?1",
                [extra_id],
                |row| row.get(0),
            )?;
            if episode_count == 0 && file_count == 0 {
                conn.execute("DELETE FROM media_item WHERE id = ?1", [extra_id])?;
            } else {
                tracing::warn!(
                    column,
                    ext_id,
                    keep_id,
                    extra_id,
                    episode_count,
                    file_count,
                    "cannot safely dedupe media_item; extra row has episodes or files"
                );
                safe = false;
            }
        }
    }
    Ok(safe)
}

fn ensure_unique_external_id_index(
    conn: &Connection,
    column: &str,
    unique_name: &str,
    unique_ddl: &str,
    lookup_ddl: &str,
) -> anyhow::Result<()> {
    let unique_ok = if dedupe_media_item_external_id(conn, column)? {
        create_unique_index_best_effort(conn, unique_name, unique_ddl)
    } else {
        tracing::warn!(
            index = unique_name,
            column,
            "skipping unique index; media_item still has unsafely-duplicated rows"
        );
        false
    };
    if !unique_ok && !index_exists(conn, unique_name)? {
        conn.execute(lookup_ddl, [])?;
    }
    Ok(())
}

fn ensure_indexes(conn: &Connection) -> anyhow::Result<()> {
    ensure_unique_external_id_index(
        conn,
        "tvdb_id",
        "idx_media_item_tvdb",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_media_item_tvdb
         ON media_item(tvdb_id) WHERE tvdb_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_media_item_tvdb_lookup ON media_item(tvdb_id)",
    )?;
    ensure_unique_external_id_index(
        conn,
        "tmdb_id",
        "idx_media_item_tmdb",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_media_item_tmdb
         ON media_item(tmdb_id) WHERE tmdb_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_media_item_tmdb_lookup ON media_item(tmdb_id)",
    )?;

    create_unique_index_best_effort(
        conn,
        "idx_episode_file_path",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_episode_file_path ON episode_file(path)",
    );
    create_unique_index_best_effort(
        conn,
        "idx_alias_item_text",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_alias_item_text ON alias(media_item_id, text)",
    );
    create_unique_index_best_effort(
        conn,
        "idx_review_pending_title",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_review_pending_title
         ON review_queue(candidate_media_item_id, raw_release_title)
         WHERE status = 'pending'",
    );
    // No unique on release(source_id, guid): status can cycle and a
    // re-grab of the same guid is legitimate. `seen_guid` already records
    // first-seen.

    Ok(())
}

/// Appends one row to `event_history`. `event_type` must be one of the
/// values the table's `CHECK` constraint allows — a bad value surfaces
/// immediately as a `rusqlite::Error`, not a silent no-op.
pub fn record_event(
    conn: &Connection,
    media_item_id: i64,
    episode_id: Option<i64>,
    event_type: &str,
    detail: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO event_history (media_item_id, episode_id, event_type, detail, occurred_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        rusqlite::params![media_item_id, episode_id, event_type, detail],
    )?;
    Ok(())
}

/// Appends one row to `torrent_fetch` — the permanent audit log, distinct
/// from `release` (whose status/hash can change or which can be deleted
/// alongside its media_item). Called once per actual qBittorrent add,
/// regardless of what happens to the grab afterward.
#[allow(clippy::too_many_arguments)]
pub fn record_torrent_fetch(
    conn: &Connection,
    torrent_hash: Option<&str>,
    name: &str,
    size_bytes: Option<u64>,
    category: &str,
    source_id: i64,
    media_item_id: i64,
    episode_id: Option<i64>,
    status: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO torrent_fetch (torrent_hash, name, size_bytes, category, source_id, media_item_id, episode_id, status, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
        rusqlite::params![torrent_hash, name, size_bytes, category, source_id, media_item_id, episode_id, status],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();

        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 18);
    }

    #[test]
    fn record_event_inserts_a_queryable_row() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        record_event(&conn, 42, Some(7), "grabbed", "score=8.5").unwrap();

        let (media_item_id, episode_id, event_type, detail): (i64, Option<i64>, String, String) =
            conn.query_row(
                "SELECT media_item_id, episode_id, event_type, detail FROM event_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(media_item_id, 42);
        assert_eq!(episode_id, Some(7));
        assert_eq!(event_type, "grabbed");
        assert_eq!(detail, "score=8.5");
    }

    #[test]
    fn record_event_rejects_an_unknown_event_type() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        assert!(record_event(&conn, 1, None, "not_a_real_type", "x").is_err());
    }

    #[test]
    fn record_torrent_fetch_inserts_a_queryable_row() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        record_torrent_fetch(
            &conn,
            Some("deadbeef"),
            "Some.Release.Title",
            Some(1_500_000_000),
            "breadarr",
            1,
            42,
            Some(7),
            "grabbed",
        )
        .unwrap();

        let (hash, name, size, status): (Option<String>, String, Option<i64>, String) = conn
            .query_row(
                "SELECT torrent_hash, name, size_bytes, status FROM torrent_fetch",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(hash.as_deref(), Some("deadbeef"));
        assert_eq!(name, "Some.Release.Title");
        assert_eq!(size, Some(1_500_000_000));
        assert_eq!(status, "grabbed");
    }

    #[test]
    fn record_torrent_fetch_allows_a_null_hash_and_size() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        record_torrent_fetch(
            &conn,
            None,
            "Uncorrelated Add",
            None,
            "breadarr",
            1,
            42,
            None,
            "failed",
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM torrent_fetch", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn backup_before_open_is_a_noop_when_no_database_exists_yet() {
        let dir = std::env::temp_dir().join(format!("breadarr-backup-noop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("breadarr.db");

        backup_before_open(&db_path).unwrap();
        let backup_dir = dir.join("backups");
        assert!(
            !backup_dir.exists(),
            "no backup dir should be created for a fresh install"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn write_real_sqlite(db_path: &std::path::Path) {
        let conn = Connection::open(db_path).unwrap();
        init(&conn).unwrap();
        drop(conn);
    }

    fn seed_movie(conn: &Connection, title: &str, tmdb_id: i64) -> i64 {
        conn.execute(
            "INSERT INTO media_item (kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
             VALUES ('movie', ?1, NULL, ?2, 1, 2, '/tmp')",
            rusqlite::params![title, tmdb_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn init_creates_unique_and_lookup_indexes_on_a_clean_database() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        assert!(index_exists(&conn, "idx_media_item_tvdb").unwrap());
        assert!(index_exists(&conn, "idx_media_item_tmdb").unwrap());
        assert!(index_exists(&conn, "idx_episode_file_path").unwrap());
        assert!(index_exists(&conn, "idx_alias_item_text").unwrap());
        assert!(index_exists(&conn, "idx_release_status").unwrap());
        assert!(index_exists(&conn, "idx_release_torrent_hash").unwrap());
        assert!(index_exists(&conn, "idx_release_episode_id").unwrap());
        assert!(index_exists(&conn, "idx_episode_file_episode_id").unwrap());
        assert!(index_exists(&conn, "idx_review_queue_status").unwrap());
        assert!(index_exists(&conn, "idx_review_pending_title").unwrap());
        assert!(index_exists(&conn, "idx_episode_air_date").unwrap());
        assert!(!index_exists(&conn, "idx_media_item_tvdb_lookup").unwrap());
        assert!(!index_exists(&conn, "idx_media_item_tmdb_lookup").unwrap());
    }

    #[test]
    fn init_dedupes_empty_duplicate_tmdb_rows_and_creates_unique_index() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn.execute("DROP INDEX IF EXISTS idx_media_item_tmdb", [])
            .unwrap();

        let first = seed_movie(&conn, "A", 99);
        seed_movie(&conn, "B", 99);

        init(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM media_item WHERE tmdb_id = 99",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let kept: i64 = conn
            .query_row("SELECT id FROM media_item WHERE tmdb_id = 99", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, first);
        assert!(index_exists(&conn, "idx_media_item_tmdb").unwrap());
    }

    #[test]
    fn init_skips_tmdb_unique_index_when_duplicate_has_files() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn.execute("DROP INDEX IF EXISTS idx_media_item_tmdb", [])
            .unwrap();

        seed_movie(&conn, "A", 99);
        let extra = seed_movie(&conn, "B", 99);
        conn.execute(
            "INSERT INTO episode_file (media_item_id, path, size_bytes, subtitle_status)
             VALUES (?1, '/tmp/b.mkv', 1, 'none')",
            [extra],
        )
        .unwrap();

        init(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM media_item WHERE tmdb_id = 99",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert!(!index_exists(&conn, "idx_media_item_tmdb").unwrap());
        assert!(index_exists(&conn, "idx_media_item_tmdb_lookup").unwrap());
    }

    #[test]
    fn init_skips_path_unique_index_when_duplicates_exist() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn.execute("DROP INDEX IF EXISTS idx_episode_file_path", [])
            .unwrap();

        let id = seed_movie(&conn, "A", 1);
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO episode_file (media_item_id, path, size_bytes, subtitle_status)
                 VALUES (?1, '/tmp/x.mkv', 1, 'none')",
                [id],
            )
            .unwrap();
        }

        init(&conn).unwrap();
        assert!(!index_exists(&conn, "idx_episode_file_path").unwrap());
    }

    #[test]
    fn backup_before_open_copies_an_existing_database() {
        let dir = std::env::temp_dir().join(format!("breadarr-backup-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("breadarr.db");
        write_real_sqlite(&db_path);

        backup_before_open(&db_path).unwrap();

        let backup_dir = dir.join("backups");
        let backups: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1, "expected exactly one backup file");

        let verify = Connection::open(&backups[0]).unwrap();
        let n: i64 = verify
            .query_row("SELECT count(*) FROM quality_profile", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_before_open_prunes_beyond_max_backups() {
        let dir =
            std::env::temp_dir().join(format!("breadarr-backup-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("breadarr.db");
        write_real_sqlite(&db_path);

        // One more than MAX_BACKUPS, sleeping a few ms between each so the
        // millisecond-resolution timestamp in the filename is guaranteed to
        // differ and sort correctly.
        for _ in 0..(MAX_BACKUPS + 2) {
            backup_before_open(&db_path).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let backup_dir = dir.join("backups");
        let backups: Vec<_> = std::fs::read_dir(&backup_dir).unwrap().collect();
        assert_eq!(backups.len(), MAX_BACKUPS);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
