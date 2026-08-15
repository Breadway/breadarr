use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItemSummary {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub monitored: bool,
    pub episode_count: i64,
    pub missing_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeSummary {
    pub id: i64,
    pub season_number: i64,
    pub episode_number: i64,
    pub title: Option<String>,
    pub air_date: Option<String>,
    pub monitored: bool,
    pub has_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItemDetail {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub monitored: bool,
    pub root_folder: String,
    pub episodes: Vec<EpisodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseSummary {
    pub id: i64,
    pub media_title: String,
    pub raw_title: String,
    pub score: Option<f64>,
    pub status: String,
    pub grabbed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewQueueEntry {
    pub id: i64,
    pub raw_release_title: String,
    pub candidate_media_title: Option<String>,
    pub confidence: f64,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalledGrab {
    pub release_id: i64,
    pub media_item_id: i64,
    pub media_title: String,
    pub raw_title: String,
    pub grabbed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxedSearchTarget {
    pub media_item_id: i64,
    pub media_title: String,
    pub search_count: i64,
    pub last_searched_at: Option<String>,
}

/// The daily "why don't I have this yet" answer — grabs that have been
/// sitting without importing longer than expected, how deep the review
/// queue has backed up, and search targets that have been failing every
/// attempt for so long their backoff has hit its ceiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StuckReport {
    pub stalled_grabs: Vec<StalledGrab>,
    pub review_queue_depth: i64,
    pub maxed_out_search_targets: Vec<MaxedSearchTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub external_id: String,
    pub title: String,
    pub year: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddSeriesRequest {
    pub tvdb_id: String,
    pub title: String,
    pub year: Option<i64>,
    pub aliases: Vec<String>,
    pub root_folder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddSeriesResponse {
    pub media_item_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMovieRequest {
    pub tmdb_id: String,
    pub title: String,
    pub year: Option<i64>,
    pub root_folder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMovieResponse {
    pub media_item_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchNowResult {
    pub targets: usize,
    pub searched: usize,
    pub grabbed: usize,
    pub errors: usize,
    pub source_exhausted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEntry {
    pub media_item_id: i64,
    pub media_title: String,
    pub season_number: i64,
    pub episode_number: i64,
    pub title: Option<String>,
    pub air_date: String,
    pub monitored: bool,
    pub has_file: bool,
}

/// Cheap liveness payload for unauthenticated `GET /health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthDetail {
    pub status: String,
    pub last_grab_cycle: Option<CycleInfo>,
    pub last_import_cycle: Option<CycleInfo>,
    pub last_search_cycle: Option<CycleInfo>,
    pub last_upgrade_cycle: Option<CycleInfo>,
    pub last_transcode_cycle: Option<CycleInfo>,
    pub search_halted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleInfo {
    pub at: chrono::DateTime<chrono::Utc>,
    pub ok: bool,
    pub detail: String,
}

/// One file flagged by a `media_file_probe` category — reused across every
/// flag list in `LibraryHealthReport` so a client can render them all with
/// the same widget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlaggedFile {
    pub episode_file_id: i64,
    pub media_title: String,
    pub episode_label: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub media_title: String,
    pub episode_label: Option<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecCount {
    pub codec: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibrarySummary {
    pub total_files: i64,
    pub total_size_bytes: i64,
    pub probed_files: i64,
    pub by_video_codec: Vec<CodecCount>,
    pub sd_count: i64,
    pub hd_720p_count: i64,
    pub full_hd_1080p_count: i64,
    pub uhd_4k_count: i64,
    pub pct_with_subtitles: f64,
}

/// One search result scored (or gate-rejected) for manual review — the
/// same candidate a search cycle would evaluate automatically, surfaced
/// before a grab decision is made instead of after. `link`/`guid`/
/// `source_id` are carried through so a chosen candidate can be grabbed
/// directly without re-searching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCandidate {
    pub raw_title: String,
    pub link: String,
    pub guid: String,
    pub source_id: i64,
    pub source_name: String,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub size_bytes: Option<u64>,
    /// `None` when gate-rejected — `rejected_reason` explains why.
    pub score: Option<f32>,
    pub rejected_reason: Option<String>,
    pub resolution: Option<u32>,
    pub is_repack: bool,
    pub is_season_pack: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrabCandidateRequest {
    pub raw_title: String,
    pub link: String,
    pub guid: String,
    pub source_id: i64,
}

/// Every scoring axis `QualityProfile::Weights` has, mirrored 1:1 for the
/// wire — used both to show the currently-effective (defaults-plus-stored-
/// override) values and to submit a full replacement set when the user
/// edits one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WeightsDto {
    pub seeder: f32,
    pub resolution_tier: f32,
    pub source_tier: f32,
    pub codec_tier: f32,
    pub bit_depth: f32,
    pub container: f32,
    pub group_allowlist: f32,
    pub repack: f32,
    pub hdr: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityProfileSummary {
    pub id: i64,
    pub name: String,
    pub kind: String,
    /// Always the fully-resolved values (built-in defaults with any stored
    /// override already applied) — never a partial/sparse object, so the
    /// TUI always has something concrete to display and re-submit.
    pub weights: WeightsDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateQualityProfileWeightsRequest {
    pub weights: WeightsDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryHealthReport {
    pub corrupt_files: Vec<FlaggedFile>,
    pub under_quality_files: Vec<FlaggedFile>,
    pub no_subtitle_files: Vec<FlaggedFile>,
    pub no_english_audio_files: Vec<FlaggedFile>,
    pub non_english_default_audio_files: Vec<FlaggedFile>,
    pub duplicate_groups: Vec<DuplicateGroup>,
    pub summary: LibrarySummary,
}
