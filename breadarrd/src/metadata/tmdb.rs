use anyhow::{Context, Result};
use serde::Deserialize;

use super::{EpisodeInfo, MovieSearchResult, SeriesSearchResult};

pub struct TmdbClient {
    bearer_token: String,
    client: reqwest::Client,
}

impl TmdbClient {
    pub fn new(bearer_token: impl Into<String>) -> Self {
        Self {
            bearer_token: bearer_token.into(),
            // No total timeout is reqwest's default — the background loop
            // holds the DB mutex across calls into this client, so a
            // stalled connection would hang the whole daemon.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
        }
    }

    pub async fn search_tv(&self, query: &str) -> Result<Vec<SeriesSearchResult>> {
        #[derive(Deserialize)]
        struct SearchResponse {
            results: Vec<TvItem>,
        }
        #[derive(Deserialize)]
        struct TvItem {
            id: u64,
            name: String,
            first_air_date: Option<String>,
        }

        let resp: SearchResponse = self
            .client
            .get("https://api.themoviedb.org/3/search/tv")
            .bearer_auth(&self.bearer_token)
            .query(&[("query", query)])
            .send()
            .await
            .context("tmdb tv search request failed")?
            .error_for_status()
            .context("tmdb tv search returned an error status")?
            .json()
            .await
            .context("tmdb tv search response was not valid JSON")?;

        Ok(resp
            .results
            .into_iter()
            .map(|item| SeriesSearchResult {
                external_id: item.id.to_string(),
                name: item.name,
                year: year_from_date(item.first_air_date.as_deref()),
                aliases: Vec::new(),
            })
            .collect())
    }

    pub async fn search_movie(&self, query: &str) -> Result<Vec<MovieSearchResult>> {
        #[derive(Deserialize)]
        struct SearchResponse {
            results: Vec<MovieItem>,
        }
        #[derive(Deserialize)]
        struct MovieItem {
            id: u64,
            title: String,
            release_date: Option<String>,
        }

        let resp: SearchResponse = self
            .client
            .get("https://api.themoviedb.org/3/search/movie")
            .bearer_auth(&self.bearer_token)
            .query(&[("query", query)])
            .send()
            .await
            .context("tmdb movie search request failed")?
            .error_for_status()
            .context("tmdb movie search returned an error status")?
            .json()
            .await
            .context("tmdb movie search response was not valid JSON")?;

        Ok(resp
            .results
            .into_iter()
            .map(|item| MovieSearchResult {
                external_id: item.id.to_string(),
                title: item.title,
                year: year_from_date(item.release_date.as_deref()),
            })
            .collect())
    }

    pub async fn tv_season_episodes(&self, tv_id: u64, season: u32) -> Result<Vec<EpisodeInfo>> {
        #[derive(Deserialize)]
        struct SeasonResponse {
            #[serde(default)]
            episodes: Vec<EpisodeItem>,
        }
        #[derive(Deserialize)]
        struct EpisodeItem {
            season_number: u32,
            episode_number: u32,
            name: Option<String>,
            air_date: Option<String>,
        }

        let resp: SeasonResponse = self
            .client
            .get(format!(
                "https://api.themoviedb.org/3/tv/{tv_id}/season/{season}"
            ))
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .context("tmdb season request failed")?
            .error_for_status()
            .context("tmdb season returned an error status")?
            .json()
            .await
            .context("tmdb season response was not valid JSON")?;

        Ok(resp
            .episodes
            .into_iter()
            .map(|e| EpisodeInfo {
                season_number: e.season_number,
                episode_number: e.episode_number,
                absolute_number: None,
                title: e.name,
                air_date: e.air_date,
            })
            .collect())
    }
}

fn year_from_date(date: Option<&str>) -> Option<u32> {
    date.and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())
}
