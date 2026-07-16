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
        if !status.is_success() || body.trim() != "Ok." {
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
            // qBittorrent's add-torrent endpoint returns HTTP 200 even when
            // it rejects the magnet outright (a dead/malformed hash, one it
            // already knows is unreachable) — the *only* signal is the
            // response body text ("Ok." vs "Fails."). Without this check a
            // rejected magnet looks identical to a real success: the caller
            // records a `release` row as grabbed and nothing ever
            // downloads, silently and permanently (verified live — this
            // happened for a real release).
            if body.trim() != "Ok." {
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

    /// Moves a torrent's save location — qBittorrent physically relocates
    /// the underlying file(s) itself and continues seeding from the new
    /// path, rather than breadarr keeping a second permanent copy purely to
    /// satisfy its own import step.
    /// Held for the duration of an add-then-correlate-hash sequence — see
    /// `grab_lock`'s doc comment on why this needs to be process-wide, not
    /// just per-call.
    pub(crate) async fn lock_for_grab(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.grab_lock.lock().await
    }

    pub async fn set_location(&self, hash: &str, location: &str) -> Result<()> {
        self.post_form(
            "/api/v2/torrents/setLocation",
            &[("hashes", hash), ("location", location)],
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
}
