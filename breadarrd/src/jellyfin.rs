use anyhow::{bail, Context, Result};

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
}
