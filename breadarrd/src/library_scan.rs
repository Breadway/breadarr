use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::importer;
use crate::matcher::TitleMatcher;
use crate::metadata::tmdb::TmdbClient;
use crate::metadata::tvdb::TvdbClient;
use crate::metadata::{self, MovieSearchResult, SeriesSearchResult};
use crate::parser;

#[derive(Debug, Default)]
pub struct ScanReport {
    pub matched: Vec<(String, i64)>,
    pub unmatched: Vec<String>,
    pub files_linked: usize,
    pub files_renamed: usize,
    pub files_reorganized: usize,
}

fn get_episode_title(conn: &Connection, episode_id: i64) -> Result<Option<String>> {
    conn.query_row(
        "SELECT title FROM episode WHERE id = ?1",
        params![episode_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .map_err(Into::into)
}

/// Renames `file` to `target_name` within the same directory (never moves
/// across directories/filesystems); a plain filesystem rename, so it's
/// fast and doesn't touch file content. Refuses to clobber an existing,
/// differently-named file already at the target path.
fn rename_in_place(file: &Path, target_name: &str) -> Result<std::path::PathBuf> {
    let Some(parent) = file.parent() else {
        return Ok(file.to_path_buf());
    };
    let target = parent.join(target_name);
    if target == *file {
        return Ok(file.to_path_buf());
    }
    if target.exists() {
        tracing::warn!(
            from = %file.display(),
            to = %target.display(),
            "normalization target already exists, leaving file as-is"
        );
        return Ok(file.to_path_buf());
    }
    std::fs::rename(file, &target)?;
    Ok(target)
}

// A "[Group]" or "[Tag]" prefix at the very start (common anime release-
// group convention, e.g. "[SubsPlease] Show Name"); stripped before any
// other parsing so it doesn't pollute the metadata search query or survive
// into the normalized title.
static LEADING_BRACKET_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[[^\]]+\]\s*").unwrap());
// "<Series> Movie 01 - <Subtitle>" is a common anime-movie flat-file naming
// convention, but TMDB's search is not fuzzy enough to see past the "Movie
// 01" token: it returns zero results for the full string even though
// "<Series> <Subtitle>" alone matches immediately. Only fires when a dash
// follows (an un-subtitled "Some Movie 2" is left alone).
static MOVIE_NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bmovie\s*\d{1,3}\b\s*-\s*").unwrap());
static FOLDER_BRACKET_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[(\[]((?:19|20)\d{2})[)\]]").unwrap());
static FOLDER_BARE_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b((?:19|20)\d{2})\b").unwrap());
// Common release-tag vocabulary (quality/source/codec/audio/season markers)
// that shows up in folder names that were never cleaned up after being
// dropped straight out of a torrent client: everything from the first
// match onward is discarded, since normalization keeps only Name and Year.
static FOLDER_JUNK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(1080p|720p|2160p|480p|4k|bluray|blu-ray|web-?dl|webrip|hdtv|dvdrip|remux|x264|x265|h\.?264|h\.?265|hevc|av1|dual[- ]?audio|multi[- ]?audio|dual|multi|proper|repack|extended|unrated|directors?\.?cut|10bit|8bit|complete|s\d{1,2}(?:e\d{1,3})?|season\s*\d+)\b",
    )
    .unwrap()
});

fn clean_title_edge(s: &str) -> String {
    s.trim()
        .trim_end_matches(['-', ':', '(', '['])
        .trim()
        .to_string()
}

/// Extracts just Name and Year out of a folder name, discarding everything
/// else: release-tag cruft (resolution/source/codec/group/season markers)
/// that's meaningful on a torrent's own filename has no business surviving
/// into the library's folder layout. "Arrival (2016)" -> ("Arrival",
/// Some(2016)); "Arrival.2016.1080p.BluRay.x264-GROUP" -> ("Arrival",
/// Some(2016)); "Attack on Titan S01 1080p Dual Audio [Group]" -> ("Attack
/// on Titan", None); "Black Adder" -> ("Black Adder", None).
fn parse_folder_name(name: &str) -> (String, Option<u32>) {
    let name = LEADING_BRACKET_TAG_RE.replace(name, "");
    let name = MOVIE_NUMBER_RE.replace(&name, "");
    let name = name.as_ref();

    // Scene-style names pack everything into dot/underscore-separated
    // tokens with no real spaces at all; normalize those to spaces before
    // hunting for a year/junk boundary. Left untouched whenever real spaces
    // are already present, so legitimately dotted titles ("Mr. Robot")
    // aren't mangled.
    let normalized = if !name.contains(' ') && (name.contains('.') || name.contains('_')) {
        name.replace(['.', '_'], " ")
    } else {
        name.to_string()
    };
    let normalized = parser::WS_RE
        .replace_all(normalized.trim(), " ")
        .to_string();

    if let Some(c) = FOLDER_BRACKET_YEAR_RE.captures(&normalized) {
        let year = c[1].parse().ok();
        let title = clean_title_edge(&normalized[..c.get(0).unwrap().start()]);
        return (title, year);
    }
    // A bare year is only trusted as *the* year when something precedes it
    // otherwise a movie literally titled after a year ("1917", "2012")
    // would have its own title mistaken for a year with nothing left over.
    if let Some(m) = FOLDER_BARE_YEAR_RE.find(&normalized) {
        if m.start() > 0 {
            let year = normalized[m.start()..m.end()].parse().ok();
            let title = clean_title_edge(&normalized[..m.start()]);
            return (title, year);
        }
    }
    if let Some(m) = FOLDER_JUNK_RE.find(&normalized) {
        return (clean_title_edge(&normalized[..m.start()]), None);
    }
    (normalized.trim().to_string(), None)
}

fn canonical_folder_name(title: &str, year: Option<u32>) -> String {
    match year {
        Some(y) => format!("{} ({y})", importer::sanitize(title)),
        None => importer::sanitize(title),
    }
}

/// Renames a media folder in place so its name is exactly the canonical
/// "{Title} ({Year})" form, discarding whatever release-tag/resolution/
/// group cruft the original folder name carried; normalization is meant to
/// leave nothing but name and year behind. Same-filesystem rename, doesn't
/// touch file content; refuses to clobber an existing, different folder
/// already at the target name; a no-op if already canonical.
fn normalize_folder_name(
    root: &Path,
    current_dir: &Path,
    title: &str,
    year: Option<u32>,
) -> Result<std::path::PathBuf> {
    let target_dir = root.join(canonical_folder_name(title, year));
    if target_dir == *current_dir {
        return Ok(current_dir.to_path_buf());
    }
    if target_dir.exists() {
        tracing::warn!(
            from = %current_dir.display(),
            to = %target_dir.display(),
            "normalized folder name already exists, leaving folder as-is"
        );
        return Ok(current_dir.to_path_buf());
    }
    std::fs::rename(current_dir, &target_dir)?;
    Ok(target_dir)
}

fn subdirectories(root: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries: Vec<_> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

/// Below this cosine similarity, the "best" candidate among *multiple*
/// options still isn't good enough to trust over the alternatives: treated
/// as no match at all. Only applied when there's more than one candidate to
/// choose between: a lone candidate is accepted unconditionally, since a
/// low score there usually just means the provider has no English name or
/// alias for it at all (e.g. TVDB's only entry for a well-known show can be
/// its native-script title with non-English aliases), not that it's wrong.
const MIN_MATCH_CONFIDENCE: f32 = 0.5;

/// TVDB/TMDB's own search relevance ranking isn't reliable enough to trust
/// blindly: e.g. searching "Attack on Titan" ranks the spinoff "Attack on
/// Titan: Counter Rockets" above the real 2013 series, whose own alias
/// list has the far-closer "Attack on Titan (2013)". Exact year match (when
/// the folder name has one) is tried first since it's a hard, cheap
/// signal; the embedding matcher (comparing the query against every
/// result's name *and* aliases, not just the top hit) breaks ties and
/// covers the (common) case where no year is available at all.
///
/// When a year *is* known but no exact match exists, candidates whose year
/// is off by more than one are excluded before the embedding pass; without
/// this, a title-only match (a misplaced TV season-pack folder colliding
/// with an unrelated same-named movie already in the catalog) can look
/// identical to a real hit purely on text similarity.
fn pick_series_match(
    matcher: &mut TitleMatcher,
    query: &str,
    results: Vec<SeriesSearchResult>,
    want_year: Option<u32>,
) -> Result<Option<SeriesSearchResult>> {
    // No "exact year match wins outright" shortcut here on purpose: a
    // year match alone doesn't disambiguate title (e.g. two same-year,
    // differently-titled shows), and bypassing the embedding-similarity
    // check below on year alone is the same incident class as the
    // Naruto-Kai merge documented further down this file, except upstream
    // of it: that guard only catches a bad match on *reuse* of an existing
    // row, not on picking the wrong search result for a brand-new add.
    // Year still narrows the candidate pool below, just never on its own.
    let pool = match want_year {
        Some(want) => year_filtered_pool(&results, want),
        None => results.iter().collect::<Vec<_>>(),
    };
    if pool.is_empty() {
        return Ok(None);
    }

    let mut candidates = Vec::new();
    for (idx, r) in pool.iter().enumerate() {
        candidates.push((idx, r.name.clone()));
        for alias in &r.aliases {
            candidates.push((idx, alias.clone()));
        }
    }
    let Some((best_idx, score)) = matcher.best_match_index(query, &candidates)? else {
        return Ok(None);
    };
    // Gated on the *pre-filter* candidate count, not `pool.len()`: the year
    // filter can take several wrong search results down to a single
    // survivor that merely has a nearby year, which is exactly the kind of
    // wrong-but-unopposed match this floor exists to catch. The single-
    // candidate bypass is only safe when the provider's search itself
    // returned one result (e.g. TVDB's only entry for a show has no English
    // name/alias at all, still unambiguous), not when filtering produced one.
    if results.len() > 1 && score < MIN_MATCH_CONFIDENCE {
        return Ok(None);
    }
    Ok(pool.get(best_idx).map(|r| (*r).clone()))
}

fn year_filtered_pool<T>(results: &[T], want: u32) -> Vec<&T>
where
    T: HasYear,
{
    results
        .iter()
        .filter(|r| r.year().is_none_or(|y| y.abs_diff(want) <= 1))
        .collect()
}

trait HasYear {
    fn year(&self) -> Option<u32>;
}

impl HasYear for SeriesSearchResult {
    fn year(&self) -> Option<u32> {
        self.year
    }
}

impl HasYear for MovieSearchResult {
    fn year(&self) -> Option<u32> {
        self.year
    }
}

fn pick_movie_match(
    matcher: &mut TitleMatcher,
    query: &str,
    results: Vec<MovieSearchResult>,
    want_year: Option<u32>,
) -> Result<Option<MovieSearchResult>> {
    // See `pick_series_match`: no exact-year-wins-outright shortcut here
    // either, for the same reason.
    let pool = match want_year {
        Some(want) => year_filtered_pool(&results, want),
        None => results.iter().collect::<Vec<_>>(),
    };
    if pool.is_empty() {
        return Ok(None);
    }

    // TMDB movie results don't carry an alias list the way TVDB does, but
    // running the same title through the matcher still catches the same
    // class of "wrong entry ranked first" problem when titles are close
    // but not identical (sequels, re-releases, regional retitles).
    let candidates: Vec<(usize, String)> = pool
        .iter()
        .enumerate()
        .map(|(idx, r)| (idx, r.title.clone()))
        .collect();
    let Some((best_idx, score)) = matcher.best_match_index(query, &candidates)? else {
        return Ok(None);
    };
    // See `pick_series_match`: gated on the pre-filter count, not `pool`.
    if results.len() > 1 && score < MIN_MATCH_CONFIDENCE {
        return Ok(None);
    }
    Ok(pool.get(best_idx).map(|r| (*r).clone()))
}

fn get_media_item_by_tvdb_id(conn: &Connection, tvdb_id: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM media_item WHERE tvdb_id = ?1",
        params![tvdb_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn get_media_item_by_tmdb_id(conn: &Connection, tmdb_id: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM media_item WHERE tmdb_id = ?1 AND kind = 'movie'",
        params![tmdb_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Guards against blindly repointing an existing row's `root_folder` onto
/// whatever folder a provider-ID lookup happened to match, without ever
/// checking the two actually agree on *which show*. A provider ID can end
/// up on the wrong row for reasons entirely outside this function's control
/// (a bad selection when the show was originally added, a data-quality
/// slip on the provider's own end); verified live: this exact gap let
/// "Boy Swallows Universe" (added with a TVDB ID that collided with Naruto
/// Kai's real one) silently absorb Naruto Kai's folder path on a later
/// scan, and Naruto Kai's own tracking row simply vanished, merged into an
/// unrelated show. Reuses the same embedding similarity the rest of the
/// matcher uses, so "close enough" here means the same thing it means
/// everywhere else in the app.
fn existing_row_title_plausibly_matches(
    conn: &Connection,
    matcher: &mut TitleMatcher,
    media_item_id: i64,
    candidate_title: &str,
) -> Result<bool> {
    let existing_title: String = conn.query_row(
        "SELECT title FROM media_item WHERE id = ?1",
        params![media_item_id],
        |row| row.get(0),
    )?;
    let score = matcher
        .best_match_index(candidate_title, &[(0, existing_title.clone())])?
        .map(|(_, score)| score)
        .unwrap_or(0.0);
    let plausible = score >= crate::matcher::MIN_CONFIDENCE;
    if !plausible {
        tracing::warn!(
            media_item_id,
            existing_title = %existing_title,
            candidate_title,
            score,
            "provider-ID match found an existing row, but its title doesn't \
             plausibly match the folder being scanned: refusing to repoint \
             its root_folder onto a possibly-unrelated show"
        );
    }
    Ok(plausible)
}

fn find_episode_id(
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

fn episode_has_file_row(conn: &Connection, episode_id: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM episode_file WHERE episode_id = ?1",
        params![episode_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn movie_has_file_row(conn: &Connection, media_item_id: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM episode_file WHERE media_item_id = ?1 AND episode_id IS NULL",
        params![media_item_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Imports every show folder under `root` as a monitored series: matches
/// it to TVDB, populates its full episode list, then walks its actual
/// files and marks whichever episodes are already present as `has_file`
/// (instead of a fresh "add show" which starts with nothing on disk):
/// idempotent, safe to re-run against the same root later.
pub async fn scan_tv_root(
    conn: &Connection,
    tvdb: &TvdbClient,
    matcher: &mut TitleMatcher,
    root: &Path,
    quality_profile_id: i64,
    jellyfin: Option<&crate::jellyfin::JellyfinClient>,
) -> Result<ScanReport> {
    let mut report = ScanReport::default();

    for entry in subdirectories(root)? {
        let folder_name = entry.file_name().to_string_lossy().to_string();
        let (title, year) = parse_folder_name(&folder_name);

        let results = match tvdb.search_series(&title).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(folder = %folder_name, error = %e, "tvdb search failed");
                report.unmatched.push(folder_name);
                continue;
            }
        };
        let Some(best) = pick_series_match(matcher, &title, results, year)? else {
            report.unmatched.push(folder_name);
            continue;
        };
        let Some(tvdb_id) = metadata::parse_external_id(&best.external_id) else {
            tracing::warn!(
                folder = %folder_name,
                external_id = %best.external_id,
                "tvdb id was not a positive integer, skipping"
            );
            report.unmatched.push(folder_name);
            continue;
        };
        let canonical_year = best.year.or(year);
        // Normalize the folder itself down to "Title (Year)": release
        // tags/resolution/group cruft in the original folder name isn't
        // wanted in the library layout, only in the source torrent name.
        let series_dir = normalize_folder_name(root, &entry.path(), &title, canonical_year)?;

        let existing_id = get_media_item_by_tvdb_id(conn, tvdb_id)?;
        let reuse_existing = match existing_id {
            Some(id) => existing_row_title_plausibly_matches(conn, matcher, id, &title)?,
            None => false,
        };

        let media_item_id = if let (Some(id), true) = (existing_id, reuse_existing) {
            conn.execute(
                "UPDATE media_item SET root_folder = ?1 WHERE id = ?2",
                params![series_dir.to_string_lossy(), id],
            )?;
            id
        } else {
            let episodes = tvdb.episodes(&best.external_id).await?;
            // Use the folder's own title, not TVDB's `name` field: for
            // non-English-origin shows that's often the original-language
            // title (e.g. "進撃の巨人" for Attack on Titan), while the
            // folder name reflects how the user actually organizes their
            // library already.
            metadata::insert_series(
                conn,
                &best.external_id,
                &title,
                canonical_year,
                &best.aliases,
                &series_dir.to_string_lossy(),
                quality_profile_id,
                &episodes,
            )?
        };
        report.matched.push((folder_name, media_item_id));

        for file in importer::walk_files(&series_dir)? {
            let ext = file
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !importer::VIDEO_EXTS.contains(&ext.as_str()) {
                continue;
            }
            let filename = file.file_name().unwrap_or_default().to_string_lossy();
            let parsed = parser::parse(&filename);
            // `parsed.season` is `None` for the dominant fansub naming
            // convention ("[SubsPlease] Show - 05.mkv": absolute episode
            // numbering only, no season marker at all), which previously
            // skipped linking these files entirely: `has_file` never got
            // set, so the daemon kept re-downloading episodes it already
            // had on disk. `crate::scheduler::resolve_episode` already
            // handles exactly this via the anime absolute-numbering map;
            // reused here instead of duplicating it.
            let Some((season, episode)) =
                crate::scheduler::resolve_episode(conn, Some(tvdb_id), &parsed)?
            else {
                continue;
            };
            let Some(episode_id) = find_episode_id(conn, media_item_id, season, episode)? else {
                continue;
            };
            if episode_has_file_row(conn, episode_id)? {
                continue;
            }

            let episode_title = get_episode_title(conn, episode_id)?;
            let target_name = importer::deterministic_filename(
                &title,
                season,
                episode,
                episode_title.as_deref(),
                &ext,
            );
            let final_path = match rename_in_place(&file, &target_name) {
                Ok(p) => {
                    if p != file {
                        report.files_renamed += 1;
                    }
                    p
                }
                Err(e) => {
                    tracing::warn!(file = %file.display(), error = %e, "rename failed, keeping original name");
                    file.clone()
                }
            };

            // Files imported before the season-folder convention existed
            // (or moved around by hand) can still be sitting flat in the
            // show root; move them under `Season NN` now rather than just
            // recording wherever they happen to already be. Same
            // filesystem as `series_dir`, so this is a plain rename.
            let season_folder = importer::season_dir(&series_dir.to_string_lossy(), season);
            let final_path = if final_path.parent() != Some(season_folder.as_path()) {
                match std::fs::create_dir_all(&season_folder).and_then(|_| {
                    let dest = season_folder.join(final_path.file_name().unwrap());
                    std::fs::rename(&final_path, &dest).map(|_| dest)
                }) {
                    Ok(dest) => {
                        report.files_reorganized += 1;
                        dest
                    }
                    Err(e) => {
                        tracing::warn!(file = %final_path.display(), error = %e, "season-folder move failed, keeping in place");
                        final_path
                    }
                }
            } else {
                final_path
            };

            let size = std::fs::metadata(&final_path)?.len();
            conn.execute(
                "INSERT INTO episode_file (episode_id, path, size_bytes, subtitle_status) VALUES (?1, ?2, ?3, 'none')",
                params![episode_id, final_path.to_string_lossy(), size],
            )?;
            let episode_file_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE episode SET has_file = 1 WHERE id = ?1",
                params![episode_id],
            )?;
            report.files_linked += 1;
            // Best-effort: a newly-linked file should get its ground-truth
            // metadata right away rather than waiting for the next
            // `probe_library` sweep, but a probing failure here must never
            // fail the scan itself.
            if let Err(e) = importer::ensure_probed(conn, episode_file_id, &final_path) {
                tracing::warn!(episode_file_id, error = %e, "post-scan probing failed");
            }
        }
    }

    // Folder renames (via `normalize_folder_name`) change paths Jellyfin
    // already has indexed under their old names: without an explicit
    // refresh here, Jellyfin's own library index silently falls out of
    // sync with disk until its *own* next scheduled scan happens to run,
    // which could be hours away. Verified live: exactly this happened;
    // renamed anime folders vanished from Jellyfin's library view because
    // nothing told it to look again.
    if !report.matched.is_empty() {
        if let Some(jellyfin) = jellyfin {
            refresh_jellyfin_after_rename(jellyfin, "tv scan").await;
        }
    }

    Ok(report)
}

/// Refreshes Jellyfin's library index after a scan that may have renamed
/// folders: a single fire-and-forget attempt previously left Jellyfin's
/// index silently stale for however long it took its own next scheduled
/// scan to notice on a transient failure (Jellyfin mid-restart, a momentary
/// network blip; the realistic failure mode, not a persistent outage).
/// Retries a few times with backoff before giving up, and logs at `error`
/// (not `warn`) once retries are exhausted, since a permanently stale
/// Jellyfin index is exactly the kind of thing worth actually noticing
/// rather than scrolling past in the journal.
async fn refresh_jellyfin_after_rename(jellyfin: &crate::jellyfin::JellyfinClient, context: &str) {
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        match jellyfin.refresh_library().await {
            Ok(()) => return,
            Err(e) if attempt < ATTEMPTS => {
                tracing::warn!(
                    error = %e,
                    attempt,
                    context,
                    "jellyfin library refresh failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_secs(5 * u64::from(attempt))).await;
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    context,
                    "jellyfin library refresh failed after {ATTEMPTS} attempts: its index may \
                     stay stale relative to disk until its own next scheduled scan runs"
                );
            }
        }
    }
}

/// One thing to match+import: either a "one folder per movie" entry (the
/// common layout) or a bare video file sitting directly under the root
/// (some libraries, e.g. this one's "Anime Movies", are organized flat,
/// one file per movie with no per-movie folder at all).
struct MovieCandidate {
    display_name: String,
    /// Where to search for the actual video file: the folder itself for a
    /// subdirectory entry, or the file's own path for a flat file.
    video_source: std::path::PathBuf,
}

fn movie_candidates(root: &Path) -> Result<Vec<MovieCandidate>> {
    let mut out: Vec<MovieCandidate> = subdirectories(root)?
        .into_iter()
        .map(|entry| MovieCandidate {
            display_name: entry.file_name().to_string_lossy().to_string(),
            video_source: entry.path(),
        })
        .collect();

    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !importer::VIDEO_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let display_name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        out.push(MovieCandidate {
            display_name,
            video_source: path,
        });
    }

    Ok(out)
}

/// Ensures a movie has its own folder before it's otherwise processed:
/// restructures a bare file directly under `root` into "{Title}
/// ({Year})/{file}", matching the "one folder per movie" convention the
/// rest of the library already uses (some libraries, e.g. this one's
/// "Anime Movies", were organized flat, one file per movie with no
/// per-movie folder at all). Same-filesystem rename, doesn't touch file
/// content; a no-op if `video_source` is already a folder.
fn ensure_movie_folder(
    root: &Path,
    video_source: &Path,
    title: &str,
    year: Option<u32>,
) -> Result<std::path::PathBuf> {
    if video_source.is_dir() {
        return normalize_folder_name(root, video_source, title, year);
    }
    let target_dir = root.join(canonical_folder_name(title, year));
    std::fs::create_dir_all(&target_dir)?;

    let file_name = video_source
        .file_name()
        .context("video file has no filename")?;
    let target_file = target_dir.join(file_name);
    if target_file != *video_source {
        std::fs::rename(video_source, &target_file)?;
    }
    Ok(target_dir)
}

/// Same idea as [`scan_tv_root`] but for movies: one media_item per
/// candidate (folder or bare file, restructured into its own folder if it
/// wasn't already one), matched to TMDB, with its video file linked as its
/// `episode_file` (movies have no `episode` row; `episode_id` is NULL).
pub async fn scan_movie_root(
    conn: &Connection,
    tmdb: &TmdbClient,
    matcher: &mut TitleMatcher,
    root: &Path,
    quality_profile_id: i64,
    jellyfin: Option<&crate::jellyfin::JellyfinClient>,
) -> Result<ScanReport> {
    let mut report = ScanReport::default();

    for candidate in movie_candidates(root)? {
        let folder_name = candidate.display_name;
        let (title, year) = parse_folder_name(&folder_name);

        let results = match tmdb.search_movie(&title).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(folder = %folder_name, error = %e, "tmdb search failed");
                report.unmatched.push(folder_name);
                continue;
            }
        };
        let Some(best) = pick_movie_match(matcher, &title, results, year)? else {
            report.unmatched.push(folder_name);
            continue;
        };
        let Some(tmdb_id) = metadata::parse_external_id(&best.external_id) else {
            tracing::warn!(
                folder = %folder_name,
                external_id = %best.external_id,
                "tmdb id was not a positive integer, skipping"
            );
            report.unmatched.push(folder_name);
            continue;
        };
        let canonical_year = best.year.or(year);

        // Normalizes the folder itself down to "Title (Year)" too: release
        // tags/resolution/group cruft from the original folder/file name
        // isn't wanted in the library layout, only in the source torrent
        // name (covers both a pre-existing messily-named folder and a flat
        // file getting its own folder created for the first time).
        let item_root_folder =
            ensure_movie_folder(root, &candidate.video_source, &title, canonical_year)?;

        let existing_id = get_media_item_by_tmdb_id(conn, tmdb_id)?;
        let reuse_existing = match existing_id {
            Some(id) => existing_row_title_plausibly_matches(conn, matcher, id, &title)?,
            None => false,
        };

        let media_item_id = if let (Some(id), true) = (existing_id, reuse_existing) {
            conn.execute(
                "UPDATE media_item SET root_folder = ?1 WHERE id = ?2",
                params![item_root_folder.to_string_lossy(), id],
            )?;
            id
        } else {
            conn.execute(
                "INSERT INTO media_item (kind, title, year, tmdb_id, monitored, quality_profile_id, root_folder)
                 VALUES ('movie', ?1, ?2, ?3, 1, ?4, ?5)",
                params![
                    title,
                    canonical_year,
                    tmdb_id,
                    quality_profile_id,
                    item_root_folder.to_string_lossy()
                ],
            )?;
            conn.last_insert_rowid()
        };
        report.matched.push((folder_name, media_item_id));

        if !movie_has_file_row(conn, media_item_id)? {
            if let Ok(file) = importer::largest_video_file(&item_root_folder) {
                let ext = file
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("mkv")
                    .to_lowercase();
                let target_name = importer::deterministic_movie_filename(
                    &title,
                    canonical_year.map(i64::from),
                    &ext,
                );
                let final_path = match rename_in_place(&file, &target_name) {
                    Ok(p) => {
                        if p != file {
                            report.files_renamed += 1;
                        }
                        p
                    }
                    Err(e) => {
                        tracing::warn!(file = %file.display(), error = %e, "rename failed, keeping original name");
                        file.clone()
                    }
                };

                let size = std::fs::metadata(&final_path)?.len();
                conn.execute(
                    "INSERT INTO episode_file (media_item_id, path, size_bytes, subtitle_status) VALUES (?1, ?2, ?3, 'none')",
                    params![media_item_id, final_path.to_string_lossy(), size],
                )?;
                let episode_file_id = conn.last_insert_rowid();
                report.files_linked += 1;
                if let Err(e) = importer::ensure_probed(conn, episode_file_id, &final_path) {
                    tracing::warn!(episode_file_id, error = %e, "post-scan probing failed");
                }
            }
        }
    }

    // See the matching comment in `scan_tv_root`: folder renames need an
    // explicit refresh or Jellyfin's index silently falls out of sync with
    // disk until its own next scheduled scan.
    if !report.matched.is_empty() {
        if let Some(jellyfin) = jellyfin {
            refresh_jellyfin_after_rename(jellyfin, "movie scan").await;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_folder_name_with_year() {
        assert_eq!(
            parse_folder_name("Arrival (2016)"),
            ("Arrival".to_string(), Some(2016))
        );
    }

    #[test]
    fn parses_folder_name_without_year() {
        assert_eq!(
            parse_folder_name("Black Adder"),
            ("Black Adder".to_string(), None)
        );
    }

    #[test]
    fn strips_scene_style_dot_separated_tags() {
        assert_eq!(
            parse_folder_name("Arrival.2016.1080p.BluRay.x264-GROUP"),
            ("Arrival".to_string(), Some(2016))
        );
    }

    #[test]
    fn strips_season_and_quality_tags_from_series_folder() {
        assert_eq!(
            parse_folder_name("Attack on Titan S01 1080p Dual Audio [Group]"),
            ("Attack on Titan".to_string(), None)
        );
    }

    #[test]
    fn keeps_bare_numeric_title_when_no_other_year_found() {
        assert_eq!(
            parse_folder_name("1917.1080p.BluRay.x264-GROUP"),
            ("1917".to_string(), None)
        );
    }

    #[test]
    fn does_not_mangle_titles_with_real_dots() {
        assert_eq!(
            parse_folder_name("Mr. Robot (2015)"),
            ("Mr. Robot".to_string(), Some(2015))
        );
    }

    #[test]
    fn strips_leading_release_group_bracket_tag_and_movie_number() {
        assert_eq!(
            parse_folder_name(
                "[Judas] Code Geass Movie 01 - Lelouch of the Rebellion - Initiation"
            ),
            (
                "Code Geass Lelouch of the Rebellion - Initiation".to_string(),
                None
            )
        );
    }

    #[test]
    fn parse_external_id_never_yields_zero() {
        assert_eq!(metadata::parse_external_id("0"), None);
        assert_eq!(metadata::parse_external_id("000"), None);
        assert_eq!(metadata::parse_external_id("not-a-number"), None);
        assert_eq!(metadata::parse_external_id("550"), Some(550));
    }
}
