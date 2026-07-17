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
}

/// Where the TUI's "add show" flow places new series by default. Sonarr/
/// Radarr let you pick a root folder per add; a single configured default
/// is a reasonable v1 simplification — per-add picking can follow later.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LibraryConfig {
    #[serde(default = "default_root_folder")]
    pub default_root_folder: String,
}

fn default_root_folder() -> String {
    "~/breadarr-library".to_string()
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
    /// (the default) means auth is off entirely — `listen_addr` defaults to
    /// loopback-only, so a fresh install isn't suddenly locked out of its
    /// own unconfigured daemon. This matters once `listen_addr` is changed
    /// to bind non-loopback (e.g. so a TUI on a different host on the same
    /// tailnet can reach it) — without a token, that's unauthenticated
    /// add/delete/search access to anyone who can reach the port.
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
        Ok(cfg)
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
}
