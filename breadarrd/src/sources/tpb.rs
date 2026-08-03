use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{urlencode, RawReleaseItem, ReleaseSource};

/// A valid BitTorrent v1 info_hash: 40 hex chars or 32 base32 chars — same
/// shape `qbit::extract_btih` accepts out of a magnet URI. Checked before
/// building a magnet from `info_hash` at all: apibay is normally reliable,
/// but a malformed value would otherwise silently produce a magnet
/// `extract_btih` can't parse back out, downgrading that grab to the slow
/// ~30s `torrents/info` polling path with no visible error anywhere.
fn is_valid_info_hash(hash: &str) -> bool {
    (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        || (hash.len() == 32 && hash.bytes().all(|b| matches!(b, b'2'..=b'7' | b'a'..=b'z' | b'A'..=b'Z')))
}

/// A community-run JSON API mirror of The Pirate Bay's search — unlike
/// 1337x, this is a genuine machine-readable API (not HTML scraping), and
/// unlike 1337x's HTML results table, `info_hash` is enough to build a
/// magnet directly: no second detail-page fetch needed to resolve a link
/// before grabbing. Also proved dramatically more precise in practice —
/// its search actually ranks by relevance, where 1337x's is a pure
/// seeder-count sort that buries genuine matches for common-word titles
/// under unrelated, much-more-seeded content.
pub struct TpbSource {
    api_url: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct TpbResult {
    id: String,
    name: String,
    info_hash: String,
    seeders: String,
    leechers: String,
    size: String,
}

const TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://exodus.desync.com:6969/announce",
];

fn build_magnet(info_hash: &str, name: &str) -> String {
    let mut magnet = format!("magnet:?xt=urn:btih:{info_hash}&dn={}", urlencode(name));
    for t in TRACKERS {
        magnet.push_str("&tr=");
        magnet.push_str(&urlencode(t));
    }
    magnet
}

impl TpbSource {
    pub fn new(api_url: impl Into<String>) -> Self {
        Self {
            api_url: api_url.into(),
            // No total timeout is reqwest's default — the background loop
            // holds the DB mutex across this fetch, so a stalled connection
            // would hang the whole daemon.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
        }
    }
}

#[async_trait]
impl ReleaseSource for TpbSource {
    async fn fetch(&self, query: Option<&str>) -> Result<Vec<RawReleaseItem>> {
        let Some(query) = query else {
            anyhow::bail!(
                "TpbSource requires a search query (this is a search-driven source, not a feed)"
            );
        };
        let url = format!("{}?q={}", self.api_url, urlencode(query));
        let results: Vec<TpbResult> = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?
            .error_for_status()
            .with_context(|| format!("{url} returned an error status"))?
            .json()
            .await
            .context("failed to parse apibay response as JSON")?;

        Ok(results
            .into_iter()
            // A query with no matches returns a single sentinel row
            // (id="0", an all-zero info_hash) rather than an empty array —
            // has to be filtered out explicitly or it'd be treated as one
            // real (and completely bogus) result. The all-zero hash is
            // itself 40 valid hex characters, so `is_valid_info_hash` alone
            // wouldn't catch it — both checks are needed, not one replacing
            // the other.
            .filter(|r| {
                r.id != "0"
                    && !r.info_hash.chars().all(|c| c == '0')
                    && is_valid_info_hash(&r.info_hash)
            })
            .map(|r| RawReleaseItem {
                title: r.name.clone(),
                link: build_magnet(&r.info_hash, &r.name),
                guid: r.info_hash,
                size_bytes: r.size.parse().ok(),
                seeders: r.seeders.parse().ok(),
                leechers: r.leechers.parse().ok(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_magnet_includes_hash_name_and_trackers() {
        let magnet = build_magnet("ABC123", "Some.Show.S01E01");
        assert!(magnet.starts_with("magnet:?xt=urn:btih:ABC123&dn=Some.Show.S01E01"));
        assert!(magnet.contains("tracker.opentrackr.org"));
    }

    #[test]
    fn parses_a_real_captured_response() {
        let body = r#"[{"id":"51630137","name":"Modern Family S01 (1080p BluRay)","info_hash":"8F87C7C186172F17E35F4512BB1A3E93B614ADED","leechers":"309","seeders":"379","size":"5395577424","num_files":"28","username":"rjaa","added":"1629816694","status":"vip","category":"208","imdb":""}]"#;
        let results: Vec<TpbResult> = serde_json::from_str(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Modern Family S01 (1080p BluRay)");
        assert_eq!(results[0].seeders, "379");
    }

    #[test]
    fn filters_out_the_no_results_sentinel() {
        let body = r#"[{"id":"0","name":"No results returned","info_hash":"0000000000000000000000000000000000000000","leechers":"0","seeders":"0","size":"0","num_files":"0","username":"","added":"0","status":"","category":"0","imdb":""}]"#;
        let results: Vec<TpbResult> = serde_json::from_str(body).unwrap();
        let filtered: Vec<_> = results
            .into_iter()
            .filter(|r| {
                r.id != "0"
                    && !r.info_hash.chars().all(|c| c == '0')
                    && is_valid_info_hash(&r.info_hash)
            })
            .collect();
        assert!(filtered.is_empty());
    }

    #[test]
    fn is_valid_info_hash_accepts_both_real_shapes() {
        assert!(is_valid_info_hash("8F87C7C186172F17E35F4512BB1A3E93B614ADED")); // 40 hex
        assert!(is_valid_info_hash("abcdefghijklmnopqrstuvwxyz234567")); // 32 base32
    }

    // Regression test for a real gap found in review: a malformed
    // `info_hash` from apibay used to flow straight into `build_magnet`
    // with no validation, silently producing a magnet `qbit::extract_btih`
    // can't parse back out — downgrading that grab to the slow polling path
    // with no error surfaced anywhere.
    #[test]
    fn is_valid_info_hash_rejects_malformed_values() {
        assert!(!is_valid_info_hash(""));
        assert!(!is_valid_info_hash("too-short"));
        assert!(!is_valid_info_hash("not-a-hex-string-at-all-nope!!!!!!!!!!!!")); // 40 chars, non-hex
        assert!(!is_valid_info_hash("8F87C7C186172F17E35F4512BB1A3E93B614ADE")); // 39 hex chars
    }
}
