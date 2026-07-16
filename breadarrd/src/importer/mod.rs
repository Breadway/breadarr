pub mod ffprobe;
pub mod mkv;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::jellyfin::JellyfinClient;
use crate::qbit::QbitClient;

pub(crate) const VIDEO_EXTS: &[&str] = &["mkv", "mp4", "avi", "mov"];

/// Height/width floor for `flag_under_quality`, matching the 1080p bar
/// already established by the scoring gate's own "reject a sub-1080p
/// release when a better alternative exists" rule (`scoring/gate.rs`),
/// applied here to the actual decoded file rather than a claimed
/// title-text resolution. Not configurable, unlike `QualityProfile`'s
/// per-axis weights (see `QualityProfile::with_weights_override`): this is
/// a post-import ground-truth sanity floor, not a scoring preference, so it
/// stays a constant rather than a config knob.
///
/// **Both** width and height must be below their own floor before a file
/// is flagged — height alone isn't a valid "1080p or not" test, since a
/// wider-than-16:9 master (2.00:1 is common for prestige/streaming shows;
/// e.g. Apple TV+'s "For All Mankind" ships at 1920x960) legitimately has
/// full 1920px width with height well under 1080 purely from the aspect
/// ratio, not from being a worse encode. Verified live: 49 real episodes
/// at 1920x960 were false-positive-flagged under a height-only check
/// before this was caught. Width is the aspect-ratio-invariant half of the
/// pair, so requiring *both* dimensions to read low is what actually
/// distinguishes "genuinely low resolution" from "correctly cropped."
const UNDER_QUALITY_HEIGHT: i64 = 1080;
const UNDER_QUALITY_WIDTH: i64 = 1920;

enum PendingGrab {
    Episode {
        release_id: i64,
        episode_id: i64,
        media_item_id: i64,
        torrent_hash: String,
        series_title: String,
        season_number: u32,
        episode_number: u32,
        episode_title: Option<String>,
        root_folder: String,
    },
    Movie {
        release_id: i64,
        media_item_id: i64,
        torrent_hash: String,
        title: String,
        year: Option<i64>,
        root_folder: String,
    },
    /// A season-pack/batch torrent: one release row, but potentially many
    /// files inside mapping to many episodes — see `import_season_pack`.
    /// `episode_id` stays absent on the release row (`season_number`
    /// disambiguates instead, same convention as everywhere else this
    /// column is used).
    SeasonPack {
        release_id: i64,
        media_item_id: i64,
        torrent_hash: String,
        series_title: String,
        season_number: u32,
        root_folder: String,
    },
}

impl PendingGrab {
    fn release_id(&self) -> i64 {
        match self {
            PendingGrab::Episode { release_id, .. }
            | PendingGrab::Movie { release_id, .. }
            | PendingGrab::SeasonPack { release_id, .. } => *release_id,
        }
    }

    fn media_item_id(&self) -> i64 {
        match self {
            PendingGrab::Episode { media_item_id, .. }
            | PendingGrab::Movie { media_item_id, .. }
            | PendingGrab::SeasonPack { media_item_id, .. } => *media_item_id,
        }
    }

    fn torrent_hash(&self) -> &str {
        match self {
            PendingGrab::Episode { torrent_hash, .. }
            | PendingGrab::Movie { torrent_hash, .. }
            | PendingGrab::SeasonPack { torrent_hash, .. } => torrent_hash,
        }
    }

    fn episode_id(&self) -> Option<i64> {
        match self {
            PendingGrab::Episode { episode_id, .. } => Some(*episode_id),
            PendingGrab::Movie { .. } | PendingGrab::SeasonPack { .. } => None,
        }
    }

    fn root_folder(&self) -> &str {
        match self {
            PendingGrab::Episode { root_folder, .. }
            | PendingGrab::Movie { root_folder, .. }
            | PendingGrab::SeasonPack { root_folder, .. } => root_folder,
        }
    }
}

/// Three separate queries (rather than one `LEFT JOIN episode`) because a
/// movie/season-pack release's `episode_id` is NULL, which would otherwise
/// force every episode-only column to be handled as `Option` for no
/// benefit — matches the movie/episode split already used elsewhere
/// (`process_item`, `enumerate_search_targets`).
fn fetch_pending_grabs(conn: &Connection) -> Result<Vec<PendingGrab>> {
    let mut out = Vec::new();

    let mut ep_stmt = conn.prepare(
        "SELECT r.id, r.episode_id, r.media_item_id, r.torrent_hash, m.title, e.season_number, e.episode_number, e.title, m.root_folder
         FROM release r
         JOIN episode e ON e.id = r.episode_id
         JOIN media_item m ON m.id = r.media_item_id
         WHERE r.status = 'grabbed' AND r.torrent_hash IS NOT NULL",
    )?;
    let ep_rows = ep_stmt.query_map([], |row| {
        Ok(PendingGrab::Episode {
            release_id: row.get(0)?,
            episode_id: row.get(1)?,
            media_item_id: row.get(2)?,
            torrent_hash: row.get(3)?,
            series_title: row.get(4)?,
            season_number: row.get(5)?,
            episode_number: row.get(6)?,
            episode_title: row.get(7)?,
            root_folder: row.get(8)?,
        })
    })?;
    out.extend(ep_rows.collect::<rusqlite::Result<Vec<_>>>()?);

    let mut movie_stmt = conn.prepare(
        "SELECT r.id, r.media_item_id, r.torrent_hash, m.title, m.year, m.root_folder
         FROM release r
         JOIN media_item m ON m.id = r.media_item_id
         WHERE r.status = 'grabbed' AND r.torrent_hash IS NOT NULL AND r.episode_id IS NULL AND r.season_number IS NULL",
    )?;
    let movie_rows = movie_stmt.query_map([], |row| {
        Ok(PendingGrab::Movie {
            release_id: row.get(0)?,
            media_item_id: row.get(1)?,
            torrent_hash: row.get(2)?,
            title: row.get(3)?,
            year: row.get(4)?,
            root_folder: row.get(5)?,
        })
    })?;
    out.extend(movie_rows.collect::<rusqlite::Result<Vec<_>>>()?);

    let mut pack_stmt = conn.prepare(
        "SELECT r.id, r.media_item_id, r.torrent_hash, m.title, r.season_number, m.root_folder
         FROM release r
         JOIN media_item m ON m.id = r.media_item_id
         WHERE r.status = 'grabbed' AND r.torrent_hash IS NOT NULL AND r.episode_id IS NULL AND r.season_number IS NOT NULL",
    )?;
    let pack_rows = pack_stmt.query_map([], |row| {
        Ok(PendingGrab::SeasonPack {
            release_id: row.get(0)?,
            media_item_id: row.get(1)?,
            torrent_hash: row.get(2)?,
            series_title: row.get(3)?,
            season_number: row.get(4)?,
            root_folder: row.get(5)?,
        })
    })?;
    out.extend(pack_rows.collect::<rusqlite::Result<Vec<_>>>()?);

    Ok(out)
}

fn locate_video_file(content_path: &Path) -> Result<PathBuf> {
    if content_path.is_file() {
        return Ok(content_path.to_path_buf());
    }
    largest_video_file(content_path)
}

pub(crate) fn largest_video_file(dir: &Path) -> Result<PathBuf> {
    let mut best: Option<(PathBuf, u64)> = None;
    for entry in walk_files(dir)? {
        let ext = entry
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !VIDEO_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let size = std::fs::metadata(&entry)?.len();
        if best.as_ref().is_none_or(|(_, s)| size > *s) {
            best = Some((entry, size));
        }
    }
    best.map(|(p, _)| p)
        .with_context(|| format!("no video file found under {}", dir.display()))
}

pub(crate) fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(walk_files(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

pub(crate) fn deterministic_filename(
    series_title: &str,
    season: u32,
    episode: u32,
    episode_title: Option<&str>,
    ext: &str,
) -> String {
    let series = sanitize(series_title);
    match episode_title.filter(|t| !t.is_empty()) {
        Some(t) => format!(
            "{series} - S{season:02}E{episode:02} - {}.{ext}",
            sanitize(t)
        ),
        None => format!("{series} - S{season:02}E{episode:02}.{ext}"),
    }
}

pub(crate) fn deterministic_movie_filename(title: &str, year: Option<i64>, ext: &str) -> String {
    let title = sanitize(title);
    match year {
        Some(y) => format!("{title} ({y}).{ext}"),
        None => format!("{title}.{ext}"),
    }
}

pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect()
}

/// True when there isn't enough free space at `dir` to hold `needed_bytes`.
/// Checked before every import — a library volume running out of space is a
/// real failure mode, and a plain `copy` with no such guard leaves an ENOSPC
/// partway through as a truncated file sitting at the final destination
/// path. `blocks_available` (rather than `blocks_free`) matches what `df`
/// reports, since it excludes space reserved for the superuser.
fn insufficient_space(dir: &Path, needed_bytes: u64) -> Result<bool> {
    let stat = nix::sys::statvfs::statvfs(dir)
        .with_context(|| format!("failed to stat filesystem for {}", dir.display()))?;
    let available = stat.blocks_available() * stat.fragment_size();
    Ok(available < needed_bytes)
}

/// Hardlinks into the destination (same filesystem, effectively free) and
/// falls back to a plain copy cross-filesystem — deliberately leaves `src`
/// untouched either way. The previous `rename`-then-delete behavior yanked
/// the file out of qBittorrent's payload directory on every import, leaving
/// qBittorrent holding a torrent whose data had vanished (an errored
/// "missing files" state, seeding stopped instantly). A hardlink costs
/// nothing extra on disk and lets qBittorrent keep seeding after import;
/// even the copy fallback preserves seeding, just at the cost of double
/// disk usage for that one file.
///
/// The copy path writes to a `.part` sibling of `dest` and only renames it
/// into place once the copy is complete (a same-filesystem rename, so
/// atomic) — a crash mid-copy leaves an orphaned `.part` file rather than a
/// truncated file at `dest`, so a retried import can't ever double-count a
/// half-written file as already present.
fn link_or_copy_file(src: &Path, dest: &Path) -> Result<()> {
    if std::fs::hard_link(src, dest).is_ok() {
        return Ok(());
    }
    copy_via_temp_file(src, dest)
}

/// The copy fallback's actual mechanics, split out so it's directly
/// testable without needing a real cross-filesystem boundary to force
/// `hard_link` to fail.
fn copy_via_temp_file(src: &Path, dest: &Path) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.part", dest.display()));
    std::fs::copy(src, &tmp)
        .with_context(|| format!("failed to copy {} to {}", src.display(), tmp.display()))?;
    std::fs::rename(&tmp, dest).with_context(|| {
        format!(
            "failed to move completed copy into place at {}",
            dest.display()
        )
    })?;
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct ImportStats {
    pub imported: usize,
    pub remuxed: usize,
    pub skipped_incomplete: usize,
    pub errors: usize,
    pub failed: usize,
    /// Freshly-imported files where the post-import ffprobe found a
    /// ground-truth problem the title text didn't reveal (under-quality
    /// despite a claimed-good resolution, or unreadable) — see
    /// `probe_indicates_import_problem`.
    pub quality_flagged: usize,
}

/// How long a grab can sit with its progress unchanged before it's declared
/// stalled (dead seeders, a torrent that will never complete) and released
/// back into the search pool.
const STALL_THRESHOLD_HOURS: f64 = 72.0;

/// How long a grab's torrent hash can be absent from qBittorrent's list
/// before it's treated as genuinely gone (manually removed, category
/// changed) rather than just not-yet-indexed since the grab.
const MISSING_GRACE_MINUTES: f64 = 15.0;

/// Advances the release's progress watermark, but only when progress has
/// genuinely increased — `last_progress_at` staying still while progress
/// stays still is exactly the signal `grab_is_stalled` looks for.
fn update_grab_progress(conn: &Connection, release_id: i64, progress: f64) -> Result<()> {
    let prev: Option<f64> = conn.query_row(
        "SELECT last_seen_progress FROM release WHERE id = ?1",
        params![release_id],
        |row| row.get(0),
    )?;
    if prev.is_none_or(|p| progress > p) {
        conn.execute(
            "UPDATE release SET last_seen_progress = ?2, last_progress_at = datetime('now') WHERE id = ?1",
            params![release_id, progress],
        )?;
    }
    Ok(())
}

/// True once a grab has gone `STALL_THRESHOLD_HOURS` without its progress
/// advancing. Falls back to `grabbed_at` when progress has never been
/// observed advancing at all (stuck at the same percentage, including 0%,
/// since the moment it was grabbed).
fn grab_is_stalled(conn: &Connection, release_id: i64) -> Result<bool> {
    conn.query_row(
        "SELECT (julianday('now') - julianday(COALESCE(last_progress_at, grabbed_at))) * 24.0 > ?2
         FROM release WHERE id = ?1",
        params![release_id, STALL_THRESHOLD_HOURS],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// True once a grabbed release's torrent hash has been missing from qBit's
/// torrent list for longer than a short grace period — long enough to rule
/// out qBit simply not having indexed a just-added torrent yet, short
/// enough that a torrent genuinely removed by hand doesn't block re-search
/// for days.
fn grab_missing_past_grace(conn: &Connection, release_id: i64) -> Result<bool> {
    conn.query_row(
        "SELECT (julianday('now') - julianday(grabbed_at)) * 24.0 * 60.0 > ?2
         FROM release WHERE id = ?1",
        params![release_id, MISSING_GRACE_MINUTES],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// How many consecutive `import_one` failures a completed torrent gets
/// before it's given up on. A few retries absorb transient issues (a
/// filesystem hiccup, a momentarily-unavailable mount); beyond that, the
/// same error every cycle forever means it's not going to fix itself, and
/// leaving `status = 'grabbed'` blocks the episode/movie from ever being
/// re-searched via a different release.
const MAX_IMPORT_ERRORS: i64 = 5;

/// Increments the release's import-error counter and returns the new
/// total, so the caller can decide whether it's crossed `MAX_IMPORT_ERRORS`.
fn record_import_error(conn: &Connection, release_id: i64) -> Result<i64> {
    conn.execute(
        "UPDATE release SET import_error_count = import_error_count + 1 WHERE id = ?1",
        params![release_id],
    )?;
    conn.query_row(
        "SELECT import_error_count FROM release WHERE id = ?1",
        params![release_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Marks a stalled or missing grab as failed. `'failed'` isn't in the
/// `('grabbed','downloading')` in-flight check used by
/// `enumerate_search_targets`/`movie_needs_grab`, so the episode or movie
/// becomes searchable again on the very next cycle — this is the fallback-
/// into-the-search-pool step, not just bookkeeping.
fn fail_grab(conn: &Connection, grab: &PendingGrab, reason: &str) -> Result<()> {
    conn.execute(
        "UPDATE release SET status = 'failed' WHERE id = ?1",
        params![grab.release_id()],
    )?;
    crate::db::record_event(
        conn,
        grab.media_item_id(),
        grab.episode_id(),
        "failed",
        reason,
    )?;
    tracing::warn!(
        release_id = grab.release_id(),
        media_item_id = grab.media_item_id(),
        reason,
        "grab failed, releasing back to the search pool"
    );
    Ok(())
}

/// Result of a `reconcile_missing_files` pass.
#[derive(Debug, Default, PartialEq)]
pub struct ReconcileOutcome {
    /// Rows whose file had simply moved (e.g. a show folder renamed since
    /// the row was written) — found by basename under the owning
    /// `root_folder` and repaired in place rather than cleared.
    pub repaired: usize,
    /// Rows genuinely gone: not at their stored path, and no file with the
    /// same name found anywhere under `root_folder` either.
    pub cleared: usize,
    /// True when the pass found more "genuinely gone" candidates than
    /// `RECONCILE_MAX_CLEAR_FRACTION` allows and refused to clear anything —
    /// see its doc comment for why.
    pub aborted: bool,
}

/// Refuse to clear more than this fraction of all tracked files in a single
/// pass. A healthy library loses files one or two at a time; a mount going
/// offline, a bulk rename that outpaces the basename-repair fallback below,
/// or a bug can instead make *every* row look gone at once — exactly the
/// shape of failure that isn't self-evident from any single row, only from
/// the batch as a whole.
const RECONCILE_MAX_CLEAR_FRACTION: f64 = 0.10;

/// ...but always allow at least this many, so a small library (where 10% is
/// less than one file) still self-heals from genuinely missing files.
const RECONCILE_MIN_CLEAR_FLOOR: usize = 10;

/// Clears `episode_file`/`has_file` state for files that no longer exist on
/// disk — a file deleted or moved by hand outside breadarr (or Jellyfin's
/// own library tools) previously left stale state forever: the episode
/// looked permanently satisfied and never got re-searched, no matter how
/// long the file had actually been gone. Runs on a slow, separate ticker
/// (disk state doesn't change on its own) — see `main.rs`.
///
/// Before treating a missing path as "gone," this looks for a file of the
/// same name (or, failing that, the same stem under a different video
/// extension — see `find_by_stem`) anywhere under the owning media item's
/// `root_folder` and repairs the stored path instead — the DB `path` column
/// is only ever written once, on import, so anything that changes a file's
/// location or name afterward (the library-normalization scan appending a
/// year to a show folder; Tdarr re-encoding a file to a different container
/// in place) silently strands every affected `episode_file.path` unless
/// something re-derives it. This is that something, and it also guards
/// against a transient mount outage (`root_folder` itself absent) being
/// misread as every file under it having vanished.
///
/// `dry_run` computes and logs what would happen without writing anything —
/// used for a one-time report against a freshly-restored database before
/// trusting this to run unattended again.
pub fn reconcile_missing_files(conn: &Connection, dry_run: bool) -> Result<ReconcileOutcome> {
    // `episode_file.media_item_id` is NULL for TV rows written by the
    // library scan (only `episode_id` is set there) and `episode_id` is
    // NULL for movie rows — resolving the owning `root_folder` needs both
    // joins, COALESCEd, or half the library silently skips repair.
    let mut stmt = conn.prepare(
        "SELECT ef.id, ef.episode_id, ef.path,
                COALESCE(mi_direct.root_folder, mi_ep.root_folder) AS root_folder
         FROM episode_file ef
         LEFT JOIN media_item mi_direct ON mi_direct.id = ef.media_item_id
         LEFT JOIN episode    e         ON e.id = ef.episode_id
         LEFT JOIN media_item mi_ep     ON mi_ep.id = e.media_item_id",
    )?;
    let rows: Vec<(i64, Option<i64>, String, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let total = rows.len();

    let mut to_repair: Vec<(i64, String)> = Vec::new();
    let mut to_clear: Vec<(i64, Option<i64>, String)> = Vec::new();

    for (file_id, episode_id, path, root_folder) in rows {
        if Path::new(&path).exists() {
            continue;
        }
        let Some(root_folder) = root_folder else {
            tracing::warn!(
                file_id,
                path,
                "missing file has no resolvable root_folder; skipping"
            );
            continue;
        };
        if !Path::new(&root_folder).is_dir() {
            // The whole show/movie folder is absent — almost certainly a
            // mount that isn't up yet, not hundreds of individually deleted
            // files. Never clear on this basis.
            tracing::warn!(
                file_id,
                root_folder,
                "owning root_folder is absent (mount offline?); skipping"
            );
            continue;
        }
        let stored_path = Path::new(&path);
        let found = stored_path
            .file_name()
            .and_then(|name| find_by_basename(Path::new(&root_folder), name))
            .or_else(|| {
                // Tdarr (this library's AV1-transcode pipeline) re-encodes a
                // file in place and can land it under a different extension
                // — same stem, e.g. `Upgrade (2018).mp4` becomes `Upgrade
                // (2018).mkv`, with the original removed. An exact basename
                // match won't see that; falling back to a stem match against
                // known video extensions catches it exactly like a folder
                // rename, instead of treating a freshly-transcoded file as a
                // deletion.
                let stem = stored_path.file_stem()?;
                find_by_stem(Path::new(&root_folder), stem)
            });
        match found {
            Some(new_path) => to_repair.push((file_id, new_path.to_string_lossy().into_owned())),
            None => to_clear.push((file_id, episode_id, path)),
        }
    }

    let clear_limit = (((total as f64) * RECONCILE_MAX_CLEAR_FRACTION).ceil() as usize)
        .max(RECONCILE_MIN_CLEAR_FLOOR);
    if to_clear.len() > clear_limit {
        tracing::error!(
            would_clear = to_clear.len(),
            would_repair = to_repair.len(),
            total,
            clear_limit,
            "reconcile wanted to clear an anomalous number of files in one pass — refusing \
             (a mount may be offline, or something changed how paths are laid out). \
             No rows were changed."
        );
        return Ok(ReconcileOutcome {
            aborted: true,
            ..Default::default()
        });
    }

    if dry_run {
        for (file_id, new_path) in &to_repair {
            tracing::info!(file_id, new_path, "[dry-run] would repair stale path");
        }
        for (file_id, episode_id, path) in &to_clear {
            tracing::info!(file_id, episode_id, path, "[dry-run] would clear");
        }
        return Ok(ReconcileOutcome {
            repaired: to_repair.len(),
            cleared: to_clear.len(),
            aborted: false,
        });
    }

    // One transaction so a mid-run error can't leave the library half
    // repaired/half cleared.
    let tx = conn.unchecked_transaction()?;
    for (file_id, new_path) in &to_repair {
        tx.execute(
            "UPDATE episode_file SET path = ?1 WHERE id = ?2",
            params![new_path, file_id],
        )?;
        tracing::info!(file_id, new_path, "repaired stale episode_file path");
    }
    for (file_id, episode_id, path) in &to_clear {
        tx.execute("DELETE FROM episode_file WHERE id = ?1", params![file_id])?;
        if let Some(episode_id) = episode_id {
            tx.execute(
                "UPDATE episode SET has_file = 0 WHERE id = ?1",
                params![episode_id],
            )?;
        }
        tracing::info!(
            file_id,
            episode_id,
            path,
            "cleared missing file from library state"
        );
    }
    tx.commit()?;

    Ok(ReconcileOutcome {
        repaired: to_repair.len(),
        cleared: to_clear.len(),
        aborted: false,
    })
}

/// First file found at any depth under `root` whose file name is exactly
/// `name` — used to relocate an `episode_file` row whose stored path no
/// longer exists but whose owning show/movie folder does, e.g. after the
/// folder itself was renamed. Bounded to one media item's own folder (a
/// handful of season directories at most), and safe from cross-show
/// collisions since deterministic filenames are unique within a show.
fn find_by_basename(root: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_by_basename(&path, name) {
                return Some(found);
            }
        } else if path.file_name() == Some(name) {
            return Some(path);
        }
    }
    None
}

/// First file found at any depth under `root` with the given file stem
/// (name minus extension) and a recognized video extension — the
/// transcode-in-place counterpart to `find_by_basename`: a re-encode can
/// change the container/extension (e.g. Tdarr converting `.mp4` to `.mkv`)
/// while keeping the stem, which an exact-name match won't see.
fn find_by_stem(root: &Path, stem: &std::ffi::OsStr) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_by_stem(&path, stem) {
                return Some(found);
            }
        } else {
            let ext_is_video = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| VIDEO_EXTS.contains(&e.to_lowercase().as_str()));
            if ext_is_video && path.file_stem() == Some(stem) {
                return Some(path);
            }
        }
    }
    None
}

/// Runs ffprobe on `path` and upserts the result into `media_file_probe`
/// against `episode_file_id` — but only if the file's size or mtime has
/// actually changed since the last probe, so a routine sweep over a large,
/// mostly-unchanged library is cheap after the first pass. This is also
/// what keeps a probe from silently describing stale content after
/// `reconcile_missing_files` repairs a renamed/transcoded file onto the
/// same `episode_file.id` — the freshness check catches the change and
/// forces a re-probe. Returns `true` if a probe actually ran, `false` if
/// skipped as already up to date. ffprobe itself failing (missing binary,
/// unreadable/corrupt file) is recorded as `corruption_status =
/// 'probe_failed'` rather than propagated — probing must never abort a
/// scan or import.
pub fn ensure_probed(conn: &Connection, episode_file_id: i64, path: &Path) -> Result<bool> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {} for probing", path.display()))?;
    let size = metadata.len() as i64;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let existing: Option<(i64, i64)> = conn
        .query_row(
            "SELECT probe_size_bytes, probe_mtime FROM media_file_probe WHERE episode_file_id = ?1",
            params![episode_file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if existing == Some((size, mtime)) {
        return Ok(false);
    }

    match ffprobe::probe(path) {
        Ok(probe) => upsert_probe(conn, episode_file_id, size, mtime, Some(&probe), "probe_ok")?,
        Err(e) => {
            tracing::warn!(
                episode_file_id,
                path = %path.display(),
                error = %e,
                "ffprobe failed; recording as probe_failed and continuing"
            );
            upsert_probe(conn, episode_file_id, size, mtime, None, "probe_failed")?;
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
/// Everything `upsert_probe` needs derived from a `MediaProbe` (or blanked
/// out entirely when probing itself failed) — a struct rather than the
/// wall-of-positional-`Option`s this used to be, since that stopped being
/// readable once the extra-metadata fields were added.
#[derive(Default)]
struct ProbeFields {
    duration: Option<f64>,
    container_bitrate: Option<i64>,
    container_format: Option<String>,
    video_codec: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    video_bitrate: Option<i64>,
    frame_rate: Option<f64>,
    hdr: bool,
    color_transfer: Option<String>,
    audio_codecs: Option<String>,
    audio_langs: Option<String>,
    default_audio_lang: Option<String>,
    default_audio_channels: Option<i64>,
    subtitle_langs: Option<String>,
    raw_json: Option<String>,
    under_quality: bool,
    no_subs: bool,
    no_eng_audio: bool,
    non_eng_default: bool,
}

impl ProbeFields {
    fn from_probe(p: &ffprobe::MediaProbe) -> Self {
        let audio_codecs = p
            .audio
            .iter()
            .filter_map(|a| a.codec.clone())
            .collect::<Vec<_>>()
            .join(",");
        let audio_langs = p
            .audio
            .iter()
            .filter_map(|a| a.language.clone())
            .collect::<Vec<_>>()
            .join(",");
        let default_audio = p.audio.iter().find(|a| a.is_default);
        let subtitle_langs = p
            .subtitles
            .iter()
            .filter_map(|s| s.language.clone())
            .collect::<Vec<_>>()
            .join(",");
        Self {
            duration: p.duration_secs,
            container_bitrate: p.container_bitrate,
            container_format: p.container_format.clone(),
            video_codec: p.video_codec.clone(),
            width: p.width,
            height: p.height,
            video_bitrate: p.video_bitrate,
            frame_rate: p.frame_rate,
            hdr: p.is_hdr(),
            color_transfer: p.color_transfer.clone(),
            audio_codecs: Some(audio_codecs),
            audio_langs: Some(audio_langs),
            default_audio_lang: default_audio.and_then(|a| a.language.clone()),
            default_audio_channels: default_audio.and_then(|a| a.channels),
            subtitle_langs: Some(subtitle_langs),
            raw_json: Some(p.raw_json.clone()),
            under_quality: p.height.is_some_and(|h| h < UNDER_QUALITY_HEIGHT)
                && p.width.is_some_and(|w| w < UNDER_QUALITY_WIDTH),
            no_subs: p.subtitles.is_empty(),
            no_eng_audio: !p.has_english_audio(),
            non_eng_default: p.default_audio_is_non_english(),
        }
    }
}

fn upsert_probe(
    conn: &Connection,
    episode_file_id: i64,
    size: i64,
    mtime: i64,
    probe: Option<&ffprobe::MediaProbe>,
    corruption_status: &str,
) -> Result<()> {
    let f = probe.map(ProbeFields::from_probe).unwrap_or_default();

    conn.execute(
        "INSERT INTO media_file_probe (
            episode_file_id, probed_at, probe_size_bytes, probe_mtime,
            duration_secs, container_bitrate, container_format,
            video_codec, width, height, video_bitrate, frame_rate, hdr, color_transfer,
            audio_codecs, audio_langs, default_audio_lang, default_audio_channels, subtitle_langs,
            raw_ffprobe_json,
            corruption_status, corruption_checked_at,
            flag_under_quality, flag_no_subtitles, flag_no_english_audio, flag_non_english_default_audio
         ) VALUES (?1, datetime('now'), ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, NULL, ?21, ?22, ?23, ?24)
         ON CONFLICT(episode_file_id) DO UPDATE SET
            probed_at = excluded.probed_at,
            probe_size_bytes = excluded.probe_size_bytes,
            probe_mtime = excluded.probe_mtime,
            duration_secs = excluded.duration_secs,
            container_bitrate = excluded.container_bitrate,
            container_format = excluded.container_format,
            video_codec = excluded.video_codec,
            width = excluded.width,
            height = excluded.height,
            video_bitrate = excluded.video_bitrate,
            frame_rate = excluded.frame_rate,
            hdr = excluded.hdr,
            color_transfer = excluded.color_transfer,
            audio_codecs = excluded.audio_codecs,
            audio_langs = excluded.audio_langs,
            default_audio_lang = excluded.default_audio_lang,
            default_audio_channels = excluded.default_audio_channels,
            subtitle_langs = excluded.subtitle_langs,
            raw_ffprobe_json = excluded.raw_ffprobe_json,
            corruption_status = excluded.corruption_status,
            corruption_checked_at = NULL,
            flag_under_quality = excluded.flag_under_quality,
            flag_no_subtitles = excluded.flag_no_subtitles,
            flag_no_english_audio = excluded.flag_no_english_audio,
            flag_non_english_default_audio = excluded.flag_non_english_default_audio",
        params![
            episode_file_id,
            size,
            mtime,
            f.duration,
            f.container_bitrate,
            f.container_format,
            f.video_codec,
            f.width,
            f.height,
            f.video_bitrate,
            f.frame_rate,
            f.hdr,
            f.color_transfer,
            f.audio_codecs,
            f.audio_langs,
            f.default_audio_lang,
            f.default_audio_channels,
            f.subtitle_langs,
            f.raw_json,
            corruption_status,
            f.under_quality,
            f.no_subs,
            f.no_eng_audio,
            f.non_eng_default,
        ],
    )?;
    Ok(())
}

/// Records the outcome of the expensive full-decode corruption check (see
/// `ffprobe::verify_decodable`) against an already-probed row. Separate
/// from `upsert_probe` since this is only ever called by the opt-in
/// `verify-library` pass, never by a routine probe.
pub fn record_decode_check(
    conn: &Connection,
    episode_file_id: i64,
    result: &ffprobe::DecodeCheck,
) -> Result<()> {
    let status = match result {
        ffprobe::DecodeCheck::Ok => "decode_ok",
        ffprobe::DecodeCheck::Corrupt(_) => "decode_failed",
    };
    conn.execute(
        "UPDATE media_file_probe SET corruption_status = ?1, corruption_checked_at = datetime('now')
         WHERE episode_file_id = ?2",
        params![status, episode_file_id],
    )?;
    Ok(())
}

/// True when a just-probed file has a problem worth alerting on
/// immediately (used for the post-import ground-truth quality check) —
/// deliberately narrower than "any flag is set": `flag_no_subtitles` and
/// `flag_non_english_default_audio` are common and already handled
/// elsewhere (the latter is fixed automatically by the remux-at-import step
/// right before this ever runs), so surfacing them here on every single
/// import would just be noise. Under-quality and probe failure are the two
/// signals that mean "this import silently isn't what it claimed to be."
fn probe_indicates_import_problem(conn: &Connection, episode_file_id: i64) -> Result<bool> {
    conn.query_row(
        "SELECT flag_under_quality OR corruption_status = 'probe_failed'
         FROM media_file_probe WHERE episode_file_id = ?1",
        params![episode_file_id],
        |row| row.get(0),
    )
    .optional()
    .map(|r| r.unwrap_or(false))
    .map_err(Into::into)
}

#[derive(Debug, Default)]
pub struct ProbeSweepReport {
    pub probed: usize,
    pub skipped_up_to_date: usize,
    pub failed: usize,
}

/// Incremental sweep over every tracked file: probes anything not probed
/// yet, or whose content has drifted (size/mtime) since it last was — the
/// backlog-and-drift half of keeping `media_file_probe` current, alongside
/// the inline probing done right after a file is linked or imported. Files
/// missing from disk are left alone (that's `reconcile_missing_files`'s
/// job, not this one's) rather than double-handled here.
/// Caps how many files actually get *probed* (not just checked-and-skipped)
/// in one `probe_library` call. Skipping this check runs an unbounded first
/// sweep against a large existing library in a single pass — each probe
/// spawns a real `ffprobe` subprocess, and the whole call holds the shared
/// `Connection` mutex the entire time (the same mutex `main.rs`'s grab/
/// import/search tickers need), so an unbounded sweep would stall those
/// cycles for however long a full library backlog takes. Capping it instead
/// spreads a large backlog across successive hourly ticks — see
/// `main.rs`'s reconcile ticker, which this shares a tick with.
const PROBE_SWEEP_BATCH_LIMIT: usize = 200;

pub fn probe_library(conn: &Connection) -> Result<ProbeSweepReport> {
    let mut stmt = conn.prepare("SELECT id, path FROM episode_file")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut report = ProbeSweepReport::default();
    for (episode_file_id, path) in rows {
        if report.probed >= PROBE_SWEEP_BATCH_LIMIT {
            break;
        }
        let p = Path::new(&path);
        if !p.exists() {
            continue;
        }
        match ensure_probed(conn, episode_file_id, p) {
            Ok(true) => report.probed += 1,
            Ok(false) => report.skipped_up_to_date += 1,
            Err(e) => {
                tracing::warn!(episode_file_id, path, error = %e, "probe sweep: failed to probe file");
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct VerifyLibraryReport {
    pub verified_ok: usize,
    pub corrupt: usize,
    pub errors: usize,
}

/// Runs the expensive full-decode corruption check
/// (`ffprobe::verify_decodable`) against every file whose cheap header
/// probe succeeded but hasn't yet been decode-verified
/// (`corruption_status = 'probe_ok'`) — confirming a file actually decodes
/// end-to-end, not just that its container header parses. `probe_failed`
/// files are skipped: ffprobe already couldn't parse them, so a decode
/// attempt would only confirm the same failure at much higher cost.
/// Deliberately never run by any ticker (this can take minutes per file —
/// see `ffprobe::verify_decodable`'s doc comment) — only ever runs when
/// explicitly invoked via `breadarrd verify-library`.
pub fn verify_library(conn: &Connection) -> Result<VerifyLibraryReport> {
    let mut stmt = conn.prepare(
        "SELECT ef.id, ef.path FROM episode_file ef
         JOIN media_file_probe p ON p.episode_file_id = ef.id
         WHERE p.corruption_status = 'probe_ok'",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut report = VerifyLibraryReport::default();
    for (episode_file_id, path) in rows {
        let source_path = Path::new(&path);
        if !source_path.exists() {
            continue; // reconcile's job, not ours
        }
        match ffprobe::verify_decodable(source_path) {
            Ok(result) => {
                record_decode_check(conn, episode_file_id, &result)?;
                match result {
                    ffprobe::DecodeCheck::Ok => {
                        report.verified_ok += 1;
                        tracing::info!(episode_file_id, path, "verify library: decodes cleanly");
                    }
                    ffprobe::DecodeCheck::Corrupt(detail) => {
                        report.corrupt += 1;
                        tracing::warn!(
                            episode_file_id,
                            path,
                            detail,
                            "verify library: decode failed, file looks corrupt"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(episode_file_id, path, error = %e, "verify library: failed to run decode check");
                report.errors += 1;
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct RemuxBacklogReport {
    pub remuxed: usize,
    pub skipped_not_mkv: usize,
    pub skipped_no_english_track: usize,
    pub errors: usize,
}

/// Applies the same "promote the English audio track to default" fix
/// (`mkv::remux_english_default`) that already runs automatically right
/// after a fresh download, but against files *already* sitting in the
/// library — the backlog that existed before this feature shipped, or any
/// file `media_file_probe` has flagged since. Deliberately not wired to any
/// automatic ticker (unlike `probe_library`, which is read-mostly): this
/// rewrites real files in the library, so it only ever runs when explicitly
/// invoked (see `breadarrd remux-backlog`).
///
/// Only `.mkv` files are eligible — `mkv::remux_english_default` retags
/// tracks via `mkvmerge`, the same constraint the at-import-time fix
/// already has.
pub fn remux_backlog(conn: &Connection) -> Result<RemuxBacklogReport> {
    let mut stmt = conn.prepare(
        "SELECT ef.id, ef.path FROM episode_file ef
         JOIN media_file_probe p ON p.episode_file_id = ef.id
         WHERE p.flag_non_english_default_audio = 1",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut report = RemuxBacklogReport::default();
    for (episode_file_id, path) in rows {
        let source_path = Path::new(&path);
        if !source_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
        {
            report.skipped_not_mkv += 1;
            continue;
        }
        if !source_path.exists() {
            continue; // reconcile's job, not ours
        }

        match remux_one_backlog_file(conn, episode_file_id, source_path) {
            Ok(true) => report.remuxed += 1,
            Ok(false) => report.skipped_no_english_track += 1,
            Err(e) => {
                tracing::warn!(episode_file_id, path, error = %e, "remux backlog: failed to remux file");
                report.errors += 1;
            }
        }
    }
    Ok(report)
}

/// Returns `true` if a remux actually happened, `false` if there was no
/// English track to promote (nothing to do — not an error).
fn remux_one_backlog_file(conn: &Connection, episode_file_id: i64, path: &Path) -> Result<bool> {
    let tracks = mkv::inspect_audio_tracks(path)?;
    if !mkv::has_english_track(&tracks) {
        return Ok(false);
    }

    let tmp = path.with_extension("fixed.mkv");
    mkv::remux_english_default(path, &tmp, &tracks)?;

    // Same atomic-swap shape as `copy_via_temp_file`: rename the freshly
    // remuxed output over the original on the same filesystem, so a crash
    // mid-swap can never leave a half-written file at the real path.
    std::fs::rename(&tmp, path)?;

    let size_bytes = std::fs::metadata(path)?.len();
    conn.execute(
        "UPDATE episode_file SET size_bytes = ?1 WHERE id = ?2",
        params![size_bytes, episode_file_id],
    )?;
    // Force a re-probe even though size may coincidentally match — a
    // flag-only remux barely changes file size, so the freshness check
    // could otherwise skip it. Clearing the stored probe row is simpler
    // than adding a "force" parameter to `ensure_probed` for this one caller.
    conn.execute(
        "DELETE FROM media_file_probe WHERE episode_file_id = ?1",
        params![episode_file_id],
    )?;
    ensure_probed(conn, episode_file_id, path)?;

    Ok(true)
}

/// Translates a path qBittorrent reports via its API into one breadarr can
/// actually open — needed when qBittorrent runs in a container (its own
/// downloads mounted at some internal prefix like "/downloads") while
/// breadarr runs natively on the same host. A no-op when either prefix is
/// empty (qBittorrent's reported path is used as-is).
fn remap_path(reported: &str, container_prefix: &str, host_prefix: &str) -> PathBuf {
    if container_prefix.is_empty() || host_prefix.is_empty() {
        return PathBuf::from(reported);
    }
    match reported.strip_prefix(container_prefix) {
        Some(rest) => PathBuf::from(format!("{host_prefix}{rest}")),
        None => PathBuf::from(reported),
    }
}

/// Marker directory name for the seeding-preserving staging area — checked
/// as a plain substring of a torrent's reported `content_path` to tell
/// whether it's already been relocated there in a previous cycle.
const STAGING_DIR_NAME: &str = ".breadarr-staging";

/// How many times to poll qBittorrent for a `setLocation` move to finish
/// before giving up for this cycle (it'll simply be retried next cycle —
/// see `relocate_completed_to_staging`).
const RELOCATE_POLL_ATTEMPTS: u32 = 5;
const RELOCATE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Finds the nearest ancestor of `path` that actually exists — `path`
/// itself (e.g. a show's season folder) may not have been created yet.
fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        current = current
            .parent()
            .with_context(|| format!("no existing ancestor found for {}", path.display()))?;
    }
}

/// Finds the filesystem mount-point directory containing `path`, by
/// walking up parents until the device id changes. Used to place the
/// seeding-staging directory on the same physical filesystem as the final
/// destination — the whole point of staging is that the later hardlink-out
/// in `import_one` is guaranteed to succeed as a true hardlink rather than
/// silently falling back to a full copy, and a hardlink can never cross a
/// filesystem boundary.
fn find_mount_root(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let start = nearest_existing_ancestor(path)?;
    let dev = std::fs::metadata(&start)?.dev();
    let mut current = start;
    loop {
        let Some(parent) = current.parent() else {
            return Ok(current);
        };
        let Ok(parent_meta) = std::fs::metadata(parent) else {
            return Ok(current);
        };
        if parent_meta.dev() != dev {
            return Ok(current);
        }
        current = parent.to_path_buf();
    }
}

/// The staging directory a given torrent's data should be relocated to —
/// on the same filesystem as `dest_dir`, named by torrent hash so multiple
/// torrents' leftover extras (samples/nfo/screenshots) never collide.
fn staging_dir_for(dest_dir: &Path, torrent_hash: &str) -> Result<PathBuf> {
    let mount_root = find_mount_root(dest_dir)?;
    Ok(mount_root.join(STAGING_DIR_NAME).join(torrent_hash))
}

/// For every pending grab whose torrent has finished downloading but whose
/// data isn't already staged, relocates it via qBittorrent's own
/// `setLocation` to a directory on the same filesystem as its eventual
/// destination, then waits (briefly, bounded) for the move to actually
/// finish — `setLocation` returns before the physical move completes.
///
/// Best-effort and self-healing by design: a grab that isn't staged yet
/// this cycle is simply retried on the next one (nothing here ever fails
/// the whole import cycle), so a slow move or a transient qBit API error
/// never blocks or loses anything — it just costs one extra cycle.
///
/// Returns whether anything was actually relocated, so the caller knows
/// whether it's worth re-fetching the torrent list before importing (a
/// relocated torrent's `content_path` only reflects its new home after a
/// fresh `list_torrents` call).
async fn relocate_completed_to_staging(
    qbit: &QbitClient,
    pending: &[PendingGrab],
    torrents: &[crate::qbit::TorrentInfo],
    category: &str,
) -> bool {
    let mut relocated_any = false;
    for grab in pending {
        let Some(torrent) = torrents.iter().find(|t| t.hash == grab.torrent_hash()) else {
            continue;
        };
        if torrent.progress < 1.0 {
            continue;
        }
        if torrent.content_path.contains(STAGING_DIR_NAME) {
            continue; // already staged in a previous cycle
        }

        let staging = match staging_dir_for(Path::new(grab.root_folder()), grab.torrent_hash()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    hash = grab.torrent_hash(),
                    "could not determine a staging directory this cycle"
                );
                continue;
            }
        };

        if let Err(e) = qbit
            .set_location(grab.torrent_hash(), &staging.to_string_lossy())
            .await
        {
            tracing::warn!(
                error = %e,
                hash = grab.torrent_hash(),
                "qbit relocate-to-staging failed, will retry next cycle"
            );
            continue;
        }

        if wait_for_relocation(qbit, grab.torrent_hash(), category, &staging).await {
            relocated_any = true;
        } else {
            tracing::warn!(
                hash = grab.torrent_hash(),
                "qbit relocate-to-staging didn't finish in time, will retry next cycle"
            );
        }
    }
    relocated_any
}

/// Waits for qBittorrent to report the torrent's `content_path` as staged,
/// then verifies the reported path actually landed where expected —
/// qBittorrent (when it's running in its own Docker container) reports *its
/// own* filesystem view, and `staging_dir_for`/`set_location` currently
/// rely on every library mount being set up as an identity mount (container
/// path == host path) for that reported path to be directly meaningful
/// from the host's side. That's a deployment convention, not something the
/// code enforces, so it can be wrong for a given install's mount layout. If
/// it is, treating the reported path as trustworthy without checking could
/// let `import_one`'s later hardlink-out silently fall back to a full
/// cross-device copy (or fail outright) instead of the guaranteed-cheap
/// hardlink staging exists to provide — so this confirms the reported path
/// resolves, from the host, to the *same physical filesystem* as
/// `expected_staging` before trusting it.
async fn wait_for_relocation(
    qbit: &QbitClient,
    hash: &str,
    category: &str,
    expected_staging: &Path,
) -> bool {
    use std::os::unix::fs::MetadataExt;

    for _ in 0..RELOCATE_POLL_ATTEMPTS {
        tokio::time::sleep(RELOCATE_POLL_INTERVAL).await;
        let Ok(torrents) = qbit.list_torrents(Some(category)).await else {
            continue;
        };
        let Some(t) = torrents.iter().find(|t| t.hash == hash) else {
            continue;
        };
        if !t.content_path.contains(STAGING_DIR_NAME) {
            continue;
        }
        let reported = Path::new(&t.content_path);
        match (
            std::fs::metadata(reported),
            std::fs::metadata(expected_staging),
        ) {
            (Ok(reported_meta), Ok(expected_meta))
                if reported_meta.dev() == expected_meta.dev() =>
            {
                return true;
            }
            _ => {
                tracing::error!(
                    hash,
                    reported = %t.content_path,
                    expected = %expected_staging.display(),
                    "qbit reports the torrent as staged, but its content_path isn't visible \
                     on the same host filesystem as expected — the container's mount for this \
                     library path may not be an identity mount; refusing to trust this relocation"
                );
                return false;
            }
        }
    }
    false
}

pub async fn run_import_cycle(
    conn: &Connection,
    qbit: &QbitClient,
    jellyfin: Option<&JellyfinClient>,
    category: &str,
    container_downloads_path: &str,
    host_downloads_path: &str,
) -> Result<ImportStats> {
    let pending = fetch_pending_grabs(conn)?;
    if pending.is_empty() {
        return Ok(ImportStats::default());
    }

    let torrents = qbit.list_torrents(Some(category)).await?;

    // Relocate completed-but-not-yet-staged torrents onto the same
    // filesystem as their destination *before* importing, so the
    // hardlink-out below (`link_or_copy_file`, unchanged) is a true
    // hardlink instead of a full cross-drive copy — qBittorrent keeps
    // seeding indefinitely from the staged location afterward, with no
    // permanent second copy of the data anywhere.
    let relocated_any = relocate_completed_to_staging(qbit, &pending, &torrents, category).await;
    let torrents = if relocated_any {
        qbit.list_torrents(Some(category)).await?
    } else {
        torrents
    };

    let stats = process_pending_grabs(
        conn,
        &pending,
        &torrents,
        container_downloads_path,
        host_downloads_path,
    )?;

    if stats.imported > 0 {
        if let Some(jellyfin) = jellyfin {
            jellyfin.refresh_library().await?;
        }
    }

    Ok(stats)
}

/// The actual hash-matching, progress-gating, stall/missing-detection, and
/// import decision logic — split out from `run_import_cycle` so it's
/// testable against a synthetic `&[TorrentInfo]` instead of requiring a
/// real qBittorrent server. `run_import_cycle`'s only other job is the one
/// network call (`list_torrents`) and the post-import Jellyfin refresh,
/// neither of which this function touches. This is exactly the kind of
/// unmockable I/O seam that let the `MagnetRejected` silent-failure bug
/// hide for as long as it did.
fn process_pending_grabs(
    conn: &Connection,
    pending: &[PendingGrab],
    torrents: &[crate::qbit::TorrentInfo],
    container_downloads_path: &str,
    host_downloads_path: &str,
) -> Result<ImportStats> {
    let mut stats = ImportStats::default();

    for grab in pending {
        let Some(torrent) = torrents.iter().find(|t| t.hash == grab.torrent_hash()) else {
            if grab_missing_past_grace(conn, grab.release_id())? {
                fail_grab(conn, grab, "torrent hash absent from qBittorrent")?;
                stats.failed += 1;
            }
            continue;
        };
        if torrent.progress < 1.0 {
            stats.skipped_incomplete += 1;
            update_grab_progress(conn, grab.release_id(), torrent.progress)?;
            if grab_is_stalled(conn, grab.release_id())? {
                fail_grab(
                    conn,
                    grab,
                    "no progress for longer than the stall threshold",
                )?;
                stats.failed += 1;
            }
            continue;
        }
        if torrent.state == "moving" {
            // qBittorrent's own `setLocation` relocation (or a manual move)
            // is still physically in flight — `content_path` may already
            // point at the new location while the actual bytes are still
            // being copied there. Importing now risks hardlinking/copying
            // a partially-moved (truncated) file into the library and
            // marking it complete. Simply wait; this is checked every
            // cycle, so it proceeds as soon as the move finishes.
            stats.skipped_incomplete += 1;
            continue;
        }

        let content_path = remap_path(
            &torrent.content_path,
            container_downloads_path,
            host_downloads_path,
        );

        if let PendingGrab::SeasonPack {
            release_id,
            media_item_id,
            series_title,
            season_number,
            root_folder,
            ..
        } = grab
        {
            match import_season_pack(
                conn,
                *release_id,
                *media_item_id,
                series_title,
                *season_number,
                root_folder,
                &content_path,
            ) {
                Ok(outcome) => {
                    stats.imported += outcome.episodes_imported;
                    stats.quality_flagged += outcome.quality_flagged;
                }
                Err(e) => {
                    let error_count = record_import_error(conn, *release_id)?;
                    if error_count >= MAX_IMPORT_ERRORS {
                        fail_grab(
                            conn,
                            grab,
                            &format!("season pack import failed {error_count} times in a row: {e}"),
                        )?;
                        stats.failed += 1;
                    } else {
                        tracing::warn!(
                            error = %e,
                            release_id = *release_id,
                            error_count,
                            "season pack import failed"
                        );
                        stats.errors += 1;
                    }
                }
            }
            continue;
        }

        match import_one(conn, grab, &content_path) {
            Ok(ImportOutcome::Imported {
                remuxed,
                quality_flagged,
            }) => {
                stats.imported += 1;
                if remuxed {
                    stats.remuxed += 1;
                }
                if quality_flagged {
                    stats.quality_flagged += 1;
                }
            }
            Ok(ImportOutcome::SkippedAlreadyHaveBetter) => {}
            Err(e) => {
                let error_count = record_import_error(conn, grab.release_id())?;
                if error_count >= MAX_IMPORT_ERRORS {
                    fail_grab(
                        conn,
                        grab,
                        &format!("import failed {error_count} times in a row: {e}"),
                    )?;
                    stats.failed += 1;
                } else {
                    tracing::warn!(
                        error = %e,
                        release_id = grab.release_id(),
                        error_count,
                        "import failed"
                    );
                    stats.errors += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// What actually happened when `import_one` was asked to import a
/// completed torrent — distinct from an error, `SkippedAlreadyHaveBetter`
/// is a deliberate no-op (see `import_one`'s dest-collision check), not a
/// failure, so callers shouldn't count it toward `ImportStats::imported`.
enum ImportOutcome {
    Imported {
        remuxed: bool,
        quality_flagged: bool,
    },
    SkippedAlreadyHaveBetter,
}

fn import_one(conn: &Connection, grab: &PendingGrab, content_path: &Path) -> Result<ImportOutcome> {
    let source_path = locate_video_file(content_path)?;
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv")
        .to_string();

    let mut remuxed = false;
    let mut working_path = source_path.clone();

    if ext.eq_ignore_ascii_case("mkv") {
        let tracks = mkv::inspect_audio_tracks(&source_path)?;
        let needs_fix =
            !mkv::default_track_is_english_or_unset(&tracks) && mkv::has_english_track(&tracks);
        if needs_fix {
            let tmp = source_path.with_extension("fixed.mkv");
            mkv::remux_english_default(&source_path, &tmp, &tracks)?;
            // `source_path` is qBittorrent's actual seeding payload for
            // this torrent — deleting it here, before the free-space check
            // and the import below have even run, defeats the whole
            // staging design (whose point is to keep seeding intact) and
            // risks real data loss if either subsequent step fails: the
            // original would already be gone, `working_path` might not
            // have made it into the library either, and qBittorrent can't
            // necessarily re-fetch a dead swarm. `link_or_copy_file`
            // already leaves its source untouched for exactly this reason
            // in the non-remux path (see its own doc comment) — the remux
            // scratch output below gets the same treatment: only removed
            // once it's been successfully imported.
            working_path = tmp;
            remuxed = true;
        }
        // Anime-JP-fallback case (no English track at all, allowed through
        // by the gate for anime) has nothing to fix — falls through to a
        // clean import below, same as an already-English-default file.
    }

    let (root_folder, filename, episode_id) = match grab {
        PendingGrab::Episode {
            series_title,
            season_number,
            episode_number,
            episode_title,
            root_folder,
            episode_id,
            ..
        } => {
            let filename = deterministic_filename(
                series_title,
                *season_number,
                *episode_number,
                episode_title.as_deref(),
                &ext,
            );
            (root_folder.as_str(), filename, Some(*episode_id))
        }
        PendingGrab::Movie {
            title,
            year,
            root_folder,
            ..
        } => {
            let filename = deterministic_movie_filename(title, *year, &ext);
            (root_folder.as_str(), filename, None)
        }
        // `process_pending_grabs` dispatches `SeasonPack` to
        // `import_season_pack` and never reaches this function with one.
        PendingGrab::SeasonPack { .. } => unreachable!(
            "SeasonPack grabs are handled by import_season_pack before import_one is ever called"
        ),
    };

    std::fs::create_dir_all(root_folder)?;
    let dest = Path::new(root_folder).join(&filename);

    // The deterministic filename means a second release for the same
    // episode/movie collides on this exact path. `link_or_copy_file`'s copy
    // fallback renames into place, which *silently overwrites* an existing
    // file with no comparison at all — a lower-scored duplicate arriving
    // second (e.g. a 480p release importing after an already-imported
    // 1080p one, entirely possible before resolution was scored, and still
    // possible from a race between two grabs of the same episode) would
    // quietly replace the better file already in the library. Checked here
    // rather than left to the filesystem to decide by import order.
    if dest.exists() {
        let current_score: f32 = conn
            .query_row(
                "SELECT score FROM release WHERE id = ?1",
                params![grab.release_id()],
                |row| row.get::<_, Option<f32>>(0),
            )?
            .unwrap_or(0.0);
        let existing_best: Option<f32> = conn.query_row(
            "SELECT MAX(score) FROM release WHERE status = 'imported' AND id != ?1
             AND ((episode_id = ?2 AND ?2 IS NOT NULL) OR (media_item_id = ?3 AND ?2 IS NULL))",
            params![grab.release_id(), episode_id, grab.media_item_id()],
            |row| row.get(0),
        )?;
        if existing_best.is_some_and(|best| best >= current_score) {
            conn.execute(
                "UPDATE release SET status = 'upgraded' WHERE id = ?1",
                params![grab.release_id()],
            )?;
            crate::db::record_event(
                conn,
                grab.media_item_id(),
                episode_id,
                // event_history.event_type's CHECK constraint doesn't have
                // an "upgraded" value (unlike release.status, which does) —
                // reusing "failed" here rather than a schema migration for
                // one more enum value; the detail text carries the real
                // reason.
                "failed",
                &format!(
                    "skipped import: already have a file at {} scoring {:.1} or better (this release scored {current_score:.1})",
                    dest.display(),
                    existing_best.unwrap_or(0.0)
                ),
            )?;
            if remuxed {
                std::fs::remove_file(&working_path).ok();
            }
            return Ok(ImportOutcome::SkippedAlreadyHaveBetter);
        }
        // The new file scores strictly better than what's currently there —
        // remove the stale file and its tracking row before placing the
        // replacement, rather than letting `link_or_copy_file`'s copy
        // fallback silently overwrite it in place. This matters for two
        // reasons: it keeps exactly one `episode_file` row per episode (an
        // old row left behind plus the fresh `INSERT` below would otherwise
        // leave a duplicate — the same bug class found live in the
        // Dr.STONE/Battlestar Galactica rows), and it lets the primary
        // hardlink path actually succeed (`std::fs::hard_link` fails
        // outright if the destination already exists), so an upgrade is a
        // cheap hardlink swap instead of an unnecessary full copy.
        conn.execute(
            "DELETE FROM episode_file WHERE path = ?1",
            params![dest.to_string_lossy()],
        )?;
        std::fs::remove_file(&dest).ok();
    }

    let needed_bytes = std::fs::metadata(&working_path)?.len();
    if insufficient_space(Path::new(root_folder), needed_bytes)? {
        anyhow::bail!(
            "not enough free space at {root_folder} for {needed_bytes} bytes (source: {})",
            working_path.display()
        );
    }

    link_or_copy_file(&working_path, &dest)?;
    if remuxed {
        // `working_path` here is the scratch remux output, not qBittorrent's
        // original content file (which was already removed above, in the
        // remux branch, to make way for it) — nothing else reads it, so
        // unlike the plain-import case there's no seeding reason to keep it.
        std::fs::remove_file(&working_path).ok();
    }

    let size_bytes = std::fs::metadata(&dest)?.len();
    conn.execute(
        "INSERT INTO episode_file (episode_id, media_item_id, path, size_bytes, subtitle_status) VALUES (?1, ?2, ?3, ?4, 'none')",
        params![episode_id, grab.media_item_id(), dest.to_string_lossy(), size_bytes],
    )?;
    let episode_file_id = conn.last_insert_rowid();
    if let Some(episode_id) = episode_id {
        conn.execute(
            "UPDATE episode SET has_file = 1 WHERE id = ?1",
            params![episode_id],
        )?;
    }
    conn.execute(
        "UPDATE release SET status = 'imported' WHERE id = ?1",
        params![grab.release_id()],
    )?;
    crate::db::record_event(
        conn,
        grab.media_item_id(),
        episode_id,
        "imported",
        &format!("path={} remuxed={remuxed}", dest.display()),
    )?;

    // Ground-truth check on the file that actually landed, not the
    // release's claimed title text — this is what catches e.g. an
    // untagged-resolution release that turns out to really be SD. Probing
    // failure itself must never fail an otherwise-successful import.
    let quality_flagged = match ensure_probed(conn, episode_file_id, &dest) {
        Ok(_) => probe_indicates_import_problem(conn, episode_file_id)?,
        Err(e) => {
            tracing::warn!(episode_file_id, error = %e, "post-import probing failed");
            false
        }
    };
    if quality_flagged {
        crate::db::record_event(
            conn,
            grab.media_item_id(),
            episode_id,
            // event_history's event_type CHECK has no dedicated value for
            // this — reusing "failed" per the same established workaround
            // used elsewhere in this file (see the dest-collision skip case
            // above); the detail text carries the real reason.
            "failed",
            &format!(
                "quality concern flagged after import: path={}",
                dest.display()
            ),
        )?;
    }

    Ok(ImportOutcome::Imported {
        remuxed,
        quality_flagged,
    })
}

#[derive(Debug, Default)]
struct SeasonPackImportOutcome {
    episodes_imported: usize,
    episodes_already_had_better: usize,
    episodes_unmatched: usize,
    quality_flagged: usize,
}

/// Imports a season-pack/batch torrent: unlike `import_one` (which resolves
/// to exactly one destination file), a pack's `content_path` is typically a
/// directory holding one file per episode. Each file is matched to its own
/// episode independently by re-parsing *its own filename* — batch releases
/// almost always name each inner file with its own SxxExx marker even
/// though the outer torrent name doesn't (that's precisely what made it
/// unparseable as a single episode in the first place, see
/// `looks_like_season_pack`). A file that can't be matched to a tracked
/// episode is skipped, not fatal to the rest of the pack.
///
/// Deliberately does not run the mkv English-audio-default remux fix
/// `import_one` applies inline — doing that per-file here would roughly
/// double a season pack's import time. Any file that needs it is still
/// reachable afterward via `remux_backlog`, which sweeps the whole library
/// including season-pack imports.
fn import_season_pack(
    conn: &Connection,
    release_id: i64,
    media_item_id: i64,
    series_title: &str,
    season_number: u32,
    root_folder: &str,
    content_path: &Path,
) -> Result<SeasonPackImportOutcome> {
    let release_score: f32 = conn
        .query_row(
            "SELECT score FROM release WHERE id = ?1",
            params![release_id],
            |row| row.get::<_, Option<f32>>(0),
        )?
        .unwrap_or(0.0);

    let candidates = if content_path.is_file() {
        vec![content_path.to_path_buf()]
    } else {
        walk_files(content_path)?
    };
    let video_files: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| VIDEO_EXTS.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false)
        })
        .collect();
    if video_files.is_empty() {
        anyhow::bail!("no video files found under {}", content_path.display());
    }

    std::fs::create_dir_all(root_folder)?;

    let mut outcome = SeasonPackImportOutcome::default();
    for source_path in &video_files {
        let filename_only = source_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        let parsed = crate::parser::parse(filename_only);
        let Some(episode_number) = parsed.episode.or(parsed.absolute_episode) else {
            outcome.episodes_unmatched += 1;
            tracing::warn!(
                file = filename_only,
                "season pack: could not determine an episode number for this file, skipping"
            );
            continue;
        };
        // Prefer the individual file's own season marker when it has one
        // (a pack can occasionally mix seasons); fall back to the pack's
        // own season otherwise.
        let file_season = parsed.season.unwrap_or(season_number);

        let episode_id = match crate::scheduler::find_episode_id(
            conn,
            media_item_id,
            file_season,
            episode_number,
        )? {
            Some(id) => id,
            None => {
                outcome.episodes_unmatched += 1;
                tracing::warn!(
                    file = filename_only,
                    season = file_season,
                    episode = episode_number,
                    "season pack: no tracked episode matches this file, skipping"
                );
                continue;
            }
        };

        match import_season_pack_file(
            conn,
            release_id,
            release_score,
            media_item_id,
            episode_id,
            series_title,
            file_season,
            episode_number,
            root_folder,
            source_path,
        ) {
            Ok(PackFileOutcome::Imported { quality_flagged }) => {
                outcome.episodes_imported += 1;
                if quality_flagged {
                    outcome.quality_flagged += 1;
                }
            }
            Ok(PackFileOutcome::SkippedAlreadyHaveBetter) => {
                outcome.episodes_already_had_better += 1;
            }
            Err(e) => {
                outcome.episodes_unmatched += 1;
                tracing::warn!(
                    file = filename_only,
                    error = %e,
                    "season pack: failed to import this file, continuing with the rest of the pack"
                );
            }
        }
    }

    if outcome.episodes_imported > 0 {
        conn.execute(
            "UPDATE release SET status = 'imported' WHERE id = ?1",
            params![release_id],
        )?;
        crate::db::record_event(
            conn,
            media_item_id,
            None,
            "imported",
            &format!(
                "season pack S{season_number:02}: {} imported, {} already had a better file, {} unmatched",
                outcome.episodes_imported, outcome.episodes_already_had_better, outcome.episodes_unmatched
            ),
        )?;
    } else if outcome.episodes_already_had_better > 0 {
        // Mirrors import_one's single-file "SkippedAlreadyHaveBetter" ->
        // 'upgraded' handling: nothing new landed, but that's because the
        // whole pack was a no-op upgrade attempt, not a failure.
        conn.execute(
            "UPDATE release SET status = 'upgraded' WHERE id = ?1",
            params![release_id],
        )?;
    } else {
        // Every file was unmatched and nothing was already-better either —
        // genuinely wrong (mismatched season, mislabeled pack). Propagating
        // this as an error routes it through the same retry/escalate-to-
        // failed machinery `process_pending_grabs` already applies to a
        // persistently failing single-file import.
        anyhow::bail!(
            "season pack import matched none of {} video file(s) to a tracked episode",
            video_files.len()
        );
    }

    Ok(outcome)
}

enum PackFileOutcome {
    Imported { quality_flagged: bool },
    SkippedAlreadyHaveBetter,
}

/// One file's worth of the season-pack import: the same dest-collision
/// scoring, free-space check, hardlink-or-copy, `episode_file` bookkeeping,
/// and post-import probe that `import_one` does for a single-episode grab,
/// scoped to one already-identified `episode_id` within a larger pack.
#[allow(clippy::too_many_arguments)]
fn import_season_pack_file(
    conn: &Connection,
    release_id: i64,
    release_score: f32,
    media_item_id: i64,
    episode_id: i64,
    series_title: &str,
    season_number: u32,
    episode_number: u32,
    root_folder: &str,
    source_path: &Path,
) -> Result<PackFileOutcome> {
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv")
        .to_string();
    let episode_title: Option<String> = conn.query_row(
        "SELECT title FROM episode WHERE id = ?1",
        params![episode_id],
        |row| row.get(0),
    )?;
    let filename = deterministic_filename(
        series_title,
        season_number,
        episode_number,
        episode_title.as_deref(),
        &ext,
    );
    let dest = Path::new(root_folder).join(&filename);

    // Same reasoning as import_one's own dest-collision check: a second
    // release for the same episode (here, from a *different* pack or a
    // single-episode grab) must not silently overwrite a better file
    // already in place.
    if dest.exists() {
        let existing_best: Option<f32> = conn.query_row(
            "SELECT MAX(score) FROM release WHERE status = 'imported' AND id != ?1 AND episode_id = ?2",
            params![release_id, episode_id],
            |row| row.get(0),
        )?;
        if existing_best.is_some_and(|best| best >= release_score) {
            return Ok(PackFileOutcome::SkippedAlreadyHaveBetter);
        }
        // This episode's file is being upgraded — clear the stale row and
        // file first so the hardlink below is a real hardlink swap rather
        // than a copy-fallback overwrite, and so no duplicate episode_file
        // row survives. See import_one's matching comment for the fuller
        // reasoning.
        conn.execute(
            "DELETE FROM episode_file WHERE path = ?1",
            params![dest.to_string_lossy()],
        )?;
        std::fs::remove_file(&dest).ok();
    }

    let needed_bytes = std::fs::metadata(source_path)?.len();
    if insufficient_space(Path::new(root_folder), needed_bytes)? {
        anyhow::bail!(
            "not enough free space at {root_folder} for {needed_bytes} bytes (source: {})",
            source_path.display()
        );
    }

    link_or_copy_file(source_path, &dest)?;

    let size_bytes = std::fs::metadata(&dest)?.len();
    conn.execute(
        "INSERT INTO episode_file (episode_id, media_item_id, path, size_bytes, subtitle_status) VALUES (?1, ?2, ?3, ?4, 'none')",
        params![episode_id, media_item_id, dest.to_string_lossy(), size_bytes],
    )?;
    let episode_file_id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE episode SET has_file = 1 WHERE id = ?1",
        params![episode_id],
    )?;

    let quality_flagged = match ensure_probed(conn, episode_file_id, &dest) {
        Ok(_) => probe_indicates_import_problem(conn, episode_file_id)?,
        Err(e) => {
            tracing::warn!(episode_file_id, error = %e, "post-import probing failed");
            false
        }
    };

    Ok(PackFileOutcome::Imported { quality_flagged })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates a tiny real video via ffmpeg's `lavfi` synthetic source —
    /// gives `ensure_probed`/`import_one` a real file to run actual ffprobe
    /// against, rather than only exercising the DB/flag-computation logic
    /// against hand-built fixtures.
    fn generate_test_clip(dir: &Path, width: u32, height: u32) -> PathBuf {
        let path = dir.join("clip.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={width}x{height}:duration=1:rate=1"))
            .args(["-c:v", "libx264"])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a test clip");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate test clip: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    #[test]
    fn ensure_probed_flags_a_low_resolution_file_as_under_quality() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, '/tmp/placeholder.mkv', 4, 'none')",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-ensure-probed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, 640, 360);

        let probed = ensure_probed(&conn, 1, &clip).unwrap();
        assert!(probed, "first probe of a file should always run");

        let (height, under_quality, status): (Option<i64>, i64, String) = conn
            .query_row(
                "SELECT height, flag_under_quality, corruption_status FROM media_file_probe WHERE episode_file_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(height, Some(360));
        assert_eq!(under_quality, 1);
        assert_eq!(status, "probe_ok");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A wider-than-16:9 master (2.00:1 is common for prestige/streaming
    /// shows — verified live against a real "For All Mankind" episode at
    /// exactly this resolution) has full 1920px width but under-1080
    /// height purely from the aspect ratio, not from being a worse encode.
    /// A height-only check flagged 49 real episodes of one show as
    /// under-quality before this was caught — this file must NOT be
    /// flagged, since its width clears the 1080p-class bar even though its
    /// height doesn't.
    #[test]
    fn ensure_probed_does_not_flag_a_wide_aspect_ratio_file_as_under_quality() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, '/tmp/placeholder.mkv', 4, 'none')",
            [],
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "breadarr-ensure-probed-wide-aspect-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, 1920, 960);

        ensure_probed(&conn, 1, &clip).unwrap();

        let (width, height, under_quality): (Option<i64>, Option<i64>, i64) = conn
            .query_row(
                "SELECT width, height, flag_under_quality FROM media_file_probe WHERE episode_file_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(width, Some(1920));
        assert_eq!(height, Some(960));
        assert_eq!(under_quality, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ensure_probed_skips_reprobing_an_unchanged_file() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, '/tmp/placeholder.mkv', 4, 'none')",
            [],
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "breadarr-ensure-probed-skip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, 640, 360);

        assert!(ensure_probed(&conn, 1, &clip).unwrap());
        assert!(
            !ensure_probed(&conn, 1, &clip).unwrap(),
            "second probe against the exact same file content should be a no-op"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_library_reports_probed_vs_skipped_up_to_date() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-probe-library-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, 640, 360);
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, ?1, 4, 'none')",
            params![clip.to_string_lossy()],
        )
        .unwrap();

        let first = probe_library(&conn).unwrap();
        assert_eq!(first.probed, 1);
        assert_eq!(first.skipped_up_to_date, 0);

        let second = probe_library(&conn).unwrap();
        assert_eq!(second.probed, 0);
        assert_eq!(second.skipped_up_to_date, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_library_confirms_a_genuinely_decodable_file() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, '/tmp/placeholder.mkv', 4, 'none')",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-verify-library-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, 640, 360);
        conn.execute(
            "UPDATE episode_file SET path = ?1 WHERE id = 1",
            params![clip.to_string_lossy()],
        )
        .unwrap();
        // `ensure_probed` leaves a freshly-probed, never-decode-checked file
        // at exactly the `corruption_status = 'probe_ok'` state
        // `verify_library` targets.
        ensure_probed(&conn, 1, &clip).unwrap();

        let report = verify_library(&conn).unwrap();
        assert_eq!(report.verified_ok, 1);
        assert_eq!(report.corrupt, 0);
        assert_eq!(report.errors, 0);

        let status: String = conn
            .query_row(
                "SELECT corruption_status FROM media_file_probe WHERE episode_file_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "decode_ok");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_library_flags_a_file_that_parses_but_does_not_decode() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "breadarr-verify-library-bad-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // A file with an .mkv extension but garbage content: `ffprobe`'s
        // cheap header check may or may not accept it, but the point here
        // is simulating a row that's already `probe_ok` (however it got
        // there) and confirming the full decode check catches what the
        // header probe missed.
        let bad_path = dir.join("bad.mkv");
        std::fs::write(&bad_path, b"not a real matroska file").unwrap();

        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, ?1, 4, 'none')",
            params![bad_path.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime, corruption_status)
             VALUES (1, datetime('now'), 4, 0, 'probe_ok')",
            [],
        )
        .unwrap();

        let report = verify_library(&conn).unwrap();
        assert_eq!(report.verified_ok, 0);
        assert_eq!(report.corrupt, 1);
        assert_eq!(report.errors, 0);

        let status: String = conn
            .query_row(
                "SELECT corruption_status FROM media_file_probe WHERE episode_file_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "decode_failed");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_library_skips_files_that_never_even_passed_the_header_probe() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, '/tmp/nonexistent.mkv', 4, 'none')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime, corruption_status)
             VALUES (1, datetime('now'), 4, 0, 'probe_failed')",
            [],
        )
        .unwrap();

        let report = verify_library(&conn).unwrap();
        assert_eq!(report.verified_ok, 0);
        assert_eq!(report.corrupt, 0);
        assert_eq!(report.errors, 0);
    }

    /// Seeds a movie release with `grabbed_at` shifted `hours_ago` into the
    /// past, so stall/grace-period thresholds can be tested deterministically
    /// instead of depending on real wall-clock time passing.
    fn seeded_release_conn(hours_ago: f64) -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Some Movie 2016', 1, 'guid-1', 'grabbed', 'deadbeef', datetime('now', ?1))",
            params![format!("-{hours_ago} hours")],
        )
        .unwrap();
        (conn, 1)
    }

    /// A dedicated root_folder per row (rather than one shared dir with many
    /// rows in it) so `RECONCILE_MIN_CLEAR_FLOOR` doesn't mask the specific
    /// count each of these small tests is asserting on.
    fn media_item_with_root(conn: &Connection, id: i64, root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (?1, 'series', 'Some Show', 2020, 1, 1, ?2)",
            params![id, root.to_string_lossy()],
        )
        .unwrap();
    }

    #[test]
    fn reconcile_clears_episode_file_rows_whose_path_no_longer_exists() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let dir = std::env::temp_dir().join(format!("breadarr-reconcile-{}", std::process::id()));
        media_item_with_root(&conn, 1, &dir);
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 1, 1, 1, 1)",
            [],
        )
        .unwrap();

        let existing = dir.join("exists.mkv");
        std::fs::write(&existing, b"data").unwrap();
        let missing = dir.join("gone.mkv"); // deliberately never created, and
                                            // no same-named file anywhere under `dir` either

        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, 1, 1, ?1, 4, 'none')",
            params![existing.to_string_lossy()],
        )
        .unwrap();
        // episode_id NULL simulates a movie's file going missing.
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (2, NULL, 1, ?1, 4, 'none')",
            params![missing.to_string_lossy()],
        )
        .unwrap();

        let outcome = reconcile_missing_files(&conn, false).unwrap();
        assert_eq!(outcome.cleared, 1);
        assert_eq!(outcome.repaired, 0);
        assert!(!outcome.aborted);

        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM episode_file", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the still-existing file survives");

        let has_file: i64 = conn
            .query_row("SELECT has_file FROM episode WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(has_file, 1, "untouched — its own file still exists");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reconcile_repairs_a_path_whose_show_folder_was_renamed_instead_of_clearing_it() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let root =
            std::env::temp_dir().join(format!("breadarr-reconcile-repair-{}", std::process::id()));
        // Simulates the folder-normalization case: `root_folder` already
        // reflects the renamed-to-include-year directory, but the stored
        // `episode_file.path` still points at the old (pre-rename) name.
        let renamed_show_dir = root.join("Black Adder (1983)").join("Season 1");
        std::fs::create_dir_all(&renamed_show_dir).unwrap();
        let real_file = renamed_show_dir.join("Blackadder - S01E04.mkv");
        std::fs::write(&real_file, b"data").unwrap();

        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Black Adder', 1983, 1, 1, ?1)",
            params![root.join("Black Adder (1983)").to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 1, 4, 1, 1)",
            [],
        )
        .unwrap();
        let stale_path = root
            .join("Black Adder") // missing the "(1983)" suffix
            .join("Season 1")
            .join("Blackadder - S01E04.mkv");
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, 1, NULL, ?1, 4, 'none')",
            params![stale_path.to_string_lossy()],
        )
        .unwrap();

        let outcome = reconcile_missing_files(&conn, false).unwrap();
        assert_eq!(outcome.repaired, 1);
        assert_eq!(outcome.cleared, 0);
        assert!(!outcome.aborted);

        let new_path: String = conn
            .query_row("SELECT path FROM episode_file WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(new_path, real_file.to_string_lossy());

        let has_file: i64 = conn
            .query_row("SELECT has_file FROM episode WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(has_file, 1, "repaired, not cleared — file genuinely exists");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reconcile_repairs_a_path_whose_extension_changed_from_a_transcode() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let root = std::env::temp_dir().join(format!(
            "breadarr-reconcile-transcode-{}",
            std::process::id()
        ));
        let movie_dir = root.join("Upgrade (2018)");
        std::fs::create_dir_all(&movie_dir).unwrap();
        // Simulates Tdarr re-encoding in place: the original .mp4 is gone,
        // replaced by a .mkv with the same stem.
        let real_file = movie_dir.join("Upgrade (2018).mkv");
        std::fs::write(&real_file, b"data").unwrap();

        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Upgrade', 2018, 1, 1, ?1)",
            params![movie_dir.to_string_lossy()],
        )
        .unwrap();
        let stale_path = movie_dir.join("Upgrade (2018).mp4");
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, ?1, 4, 'none')",
            params![stale_path.to_string_lossy()],
        )
        .unwrap();

        let outcome = reconcile_missing_files(&conn, false).unwrap();
        assert_eq!(outcome.repaired, 1);
        assert_eq!(outcome.cleared, 0);
        assert!(!outcome.aborted);

        let new_path: String = conn
            .query_row("SELECT path FROM episode_file WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(new_path, real_file.to_string_lossy());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reconcile_refuses_to_clear_an_anomalous_fraction_in_one_pass() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let root =
            std::env::temp_dir().join(format!("breadarr-reconcile-breaker-{}", std::process::id()));
        media_item_with_root(&conn, 1, &root);

        // 20 rows, all genuinely missing (no matching basename anywhere) —
        // comfortably past both the 10% fraction and the floor of 10.
        for i in 0..20 {
            conn.execute(
                "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file)
                 VALUES (?1, 1, 1, ?1, 1, 1)",
                params![i],
            )
            .unwrap();
            let path = root.join(format!("gone-{i}.mkv"));
            conn.execute(
                "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
                 VALUES (?1, ?1, NULL, ?2, 4, 'none')",
                params![i, path.to_string_lossy()],
            )
            .unwrap();
        }

        let outcome = reconcile_missing_files(&conn, false).unwrap();
        assert!(outcome.aborted);
        assert_eq!(outcome.cleared, 0);
        assert_eq!(outcome.repaired, 0);

        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM episode_file", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 20, "nothing cleared once the breaker trips");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reconcile_dry_run_reports_without_writing_anything() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let dir =
            std::env::temp_dir().join(format!("breadarr-reconcile-dryrun-{}", std::process::id()));
        media_item_with_root(&conn, 1, &dir);
        let missing = dir.join("gone.mkv");
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, ?1, 4, 'none')",
            params![missing.to_string_lossy()],
        )
        .unwrap();

        let outcome = reconcile_missing_files(&conn, true).unwrap();
        assert_eq!(outcome.cleared, 1);

        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM episode_file", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "dry run must not actually delete the row");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fresh_grab_is_not_stalled() {
        let (conn, release_id) = seeded_release_conn(1.0);
        assert!(!grab_is_stalled(&conn, release_id).unwrap());
    }

    #[test]
    fn a_grab_untouched_past_the_threshold_is_stalled() {
        let (conn, release_id) = seeded_release_conn(STALL_THRESHOLD_HOURS + 1.0);
        assert!(grab_is_stalled(&conn, release_id).unwrap());
    }

    #[test]
    fn progress_advancing_resets_the_stall_clock() {
        let (conn, release_id) = seeded_release_conn(STALL_THRESHOLD_HOURS + 1.0);
        // grabbed_at is old, but progress was just observed advancing, so
        // last_progress_at (which the stall check prefers) is fresh.
        update_grab_progress(&conn, release_id, 0.5).unwrap();
        assert!(!grab_is_stalled(&conn, release_id).unwrap());
    }

    #[test]
    fn update_grab_progress_ignores_a_non_increasing_value() {
        let (conn, release_id) = seeded_release_conn(0.0);
        update_grab_progress(&conn, release_id, 0.5).unwrap();
        let first_seen: String = conn
            .query_row(
                "SELECT last_progress_at FROM release WHERE id = ?1",
                params![release_id],
                |r| r.get(0),
            )
            .unwrap();
        // Same progress again shouldn't touch the watermark timestamp.
        update_grab_progress(&conn, release_id, 0.5).unwrap();
        let second_seen: String = conn
            .query_row(
                "SELECT last_progress_at FROM release WHERE id = ?1",
                params![release_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first_seen, second_seen);
    }

    #[test]
    fn a_torrent_missing_within_the_grace_period_is_not_yet_failed() {
        let (conn, release_id) = seeded_release_conn(0.0);
        assert!(!grab_missing_past_grace(&conn, release_id).unwrap());
    }

    #[test]
    fn a_torrent_missing_past_the_grace_period_is_failed() {
        let (conn, release_id) = seeded_release_conn(MISSING_GRACE_MINUTES / 60.0 + 1.0);
        assert!(grab_missing_past_grace(&conn, release_id).unwrap());
    }

    #[test]
    fn fail_grab_reopens_the_release_for_search() {
        let (conn, release_id) = seeded_release_conn(0.0);
        let grab = PendingGrab::Movie {
            release_id,
            media_item_id: 1,
            torrent_hash: "deadbeef".to_string(),
            title: "Some Movie".to_string(),
            year: Some(2016),
            root_folder: "/tmp".to_string(),
        };
        fail_grab(&conn, &grab, "test").unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM release WHERE id = ?1",
                params![release_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");

        let (event_type, detail): (String, String) = conn
            .query_row(
                "SELECT event_type, detail FROM event_history WHERE media_item_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event_type, "failed");
        assert_eq!(detail, "test");
    }

    #[test]
    fn builds_filename_with_episode_title() {
        assert_eq!(
            deterministic_filename("Some Show", 4, 13, Some("Winter Gathering"), "mkv"),
            "Some Show - S04E13 - Winter Gathering.mkv"
        );
    }

    #[test]
    fn builds_filename_without_episode_title() {
        assert_eq!(
            deterministic_filename("Some Show", 1, 1, None, "mkv"),
            "Some Show - S01E01.mkv"
        );
    }

    #[test]
    fn sanitizes_path_hostile_characters() {
        assert_eq!(sanitize("Kill: Ao / Blue?"), "Kill_ Ao _ Blue_");
    }

    #[test]
    fn nearest_existing_ancestor_returns_the_path_itself_when_it_exists() {
        let dir = std::env::temp_dir();
        assert_eq!(nearest_existing_ancestor(&dir).unwrap(), dir);
    }

    #[test]
    fn nearest_existing_ancestor_walks_up_past_nonexistent_components() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-ancestor-test-{}/does/not/exist/yet",
            std::process::id()
        ));
        let expected =
            std::env::temp_dir().join(format!("breadarr-ancestor-test-{}", std::process::id()));
        std::fs::create_dir_all(&expected).unwrap();
        assert_eq!(nearest_existing_ancestor(&dir).unwrap(), expected);
        std::fs::remove_dir_all(&expected).unwrap();
    }

    #[test]
    fn staging_dir_for_is_named_by_torrent_hash_under_the_marker_directory() {
        let dest = std::env::temp_dir();
        let staging = staging_dir_for(&dest, "deadbeef1234").unwrap();
        assert_eq!(
            staging.file_name().unwrap().to_str().unwrap(),
            "deadbeef1234"
        );
        assert_eq!(
            staging
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            STAGING_DIR_NAME
        );
    }

    #[test]
    fn insufficient_space_is_false_for_a_trivially_small_request() {
        assert!(!insufficient_space(&std::env::temp_dir(), 1).unwrap());
    }

    #[test]
    fn insufficient_space_is_true_for_an_absurd_request() {
        // No real filesystem has an exabyte free.
        assert!(insufficient_space(&std::env::temp_dir(), u64::MAX / 2).unwrap());
    }

    #[test]
    fn copy_via_temp_file_writes_through_a_part_file_and_renames_into_place() {
        let dir = std::env::temp_dir().join(format!("breadarr-copy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.mkv");
        std::fs::write(&src, b"fake video data").unwrap();
        let dest = dir.join("dest.mkv");

        copy_via_temp_file(&src, &dest).unwrap();

        assert!(dest.exists());
        assert!(
            !dir.join("dest.mkv.part").exists(),
            "the .part file should be renamed away, not left behind"
        );
        assert!(src.exists(), "the copy fallback should also preserve src");
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dest).unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn link_or_copy_file_leaves_the_source_in_place() {
        let dir = std::env::temp_dir().join(format!("breadarr-link-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.mkv");
        std::fs::write(&src, b"fake video data").unwrap();
        let dest = dir.join("dest.mkv");

        link_or_copy_file(&src, &dest).unwrap();

        assert!(src.exists(), "source should survive a hardlink-or-copy");
        assert!(dest.exists());
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dest).unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn locates_a_single_file_torrent() {
        let dir = std::env::temp_dir().join(format!("breadarr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("episode.mkv");
        std::fs::write(&file, b"fake video data").unwrap();

        let found = locate_video_file(&file).unwrap();
        assert_eq!(found, file);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remap_path_translates_container_prefix_to_host_prefix() {
        assert_eq!(
            remap_path(
                "/downloads/Big Buck Bunny",
                "/downloads",
                "/home/breadway/downloads"
            ),
            PathBuf::from("/home/breadway/downloads/Big Buck Bunny")
        );
    }

    #[test]
    fn remap_path_is_noop_when_prefixes_not_configured() {
        assert_eq!(
            remap_path("/downloads/Big Buck Bunny", "", ""),
            PathBuf::from("/downloads/Big Buck Bunny")
        );
    }

    #[test]
    fn process_pending_grabs_routes_each_grab_by_torrent_state() {
        use crate::qbit::TorrentInfo;

        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-process-grabs-{}", std::process::id()));
        let dest_root = dir.join("library");
        std::fs::create_dir_all(&dest_root).unwrap();

        // Grab 1: complete, should import.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Complete Movie', 2020, 1, 1, ?1)",
            params![dest_root.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Complete Movie 2020', 1, 'guid-1', 'grabbed', 'hash-complete', datetime('now'))",
            [],
        )
        .unwrap();
        let complete_content = dir.join("complete-content");
        std::fs::create_dir_all(&complete_content).unwrap();
        std::fs::write(complete_content.join("movie.mp4"), b"data").unwrap();

        // Grab 2: still downloading, should be skipped as incomplete.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (2, 'movie', 'Downloading Movie', 2020, 1, 1, ?1)",
            params![dest_root.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (2, 2, NULL, 'Downloading Movie 2020', 1, 'guid-2', 'grabbed', 'hash-downloading', datetime('now'))",
            [],
        )
        .unwrap();

        // Grab 3: torrent hash absent from qBit's list, but well within the
        // grace period — should not yet be failed.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (3, 'movie', 'Fresh Grab', 2020, 1, 1, ?1)",
            params![dest_root.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (3, 3, NULL, 'Fresh Grab 2020', 1, 'guid-3', 'grabbed', 'hash-missing-fresh', datetime('now'))",
            [],
        )
        .unwrap();

        // Grab 4: torrent hash absent from qBit's list, grabbed long ago —
        // should be marked failed.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (4, 'movie', 'Abandoned Grab', 2020, 1, 1, ?1)",
            params![dest_root.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (4, 4, NULL, 'Abandoned Grab 2020', 1, 'guid-4', 'grabbed', 'hash-missing-stale', datetime('now', '-1 hour'))",
            [],
        )
        .unwrap();

        let pending = fetch_pending_grabs(&conn).unwrap();
        assert_eq!(pending.len(), 4);

        let torrents = vec![
            TorrentInfo {
                hash: "hash-complete".to_string(),
                name: "complete".to_string(),
                state: "uploading".to_string(),
                progress: 1.0,
                save_path: dir.to_string_lossy().to_string(),
                content_path: complete_content.to_string_lossy().to_string(),
            },
            TorrentInfo {
                hash: "hash-downloading".to_string(),
                name: "downloading".to_string(),
                state: "downloading".to_string(),
                progress: 0.4,
                save_path: dir.to_string_lossy().to_string(),
                content_path: dir
                    .join("downloading-content")
                    .to_string_lossy()
                    .to_string(),
            },
            // hash-missing-fresh and hash-missing-stale are deliberately
            // absent — simulating torrents qBit no longer knows about.
        ];

        let stats = process_pending_grabs(&conn, &pending, &torrents, "", "").unwrap();

        assert_eq!(stats.imported, 1);
        assert_eq!(stats.skipped_incomplete, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.errors, 0);

        let statuses: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, status FROM release ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(statuses[0], (1, "imported".to_string()));
        assert_eq!(statuses[1], (2, "grabbed".to_string()));
        assert_eq!(statuses[2], (3, "grabbed".to_string()));
        assert_eq!(statuses[3], (4, "failed".to_string()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn does_not_import_a_torrent_while_it_is_still_being_physically_moved() {
        use crate::qbit::TorrentInfo;

        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Moving Movie', 2020, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Moving Movie 2020', 1, 'guid-1', 'grabbed', 'hash-moving', datetime('now'))",
            [],
        )
        .unwrap();

        let pending = fetch_pending_grabs(&conn).unwrap();
        let torrents = vec![TorrentInfo {
            hash: "hash-moving".to_string(),
            name: "moving".to_string(),
            // qBittorrent's own state for "setLocation relocation (or a
            // manual move) still physically in flight" — content_path may
            // already point at the destination while the bytes aren't
            // fully there yet.
            state: "moving".to_string(),
            progress: 1.0,
            save_path: "/tmp".to_string(),
            content_path: "/tmp/somewhere-mid-move".to_string(),
        }];

        let stats = process_pending_grabs(&conn, &pending, &torrents, "", "").unwrap();
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.skipped_incomplete, 1);
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.failed, 0);

        let status: String = conn
            .query_row("SELECT status FROM release WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "grabbed");
    }

    #[test]
    fn a_persistently_failing_import_escalates_to_failed_instead_of_retrying_forever() {
        use crate::qbit::TorrentInfo;

        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Broken Movie', 2020, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Broken Movie 2020', 1, 'guid-1', 'grabbed', 'hash-broken', datetime('now'))",
            [],
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "breadarr-persistent-import-error-{}",
            std::process::id()
        ));
        // Deliberately empty — `locate_video_file` finds no video file
        // here, so `import_one` fails deterministically every time,
        // simulating a persistent real-world error (bad path remap,
        // unreadable file) without needing to fake one out.
        std::fs::create_dir_all(&dir).unwrap();

        let torrents = vec![TorrentInfo {
            hash: "hash-broken".to_string(),
            name: "broken".to_string(),
            state: "uploading".to_string(),
            progress: 1.0,
            save_path: dir.to_string_lossy().to_string(),
            content_path: dir.to_string_lossy().to_string(),
        }];

        for i in 1..MAX_IMPORT_ERRORS {
            let pending = fetch_pending_grabs(&conn).unwrap();
            let stats = process_pending_grabs(&conn, &pending, &torrents, "", "").unwrap();
            assert_eq!(stats.errors, 1, "iteration {i}");
            assert_eq!(stats.failed, 0, "iteration {i}");
            let status: String = conn
                .query_row("SELECT status FROM release WHERE id = 1", [], |r| r.get(0))
                .unwrap();
            assert_eq!(status, "grabbed", "iteration {i}");
        }

        // The Nth failure crosses the threshold and gives up.
        let pending = fetch_pending_grabs(&conn).unwrap();
        let stats = process_pending_grabs(&conn, &pending, &torrents, "", "").unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.errors, 0);
        let status: String = conn
            .query_row("SELECT status FROM release WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "failed");

        // Failed releases are excluded from `fetch_pending_grabs`, so it
        // stops being retried at all.
        assert!(fetch_pending_grabs(&conn).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn imports_a_movie_release_with_no_episode_id() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Some Movie 2016 1080p', 1, 'guid-1', 'grabbed', 'deadbeef', datetime('now'))",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-movie-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest_root = dir.join("library");
        std::fs::create_dir_all(&dest_root).unwrap();
        let content = dir.join("Some.Movie.2016.1080p.mp4");
        std::fs::write(&content, b"fake movie data").unwrap();

        let grab = PendingGrab::Movie {
            release_id: 1,
            media_item_id: 1,
            torrent_hash: "deadbeef".to_string(),
            title: "Some Movie".to_string(),
            year: Some(2016),
            root_folder: dest_root.to_string_lossy().to_string(),
        };

        let outcome = import_one(&conn, &grab, &content).unwrap();
        let ImportOutcome::Imported { remuxed, .. } = outcome else {
            panic!("expected a real import, got a skip");
        };
        assert!(!remuxed);

        let dest = dest_root.join("Some Movie (2016).mp4");
        assert!(dest.exists(), "expected {} to exist", dest.display());
        assert!(
            content.exists(),
            "source file should survive import so qBittorrent can keep seeding"
        );

        let status: String = conn
            .query_row("SELECT status FROM release WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "imported");

        let (episode_id, media_item_id): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT episode_id, media_item_id FROM episode_file WHERE path = ?1",
                params![dest.to_string_lossy()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(episode_id, None);
        assert_eq!(media_item_id, Some(1));

        let event_type: String = conn
            .query_row(
                "SELECT event_type FROM event_history WHERE media_item_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(event_type, "imported");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_one_flags_quality_when_the_real_file_is_under_1080p() {
        // Mirrors a real production case: a release whose title carries no
        // resolution tag at all turned out, once ffprobed, to actually be
        // SD — this is the exact gap the post-import ground-truth check
        // closes (the pipeline previously only ever trusted claimed title
        // text, never the real downloaded bytes).
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Some.Movie.WEB.H264-GROUP', 1, 'guid-1', 'grabbed', 'deadbeef', datetime('now'))",
            [],
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "breadarr-quality-flag-import-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dest_root = dir.join("library");
        std::fs::create_dir_all(&dest_root).unwrap();
        let generated = generate_test_clip(&dir, 640, 360);
        let content = dir.join("Some.Movie.WEB.H264-GROUP.mkv");
        std::fs::rename(&generated, &content).unwrap();

        let grab = PendingGrab::Movie {
            release_id: 1,
            media_item_id: 1,
            torrent_hash: "deadbeef".to_string(),
            title: "Some Movie".to_string(),
            year: Some(2016),
            root_folder: dest_root.to_string_lossy().to_string(),
        };

        let outcome = import_one(&conn, &grab, &content).unwrap();
        let ImportOutcome::Imported {
            quality_flagged, ..
        } = outcome
        else {
            panic!("expected a real import, got a skip");
        };
        assert!(
            quality_flagged,
            "a genuinely sub-1080p file should be flagged even with an untagged title"
        );

        let flagged: i64 = conn
            .query_row(
                "SELECT flag_under_quality FROM media_file_probe WHERE episode_file_id = (SELECT id FROM episode_file LIMIT 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flagged, 1);

        let quality_events: i64 = conn
            .query_row(
                "SELECT count(*) FROM event_history WHERE media_item_id = 1 AND detail LIKE 'quality concern%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(quality_events, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Generates a real mkv with two audio tracks — a non-English one
    /// flagged as default (track order 0), and English not-default (track
    /// order 1) — so `remux_backlog` has a genuine "Italian track 1,
    /// English track 2" case to fix, not just a hand-built fixture.
    fn generate_dual_audio_clip(dir: &Path) -> PathBuf {
        let path = dir.join("dual-audio.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1920x1080:duration=1:rate=1",
            ])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-f", "lavfi", "-i", "sine=frequency=880:duration=1"])
            .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
            .args([
                "-metadata:s:a:0",
                "language=ita",
                "-disposition:a:0",
                "default",
            ])
            .args(["-metadata:s:a:1", "language=eng", "-disposition:a:1", "0"])
            .args(["-c:v", "libx264", "-c:a", "aac"])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a dual-audio test clip");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate dual-audio clip: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    #[test]
    fn remux_backlog_promotes_english_to_default_for_a_flagged_file() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-remux-backlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_dual_audio_clip(&dir);

        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, ?1, 4, 'none')",
            params![clip.to_string_lossy()],
        )
        .unwrap();
        // Probe it first so the flag actually gets set from real ffprobe
        // output, exactly like it would in the running daemon.
        ensure_probed(&conn, 1, &clip).unwrap();
        let flagged_before: i64 = conn
            .query_row(
                "SELECT flag_non_english_default_audio FROM media_file_probe WHERE episode_file_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flagged_before, 1, "fixture clip should start flagged");

        let report = remux_backlog(&conn).unwrap();
        assert_eq!(report.remuxed, 1);
        assert_eq!(report.errors, 0);

        let tracks = mkv::inspect_audio_tracks(&clip).unwrap();
        assert!(
            mkv::default_track_is_english_or_unset(&tracks),
            "the remuxed file's default audio track should now be English"
        );

        let flagged_after: i64 = conn
            .query_row(
                "SELECT flag_non_english_default_audio FROM media_file_probe WHERE episode_file_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            flagged_after, 0,
            "re-probe after remux should clear the flag"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remux_backlog_skips_non_mkv_files() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, '/tmp/some-movie.mp4', 4, 'none')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime,
                corruption_status, flag_under_quality, flag_no_subtitles, flag_no_english_audio, flag_non_english_default_audio)
             VALUES (1, datetime('now'), 4, 0, 'probe_ok', 0, 0, 0, 1)",
            [],
        )
        .unwrap();

        let report = remux_backlog(&conn).unwrap();
        assert_eq!(report.remuxed, 0);
        assert_eq!(report.skipped_not_mkv, 1);
    }

    #[test]
    fn refuses_to_overwrite_an_already_imported_higher_scoring_file() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        // The winner: already imported, high score.
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, score, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Some Movie 2016 1080p', 1, 'guid-1', 20.0, 'imported', 'aaaa', datetime('now'))",
            [],
        )
        .unwrap();
        // The loser: a second, lower-scoring release for the same movie,
        // now sitting completed in qBittorrent and up for import.
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, score, status, torrent_hash, grabbed_at)
             VALUES (2, 1, NULL, 'Some Movie 2016 480p', 1, 'guid-2', 5.0, 'grabbed', 'bbbb', datetime('now'))",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-collision-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest_root = dir.join("library");
        std::fs::create_dir_all(&dest_root).unwrap();

        // The winner's file already sits at the deterministic destination.
        let dest = dest_root.join("Some Movie (2016).mp4");
        std::fs::write(&dest, b"the better file, already imported").unwrap();

        let content = dir.join("Some.Movie.2016.480p.mp4");
        std::fs::write(&content, b"a worse duplicate arriving late").unwrap();

        let grab = PendingGrab::Movie {
            release_id: 2,
            media_item_id: 1,
            torrent_hash: "bbbb".to_string(),
            title: "Some Movie".to_string(),
            year: Some(2016),
            root_folder: dest_root.to_string_lossy().to_string(),
        };

        let outcome = import_one(&conn, &grab, &content).unwrap();
        assert!(matches!(outcome, ImportOutcome::SkippedAlreadyHaveBetter));

        // The existing better file must be untouched, not overwritten.
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"the better file, already imported"
        );

        let status: String = conn
            .query_row("SELECT status FROM release WHERE id = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "upgraded");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_strictly_better_release_replaces_the_existing_file_via_a_real_hardlink_swap() {
        use std::os::unix::fs::MetadataExt;

        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        // The old, worse release: already imported, low score.
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, score, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 'Some Movie 2016 480p', 1, 'guid-1', 5.0, 'imported', 'aaaa', datetime('now'))",
            [],
        )
        .unwrap();
        // The new, better release: sitting completed in qBittorrent, up for import.
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, score, status, torrent_hash, grabbed_at)
             VALUES (2, 1, NULL, 'Some Movie 2016 1080p', 1, 'guid-2', 20.0, 'grabbed', 'bbbb', datetime('now'))",
            [],
        )
        .unwrap();

        let dir =
            std::env::temp_dir().join(format!("breadarr-upgrade-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest_root = dir.join("library");
        std::fs::create_dir_all(&dest_root).unwrap();

        // The old, worse file already sits at the deterministic destination,
        // tracked by its own episode_file row.
        let dest = dest_root.join("Some Movie (2016).mp4");
        std::fs::write(&dest, b"the old, worse file").unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, 1, ?1, 20, 'none')",
            params![dest.to_string_lossy()],
        )
        .unwrap();

        let content = dir.join("Some.Movie.2016.1080p.mp4");
        std::fs::write(&content, b"the new, better file").unwrap();

        let grab = PendingGrab::Movie {
            release_id: 2,
            media_item_id: 1,
            torrent_hash: "bbbb".to_string(),
            title: "Some Movie".to_string(),
            year: Some(2016),
            root_folder: dest_root.to_string_lossy().to_string(),
        };

        let outcome = import_one(&conn, &grab, &content).unwrap();
        assert!(matches!(outcome, ImportOutcome::Imported { .. }));

        // The new file's content landed at the shared deterministic path.
        assert_eq!(std::fs::read(&dest).unwrap(), b"the new, better file");
        // It's a genuine hardlink to the source (same inode), not a copy —
        // confirms the old file/row was cleared first so `hard_link`
        // itself succeeded instead of falling back to `copy_via_temp_file`.
        assert_eq!(
            std::fs::metadata(&dest).unwrap().ino(),
            std::fs::metadata(&content).unwrap().ino()
        );

        // Exactly one episode_file row survives for this movie — the old
        // one was removed, not left behind as a duplicate alongside the new.
        let file_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM episode_file WHERE media_item_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn locates_the_largest_video_file_in_a_directory() {
        let dir = std::env::temp_dir().join(format!("breadarr-test-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sample.mkv"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("episode.mkv"), vec![0u8; 10_000]).unwrap();
        std::fs::write(dir.join("readme.txt"), b"not a video").unwrap();

        let found = locate_video_file(&dir).unwrap();
        assert_eq!(found.file_name().unwrap(), "episode.mkv");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Seeds a series with `episode_count` episodes in season 1, all
    /// monitored and missing, plus a `grabbed` season-pack release row.
    /// Returns the media_item_id (always 1) and release_id (always 1).
    fn seeded_season_pack_conn(episode_count: u32) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Some Show', 2020, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        for ep in 1..=episode_count {
            conn.execute(
                "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
                 VALUES (1, 1, ?1, 1, 0)",
                params![ep],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, season_number, raw_title, source_id, guid, score, status, torrent_hash, grabbed_at)
             VALUES (1, 1, NULL, 1, 'Some Show S01 Complete 1080p', 1, 'guid-1', 15.0, 'grabbed', 'aaaa', datetime('now'))",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn import_season_pack_imports_every_file_and_marks_episodes_owned() {
        let conn = seeded_season_pack_conn(3);
        let dir = std::env::temp_dir().join(format!("breadarr-season-pack-{}", std::process::id()));
        let pack_dir = dir.join("Some.Show.S01.1080p.WEB-DL");
        std::fs::create_dir_all(&pack_dir).unwrap();
        for ep in 1..=3u32 {
            std::fs::write(
                pack_dir.join(format!("Some.Show.S01E{ep:02}.1080p.WEB-DL.mkv")),
                format!("episode {ep} content"),
            )
            .unwrap();
        }
        let dest_root = dir.join("library");

        let outcome = import_season_pack(
            &conn,
            1,
            1,
            "Some Show",
            1,
            &dest_root.to_string_lossy(),
            &pack_dir,
        )
        .unwrap();

        assert_eq!(outcome.episodes_imported, 3);
        assert_eq!(outcome.episodes_already_had_better, 0);
        assert_eq!(outcome.episodes_unmatched, 0);

        let has_file_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM episode WHERE media_item_id = 1 AND has_file = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_file_count, 3);

        let status: String = conn
            .query_row("SELECT status FROM release WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "imported");

        assert!(dest_root.join("Some Show - S01E01.mkv").exists());
        assert!(dest_root.join("Some Show - S01E02.mkv").exists());
        assert!(dest_root.join("Some Show - S01E03.mkv").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_season_pack_skips_episodes_that_already_have_a_better_file() {
        let conn = seeded_season_pack_conn(2);
        let dir = std::env::temp_dir().join(format!(
            "breadarr-season-pack-partial-{}",
            std::process::id()
        ));
        let pack_dir = dir.join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(pack_dir.join("Show.S01E01.mkv"), b"new e01").unwrap();
        std::fs::write(pack_dir.join("Show.S01E02.mkv"), b"new e02").unwrap();
        let dest_root = dir.join("library");
        std::fs::create_dir_all(&dest_root).unwrap();

        // Episode 1 already has a higher-scoring imported release.
        conn.execute(
            "INSERT INTO release (id, media_item_id, episode_id, raw_title, source_id, guid, score, status, torrent_hash, grabbed_at)
             VALUES (2, 1, 1, 'Some Show S01E01 1080p REMUX', 1, 'guid-2', 50.0, 'imported', 'bbbb', datetime('now'))",
            [],
        )
        .unwrap();
        let existing = dest_root.join("Some Show - S01E01.mkv");
        std::fs::write(&existing, b"already-better e01").unwrap();
        conn.execute(
            "INSERT INTO episode_file (episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, NULL, ?1, 19, 'none')",
            params![existing.to_string_lossy()],
        )
        .unwrap();

        let outcome = import_season_pack(
            &conn,
            1,
            1,
            "Some Show",
            1,
            &dest_root.to_string_lossy(),
            &pack_dir,
        )
        .unwrap();

        assert_eq!(outcome.episodes_imported, 1); // only episode 2
        assert_eq!(outcome.episodes_already_had_better, 1); // episode 1 skipped
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            b"already-better e01",
            "episode 1's better file must survive untouched"
        );
        assert!(dest_root.join("Some Show - S01E02.mkv").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_season_pack_fails_when_nothing_in_it_matches_a_tracked_episode() {
        let conn = seeded_season_pack_conn(2);
        let dir = std::env::temp_dir().join(format!(
            "breadarr-season-pack-nomatch-{}",
            std::process::id()
        ));
        let pack_dir = dir.join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        // Wrong season entirely — nothing here matches season 1's episodes.
        std::fs::write(pack_dir.join("Show.S02E01.mkv"), b"wrong season").unwrap();
        std::fs::write(pack_dir.join("Show.S02E02.mkv"), b"wrong season").unwrap();
        let dest_root = dir.join("library");

        let result = import_season_pack(
            &conn,
            1,
            1,
            "Some Show",
            1,
            &dest_root.to_string_lossy(),
            &pack_dir,
        );
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
