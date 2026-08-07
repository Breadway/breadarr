use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::sync::LazyLock;

static BTIH_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)urn:btih:([0-9a-f]{40}|[2-7a-z]{32})").unwrap());

/// Pulls a magnet link's own infohash out of it directly — no need to ask
/// qBittorrent to correlate anything when the caller already handed us the
/// hash. Returns `None` for anything that isn't a magnet URI (e.g. nyaa's
/// `.torrent`-file download URLs), which callers fall back to polling for.
/// Lowercased to match the casing qBittorrent's own API always returns.
pub(crate) fn extract_btih(link: &str) -> Option<String> {
    BTIH_RE.captures(link).map(|c| c[1].to_lowercase())
}

/// qBittorrent rejected a magnet outright (HTTP 200 with body "Fails.") —
/// deterministic for a given hash (a dead/unreachable torrent, a malformed
/// magnet), unlike a network-level failure. A distinct type so callers can
/// `downcast_ref` and treat it differently from a transient error: retrying
/// the exact same hash next cycle would just fail identically forever,
/// where a real network blip is worth retrying.
#[derive(Debug)]
pub struct MagnetRejected {
    pub body: String,
}

impl std::fmt::Display for MagnetRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "qbit rejected the magnet: body={:?}", self.body)
    }
}

impl std::error::Error for MagnetRejected {}

/// Newer qBittorrent WebUI API versions' `torrents/add` JSON response shape.
#[derive(Debug, Deserialize, Default)]
struct AddTorrentResponse {
    #[serde(default)]
    success_count: u32,
    #[serde(default)]
    failure_count: u32,
    /// Present when adding by URL rather than by a magnet/`.torrent` blob
    /// qBittorrent already has in hand — the torrent itself is fetched
    /// asynchronously, so the immediate response has neither succeeded nor
    /// failed yet, just queued. nyaa's RSS feed always hands over a
    /// `.torrent` download URL (never a magnet), so this is the normal
    /// shape for every anime grab, not an edge case. Verified live against
    /// the deployed qBittorrent: a URL add returns HTTP 202 with
    /// `{"pending_count":1,"success_count":0,"failure_count":0}` — treating
    /// `success_count == 0` alone as rejection (the previous check) silently
    /// dropped every one of these while the torrent downloaded successfully
    /// in the background, with no DB record and no log line.
    #[serde(default)]
    pending_count: u32,
}

/// Interprets a `torrents/add` response body — split out from `add_magnet`
/// so it's directly testable without a live qBittorrent server.
/// qBittorrent's add-torrent endpoint returns HTTP 200/202 even when it
/// rejects the magnet outright (a dead/malformed hash, one it already knows
/// is unreachable) — the response body is the only signal. Older
/// qBittorrent versions returned plain text ("Ok." vs "Fails."); newer ones
/// return a JSON summary with success_count/failure_count/pending_count
/// instead (verified live against the currently deployed version). Without
/// checking whichever shape is actually in play, a rejected magnet looks
/// identical to a real success: the caller records a `release` row as
/// grabbed and nothing ever downloads, silently and permanently (verified
/// live — this happened for a real release under the old text-only check,
/// and separately for every nyaa URL-add under a since-fixed
/// `success_count == 0` check that didn't account for `pending_count`).
fn add_torrent_response_is_rejected(body: &str) -> bool {
    match serde_json::from_str::<AddTorrentResponse>(body) {
        Ok(r) => r.failure_count > 0 || (r.success_count == 0 && r.pending_count == 0),
        Err(_) => body.trim() != "Ok.",
    }
}

pub struct QbitClient {
    base_url: String,
    client: reqwest::Client,
    /// Credentials from the last successful `login()`, kept so a session
    /// that's expired mid-run (a qBittorrent container restarting under it
    /// is common in a Docker-based setup) can be silently re-established
    /// instead of every subsequent call failing until breadarrd itself is
    /// restarted.
    credentials: tokio::sync::RwLock<Option<(String, String)>>,
    /// Serializes the add-then-correlate-hash sequence (see
    /// `scheduler::grab_and_capture_hash`) across every caller — the
    /// background grab loop and the API's review-approve handler both add
    /// torrents against the same category and can run concurrently.
    /// Without this, two adds interleaved between one caller's before/after
    /// `torrents/info` snapshots let the *other* caller's torrent look like
    /// "the new one," recording the wrong hash against the wrong release
    /// (verified as a real risk, not just theoretical, during the Fable 5
    /// audit — see the caller for the fuller writeup).
    grab_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Deserialize)]
pub struct TorrentInfo {
    pub hash: String,
    pub name: String,
    pub state: String,
    pub progress: f64,
    // Not read internally today, but kept to mirror qBittorrent's actual API
    // shape 1:1 — useful in `{:?}` debug logging and cheap to keep in sync.
    #[allow(dead_code)]
    pub save_path: String,
    /// Full path to the torrent's content (file or directory root) —
    /// qBittorrent resolves this for us, so importers don't need to guess
    /// at how save_path and name combine for a given torrent.
    pub content_path: String,
}

impl QbitClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        // No total timeout is reqwest's default — the background loop holds
        // the DB mutex across calls into this client, so a stalled
        // connection would hang the whole daemon.
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base_url: base_url.into(),
            client,
            credentials: tokio::sync::RwLock::new(None),
            grab_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<()> {
        self.do_login(username, password).await?;
        *self.credentials.write().await = Some((username.to_string(), password.to_string()));
        Ok(())
    }

    async fn do_login(&self, username: &str, password: &str) -> Result<()> {
        let resp = self
            .client
            .post(format!("{}/api/v2/auth/login", self.base_url))
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .context("qbit login request failed")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Older qBittorrent WebUI API versions return 200 with body "Ok.";
        // newer ones return 204 No Content with an empty body instead. Bad
        // credentials return a real error status (401), which `is_success`
        // already catches — the response body's exact text isn't part of
        // the actual success contract, just an artifact of the old version.
        if !status.is_success() {
            bail!("qbit login failed: status={status} body={body:?}");
        }
        Ok(())
    }

    /// Re-authenticates using the credentials from the last successful
    /// `login()`, if any. Returns `true` on success so callers know it's
    /// worth retrying the request that got a 403 in the first place.
    async fn try_reauth(&self) -> bool {
        let creds = self.credentials.read().await.clone();
        let Some((username, password)) = creds else {
            return false;
        };
        self.do_login(&username, &password).await.is_ok()
    }

    pub async fn add_magnet(&self, magnet: &str, category: &str) -> Result<()> {
        for attempt in 0..2 {
            let form = reqwest::multipart::Form::new()
                .text("urls", magnet.to_string())
                .text("category", category.to_string());

            let resp = self
                .client
                .post(format!("{}/api/v2/torrents/add", self.base_url))
                .multipart(form)
                .send()
                .await
                .context("qbit add-torrent request failed")?;

            let status = resp.status();
            if status == reqwest::StatusCode::FORBIDDEN && attempt == 0 && self.try_reauth().await {
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("qbit add-torrent failed: status={status} body={body:?}");
            }
            if add_torrent_response_is_rejected(&body) {
                return Err(anyhow::Error::new(MagnetRejected { body }));
            }
            return Ok(());
        }
        unreachable!("loop always returns or bails on its second iteration")
    }

    pub async fn list_torrents(&self, category: Option<&str>) -> Result<Vec<TorrentInfo>> {
        for attempt in 0..2 {
            let mut req = self
                .client
                .get(format!("{}/api/v2/torrents/info", self.base_url));
            if let Some(category) = category {
                req = req.query(&[("category", category)]);
            }

            let resp = req.send().await.context("qbit list-torrents failed")?;
            let status = resp.status();
            if status == reqwest::StatusCode::FORBIDDEN && attempt == 0 && self.try_reauth().await {
                continue;
            }
            if !status.is_success() {
                bail!("qbit list-torrents failed: status={status}");
            }
            return Ok(resp.json().await?);
        }
        unreachable!("loop always returns or bails on its second iteration")
    }

    /// Sends a POST form request, retrying once after a silent re-login if
    /// the session had expired (403). Shared by the file-relocation methods
    /// below — `add_magnet`/`list_torrents` predate this and are left as
    /// they are rather than churned for the sake of it.
    async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<String> {
        for attempt in 0..2 {
            let resp = self
                .client
                .post(format!("{}{path}", self.base_url))
                .form(form)
                .send()
                .await
                .with_context(|| format!("qbit {path} request failed"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::FORBIDDEN && attempt == 0 && self.try_reauth().await {
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("qbit {path} failed: status={status} body={body:?}");
            }
            return Ok(body);
        }
        unreachable!("loop always returns or bails on its second iteration")
    }

    /// Held for the duration of an add-then-correlate-hash sequence — see
    /// `grab_lock`'s doc comment on why this needs to be process-wide, not
    /// just per-call.
    pub(crate) async fn lock_for_grab(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.grab_lock.lock().await
    }

    /// Removes a torrent from qBittorrent's own tracking after breadarr has
    /// already moved its data straight into the library — `delete_files:
    /// false` because by the time this is called there's nothing left at
    /// the torrent's original save path for qBittorrent to delete; leaving
    /// the torrent registered would just leave it sitting in an "files
    /// missing" error state indefinitely. Best-effort from the caller's
    /// side: a failure here never undoes or blocks the import that already
    /// succeeded, it just leaves one stale entry in the qBittorrent UI to
    /// clean up by hand.
    pub async fn delete_torrent(&self, hash: &str) -> Result<()> {
        self.post_form(
            "/api/v2/torrents/delete",
            &[("hashes", hash), ("deleteFiles", "false")],
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_hash_from_a_real_magnet() {
        let link = "magnet:?xt=urn:btih:3F493B821C8B13CA2EE6DA0183B4803B187F9363&dn=Some+Show&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce";
        assert_eq!(
            extract_btih(link).as_deref(),
            Some("3f493b821c8b13ca2ee6da0183b4803b187f9363")
        );
    }

    #[test]
    fn extracts_base32_hash_from_a_magnet() {
        let link = "magnet:?xt=urn:btih:jz2eqzsm3mmirrbcaacegz3czfzwxjbc&dn=Some+Show";
        assert_eq!(
            extract_btih(link).as_deref(),
            Some("jz2eqzsm3mmirrbcaacegz3czfzwxjbc")
        );
    }

    #[test]
    fn returns_none_for_a_torrent_download_url() {
        assert_eq!(
            extract_btih("https://nyaa.si/download/2131680.torrent"),
            None
        );
    }

    #[test]
    fn returns_none_for_a_1337x_page_url() {
        assert_eq!(
            extract_btih("https://1337x.to/torrent/3740704/Some-Show/"),
            None
        );
    }

    #[test]
    fn a_pending_url_add_is_not_treated_as_rejected() {
        // Exact shape qBittorrent 5.x returns for a URL add (every nyaa
        // grab) — the torrent is queued for an async fetch, not yet
        // succeeded or failed. This is the shape that used to be
        // misread as an outright rejection.
        let body = r#"{"added_torrent_ids":[],"failure_count":0,"pending_count":1,"success_count":0}"#;
        assert!(!add_torrent_response_is_rejected(body));
    }

    #[test]
    fn a_genuine_failure_is_still_rejected() {
        let body = r#"{"failure_count":1,"pending_count":0,"success_count":0}"#;
        assert!(add_torrent_response_is_rejected(body));
    }

    #[test]
    fn an_in_band_success_is_not_rejected() {
        // TPB/1337x magnets: qBittorrent already has the info hash, no
        // async fetch needed, so success_count is set immediately.
        let body = r#"{"failure_count":0,"pending_count":0,"success_count":1}"#;
        assert!(!add_torrent_response_is_rejected(body));
    }

    #[test]
    fn the_old_plain_text_ok_response_is_not_rejected() {
        assert!(!add_torrent_response_is_rejected("Ok."));
    }

    #[test]
    fn the_old_plain_text_fails_response_is_rejected() {
        assert!(add_torrent_response_is_rejected("Fails."));
    }
}
