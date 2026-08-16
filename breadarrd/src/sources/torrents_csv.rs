use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{build_magnet, is_valid_info_hash, urlencode, RawReleaseItem, ReleaseSource};

/// Public JSON search over the torrents.csv DHT dump. Same grab shape as
/// TPB (info-hash → magnet, no HTML): used as the first general-content
/// fallback when apibay is down or returns nothing. Seeders are scrape
/// snapshots, not live tracker data, so a high number can still stall —
/// the existing seeder gate still applies.
pub struct TorrentsCsvSource {
    api_url: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct CsvResponse {
    #[serde(default)]
    torrents: Vec<CsvTorrent>,
}

#[derive(Deserialize)]
struct CsvTorrent {
    infohash: String,
    name: String,
    size_bytes: Option<u64>,
    seeders: Option<i64>,
    leechers: Option<i64>,
}

impl TorrentsCsvSource {
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

#[async_trait]
impl ReleaseSource for TorrentsCsvSource {
    async fn fetch(&self, query: Option<&str>) -> Result<Vec<RawReleaseItem>> {
        let Some(query) = query else {
            anyhow::bail!(
                "TorrentsCsvSource requires a search query (this is a search-driven source, not a feed)"
            );
        };
        let sep = if self.api_url.contains('?') { '&' } else { '?' };
        let url = format!(
            "{}{sep}q={}&size=25",
            self.api_url.trim_end_matches('/'),
            urlencode(query)
        );
        let parsed: CsvResponse = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?
            .error_for_status()
            .with_context(|| format!("{url} returned an error status"))?
            .json()
            .await
            .context("failed to parse torrents-csv response as JSON")?;

        Ok(parsed
            .torrents
            .into_iter()
            .filter(|t| is_valid_info_hash(&t.infohash))
            .map(|t| RawReleaseItem {
                title: t.name.clone(),
                link: build_magnet(&t.infohash, &t.name),
                guid: t.infohash,
                size_bytes: t.size_bytes,
                seeders: as_u32(t.seeders),
                leechers: as_u32(t.leechers),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_captured_response() {
        let body = r#"{
            "torrents": [
                {
                    "infohash": "ed0da850c273e3e15a819bdcbbf418bc85107ec8",
                    "name": "Dune (2021) [1080p] [WEBRip]",
                    "size_bytes": 2947989023,
                    "seeders": 746,
                    "leechers": 38
                },
                {
                    "infohash": "not-a-hash",
                    "name": "garbage",
                    "size_bytes": 1,
                    "seeders": 0,
                    "leechers": 0
                }
            ]
        }"#;
        let parsed: CsvResponse = serde_json::from_str(body).unwrap();
        let items: Vec<_> = parsed
            .torrents
            .into_iter()
            .filter(|t| is_valid_info_hash(&t.infohash))
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Dune (2021) [1080p] [WEBRip]");
        assert_eq!(items[0].seeders, Some(746));
    }

    #[test]
    fn empty_payload_is_no_results_not_an_error() {
        let parsed: CsvResponse = serde_json::from_str(r#"{"torrents":[]}"#).unwrap();
        assert!(parsed.torrents.is_empty());
    }
}
