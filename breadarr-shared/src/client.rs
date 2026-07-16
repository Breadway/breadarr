use anyhow::{Context, Result};

use crate::dto::{
    AddMovieRequest, AddMovieResponse, AddSeriesRequest, AddSeriesResponse, CalendarEntry,
    GrabCandidateRequest, HealthDetail, LibraryHealthReport, MediaItemDetail, MediaItemSummary,
    QualityProfileSummary, ReleaseCandidate, ReleaseSummary, ReviewQueueEntry, SearchNowResult,
    SearchResult, StuckReport, UpdateQualityProfileWeightsRequest, WeightsDto,
};

pub struct DaemonClient {
    base_url: String,
    client: reqwest::Client,
}

impl DaemonClient {
    /// `api_token` mirrors `config.daemon.api_token` server-side — empty
    /// means "no auth configured," so this stays a no-op default header
    /// rather than sending a meaningless empty bearer token on every
    /// request.
    pub fn new(base_url: impl Into<String>, api_token: &str) -> Self {
        let mut builder = reqwest::Client::builder()
            // A default so no request can hang the TUI forever with zero
            // feedback if the daemon is unreachable or a connection stalls.
            // Routes that legitimately need longer (search_now) or shorter
            // (health) windows set their own per-request `.timeout(...)`,
            // which overrides this.
            .timeout(std::time::Duration::from_secs(30));
        if !api_token.is_empty() {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Ok(value) =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {api_token}"))
            {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
            builder = builder.default_headers(headers);
        }
        Self {
            base_url: base_url.into(),
            client: builder
                .build()
                .expect("reqwest client builder should not fail with only a timeout/headers set"),
        }
    }

    pub async fn health(&self) -> Result<bool> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await;
        Ok(matches!(resp, Ok(r) if r.status().is_success()))
    }

    /// The richer health payload (per-cycle status, search_halted) — a
    /// separate call from `health()` rather than folding this into it, so
    /// a caller that only needs the fast up/down check (a tight poll loop)
    /// isn't forced to also pay for parsing/holding the full detail.
    pub async fn health_detail(&self) -> Result<HealthDetail> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .context("health_detail request failed")?
            .error_for_status()
            .context("health_detail returned an error status")?;
        resp.json()
            .await
            .context("health_detail response was not valid JSON")
    }

    pub async fn list_media(&self) -> Result<Vec<MediaItemSummary>> {
        self.get("/media").await
    }

    pub async fn media_detail(&self, id: i64) -> Result<MediaItemDetail> {
        self.get(&format!("/media/{id}")).await
    }

    pub async fn releases(&self) -> Result<Vec<ReleaseSummary>> {
        self.get("/releases").await
    }

    pub async fn review_queue(&self) -> Result<Vec<ReviewQueueEntry>> {
        self.get("/review").await
    }

    pub async fn stuck(&self) -> Result<StuckReport> {
        self.get("/stuck").await
    }

    pub async fn calendar(&self) -> Result<Vec<CalendarEntry>> {
        self.get("/calendar").await
    }

    pub async fn library_health(&self) -> Result<LibraryHealthReport> {
        self.get("/library/health").await
    }

    pub async fn quality_profiles(&self) -> Result<Vec<QualityProfileSummary>> {
        self.get("/quality-profiles").await
    }

    pub async fn update_quality_profile_weights(&self, id: i64, weights: WeightsDto) -> Result<()> {
        let req = UpdateQualityProfileWeightsRequest { weights };
        self.client
            .put(format!("{}/quality-profiles/{id}/weights", self.base_url))
            .json(&req)
            .send()
            .await
            .context("update_quality_profile_weights request failed")?
            .error_for_status()
            .context("update_quality_profile_weights returned an error status")?;
        Ok(())
    }

    pub async fn approve_review(&self, id: i64) -> Result<()> {
        self.post_empty(&format!("/review/{id}/approve")).await
    }

    pub async fn reject_review(&self, id: i64) -> Result<()> {
        self.post_empty(&format!("/review/{id}/reject")).await
    }

    pub async fn search_series(&self, query: &str) -> Result<Vec<SearchResult>> {
        let resp = self
            .client
            .get(format!("{}/search", self.base_url))
            .query(&[("q", query)])
            .send()
            .await
            .context("search request failed")?
            .error_for_status()
            .context("search returned an error status")?;
        resp.json()
            .await
            .context("search response was not valid JSON")
    }

    pub async fn add_series(&self, req: &AddSeriesRequest) -> Result<AddSeriesResponse> {
        let resp = self
            .client
            .post(format!("{}/media", self.base_url))
            .json(req)
            .send()
            .await
            .context("add_series request failed")?
            .error_for_status()
            .context("add_series returned an error status")?;
        resp.json()
            .await
            .context("add_series response was not valid JSON")
    }

    pub async fn search_movies(&self, query: &str) -> Result<Vec<SearchResult>> {
        let resp = self
            .client
            .get(format!("{}/search", self.base_url))
            .query(&[("q", query), ("kind", "movie")])
            .send()
            .await
            .context("search request failed")?
            .error_for_status()
            .context("search returned an error status")?;
        resp.json()
            .await
            .context("search response was not valid JSON")
    }

    pub async fn add_movie(&self, req: &AddMovieRequest) -> Result<AddMovieResponse> {
        let resp = self
            .client
            .post(format!("{}/media/movie", self.base_url))
            .json(req)
            .send()
            .await
            .context("add_movie request failed")?
            .error_for_status()
            .context("add_movie returned an error status")?;
        resp.json()
            .await
            .context("add_movie response was not valid JSON")
    }

    /// Manual "search now" for one show/movie's whole current backlog — can
    /// take a while (jittered, one request per missing item), so callers
    /// should expect this to be slow, not instant.
    pub async fn search_now(&self, id: i64) -> Result<SearchNowResult> {
        let resp = self
            .client
            .post(format!("{}/media/{id}/search", self.base_url))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .context("search_now request failed")?
            .error_for_status()
            .context("search_now returned an error status")?;
        resp.json()
            .await
            .context("search_now response was not valid JSON")
    }

    /// Scored (or gate-rejected) candidates for a movie, without grabbing —
    /// the manual release picker. Same 600s allowance as `search_now` since
    /// it runs a real search under the hood.
    pub async fn movie_candidates(&self, media_item_id: i64) -> Result<Vec<ReleaseCandidate>> {
        let resp = self
            .client
            .get(format!(
                "{}/media/{media_item_id}/candidates",
                self.base_url
            ))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .context("movie_candidates request failed")?
            .error_for_status()
            .context("movie_candidates returned an error status")?;
        resp.json()
            .await
            .context("movie_candidates response was not valid JSON")
    }

    pub async fn episode_candidates(&self, episode_id: i64) -> Result<Vec<ReleaseCandidate>> {
        let resp = self
            .client
            .get(format!("{}/episode/{episode_id}/candidates", self.base_url))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .context("episode_candidates request failed")?
            .error_for_status()
            .context("episode_candidates returned an error status")?;
        resp.json()
            .await
            .context("episode_candidates response was not valid JSON")
    }

    pub async fn grab_movie_candidate(
        &self,
        media_item_id: i64,
        candidate: &ReleaseCandidate,
    ) -> Result<()> {
        self.grab_candidate(&format!("/media/{media_item_id}/candidates"), candidate)
            .await
    }

    pub async fn grab_episode_candidate(
        &self,
        episode_id: i64,
        candidate: &ReleaseCandidate,
    ) -> Result<()> {
        self.grab_candidate(&format!("/episode/{episode_id}/candidates"), candidate)
            .await
    }

    async fn grab_candidate(&self, path: &str, candidate: &ReleaseCandidate) -> Result<()> {
        let req = GrabCandidateRequest {
            raw_title: candidate.raw_title.clone(),
            link: candidate.link.clone(),
            guid: candidate.guid.clone(),
            source_id: candidate.source_id,
        };
        self.client
            .post(format!("{}{path}", self.base_url))
            .timeout(std::time::Duration::from_secs(120))
            .json(&req)
            .send()
            .await
            .with_context(|| format!("grab_candidate POST {path} failed"))?
            .error_for_status()
            .with_context(|| format!("grab_candidate POST {path} returned an error status"))?;
        Ok(())
    }

    pub async fn monitor(&self, id: i64) -> Result<()> {
        self.post_empty(&format!("/media/{id}/monitor")).await
    }

    pub async fn unmonitor(&self, id: i64) -> Result<()> {
        self.post_empty(&format!("/media/{id}/unmonitor")).await
    }

    pub async fn monitor_episode(&self, episode_id: i64) -> Result<()> {
        self.post_empty(&format!("/episode/{episode_id}/monitor"))
            .await
    }

    pub async fn unmonitor_episode(&self, episode_id: i64) -> Result<()> {
        self.post_empty(&format!("/episode/{episode_id}/unmonitor"))
            .await
    }

    pub async fn monitor_season(&self, media_item_id: i64, season_number: i64) -> Result<()> {
        self.post_empty(&format!(
            "/media/{media_item_id}/season/{season_number}/monitor"
        ))
        .await
    }

    pub async fn unmonitor_season(&self, media_item_id: i64, season_number: i64) -> Result<()> {
        self.post_empty(&format!(
            "/media/{media_item_id}/season/{season_number}/unmonitor"
        ))
        .await
    }

    pub async fn delete_media(&self, id: i64) -> Result<()> {
        self.client
            .delete(format!("{}/media/{id}", self.base_url))
            .send()
            .await
            .context("delete_media request failed")?
            .error_for_status()
            .context("delete_media returned an error status")?;
        Ok(())
    }

    /// Deletes an episode's imported file (from disk, not just tracking) so
    /// it can be re-grabbed — for a confirmed-wrong or broken file, not a
    /// routine action.
    pub async fn delete_episode_file(&self, episode_id: i64) -> Result<()> {
        self.client
            .delete(format!("{}/episode/{episode_id}/file", self.base_url))
            .send()
            .await
            .context("delete_episode_file request failed")?
            .error_for_status()
            .context("delete_episode_file returned an error status")?;
        Ok(())
    }

    /// Movie counterpart to `delete_episode_file`.
    pub async fn delete_movie_file(&self, media_item_id: i64) -> Result<()> {
        self.client
            .delete(format!("{}/media/{media_item_id}/file", self.base_url))
            .send()
            .await
            .context("delete_movie_file request failed")?
            .error_for_status()
            .context("delete_movie_file returned an error status")?;
        Ok(())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .with_context(|| format!("GET {path} failed"))?
            .error_for_status()
            .with_context(|| format!("GET {path} returned an error status"))?;
        resp.json()
            .await
            .with_context(|| format!("GET {path} response was not valid JSON"))
    }

    async fn post_empty(&self, path: &str) -> Result<()> {
        self.client
            .post(format!("{}{path}", self.base_url))
            .send()
            .await
            .with_context(|| format!("POST {path} failed"))?
            .error_for_status()
            .with_context(|| format!("POST {path} returned an error status"))?;
        Ok(())
    }
}
