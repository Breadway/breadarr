use anyhow::{bail, Context, Result};

#[derive(Clone)]
pub struct JellyfinClient {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl JellyfinClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            // No total timeout is reqwest's default — fine for a one-shot
            // debug command, but the daemon's background loop holds the DB
            // mutex across this call, so a stalled connection here would
            // hang the whole daemon indefinitely.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
        }
    }

    pub async fn refresh_library(&self) -> Result<()> {
        let resp = self
            .client
            .post(format!("{}/Library/Refresh", self.base_url))
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await
            .context("jellyfin library refresh request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("jellyfin library refresh failed: status={status} body={body:?}");
        }
        Ok(())
    }

    /// Counts sessions Jellyfin is actively transcoding for right now (as
    /// opposed to direct-play/direct-stream, which cost the GPU nothing) —
    /// used to throttle the AV1 batch-transcode worker back so it doesn't
    /// contend with a real viewer for the same encode/decode engines.
    /// `TranscodingInfo` is only present on a session object while that
    /// session is actually transcoding.
    pub async fn active_transcode_sessions(&self) -> Result<usize> {
        let resp = self
            .client
            .get(format!("{}/Sessions", self.base_url))
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await
            .context("jellyfin sessions request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("jellyfin sessions request failed: status={status} body={body:?}");
        }

        let sessions: Vec<serde_json::Value> =
            resp.json().await.context("failed to parse jellyfin sessions response")?;
        Ok(sessions
            .iter()
            .filter(|s| !s["TranscodingInfo"].is_null())
            .count())
    }
}
