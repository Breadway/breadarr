use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{EpisodeInfo, SeriesSearchResult};

/// TVDB v4 JWTs are valid for roughly a month; refresh well before that so
/// clock skew or early invalidation on their end doesn't strand us.
const TOKEN_TTL: Duration = Duration::from_secs(20 * 60 * 60);

pub struct TvdbClient {
    api_key: String,
    client: reqwest::Client,
    token: Mutex<Option<(String, Instant)>>,
}

impl TvdbClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            // No total timeout is reqwest's default — the background loop
            // holds the DB mutex across calls into this client, so a
            // stalled connection would hang the whole daemon.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
            token: Mutex::new(None),
        }
    }

    async fn token(&self) -> Result<String> {
        let cached = self.token.lock().unwrap().clone();
        if let Some((token, obtained_at)) = cached {
            if obtained_at.elapsed() < TOKEN_TTL {
                return Ok(token);
            }
        }

        #[derive(Deserialize)]
        struct LoginResponse {
            data: LoginData,
        }
        #[derive(Deserialize)]
        struct LoginData {
            token: String,
        }

        let resp: LoginResponse = self
            .client
            .post("https://api4.thetvdb.com/v4/login")
            .json(&serde_json::json!({ "apikey": self.api_key }))
            .send()
            .await
            .context("tvdb login request failed")?
            .error_for_status()
            .context("tvdb login returned an error status")?
            .json()
            .await
            .context("tvdb login response was not valid JSON")?;

        *self.token.lock().unwrap() = Some((resp.data.token.clone(), Instant::now()));
        Ok(resp.data.token)
    }

    pub async fn search_series(&self, query: &str) -> Result<Vec<SeriesSearchResult>> {
        let token = self.token().await?;

        #[derive(Deserialize)]
        struct SearchResponse {
            data: Vec<SearchItem>,
        }
        #[derive(Deserialize)]
        struct SearchItem {
            tvdb_id: Option<String>,
            name: Option<String>,
            year: Option<String>,
            #[serde(default)]
            aliases: Vec<String>,
        }

        let resp: SearchResponse = self
            .client
            .get("https://api4.thetvdb.com/v4/search")
            .bearer_auth(token)
            .query(&[("query", query), ("type", "series")])
            .send()
            .await
            .context("tvdb search request failed")?
            .error_for_status()
            .context("tvdb search returned an error status")?
            .json()
            .await
            .context("tvdb search response was not valid JSON")?;

        Ok(resp
            .data
            .into_iter()
            .filter_map(|item| {
                Some(SeriesSearchResult {
                    external_id: item.tvdb_id?,
                    name: item.name?,
                    year: item.year.and_then(|y| y.parse().ok()),
                    aliases: item.aliases,
                })
            })
            .collect())
    }

    pub async fn episodes(&self, series_id: &str) -> Result<Vec<EpisodeInfo>> {
        #[derive(Deserialize)]
        struct EpisodesResponse {
            data: EpisodesData,
        }
        #[derive(Deserialize)]
        struct EpisodesData {
            episodes: Vec<EpisodeItem>,
        }
        #[derive(Deserialize)]
        struct EpisodeItem {
            #[serde(rename = "seasonNumber")]
            season_number: u32,
            number: u32,
            #[serde(rename = "absoluteNumber")]
            absolute_number: Option<u32>,
            name: Option<String>,
            aired: Option<String>,
        }

        // TVDB's plain `/episodes/default` returns names in the show's
        // original airing language — for a lot of anime that's Japanese,
        // with no English name at all (verified live: entire shows came
        // back Japanese-only). `/episodes/default/eng` is the same episode
        // list (same numbering/air-date fields) but with TVDB's own
        // crowd-sourced English translation substituted in for `name`
        // wherever one exists — a real translation, not a guess, so it's
        // tried first and only falls back to the original-language
        // endpoint if TVDB has no English data for this show at all.
        let token = self.token().await?;
        let fetch = |url: String| {
            let client = self.client.clone();
            let token = token.clone();
            async move {
                client
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .context("tvdb episodes request failed")?
                    .error_for_status()
                    .context("tvdb episodes returned an error status")?
                    .json::<EpisodesResponse>()
                    .await
                    .context("tvdb episodes response was not valid JSON")
            }
        };

        let resp = match fetch(format!(
            "https://api4.thetvdb.com/v4/series/{series_id}/episodes/default/eng"
        ))
        .await
        {
            Ok(resp) => resp,
            Err(_) => {
                fetch(format!(
                    "https://api4.thetvdb.com/v4/series/{series_id}/episodes/default"
                ))
                .await?
            }
        };

        Ok(resp
            .data
            .episodes
            .into_iter()
            .map(|e| EpisodeInfo {
                season_number: e.season_number,
                episode_number: e.number,
                absolute_number: e.absolute_number.filter(|&n| n != 0),
                title: e.name,
                air_date: e.aired,
            })
            .collect())
    }
}
