use anyhow::{Context, Result};
use serde::Deserialize;

use super::MovieSearchResult;

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
}

fn year_from_date(date: Option<&str>) -> Option<u32> {
    date.and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())
}
