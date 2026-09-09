use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{build_magnet, is_valid_info_hash, urlencode, RawReleaseItem, ReleaseSource};

/// YTS movie API. `yts.mx` itself no longer resolves (checked 2026-07-12
/// and again 2026-08-16); the `yts.lt` / `yts.am` hosts still serve the
/// v2 JSON API, which is why the default URL is a working mirror rather
/// than the brand domain. Movies only: each hit expands into one
/// `RawReleaseItem` per quality so the scorer sees 720p/1080p/2160p as
/// distinct candidates, same as if they were separate TPB rows.
pub struct YtsSource {
    api_url: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct YtsResponse {
    status: String,
    data: Option<YtsData>,
}

#[derive(Deserialize, Default)]
struct YtsData {
    #[serde(default)]
    movies: Vec<YtsMovie>,
}

#[derive(Deserialize)]
struct YtsMovie {
    title: String,
    year: Option<i64>,
    #[serde(default)]
    torrents: Vec<YtsTorrent>,
}

#[derive(Deserialize)]
struct YtsTorrent {
    hash: String,
    quality: Option<String>,
    #[serde(rename = "type")]
    source_type: Option<String>,
    video_codec: Option<String>,
    seeds: Option<i64>,
    peers: Option<i64>,
    size_bytes: Option<u64>,
}

impl YtsSource {
    pub fn new(api_url: impl Into<String>) -> Self {
        Self {
            api_url: api_url.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
        }
    }
}

fn as_u32(n: Option<i64>) -> Option<u32> {
    n.and_then(|v| u32::try_from(v.max(0)).ok())
}

/// Builds a release title the existing parser can read quality/source/codec
/// out of; YTS stores those as structured fields, not in `title`.
fn release_title(movie: &YtsMovie, torrent: &YtsTorrent) -> String {
    let mut title = movie.title.clone();
    if let Some(year) = movie.year {
        title.push_str(&format!(" ({year})"));
    }
    for part in [
        torrent.quality.as_deref(),
        torrent.source_type.as_deref(),
        torrent.video_codec.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !part.is_empty() {
            title.push_str(&format!(" [{part}]"));
        }
    }
    title
}

#[async_trait]
impl ReleaseSource for YtsSource {
    async fn fetch(&self, query: Option<&str>) -> Result<Vec<RawReleaseItem>> {
        let Some(query) = query else {
            anyhow::bail!(
                "YtsSource requires a search query (this is a search-driven source, not a feed)"
            );
        };
        let sep = if self.api_url.contains('?') { '&' } else { '?' };
        let url = format!(
            "{}{sep}query_term={}&limit=20&sort_by=seeds",
            self.api_url.trim_end_matches('/'),
            urlencode(query)
        );
        let parsed: YtsResponse = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?
            .error_for_status()
            .with_context(|| format!("{url} returned an error status"))?
            .json()
            .await
            .context("failed to parse YTS response as JSON")?;
        anyhow::ensure!(
            parsed.status == "ok",
            "YTS returned status {:?}",
            parsed.status
        );

        let movies = parsed.data.unwrap_or_default().movies;
        let mut items = Vec::new();
        for movie in movies {
            for torrent in &movie.torrents {
                if !is_valid_info_hash(&torrent.hash) {
                    continue;
                }
                let title = release_title(&movie, torrent);
                items.push(RawReleaseItem {
                    title: title.clone(),
                    link: build_magnet(&torrent.hash, &title),
                    guid: torrent.hash.clone(),
                    size_bytes: torrent.size_bytes,
                    seeders: as_u32(torrent.seeds),
                    leechers: as_u32(torrent.peers),
                });
            }
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "status": "ok",
        "data": {
            "movie_count": 1,
            "movies": [{
                "title": "Dune: Part One",
                "year": 2021,
                "torrents": [
                    {
                        "hash": "DEB6929BEEB09ADCBD14DC4D6081F7E6B297B88C",
                        "quality": "1080p",
                        "type": "web",
                        "video_codec": "x264",
                        "seeds": 12,
                        "peers": 3,
                        "size_bytes": 2147483648
                    },
                    {
                        "hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "quality": "720p",
                        "type": "bluray",
                        "video_codec": "x265",
                        "seeds": 4,
                        "peers": 1,
                        "size_bytes": 1073741824
                    }
                ]
            }]
        }
    }"#;

    #[test]
    fn expands_one_movie_into_per_quality_rows() {
        let parsed: YtsResponse = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(parsed.status, "ok");
        let movie = &parsed.data.unwrap().movies[0];
        assert_eq!(movie.torrents.len(), 2);
        let t0 = release_title(movie, &movie.torrents[0]);
        assert_eq!(t0, "Dune: Part One (2021) [1080p] [web] [x264]");
        let t1 = release_title(movie, &movie.torrents[1]);
        assert_eq!(t1, "Dune: Part One (2021) [720p] [bluray] [x265]");
    }

    #[test]
    fn missing_movies_array_is_empty_not_an_error() {
        let parsed: YtsResponse =
            serde_json::from_str(r#"{"status":"ok","data":{"movie_count":0}}"#).unwrap();
        assert!(parsed.data.unwrap_or_default().movies.is_empty());
    }
}
