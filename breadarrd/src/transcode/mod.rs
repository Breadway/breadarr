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

/// Runs the live-action GPU encode via `av1_vaapi`, decoding through VAAPI
/// too (`-hwaccel_output_format vaapi`) so the whole pipeline stays on-GPU
/// rather than round-tripping frames through the CPU. Video-only re-encode
/// — every audio/subtitle/data stream is copied verbatim (`-c:a copy -c:s
/// copy -c:d copy`).
///
/// Rate control is `QVBR` (quality-defined VBR): `-global_quality` is the
/// actual quality target driving output size, `-b:v`/`-maxrate`/`-bufsize`
/// (computed from `target_bitrate_kbps`) are a *ceiling*, not a target —
/// content that's already efficient spends less than the ceiling rather
/// than being inflated up toward it. This replaced a flat `VBR + -b:v`
/// scheme after a real incident: that scheme treated `-b:v` as the target
/// average rather than a cap, so already-efficient sources (some well under
/// the model's own reference bitrate) got re-encoded *larger* than the
/// original on the majority of a real backfill. `encode_and_verify`'s
/// post-encode size check is the actual hard guarantee against that
/// happening again regardless of how this rate-control tuning holds up —
/// this function only has to get it roughly right, not perfectly.
///
/// Blocking and slow by design (a real GPU encode, potentially minutes per
/// file) — callers must run this inside `tokio::task::spawn_blocking`, never
/// directly on an async task, and never while holding the shared DB mutex.
///
/// The output container is always Matroska, so this maps streams by type
/// rather than the blanket `-map 0` the first version used — two real
/// gaps that caused, both fixed:
/// - `-map 0` pulled in attached-picture streams (embedded cover art,
///   exposed by ffmpeg as a second `video`-type stream, `mjpeg`/`png`) and
///   `-c:v av1_vaapi` then tried to encode *that* too, which the VAAPI
///   filter chain can't handle — failed the whole job on any file with
///   embedded cover art (several real library files hit this). `-map
///   0:v:0` selects only the first video stream — the actual content,
///   never the attached-pic that (per `ffprobe::probe`'s own convention)
///   always comes after it — so cover art is silently dropped rather than
///   crashing the encode.
/// - mp4 sources using `mov_text` subtitles aren't valid inside Matroska
///   via stream copy and failed the whole job. `subtitle_codec_args`
///   converts just the `mov_text` streams to `srt` (a lossless format
///   change for plain timed text) and copies everything else untouched
///   (so an already-Matroska-compatible `ass`/`srt` track keeps its
///   styling rather than being flattened).
///
/// Rate control mode is `ICQ`, not `QVBR` — a live validation run found
/// Hestia's iHD driver rejects `QVBR` outright ("Driver does not support
/// QVBR RC mode (supported modes: CQP, CBR, VBR, ICQ)"), despite `ffmpeg -h
/// encoder=av1_vaapi` listing `QVBR` as a libavcodec-level option; the
/// *driver* is the actual authority on what's usable, and libavcodec
/// doesn't validate that ahead of time. `ICQ` is the quality-driven mode
/// this driver actually has, so it's the correct target regardless — the
/// `-b:v`/`-maxrate`/`-bufsize` ceiling stays as a best-effort cap (harmless
/// if this driver ignores it under ICQ; `encode_and_verify`'s post-encode
/// `is_beneficial` check is the actual hard guarantee either way, not this).
///
/// `-loglevel error` suppresses ffmpeg's default per-frame progress output
/// (`frame=... fps=... bitrate=... speed=...`, one line per progress tick)
/// — for a long encode this otherwise accumulates to megabytes in the
/// `Command::output()`-captured stderr buffer, which is pure noise in a
/// failure's stored `error` text and unnecessary memory held by the parent
/// process for the whole encode's duration.
///
/// Decode is `-hwaccel vaapi` but deliberately *without*
/// `-hwaccel_output_format vaapi` — a live validation run found that
/// combination fails even on the simplest possible single-stream input
/// ("Impossible to convert between the formats supported by the filter
/// 'Parsed_null_0' and the filter 'auto_scale_0'" / "Function not
/// implemented"), because it makes ffmpeg keep the decoded frame
/// GPU-resident and then implicitly spin up a *second*, separate VAAPI
/// device context to bridge into the encoder — the two contexts don't
/// negotiate a compatible format with each other on this driver, wrong
/// devices or not. Explicitly downloading to a normal system-memory frame
/// (`-hwaccel_output_format` omitted) and re-uploading through the *same*
/// device context via `-vf format=nv12,hwupload` avoids that implicit
/// second context entirely, and is what actually produces a valid,
/// decodable AV1 output in practice — confirmed against real hardware
/// before trusting it further.
fn run_ffmpeg_encode_live_action(
    input: &Path,
    output: &Path,
    bitrate_ceiling_kbps: u32,
    quality: u32,
    vaapi_device: &str,
    subtitles: &[ffprobe::SubtitleStream],
) -> Result<()> {
    let maxrate = bitrate_ceiling_kbps * 3 / 2;
    let bufsize = bitrate_ceiling_kbps * 2;

    let result = Command::new("ffmpeg")
        .arg("-y")
        .args(["-loglevel", "error"])
        .args(["-hwaccel", "vaapi"])
        .args(["-hwaccel_device", vaapi_device])
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v:0"])
        .args(["-map", "0:a?"])
        .args(["-map", "0:s?"])
        .args(["-map", "0:d?"])
        .args(["-vf", "format=nv12,hwupload"])
        .args(["-c:v", "av1_vaapi"])
        .args(["-rc_mode", "ICQ"])
        .args(["-global_quality", &quality.to_string()])
        .args(["-b:v", &format!("{bitrate_ceiling_kbps}k")])
        .args(["-maxrate", &format!("{maxrate}k")])
        .args(["-bufsize", &format!("{bufsize}k")])
        .args(["-c:a", "copy"])
        .args(subtitle_codec_args(subtitles))
        .args(["-c:d", "copy"])
        // Explicit rather than relying on `output`'s extension to imply the
        // muxer: `temp_path_for` deliberately gives the scratch file a
        // non-video extension so library scans skip it (see its own doc
        // comment), which would otherwise leave ffmpeg unable to guess a
        // container from the output path at all.
        .args(["-f", "matroska"])
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

/// Anime pipeline: software `libsvtav1` at 10-bit (`yuv420p10le`), CRF-driven
/// (no bitrate ceiling — this is pure quality-mode, unlike the live-action
/// path). Two reasons this isn't just `run_ffmpeg_encode_live_action` with a
/// different quality number:
///
/// 1. No hardware AV1 10-bit encode exists on Hestia's Arc A380 — `vainfo`
///    only lists `VAProfileAV1Profile0` (8-bit). Anime art (flat color
///    fields, gradient shading/skies) shows 8-bit banding far more readily
///    than live-action's grain/texture does at an equivalent bitrate, so
///    getting true 10-bit output is worth trading GPU offload for CPU time.
/// 2. `av1_vaapi` exposes no tune/animation-specific options at all (its
///    `AVOption` list is rate-control/GOP-structure knobs only) — a
///    meaningfully different anime pipeline wasn't achievable on the
///    hardware encoder regardless of quality value chosen.
///
/// Fully software: decode is left default (not `-hwaccel vaapi`) since a
/// software-encode target gains nothing from a hardware-decoded/GPU-resident
/// frame (it would need `hwdownload` back to system memory anyway), and
/// `libsvtav1` is overwhelmingly the throughput bottleneck either way.
///
/// Same blocking/slow-by-design and stream-mapping/subtitle-codec handling
/// as `run_ffmpeg_encode_live_action` (attached-pic cover art dropped via
/// `-map 0:v:0`, `mov_text` subtitles converted to `srt`) apply here too.
///
/// `max_threads` is passed through as `-svtav1-params lp=N` (SVT-AV1's own
/// "logical processors" cap) — a real validation run on Hestia's 6c/12t box
/// let `libsvtav1` use every thread at once with no explicit bound, and a
/// single 1080p episode grew to **9.3GB resident memory** before the kernel
/// OOM-killer took it out (no other services lost, but a second real OOM
/// incident is a second time too many). SVT-AV1's memory use scales with
/// both preset and thread count — more parallel workers means more
/// concurrently-buffered lookahead/reference frames — so capping threads
/// bounds peak memory to something predictable regardless of how many
/// cores the host actually has, independent of whatever preset is chosen.
///
/// `-loglevel error` for the same reason as the live-action encoder — this
/// path runs for tens of minutes per file, and unsuppressed progress output
/// would otherwise dominate the captured stderr buffer for that whole time.
fn run_ffmpeg_encode_anime(
    input: &Path,
    output: &Path,
    crf: u32,
    preset: u32,
    max_threads: u32,
    subtitles: &[ffprobe::SubtitleStream],
) -> Result<()> {
    let result = Command::new("ffmpeg")
        .arg("-y")
        .args(["-loglevel", "error"])
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v:0"])
        .args(["-map", "0:a?"])
        .args(["-map", "0:s?"])
        .args(["-map", "0:d?"])
        .args(["-c:v", "libsvtav1"])
        .args(["-pix_fmt", "yuv420p10le"])
        .args(["-crf", &crf.to_string()])
        .args(["-preset", &preset.to_string()])
        .args(["-svtav1-params", &format!("lp={max_threads}")])
        .args(["-c:a", "copy"])
        .args(subtitle_codec_args(subtitles))
        .args(["-c:d", "copy"])
        // See the matching comment in `run_ffmpeg_encode_live_action`.
        .args(["-f", "matroska"])
        .arg(output)
        .output()
        .context("failed to run ffmpeg libsvtav1 encode")?;

    if !result.status.success() {
        let _ = std::fs::remove_file(output);
        bail!(
            "ffmpeg libsvtav1 encode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

/// Per-subtitle-stream `-c:s:{i}` codec args, in the same order `-map
/// 0:s?` presents them in the output. `mov_text` (mp4's timed-text codec)
/// isn't valid inside the Matroska container this pipeline always writes,
/// so it's converted to `srt` — a lossless format change for plain timed
/// text. Everything else (`ass`, `srt`, etc. — already Matroska-compatible)
/// is copied untouched rather than flattened through a lossy re-encode.
/// Pulled out as its own pure function so this exact per-stream decision —
/// the thing that made every mp4-sourced file with subtitles fail outright
/// — has direct unit coverage without needing a real ffmpeg encode.
fn subtitle_codec_args(subtitles: &[ffprobe::SubtitleStream]) -> Vec<String> {
    subtitles
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            let codec = if s.codec.as_deref() == Some("mov_text") { "srt" } else { "copy" };
            [format!("-c:s:{i}"), codec.to_string()]
        })
        .collect()
}

/// The temp path a given job's encode writes to — job-id-scoped (not just
/// derived from the input filename) so two jobs can never collide on the
/// same temp file even if something ends up processing the same
/// `episode_file` path twice (e.g. a daemon restart racing a still-live
/// `transcode-library` backfill process). Also what `reset_orphaned_
/// running_jobs` uses to find and clean up a crashed job's leftover partial.
///
/// Written into the *library* directory (same filesystem as the original —
/// required for the atomic rename-based swap in `finalize_job`, never a
/// separate staging drive) for however long the encode takes (minutes to
/// hours). Deliberately given a non-video final extension (`.tmp`, not
/// `.mkv`) and a leading dot: a real production shape found in review — a
/// growing `....mkv` sitting mid-encode in the library was itself picked up
/// by `collect_video_files`/`walk_files` (both filter on `VIDEO_EXTS`), so
/// `find_relinkable_episode_files` saw two candidate files for the same
/// `SxxExx` and reported the episode "ambiguous," `library_scan` indexed
/// the partial file, and Jellyfin could index/attempt to play a
/// still-encoding file mid-scan. `.tmp` sidesteps every one of those
/// `VIDEO_EXTS`-based scans without needing to special-case any of them
/// individually; `-f matroska` is passed explicitly to both encode
/// functions so ffmpeg doesn't need `output`'s extension to infer the
/// container now that it's no longer `.mkv`-shaped.
///
/// Known cosmetic limitation, unrelated to the above: `finalize_job`
/// renames this over the *original* path verbatim, keeping whatever
/// extension the source had — an `.mp4` source ends up as Matroska bytes at
/// an `.mp4` path (Jellyfin content-sniffs, so playback isn't affected, but
/// `episode_file.path`'s extension no longer matches the real container).
/// Not fixed here; would need the rename plus an `episode_file.path` update
/// done together in `finalize_job`'s transaction.
fn temp_path_for(input: &Path, job_id: i64) -> PathBuf {
    let file_name = input
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("transcode");
    input.with_file_name(format!(".{file_name}.job{job_id}.tmp"))
}

/// What `encode_and_verify` decided to do. Distinct from a hard `Err` (an
/// actual failure — ffmpeg crashed, output was corrupt, duration mismatched)
/// — both `AlreadyAv1` and `NotBeneficial` are successful, deliberate
/// no-ops: nothing went wrong, there's just no transcode worth keeping.
#[derive(Debug)]
enum EncodeOutcome {
    /// The input was already AV1 — no encode was even attempted.
    AlreadyAv1,
    /// An encode ran (or was skipped pre-emptively) but the result wasn't
    /// meaningfully smaller than the original, so it was discarded rather
    /// than swapped in.
    NotBeneficial,
    /// A verified, meaningfully-smaller output ready to be swapped in.
    Encoded(PathBuf),
}

/// The blocking half of a transcode: encode to a temp file alongside the
/// original (same filesystem, required for the atomic rename-based swap
/// later — never a separate staging drive), then verify the result is
/// actually good *before* anything touches the original. Leaves the temp
/// file on disk only for the `Encoded` outcome (the caller finalizes the
/// swap under the DB lock); cleans it up itself in every other case, so the
/// original is never at risk regardless of what happens here.
///
/// Returns `AlreadyAv1` (no encode run at all) if the input turns out to
/// already be AV1 — checked fresh against the real file here, not trusted
/// from whatever `media_file_probe` said when the job was claimed. This is
/// what makes a job that's re-run after a crash between the file-swap
/// rename and the DB bookkeeping (a narrow but real window — see
/// `finalize_job`) self-correct into a no-op instead of re-encoding an
/// already-AV1 file a second time. `force_reencode` (set only by
/// `find_oversized_av1_candidates`'s remediation jobs) skips this check
/// entirely — for a file that's already AV1 but was mis-encoded (e.g. left
/// larger than its original by the rate-control bug this whole module's
/// history is built around), "already AV1" is exactly the case that *does*
/// need a real re-encode, not a no-op. The `is_beneficial` check further
/// down still applies unconditionally either way — forcing a re-encode
/// attempt never bypasses the "never keep a non-improvement" guarantee.
///
/// Returns `NotBeneficial` in two cases: (1) a pre-check, before spending
/// any GPU/CPU time, when the source's current bitrate is already at or
/// below `skip_below_ceiling_ratio` of the resolution-scaled ceiling — a
/// strong signal there's little room to save; (2) after encoding, if the
/// result isn't at least `min_size_reduction_pct` smaller than the
/// original. (2) is the actual hard guarantee against a real incident where
/// flat-bitrate rate control silently produced outputs *larger* than the
/// original on most of a real backfill — regardless of how well-tuned the
/// rate control is, this is what makes "never keep a non-improvement" true
/// unconditionally.
fn encode_and_verify(
    input: PathBuf,
    job_id: i64,
    cfg: TranscodeConfig,
    width: i64,
    height: i64,
    original_duration: Option<f64>,
    is_anime: bool,
    force_reencode: bool,
) -> Result<EncodeOutcome> {
    // Kept (not just checked and discarded) so its `subtitles` can be
    // reused for `subtitle_codec_args` below without a second ffprobe call.
    let fresh_probe = ffprobe::probe(&input);
    if !force_reencode {
        if let Ok(p) = &fresh_probe {
            if p.video_codec.as_deref() == Some("av1") {
                return Ok(EncodeOutcome::AlreadyAv1);
            }
        }
    }
    // Any other outcome (a different codec, or the probe itself failing)
    // falls through to a real encode attempt as normal — the check above
    // exists only to short-circuit the one case where there's provably
    // nothing to do. A failed probe means no subtitle info either, so
    // `subtitle_codec_args` just gets an empty slice (falls back to
    // whatever ffmpeg's own default subtitle handling is for the output
    // container) rather than guessing.
    let subtitles: &[ffprobe::SubtitleStream] =
        fresh_probe.as_ref().map(|p| p.subtitles.as_slice()).unwrap_or(&[]);

    let original_bytes = std::fs::metadata(&input)
        .context("failed to stat input file before transcode")?
        .len();
    let ceiling_bitrate_kbps = target_bitrate_kbps(width, height, &cfg);

    if let Some(duration) = original_duration {
        if duration > 0.0 {
            let source_bitrate_kbps = (original_bytes as f64 * 8.0) / duration / 1000.0;
            if source_bitrate_kbps <= ceiling_bitrate_kbps as f64 * cfg.skip_below_ceiling_ratio {
                return Ok(EncodeOutcome::NotBeneficial);
            }
        }
    }

    let tmp_path = temp_path_for(&input, job_id);

    if is_anime {
        run_ffmpeg_encode_anime(
            &input,
            &tmp_path,
            cfg.quality_anime,
            cfg.anime_svtav1_preset,
            cfg.anime_svtav1_max_threads,
            subtitles,
        )?;
    } else {
        run_ffmpeg_encode_live_action(
            &input,
            &tmp_path,
            ceiling_bitrate_kbps,
            cfg.quality_live_action,
            &cfg.vaapi_device,
            subtitles,
        )?;
    }

    let decode_check = match original_duration {
        Some(duration) => ffprobe::verify_decodable_sampled(&tmp_path, duration, cfg.verify_sample_secs),
        // Duration unknown — can't pick sample windows sensibly, so fall
        // back to the thorough full-file check rather than guess.
        None => ffprobe::verify_decodable(&tmp_path),
    };
    match decode_check {
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

    let new_bytes = std::fs::metadata(&tmp_path)
        .context("failed to stat transcoded output")?
        .len();
    if !is_beneficial(original_bytes, new_bytes, cfg.min_size_reduction_pct) {
        let _ = std::fs::remove_file(&tmp_path);
        return Ok(EncodeOutcome::NotBeneficial);
    }

    Ok(EncodeOutcome::Encoded(tmp_path))
}

/// The hard invariant that makes "never keep a non-improvement" true
/// regardless of how well any rate-control tuning holds up: an output only
/// counts as worth keeping if it's at least `min_size_reduction_pct`
/// smaller than the original. Pulled out as its own pure function so this
/// exact arithmetic — the thing a real incident got wrong — has direct unit
/// coverage without needing a real ffmpeg encode to exercise it.
fn is_beneficial(original_bytes: u64, new_bytes: u64, min_size_reduction_pct: f64) -> bool {
    let max_allowed_bytes = (original_bytes as f64 * (1.0 - min_size_reduction_pct)) as u64;
    new_bytes <= max_allowed_bytes
}

/// Whether `media_item_id` is anime per metadata — same two membership
/// checks (`anime_mapping` for TV, `anime_tmdb_movie` for movies) already
/// used elsewhere for scoring/upgrade exclusions. Known to have real
/// coverage gaps (Avatar: The Last Airbender and some Dragon Ball movies
/// were missing from these tables) — `is_anime_content` below is the
/// combined check that should actually be used for routing; this is kept
/// as its own function since other callers (`scheduler.rs`'s search
/// routing) still want the metadata-only check.
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

/// Whether `path` falls under one of `cfg.anime_root_folders` — a plain
/// prefix match. Deliberately independent of any DB metadata: it's how the
/// library is actually organized on disk, and it's what catches the real
/// gaps in `anime_mapping`/`anime_tmdb_movie` coverage (see `is_anime`'s
/// doc comment).
pub fn is_anime_path(path: &Path, cfg: &TranscodeConfig) -> bool {
    cfg.anime_root_folders.iter().any(|root| path.starts_with(root))
}

/// The combined anime check every enqueue site should use to decide which
/// encode pipeline (`run_ffmpeg_encode_anime` vs `_live_action`) a file
/// gets routed to: path-based first (cheap, no DB access, and the more
/// reliable signal — see `is_anime_path`), falling back to the
/// metadata-based `is_anime` check for anime content filed outside the
/// configured anime root folders.
pub fn is_anime_content(
    conn: &Connection,
    media_item_id: i64,
    path: &Path,
    cfg: &TranscodeConfig,
) -> Result<bool> {
    if is_anime_path(path, cfg) {
        return Ok(true);
    }
    is_anime(conn, media_item_id)
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
/// `is_anime` is decided once here (by the caller, via `is_anime_content`)
/// and carried on the job row so `claim_pending_jobs` can dispatch to the
/// right encode pipeline without re-deriving it at claim time. Every normal
/// enqueue site passes `force_reencode: false`; `true` is only for
/// `find_oversized_av1_candidates`'s remediation jobs — see
/// `encode_and_verify`'s doc comment for why that flag exists.
pub fn enqueue(
    conn: &Connection,
    episode_file_id: i64,
    original_codec: Option<&str>,
    original_bytes: i64,
    is_anime: bool,
    force_reencode: bool,
) -> Result<()> {
    conn.execute(
        "INSERT INTO transcode_job (episode_file_id, status, original_codec, original_bytes, is_anime, force_reencode, queued_at)
         VALUES (?1, 'pending', ?2, ?3, ?4, ?5, datetime('now'))",
        params![episode_file_id, original_codec, original_bytes, is_anime as i64, force_reencode as i64],
    )?;
    Ok(())
}

/// The single source of truth for "should this freshly-imported file be
/// queued for transcoding at all" — every enqueue site (`import_one`,
/// season-pack import) must go through this rather than re-deriving its own
/// subset of the rules. Called after `ensure_probed`, once real ffprobe
/// data (codec/hdr/height) exists for the file — the codec/hdr/height
/// parameters come from that probe, not from anything parsed off the
/// release title.
///
/// No longer excludes anime — anime now has its own encode pipeline
/// (`run_ffmpeg_encode_anime`, see `is_anime_content`) instead of being
/// skipped outright. A pure function now that anime is out of it (no DB
/// access needed to decide codec/HDR/height eligibility).
pub fn should_enqueue(
    video_codec: Option<&str>,
    hdr: bool,
    height: Option<i64>,
    cfg: &TranscodeConfig,
) -> bool {
    // A `probe_failed` row has NULL codec/height (see `ensure_probed`), not a
    // missing row — without this check `should_enqueue` would happily enqueue
    // a job for it. `claim_pending_jobs` requires `p.width`/`p.height IS NOT
    // NULL` so that job can never be claimed, and the unique partial index on
    // (pending/running) then permanently blocks any future, real enqueue for
    // this file once probing succeeds.
    if video_codec.is_none() || height.is_none() {
        return false;
    }
    if video_codec == Some("av1") {
        return false;
    }
    if cfg.exclude_hdr && hdr {
        return false;
    }
    if height.is_some_and(|h| h >= cfg.exclude_min_height as i64) {
        return false;
    }
    true
}

/// Every existing-library file eligible for the `transcode-library` backfill:
/// not already AV1, not HDR/2160p+ (per `cfg.exclude_hdr` /
/// `cfg.exclude_min_height` — the first pass is scoped to SDR 1080p/720p),
/// not already queued, done, or `skipped` (a completed encode that
/// `finalize_job` rejected as not-beneficial — re-selecting it here would
/// re-run the full encode from scratch every time this backfill runs, only
/// to reject it again; a genuinely `failed` job *is* re-selected, since a
/// transient failure — disk full, transient ffmpeg crash — deserves a retry
/// on the next manual run). Anime is included (routed to its own
/// pipeline, not excluded) — `is_anime` on each candidate is the metadata
/// half of that decision; combined with the path-based check
/// (`is_anime_path`) in Rust after the query, since a dynamic list of path
/// prefixes (`cfg.anime_root_folders`) doesn't fit cleanly into static SQL.
pub fn find_backlog_candidates(conn: &Connection, cfg: &TranscodeConfig) -> Result<Vec<BacklogCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT ef.id, ef.path, ef.size_bytes, p.video_codec,
                (m.tvdb_id IS NOT NULL AND m.tvdb_id IN
                    (SELECT tvdb_id FROM anime_mapping WHERE tvdb_id IS NOT NULL))
                OR
                (m.tmdb_id IS NOT NULL AND m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie))
                AS db_is_anime
         FROM episode_file ef
         JOIN media_file_probe p ON p.episode_file_id = ef.id
         LEFT JOIN episode e ON e.id = ef.episode_id
         JOIN media_item m ON m.id = COALESCE(e.media_item_id, ef.media_item_id)
         WHERE p.video_codec IS NOT NULL AND p.video_codec != 'av1'
           AND (ef.upgrade_locked IS NULL OR ef.upgrade_locked = 0)
           AND (?1 = 0 OR p.hdr = 0)
           AND (p.height IS NOT NULL AND p.height < ?2)
           AND ef.id NOT IN (
                SELECT episode_file_id FROM transcode_job WHERE status IN ('pending','running','done','skipped')
           )
         ORDER BY (ef.size_bytes * 8.0 / NULLIF(p.duration_secs, 0)) DESC",
    )?;
    let rows = stmt
        .query_map(params![cfg.exclude_hdr as i64, cfg.exclude_min_height], |row| {
            let path: String = row.get(1)?;
            let db_is_anime: bool = row.get(4)?;
            Ok(BacklogCandidate {
                episode_file_id: row.get(0)?,
                is_anime: db_is_anime || is_anime_path(Path::new(&path), cfg),
                path,
                size_bytes: row.get(2)?,
                video_codec: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every file whose *most recent* transcode attempt succeeded (`done`) but
/// wasn't actually beneficial by the current `min_size_reduction_pct` bar —
/// the 476-file legacy of a real rate-control bug that left files larger
/// than their original, back before `is_beneficial` existed as a hard
/// invariant. These are already AV1 (`find_backlog_candidates` excludes
/// them for exactly that reason), and their pre-transcode originals are
/// long gone — the only way to reclaim the space is to re-transcode the
/// current, oversized AV1 file itself, which needs `enqueue`'s
/// `force_reencode: true` to get past `encode_and_verify`'s normal
/// already-AV1 short-circuit. The `WITH latest_job` CTE picks each file's
/// single most recent job so a file already successfully re-transcoded in
/// a prior run of this same query (its latest job now `done` and properly
/// smaller) doesn't get selected again.
pub fn find_oversized_av1_candidates(conn: &Connection, cfg: &TranscodeConfig) -> Result<Vec<BacklogCandidate>> {
    let mut stmt = conn.prepare(
        "WITH latest_job AS (
            SELECT episode_file_id, MAX(id) AS job_id
            FROM transcode_job
            GROUP BY episode_file_id
        )
         SELECT ef.id, ef.path, ef.size_bytes, p.video_codec,
                (m.tvdb_id IS NOT NULL AND m.tvdb_id IN
                    (SELECT tvdb_id FROM anime_mapping WHERE tvdb_id IS NOT NULL))
                OR
                (m.tmdb_id IS NOT NULL AND m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie))
                AS db_is_anime
         FROM latest_job lj
         JOIN transcode_job tj ON tj.id = lj.job_id
         JOIN episode_file ef ON ef.id = lj.episode_file_id
         JOIN media_file_probe p ON p.episode_file_id = ef.id
         LEFT JOIN episode e ON e.id = ef.episode_id
         JOIN media_item m ON m.id = COALESCE(e.media_item_id, ef.media_item_id)
         WHERE tj.status = 'done'
           AND tj.original_bytes IS NOT NULL AND tj.new_bytes IS NOT NULL AND tj.original_bytes > 0
           AND tj.new_bytes > tj.original_bytes * (1.0 - ?1)
           AND p.video_codec = 'av1'
         ORDER BY (tj.new_bytes - tj.original_bytes) DESC",
    )?;
    let rows = stmt
        .query_map(params![cfg.min_size_reduction_pct], |row| {
            let path: String = row.get(1)?;
            let db_is_anime: bool = row.get(4)?;
            Ok(BacklogCandidate {
                episode_file_id: row.get(0)?,
                is_anime: db_is_anime || is_anime_path(Path::new(&path), cfg),
                path,
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
    pub is_anime: bool,
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
    is_anime: bool,
    force_reencode: bool,
}

/// Claims up to `live_action_limit` live-action jobs and up to
/// `anime_limit` anime jobs (marking them `running` so a crash mid-cycle
/// doesn't leave them silently re-claimable forever without at least
/// having been attempted once) and returns everything the encode step
/// needs. Only claims jobs whose file already has probe data — a job
/// enqueued moments after import but before `ensure_probed` has run yet
/// simply isn't claimed this tick, and picks up naturally on the next one.
///
/// The two limits are enforced **independently** — a real gap found after
/// raising the live-action cap for a validated GPU-scaling reason: a single
/// shared cap would let "raise GPU parallelism" silently also raise anime
/// (CPU-bound, thread-hungry) concurrency to something never load-tested.
/// Each limit is treated as a *total* concurrency target for its own
/// pipeline, not "claim this many more" — first reduced by however many
/// jobs of that same pipeline are already `running` (set by anyone: this
/// same daemon's previous still-in-flight cycle, or a separately-invoked
/// `transcode-library`/`retranscode-oversized` process hitting the same
/// database). Learned the hard way (the *original* version of this bug,
/// before the pipelines were even split): without this, a long-running
/// cycle and a concurrently-run backfill each independently claimed up to
/// their own cap with no awareness of the other, stacking to 20+
/// simultaneous GPU encodes and OOMing the host. `status='running'` is
/// shared database state, not per-process, so counting it is what makes
/// this a real global cap no matter how many processes are hitting this
/// table at once — and wrapping each pipeline's count+select+update in one
/// `BEGIN IMMEDIATE` transaction (rather than separate statements, and both
/// pipelines in the *same* transaction) is what stops two processes from
/// both reading the same low count and both claiming past the limit before
/// either one's UPDATE lands.
async fn claim_pending_jobs(
    conn: &Arc<Mutex<Connection>>,
    live_action_limit: usize,
    anime_limit: usize,
) -> Result<Vec<ClaimedJob>> {
    let mut conn = conn.lock().await;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let mut claimed = Vec::new();
    for (is_anime_flag, limit) in [(0i64, live_action_limit), (1i64, anime_limit)] {
        let running: i64 = tx.query_row(
            "SELECT count(*) FROM transcode_job WHERE status = 'running' AND is_anime = ?1",
            params![is_anime_flag],
            |row| row.get(0),
        )?;
        let available = (limit as i64 - running).max(0);
        if available == 0 {
            continue;
        }

        // Highest current bitrate first (within this pipeline) — the worst
        // offenders (REMUX, huge season packs) free the most space per
        // file transcoded, so they're worth reaching before smaller,
        // already-reasonable files.
        let mut stmt = tx.prepare(
            "SELECT j.id, j.episode_file_id, ef.path, p.width, p.height, p.duration_secs, j.is_anime, j.force_reencode
             FROM transcode_job j
             JOIN episode_file ef ON ef.id = j.episode_file_id
             JOIN media_file_probe p ON p.episode_file_id = ef.id
             WHERE j.status = 'pending' AND j.is_anime = ?2 AND p.width IS NOT NULL AND p.height IS NOT NULL
             ORDER BY (ef.size_bytes * 8.0 / NULLIF(p.duration_secs, 0)) DESC
             LIMIT ?1",
        )?;
        let batch = stmt
            .query_map(params![available, is_anime_flag], |row| {
                Ok(ClaimedJob {
                    job_id: row.get(0)?,
                    episode_file_id: row.get(1)?,
                    path: PathBuf::from(row.get::<_, String>(2)?),
                    width: row.get(3)?,
                    height: row.get(4)?,
                    duration_secs: row.get(5)?,
                    is_anime: row.get::<_, i64>(6)? != 0,
                    force_reencode: row.get::<_, i64>(7)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for job in &batch {
            tx.execute(
                "UPDATE transcode_job SET status = 'running' WHERE id = ?1",
                params![job.job_id],
            )?;
        }
        claimed.extend(batch);
    }

    tx.commit()?;
    Ok(claimed)
}

/// Finalizes one job under the DB lock: on a real `Encoded` result,
/// atomically swaps the verified temp file over the original, updates
/// `episode_file` (new size + `upgrade_locked = 1`, the flag that keeps the
/// upgrade cycle from ever trying to replace a file breadarr itself just
/// intentionally shrank), and forces a fresh `ensure_probed` so
/// `media_file_probe` reflects the real AV1 ground truth — same shape as
/// `remux_one_backlog_file`. On failure, the original is left completely
/// untouched; the job is marked `failed` with the error recorded, no
/// automatic retry.
///
/// `AlreadyAv1`/`NotBeneficial` from the encode step are both marked `done`/
/// `skipped` respectively with no byte-count change and no swap — neither
/// is a failure, there's just nothing to keep.
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
    encode_result: Result<EncodeOutcome>,
) -> Result<FinalizedJob> {
    let mut conn = conn.lock().await;
    let tmp_path = match &encode_result {
        Ok(EncodeOutcome::Encoded(p)) => Some(p.clone()),
        _ => None,
    };

    let swap: Result<(&'static str, TranscodeOutcome)> = match encode_result {
        Ok(EncodeOutcome::AlreadyAv1) => {
            Ok(("done", TranscodeOutcome { original_bytes: 0, new_bytes: 0 }))
        }
        Ok(EncodeOutcome::NotBeneficial) => {
            Ok(("skipped", TranscodeOutcome { original_bytes: 0, new_bytes: 0 }))
        }
        Ok(EncodeOutcome::Encoded(tmp_path)) => (|| {
            let original_bytes = std::fs::metadata(&job.path)?.len();
            std::fs::rename(&tmp_path, &job.path)?;
            let new_bytes = std::fs::metadata(&job.path)?.len();

            // `size_bytes` and `upgrade_locked` must land together, in one
            // transaction: the rename just above is already irreversible in
            // practice (the temp file is gone), so if these two writes were
            // separate auto-committed statements and the second one failed,
            // the on-disk file would already be the smaller AV1 encode while
            // `upgrade_locked` stayed 0 — leaving it exactly as eligible for
            // the ordinary upgrade cycle to "improve" as any untouched file,
            // silently replacing a deliberate, already-paid-for shrink with
            // a much larger release. Wrapping both in one transaction means
            // either both land or neither does; on failure `swap` returns
            // `Err` below and the job is marked `failed` for a human to
            // notice, rather than quietly losing the lock.
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE episode_file SET size_bytes = ?1, upgrade_locked = 1 WHERE id = ?2",
                params![new_bytes as i64, job.episode_file_id],
            )?;
            tx.execute(
                "DELETE FROM media_file_probe WHERE episode_file_id = ?1",
                params![job.episode_file_id],
            )?;
            tx.commit()?;

            // Best-effort from here: `ensure_probed` only refreshes
            // `media_file_probe`'s descriptive fields (codec/height/etc, fed
            // to `find_backlog_candidates` and library-health reporting) —
            // it already never propagates an *ffprobe* failure (see its own
            // doc comment: "probing must never abort a scan or import"), so
            // an `Err` here means a genuine DB-level error, most likely
            // transient. The facts that actually matter —  the file is now
            // AV1, its new size, and `upgrade_locked` — are already durably
            // committed above; failing the whole job over a stale probe row
            // would misreport a successful transcode as `failed`.
            if let Err(e) = importer::ensure_probed(&conn, job.episode_file_id, &job.path) {
                tracing::warn!(
                    episode_file_id = job.episode_file_id,
                    error = %e,
                    "post-transcode re-probe failed; media_file_probe left stale until the next scan"
                );
            }
            Ok(("done", TranscodeOutcome { original_bytes, new_bytes }))
        })(),
        Err(e) => Err(e),
    };

    match swap {
        Ok((status, outcome)) => {
            conn.execute(
                "UPDATE transcode_job SET status = ?1, new_bytes = ?2, finished_at = datetime('now') WHERE id = ?3",
                params![status, outcome.new_bytes as i64, job.job_id],
            )?;
            Ok(FinalizedJob { status, outcome })
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

struct FinalizedJob {
    status: &'static str,
    outcome: TranscodeOutcome,
}

#[derive(Debug, Default)]
pub struct TranscodeCycleStats {
    pub attempted: usize,
    pub succeeded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes_saved: i64,
}

impl TranscodeCycleStats {
    fn merge(self, other: Self) -> Self {
        TranscodeCycleStats {
            attempted: self.attempted + other.attempted,
            succeeded: self.succeeded + other.succeeded,
            skipped: self.skipped + other.skipped,
            failed: self.failed + other.failed,
            bytes_saved: self.bytes_saved + other.bytes_saved,
        }
    }
}

/// The live-action pipeline's parallelism given how many real Jellyfin
/// transcode sessions are active right now: `budget` (`parallelism_max`,
/// the measured total GPU concurrency ceiling — see its doc comment) minus
/// however many streams Jellyfin itself is actually using, floored at 0.
/// Reserves *exactly* the GPU headroom real viewers need instead of
/// dropping to a flat `parallelism_min` regardless of whether 1 or 5
/// people are watching — the whole point of measuring the real ceiling
/// (rather than guessing) is to spend the difference precisely. Pulled out
/// as its own pure function for direct unit coverage without needing a
/// real (or mocked) `JellyfinClient`.
fn live_action_parallelism_for(budget: usize, active_jellyfin_sessions: usize) -> usize {
    budget.saturating_sub(active_jellyfin_sessions)
}

/// Active parallelism for this cycle, returned as `(live_action, anime)`.
/// Live-action: `live_action_parallelism_for(cfg.parallelism_max,
/// active_sessions)` — dynamically reserves exactly as many GPU streams as
/// Jellyfin is actually using, since Jellyfin transcoding contends for the
/// same encode/decode engines the batch job does. Anime: still a flat
/// `parallelism_max_anime`/`parallelism_min` binary switch on *any* active
/// session — the CPU-bound anime pipeline doesn't contend for the GPU the
/// way live-action does, so it isn't part of this dynamic reservation (per
/// explicit user direction: "for the live action ones"); it still backs
/// off to `parallelism_min` (not eliminated, just conservative) whenever
/// someone's actively watching anything, on the more general theory that a
/// real viewer's experience should never compete with unattended batch
/// work for the box's resources, GPU or not. Only one Jellyfin poll either
/// way, not one per pipeline.
async fn effective_parallelism(jellyfin: Option<&JellyfinClient>, cfg: &TranscodeConfig) -> (usize, usize) {
    let Some(client) = jellyfin else {
        return (cfg.parallelism_max, cfg.parallelism_max_anime);
    };
    match client.active_transcode_sessions().await {
        Ok(active_sessions) => {
            let live_action = live_action_parallelism_for(cfg.parallelism_max, active_sessions);
            let anime = if active_sessions == 0 { cfg.parallelism_max_anime } else { cfg.parallelism_min };
            (live_action, anime)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to poll jellyfin sessions; assuming worst case");
            (cfg.parallelism_min, cfg.parallelism_min)
        }
    }
}

/// One transcode-worker pass across both pipelines. Runs
/// `run_pipeline_cycle` for live-action and anime *concurrently* via
/// `tokio::join!` and merges their stats — see that function's doc
/// comment for why they must not share one claim-spawn-join batch.
///
/// Deliberately `join!`, not `try_join!`: `try_join!` cancels (drops) the
/// other branch's future the instant either one returns `Err`, and dropping
/// a `JoinSet` of `spawn_blocking` encode tasks cannot abort work that has
/// already started — the ffmpeg/SVT-AV1 process keeps running to completion
/// with its result simply discarded, `finalize_job` never runs for it, and
/// its `transcode_job` row is stuck `status='running'` (consuming
/// concurrency budget and leaving its multi-GB temp file on disk) until the
/// daemon restarts. `run_pipeline_cycle` itself now treats a
/// `claim_pending_jobs` error as "claim nothing this iteration" rather than
/// propagating it, so in practice this only guards a pipeline that fails for
/// some other reason — but `join!` means even that failure can no longer
/// take the other, healthy pipeline's in-flight work down with it. Each
/// pipeline's error is logged and treated as an empty cycle rather than
/// failing the whole `run_cycle` call. Shared by both the steady-state
/// daemon ticker and the `transcode-library` backfill CLI, so there's
/// exactly one code path that actually runs an encode.
pub async fn run_cycle(
    conn: Arc<Mutex<Connection>>,
    cfg: TranscodeConfig,
    jellyfin: Option<&JellyfinClient>,
) -> Result<TranscodeCycleStats> {
    let (live_action_result, anime_result) = tokio::join!(
        run_pipeline_cycle(conn.clone(), cfg.clone(), jellyfin, false),
        run_pipeline_cycle(conn.clone(), cfg.clone(), jellyfin, true),
    );
    let live_action_stats = live_action_result.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "live-action transcode pipeline cycle failed");
        TranscodeCycleStats::default()
    });
    let anime_stats = anime_result.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "anime transcode pipeline cycle failed");
        TranscodeCycleStats::default()
    });
    Ok(live_action_stats.merge(anime_stats))
}

/// One pipeline's worker loop: repeatedly claims whatever's currently
/// available for *this* pipeline only (the other pipeline's limit is passed
/// as 0, so `claim_pending_jobs` naturally claims nothing for it), spawns
/// each newly claimed job, and as soon as any one of them finishes,
/// finalizes it and immediately loops back to claim a replacement — rather
/// than waiting for the whole batch to drain before claiming more. Exits
/// once a claim attempt comes back empty and nothing is left in flight.
///
/// This replaces an earlier design where one shared claim+spawn+join batch
/// covered both pipelines at once: claimed jobs from both pipelines were
/// joined together, so the *whole* batch — and therefore any new claim for
/// either pipeline — blocked until every job in it finished. A single long
/// anime file (feature-length movies can take over an hour via the
/// CPU-bound SVT-AV1 path) would leave the GPU-bound live-action pipeline
/// sitting completely idle for that entire duration despite having its own
/// independent concurrency budget and a large backlog of its own —
/// discovered in production: 512 pending live-action jobs and a budget of 7,
/// but the GPU sat unused for the better part of an hour waiting on one
/// anime movie to join. Running each pipeline as its own independent
/// replenish-loop, joined concurrently rather than sequentially, means
/// neither pipeline's throughput is ever gated by the other's job durations.
///
/// Uses `JoinSet::join_next_with_id` rather than awaiting a `Vec` of handles
/// in order: a real validation run surfaced that the naive
/// awaited-in-claim-order approach leaves a fast job's already-finished
/// result (verified output sitting on disk, or even a fast failure) fully
/// idle — not written back to the DB, not freeing its slot for the next
/// claim — for as long as whichever job happens to be *earlier* in claim
/// order (highest-bitrate-first, so often the largest file) keeps running.
/// `join_next_with_id`'s `(Id, T)`/`JoinError::id()` pairing is what lets a
/// panicked task still be matched back to its `ClaimedJob` (a panic doesn't
/// get to return the job alongside its result), which a plain
/// `JoinSet<Result<EncodeOutcome>>` with the job moved into the closure
/// wouldn't allow.
async fn run_pipeline_cycle(
    conn: Arc<Mutex<Connection>>,
    cfg: TranscodeConfig,
    jellyfin: Option<&JellyfinClient>,
    is_anime: bool,
) -> Result<TranscodeCycleStats> {
    let mut stats = TranscodeCycleStats::default();
    let mut set = tokio::task::JoinSet::new();
    let mut jobs_by_task_id: std::collections::HashMap<tokio::task::Id, ClaimedJob> =
        std::collections::HashMap::new();

    loop {
        let (live_action_parallelism, anime_parallelism) = effective_parallelism(jellyfin, &cfg).await;
        let (live_action_limit, anime_limit) = if is_anime {
            (0, anime_parallelism)
        } else {
            (live_action_parallelism, 0)
        };
        // A claim failure (e.g. transient SQLITE_BUSY under `busy_timeout`
        // when another process holds the write lock) must not propagate out
        // of this loop: that would return `Err` from `run_pipeline_cycle`,
        // and in `run_cycle` that used to cancel the *other* pipeline's
        // future mid-flight via `try_join!`, orphaning its already-running
        // encodes (see `run_cycle`'s doc comment). Skip claiming new work
        // this iteration instead — whatever's already in `set` keeps
        // draining normally, and the next tick retries the claim.
        let claimed = match claim_pending_jobs(&conn, live_action_limit, anime_limit).await {
            Ok(claimed) => claimed,
            Err(e) => {
                tracing::warn!(error = %e, is_anime, "failed to claim pending transcode jobs this cycle; will retry next iteration");
                Vec::new()
            }
        };

        for job in claimed {
            let cfg = cfg.clone();
            let path = job.path.clone();
            let job_id = job.job_id;
            let (width, height, duration, job_is_anime, force_reencode) =
                (job.width, job.height, job.duration_secs, job.is_anime, job.force_reencode);
            let abort_handle = set.spawn_blocking(move || {
                encode_and_verify(path, job_id, cfg, width, height, duration, job_is_anime, force_reencode)
            });
            jobs_by_task_id.insert(abort_handle.id(), job);
        }

        // `None` here means this claim found nothing new *and* nothing from
        // an earlier claim is still in flight — this pipeline's queue is
        // genuinely empty for now.
        let Some(joined) = set.join_next_with_id().await else {
            break;
        };

        stats.attempted += 1;
        let (task_id, encode_result) = match joined {
            Ok((task_id, result)) => (task_id, result),
            Err(join_err) => {
                let task_id = join_err.id();
                (task_id, Err(anyhow::anyhow!("encode task panicked: {join_err}")))
            }
        };
        let job = jobs_by_task_id
            .remove(&task_id)
            .expect("every spawned task's id was inserted before the task could complete");

        match finalize_job(&conn, job, encode_result).await {
            Ok(finalized) => {
                if finalized.status == "skipped" {
                    stats.skipped += 1;
                } else {
                    stats.succeeded += 1;
                }
                stats.bytes_saved +=
                    finalized.outcome.original_bytes as i64 - finalized.outcome.new_bytes as i64;
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
            parallelism_max_anime: 2,
            reference_bitrate_kbps: 5320,
            reference_height: 1080,
            av1_efficiency_factor: 0.7,
            exclude_hdr: true,
            exclude_min_height: 2000,
            quality_live_action: 26,
            anime_root_folders: vec!["/mnt/media/Anime".to_string(), "/mnt/media/Anime Movies".to_string()],
            quality_anime: 24,
            anime_svtav1_preset: 10,
            anime_svtav1_max_threads: 2,
            min_size_reduction_pct: 0.10,
            skip_below_ceiling_ratio: 0.5,
            verify_sample_secs: 20.0,
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
        // Asking for a total of 3 concurrent live-action, with 2 already
        // running — should claim exactly 1 more, not 3 more. All seeded
        // jobs default to is_anime=0 (live-action), so anime_limit=0 here
        // is correct, not a placeholder.
        let claimed = claim_pending_jobs(&conn, 3, 0).await.unwrap();
        assert_eq!(claimed.len(), 1);

        let conn = conn.lock().await;
        let running: i64 = conn
            .query_row("SELECT count(*) FROM transcode_job WHERE status = 'running'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(running, 3, "total in-flight jobs must never exceed the requested cap");
    }

    // Regression test for a real gap found in review: a file whose encode
    // completed but was rejected by `is_beneficial` (recorded `skipped` by
    // `finalize_job`) was not in `find_backlog_candidates`'s exclusion list,
    // so every re-run of the `transcode-library` backfill re-selected it,
    // paying the full GPU/CPU encode cost again only to reject it again.
    #[test]
    fn find_backlog_candidates_excludes_skipped_jobs() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        seed_file(&conn, 1, 1_000_000_000);
        conn.execute(
            "INSERT INTO transcode_job (episode_file_id, status, queued_at) VALUES (1, 'skipped', datetime('now'))",
            [],
        )
        .unwrap();

        let candidates = find_backlog_candidates(&conn, &cfg()).unwrap();
        assert!(
            candidates.is_empty(),
            "a file with a 'skipped' (not-beneficial) job must not be re-selected for backfill"
        );
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
        let claimed = claim_pending_jobs(&conn, 2, 0).await.unwrap();
        assert!(claimed.is_empty(), "already at the cap — must not claim more");
    }

    // Regression test for a real gap: raising the live-action cap for a
    // validated GPU-scaling reason must not silently also raise anime
    // concurrency (CPU-bound, thread-hungry, never load-tested at higher
    // counts) — the two caps are enforced completely independently.
    #[tokio::test]
    async fn claim_pending_jobs_enforces_live_action_and_anime_caps_independently() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        for id in 1..=6 {
            seed_file(&conn, id, 1_000_000_000);
        }
        // 3 live-action pending (ids 1-3), 3 anime pending (ids 4-6).
        for id in 1..=3 {
            conn.execute(
                "INSERT INTO transcode_job (episode_file_id, status, is_anime, queued_at) VALUES (?1, 'pending', 0, datetime('now'))",
                params![id],
            )
            .unwrap();
        }
        for id in 4..=6 {
            conn.execute(
                "INSERT INTO transcode_job (episode_file_id, status, is_anime, queued_at) VALUES (?1, 'pending', 1, datetime('now'))",
                params![id],
            )
            .unwrap();
        }

        let conn = Arc::new(Mutex::new(conn));
        // A generous live-action cap (6, matching the real raised default)
        // alongside a conservative anime cap (2) must claim up to 6
        // live-action jobs (only 3 exist) but no more than 2 anime jobs,
        // even though 3 anime jobs are pending and the anime cap is far
        // below the live-action one.
        let claimed = claim_pending_jobs(&conn, 6, 2).await.unwrap();
        let live_action_claimed = claimed.iter().filter(|j| !j.is_anime).count();
        let anime_claimed = claimed.iter().filter(|j| j.is_anime).count();
        assert_eq!(live_action_claimed, 3, "all 3 pending live-action jobs should be claimed");
        assert_eq!(anime_claimed, 2, "anime claims must stay capped at 2 regardless of the live-action cap");
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

    // Regression test for a real gap found in review: `ensure_probed`
    // inserts a `probe_failed` row with NULL codec/height on ffprobe
    // failure rather than leaving no row at all, so `maybe_enqueue_transcode`'s
    // "no probe row -> skip" guard never fires for it. Without this check,
    // `should_enqueue` would return true for that NULL/NULL row, `enqueue`
    // would insert a job `claim_pending_jobs` can never select (it requires
    // `p.width`/`p.height IS NOT NULL`), and the unique partial index on
    // (pending, running) would then permanently block any future, real
    // enqueue for the file once probing eventually succeeds.
    #[test]
    fn should_enqueue_rejects_missing_codec_or_height() {
        let c = cfg();
        assert!(!should_enqueue(None, false, None, &c), "no codec, no height -> never enqueue");
        assert!(!should_enqueue(None, false, Some(1080), &c), "missing codec alone must block enqueue");
        assert!(!should_enqueue(Some("hevc"), false, None, &c), "missing height alone must block enqueue");
        assert!(should_enqueue(Some("hevc"), false, Some(1080), &c), "codec+height present -> normal eligibility rules apply");
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

        let result =
            encode_and_verify(clip.clone(), 999, cfg(), 320, 240, Some(1.0), false, false).unwrap();
        assert!(
            matches!(result, EncodeOutcome::AlreadyAv1),
            "an already-AV1 file must not be re-encoded"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Regression test for a real gap found in review: the temp path used to
    // be produced via `input.with_extension("job{id}.av1.mkv")`, so a
    // multi-hour in-progress encode sat in the library as a normal-looking
    // `.mkv` file — `collect_video_files`/`walk_files` (both filter purely
    // on `VIDEO_EXTS`) would pick it up as a second candidate video file for
    // the same episode, and `library_scan`/Jellyfin could index or attempt
    // to play a still-encoding file. The fix must produce a path whose
    // final extension isn't in `VIDEO_EXTS` at all, still under the same
    // parent directory (required for the atomic rename-based swap), and
    // still job-id-scoped (so two jobs on the same file never collide).
    #[test]
    fn temp_path_for_is_not_picked_up_by_video_file_scans() {
        let input = Path::new("/library/Show/Season 01/Show - S01E01 - Title.mkv");
        let tmp = temp_path_for(input, 42);

        assert_eq!(tmp.parent(), input.parent(), "must stay on the same filesystem/directory as the original");
        let ext = tmp.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        assert!(
            !crate::importer::VIDEO_EXTS.contains(&ext.as_str()),
            "temp path {tmp:?} has a video extension and would be picked up by library scans"
        );
        assert!(
            tmp.to_string_lossy().contains("job42"),
            "must stay job-id-scoped so two jobs on the same file can never collide"
        );

        // Different job ids on the same input must never collide.
        let other = temp_path_for(input, 43);
        assert_ne!(tmp, other);
    }

    // Fast, deterministic regression test for the actual root cause of a
    // real incident: flat-bitrate rate control on `av1_vaapi` produced
    // outputs *larger* than the original on 476 of 700 backfilled files.
    // This is the exact arithmetic that must reject that outcome regardless
    // of how any encoder's rate control behaves.
    #[test]
    fn is_beneficial_rejects_growth_and_marginal_shrinkage() {
        // The real incident's shape: ~2.1GB HEVC -> ~4.9GB "AV1".
        assert!(!is_beneficial(2_100_000_000, 4_900_000_000, 0.10));
        // Only a 5% reduction — below the 10% floor.
        assert!(!is_beneficial(1_000_000_000, 950_000_000, 0.10));
        // A real win: 15% smaller, clears the floor.
        assert!(is_beneficial(1_000_000_000, 850_000_000, 0.10));
        // Exactly at the floor — inclusive.
        assert!(is_beneficial(1_000_000_000, 900_000_000, 0.10));
    }

    // The dynamic Jellyfin-reservation formula: reserve exactly as many
    // GPU streams as Jellyfin is actually using out of the measured total
    // budget, rather than dropping to a flat minimum regardless of viewer
    // count.
    #[test]
    fn live_action_parallelism_for_reserves_exactly_what_jellyfin_is_using() {
        assert_eq!(live_action_parallelism_for(7, 0), 7, "no active viewers — use the full budget");
        assert_eq!(live_action_parallelism_for(7, 1), 6, "one viewer — reserve exactly one stream");
        assert_eq!(live_action_parallelism_for(7, 3), 4, "three viewers — reserve exactly three streams");
        assert_eq!(
            live_action_parallelism_for(7, 9),
            0,
            "more active viewers than the whole budget — batch gets nothing, never negative"
        );
    }

    // Regression test for the second half of the same incident: several of
    // the 199 failures were files (some anime, notably) already efficient
    // enough that transcoding them was never going to help — this is the
    // pre-check that skips the encode attempt entirely rather than burning
    // GPU/CPU time to find that out the slow way. No real ffmpeg encode
    // should run here — a huge fabricated duration forces the source
    // bitrate calculation near zero regardless of resolution, so this stays
    // fast and deterministic.
    #[test]
    fn encode_and_verify_skips_pre_emptively_when_source_is_already_efficient() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-already-efficient-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, "libx264");

        let result =
            encode_and_verify(clip.clone(), 1, cfg(), 1920, 1080, Some(100_000.0), false, false).unwrap();
        assert!(
            matches!(result, EncodeOutcome::NotBeneficial),
            "an already-efficient source must be skipped before spending encode time"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn generate_lossless_test_clip(dir: &Path) -> PathBuf {
        let path = dir.join("clip_lossless.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=640x480:duration=2:rate=15"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-qp", "0"])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a lossless test clip");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate a lossless test clip: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    // Reproduces the real "embedded cover art" incident: a second video
    // stream with `attached_pic` disposition (a still image, ffmpeg reports
    // it as a normal `video`-type stream) alongside the real content stream.
    // The old blanket `-map 0` tried to re-encode this too, which the
    // VAAPI/libsvtav1 encoders can't handle — failed the whole job on
    // several real library files (GoT, Avatar).
    fn generate_clip_with_attached_pic(dir: &Path) -> PathBuf {
        let path = dir.join("clip_with_cover_art.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=640x480:duration=2:rate=15"])
            .args(["-f", "lavfi", "-i", "color=c=red:size=64x64:duration=1"])
            .args(["-map", "0:v", "-map", "1:v"])
            .args(["-c:v:0", "libx264", "-preset", "ultrafast", "-qp", "0"])
            .args(["-c:v:1", "png"])
            .args(["-disposition:v:1", "attached_pic"])
            .arg("-shortest")
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a test clip with attached cover art");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate a test clip with attached cover art: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    // Reproduces the real "mov_text subtitle" incident: mp4 sources using
    // mov_text (mp4's own timed-text codec) aren't valid inside the
    // Matroska container this pipeline always writes, and stream-copying
    // them failed the whole job outright.
    fn generate_clip_with_mov_text_subtitle(dir: &Path) -> PathBuf {
        let srt_path = dir.join("sub.srt");
        std::fs::write(&srt_path, "1\n00:00:00,000 --> 00:00:01,000\nTest subtitle\n").unwrap();

        let path = dir.join("clip_with_mov_text.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=640x480:duration=2:rate=15"])
            .arg("-i")
            .arg(&srt_path)
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-qp", "0"])
            .args(["-c:s", "mov_text"])
            .args(["-f", "mp4"])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a test clip with a mov_text subtitle");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate a test clip with a mov_text subtitle: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    #[test]
    fn subtitle_codec_args_converts_only_mov_text() {
        let subs = vec![
            ffprobe::SubtitleStream { language: Some("eng".to_string()), codec: Some("mov_text".to_string()) },
            ffprobe::SubtitleStream { language: Some("eng".to_string()), codec: Some("ass".to_string()) },
            ffprobe::SubtitleStream { language: None, codec: None },
        ];
        let args = subtitle_codec_args(&subs);
        assert_eq!(
            args,
            vec![
                "-c:s:0".to_string(), "srt".to_string(),
                "-c:s:1".to_string(), "copy".to_string(),
                "-c:s:2".to_string(), "copy".to_string(),
            ]
        );
    }

    // End-to-end regression test for the real "embedded cover art" incident
    // — an attached-pic stream must no longer crash the whole encode.
    // Uses the anime (software) pipeline since it needs no VAAPI hardware.
    #[test]
    fn encode_and_verify_handles_a_source_with_attached_cover_art() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-attached-pic-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_clip_with_attached_pic(&dir);

        let mut c = cfg();
        c.skip_below_ceiling_ratio = 0.0; // force past the pre-check for this test
        let result = encode_and_verify(clip.clone(), 3, c, 640, 480, Some(2.0), true, false).unwrap();
        match result {
            EncodeOutcome::Encoded(tmp_path) => {
                assert!(tmp_path.exists());
                let _ = std::fs::remove_file(&tmp_path);
            }
            other => panic!("expected a beneficial encode despite the attached cover art, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // End-to-end regression test for the real "mov_text subtitle" incident
    // — a mov_text track must be converted rather than crashing the encode.
    #[test]
    fn encode_and_verify_handles_a_source_with_a_mov_text_subtitle() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-mov-text-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_clip_with_mov_text_subtitle(&dir);

        let mut c = cfg();
        c.skip_below_ceiling_ratio = 0.0;
        let result = encode_and_verify(clip.clone(), 4, c, 640, 480, Some(2.0), true, false).unwrap();
        match result {
            EncodeOutcome::Encoded(tmp_path) => {
                assert!(tmp_path.exists());
                let _ = std::fs::remove_file(&tmp_path);
            }
            other => panic!("expected a beneficial encode despite the mov_text subtitle, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // End-to-end sanity check for the anime pipeline specifically: a real
    // `libsvtav1` 10-bit encode of a deliberately bloated (lossless x264)
    // source should produce a verified, meaningfully smaller output. Doesn't
    // need real VAAPI hardware (unlike the live-action path) since
    // `libsvtav1` is software — safe to run in any dev/CI environment that
    // has an AV1-capable ffmpeg build.
    #[test]
    fn encode_and_verify_anime_pipeline_shrinks_a_bloated_source() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-anime-e2e-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_lossless_test_clip(&dir);

        let mut c = cfg();
        c.skip_below_ceiling_ratio = 0.0; // force past the pre-check for this test
        let result = encode_and_verify(clip.clone(), 2, c, 640, 480, Some(2.0), true, false).unwrap();
        match result {
            EncodeOutcome::Encoded(tmp_path) => {
                assert!(tmp_path.exists());
                let _ = std::fs::remove_file(&tmp_path);
            }
            other => panic!("expected a beneficial encode of a lossless source, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Regression test for the remediation path: `force_reencode: true` must
    // bypass the normal "already AV1 -> nothing to do" short-circuit and
    // attempt a real re-encode — this is the whole point of
    // `find_oversized_av1_candidates`'s jobs, which target files that are
    // already AV1 but were left larger than their original by a real
    // rate-control bug. Uses a genuinely oversized AV1 source (a lossless
    // x264 clip re-encoded to AV1 at a high bitrate, deliberately bigger
    // than what a sane target would produce) so the *second* pass — the one
    // under test — has real room to shrink it.
    #[test]
    fn encode_and_verify_force_reencode_bypasses_the_already_av1_short_circuit() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-force-reencode-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let oversized_av1 = dir.join("oversized.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=640x480:duration=2:rate=15"])
            .args(["-c:v", "libsvtav1", "-crf", "10", "-preset", "12"]) // deliberately high quality/bitrate
            .arg(&oversized_av1)
            .output()
            .expect("failed to run ffmpeg to generate an oversized AV1 test clip");
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));

        // Sanity check: without force_reencode, this must still short-circuit.
        let unforced =
            encode_and_verify(oversized_av1.clone(), 5, cfg(), 640, 480, Some(2.0), true, false).unwrap();
        assert!(matches!(unforced, EncodeOutcome::AlreadyAv1));

        // is_anime: true — routes through the software libsvtav1 path, same
        // as the other end-to-end tests here, since this dev/CI environment
        // has no VAAPI hardware for the live-action path to use.
        let mut c = cfg();
        c.skip_below_ceiling_ratio = 0.0;
        let forced =
            encode_and_verify(oversized_av1.clone(), 6, c, 640, 480, Some(2.0), true, true).unwrap();
        match forced {
            EncodeOutcome::Encoded(tmp_path) => {
                let new_bytes = std::fs::metadata(&tmp_path).unwrap().len();
                let original_bytes = std::fs::metadata(&oversized_av1).unwrap().len();
                assert!(
                    new_bytes < original_bytes,
                    "forced re-encode should shrink an oversized AV1 source"
                );
                let _ = std::fs::remove_file(&tmp_path);
            }
            other => panic!("expected force_reencode to produce a real, smaller encode, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn is_anime_path_matches_configured_root_folders() {
        let c = cfg();
        assert!(is_anime_path(
            Path::new("/mnt/media/Anime/Attack on Titan (2013)/Season 02/ep.mkv"),
            &c
        ));
        assert!(is_anime_path(
            Path::new("/mnt/media/Anime Movies/Bardock (1990)/movie.mkv"),
            &c
        ));
        assert!(!is_anime_path(
            Path::new("/mnt/media/TV Shows/The Expanse (2015)/ep.mkv"),
            &c
        ));
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

    // Regression test for a real latency bug a live validation run
    // surfaced: `run_cycle` used to await claimed jobs' handles in claim
    // order (highest-bitrate-first), so a fast job's already-finished
    // result sat idle — not written back to the DB — for as long as
    // whichever job happened to come *first* in that order kept running.
    // The fix (`JoinSet::join_next_with_id`) finalizes whichever job
    // actually finishes first, which raises a sharper risk worth its own
    // direct test: a job's result must land on *that job's* DB row, not
    // get cross-attributed to a different concurrently-running job via a
    // wrong task-id lookup. This seeds one job that's claimed first (by
    // this test's bitrate ordering) but takes measurably longer, and one
    // claimed second that finishes near-instantly, and confirms each
    // outcome lands on the correct `episode_file`/`transcode_job` row
    // regardless of which one the JoinSet actually resolves first.
    #[tokio::test]
    async fn run_cycle_attributes_each_result_to_the_correct_job_even_out_of_claim_order() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-transcode-run-cycle-attribution-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Claimed first (much higher bitrate) but genuinely slower — a
        // real (tiny) SVT-AV1 encode, not a short-circuit.
        let slow_clip = generate_lossless_test_clip(&dir);
        let slow_id = 101i64;
        // Claimed second (lower bitrate) but resolves almost immediately —
        // already AV1, hits the `AlreadyAv1` short-circuit with no real
        // encode at all.
        let fast_clip = generate_test_clip(&dir, "libsvtav1");
        let fast_id = 102i64;

        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        for (id, path, codec, size_bytes, duration) in [
            (slow_id, &slow_clip, "h264", 50_000_000i64, 2.0),
            (fast_id, &fast_clip, "av1", 1_000_000i64, 1.0),
        ] {
            conn.execute(
                "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes)
                 VALUES (?1, NULL, NULL, ?2, ?3)",
                params![id, path.to_string_lossy().to_string(), size_bytes],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime,
                                                duration_secs, video_codec, width, height)
                 VALUES (?1, datetime('now'), ?2, 0, ?3, ?4, 640, 480)",
                params![id, size_bytes, duration, codec],
            )
            .unwrap();
        }
        // Bitrate-descending claim order puts the slow job first: its size
        // is set so `size_bytes*8/duration` comfortably exceeds the fast
        // job's, matching the real incident's shape (a big, slow file
        // claimed ahead of a small, fast one).
        conn.execute(
            "INSERT INTO transcode_job (episode_file_id, status, is_anime, queued_at) VALUES (?1, 'pending', 1, datetime('now'))",
            params![slow_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcode_job (episode_file_id, status, is_anime, queued_at) VALUES (?1, 'pending', 0, datetime('now'))",
            params![fast_id],
        )
        .unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let mut c = cfg();
        c.skip_below_ceiling_ratio = 0.0; // don't pre-skip the slow job
        let stats = run_cycle(conn.clone(), c, None).await.unwrap();
        assert_eq!(stats.attempted, 2);
        assert_eq!(stats.failed, 0);

        let conn = conn.lock().await;
        let (slow_status, slow_path): (String, String) = conn
            .query_row(
                "SELECT tj.status, ef.path FROM transcode_job tj JOIN episode_file ef ON ef.id = tj.episode_file_id WHERE tj.episode_file_id = ?1",
                params![slow_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (fast_status, fast_new_bytes, fast_path): (String, i64, String) = conn
            .query_row(
                "SELECT tj.status, tj.new_bytes, ef.path FROM transcode_job tj JOIN episode_file ef ON ef.id = tj.episode_file_id WHERE tj.episode_file_id = ?1",
                params![fast_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(slow_status, "done", "the real encode must land on the slow job's own row");
        assert_eq!(slow_path, slow_clip.to_string_lossy(), "must not cross-attribute the fast job's path");
        assert_eq!(fast_status, "done", "the already-AV1 short-circuit must land on the fast job's own row");
        assert_eq!(fast_new_bytes, 0, "AlreadyAv1 records no byte-count change");
        assert_eq!(fast_path, fast_clip.to_string_lossy(), "must not cross-attribute the slow job's path");

        // Regression coverage for a real gap found in review: `finalize_job`
        // must set `upgrade_locked` in the same transaction as the new
        // `size_bytes`, and a best-effort re-probe afterward must still
        // leave `media_file_probe` correctly reflecting the swapped-in AV1
        // file — not the stale pre-encode codec, and not silently unset
        // just because that refresh is no longer allowed to fail the job.
        let (upgrade_locked, probed_codec): (i64, Option<String>) = conn
            .query_row(
                "SELECT ef.upgrade_locked, p.video_codec FROM episode_file ef
                 LEFT JOIN media_file_probe p ON p.episode_file_id = ef.id
                 WHERE ef.id = ?1",
                params![slow_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(upgrade_locked, 1, "a real encode must lock the file against the ordinary upgrade cycle");
        assert_eq!(probed_codec.as_deref(), Some("av1"), "the post-swap re-probe must reflect the new AV1 file, not stale pre-encode data");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
