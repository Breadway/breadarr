use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub qbit: QbitConfig,
    #[serde(default)]
    pub jellyfin: JellyfinConfig,
    #[serde(default)]
    pub tvdb: TvdbConfig,
    #[serde(default)]
    pub tmdb: TmdbConfig,
    #[serde(default)]
    pub library: LibraryConfig,
    #[serde(default)]
    pub sources: SourcesConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub transcode: TranscodeConfig,
}

/// Where the TUI's "add" flow places new series/movies by default. Sonarr/
/// Radarr let you pick a root folder per add; a single configured default per
/// kind is a reasonable v1 simplification — per-add picking can follow later.
/// Series and movies need *separate* defaults (not one shared value) because
/// they live under different category roots on disk (e.g. `TV Shows/` vs
/// `Movies/`) — a real production bug had both kinds falling back to one
/// bare library root with no per-item subfolder, landing new grabs directly
/// in the library root instead of inside their own show/movie folder,
/// invisible to Jellyfin's per-category libraries.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LibraryConfig {
    #[serde(default = "default_root_folder")]
    pub default_root_folder: String,
    #[serde(default = "default_movies_root_folder")]
    pub movies_root_folder: String,
}

fn default_root_folder() -> String {
    "~/breadarr-library".to_string()
}

fn default_movies_root_folder() -> String {
    "~/breadarr-library/Movies".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourcesConfig {
    /// 1337x's main domain bans IPs at the Cloudflare WAF level after
    /// bursts of automated traffic. Its community mirrors run on separate
    /// domains/Cloudflare zones, so a ban on one doesn't carry over — tried
    /// in order, falling back on failure, so losing one to a future ban
    /// doesn't take the source down. Verified working (not Cloudflare-
    /// challenged, genuine 1337x content) as of 2026-07-11.
    #[serde(default = "default_1337x_mirrors")]
    pub torrent_1337x_mirrors: Vec<String>,
    /// English-translated anime category — matches the actual anime library
    /// (nyaa carries no non-anime content, so a broader/unfiltered feed
    /// would just be pure noise against everything else monitored).
    #[serde(default = "default_nyaa_rss_url")]
    pub nyaa_rss_url: String,
    #[serde(default = "default_grab_poll_interval_secs")]
    pub grab_poll_interval_secs: u64,
    /// Human kill switch for the passive RSS-feed grab loop (nyaa, anime
    /// only) — same reasoning as `search_enabled`/`upgrade_enabled`, kept
    /// as its own flag since this loop watches a different source and can
    /// need to be paused independently of the search-driven ones.
    #[serde(default = "default_grab_enabled")]
    pub grab_enabled: bool,
    #[serde(default = "default_import_poll_interval_secs")]
    pub import_poll_interval_secs: u64,
    #[serde(default = "default_search_poll_interval_secs")]
    pub search_poll_interval_secs: u64,
    /// Search requests per cycle across 1337x + nyaa search combined — kept
    /// small since this is the one source with a ban history; clears a
    /// 50-item backlog in ~5 hours at the default interval without ever
    /// looking like a request flood to any single mirror.
    #[serde(default = "default_search_budget_per_cycle")]
    pub search_budget_per_cycle: usize,
    /// Human kill switch for the search-driven loop — a restart resets all
    /// in-memory mirror cooldown/backoff state, so this (not a persisted
    /// flag) is the deliberate way to keep it off across restarts.
    #[serde(default = "default_search_enabled")]
    pub search_enabled: bool,
    /// A community JSON API mirror of The Pirate Bay's search — the
    /// primary general-content (movies + non-anime TV) search source.
    /// Unlike 1337x this needs no HTML scraping and actually ranks by
    /// relevance rather than pure seeder count, which matters a lot for
    /// titles made of common words.
    #[serde(default = "default_tpb_api_url")]
    pub tpb_api_url: String,
    /// Human kill switch for the upgrade-search loop, same reasoning as
    /// `search_enabled` — off by default would mean nothing ever improves,
    /// but a user who's happy with their current files (or wants to save
    /// request budget) can disable it independently of missing-content
    /// search.
    #[serde(default = "default_upgrade_enabled")]
    pub upgrade_enabled: bool,
    /// Deliberately much longer than `search_poll_interval_secs` — this
    /// loop re-checks content that's already satisfied (a file exists), so
    /// there's no urgency the way a missing episode has, and every cycle
    /// still costs the same request budget as a missing-content search.
    #[serde(default = "default_upgrade_poll_interval_secs")]
    pub upgrade_poll_interval_secs: u64,
    #[serde(default = "default_upgrade_budget_per_cycle")]
    pub upgrade_budget_per_cycle: usize,
    /// Minimum score improvement (on top of the same weighted-score scale
    /// `should_grab` already compares) required before a periodic upgrade
    /// check will actually re-grab — without this, a file already on disk
    /// could get replaced over and over for score deltas too small to
    /// matter, wasting bandwidth on churn. Repacks/propers always supersede
    /// regardless of this threshold, same as the normal grab path.
    #[serde(default = "default_upgrade_min_score_gain")]
    pub upgrade_min_score_gain: f32,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            torrent_1337x_mirrors: default_1337x_mirrors(),
            nyaa_rss_url: default_nyaa_rss_url(),
            grab_poll_interval_secs: default_grab_poll_interval_secs(),
            grab_enabled: default_grab_enabled(),
            import_poll_interval_secs: default_import_poll_interval_secs(),
            search_poll_interval_secs: default_search_poll_interval_secs(),
            search_budget_per_cycle: default_search_budget_per_cycle(),
            search_enabled: default_search_enabled(),
            tpb_api_url: default_tpb_api_url(),
            upgrade_enabled: default_upgrade_enabled(),
            upgrade_poll_interval_secs: default_upgrade_poll_interval_secs(),
            upgrade_budget_per_cycle: default_upgrade_budget_per_cycle(),
            upgrade_min_score_gain: default_upgrade_min_score_gain(),
        }
    }
}

fn default_tpb_api_url() -> String {
    "https://apibay.org/q.php".to_string()
}

fn default_upgrade_enabled() -> bool {
    true
}

fn default_upgrade_poll_interval_secs() -> u64 {
    6 * 60 * 60
}

fn default_upgrade_budget_per_cycle() -> usize {
    3
}

fn default_upgrade_min_score_gain() -> f32 {
    5.0
}

fn default_search_poll_interval_secs() -> u64 {
    30 * 60
}

fn default_search_budget_per_cycle() -> usize {
    5
}

fn default_search_enabled() -> bool {
    true
}

fn default_nyaa_rss_url() -> String {
    "https://nyaa.si/?page=rss&c=1_2".to_string()
}

fn default_grab_poll_interval_secs() -> u64 {
    300
}

fn default_grab_enabled() -> bool {
    true
}

fn default_import_poll_interval_secs() -> u64 {
    60
}

fn default_1337x_mirrors() -> Vec<String> {
    [
        "https://13377x.info",
        "https://13377x.email",
        "https://1337xto.info",
        "https://1337x.maskbay.info",
        "https://1337x.ninjaproxy.live",
        "https://1337x.proxyhive.pro",
        "https://1337x.torproxy.live",
        "https://1337x.unblockit.world",
        "https://1337x.unblockpirate.xyz",
        "https://1337x.unblockshark.info",
        "https://1337x.unblocktorrent.click",
        "https://1337x.unblocktorrent.info",
        "https://1337x.unblocktor.xyz",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_db_path")]
    pub db_path: String,
    #[serde(default = "default_model_dir")]
    pub model_dir: String,
    /// Bearer token required on every API request when non-empty. Empty
    /// (the default) means auth is off entirely — allowed only when
    /// `listen_addr` is loopback. A non-loopback bind with an empty token
    /// is rejected at load. When set, the token must be at least 16
    /// characters so a length-oracle of short guesses is useless.
    #[serde(default)]
    pub api_token: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            listen_addr: default_listen_addr(),
            db_path: default_db_path(),
            model_dir: default_model_dir(),
            api_token: String::new(),
        }
    }
}

/// qBittorrent WebUI connection. `base_url` empty means "not configured" —
/// callers should error out rather than guessing a default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QbitConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_qbit_category")]
    pub category: String,
    /// qBittorrent's own container-internal path prefix for its downloads
    /// (e.g. "/downloads"), when it runs in Docker while breadarr runs
    /// natively on the same host — needed to translate the paths qBittorrent
    /// reports via its API into paths breadarr can actually open. Both empty
    /// means no remapping (qBittorrent's reported paths are used as-is).
    #[serde(default)]
    pub container_downloads_path: String,
    #[serde(default)]
    pub host_downloads_path: String,
}

/// Push-notification target for events that otherwise sit invisible until
/// the TUI is next opened (a review-queue item, a run of import failures,
/// the search-driven loop halting). `webhook_url` empty means "not
/// configured" — no notifications sent, matching every other optional
/// integration's default. The payload is a simple `{"title", "message"}`
/// JSON body, which is directly Gotify's own message API shape (this
/// user's actual self-hosted push service) and close enough to what most
/// other self-hosted webhook receivers expect that this isn't tied to one
/// specific service.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub webhook_url: String,
}

/// Jellyfin API connection. `base_url`/`api_key` empty means "not configured".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JellyfinConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

/// GPU-accelerated AV1 transcode: re-encodes freshly-grabbed and existing
/// library files down to a space-reasonable size instead of keeping
/// whatever the source release happened to be (REMUX, huge season packs,
/// etc). `enabled` defaults false — this needs a manual calibration pass
/// against real content on the target GPU before it's safe to run
/// unattended against a whole library.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscodeConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Tight on purpose — the actual pace is bottlenecked by encode time
    /// (minutes per file), not this interval; a short poll just means a
    /// freshly-completed job's slot gets refilled promptly instead of
    /// sitting idle for the rest of a longer interval.
    #[serde(default = "default_transcode_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_vaapi_device")]
    pub vaapi_device: String,
    /// Applies to both pipelines identically when Jellyfin reports an
    /// active transcoding session — deliberately not split per-pipeline;
    /// when someone's actually watching something, both the GPU (which
    /// they're using) and CPU (contending for the same box) should back off.
    #[serde(default = "default_parallelism_min")]
    pub parallelism_min: usize,
    /// The total concurrent **live-action** (`av1_vaapi`, GPU-bound) stream
    /// budget — not just "the batch job's cap", but the real ceiling the
    /// GPU can sustain at all, batch work and live Jellyfin viewers
    /// combined. `transcode::live_action_parallelism_for` subtracts however
    /// many Jellyfin transcode sessions are actually active from this
    /// number to get the batch job's actual parallelism each cycle, so
    /// real viewers get exactly the headroom they need rather than the
    /// batch job dropping to a flat minimum regardless of how many people
    /// are watching. Deliberately separate from `parallelism_max_anime` —
    /// the two pipelines contend for genuinely different hardware (GPU
    /// encode engine vs CPU threads), so raising one shouldn't raise the
    /// other. Empirically calibrated on Hestia's Arc A380: per-stream
    /// throughput stays above 1.5x realtime through 7 concurrent streams,
    /// crosses below it at 8 (aggregate throughput itself plateaus around
    /// ~12x realtime from 5-6 streams on, i.e. the GPU's actual saturation
    /// point) — see [[breadarr-av1-transcode]] for the full scaling-test
    /// numbers.
    #[serde(default = "default_parallelism_max")]
    pub parallelism_max: usize,
    /// Ramp-up ceiling for concurrent **anime** (`libsvtav1`, CPU-bound)
    /// encode streams — capped separately from `parallelism_max` (see its
    /// doc comment) precisely because a single shared cap would let "raise
    /// GPU parallelism" accidentally also raise anime concurrency, and
    /// anime jobs are CPU-thread-hungry (`anime_svtav1_max_threads` each)
    /// in a way live-action jobs aren't. Kept at the original
    /// conservative shared-cap default (2) since concurrent-anime-job
    /// memory/CPU behavior at higher counts hasn't been load-tested the
    /// way the live-action GPU path has.
    #[serde(default = "default_parallelism_max_anime")]
    pub parallelism_max_anime: usize,
    /// The "looks fine, no complaints" calibration reference: a real
    /// bitrate (Mbps, in kbps here) from content already in the library at
    /// `reference_height` that the user is happy with. New AV1 encodes are
    /// targeted relative to this, scaled by resolution and AV1's encoding
    /// efficiency, rather than picking a bitrate out of thin air.
    #[serde(default = "default_reference_bitrate_kbps")]
    pub reference_bitrate_kbps: u32,
    #[serde(default = "default_reference_height")]
    pub reference_height: u32,
    /// AV1 reaches equivalent perceived quality to HEVC at a meaningfully
    /// lower bitrate — this factor is applied on top of the resolution
    /// scaling so the AV1 target isn't just a like-for-like copy of the
    /// HEVC/H264 reference bitrate. Conservative (not maximally aggressive)
    /// on purpose: erring toward "still clearly smaller" over "as small as
    /// AV1 could theoretically go" leaves margin against visible artifacts.
    #[serde(default = "default_av1_efficiency_factor")]
    pub av1_efficiency_factor: f32,
    /// HDR10/Dolby Vision metadata preservation through the GPU encoder
    /// hasn't been verified yet — excluded from both the backfill and the
    /// post-grab path until that's specifically checked on a few samples.
    #[serde(default = "default_exclude_hdr")]
    pub exclude_hdr: bool,
    /// Excludes the 2160p tier from the first pass for the same reason as
    /// `exclude_hdr` (most current 4K content in this library is HDR
    /// anyway) — revisit once HDR handling is confirmed safe.
    #[serde(default = "default_exclude_min_height")]
    pub exclude_min_height: u32,
    /// `global_quality` for the live-action `av1_vaapi` `QVBR` encode — the
    /// actual quality driver now that rate control is quality-based rather
    /// than a flat bitrate target (see `run_ffmpeg_encode_live_action`).
    /// `reference_bitrate_kbps`/`av1_efficiency_factor` still compute a
    /// `-b:v`/`-maxrate`/`-bufsize` ceiling alongside this, so a source that's
    /// already unusually efficient doesn't get inflated up toward the
    /// ceiling — QVBR only spends up to it on content that actually needs it.
    #[serde(default = "default_quality_live_action")]
    pub quality_live_action: u32,
    /// Root folder path prefixes (exact string prefix match against
    /// `episode_file.path`) routed to the anime encode pipeline
    /// (`run_ffmpeg_encode_anime`) instead of the live-action one, regardless
    /// of `anime_mapping`/`anime_tmdb_movie` metadata coverage — path is a
    /// more reliable signal than TVDB/TMDB anime-list membership, which has
    /// real gaps (e.g. Avatar: The Last Airbender and some Dragon Ball movies
    /// were missing from those tables and slipped through as "not anime").
    /// Empty by default (a no-op) — set per-deployment to match how the
    /// library is actually organized.
    #[serde(default)]
    pub anime_root_folders: Vec<String>,
    /// CRF for the anime pipeline's software `libsvtav1` encode (0-63, lower
    /// = higher quality/larger). No hardware AV1 10-bit encode entrypoint
    /// exists on Hestia's Arc A380 (`vainfo` only lists `AV1Profile0`,
    /// 8-bit) — anime needs true 10-bit output to avoid banding in the flat
    /// gradients the art style is full of, so this pipeline trades GPU
    /// offload for CPU-based `libsvtav1` specifically to get it.
    #[serde(default = "default_quality_anime")]
    pub quality_anime: u32,
    /// `libsvtav1` preset (0-13, lower = slower/better compression AND more
    /// memory-hungry — SVT-AV1's lookahead/reference buffering scales with
    /// preset, not just thread count). Raised from an initial guess of 6 to
    /// 10 after a real validation run hit a genuine kernel OOM: preset 6 on
    /// a single 1080p anime episode grew to 9.3GB resident memory on
    /// Hestia's 6-core/12-thread box. This runs as unattended background
    /// work, so trading some compression efficiency for a much smaller,
    /// safer memory footprint is the right call — see
    /// `anime_svtav1_max_threads` for the other half of that fix.
    #[serde(default = "default_anime_svtav1_preset")]
    pub anime_svtav1_preset: u32,
    /// Passed to `libsvtav1` as `-svtav1-params lp=N` — caps how many
    /// worker threads it uses, independent of preset. More parallel workers
    /// means more concurrently-buffered frames, so this is the other lever
    /// (alongside `anime_svtav1_preset`) for bounding the encoder's peak
    /// memory to something predictable regardless of how many cores the
    /// host actually has. Default is conservative (well under a typical
    /// modern host's core count) after the same OOM incident that raised
    /// the preset default.
    #[serde(default = "default_anime_svtav1_max_threads")]
    pub anime_svtav1_max_threads: u32,
    /// Hard floor on what counts as "worth keeping": a transcode whose
    /// output isn't at least this fraction smaller than the original is
    /// discarded (job marked `skipped`, original left untouched) rather than
    /// swapped in. Exists because quality-driven rate control can still
    /// occasionally produce an output that's the same size as or larger than
    /// an already-efficient source — this is the invariant that makes that
    /// safe regardless of how good the rate-control tuning is, after a real
    /// incident where flat-bitrate VBR targeting silently produced files
    /// *larger* than the original on the majority of a backfill.
    #[serde(default = "default_min_size_reduction_pct")]
    pub min_size_reduction_pct: f64,
    /// Skip attempting a transcode at all (no GPU/CPU time spent) when the
    /// source's current bitrate is already at or below this fraction of the
    /// resolution-scaled ceiling (`target_bitrate_kbps`) — a strong signal
    /// there's little room left to save, so it's not worth the encode time
    /// to find out (the `min_size_reduction_pct` check above would reject
    /// most of these anyway, this just avoids paying for that finding).
    #[serde(default = "default_skip_below_ceiling_ratio")]
    pub skip_below_ceiling_ratio: f64,
    /// Size (seconds) of each of the three start/middle/end windows
    /// `ffprobe::verify_decodable_sampled` actually decodes, instead of the
    /// whole file — a full decode verification was measured as the actual
    /// CPU bottleneck of a transcode cycle (400%+ CPU per job, dwarfing the
    /// GPU encode time), not the encode itself. Bounds verification cost to
    /// a small constant regardless of source length.
    #[serde(default = "default_verify_sample_secs")]
    pub verify_sample_secs: f64,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: default_transcode_poll_interval_secs(),
            vaapi_device: default_vaapi_device(),
            parallelism_min: default_parallelism_min(),
            parallelism_max: default_parallelism_max(),
            parallelism_max_anime: default_parallelism_max_anime(),
            reference_bitrate_kbps: default_reference_bitrate_kbps(),
            reference_height: default_reference_height(),
            av1_efficiency_factor: default_av1_efficiency_factor(),
            exclude_hdr: default_exclude_hdr(),
            exclude_min_height: default_exclude_min_height(),
            quality_live_action: default_quality_live_action(),
            anime_root_folders: Vec::new(),
            quality_anime: default_quality_anime(),
            anime_svtav1_preset: default_anime_svtav1_preset(),
            anime_svtav1_max_threads: default_anime_svtav1_max_threads(),
            min_size_reduction_pct: default_min_size_reduction_pct(),
            skip_below_ceiling_ratio: default_skip_below_ceiling_ratio(),
            verify_sample_secs: default_verify_sample_secs(),
        }
    }
}

fn default_transcode_poll_interval_secs() -> u64 {
    60
}

fn default_vaapi_device() -> String {
    "/dev/dri/renderD128".to_string()
}

fn default_parallelism_min() -> usize {
    1
}

fn default_parallelism_max() -> usize {
    // The measured total GPU budget (not "batch cap plus a static
    // reservation") — `live_action_parallelism_for` dynamically subtracts
    // real Jellyfin transcode sessions from this each cycle. Raised from
    // an initial conservative guess of 2 after an actual concurrent-stream
    // scaling test on Hestia's Arc A380 (see `parallelism_max`'s doc
    // comment): per-stream throughput stays above 1.5x realtime through 7
    // total concurrent streams, crossing below at 8.
    7
}

fn default_parallelism_max_anime() -> usize {
    // Kept at the original conservative shared-cap value — unlike
    // `parallelism_max`, this hasn't been load-tested at higher counts.
    // A real incident already showed a *single* uncapped anime job could
    // hit 9.3GB resident memory; multiple concurrent anime jobs (each its
    // own `anime_svtav1_max_threads`-sized thread pool) multiply both CPU
    // thread contention and memory pressure in a way the GPU path doesn't
    // have to worry about. Raise deliberately, per-deployment, only after
    // watching `free -h` and CPU load under real concurrent-anime load.
    2
}

fn default_reference_bitrate_kbps() -> u32 {
    5320
}

fn default_reference_height() -> u32 {
    1080
}

fn default_av1_efficiency_factor() -> f32 {
    0.7
}

fn default_exclude_hdr() -> bool {
    true
}

fn default_exclude_min_height() -> u32 {
    2000
}

// Starting point for `av1_vaapi`'s `-global_quality` under `QVBR`, needs the
// same real-hardware calibration pass as the bitrate reference did — this is
// a reasonable guess (roughly x264/x265 "visually near-lossless" territory
// on the encoder's internal QP-like scale), not a measured value.
fn default_quality_live_action() -> u32 {
    26
}

// SVT-AV1 CRF starting point for the anime pipeline — slightly lower
// (higher quality) than the live-action guess above since flat-color/
// gradient-heavy anime content shows banding more readily than live-action
// grain/texture does at the same nominal quality level. Also unvalidated
// against real hardware/content yet.
fn default_quality_anime() -> u32 {
    24
}

fn default_anime_svtav1_preset() -> u32 {
    10
}

fn default_anime_svtav1_max_threads() -> u32 {
    4
}

fn default_min_size_reduction_pct() -> f64 {
    0.10
}

fn default_skip_below_ceiling_ratio() -> f64 {
    0.5
}

fn default_verify_sample_secs() -> f64 {
    20.0
}

/// TVDB v4 API key, exchanged for a short-lived JWT at request time.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TvdbConfig {
    #[serde(default)]
    pub api_key: String,
}

/// TMDB API Read Access Token (v4 auth), used directly as a bearer token.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TmdbConfig {
    #[serde(default)]
    pub bearer_token: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path)?;
        let cfg: Config = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Rejects values that are individually syntactically valid TOML but
    /// make the daemon unsafe or the transcode pipeline's math nonsensical
    /// — a typo here would otherwise only surface much later, or bind an
    /// unauthenticated API on a reachable address.
    fn validate(&self) -> Result<()> {
        // `target_bitrate_kbps` divides by `reference_height` (via
        // `reference_pixels`); zero makes that ratio `f64::INFINITY`, which
        // saturates to `u32::MAX` on the cast back to `u32` — then
        // `run_ffmpeg_encode_live_action`'s `bitrate_ceiling_kbps * 3`
        // overflows that `u32::MAX` (panics in a debug build, silently
        // wraps to a nonsense small value in release).
        anyhow::ensure!(
            self.transcode.reference_height > 0,
            "transcode.reference_height must be greater than 0"
        );
        // `is_beneficial` computes `original_bytes * (1.0 -
        // min_size_reduction_pct)` as the max allowed output size — a
        // negative value here would raise that ceiling *above* the
        // original, letting a transcode that actually grew the file still
        // count as "beneficial." That's the exact failure mode
        // `min_size_reduction_pct` exists to prevent (see its own doc
        // comment: a real incident where flat-bitrate VBR silently produced
        // files larger than the original).
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.transcode.min_size_reduction_pct),
            "transcode.min_size_reduction_pct must be between 0.0 and 1.0"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.transcode.skip_below_ceiling_ratio),
            "transcode.skip_below_ceiling_ratio must be between 0.0 and 1.0"
        );
        anyhow::ensure!(
            self.transcode.parallelism_min >= 1,
            "transcode.parallelism_min must be at least 1"
        );
        anyhow::ensure!(
            self.transcode.parallelism_max >= self.transcode.parallelism_min,
            "transcode.parallelism_max must be >= parallelism_min"
        );
        anyhow::ensure!(
            self.transcode.parallelism_max_anime >= 1,
            "transcode.parallelism_max_anime must be at least 1"
        );
        anyhow::ensure!(
            (0..=63).contains(&self.transcode.quality_anime),
            "transcode.quality_anime must be between 0 and 63"
        );
        anyhow::ensure!(
            (0..=13).contains(&self.transcode.anime_svtav1_preset),
            "transcode.anime_svtav1_preset must be between 0 and 13"
        );
        anyhow::ensure!(
            self.transcode.verify_sample_secs.is_finite()
                && self.transcode.verify_sample_secs > 0.0,
            "transcode.verify_sample_secs must be greater than 0"
        );
        const LOG_LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace", "off"];
        anyhow::ensure!(
            LOG_LEVELS
                .iter()
                .any(|l| self.daemon.log_level.eq_ignore_ascii_case(l)),
            "daemon.log_level must be one of error, warn, info, debug, trace, off"
        );
        if !self.daemon.api_token.is_empty() {
            anyhow::ensure!(
                self.daemon.api_token.len() >= 16,
                "daemon.api_token must be at least 16 characters when set"
            );
        }
        if self.daemon.api_token.is_empty() && !is_loopback_listen_addr(&self.daemon.listen_addr) {
            anyhow::bail!(
                "daemon.api_token is required when listen_addr ({}) is not loopback",
                self.daemon.listen_addr
            );
        }
        Ok(())
    }

    /// `true` when `daemon.listen_addr` is loopback (`127.0.0.1`, `::1`,
    /// `localhost`). Used at startup to decide whether an empty token is
    /// merely a local-process warning or already refused by `validate`.
    pub fn listen_is_loopback(&self) -> bool {
        is_loopback_listen_addr(&self.daemon.listen_addr)
    }

    /// Expand a `~/...` path the same way configured library roots are.
    pub fn expand_path(input: &str) -> PathBuf {
        expand_home(input)
    }

    pub fn db_path(&self) -> PathBuf {
        expand_home(&self.daemon.db_path)
    }

    pub fn model_dir(&self) -> PathBuf {
        expand_home(&self.daemon.model_dir)
    }

    pub fn default_root_folder(&self) -> PathBuf {
        expand_home(&self.library.default_root_folder)
    }

    pub fn movies_root_folder(&self) -> PathBuf {
        expand_home(&self.library.movies_root_folder)
    }
}

fn config_path() -> PathBuf {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        return Path::new(&xdg).join("breadarr").join("breadarrd.toml");
    }

    expand_home("~/.config/breadarr/breadarrd.toml")
}

fn expand_home(input: &str) -> PathBuf {
    // Was: falls through to `PathBuf::from(input)` — a literal, unexpanded
    // "~/..." string — whenever the `HOME` env var itself isn't set.
    // PathBuf/std::fs never expand `~`, so that fallback silently produced
    // a path relative to the current working directory instead of the
    // user's actual home. Same bug class as breadclip-core/breadpad-shared/
    // breadmon (see bread_utils::xdg's doc comment); bread_utils::xdg::home_dir
    // resolves a real home directory (falling back to `/root`, never a
    // literal tilde) before this ever needs to fall back at all.
    if let Some(stripped) = input.strip_prefix("~/") {
        return bread_utils::xdg::home_dir().join(stripped);
    }
    PathBuf::from(input)
}

/// Loopback hosts we allow to bind without an API token: IPv4/IPv6
/// loopback socket addresses, plus the `localhost` hostname form.
fn is_loopback_listen_addr(listen_addr: &str) -> bool {
    if let Ok(addr) = listen_addr.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    let host = if let Some(rest) = listen_addr.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if let Some((h, port)) = listen_addr.rsplit_once(':') {
        if port.parse::<u16>().is_ok() && !h.contains(':') {
            h
        } else {
            listen_addr
        }
    } else {
        listen_addr
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_listen_addr() -> String {
    "127.0.0.1:7879".to_string()
}

fn default_db_path() -> String {
    "~/.local/share/breadarr/breadarr.db".to_string()
}

fn default_model_dir() -> String {
    "~/.cache/breadarr/models/all-MiniLM-L6-v2".to_string()
}

fn default_qbit_category() -> String {
    "breadarr".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = Config::default();
        assert_eq!(cfg.daemon.log_level, "info");
        assert_eq!(cfg.daemon.listen_addr, "127.0.0.1:7879");
    }

    #[test]
    fn load_falls_back_to_default_when_file_missing() {
        // SAFETY: single-threaded test setting an isolated var it also restores below.
        unsafe {
            env::set_var("XDG_CONFIG_HOME", "/tmp/breadarr-test-nonexistent-dir");
        }
        let cfg = Config::load().unwrap();
        assert_eq!(cfg.daemon.log_level, "info");
        unsafe {
            env::remove_var("XDG_CONFIG_HOME");
        }
    }

    #[test]
    fn parses_partial_toml_with_defaults() {
        let cfg: Config = toml::from_str("[daemon]\nlog_level = \"debug\"\n").unwrap();
        assert_eq!(cfg.daemon.log_level, "debug");
        assert_eq!(cfg.daemon.listen_addr, "127.0.0.1:7879");
    }

    #[test]
    fn default_config_passes_validation() {
        Config::default().validate().unwrap();
    }

    // Regression test for a real gap found in review: `reference_height =
    // 0` makes `target_bitrate_kbps`'s resolution-scaling ratio divide by
    // zero, which eventually overflows a `u32` multiplication deep inside
    // the live-action encoder's maxrate calculation — a config typo that
    // used to only surface as a panic/garbage value in the middle of an
    // encode, not at startup.
    #[test]
    fn rejects_a_zero_reference_height() {
        let cfg: Config = toml::from_str("[transcode]\nreference_height = 0\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    // Regression test for a real gap found in review: a negative
    // `min_size_reduction_pct` would let `is_beneficial` accept an encode
    // that actually *grew* the file — the exact invariant this field exists
    // to guarantee against.
    #[test]
    fn rejects_a_negative_min_size_reduction_pct() {
        let cfg: Config = toml::from_str("[transcode]\nmin_size_reduction_pct = -0.1\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_a_min_size_reduction_pct_above_one() {
        let cfg: Config = toml::from_str("[transcode]\nmin_size_reduction_pct = 1.5\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn default_loopback_with_empty_token_is_ok() {
        Config::default().validate().unwrap();
        assert!(is_loopback_listen_addr("127.0.0.1:7879"));
        assert!(is_loopback_listen_addr("localhost:7879"));
        assert!(is_loopback_listen_addr("[::1]:7879"));
        assert!(is_loopback_listen_addr("::1"));
    }

    #[test]
    fn non_loopback_without_token_is_rejected() {
        let mut cfg = Config::default();
        cfg.daemon.listen_addr = "0.0.0.0:7879".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_loopback_with_token_is_ok() {
        let mut cfg = Config::default();
        cfg.daemon.listen_addr = "0.0.0.0:7879".into();
        cfg.daemon.api_token = "a-token-16-chars+".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn short_api_token_is_rejected() {
        let mut cfg = Config::default();
        cfg.daemon.api_token = "tooshort".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_invalid_log_level() {
        let mut cfg = Config::default();
        cfg.daemon.log_level = "loud".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_off_log_level() {
        let mut cfg = Config::default();
        cfg.daemon.log_level = "off".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_skip_below_ceiling_ratio_out_of_range() {
        let mut cfg = Config::default();
        cfg.transcode.skip_below_ceiling_ratio = 1.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_zero_parallelism_min() {
        let mut cfg = Config::default();
        cfg.transcode.parallelism_min = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_parallelism_max_below_min() {
        let mut cfg = Config::default();
        cfg.transcode.parallelism_min = 3;
        cfg.transcode.parallelism_max = 2;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_zero_parallelism_max_anime() {
        let mut cfg = Config::default();
        cfg.transcode.parallelism_max_anime = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_quality_anime_out_of_range() {
        let mut cfg = Config::default();
        cfg.transcode.quality_anime = 64;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_anime_svtav1_preset_out_of_range() {
        let mut cfg = Config::default();
        cfg.transcode.anime_svtav1_preset = 14;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_zero_verify_sample_secs() {
        let mut cfg = Config::default();
        cfg.transcode.verify_sample_secs = 0.0;
        assert!(cfg.validate().is_err());
    }
}
