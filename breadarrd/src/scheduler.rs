use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::matcher::{self, MatchOutcome, TitleMatcher};
use crate::metadata::anime_map;
use crate::parser::{self, ParsedRelease};
use crate::qbit::QbitClient;
use crate::scoring::{self, GateContext, GateResult, ProfileKind, QualityProfile};
use crate::sources::{self, RawReleaseItem, ReleaseSource};

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessOutcome {
    Grabbed {
        media_item_id: i64,
        episode_id: Option<i64>,
        /// Set when this grab was a season pack — lets a search-cycle
        /// caller recognize that a single-episode target got satisfied
        /// incidentally by a whole-season grab, not just by an exact
        /// `episode_id` match.
        season_number: Option<u32>,
        score: f32,
    },
    QueuedForReview,
    NoMatch,
    CouldNotResolveEpisode,
    NotMonitoredOrAlreadyHave,
    GateRejected(scoring::RejectReason),
    NotBetterThanExisting,
    /// Movie-only: the release's own parsed year and the monitored movie's
    /// year differ by more than one — the TV path's equivalent guard is
    /// season/episode resolution; movies have no such structural signal, so
    /// year is the only cheap defense against a same-named-but-wrong film
    /// (e.g. "Dune" 1984 vs 2021).
    YearMismatch,
    /// qBittorrent rejected the magnet outright (a dead hash, most likely —
    /// a source's own seeder count can be stale). Deliberately an `Ok`
    /// outcome, not a propagated `Err`: unlike a transient network failure,
    /// the exact same hash will reject identically forever, so this needs
    /// to be marked seen and moved past rather than retried — treating it
    /// as a generic error once caused an infinite retry loop on a single
    /// dead candidate (verified live).
    MagnetRejected,
    /// The torrent was (most likely) actually added to qBittorrent, but its
    /// hash couldn't be correlated back — recorded as a `failed` release
    /// (not `grabbed`) so it doesn't block the episode/movie from being
    /// re-searched forever (a `grabbed`-with-NULL-hash row is invisible to
    /// the importer's `torrent_hash IS NOT NULL` filter and never gets
    /// cleaned up any other way — verified live as a real, permanent stuck
    /// state before this was added).
    HashCaptureFailed,
    /// The release's title carries an explicit non-video format marker
    /// (ebook, audiobook, comic) — a source returning something that merely
    /// shares a monitored show/movie's title text, not an actual episode or
    /// film. Checked before title-matching so these never reach the review
    /// queue at all (verified live: nyaa's RSS feed and TPB/1337x search
    /// both surface this — a general torrent search or an unscoped feed has
    /// no concept of "only video releases", so an ebook titled after a show
    /// embeds just as confidently as a real episode would).
    NonVideoFormat,
}

struct MediaItemRow {
    id: i64,
    kind: String,
    tvdb_id: Option<i64>,
    year: Option<i64>,
    quality_profile_id: i64,
}

fn get_media_item(conn: &Connection, id: i64) -> Result<MediaItemRow> {
    conn.query_row(
        "SELECT id, kind, tvdb_id, year, quality_profile_id FROM media_item WHERE id = ?1",
        params![id],
        |row| {
            Ok(MediaItemRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                tvdb_id: row.get(2)?,
                year: row.get(3)?,
                quality_profile_id: row.get(4)?,
            })
        },
    )
    .map_err(Into::into)
}

/// Reads `quality_profile.weights` for `quality_profile_id` and applies it
/// on top of the built-in defaults for `kind` (see
/// `QualityProfile::with_weights_override`). A missing row (a dangling
/// `quality_profile_id` shouldn't happen given the FK constraint, but this
/// is scoring, not a place to ever hard-fail a grab cycle over it) falls
/// back to exactly the hardcoded defaults, same as malformed JSON does.
fn load_quality_profile(
    conn: &Connection,
    quality_profile_id: i64,
    kind: ProfileKind,
) -> Result<QualityProfile> {
    let weights_json: Option<String> = conn
        .query_row(
            "SELECT weights FROM quality_profile WHERE id = ?1",
            params![quality_profile_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(QualityProfile::with_weights_override(
        kind,
        weights_json.as_deref().unwrap_or(""),
    ))
}

/// True when a release's parsed episode markers indicate it's actually an
/// episode of some show, not the movie whose title it matched.
fn looks_like_episode(parsed: &ParsedRelease) -> bool {
    parsed.season.is_some() || parsed.episode.is_some() || parsed.absolute_episode.is_some()
}

/// True when a release's parsed markers indicate a whole-season or
/// multi-episode batch torrent rather than a single episode — season
/// known, episode/absolute-episode deliberately left unresolved by the
/// parser (see `parser::tokens::looks_like_episode_range`'s doc comment
/// for why guessing one episode out of a batch is actively harmful).
fn looks_like_season_pack(parsed: &ParsedRelease) -> bool {
    parsed.season.is_some() && parsed.episode.is_none() && parsed.absolute_episode.is_none()
}

/// Sort key for a batch of raw search results: season packs first (a pack
/// clears every missing episode in one grab instead of one at a time, and
/// each still goes through the normal seeder/quality gate in `process_item`
/// — a pack with too few seeders is rejected there and iteration just falls
/// through to the next candidate, so this never trades a viable single
/// episode for an unviable pack), highest-seeded first within each group.
fn release_sort_key(item: &RawReleaseItem) -> (std::cmp::Reverse<bool>, std::cmp::Reverse<u32>) {
    let is_pack = looks_like_season_pack(&parser::parse(&item.title));
    (
        std::cmp::Reverse(is_pack),
        std::cmp::Reverse(item.seeders.unwrap_or(0)),
    )
}

/// True when a release's raw title carries an explicit non-video format
/// marker — an ebook, audiobook, or comic that happens to share a
/// monitored show/movie's title text, not an actual episode or film. A
/// plain substring check on the raw (unparsed) title, deliberately upstream
/// of the embedding matcher: these aren't "low-confidence" matches needing
/// human review, they're not video releases at all, and title-similarity
/// alone can't tell the difference (the whole point of a shared title).
fn looks_like_non_video_format(title: &str) -> bool {
    const NON_VIDEO_MARKERS: &[&str] = &[
        "EPUB",
        "MOBI",
        "AZW",
        "PDF",
        "CBR",
        "CBZ",
        "DJVU",
        "AUDIOBOOK",
        "LIGHT NOVEL",
        "M4B",
    ];
    let upper = title.to_uppercase();
    NON_VIDEO_MARKERS.iter().any(|m| upper.contains(m))
}

/// True when both years are known and differ by more than one — the
/// movie-world guard against a same-named-but-wrong film (e.g. "Dune" 1984
/// vs. 2021). `None` on either side means "not enough signal to reject."
fn movie_year_mismatch(parsed_year: Option<u32>, media_item_year: Option<i64>) -> bool {
    match (parsed_year, media_item_year) {
        (Some(want), Some(have)) => want.abs_diff(have as u32) > 1,
        _ => false,
    }
}

/// Movie counterpart to `find_monitored_missing_episode`: true when the
/// movie is monitored, has no file yet, and isn't already mid-grab. Movies
/// have no per-item "monitored" row the way episodes do (that lives on
/// `media_item` itself), and no upgrade-after-file path — once a file
/// exists this permanently returns false, same as the TV path's `has_file`
/// check.
fn movie_needs_grab(conn: &Connection, media_item_id: i64) -> Result<bool> {
    let monitored: i64 = conn.query_row(
        "SELECT monitored FROM media_item WHERE id = ?1",
        params![media_item_id],
        |row| row.get(0),
    )?;
    if monitored == 0 {
        return Ok(false);
    }
    let has_file: i64 = conn.query_row(
        "SELECT count(*) FROM episode_file WHERE media_item_id = ?1 AND episode_id IS NULL",
        params![media_item_id],
        |row| row.get(0),
    )?;
    if has_file > 0 {
        return Ok(false);
    }
    let in_flight: i64 = conn.query_row(
        "SELECT count(*) FROM release WHERE media_item_id = ?1 AND episode_id IS NULL
         AND status IN ('grabbed','downloading')",
        params![media_item_id],
        |row| row.get(0),
    )?;
    Ok(in_flight == 0)
}

/// Upgrade-search counterpart to `movie_needs_grab`: same monitored/
/// not-in-flight checks, but deliberately has no `has_file` gate — the
/// entire point of an upgrade check is to run against a movie that already
/// has a file, so this is eligibility for a *re-grab*, not a first grab.
fn movie_eligible_for_upgrade(conn: &Connection, media_item_id: i64) -> Result<bool> {
    let monitored: i64 = conn.query_row(
        "SELECT monitored FROM media_item WHERE id = ?1",
        params![media_item_id],
        |row| row.get(0),
    )?;
    if monitored == 0 {
        return Ok(false);
    }
    let in_flight: i64 = conn.query_row(
        "SELECT count(*) FROM release WHERE media_item_id = ?1 AND episode_id IS NULL
         AND status IN ('grabbed','downloading')",
        params![media_item_id],
        |row| row.get(0),
    )?;
    Ok(in_flight == 0)
}

fn is_anime(conn: &Connection, tvdb_id: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM anime_mapping WHERE tvdb_id = ?1",
        params![tvdb_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Resolves a parsed release to a (season, episode) pair: directly if the
/// title carried explicit season/episode numbers, or via the anime absolute-
/// numbering map when it only carried an absolute episode number. `pub(crate)`
/// so `library_scan` can reuse it for fansub-named files already on disk
/// (e.g. "[SubsPlease] Show - 05.mkv", absolute numbering only, no season
/// marker at all) instead of duplicating the same anime_map fallback.
pub(crate) fn resolve_episode(
    conn: &Connection,
    tvdb_id: Option<i64>,
    parsed: &ParsedRelease,
) -> Result<Option<(u32, u32)>> {
    if let (Some(season), Some(episode)) = (parsed.season, parsed.episode) {
        return Ok(Some((season, episode)));
    }
    if let (Some(absolute), Some(tvdb_id)) = (parsed.absolute_episode, tvdb_id) {
        return anime_map::resolve_absolute_episode(conn, tvdb_id, absolute);
    }
    Ok(None)
}

/// Looks up an episode by identity alone (no monitored/has_file
/// eligibility filtering) — used by the season-pack importer, which is
/// matching "what episode is this actual file" rather than "is this worth
/// grabbing." `pub(crate)` so `importer` can reuse it instead of
/// duplicating the same query.
pub(crate) fn find_episode_id(
    conn: &Connection,
    media_item_id: i64,
    season: u32,
    episode: u32,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM episode WHERE media_item_id = ?1 AND season_number = ?2 AND episode_number = ?3",
        params![media_item_id, season, episode],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn find_monitored_missing_episode(
    conn: &Connection,
    media_item_id: i64,
    season: u32,
    episode: u32,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM episode WHERE media_item_id = ?1 AND season_number = ?2
         AND episode_number = ?3 AND monitored = 1 AND has_file = 0",
        params![media_item_id, season, episode],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Upgrade-search counterpart to `find_monitored_missing_episode`: same
/// monitored check, but deliberately has no `has_file` gate — see
/// `movie_eligible_for_upgrade`'s doc comment for why.
fn find_monitored_episode(
    conn: &Connection,
    media_item_id: i64,
    season: u32,
    episode: u32,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM episode WHERE media_item_id = ?1 AND season_number = ?2
         AND episode_number = ?3 AND monitored = 1",
        params![media_item_id, season, episode],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Season-pack eligibility check: is there anything in this season actually
/// worth grabbing the whole pack for? Without this, a season already fully
/// owned via individual episode grabs would still pull down a full
/// duplicate pack every time one showed up in a feed/search, since (unlike
/// the single-episode path) nothing else here naturally excludes an
/// already-complete season.
fn count_monitored_missing_episodes_in_season(
    conn: &Connection,
    media_item_id: i64,
    season: u32,
) -> Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM episode WHERE media_item_id = ?1 AND season_number = ?2
         AND monitored = 1 AND has_file = 0",
        params![media_item_id, season],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Rough heuristic for whether a release's title indicates English audio,
/// used only to gate obviously-wrong candidates pre-download — actual
/// audio-track composition is only known for certain after download, which
/// is where the real fix (remux, or reject) happens (Phase 8).
fn infer_has_english_audio(title_raw: &str) -> bool {
    const NON_ENGLISH_ONLY_MARKERS: &[&str] = &["VOSTFR", "VOSTA", "ITA ", "LATINO"];
    let upper = title_raw.to_uppercase();
    !NON_ENGLISH_ONLY_MARKERS.iter().any(|m| upper.contains(m))
}

fn best_existing_score(conn: &Connection, episode_id: i64) -> Result<Option<f32>> {
    conn.query_row(
        "SELECT MAX(score) FROM release WHERE episode_id = ?1 AND status IN ('grabbed','imported','upgraded')",
        params![episode_id],
        |row| row.get::<_, Option<f32>>(0),
    )
    .map_err(Into::into)
}

fn best_existing_movie_score(conn: &Connection, media_item_id: i64) -> Result<Option<f32>> {
    conn.query_row(
        "SELECT MAX(score) FROM release WHERE media_item_id = ?1 AND episode_id IS NULL
         AND status IN ('grabbed','imported','upgraded')",
        params![media_item_id],
        |row| row.get::<_, Option<f32>>(0),
    )
    .map_err(Into::into)
}

/// Season-pack counterpart to `best_existing_movie_score` — scoped by
/// `season_number` (not just `episode_id IS NULL`, which a movie's own
/// check relies on) because one TV media_item can have several *different*
/// season packs' grab history, which `episode_id IS NULL` alone can't tell
/// apart.
fn best_existing_season_pack_score(
    conn: &Connection,
    media_item_id: i64,
    season: u32,
) -> Result<Option<f32>> {
    conn.query_row(
        "SELECT MAX(score) FROM release WHERE media_item_id = ?1 AND season_number = ?2
         AND status IN ('grabbed','imported','upgraded')",
        params![media_item_id, season],
        |row| row.get::<_, Option<f32>>(0),
    )
    .map_err(Into::into)
}

/// Whether a newly-scored candidate should be grabbed: always if nothing's
/// been grabbed for this episode yet; otherwise only if it's a repack/proper
/// (fixes a known-bad prior encode) or outscores the current best by more
/// than `min_gain`. `min_gain` is 0.0 for a normal missing-content grab
/// (any strict improvement counts) and a configured positive threshold for
/// an upgrade-search grab (see `SearchTarget::upgrade_min_gain`), so a file
/// already on disk isn't replaced over and over for score deltas too small
/// to matter.
fn should_grab(new_score: f32, is_repack: bool, existing_best: Option<f32>, min_gain: f32) -> bool {
    match existing_best {
        None => true,
        Some(existing) => is_repack || new_score > existing + min_gain,
    }
}

/// `category` here is qBittorrent's category, not `source`'s `kind` — kept
/// as its own parameter (rather than derived from `qbit_category` at the
/// call site) so `torrent_fetch.category` always reflects what the torrent
/// actually got added under, even if that config value changes later.
#[allow(clippy::too_many_arguments)]
fn record_grab(
    conn: &Connection,
    media_item_id: i64,
    episode_id: Option<i64>,
    season_number: Option<u32>,
    source_id: i64,
    raw_title: &str,
    guid: &str,
    score: f32,
    size_bytes: Option<u64>,
    category: &str,
    torrent_hash: Option<&str>,
    status: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO release (media_item_id, episode_id, season_number, raw_title, source_id, guid, score, torrent_hash, status, grabbed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))",
        params![media_item_id, episode_id, season_number, raw_title, source_id, guid, score, torrent_hash, status],
    )?;
    crate::db::record_event(
        conn,
        media_item_id,
        episode_id,
        status,
        &format!("score={score:.1} title={raw_title:?}"),
    )?;
    // Permanent audit trail, independent of whatever later happens to the
    // `release` row above (status changes, or the row/its media_item being
    // deleted) — see `torrent_fetch`'s own doc comment in db.rs.
    crate::db::record_torrent_fetch(
        conn,
        torrent_hash,
        raw_title,
        size_bytes,
        category,
        source_id,
        media_item_id,
        episode_id,
        status,
    )?;
    Ok(())
}

fn is_seen(conn: &Connection, source_id: i64, guid: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM seen_guid WHERE source_id = ?1 AND guid = ?2",
        params![source_id, guid],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn mark_seen(conn: &Connection, source_id: i64, guid: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO seen_guid (source_id, guid, seen_at) VALUES (?1, ?2, datetime('now'))",
        params![source_id, guid],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_item(
    conn: &Connection,
    item: &RawReleaseItem,
    matcher: &mut TitleMatcher,
    qbit: &QbitClient,
    qbit_category: &str,
    source_id: i64,
    better_resolution_available: bool,
    upgrade_min_gain: Option<f32>,
) -> Result<ProcessOutcome> {
    if looks_like_non_video_format(&item.title) {
        return Ok(ProcessOutcome::NonVideoFormat);
    }

    let parsed = parser::parse(&item.title);

    let candidate = match matcher.match_title(conn, &parsed.title_normalized)? {
        MatchOutcome::Auto(c) => c,
        MatchOutcome::NeedsReview(c) => {
            matcher::queue_for_review(conn, &item.title, &c, Some(&item.link), Some(source_id))?;
            return Ok(ProcessOutcome::QueuedForReview);
        }
        MatchOutcome::NoMatch => return Ok(ProcessOutcome::NoMatch),
    };

    let media_item = get_media_item(conn, candidate.media_item_id)?;

    let mut episode_id: Option<i64> = None;
    let mut season_pack_number: Option<u32> = None;

    if media_item.kind == "movie" {
        // A release carrying a season/episode/absolute-episode marker
        // title-matched a movie by name alone — it's actually an episode
        // of some same-named show, not this movie.
        if looks_like_episode(&parsed) {
            return Ok(ProcessOutcome::NoMatch);
        }
        if movie_year_mismatch(parsed.year, media_item.year) {
            return Ok(ProcessOutcome::YearMismatch);
        }
        let eligible = match upgrade_min_gain {
            Some(_) => movie_eligible_for_upgrade(conn, media_item.id)?,
            None => movie_needs_grab(conn, media_item.id)?,
        };
        if !eligible {
            return Ok(ProcessOutcome::NotMonitoredOrAlreadyHave);
        }
    } else if looks_like_season_pack(&parsed) {
        let Some(season) = parsed.season else {
            return Ok(ProcessOutcome::CouldNotResolveEpisode);
        };
        if count_monitored_missing_episodes_in_season(conn, media_item.id, season)? == 0 {
            return Ok(ProcessOutcome::NotMonitoredOrAlreadyHave);
        }
        season_pack_number = Some(season);
    } else {
        let Some((season, episode)) = resolve_episode(conn, media_item.tvdb_id, &parsed)? else {
            return Ok(ProcessOutcome::CouldNotResolveEpisode);
        };
        let eid_opt = match upgrade_min_gain {
            Some(_) => find_monitored_episode(conn, media_item.id, season, episode)?,
            None => find_monitored_missing_episode(conn, media_item.id, season, episode)?,
        };
        let Some(eid) = eid_opt else {
            return Ok(ProcessOutcome::NotMonitoredOrAlreadyHave);
        };
        episode_id = Some(eid);
    }

    let anime = match media_item.tvdb_id {
        Some(id) => is_anime(conn, id)?,
        None => false,
    };
    let profile_kind = if media_item.kind == "movie" {
        ProfileKind::Movie
    } else {
        ProfileKind::Tv
    };
    let profile = load_quality_profile(conn, media_item.quality_profile_id, profile_kind)?;

    let gate_ctx = GateContext {
        seeders: item.seeders,
        size_bytes: item.size_bytes,
        runtime_minutes: None,
        has_english_audio: infer_has_english_audio(&item.title),
        is_anime: anime,
        better_resolution_available,
        is_season_pack: season_pack_number.is_some(),
    };

    if let GateResult::Reject(reason) = scoring::evaluate_gates(&parsed, &gate_ctx, &profile) {
        return Ok(ProcessOutcome::GateRejected(reason));
    }

    let release_score = scoring::score(&parsed, item.seeders.unwrap_or(0), false, &profile);
    let existing_best = match (episode_id, season_pack_number) {
        (Some(eid), _) => best_existing_score(conn, eid)?,
        (None, Some(season)) => best_existing_season_pack_score(conn, media_item.id, season)?,
        (None, None) => best_existing_movie_score(conn, media_item.id)?,
    };

    if !should_grab(
        release_score,
        parsed.is_repack,
        existing_best,
        upgrade_min_gain.unwrap_or(0.0),
    ) {
        return Ok(ProcessOutcome::NotBetterThanExisting);
    }

    let torrent_hash = match grab_and_capture_hash(qbit, &item.link, qbit_category).await {
        Ok(hash) => hash,
        Err(e) if e.downcast_ref::<crate::qbit::MagnetRejected>().is_some() => {
            return Ok(ProcessOutcome::MagnetRejected);
        }
        Err(e) => return Err(e),
    };

    let Some(torrent_hash) = torrent_hash else {
        // The add almost certainly succeeded on qBittorrent's side, but we
        // couldn't correlate which torrent it became — recording this as
        // `grabbed` with a NULL hash would strand it forever (see
        // `ProcessOutcome::HashCaptureFailed`'s doc comment), so it's
        // recorded as `failed` instead, freeing the episode/movie for a
        // future search to try again with a different candidate.
        record_grab(
            conn,
            media_item.id,
            episode_id,
            season_pack_number,
            source_id,
            &item.title,
            &item.guid,
            release_score,
            item.size_bytes,
            qbit_category,
            None,
            "failed",
        )?;
        return Ok(ProcessOutcome::HashCaptureFailed);
    };

    record_grab(
        conn,
        media_item.id,
        episode_id,
        season_pack_number,
        source_id,
        &item.title,
        &item.guid,
        release_score,
        item.size_bytes,
        qbit_category,
        Some(&torrent_hash),
        "grabbed",
    )?;

    Ok(ProcessOutcome::Grabbed {
        media_item_id: media_item.id,
        episode_id,
        season_number: season_pack_number,
        score: release_score,
    })
}

/// The number of `torrents/info` polls to attempt when a link gives us no
/// hash up front (nyaa's `.torrent`-URL links) before giving up. qBittorrent
/// must first fetch the `.torrent` file itself before the torrent exists in
/// its own list, so the old zero-delay before/after snapshot routinely
/// missed it entirely (verified live: 23/23 NULL hashes in one batch).
const HASH_POLL_ATTEMPTS: u32 = 15;
const HASH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Finds a torrent's hash so a completed download can later be correlated
/// back to its `release` row.
///
/// `link` may be a detail-page URL rather than a directly-grabbable magnet
/// (1337x search results only carry the former) — resolved here, right
/// before the grab, so this stays a no-op for sources that are already
/// directly grabbable (nyaa) and only costs a request for the one
/// candidate actually being downloaded.
///
/// Once resolved, a magnet link already carries its own infohash
/// (`urn:btih:...`) — extracted directly, no need to ask qBittorrent at all.
/// Only nyaa's `.torrent`-URL links (not magnets) fall through to polling
/// `torrents/info` for a newly-registered hash, which is both slower and
/// inherently racy against any *other* concurrent add — `grab_lock`
/// serializes the whole add-and-correlate sequence across every caller
/// (the background loop and the API's review-approve handler both add
/// against the same category and can otherwise interleave, previously
/// letting one grab record a hash that actually belonged to a different,
/// concurrently-added torrent).
async fn grab_and_capture_hash(
    qbit: &QbitClient,
    link: &str,
    category: &str,
) -> Result<Option<String>> {
    let resolved;
    let link = if sources::scrape::needs_resolution(link) {
        // No total timeout is reqwest's default — this runs inside the
        // background loop while it holds the DB mutex, so a stalled
        // connection would hang the whole daemon.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        resolved = sources::scrape::resolve_magnet(&client, link).await?;
        &resolved
    } else {
        link
    };

    let _grab_guard = qbit.lock_for_grab().await;

    if let Some(hash) = crate::qbit::extract_btih(link) {
        qbit.add_magnet(link, category).await?;
        return Ok(Some(hash));
    }

    let before: std::collections::HashSet<String> = qbit
        .list_torrents(Some(category))
        .await?
        .into_iter()
        .map(|t| t.hash)
        .collect();

    qbit.add_magnet(link, category).await?;

    for attempt in 0..HASH_POLL_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(HASH_POLL_INTERVAL).await;
        }
        let after = qbit.list_torrents(Some(category)).await?;
        let mut new_hashes = after.into_iter().filter(|t| !before.contains(&t.hash));
        let Some(hash) = new_hashes.next() else {
            continue;
        };
        if new_hashes.next().is_some() {
            tracing::warn!(
                "multiple new torrents appeared between snapshots; hash correlation may be ambiguous"
            );
        }
        return Ok(Some(hash.hash));
    }
    tracing::warn!(
        link,
        "gave up waiting for the added torrent to register with qbittorrent"
    );
    Ok(None)
}

#[derive(Debug, Default)]
pub struct GrabCycleStats {
    pub items_seen: usize,
    pub new_items: usize,
    pub grabbed: usize,
    pub errors: usize,
    pub queued_for_review: usize,
}

pub async fn run_grab_cycle(
    conn: &Connection,
    source: &dyn ReleaseSource,
    source_id: i64,
    matcher: &mut TitleMatcher,
    qbit: &QbitClient,
    qbit_category: &str,
) -> Result<GrabCycleStats> {
    let items = source.fetch(None).await?;
    let mut stats = GrabCycleStats {
        items_seen: items.len(),
        ..Default::default()
    };

    for item in items {
        if is_seen(conn, source_id, &item.guid)? {
            continue;
        }

        // Marked seen only on a deterministic verdict (matched-and-grabbed,
        // no-match, needs-review, etc.) — never on `Err`, which is usually
        // transient (qbit momentarily unreachable, a stalled request). Guids
        // aren't re-emitted by a feed indefinitely, so leaving a failed item
        // unmarked just lets the next cycle retry it while it's still in
        // the source's window, rather than permanently losing it to a blip.
        // `false`: the RSS feed-watch path sees one item at a time as it
        // streams in, with no batch of alternatives to compare against —
        // the low-resolution-alternative-exists gate only makes sense on
        // the search-driven path below, which fetches a whole candidate
        // list per target up front.
        match process_item(
            conn,
            &item,
            matcher,
            qbit,
            qbit_category,
            source_id,
            false,
            None,
        )
        .await
        {
            Ok(outcome) => {
                mark_seen(conn, source_id, &item.guid)?;
                stats.new_items += 1;
                match outcome {
                    ProcessOutcome::Grabbed { .. } => stats.grabbed += 1,
                    ProcessOutcome::QueuedForReview => stats.queued_for_review += 1,
                    _ => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, title = %item.title, "failed to process release item");
                stats.errors += 1;
            }
        }
    }

    Ok(stats)
}

// --- Search-driven acquisition (1337x for general TV/movies, nyaa search
// for anime movies) ---
//
// Unlike the feed-based path above, there's no natural stream of "new"
// items to dedup against — the recurring cost here is the *search itself*,
// repeated against the same monitored-but-missing catalog. `search_state`
// tracks per-item search history so the same item isn't re-searched every
// single cycle; cadence backs off exponentially (6h, 12h, 24h, 48h, 96h,
// capped at a week) the more times it's been searched without success.

/// Other general-content sources considered and rejected (live-tested
/// 2026-07-12, not just assumed) before landing on TPB as primary:
/// - **YTS** (`yts.mx`): DNS doesn't resolve at all. Every known mirror
///   (`yts.am`, `yts.ag`, `yts.lt`, `yts.pe`) either 301s in a loop or drops
///   the query and lands on a bare homepage. The whole mirror network looks
///   dead, not just one domain — re-check before assuming a fix is quick.
/// - **EZTV** (`eztv.re`): redirects to `eztvx.to`, which fails to connect
///   outright (TLS/connection error, not a slow response). Also
///   Cloudflare-fronted, so even if connectivity is restored it carries the
///   same risk profile 1337x does.
/// If revisiting either, re-verify connectivity first — this isn't a
/// permanent architectural decision, just what was true when checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SearchRoute {
    /// Primary general-content (movies + non-anime TV) route — a JSON API,
    /// ranked by relevance rather than pure seeder count.
    Tpb,
    /// Kept wired in as a fallback for when TPB's own fetch fails outright
    /// (not just "no relevant results") — the mirror-rotation/cooldown
    /// machinery already built for it is real resilience worth keeping,
    /// just no longer the first choice given TPB's better precision.
    X1337,
    NyaaSearch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchTarget {
    media_item_id: i64,
    episode_id: Option<i64>,
    /// The target episode's season — `None` for a movie target. Lets
    /// `execute_search_targets` recognize a season-pack grab as having
    /// satisfied this target even though it has no single `episode_id`
    /// of its own.
    season_number: Option<u32>,
    query: String,
    route: SearchRoute,
    /// Significant (len >= 4, alphanumeric) lowercased words from the
    /// show/movie's own title — used to pre-filter obviously-irrelevant
    /// search results before they ever reach the title matcher. 1337x's
    /// search does loose keyword matching, not phrase matching: a query
    /// for "Modern Family S00E01" surfaces "Family Guy" and "The Addams
    /// Family" at the top by seeder count, sharing only the generic word
    /// "family" — without this filter those get fed straight into the
    /// embedding matcher and, worse, can occasionally out-similarity a
    /// genuine but lower-quality result.
    title_words: Vec<String>,
    /// `None` for a normal missing-content target. `Some(gain)` marks this
    /// as an upgrade-search target for an episode/movie that already has a
    /// file — `process_item` then looks the item up regardless of
    /// `has_file`/existing-file state, and a new candidate must beat the
    /// current best score by at least `gain` (repacks/propers still always
    /// supersede) rather than merely being strictly better.
    upgrade_min_gain: Option<f32>,
}

fn significant_words(title: &str) -> Vec<String> {
    title
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 4 && w.chars().all(|c| c.is_alphanumeric()))
        .collect()
}

/// True unless the target has at least one significant word (nothing to
/// filter on — a one-or-two-short-word title lets everything through) and
/// the candidate title is missing *any* of them. Requiring every word
/// rather than a fraction is deliberately strict: the false positives seen
/// in practice ("The First Purge" for a "...First Marriage" query) share
/// exactly one word with the target, and a fractional threshold wouldn't
/// exclude them for short titles.
fn passes_relevance_filter(target_words: &[String], candidate_title: &str) -> bool {
    if target_words.is_empty() {
        return true;
    }
    let lower = candidate_title.to_lowercase();
    target_words.iter().all(|w| lower.contains(w.as_str()))
}

/// 1337x's search chokes on punctuation (colons, apostrophes) — replace
/// anything that isn't alphanumeric/whitespace with a space and collapse.
fn sanitize_query_text(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    parser::WS_RE.replace_all(cleaned.trim(), " ").to_string()
}

/// TPB's search (apibay) does literal multi-term matching — appending an
/// "SxxEyy" token alongside a multi-word title routinely returns zero
/// results even when genuine episodes for that exact season/episode exist
/// and would surface from a plain title search (verified live: "Show Title
/// S01E01" → zero results, "Show Title" alone → real S01E13/E14/E15 results
/// a few rows down). So TPB gets a title-only query; `process_item` already
/// parses season/episode back out of each *result's* own title via
/// `parser::parse`, so nothing about episode identification depends on the
/// query carrying it. 1337x (still reachable as TPB's fetch-failure
/// fallback) keeps the season/episode suffix, since it doesn't share this
/// failure mode and the extra scoping helps there.
fn build_tv_query(title: &str, season: i64, episode: i64, route: SearchRoute) -> String {
    match route {
        SearchRoute::Tpb => sanitize_query_text(title),
        SearchRoute::X1337 | SearchRoute::NyaaSearch => {
            format!("{} S{season:02}E{episode:02}", sanitize_query_text(title))
        }
    }
}

fn build_movie_query(title: &str, year: Option<i64>) -> String {
    match year {
        Some(y) => format!("{} {y}", sanitize_query_text(title)),
        None => sanitize_query_text(title),
    }
}

/// SQL fragment (embedded inline, not a bind parameter — SQLite has no
/// syntax for parameterizing an expression) implementing the cadence
/// formula: due immediately if never searched, otherwise `6h * 2^(count-1)`
/// after the last attempt, capped at a week. The exponent itself is
/// separately clamped to 10 (`6 * 2^10` hours is already ~4.7 years, far
/// past the 168-hour outer cap) — SQLite integers are 64-bit signed, so an
/// uncapped `search_count` eventually overflows `1 << (search_count - 1)`
/// into a negative number (verified in SQLite directly: happens at
/// search_count=64) and `min(168, negative)` then picks the negative value,
/// making `datetime(..., '+' || negative || ' hours')` land in the past —
/// so a chronically failing target would silently flip from backing off
/// weekly to being searched *every single cycle* instead, right when it
/// should be backing off the most.
const DUE_CLAUSE: &str = "(last_searched_at IS NULL \
     OR datetime(last_searched_at, '+' || min(168, 6 * (1 << min(search_count - 1, 10))) || ' hours') <= datetime('now'))";

/// Same exponential-backoff shape as `DUE_CLAUSE`, but with a much longer
/// base (24h vs. 6h) and outer cap (30 days vs. 1 week) — there's no urgency
/// to re-checking something that's already satisfied by a file on disk, so
/// this cadence is deliberately slower than the missing-content one.
const UPGRADE_DUE_CLAUSE: &str = "(last_checked_at IS NULL \
     OR datetime(last_checked_at, '+' || min(720, 24 * (1 << min(check_count - 1, 10))) || ' hours') <= datetime('now'))";

/// Enumerates up to `budget` monitored-but-missing items due for a search
/// attempt, across both non-anime TV episodes and movies. Anime TV isn't
/// included — it's already covered by the proven nyaa RSS feed watch, which
/// carries no request-budget risk the way repeated searches do.
///
/// Priority when there are more due candidates than budget: never-searched
/// items first (so a newly added show/movie gets immediate coverage instead
/// of starving behind a backlog of retries), then most-overdue-searched
/// first. `Option<String>` sorts `None` before `Some` in Rust, which
/// matches SQLite's own default NULLS-FIRST behavior for `ASC` — no manual
/// timestamp parsing needed to get this ordering right.
fn enumerate_search_targets(conn: &Connection, budget: usize) -> Result<Vec<SearchTarget>> {
    let mut candidates: Vec<(SearchTarget, Option<String>, String)> = Vec::new();

    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, e.media_item_id, m.title, e.season_number, e.episode_number, e.air_date,
                ss.last_searched_at
         FROM episode e
         JOIN media_item m ON m.id = e.media_item_id
         LEFT JOIN search_state ss ON ss.episode_id = e.id
         WHERE m.monitored = 1 AND m.kind = 'series'
           AND e.monitored = 1 AND e.has_file = 0
           AND e.air_date IS NOT NULL AND e.air_date <= date('now')
           AND (m.tvdb_id IS NULL OR m.tvdb_id NOT IN
                (SELECT tvdb_id FROM anime_mapping WHERE tvdb_id IS NOT NULL))
           AND NOT EXISTS (SELECT 1 FROM release r WHERE r.episode_id = e.id
                           AND r.status IN ('grabbed','downloading'))
           AND (ss.last_searched_at IS NULL OR {})",
        DUE_CLAUSE
            .replace("last_searched_at", "ss.last_searched_at")
            .replace("search_count", "ss.search_count")
    ))?;
    struct EpisodeCandidateRow {
        episode_id: i64,
        media_item_id: i64,
        title: String,
        season: i64,
        episode: i64,
        air_date: String,
        last_searched_at: Option<String>,
    }
    let rows: Vec<EpisodeCandidateRow> = stmt
        .query_map([], |row| {
            Ok(EpisodeCandidateRow {
                episode_id: row.get(0)?,
                media_item_id: row.get(1)?,
                title: row.get(2)?,
                season: row.get(3)?,
                episode: row.get(4)?,
                air_date: row.get(5)?,
                last_searched_at: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for EpisodeCandidateRow {
        episode_id,
        media_item_id,
        title,
        season,
        episode,
        air_date,
        last_searched_at,
    } in rows
    {
        candidates.push((
            SearchTarget {
                media_item_id,
                episode_id: Some(episode_id),
                season_number: Some(season as u32),
                query: build_tv_query(&title, season, episode, SearchRoute::Tpb),
                title_words: significant_words(&title),
                route: SearchRoute::Tpb,
                upgrade_min_gain: None,
            },
            last_searched_at,
            air_date,
        ));
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.title, m.year,
                (m.anidb_id IS NOT NULL
                 OR m.tmdb_id IN (SELECT tmdb_id FROM anime_mapping WHERE tmdb_id IS NOT NULL)
                 OR m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie)) AS is_anime_movie,
                ss.last_searched_at
         FROM media_item m
         LEFT JOIN search_state ss ON ss.media_item_id = m.id AND ss.episode_id IS NULL
         WHERE m.kind = 'movie' AND m.monitored = 1
           AND NOT EXISTS (SELECT 1 FROM episode_file f
                           WHERE f.media_item_id = m.id AND f.episode_id IS NULL)
           AND NOT EXISTS (SELECT 1 FROM release r WHERE r.media_item_id = m.id
                           AND r.episode_id IS NULL AND r.status IN ('grabbed','downloading'))
           AND (ss.last_searched_at IS NULL OR {})",
        DUE_CLAUSE
            .replace("last_searched_at", "ss.last_searched_at")
            .replace("search_count", "ss.search_count")
    ))?;
    struct MovieCandidateRow {
        media_item_id: i64,
        title: String,
        year: Option<i64>,
        is_anime_movie: i64,
        last_searched_at: Option<String>,
    }
    let rows: Vec<MovieCandidateRow> = stmt
        .query_map([], |row| {
            Ok(MovieCandidateRow {
                media_item_id: row.get(0)?,
                title: row.get(1)?,
                year: row.get(2)?,
                is_anime_movie: row.get(3)?,
                last_searched_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for MovieCandidateRow {
        media_item_id,
        title,
        year,
        is_anime_movie,
        last_searched_at,
    } in rows
    {
        let route = if is_anime_movie != 0 {
            SearchRoute::NyaaSearch
        } else {
            SearchRoute::Tpb
        };
        candidates.push((
            SearchTarget {
                media_item_id,
                episode_id: None,
                season_number: None,
                query: build_movie_query(&title, year),
                title_words: significant_words(&title),
                route,
                upgrade_min_gain: None,
            },
            last_searched_at,
            format!("{media_item_id:020}"),
        ));
    }

    candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| b.2.cmp(&a.2)));
    candidates.truncate(budget);
    Ok(candidates.into_iter().map(|(t, _, _)| t).collect())
}

/// Upgrade-search counterpart to `enumerate_search_targets`: the mirror
/// image query — `has_file = 1`/`EXISTS episode_file` instead of `= 0`/
/// `NOT EXISTS`, joined against `upgrade_state` instead of `search_state` —
/// enumerating up to `budget` monitored-and-already-owned items due for an
/// upgrade check. Anime TV is excluded for the same reason as the
/// missing-content path: it's covered by the nyaa RSS feed watch instead.
/// Every target gets `upgrade_min_gain: Some(min_gain)`, which is what
/// `process_item` uses both to bypass the normal `has_file` eligibility
/// gates and to require a new candidate to beat the current file's score by
/// more than a marginal amount (see `should_grab`).
/// SQL fragment computing whether a file has any of the ground-truth
/// `media_file_probe` problem signals worth prioritizing an upgrade check
/// for — under-quality (probe-verified, not just what the release title
/// claimed), missing subtitles, a non-English default audio track (the
/// at-import remux fix should normally catch this, but a pre-existing
/// library file might predate it), or a probe/decode failure. `p` must be
/// the table's alias in the query this is embedded in. A file with no probe
/// row yet (still queued for the incremental probe sweep) reads as
/// not-flagged rather than flagged — there's nothing to act on until it's
/// actually been probed.
const NEEDS_ATTENTION_CLAUSE: &str = "(COALESCE(p.flag_under_quality, 0) = 1 \
     OR COALESCE(p.flag_no_subtitles, 0) = 1 \
     OR COALESCE(p.flag_non_english_default_audio, 0) = 1 \
     OR COALESCE(p.corruption_status, '') IN ('probe_failed', 'decode_failed'))";

/// Upgrade-search counterpart to `enumerate_search_targets`: the mirror
/// image query — `has_file = 1`/`EXISTS episode_file` instead of `= 0`/
/// `NOT EXISTS`, joined against `upgrade_state` instead of `search_state` —
/// enumerating up to `budget` monitored-and-already-owned items due for an
/// upgrade check. Anime TV is excluded for the same reason as the
/// missing-content path: it's covered by the nyaa RSS feed watch instead.
/// Every target gets `upgrade_min_gain: Some(min_gain)`, which is what
/// `process_item` uses both to bypass the normal `has_file` eligibility
/// gates and to require a new candidate to beat the current file's score by
/// more than a marginal amount (see `should_grab`).
///
/// Ordering prioritizes `media_file_probe`-flagged files
/// (`NEEDS_ATTENTION_CLAUSE`) ahead of everything else, so a limited budget
/// lands on files with a real, ground-truth-verified problem before it's
/// spent on routine re-checks of files nothing is actually wrong with. Due-
/// ness (never-checked first, then most-overdue) is still the tiebreaker
/// within each of those two priority tiers.
fn enumerate_upgrade_targets(
    conn: &Connection,
    budget: usize,
    min_gain: f32,
) -> Result<Vec<SearchTarget>> {
    let mut candidates: Vec<(SearchTarget, bool, Option<String>, String)> = Vec::new();

    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, e.media_item_id, m.title, e.season_number, e.episode_number, e.air_date,
                us.last_checked_at, {NEEDS_ATTENTION_CLAUSE} AS needs_attention
         FROM episode e
         JOIN media_item m ON m.id = e.media_item_id
         LEFT JOIN upgrade_state us ON us.episode_id = e.id
         LEFT JOIN episode_file ef ON ef.episode_id = e.id
         LEFT JOIN media_file_probe p ON p.episode_file_id = ef.id
         WHERE m.monitored = 1 AND m.kind = 'series'
           AND e.monitored = 1 AND e.has_file = 1
           AND (m.tvdb_id IS NULL OR m.tvdb_id NOT IN
                (SELECT tvdb_id FROM anime_mapping WHERE tvdb_id IS NOT NULL))
           AND NOT EXISTS (SELECT 1 FROM release r WHERE r.episode_id = e.id
                           AND r.status IN ('grabbed','downloading'))
           AND (us.last_checked_at IS NULL OR {})",
        UPGRADE_DUE_CLAUSE
            .replace("last_checked_at", "us.last_checked_at")
            .replace("check_count", "us.check_count")
    ))?;
    struct EpisodeCandidateRow {
        episode_id: i64,
        media_item_id: i64,
        title: String,
        season: i64,
        episode: i64,
        last_checked_at: Option<String>,
        needs_attention: bool,
    }
    let rows: Vec<EpisodeCandidateRow> = stmt
        .query_map([], |row| {
            Ok(EpisodeCandidateRow {
                episode_id: row.get(0)?,
                media_item_id: row.get(1)?,
                title: row.get(2)?,
                season: row.get(3)?,
                episode: row.get(4)?,
                last_checked_at: row.get(6)?,
                needs_attention: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for EpisodeCandidateRow {
        episode_id,
        media_item_id,
        title,
        season,
        episode,
        last_checked_at,
        needs_attention,
    } in rows
    {
        candidates.push((
            SearchTarget {
                media_item_id,
                episode_id: Some(episode_id),
                season_number: Some(season as u32),
                query: build_tv_query(&title, season, episode, SearchRoute::Tpb),
                title_words: significant_words(&title),
                route: SearchRoute::Tpb,
                upgrade_min_gain: Some(min_gain),
            },
            needs_attention,
            last_checked_at,
            format!("{episode_id:020}"),
        ));
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.title, m.year,
                (m.anidb_id IS NOT NULL
                 OR m.tmdb_id IN (SELECT tmdb_id FROM anime_mapping WHERE tmdb_id IS NOT NULL)
                 OR m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie)) AS is_anime_movie,
                us.last_checked_at, {NEEDS_ATTENTION_CLAUSE} AS needs_attention
         FROM media_item m
         LEFT JOIN upgrade_state us ON us.media_item_id = m.id AND us.episode_id IS NULL
         LEFT JOIN episode_file ef ON ef.media_item_id = m.id AND ef.episode_id IS NULL
         LEFT JOIN media_file_probe p ON p.episode_file_id = ef.id
         WHERE m.kind = 'movie' AND m.monitored = 1
           AND EXISTS (SELECT 1 FROM episode_file f
                       WHERE f.media_item_id = m.id AND f.episode_id IS NULL)
           AND NOT EXISTS (SELECT 1 FROM release r WHERE r.media_item_id = m.id
                           AND r.episode_id IS NULL AND r.status IN ('grabbed','downloading'))
           AND (us.last_checked_at IS NULL OR {})",
        UPGRADE_DUE_CLAUSE
            .replace("last_checked_at", "us.last_checked_at")
            .replace("check_count", "us.check_count")
    ))?;
    struct MovieCandidateRow {
        media_item_id: i64,
        title: String,
        year: Option<i64>,
        is_anime_movie: i64,
        last_checked_at: Option<String>,
        needs_attention: bool,
    }
    let rows: Vec<MovieCandidateRow> = stmt
        .query_map([], |row| {
            Ok(MovieCandidateRow {
                media_item_id: row.get(0)?,
                title: row.get(1)?,
                year: row.get(2)?,
                is_anime_movie: row.get(3)?,
                last_checked_at: row.get(4)?,
                needs_attention: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for MovieCandidateRow {
        media_item_id,
        title,
        year,
        is_anime_movie,
        last_checked_at,
        needs_attention,
    } in rows
    {
        let route = if is_anime_movie != 0 {
            SearchRoute::NyaaSearch
        } else {
            SearchRoute::Tpb
        };
        candidates.push((
            SearchTarget {
                media_item_id,
                episode_id: None,
                season_number: None,
                query: build_movie_query(&title, year),
                title_words: significant_words(&title),
                route,
                upgrade_min_gain: Some(min_gain),
            },
            needs_attention,
            last_checked_at,
            format!("{media_item_id:020}"),
        ));
    }

    candidates.sort_by(|a, b| {
        // `!needs_attention` first so `true` (flagged) sorts ahead of
        // `false` — `bool`'s `Ord` puts `false < true`.
        (!a.1)
            .cmp(&!b.1)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| b.3.cmp(&a.3))
    });
    candidates.truncate(budget);
    Ok(candidates.into_iter().map(|(t, ..)| t).collect())
}

fn record_search_attempt(
    conn: &Connection,
    media_item_id: i64,
    episode_id: Option<i64>,
    result: &str,
) -> Result<()> {
    let existing: Option<i64> = match episode_id {
        Some(eid) => conn
            .query_row(
                "SELECT id FROM search_state WHERE episode_id = ?1",
                params![eid],
                |r| r.get(0),
            )
            .optional()?,
        None => conn
            .query_row(
                "SELECT id FROM search_state WHERE media_item_id = ?1 AND episode_id IS NULL",
                params![media_item_id],
                |r| r.get(0),
            )
            .optional()?,
    };
    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE search_state SET last_searched_at = datetime('now'),
                 search_count = search_count + 1, last_result = ?1 WHERE id = ?2",
                params![result, id],
            )?;
        }
        None => {
            conn.execute(
                "INSERT INTO search_state (media_item_id, episode_id, last_searched_at, search_count, last_result)
                 VALUES (?1, ?2, datetime('now'), 1, ?3)",
                params![media_item_id, episode_id, result],
            )?;
        }
    }
    Ok(())
}

/// Same upsert shape as `record_search_attempt`, against `upgrade_state`
/// instead — kept as a separate table/function pair rather than adding an
/// `is_upgrade` column to `search_state`, so the two due-ness cadences never
/// interfere with each other's clock (see `upgrade_state`'s doc comment in
/// db.rs).
fn record_upgrade_attempt(
    conn: &Connection,
    media_item_id: i64,
    episode_id: Option<i64>,
    result: &str,
) -> Result<()> {
    let existing: Option<i64> = match episode_id {
        Some(eid) => conn
            .query_row(
                "SELECT id FROM upgrade_state WHERE episode_id = ?1",
                params![eid],
                |r| r.get(0),
            )
            .optional()?,
        None => conn
            .query_row(
                "SELECT id FROM upgrade_state WHERE media_item_id = ?1 AND episode_id IS NULL",
                params![media_item_id],
                |r| r.get(0),
            )
            .optional()?,
    };
    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE upgrade_state SET last_checked_at = datetime('now'),
                 check_count = check_count + 1, last_result = ?1 WHERE id = ?2",
                params![result, id],
            )?;
        }
        None => {
            conn.execute(
                "INSERT INTO upgrade_state (media_item_id, episode_id, last_checked_at, check_count, last_result)
                 VALUES (?1, ?2, datetime('now'), 1, ?3)",
                params![media_item_id, episode_id, result],
            )?;
        }
    }
    Ok(())
}

/// Routes to `record_search_attempt` or `record_upgrade_attempt` based on
/// which cadence `target` belongs to — lets `execute_search_targets` stay a
/// single shared implementation for both the missing-content search cycle
/// and the upgrade-search cycle.
fn record_target_attempt(conn: &Connection, target: &SearchTarget, result: &str) -> Result<()> {
    match target.upgrade_min_gain {
        Some(_) => record_upgrade_attempt(conn, target.media_item_id, target.episode_id, result),
        None => record_search_attempt(conn, target.media_item_id, target.episode_id, result),
    }
}

#[derive(Debug, Default)]
pub struct SearchCycleStats {
    pub targets: usize,
    pub searched: usize,
    pub grabbed: usize,
    pub errors: usize,
    pub queued_for_review: usize,
    /// Set when a source fetch failed outright (all mirrors down/cooling,
    /// or two consecutive fetch errors this cycle) — the caller uses this
    /// to trigger cycle-level backoff rather than tightening the search
    /// loop's own retry interval, which stays fixed.
    pub source_exhausted: bool,
}

/// Results are sorted by seeders and only the top N are evaluated — bounds
/// both the title-matcher's per-item inference cost and how much noise a
/// single broad query can dump into the review queue.
const MAX_RESULTS_PER_SEARCH: usize = 15;

#[allow(clippy::too_many_arguments)]
pub async fn run_search_cycle(
    conn: &Connection,
    tpb: &sources::tpb::TpbSource,
    tpb_source_id: i64,
    scrape: &sources::scrape::ScrapeSource,
    scrape_source_id: i64,
    nyaa_search: &sources::rss::RssSource,
    nyaa_source_id: i64,
    matcher: &mut TitleMatcher,
    qbit: &QbitClient,
    qbit_category: &str,
    budget: usize,
) -> Result<SearchCycleStats> {
    let targets = enumerate_search_targets(conn, budget)?;
    execute_search_targets(
        conn,
        &targets,
        tpb,
        tpb_source_id,
        scrape,
        scrape_source_id,
        nyaa_search,
        nyaa_source_id,
        matcher,
        qbit,
        qbit_category,
    )
    .await
}

/// Upgrade-search counterpart to `run_search_cycle`: same execution engine
/// (`execute_search_targets`), fed already-owned targets instead of missing
/// ones. `min_gain` is threaded onto every target via
/// `enumerate_upgrade_targets`, which is what routes `process_item` into
/// its upgrade-eligibility path instead of the normal missing-content one.
#[allow(clippy::too_many_arguments)]
pub async fn run_upgrade_cycle(
    conn: &Connection,
    tpb: &sources::tpb::TpbSource,
    tpb_source_id: i64,
    scrape: &sources::scrape::ScrapeSource,
    scrape_source_id: i64,
    nyaa_search: &sources::rss::RssSource,
    nyaa_source_id: i64,
    matcher: &mut TitleMatcher,
    qbit: &QbitClient,
    qbit_category: &str,
    budget: usize,
    min_gain: f32,
) -> Result<SearchCycleStats> {
    let targets = enumerate_upgrade_targets(conn, budget, min_gain)?;
    execute_search_targets(
        conn,
        &targets,
        tpb,
        tpb_source_id,
        scrape,
        scrape_source_id,
        nyaa_search,
        nyaa_source_id,
        matcher,
        qbit,
        qbit_category,
    )
    .await
}

/// Every currently-missing episode/movie for one specific `media_item`,
/// ignoring the normal due-ness cadence and global budget — for a manual,
/// explicitly-scoped one-off ("go find everything for this one show now"),
/// not part of the recurring background loop, so the usual "don't re-search
/// something too soon" throttling doesn't apply: there's nothing to
/// throttle against when the whole point is a single bounded pass over one
/// show's own backlog.
pub fn enumerate_search_targets_for_media_item(
    conn: &Connection,
    media_item_id: i64,
) -> Result<Vec<SearchTarget>> {
    let kind: String = conn.query_row(
        "SELECT kind FROM media_item WHERE id = ?1",
        params![media_item_id],
        |row| row.get(0),
    )?;

    if kind == "movie" {
        let row: Option<(String, Option<i64>, i64)> = conn
            .query_row(
                "SELECT m.title, m.year,
                        (m.anidb_id IS NOT NULL
                         OR m.tmdb_id IN (SELECT tmdb_id FROM anime_mapping WHERE tmdb_id IS NOT NULL)
                         OR m.tmdb_id IN (SELECT tmdb_id FROM anime_tmdb_movie))
                 FROM media_item m
                 WHERE m.id = ?1
                   AND NOT EXISTS (SELECT 1 FROM episode_file f
                                   WHERE f.media_item_id = m.id AND f.episode_id IS NULL)",
                params![media_item_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((title, year, is_anime_movie)) = row else {
            return Ok(Vec::new());
        };
        let route = if is_anime_movie != 0 {
            SearchRoute::NyaaSearch
        } else {
            SearchRoute::Tpb
        };
        return Ok(vec![SearchTarget {
            media_item_id,
            episode_id: None,
            season_number: None,
            query: build_movie_query(&title, year),
            title_words: significant_words(&title),
            route,
            upgrade_min_gain: None,
        }]);
    }

    let mut stmt = conn.prepare(
        "SELECT e.id, m.title, e.season_number, e.episode_number
         FROM episode e
         JOIN media_item m ON m.id = e.media_item_id
         WHERE e.media_item_id = ?1 AND e.monitored = 1 AND e.has_file = 0
           AND e.air_date IS NOT NULL AND e.air_date <= date('now')
           AND NOT EXISTS (SELECT 1 FROM release r WHERE r.episode_id = e.id
                           AND r.status IN ('grabbed','downloading'))
         ORDER BY e.season_number, e.episode_number",
    )?;
    let rows: Vec<(i64, String, i64, i64)> = stmt
        .query_map(params![media_item_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(episode_id, title, season, episode)| SearchTarget {
            media_item_id,
            episode_id: Some(episode_id),
            season_number: Some(season as u32),
            query: build_tv_query(&title, season, episode, SearchRoute::Tpb),
            title_words: significant_words(&title),
            route: SearchRoute::Tpb,
            upgrade_min_gain: None,
        })
        .collect())
}

pub fn find_media_item_id_by_title(conn: &Connection, title: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM media_item WHERE title = ?1",
        params![title],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_search_targets(
    conn: &Connection,
    targets: &[SearchTarget],
    tpb: &sources::tpb::TpbSource,
    tpb_source_id: i64,
    scrape: &sources::scrape::ScrapeSource,
    scrape_source_id: i64,
    nyaa_search: &sources::rss::RssSource,
    nyaa_source_id: i64,
    matcher: &mut TitleMatcher,
    qbit: &QbitClient,
    qbit_category: &str,
) -> Result<SearchCycleStats> {
    let mut stats = SearchCycleStats {
        targets: targets.len(),
        ..Default::default()
    };

    let mut consecutive_fetch_errors = 0u32;
    // TPB queries are title-only (see `build_tv_query`), so every missing
    // episode of the same show produces the *identical* query string —
    // without this, a 10-episode backlog fires 10 indistinguishable
    // requests at a single-domain API with no mirror fallback, which is
    // exactly the kind of pattern that gets a source rate-limited. Caches
    // which source actually answered too (the TPB→1337x fallback can mean
    // two different targets with the same query string were served by two
    // different sources), so a cache hit still attributes dedup/grab
    // records to the right source id.
    let mut query_cache: std::collections::HashMap<
        (SearchRoute, String),
        (Vec<RawReleaseItem>, i64),
    > = std::collections::HashMap::new();

    for (i, target) in targets.iter().enumerate() {
        let (primary, primary_id): (&dyn ReleaseSource, i64) = match target.route {
            SearchRoute::Tpb => (tpb, tpb_source_id),
            SearchRoute::X1337 => (scrape, scrape_source_id),
            SearchRoute::NyaaSearch => (nyaa_search, nyaa_source_id),
        };

        let cache_key = (target.route, target.query.clone());
        let (items, source_id) =
            if let Some((cached_items, cached_source_id)) = query_cache.get(&cache_key) {
                (cached_items.clone(), *cached_source_id)
            } else {
                if i > 0 {
                    let jitter_secs = fastrand::u64(8..=20);
                    tokio::time::sleep(std::time::Duration::from_secs(jitter_secs)).await;
                }

                let primary_result = primary.fetch(Some(&target.query)).await;
                // TPB is the primary route for general content, but a fetch
                // *failure* there (not just "no relevant results") falls back
                // to 1337x for the same query before giving up — keeps the
                // mirror-rotation/cooldown machinery built for 1337x as real
                // resilience rather than dead code, just no longer the first
                // choice given TPB's better precision. `result_source_id`
                // tracks which source actually produced whatever we end up
                // with, since dedup (`is_seen`/`mark_seen`) and grab records
                // are keyed by source id — attributing a 1337x-sourced guid to
                // TPB's source id would silently break dedup between the two.
                let (fetch_result, result_source_id) =
                    if primary_result.is_err() && matches!(target.route, SearchRoute::Tpb) {
                        tracing::warn!(
                            query = %target.query,
                            error = %primary_result.as_ref().unwrap_err(),
                            "TPB search failed, falling back to 1337x"
                        );
                        (scrape.fetch(Some(&target.query)).await, scrape_source_id)
                    } else {
                        (primary_result, primary_id)
                    };

                match fetch_result {
                    Ok(items) => {
                        consecutive_fetch_errors = 0;
                        query_cache.insert(cache_key, (items.clone(), result_source_id));
                        (items, result_source_id)
                    }
                    Err(e) => {
                        consecutive_fetch_errors += 1;
                        tracing::warn!(query = %target.query, error = %e, "search fetch failed");
                        record_target_attempt(conn, target, "error")?;
                        stats.errors += 1;
                        let exhausted = e
                            .downcast_ref::<sources::scrape::AllMirrorsFailed>()
                            .is_some()
                            || consecutive_fetch_errors >= 2;
                        if exhausted {
                            stats.source_exhausted = true;
                            break;
                        }
                        continue;
                    }
                }
            };

        let mut sorted = items;
        sorted.sort_by_key(release_sort_key);
        sorted.truncate(MAX_RESULTS_PER_SEARCH);

        // Computed once per target, over candidates that at least pass the
        // cheap relevance pre-filter (not the full title-matcher — running
        // that here just to compute this flag would duplicate the real
        // matching `process_item` already does per-item below). Used to
        // decide whether a sub-1080p candidate is a real downgrade or the
        // only option actually available for this target.
        let better_resolution_available = sorted.iter().any(|item| {
            passes_relevance_filter(&target.title_words, &item.title)
                && parser::parse(&item.title)
                    .resolution
                    .is_some_and(|r| r >= 1080)
        });

        // A query built for one target's title can surface a *different*
        // monitored show/movie in its results (1337x's search isn't tightly
        // scoped — related/similarly-tagged content shows up, e.g. "House
        // of the Dragon" search results including "Game of Thrones"
        // releases). `process_item` re-matches every result independently
        // and will correctly grab such an opportunistic hit if it's
        // genuinely missing — that's a real bonus, not a bug — but only
        // grabbing *this specific target* should mark it satisfied and stop
        // the scan; otherwise the target is wrongly recorded as resolved
        // while its own release goes unevaluated, potentially forever if
        // the same unrelated show keeps outranking it on seeders.
        let mut target_satisfied = false;
        for item in &sorted {
            if is_seen(conn, source_id, &item.guid)? {
                continue;
            }
            // 1337x's search does loose keyword matching, not phrase
            // matching — a query for a specific show/movie routinely
            // surfaces massively-seeded, completely unrelated top hits that
            // merely share one common word (see `passes_relevance_filter`).
            // Skip those before they reach the embedding matcher at all.
            if !passes_relevance_filter(&target.title_words, &item.title) {
                continue;
            }
            match process_item(
                conn,
                item,
                matcher,
                qbit,
                qbit_category,
                source_id,
                better_resolution_available,
                target.upgrade_min_gain,
            )
            .await
            {
                // `GateRejected` deliberately does not mark seen: the
                // common real case is a release rejected today for too few
                // seeders that climbs into viability by the next scheduled
                // search — re-evaluating it costs nothing extra (it's
                // already in the response just fetched), but permanently
                // skipping it would strand the item until a different
                // release happens to appear.
                Ok(ProcessOutcome::GateRejected(_)) => {}
                Ok(outcome) => {
                    mark_seen(conn, source_id, &item.guid)?;
                    match outcome {
                        ProcessOutcome::Grabbed {
                            media_item_id,
                            episode_id,
                            season_number,
                            ..
                        } => {
                            stats.grabbed += 1;
                            // A season-pack grab has no single `episode_id`
                            // of its own, but it still satisfies this
                            // target if the target's own episode falls in
                            // the season it covers — without this, the
                            // search cycle wouldn't recognize the pack as
                            // having answered a single-episode search, and
                            // would keep re-searching (and potentially
                            // re-grabbing) the same already-covered episode
                            // every cycle.
                            let satisfies_target = (media_item_id == target.media_item_id
                                && episode_id == target.episode_id)
                                || (media_item_id == target.media_item_id
                                    && season_number.is_some()
                                    && season_number == target.season_number);
                            if satisfies_target {
                                target_satisfied = true;
                                break;
                            }
                        }
                        ProcessOutcome::QueuedForReview => stats.queued_for_review += 1,
                        _ => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, title = %item.title, "failed to process search result");
                    stats.errors += 1;
                    // Not marked seen — same transient-failure reasoning as
                    // the feed path (`run_grab_cycle`).
                }
            }
        }
        let result = if target_satisfied {
            "grabbed"
        } else if sorted.is_empty() {
            "no_results"
        } else {
            "no_viable"
        };
        record_target_attempt(conn, target, result)?;
        stats.searched += 1;
    }

    Ok(stats)
}

fn source_route_name(route: SearchRoute) -> &'static str {
    match route {
        SearchRoute::Tpb => "tpb",
        SearchRoute::X1337 => "1337x",
        SearchRoute::NyaaSearch => "nyaa",
    }
}

/// Fetches and scores (or gate-rejects) candidates for one search target —
/// the same evaluation `execute_search_targets` does automatically, minus
/// the grab decision, surfaced instead for a human to choose from. Used by
/// the manual release picker: `episode_id` selects which of a media item's
/// several possible targets (missing episodes, or the movie itself) to
/// search for. Returns an empty list if there's no live target for that
/// episode/movie right now (already owned, unmonitored, or mid-grab) —
/// same "nothing to do" cases `enumerate_search_targets_for_media_item`
/// already excludes.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_candidates(
    conn: &Connection,
    media_item_id: i64,
    episode_id: Option<i64>,
    tpb: &sources::tpb::TpbSource,
    tpb_source_id: i64,
    scrape: &sources::scrape::ScrapeSource,
    scrape_source_id: i64,
    nyaa_search: &sources::rss::RssSource,
    nyaa_source_id: i64,
) -> Result<Vec<breadarr_shared::dto::ReleaseCandidate>> {
    let targets = enumerate_search_targets_for_media_item(conn, media_item_id)?;
    let Some(target) = targets.into_iter().find(|t| t.episode_id == episode_id) else {
        return Ok(Vec::new());
    };

    let (source, source_id): (&dyn ReleaseSource, i64) = match target.route {
        SearchRoute::Tpb => (tpb, tpb_source_id),
        SearchRoute::X1337 => (scrape, scrape_source_id),
        SearchRoute::NyaaSearch => (nyaa_search, nyaa_source_id),
    };
    let items = source.fetch(Some(&target.query)).await?;

    let media_item = get_media_item(conn, media_item_id)?;
    let anime = match media_item.tvdb_id {
        Some(id) => is_anime(conn, id)?,
        None => false,
    };
    let profile_kind = if media_item.kind == "movie" {
        ProfileKind::Movie
    } else {
        ProfileKind::Tv
    };
    let profile = load_quality_profile(conn, media_item.quality_profile_id, profile_kind)?;

    let mut sorted = items;
    sorted.sort_by_key(|i| std::cmp::Reverse(i.seeders.unwrap_or(0)));
    sorted.truncate(MAX_RESULTS_PER_SEARCH);

    let better_resolution_available = sorted.iter().any(|item| {
        passes_relevance_filter(&target.title_words, &item.title)
            && parser::parse(&item.title)
                .resolution
                .is_some_and(|r| r >= 1080)
    });

    let mut candidates = Vec::new();
    for item in &sorted {
        if !passes_relevance_filter(&target.title_words, &item.title) {
            continue;
        }
        let parsed = parser::parse(&item.title);
        let is_season_pack = looks_like_season_pack(&parsed);
        let gate_ctx = GateContext {
            seeders: item.seeders,
            size_bytes: item.size_bytes,
            runtime_minutes: None,
            has_english_audio: infer_has_english_audio(&item.title),
            is_anime: anime,
            better_resolution_available,
            is_season_pack,
        };
        let (score, rejected_reason) = match scoring::evaluate_gates(&parsed, &gate_ctx, &profile) {
            GateResult::Accept => (
                Some(scoring::score(
                    &parsed,
                    item.seeders.unwrap_or(0),
                    false,
                    &profile,
                )),
                None,
            ),
            GateResult::Reject(reason) => (None, Some(format!("{reason:?}"))),
        };
        candidates.push(breadarr_shared::dto::ReleaseCandidate {
            raw_title: item.title.clone(),
            link: item.link.clone(),
            guid: item.guid.clone(),
            source_id,
            source_name: source_route_name(target.route).to_string(),
            seeders: item.seeders,
            leechers: item.leechers,
            size_bytes: item.size_bytes,
            score,
            rejected_reason,
            resolution: parsed.resolution,
            is_repack: parsed.is_repack,
            is_season_pack,
        });
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(candidates)
}

/// Grabs a specific candidate a human picked from `fetch_candidates`'
/// output, bypassing the score-vs-existing-best `should_grab` comparison
/// entirely — a manual pick is an explicit override, not a competing
/// automatic decision. Whether the chosen release is a season pack is
/// re-derived from its own title rather than trusted from the candidate
/// list (the two must agree by construction, but re-deriving here means
/// this function's correctness doesn't depend on a caller passing the
/// right flag back).
#[allow(clippy::too_many_arguments)]
pub async fn grab_candidate(
    conn: &Connection,
    qbit: &QbitClient,
    qbit_category: &str,
    media_item_id: i64,
    episode_id: Option<i64>,
    source_id: i64,
    raw_title: &str,
    link: &str,
    guid: &str,
) -> Result<()> {
    let parsed = parser::parse(raw_title);
    let media_item = get_media_item(conn, media_item_id)?;
    let profile_kind = if media_item.kind == "movie" {
        ProfileKind::Movie
    } else {
        ProfileKind::Tv
    };
    let profile = load_quality_profile(conn, media_item.quality_profile_id, profile_kind)?;
    let season_pack_number = if looks_like_season_pack(&parsed) {
        parsed.season
    } else {
        None
    };
    let final_episode_id = if season_pack_number.is_some() {
        None
    } else {
        episode_id
    };
    // Real-time seeder data isn't available for a candidate picked from an
    // earlier fetch — same `seeders=0` fallback `finalize_review_approval`
    // already uses for the same reason, and for the same reason it's still
    // real signal from resolution/source/codec/etc., not a meaningless
    // hardcoded score.
    let score = scoring::score(&parsed, 0, false, &profile);

    let torrent_hash = match grab_and_capture_hash(qbit, link, qbit_category).await {
        Ok(hash) => hash,
        Err(e) if e.downcast_ref::<crate::qbit::MagnetRejected>().is_some() => {
            anyhow::bail!("qBittorrent rejected this release's magnet/torrent");
        }
        Err(e) => return Err(e),
    };
    let status = if torrent_hash.is_some() {
        "grabbed"
    } else {
        "failed"
    };
    record_grab(
        conn,
        media_item_id,
        final_episode_id,
        season_pack_number,
        source_id,
        raw_title,
        guid,
        score,
        None,
        qbit_category,
        torrent_hash.as_deref(),
        status,
    )?;
    Ok(())
}

struct ReviewQueueRow {
    raw_release_title: String,
    candidate_media_item_id: Option<i64>,
    link: Option<String>,
    source_id: Option<i64>,
    status: String,
}

fn get_review_row(conn: &Connection, review_id: i64) -> Result<ReviewQueueRow> {
    conn.query_row(
        "SELECT raw_release_title, candidate_media_item_id, link, source_id, status FROM review_queue WHERE id = ?1",
        params![review_id],
        |row| {
            Ok(ReviewQueueRow {
                raw_release_title: row.get(0)?,
                candidate_media_item_id: row.get(1)?,
                link: row.get(2)?,
                source_id: row.get(3)?,
                status: row.get(4)?,
            })
        },
    )
    .map_err(Into::into)
}

pub struct PreparedApproval {
    pub media_item_id: i64,
    pub episode_id: Option<i64>,
    /// Set only when this review item resolved to a season-pack grab — see
    /// `release.season_number`'s own doc comment for why `episode_id IS
    /// NULL` alone can't disambiguate which season.
    pub season_number: Option<u32>,
    pub source_id: i64,
    pub raw_release_title: String,
    pub link: String,
    /// Computed from the parsed title with `seeders=0` (real-time seeder
    /// data isn't available for a review-queue entry by approval time) —
    /// still real signal from resolution/source/codec/etc., unlike the
    /// hardcoded `0.0` this replaced, which made *any* future automatic
    /// candidate with a nonzero score look like an upgrade over a release a
    /// human had just explicitly confirmed.
    pub score: f32,
}

pub enum ApprovalPrep {
    Ready(PreparedApproval),
    NotPending,
    MissingGrabData,
    CouldNotResolveEpisode,
    NotMonitoredOrAlreadyHave,
}

/// Sync-only: reads everything needed to grab the reviewed release. Split
/// from the actual grab (which needs `&QbitClient` and `.await`s) because a
/// `rusqlite::Connection` isn't `Sync` — holding a reference to it across an
/// `.await` makes the enclosing future `!Send`, which axum's handler trait
/// requires. Callers using a shared `Mutex<Connection>` (e.g. the HTTP API)
/// must drop their lock guard between calling this and awaiting the grab.
pub fn prepare_review_approval(conn: &Connection, review_id: i64) -> Result<ApprovalPrep> {
    let row = get_review_row(conn, review_id)?;
    if row.status != "pending" {
        return Ok(ApprovalPrep::NotPending);
    }
    let (Some(media_item_id), Some(link), Some(source_id)) =
        (row.candidate_media_item_id, row.link, row.source_id)
    else {
        return Ok(ApprovalPrep::MissingGrabData);
    };

    let media_item = get_media_item(conn, media_item_id)?;
    let parsed = parser::parse(&row.raw_release_title);

    let mut episode_id: Option<i64> = None;
    let mut season_pack_number: Option<u32> = None;

    if media_item.kind == "movie" {
        if looks_like_episode(&parsed) {
            return Ok(ApprovalPrep::CouldNotResolveEpisode);
        }
        if movie_year_mismatch(parsed.year, media_item.year) {
            return Ok(ApprovalPrep::CouldNotResolveEpisode);
        }
        if !movie_needs_grab(conn, media_item.id)? {
            return Ok(ApprovalPrep::NotMonitoredOrAlreadyHave);
        }
    } else if looks_like_season_pack(&parsed) {
        let Some(season) = parsed.season else {
            return Ok(ApprovalPrep::CouldNotResolveEpisode);
        };
        if count_monitored_missing_episodes_in_season(conn, media_item.id, season)? == 0 {
            return Ok(ApprovalPrep::NotMonitoredOrAlreadyHave);
        }
        season_pack_number = Some(season);
    } else {
        let Some((season, episode)) = resolve_episode(conn, media_item.tvdb_id, &parsed)? else {
            return Ok(ApprovalPrep::CouldNotResolveEpisode);
        };
        let Some(eid) = find_monitored_missing_episode(conn, media_item.id, season, episode)?
        else {
            return Ok(ApprovalPrep::NotMonitoredOrAlreadyHave);
        };
        episode_id = Some(eid);
    }

    let profile_kind = if media_item.kind == "movie" {
        ProfileKind::Movie
    } else {
        ProfileKind::Tv
    };
    let profile = load_quality_profile(conn, media_item.quality_profile_id, profile_kind)?;
    let score = scoring::score(&parsed, 0, false, &profile);

    Ok(ApprovalPrep::Ready(PreparedApproval {
        media_item_id: media_item.id,
        episode_id,
        season_number: season_pack_number,
        source_id,
        raw_release_title: row.raw_release_title,
        link,
        score,
    }))
}

/// The actual grab — network/qBittorrent I/O only, no `Connection` involved,
/// safe to `.await` from anywhere.
pub async fn grab_prepared_approval(
    qbit: &QbitClient,
    qbit_category: &str,
    prepared: &PreparedApproval,
) -> Result<Option<String>> {
    grab_and_capture_hash(qbit, &prepared.link, qbit_category).await
}

/// Sync-only: records the grab and marks the review approved. Call after
/// [`grab_prepared_approval`] completes.
pub fn finalize_review_approval(
    conn: &Connection,
    review_id: i64,
    prepared: &PreparedApproval,
    qbit_category: &str,
    torrent_hash: Option<&str>,
) -> Result<()> {
    // Same reasoning as `process_item`'s `HashCaptureFailed` handling: a
    // `grabbed` row with a NULL hash is invisible to the importer and
    // strands the episode/movie forever, so an uncorrelated add is recorded
    // as `failed` instead, leaving it free to be grabbed again later.
    let status = if torrent_hash.is_some() {
        "grabbed"
    } else {
        "failed"
    };
    record_grab(
        conn,
        prepared.media_item_id,
        prepared.episode_id,
        prepared.season_number,
        prepared.source_id,
        &prepared.raw_release_title,
        &prepared.link,
        prepared.score,
        // No size available for a review-queue entry (the review row
        // carries no size_bytes) — this is the one `record_grab` call site
        // where `torrent_fetch.size_bytes` is legitimately NULL.
        None,
        qbit_category,
        torrent_hash,
        status,
    )?;
    conn.execute(
        "UPDATE review_queue SET status = 'approved' WHERE id = ?1",
        params![review_id],
    )?;
    Ok(())
}

pub fn reject_review(conn: &Connection, review_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE review_queue SET status = 'rejected' WHERE id = ?1 AND status = 'pending'",
        params![review_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_grab_when_nothing_exists_yet() {
        assert!(should_grab(10.0, false, None, 0.0));
    }

    #[test]
    fn should_grab_when_strictly_better_score() {
        assert!(should_grab(15.0, false, Some(10.0), 0.0));
    }

    #[test]
    fn should_not_grab_when_worse_or_equal_score() {
        assert!(!should_grab(10.0, false, Some(10.0), 0.0));
        assert!(!should_grab(5.0, false, Some(10.0), 0.0));
    }

    #[test]
    fn should_grab_repack_even_if_not_higher_scored() {
        assert!(should_grab(10.0, true, Some(10.0), 0.0));
        assert!(should_grab(8.0, true, Some(10.0), 0.0));
    }

    #[test]
    fn should_not_grab_when_score_gain_is_below_the_minimum_threshold() {
        // A 3-point improvement doesn't clear a 5-point minimum gain.
        assert!(!should_grab(13.0, false, Some(10.0), 5.0));
        // An 8-point improvement does.
        assert!(should_grab(18.0, false, Some(10.0), 5.0));
    }

    #[test]
    fn should_grab_repack_even_below_the_minimum_gain_threshold() {
        assert!(should_grab(10.0, true, Some(10.0), 5.0));
    }

    #[test]
    fn infers_english_audio_present_by_default() {
        assert!(infer_has_english_audio(
            "Some Show S01E01 1080p WEB-DL H264"
        ));
    }

    #[test]
    fn infers_no_english_audio_for_vostfr() {
        assert!(!infer_has_english_audio(
            "Some Show S01E01 VOSTFR 1080p WEB x264"
        ));
    }

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, tvdb_id, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Some Show', 12345, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 1, 1, 0)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn finds_a_monitored_missing_episode() {
        let conn = seeded_conn();
        let episode_id = find_monitored_missing_episode(&conn, 1, 1, 1).unwrap();
        assert!(episode_id.is_some());
    }

    /// Two owned, monitored, equally-due episodes — one with a real
    /// probe-verified problem, one clean. A budget of 1 forces the ordering
    /// to matter: the flagged one must come back, not whichever happened to
    /// be inserted (or aired) first.
    fn seeded_upgrade_conn_with_one_flagged_episode() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Some Show', 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 1, 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (2, 1, 1, 2, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (1, 1, 1, '/tmp/s01e01.mkv', 100, 'none')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (id, episode_id, media_item_id, path, size_bytes, subtitle_status)
             VALUES (2, 2, 1, '/tmp/s01e02.mkv', 100, 'none')",
            [],
        )
        .unwrap();
        // Episode 1's file: clean, nothing flagged.
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime, corruption_status)
             VALUES (1, datetime('now'), 100, 0, 'probe_ok')",
            [],
        )
        .unwrap();
        // Episode 2's file: probe-verified under-quality.
        conn.execute(
            "INSERT INTO media_file_probe (episode_file_id, probed_at, probe_size_bytes, probe_mtime, corruption_status, flag_under_quality)
             VALUES (2, datetime('now'), 100, 0, 'probe_ok', 1)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn enumerate_upgrade_targets_prioritizes_a_probe_flagged_episode_over_a_clean_one() {
        let conn = seeded_upgrade_conn_with_one_flagged_episode();
        let targets = enumerate_upgrade_targets(&conn, 1, 5.0).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].episode_id, Some(2));
    }

    #[test]
    fn enumerate_upgrade_targets_returns_both_when_budget_allows() {
        let conn = seeded_upgrade_conn_with_one_flagged_episode();
        let targets = enumerate_upgrade_targets(&conn, 10, 5.0).unwrap();
        assert_eq!(targets.len(), 2);
        // Flagged episode still sorts first even when both fit.
        assert_eq!(targets[0].episode_id, Some(2));
        assert_eq!(targets[1].episode_id, Some(1));
    }

    #[test]
    fn record_grab_writes_both_a_release_row_and_a_torrent_fetch_audit_row() {
        let conn = seeded_conn();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'https://example.invalid')",
            [],
        )
        .unwrap();

        record_grab(
            &conn,
            1,
            Some(1),
            None,
            1,
            "Some.Show.S01E01.1080p",
            "guid-1",
            8.5,
            Some(1_234_567_890),
            "breadarr",
            Some("deadbeef"),
            "grabbed",
        )
        .unwrap();

        let release_count: i64 = conn
            .query_row("SELECT count(*) FROM release", [], |r| r.get(0))
            .unwrap();
        assert_eq!(release_count, 1);

        let (hash, name, size, category, status): (
            Option<String>,
            String,
            Option<i64>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT torrent_hash, name, size_bytes, category, status FROM torrent_fetch",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(hash.as_deref(), Some("deadbeef"));
        assert_eq!(name, "Some.Show.S01E01.1080p");
        assert_eq!(size, Some(1_234_567_890));
        assert_eq!(category.as_deref(), Some("breadarr"));
        assert_eq!(status, "grabbed");
    }

    #[test]
    fn record_grab_still_logs_to_torrent_fetch_when_hash_capture_failed() {
        let conn = seeded_conn();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'https://example.invalid')",
            [],
        )
        .unwrap();

        record_grab(
            &conn,
            1,
            Some(1),
            None,
            1,
            "Some.Show.S01E01.1080p",
            "guid-1",
            8.5,
            None,
            "breadarr",
            None,
            "failed",
        )
        .unwrap();

        let (hash, status): (Option<String>, String) = conn
            .query_row("SELECT torrent_hash, status FROM torrent_fetch", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(hash, None);
        assert_eq!(status, "failed");
    }

    fn insert_pending_review(conn: &Connection, raw_title: &str) -> i64 {
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'tpb', 'scrape', 'https://example.invalid')",
            [],
        )
        .ok(); // ignore if a test already inserted source id 1
        conn.execute(
            "INSERT INTO review_queue (raw_release_title, candidate_media_item_id, confidence, link, source_id, status, created_at)
             VALUES (?1, 1, 0.5, 'magnet:?xt=urn:btih:deadbeef', 1, 'pending', datetime('now'))",
            params![raw_title],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn does_not_find_an_unmonitored_or_already_have_episode() {
        let conn = seeded_conn();
        assert!(find_monitored_missing_episode(&conn, 1, 1, 2)
            .unwrap()
            .is_none());

        conn.execute(
            "UPDATE episode SET has_file = 1 WHERE season_number = 1 AND episode_number = 1",
            [],
        )
        .unwrap();
        assert!(find_monitored_missing_episode(&conn, 1, 1, 1)
            .unwrap()
            .is_none());
    }

    #[test]
    fn resolves_direct_season_episode_without_anime_map() {
        let conn = seeded_conn();
        let parsed = parser::parse("Some Show S01E05 1080p WEB-DL");
        assert_eq!(
            resolve_episode(&conn, Some(12345), &parsed).unwrap(),
            Some((1, 5))
        );
    }

    #[test]
    fn resolves_absolute_episode_via_anime_map() {
        let conn = seeded_conn();
        conn.execute(
            "INSERT INTO anime_mapping (anidb_id, tvdb_id, season_offset, episode_offset) VALUES (1, 12345, 1, 0)",
            [],
        )
        .unwrap();
        let parsed = parser::parse("[Group] Some Show - 07 [1080p]");
        assert_eq!(
            resolve_episode(&conn, Some(12345), &parsed).unwrap(),
            Some((1, 7))
        );
    }

    fn seeded_movie_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
             VALUES (1, 'movie', 'Some Movie', 2016, 98765, 1, 2, '/tmp')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn load_quality_profile_uses_defaults_for_the_seeded_empty_weights_row() {
        let conn = seeded_movie_conn();
        // db::init seeds quality_profile id=2 ("Default Movie") with weights='{}'.
        let profile = load_quality_profile(&conn, 2, ProfileKind::Movie).unwrap();
        assert_eq!(profile.weights.hdr, 2.0);
        assert_eq!(profile.weights.resolution_tier, 3.0);
    }

    #[test]
    fn load_quality_profile_applies_a_stored_override() {
        let conn = seeded_movie_conn();
        conn.execute(
            "UPDATE quality_profile SET weights = '{\"hdr\": 8.0}' WHERE id = 2",
            [],
        )
        .unwrap();
        let profile = load_quality_profile(&conn, 2, ProfileKind::Movie).unwrap();
        assert_eq!(profile.weights.hdr, 8.0);
        // Untouched axes still come from the built-in movie default.
        assert_eq!(profile.weights.resolution_tier, 3.0);
    }

    #[test]
    fn movie_needs_grab_when_monitored_and_missing() {
        let conn = seeded_movie_conn();
        assert!(movie_needs_grab(&conn, 1).unwrap());
    }

    #[test]
    fn prepares_a_movie_review_approval_with_no_episode_id() {
        let conn = seeded_movie_conn();
        let review_id = insert_pending_review(&conn, "Some Movie 2016 1080p BluRay x264");

        match prepare_review_approval(&conn, review_id).unwrap() {
            ApprovalPrep::Ready(prepared) => {
                assert_eq!(prepared.media_item_id, 1);
                assert_eq!(prepared.episode_id, None);
            }
            _ => panic!("expected ApprovalPrep::Ready for a monitored, file-less movie"),
        }
    }

    #[test]
    fn rejects_a_movie_review_whose_title_looks_like_an_episode() {
        let conn = seeded_movie_conn();
        let review_id = insert_pending_review(&conn, "Some Movie S01E02 1080p WEB-DL");

        assert!(matches!(
            prepare_review_approval(&conn, review_id).unwrap(),
            ApprovalPrep::CouldNotResolveEpisode
        ));
    }

    #[test]
    fn rejects_a_movie_review_with_a_mismatched_year() {
        let conn = seeded_movie_conn();
        let review_id = insert_pending_review(&conn, "Some Movie (1999) 1080p BluRay x264");

        assert!(matches!(
            prepare_review_approval(&conn, review_id).unwrap(),
            ApprovalPrep::CouldNotResolveEpisode
        ));
    }

    #[test]
    fn movie_does_not_need_grab_when_unmonitored() {
        let conn = seeded_movie_conn();
        conn.execute("UPDATE media_item SET monitored = 0 WHERE id = 1", [])
            .unwrap();
        assert!(!movie_needs_grab(&conn, 1).unwrap());
    }

    #[test]
    fn movie_does_not_need_grab_once_it_has_a_file() {
        let conn = seeded_movie_conn();
        conn.execute(
            "INSERT INTO episode_file (media_item_id, episode_id, path, size_bytes)
             VALUES (1, NULL, '/tmp/movie.mkv', 100)",
            [],
        )
        .unwrap();
        assert!(!movie_needs_grab(&conn, 1).unwrap());
    }

    #[test]
    fn movie_does_not_need_grab_while_a_release_is_in_flight() {
        let conn = seeded_movie_conn();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'test', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (media_item_id, episode_id, raw_title, source_id, guid, status, grabbed_at)
             VALUES (1, NULL, 'Some Movie 2016', 1, 'guid-1', 'grabbed', datetime('now'))",
            [],
        )
        .unwrap();
        assert!(!movie_needs_grab(&conn, 1).unwrap());
    }

    #[test]
    fn movie_eligible_for_upgrade_is_true_once_it_already_has_a_file() {
        let conn = seeded_movie_conn();
        conn.execute(
            "INSERT INTO episode_file (media_item_id, episode_id, path, size_bytes)
             VALUES (1, NULL, '/tmp/movie.mkv', 100)",
            [],
        )
        .unwrap();
        // Unlike `movie_needs_grab`, having a file already doesn't disqualify
        // an upgrade check — that's the whole point of the upgrade path.
        assert!(movie_eligible_for_upgrade(&conn, 1).unwrap());
    }

    #[test]
    fn movie_eligible_for_upgrade_is_false_when_unmonitored() {
        let conn = seeded_movie_conn();
        conn.execute("UPDATE media_item SET monitored = 0 WHERE id = 1", [])
            .unwrap();
        assert!(!movie_eligible_for_upgrade(&conn, 1).unwrap());
    }

    #[test]
    fn movie_eligible_for_upgrade_is_false_while_a_release_is_in_flight() {
        let conn = seeded_movie_conn();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'test', 'scrape', 'http://x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (media_item_id, episode_id, raw_title, source_id, guid, status, grabbed_at)
             VALUES (1, NULL, 'Some Movie 2016', 1, 'guid-1', 'downloading', datetime('now'))",
            [],
        )
        .unwrap();
        assert!(!movie_eligible_for_upgrade(&conn, 1).unwrap());
    }

    #[test]
    fn find_monitored_episode_ignores_has_file_but_still_requires_monitored() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO media_item (id, kind, title, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Some Show', 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 2, 0, 1)",
            [],
        )
        .unwrap();

        assert!(find_monitored_episode(&conn, 1, 1, 1).unwrap().is_some());
        assert!(find_monitored_episode(&conn, 1, 1, 2).unwrap().is_none());
    }

    #[test]
    fn looks_like_episode_detects_any_episode_marker() {
        assert!(!looks_like_episode(&parser::parse(
            "Some Movie 2016 1080p BluRay"
        )));
        assert!(looks_like_episode(&parser::parse(
            "Some Show S01E02 1080p WEB-DL"
        )));
        assert!(looks_like_episode(&parser::parse(
            "[Group] Some Show - 07 [1080p]"
        )));
    }

    #[test]
    fn looks_like_season_pack_detects_batch_releases() {
        assert!(looks_like_season_pack(&parser::parse(
            "Some Show (S01 Complete) 1080p WEB-DL"
        )));
        assert!(looks_like_season_pack(&parser::parse(
            "Some Show S01E01-E12 1080p WEB-DL"
        )));
    }

    #[test]
    fn looks_like_season_pack_is_false_for_a_single_episode() {
        assert!(!looks_like_season_pack(&parser::parse(
            "Some Show S01E02 1080p WEB-DL"
        )));
        assert!(!looks_like_season_pack(&parser::parse(
            "Some Movie 2016 1080p BluRay"
        )));
    }

    #[test]
    fn release_sort_key_prefers_a_season_pack_over_a_higher_seeded_episode() {
        let pack = RawReleaseItem {
            title: "Some Show (S02 Complete) 1080p WEB-DL".into(),
            link: String::new(),
            guid: "pack".into(),
            size_bytes: None,
            seeders: Some(10),
            leechers: None,
        };
        let episode = RawReleaseItem {
            title: "Some Show S02E05 1080p WEB-DL".into(),
            link: String::new(),
            guid: "episode".into(),
            size_bytes: None,
            seeders: Some(500),
            leechers: None,
        };
        let mut items = vec![episode.clone(), pack.clone()];
        items.sort_by_key(release_sort_key);
        assert_eq!(items[0].guid, "pack");
        assert_eq!(items[1].guid, "episode");
    }

    #[test]
    fn release_sort_key_falls_back_to_seeders_within_the_same_group() {
        let low = RawReleaseItem {
            title: "Some Show S02E05 1080p WEB-DL".into(),
            link: String::new(),
            guid: "low".into(),
            size_bytes: None,
            seeders: Some(5),
            leechers: None,
        };
        let high = RawReleaseItem {
            title: "Some Show S02E05 720p WEB-DL".into(),
            link: String::new(),
            guid: "high".into(),
            size_bytes: None,
            seeders: Some(50),
            leechers: None,
        };
        let mut items = vec![low.clone(), high.clone()];
        items.sort_by_key(release_sort_key);
        assert_eq!(items[0].guid, "high");
        assert_eq!(items[1].guid, "low");
    }

    #[test]
    fn count_monitored_missing_episodes_in_season_counts_correctly() {
        let conn = seeded_conn();
        // seeded_conn already has one monitored, missing episode (S01E01).
        conn.execute(
            "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 2, 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 3, 1, 1)", // already has a file, shouldn't count
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (media_item_id, season_number, episode_number, monitored, has_file)
             VALUES (1, 1, 4, 0, 0)", // unmonitored, shouldn't count
            [],
        )
        .unwrap();
        assert_eq!(
            count_monitored_missing_episodes_in_season(&conn, 1, 1).unwrap(),
            2
        );
        assert_eq!(
            count_monitored_missing_episodes_in_season(&conn, 1, 2).unwrap(),
            0
        );
    }

    #[test]
    fn find_episode_id_ignores_monitored_and_has_file_state() {
        let conn = seeded_conn();
        conn.execute(
            "UPDATE episode SET monitored = 0, has_file = 1 WHERE media_item_id = 1 AND season_number = 1 AND episode_number = 1",
            [],
        )
        .unwrap();
        // find_monitored_missing_episode should now see nothing...
        assert!(find_monitored_missing_episode(&conn, 1, 1, 1)
            .unwrap()
            .is_none());
        // ...but find_episode_id (identity-only lookup) still finds it.
        assert!(find_episode_id(&conn, 1, 1, 1).unwrap().is_some());
    }

    #[test]
    fn looks_like_non_video_format_catches_common_ebook_and_comic_markers() {
        assert!(looks_like_non_video_format(
            "Boy Swallows Universe by Trent Dalton EPUB"
        ));
        assert!(looks_like_non_video_format(
            "Mushoku Tensei Jobless Reincarnation (Light Novel) by Rifujin na Magonote EPUB"
        ));
        assert!(looks_like_non_video_format(
            "Batman 001 (2020) (Digital) (CBR)"
        ));
        assert!(!looks_like_non_video_format(
            "Boy Swallows Universe S01 COMPLETE 720p WEBRip x264"
        ));
        assert!(!looks_like_non_video_format(
            "Some Movie 2016 1080p BluRay x264"
        ));
    }

    #[test]
    fn movie_year_mismatch_rejects_far_apart_years_only() {
        assert!(!movie_year_mismatch(None, Some(2016)));
        assert!(!movie_year_mismatch(Some(2016), None));
        assert!(!movie_year_mismatch(Some(2016), Some(2016)));
        assert!(!movie_year_mismatch(Some(2016), Some(2017)));
        assert!(movie_year_mismatch(Some(1984), Some(2021)));
    }

    #[test]
    fn sanitize_query_text_strips_punctuation() {
        assert_eq!(
            sanitize_query_text("Kill: Ao / Blue?  Robot's Revenge"),
            "Kill Ao Blue Robot s Revenge"
        );
    }

    #[test]
    fn build_tv_query_formats_season_episode_for_x1337() {
        assert_eq!(
            build_tv_query("Some Show", 4, 13, SearchRoute::X1337),
            "Some Show S04E13"
        );
    }

    #[test]
    fn build_tv_query_is_title_only_for_tpb() {
        // apibay's search returns zero results once a season/episode token
        // is appended to a multi-word title, even when genuine matches for
        // that exact episode exist under a plain title search — verified
        // live against real show data.
        assert_eq!(
            build_tv_query("Some Show", 4, 13, SearchRoute::Tpb),
            "Some Show"
        );
    }

    #[test]
    fn build_movie_query_appends_year_when_known() {
        assert_eq!(build_movie_query("Arrival", Some(2016)), "Arrival 2016");
        assert_eq!(build_movie_query("Arrival", None), "Arrival");
    }

    /// A richer fixture than `seeded_conn`/`seeded_movie_conn` — covers both
    /// TV and movies at once, plus the specific rows each enumeration
    /// predicate needs to exclude.
    fn search_enumeration_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, base_url) VALUES (1, 'test-source', 'scrape', 'http://x')",
            [],
        )
        .unwrap();

        // A due, monitored, aired, non-anime episode — should be enumerated.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, tvdb_id, monitored, quality_profile_id, root_folder)
             VALUES (1, 'series', 'Some Show', 111, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file, air_date)
             VALUES (1, 1, 1, 1, 1, 0, '2020-01-01')",
            [],
        )
        .unwrap();

        // An unaired episode of the same show — excluded.
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file, air_date)
             VALUES (2, 1, 1, 2, 1, 0, '2999-01-01')",
            [],
        )
        .unwrap();

        // An aired episode already mid-grab — excluded.
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file, air_date)
             VALUES (3, 1, 1, 3, 1, 0, '2020-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release (media_item_id, episode_id, raw_title, source_id, guid, status, grabbed_at)
             VALUES (1, 3, 'x', 1, 'guid-inflight', 'grabbed', datetime('now'))",
            [],
        )
        .unwrap();

        // An anime series — its missing episode is excluded (stays on nyaa RSS).
        conn.execute(
            "INSERT INTO media_item (id, kind, title, tvdb_id, monitored, quality_profile_id, root_folder)
             VALUES (2, 'series', 'Some Anime', 222, 1, 1, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO anime_mapping (anidb_id, tvdb_id, season_offset, episode_offset) VALUES (1, 222, 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode (id, media_item_id, season_number, episode_number, monitored, has_file, air_date)
             VALUES (4, 2, 1, 1, 1, 0, '2020-01-01')",
            [],
        )
        .unwrap();

        // A due, monitored, missing movie (non-anime route).
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
             VALUES (3, 'movie', 'Some Movie', 2016, 333, 1, 2, '/tmp')",
            [],
        )
        .unwrap();

        // A movie already on disk — excluded.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
             VALUES (4, 'movie', 'Owned Movie', 2010, 444, 1, 2, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episode_file (media_item_id, episode_id, path, size_bytes)
             VALUES (4, NULL, '/tmp/owned.mkv', 100)",
            [],
        )
        .unwrap();

        // An anime movie — routed to nyaa search instead of 1337x.
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, anidb_id, monitored, quality_profile_id, root_folder)
             VALUES (5, 'movie', 'Some Anime Movie', 2019, 555, 1, 2, '/tmp')",
            [],
        )
        .unwrap();

        // An anime movie known only via `anime_tmdb_movie` (no anidb_id,
        // not in `anime_mapping` either) — the case that was silently
        // broken before `anime_map::refresh` started reading the
        // Fribb dataset's `movie` id list (verified live: "Your Name"
        // routed to TPB instead of nyaa until this was fixed).
        conn.execute(
            "INSERT INTO media_item (id, kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
             VALUES (6, 'movie', 'Some Other Anime Movie', 2016, 666, 1, 2, '/tmp')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO anime_tmdb_movie (tmdb_id) VALUES (666)", [])
            .unwrap();

        conn
    }

    #[test]
    fn enumerate_search_targets_excludes_unaired_inflight_and_anime() {
        let conn = search_enumeration_conn();
        let targets = enumerate_search_targets(&conn, 100).unwrap();

        assert!(targets.iter().any(|t| t.episode_id == Some(1)));
        assert!(
            !targets.iter().any(|t| t.episode_id == Some(2)),
            "unaired episode should be excluded"
        );
        assert!(
            !targets.iter().any(|t| t.episode_id == Some(3)),
            "in-flight episode should be excluded"
        );
        assert!(
            !targets.iter().any(|t| t.episode_id == Some(4)),
            "anime episode should be excluded"
        );
    }

    #[test]
    fn enumerate_search_targets_for_media_item_routes_anime_movie_via_tmdb_table() {
        let conn = search_enumeration_conn();
        // Same live-verified case ("Your Name") as the manual search-now
        // API route: `enumerate_search_targets_for_media_item` is a
        // separate query from the batch version above and must apply the
        // same `anime_tmdb_movie` check independently.
        let targets = enumerate_search_targets_for_media_item(&conn, 6).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].route, SearchRoute::NyaaSearch);

        let regular = enumerate_search_targets_for_media_item(&conn, 3).unwrap();
        assert_eq!(regular.len(), 1);
        assert_eq!(regular[0].route, SearchRoute::Tpb);
    }

    #[test]
    fn enumerate_search_targets_excludes_owned_movies_includes_missing() {
        let conn = search_enumeration_conn();
        let targets = enumerate_search_targets(&conn, 100).unwrap();

        assert!(targets
            .iter()
            .any(|t| t.media_item_id == 3 && t.episode_id.is_none()));
        assert!(
            !targets.iter().any(|t| t.media_item_id == 4),
            "movie that already has a file should be excluded"
        );
    }

    #[test]
    fn enumerate_search_targets_routes_anime_movies_to_nyaa_search() {
        let conn = search_enumeration_conn();
        let targets = enumerate_search_targets(&conn, 100).unwrap();
        let anime_movie = targets
            .iter()
            .find(|t| t.media_item_id == 5)
            .expect("anime movie should be enumerated");
        assert_eq!(anime_movie.route, SearchRoute::NyaaSearch);

        let anime_movie_via_tmdb_table = targets
            .iter()
            .find(|t| t.media_item_id == 6)
            .expect("anime_tmdb_movie-only anime movie should be enumerated");
        assert_eq!(anime_movie_via_tmdb_table.route, SearchRoute::NyaaSearch);

        let regular_movie = targets
            .iter()
            .find(|t| t.media_item_id == 3)
            .expect("regular movie should be enumerated");
        assert_eq!(regular_movie.route, SearchRoute::Tpb);
    }

    #[test]
    fn enumerate_search_targets_respects_budget() {
        let conn = search_enumeration_conn();
        let targets = enumerate_search_targets(&conn, 1).unwrap();
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn record_search_attempt_is_due_again_only_after_cadence_elapses() {
        let conn = search_enumeration_conn();

        // Freshly searched — cadence for a first attempt is 6h, so it must
        // not reappear immediately.
        record_search_attempt(&conn, 1, Some(1), "no_results").unwrap();
        let targets = enumerate_search_targets(&conn, 100).unwrap();
        assert!(
            !targets.iter().any(|t| t.episode_id == Some(1)),
            "just-searched episode should not be due yet"
        );

        // Back-date the attempt past the 6h cadence window.
        conn.execute(
            "UPDATE search_state SET last_searched_at = datetime('now', '-7 hours') WHERE episode_id = 1",
            [],
        )
        .unwrap();
        let targets = enumerate_search_targets(&conn, 100).unwrap();
        assert!(
            targets.iter().any(|t| t.episode_id == Some(1)),
            "episode past its cadence window should be due again"
        );
    }

    #[test]
    fn cadence_stays_backed_off_at_a_high_search_count_instead_of_overflowing() {
        // Regression test for a real SQLite integer overflow: uncapped,
        // `1 << (search_count - 1)` goes negative at search_count=64 (64-bit
        // signed shift), which flipped a chronically-failing target from
        // "back off weekly" to "due every single cycle" — the opposite of
        // the intended behavior, and worst exactly when backing off matters
        // most.
        let conn = search_enumeration_conn();
        record_search_attempt(&conn, 1, Some(1), "no_results").unwrap();
        conn.execute(
            "UPDATE search_state SET search_count = 64, last_searched_at = datetime('now', '-1 hour') WHERE episode_id = 1",
            [],
        )
        .unwrap();
        let targets = enumerate_search_targets(&conn, 100).unwrap();
        assert!(
            !targets.iter().any(|t| t.episode_id == Some(1)),
            "a target searched only 1 hour ago must not be due again yet, \
             regardless of how high its search_count has climbed"
        );
    }

    #[test]
    fn record_search_attempt_upserts_rather_than_duplicating() {
        let conn = search_enumeration_conn();
        record_search_attempt(&conn, 3, None, "no_results").unwrap();
        record_search_attempt(&conn, 3, None, "no_viable").unwrap();

        let (count, result): (i64, String) = conn
            .query_row(
                "SELECT search_count, last_result FROM search_state WHERE media_item_id = 3 AND episode_id IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(result, "no_viable");
    }

    #[test]
    fn enumerate_search_targets_prioritizes_never_searched_first() {
        let conn = search_enumeration_conn();
        // Mark the episode as already searched (but due again) so it's no
        // longer in the "never searched" group.
        conn.execute(
            "INSERT INTO search_state (media_item_id, episode_id, last_searched_at, search_count, last_result)
             VALUES (1, 1, datetime('now', '-7 hours'), 1, 'no_results')",
            [],
        )
        .unwrap();

        let targets = enumerate_search_targets(&conn, 100).unwrap();
        let episode_pos = targets
            .iter()
            .position(|t| t.episode_id == Some(1))
            .unwrap();
        let never_searched_movie_pos = targets
            .iter()
            .position(|t| t.media_item_id == 3 && t.episode_id.is_none())
            .unwrap();
        assert!(
            never_searched_movie_pos < episode_pos,
            "a never-searched item should sort before an overdue-but-already-searched one"
        );
    }

    #[test]
    fn significant_words_drops_short_and_punctuation_only_tokens() {
        assert_eq!(
            significant_words("Georgie & Mandy's First Marriage"),
            vec!["georgie", "first", "marriage"]
        );
    }

    #[test]
    fn relevance_filter_rejects_titles_missing_a_significant_word() {
        let words = significant_words("Modern Family");
        assert!(!passes_relevance_filter(
            &words,
            "Family.Guy.S22E15.1080p.WEB.H264-SuccessfulCrab"
        ));
        assert!(!passes_relevance_filter(
            &words,
            "The Addams Family (2019) [WEBRip] [1080p]"
        ));
    }

    #[test]
    fn relevance_filter_accepts_a_genuine_match() {
        let words = significant_words("Modern Family");
        assert!(passes_relevance_filter(
            &words,
            "Modern.Family.S11E01.1080p.WEB-DL.DDP5.1.H.264"
        ));
    }

    #[test]
    fn relevance_filter_lets_everything_through_for_titles_with_no_significant_words() {
        // e.g. a title that's entirely short/common words after filtering
        assert!(passes_relevance_filter(&[], "anything at all"));
    }
}
