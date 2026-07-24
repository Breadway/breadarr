use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use breadarr_shared::config::TranscodeConfig;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::importer::{self, ffprobe};
use crate::jellyfin::JellyfinClient;

/// Computes the target AV1 bitrate for a given resolution, scaled from the
/// configured reference (a real HEVC/H264 bitrate at `reference_height` the
/// user is already happy with) by pixel-count ratio, then discounted by
/// `av1_efficiency_factor` — AV1 reaches equivalent perceived quality to
/// HEVC/H264 at a meaningfully lower bitrate, so a like-for-like copy of the
/// reference bitrate would leave savings on the table.
pub fn target_bitrate_kbps(width: i64, height: i64, cfg: &TranscodeConfig) -> u32 {
    let reference_width = cfg.reference_height as f64 * 16.0 / 9.0;
    let reference_pixels = reference_width * cfg.reference_height as f64;
    let pixel_ratio = ((width * height) as f64 / reference_pixels).max(0.1);
    let bitrate = cfg.reference_bitrate_kbps as f64 * pixel_ratio * cfg.av1_efficiency_factor as f64;
    bitrate.round() as u32
}

/// Runs the actual GPU encode via `av1_vaapi`, decoding through VAAPI too
/// (`-hwaccel_output_format vaapi`) so the whole pipeline stays on-GPU
/// rather than round-tripping frames through the CPU. Video-only re-encode
/// — every audio/subtitle/data stream is copied verbatim (`-c:a copy -c:s
/// copy -c:d copy`), and 10-bit sources stay 10-bit (AV1 handles this
/// natively; the VAAPI driver preserves the surface format through the
/// pipeline without any extra flags needed).
///
/// Blocking and slow by design (a real GPU encode, potentially minutes per
/// file) — callers must run this inside `tokio::task::spawn_blocking`, never
/// directly on an async task, and never while holding the shared DB mutex.
///
/// Known limitation, deliberately not worked around yet: the output
/// container is always Matroska, and `-c:s copy`/`-c:d copy` stream-copy
/// whatever subtitle/data tracks the source has. mp4 sources using
/// `mov_text` subtitles (or certain data streams) aren't valid inside
/// Matroska and will make the whole `ffmpeg` invocation fail — safely
/// (the original file is never touched; the job just lands in `failed`
/// with no space saved for that file), but not silently worked around by
/// re-encoding or dropping the offending stream. Fixing this properly needs
/// probing the source's subtitle/data codecs first and choosing per-stream
/// handling, which wasn't done here rather than risk an unverified fix.
fn run_ffmpeg_encode(input: &Path, output: &Path, bitrate_kbps: u32, vaapi_device: &str) -> Result<()> {
    let maxrate = bitrate_kbps * 3 / 2;
    let bufsize = bitrate_kbps * 2;

    let result = Command::new("ffmpeg")
        .arg("-y")
        .args(["-hwaccel", "vaapi"])
        .args(["-hwaccel_device", vaapi_device])
        .args(["-hwaccel_output_format", "vaapi"])
        .arg("-i")
        .arg(input)
        .args(["-map", "0"])
        .args(["-c:v", "av1_vaapi"])
        // Explicit rather than relying on the driver's "auto" inference
        // from the bitrate flags present — VBR here spends less on simple
        // scenes and more on complex ones within the maxrate/bufsize
        // envelope below, rather than a flat per-frame target.
        .args(["-rc_mode", "VBR"])
        .args(["-b:v", &format!("{bitrate_kbps}k")])
        .args(["-maxrate", &format!("{maxrate}k")])
        .args(["-bufsize", &format!("{bufsize}k")])
        .args(["-c:a", "copy"])
        .args(["-c:s", "copy"])
        .args(["-c:d", "copy"])
        .arg(output)
        .output()
        .context("failed to run ffmpeg av1_vaapi encode")?;

    if !result.status.success() {
        let _ = std::fs::remove_file(output);
        bail!(
            "ffmpeg av1_vaapi encode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

/// The temp path a given job's encode writes to — job-id-scoped (not just
/// derived from the input filename) so two jobs can never collide on the
/// same temp file even if something ends up processing the same
/// `episode_file` path twice (e.g. a daemon restart racing a still-live
/// `transcode-library` backfill process). Also what `reset_orphaned_
/// running_jobs` uses to find and clean up a crashed job's leftover partial.
///
/// Known cosmetic limitation: `finalize_job` renames this over the
/// original path verbatim, keeping whatever extension the source had — an
/// `.mp4` source ends up as Matroska bytes at an `.mp4` path (Jellyfin
/// content-sniffs, so playback isn't affected, but `episode_file.path`'s
/// extension no longer matches the real container). Not fixed here; would
/// need the rename plus an `episode_file.path` update done together in
/// `finalize_job`'s transaction.
fn temp_path_for(input: &Path, job_id: i64) -> PathBuf {
    input.with_extension(format!("job{job_id}.av1.mkv"))
}

/// The blocking half of a transcode: encode to a temp file alongside the
/// original (same filesystem, required for the atomic rename-based swap
/// later — never a separate staging drive), then verify the result is
/// actually good *before* anything touches the original. Leaves the temp
/// file on disk on success (the caller finalizes the swap under the DB
/// lock); cleans it up itself on any failure, so the original is never at
/// risk regardless of what goes wrong here.
///
/// Returns `Ok(None)` (no encode run at all) if the input turns out to
/// already be AV1 — checked fresh against the real file here, not trusted
/// from whatever `media_file_probe` said when the job was claimed. This is
/// what makes a job that's re-run after a crash between the file-swap
/// rename and the DB bookkeeping (a narrow but real window — see
/// `finalize_job`) self-correct into a no-op instead of re-encoding an
/// already-AV1 file a second time.
fn encode_and_verify(
    input: PathBuf,
    job_id: i64,
    cfg: TranscodeConfig,
    width: i64,
    height: i64,
    original_duration: Option<f64>,
) -> Result<Option<PathBuf>> {
    match ffprobe::probe(&input) {
        Ok(p) if p.video_codec.as_deref() == Some("av1") => return Ok(None),
        // Any other outcome (a different codec, or the probe itself
        // failing) falls through to a real encode attempt as normal —
        // this check exists only to short-circuit the one case where
        // there's provably nothing to do.
        _ => {}
    }

    let tmp_path = temp_path_for(&input, job_id);
    let bitrate = target_bitrate_kbps(width, height, &cfg);

    run_ffmpeg_encode(&input, &tmp_path, bitrate, &cfg.vaapi_device)?;

    match ffprobe::verify_decodable(&tmp_path) {
        Ok(ffprobe::DecodeCheck::Ok) => {}
        Ok(ffprobe::DecodeCheck::Corrupt(detail)) => {
            let _ = std::fs::remove_file(&tmp_path);
            bail!("transcoded output failed decode verification: {detail}");
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.context("failed to run decode verification on transcoded output"));
        }
    }

    if let Some(original_secs) = original_duration {
        match ffprobe::probe(&tmp_path) {
            Ok(new_probe) => {
                if let Some(new_secs) = new_probe.duration_secs {
                    if (new_secs - original_secs).abs() > 1.0 {
                        let _ = std::fs::remove_file(&tmp_path);
                        bail!(
                            "duration mismatch after transcode: original={original_secs}s new={new_secs}s"
                        );
                    }
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e.context("failed to probe transcoded output for duration check"));
            }
        }
    }

    Ok(Some(tmp_path))
}

/// Whether `media_item_id` is anime — same two membership checks
/// (`anime_mapping` for TV, `anime_tmdb_movie` for movies) already used
/// elsewhere for scoring/upgrade exclusions. Anime needs its own encode
/// tuning (thin lines, flat color, grain) not designed yet, so it's
/// excluded from both the backfill and the post-grab path for now.
pub fn is_anime(conn: &Connection, media_item_id: i64) -> Result<bool> {
    let result: bool = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM media_item m
            WHERE m.id = ?1
              AND (
                (m.tvdb_id IS NOT NULL AND m.tvdb_id IN
                    (SELECT tvdb_id FROM anime_mapping WHERE tvdb_id IS NOT NULL))
                OR
                (m.tmdb_id IS NOT NULL AND m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie))
              )
         )",
        params![media_item_id],
        |row| row.get(0),
    )?;
    Ok(result)
}

/// Resets any `running` job back to `pending` — call once at process
/// startup (the daemon itself, and the `transcode-library` backfill), never
/// mid-run. `running` only ever means "some still-alive process is
/// currently encoding this" — a row left in that state at startup can only
/// mean the process that claimed it is gone (crashed, killed, power loss),
/// since a live process's own in-flight jobs aren't visible to it as
/// leftover state, they're just still running. Left unreset, a crash leaves
/// that job's slot permanently uncountable-but-also-unclaimable, quietly
/// shrinking real capacity forever instead of just costing one retry.
///
/// Also sweeps each reset job's expected temp file (`temp_path_for`, which
/// is job-id-scoped precisely so this lookup is unambiguous) — a killed
/// encode leaves a partial `.jobN.av1.mkv` behind that nothing else will
/// ever clean up, and on a library disk that's usually already tight on
/// space these accumulate for real over a big backfill.
pub fn reset_orphaned_running_jobs(conn: &Connection) -> Result<usize> {
    let orphaned: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT j.id, ef.path FROM transcode_job j
             JOIN episode_file ef ON ef.id = j.episode_file_id
             WHERE j.status = 'running'",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (job_id, path) in &orphaned {
        let tmp = temp_path_for(Path::new(path), *job_id);
        if tmp.exists() {
            if let Err(e) = std::fs::remove_file(&tmp) {
                tracing::warn!(job_id, path = %tmp.display(), error = %e, "failed to remove orphaned transcode temp file");
            }
        }
    }

    let reset = conn.execute(
        "UPDATE transcode_job SET status = 'pending' WHERE status = 'running'",
        [],
    )?;
    Ok(reset)
}

/// Queues one file for transcoding. `original_codec`/`original_bytes` are
/// just recorded for the eventual report — not used for any decision.
pub fn enqueue(
    conn: &Connection,
    episode_file_id: i64,
    original_codec: Option<&str>,
    original_bytes: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO transcode_job (episode_file_id, status, original_codec, original_bytes, queued_at)
         VALUES (?1, 'pending', ?2, ?3, datetime('now'))",
        params![episode_file_id, original_codec, original_bytes],
    )?;
    Ok(())
}

/// The single source of truth for "should this freshly-imported file be
/// queued for transcoding at all" — every enqueue site (`import_one`,
/// season-pack import) must go through this rather than re-deriving its own
/// subset of the rules, which is exactly how the post-import hook
/// originally ended up checking anime but silently skipping the HDR/height
/// exclusions `find_backlog_candidates` applies to the backfill. Called
/// after `ensure_probed`, once real ffprobe data (codec/hdr/height) exists
/// for the file — the codec/hdr/height parameters come from that probe, not
/// from anything parsed off the release title.
pub fn should_enqueue(
    conn: &Connection,
    media_item_id: i64,
    video_codec: Option<&str>,
    hdr: bool,
    height: Option<i64>,
    cfg: &TranscodeConfig,
) -> Result<bool> {
    if video_codec == Some("av1") {
        return Ok(false);
    }
    if cfg.exclude_hdr && hdr {
        return Ok(false);
    }
    if height.is_some_and(|h| h >= cfg.exclude_min_height as i64) {
        return Ok(false);
    }
    if is_anime(conn, media_item_id)? {
        return Ok(false);
    }
    Ok(true)
}

/// Every existing-library file eligible for the `transcode-library` backfill:
/// not already AV1, not anime, not HDR/2160p+ (per `cfg.exclude_hdr` /
/// `cfg.exclude_min_height` — the first pass is scoped to SDR 1080p/720p),
/// not already queued or done. Deliberately re-derives the anime exclusion
/// inline (rather than calling `is_anime` per row) so it's one query instead
/// of N+1 against a ~1400-file backlog.
pub fn find_backlog_candidates(conn: &Connection, cfg: &TranscodeConfig) -> Result<Vec<BacklogCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT ef.id, ef.path, ef.size_bytes, p.video_codec, p.width, p.height
         FROM episode_file ef
         JOIN media_file_probe p ON p.episode_file_id = ef.id
         LEFT JOIN episode e ON e.id = ef.episode_id
         JOIN media_item m ON m.id = COALESCE(e.media_item_id, ef.media_item_id)
         WHERE p.video_codec IS NOT NULL AND p.video_codec != 'av1'
           AND (ef.upgrade_locked IS NULL OR ef.upgrade_locked = 0)
           AND (?1 = 0 OR p.hdr = 0)
           AND (p.height IS NOT NULL AND p.height < ?2)
           AND NOT (
                (m.tvdb_id IS NOT NULL AND m.tvdb_id IN
                    (SELECT tvdb_id FROM anime_mapping WHERE tvdb_id IS NOT NULL))
                OR
                (m.tmdb_id IS NOT NULL AND m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie))
           )
           AND ef.id NOT IN (
                SELECT episode_file_id FROM transcode_job WHERE status IN ('pending','running','done')
           )
         ORDER BY (ef.size_bytes * 8.0 / NULLIF(p.duration_secs, 0)) DESC",
    )?;
    let rows = stmt
        .query_map(params![cfg.exclude_hdr as i64, cfg.exclude_min_height], |row| {
            Ok(BacklogCandidate {
                episode_file_id: row.get(0)?,
                path: row.get(1)?,
                size_bytes: row.get(2)?,
                video_codec: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct BacklogCandidate {
    pub episode_file_id: i64,
    pub path: String,
    pub size_bytes: i64,
    pub video_codec: Option<String>,
}

/// A `transcode_job` row claimed for processing this cycle, with the
/// probe/path data `encode_and_verify` needs already attached — claimed
/// under the DB lock, then the actual encode runs entirely outside it.
struct ClaimedJob {
    job_id: i64,
    episode_file_id: i64,
    path: PathBuf,
    width: i64,
    height: i64,
    duration_secs: Option<f64>,
}

/// Claims up to `limit` `pending` jobs (marking them `running` so a crash
/// mid-cycle doesn't leave them silently re-claimable forever without at
/// least having been attempted once) and returns everything the encode step
/// needs. Only claims jobs whose file already has probe data — a job
/// enqueued moments after import but before `ensure_probed` has run yet
/// simply isn't claimed this tick, and picks up naturally on the next one.
///
/// `limit` is treated as a *total* concurrency target, not "claim this many
/// more" — it's first reduced by however many jobs are already `running`
/// (set by anyone: this same daemon's previous still-in-flight cycle, or a
/// separately-invoked `transcode-library` backfill process hitting the same
/// database). Learned the hard way: without this, a long-running cycle
/// (large files easily take longer than `poll_interval_secs`) and a
/// concurrently-run backfill each independently claimed up to their own
/// `parallelism_max` with no awareness of the other, stacking to 20+
/// simultaneous GPU encodes and OOMing the host. `status='running'` is
/// shared database state, not per-process, so counting it is what makes
/// this a real global cap no matter how many processes are hitting this
/// table at once — and wrapping the count+select+update in one
/// `BEGIN IMMEDIATE` transaction (rather than three separate statements)
/// is what stops two processes from both reading the same low count and
/// both claiming past the limit before either one's UPDATE lands.
async fn claim_pending_jobs(conn: &Arc<Mutex<Connection>>, limit: usize) -> Result<Vec<ClaimedJob>> {
    let mut conn = conn.lock().await;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let running: i64 = tx.query_row(
        "SELECT count(*) FROM transcode_job WHERE status = 'running'",
        [],
        |row| row.get(0),
    )?;
    let available = (limit as i64 - running).max(0);

    let claimed = if available == 0 {
        Vec::new()
    } else {
        // Highest current bitrate first — the worst offenders (REMUX, huge
        // season packs) free the most space per file transcoded, so
        // they're worth reaching before smaller, already-reasonable files.
        let mut stmt = tx.prepare(
            "SELECT j.id, j.episode_file_id, ef.path, p.width, p.height, p.duration_secs
             FROM transcode_job j
             JOIN episode_file ef ON ef.id = j.episode_file_id
             JOIN media_file_probe p ON p.episode_file_id = ef.id
             WHERE j.status = 'pending' AND p.width IS NOT NULL AND p.height IS NOT NULL
             ORDER BY (ef.size_bytes * 8.0 / NULLIF(p.duration_secs, 0)) DESC
             LIMIT ?1",
        )?;
        let claimed = stmt
            .query_map(params![available], |row| {
                Ok(ClaimedJob {
                    job_id: row.get(0)?,
                    episode_file_id: row.get(1)?,
                    path: PathBuf::from(row.get::<_, String>(2)?),
                    width: row.get(3)?,
                    height: row.get(4)?,
                    duration_secs: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for job in &claimed {
            tx.execute(
                "UPDATE transcode_job SET status = 'running' WHERE id = ?1",
                params![job.job_id],
            )?;
        }
        claimed
    };

    tx.commit()?;
    Ok(claimed)
}

/// Finalizes one job under the DB lock: on success, atomically swaps the
/// verified temp file over the original, updates `episode_file` (new size +
/// `upgrade_locked = 1`, the flag that keeps the upgrade cycle from ever
/// trying to replace a file breadarr itself just intentionally shrank), and
/// forces a fresh `ensure_probed` so `media_file_probe` reflects the real
/// AV1 ground truth — same shape as `remux_one_backlog_file`. On failure,
/// the original is left completely untouched; the job is marked `failed`
/// with the error recorded, no automatic retry.
///
/// `Ok(None)` from the encode step means `encode_and_verify` found the
/// input already AV1 and skipped the encode entirely — marked `done` with
/// no byte-count change, not `failed`.
///
/// The whole swap-and-bookkeeping sequence runs inside a closure so any
/// failure partway through (a deleted-out-from-under-it original, a
/// transient I/O error) is caught in one place: the job is marked `failed`
/// and the temp file is cleaned up, rather than an early `?` propagating
/// out and silently leaving the job stuck `running` with a leaked temp file
/// on a disk that's usually already tight on space.
async fn finalize_job(
    conn: &Arc<Mutex<Connection>>,
    job: ClaimedJob,
    encode_result: Result<Option<PathBuf>>,
) -> Result<TranscodeOutcome> {
    let conn = conn.lock().await;
    let tmp_path = match &encode_result {
        Ok(Some(p)) => Some(p.clone()),
        _ => None,
    };

    let swap: Result<TranscodeOutcome> = match encode_result {
        Ok(None) => Ok(TranscodeOutcome { original_bytes: 0, new_bytes: 0 }),
        Ok(Some(tmp_path)) => (|| {
            let original_bytes = std::fs::metadata(&job.path)?.len();
            std::fs::rename(&tmp_path, &job.path)?;
            let new_bytes = std::fs::metadata(&job.path)?.len();

            conn.execute(
                "UPDATE episode_file SET size_bytes = ?1, upgrade_locked = 1 WHERE id = ?2",
                params![new_bytes as i64, job.episode_file_id],
            )?;
            conn.execute(
                "DELETE FROM media_file_probe WHERE episode_file_id = ?1",
                params![job.episode_file_id],
            )?;
            importer::ensure_probed(&conn, job.episode_file_id, &job.path)?;
            Ok(TranscodeOutcome { original_bytes, new_bytes })
        })(),
        Err(e) => Err(e),
    };

    match swap {
        Ok(outcome) => {
            conn.execute(
                "UPDATE transcode_job SET status = 'done', new_bytes = ?1, finished_at = datetime('now') WHERE id = ?2",
                params![outcome.new_bytes as i64, job.job_id],
            )?;
            Ok(outcome)
        }
        Err(e) => {
            if let Some(tmp_path) = tmp_path {
                let _ = std::fs::remove_file(&tmp_path);
            }
            conn.execute(
                "UPDATE transcode_job SET status = 'failed', error = ?1, finished_at = datetime('now') WHERE id = ?2",
                params![e.to_string(), job.job_id],
            )?;
            Err(e)
        }
    }
}

pub struct TranscodeOutcome {
    pub original_bytes: u64,
    pub new_bytes: u64,
}

#[derive(Debug, Default)]
pub struct TranscodeCycleStats {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub bytes_saved: i64,
}

/// Active parallelism for this cycle: full `parallelism_max` when nobody's
/// actively watching a transcoded Jellyfin stream, dropped to
/// `parallelism_min` (1) the moment anyone is — the batch job and a real
/// viewer are contending for the same GPU encode/decode engines, and a
/// stutter during someone's actual show loses every time.
async fn effective_parallelism(jellyfin: Option<&JellyfinClient>, cfg: &TranscodeConfig) -> usize {
    let Some(client) = jellyfin else {
        return cfg.parallelism_max;
    };
    match client.active_transcode_sessions().await {
        Ok(0) => cfg.parallelism_max,
        Ok(_) => cfg.parallelism_min,
        Err(e) => {
            tracing::warn!(error = %e, "failed to poll jellyfin sessions; assuming worst case");
            cfg.parallelism_min
        }
    }
}

/// One transcode-worker pass: claims up to the current (Jellyfin-aware)
/// parallelism worth of pending jobs, encodes them concurrently entirely
/// outside the DB lock (each encode can take minutes — holding the shared
/// mutex for that long would stall every other cycle: import, search,
/// review-queue actions, everything), then finalizes each result under a
/// brief lock. Shared by both the steady-state daemon ticker and the
/// `transcode-library` backfill CLI, so there's exactly one code path that
/// actually runs an encode.
pub async fn run_cycle(
    conn: Arc<Mutex<Connection>>,
    cfg: TranscodeConfig,
    jellyfin: Option<&JellyfinClient>,
) -> Result<TranscodeCycleStats> {
    let parallelism = effective_parallelism(jellyfin, &cfg).await;
    let claimed = claim_pending_jobs(&conn, parallelism).await?;

    let mut stats = TranscodeCycleStats::default();
    if claimed.is_empty() {
        return Ok(stats);
    }

    let mut handles = Vec::with_capacity(claimed.len());
    for job in claimed {
        let cfg = cfg.clone();
        let path = job.path.clone();
        let job_id = job.job_id;
        let (width, height, duration) = (job.width, job.height, job.duration_secs);
        let encode_handle = tokio::task::spawn_blocking(move || {
            encode_and_verify(path, job_id, cfg, width, height, duration)
        });
        handles.push((job, encode_handle));
    }

    for (job, encode_handle) in handles {
        stats.attempted += 1;
        let encode_result = match encode_handle.await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow::anyhow!("encode task panicked: {join_err}")),
        };
        match finalize_job(&conn, job, encode_result).await {
            Ok(outcome) => {
                stats.succeeded += 1;
                stats.bytes_saved += outcome.original_bytes as i64 - outcome.new_bytes as i64;
            }
            Err(e) => {
                stats.failed += 1;
                tracing::warn!(error = %e, "transcode job failed");
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranscodeConfig {
        TranscodeConfig {
            enabled: true,
            poll_interval_secs: 60,
            vaapi_device: "/dev/dri/renderD128".to_string(),
            parallelism_min: 1,
            parallelism_max: 4,
            reference_bitrate_kbps: 5320,
            reference_height: 1080,
            av1_efficiency_factor: 0.7,
            exclude_hdr: true,
            exclude_min_height: 2000,
        }
    }

    fn seed_file(conn: &Connection, id: i64, size_bytes: i64) {
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes)
             VALUES (?1, NULL, NULL, ?2, ?3)",
            params![id, format!("/tmp/f{id}.mkv"), size_bytes],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime,
                                            duration_secs, video_codec, width, height)
             VALUES (?1, datetime('now'), ?2, 0, 1200.0, 'hevc', 1920, 1080)",
            params![id, size_bytes],
        )
        .unwrap();
    }

    // Regression test for a real incident: a long-running cycle (large
    // files can easily outlast `poll_interval_secs`) and a separately
    // invoked `transcode-library` backfill each independently claimed up to
    // their own `parallelism_max` with no shared awareness, stacking to 20+
    // concurrent GPU encodes and OOMing the host. `claim_pending_jobs` must
    // treat `limit` as a total cap, not "claim this many more".
    #[tokio::test]
    async fn claim_pending_jobs_respects_already_running_jobs_as_a_global_cap() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        for id in 1..=5 {
            seed_file(&conn, id, 1_000_000_000);
        }
        // Two jobs already claimed by "someone else" (another process, or
        // this same process's still-in-flight previous cycle).
        conn.execute(
            "INSERT INTO transcode_job (episode_file_id, status, queued_at) VALUES (1, 'running', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcode_job (episode_file_id, status, queued_at) VALUES (2, 'running', datetime('now'))",
            [],
        )
        .unwrap();
        for id in 3..=5 {
            conn.execute(
                "INSERT INTO transcode_job (episode_file_id, status, queued_at) VALUES (?1, 'pending', datetime('now'))",
                params![id],
            )
            .unwrap();
        }

        let conn = Arc::new(Mutex::new(conn));
        // Asking for a total of 3 concurrent, with 2 already running —
        // should claim exactly 1 more, not 3 more.
        let claimed = claim_pending_jobs(&conn, 3).await.unwrap();
        assert_eq!(claimed.len(), 1);

        let conn = conn.lock().await;
        let running: i64 = conn
            .query_row("SELECT count(*) FROM transcode_job WHERE status = 'running'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(running, 3, "total in-flight jobs must never exceed the requested cap");
    }

    #[tokio::test]
    async fn claim_pending_jobs_claims_nothing_when_already_at_the_cap() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        for id in 1..=3 {
            seed_file(&conn, id, 1_000_000_000);
        }
        for id in 1..=2 {
            conn.execute(
                "INSERT INTO transcode_job (episode_file_id, status, queued_at) VALUES (?1, 'running', datetime('now'))",
                params![id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO transcode_job (episode_file_id, status, queued_at) VALUES (3, 'pending', datetime('now'))",
            [],
        )
        .unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let claimed = claim_pending_jobs(&conn, 2).await.unwrap();
        assert!(claimed.is_empty(), "already at the cap — must not claim more");
    }

    fn generate_test_clip(dir: &Path, codec: &str) -> PathBuf {
        let path = dir.join(format!("clip_{codec}.mkv"));
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=320x240:duration=1:rate=1"])
            .args(["-c:v", codec])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a test clip");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate a {codec} test clip: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    // Regression test for a real gap found in review: a crash between the
    // file-swap rename and the DB bookkeeping in `finalize_job` leaves a job
    // `running` with the file already AV1 but `media_file_probe` still
    // stale. On retry, `encode_and_verify` must notice the *actual* file is
    // already AV1 and skip re-encoding it, rather than trusting the stale
    // DB probe data it was claimed with.
    #[test]
    fn encode_and_verify_skips_a_file_that_is_already_av1() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-already-av1-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, "libsvtav1");

        let result = encode_and_verify(clip.clone(), 999, cfg(), 320, 240, Some(1.0)).unwrap();
        assert!(result.is_none(), "an already-AV1 file must not be re-encoded");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn target_bitrate_matches_reference_at_reference_resolution() {
        let bitrate = target_bitrate_kbps(1920, 1080, &cfg());
        // pixel_ratio == 1.0 at the reference resolution, so this should be
        // exactly reference_bitrate_kbps * av1_efficiency_factor.
        assert_eq!(bitrate, (5320.0_f64 * 0.7).round() as u32);
    }

    #[test]
    fn target_bitrate_scales_down_for_720p() {
        let bitrate_1080 = target_bitrate_kbps(1920, 1080, &cfg());
        let bitrate_720 = target_bitrate_kbps(1280, 720, &cfg());
        assert!(bitrate_720 < bitrate_1080);
        // Roughly proportional to pixel count (~0.44x), not some flat cut.
        let ratio = bitrate_720 as f64 / bitrate_1080 as f64;
        assert!((0.4..0.5).contains(&ratio), "ratio was {ratio}");
    }

    #[test]
    fn target_bitrate_scales_up_for_1440p() {
        let bitrate_1080 = target_bitrate_kbps(1920, 1080, &cfg());
        let bitrate_1440 = target_bitrate_kbps(2560, 1440, &cfg());
        assert!(bitrate_1440 > bitrate_1080);
    }
}
