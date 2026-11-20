use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{build_magnet, is_valid_info_hash, urlencode, RawReleaseItem, ReleaseSource};

/// A community-run JSON API mirror of The Pirate Bay's search: unlike
/// 1337x, this is a genuine machine-readable API (not HTML scraping), and
/// unlike 1337x's HTML results table, `info_hash` is enough to build a
/// magnet directly: no second detail-page fetch needed to resolve a link
/// before grabbing. Also proved dramatically more precise in practice:
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

/// apibay's q.php needs an explicit `cat=0` (all categories). Without it,
/// a multi-word query (every movie title plus year, every season-scoped TV
/// query) returns the "No results returned" sentinel even when matches
/// exist: verified 2026-11-20, `?q=VisionQuest%20S01` returned the sentinel
/// while `?q=VisionQuest%20S01&cat=0` returned 10 real results, and the
/// same held for `?q=Arrival%202016`. The client used to omit `cat`, so
/// those searches silently recorded `no_results` and backed off instead of
/// grabbing anything.
fn search_url(api_url: &str, query: &str) -> String {
    format!("{}?q={}&cat=0", api_url, urlencode(query))
}

impl TpbSource {
    pub fn new(api_url: impl Into<String>) -> Self {
        Self {
            api_url: api_url.into(),
            // No total timeout is reqwest's default; the background loop
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
        let url = search_url(&self.api_url, query);
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
            // (id="0", an all-zero info_hash) rather than an empty array;
            // has to be filtered out explicitly or it'd be treated as one
            // real (and completely bogus) result. The all-zero hash is
            // itself 40 valid hex characters, so `is_valid_info_hash` alone
            // wouldn't catch it: both checks are needed, not one replacing
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
    fn search_url_always_asks_for_all_categories() {
        assert_eq!(
            search_url("https://apibay.org/q.php", "VisionQuest S01"),
            "https://apibay.org/q.php?q=VisionQuest%20S01&cat=0"
        );
        assert_eq!(
            search_url("https://apibay.org/q.php", "Arrival 2016"),
            "https://apibay.org/q.php?q=Arrival%202016&cat=0"
        );
    }

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
        assert!(is_valid_info_hash(
            "8F87C7C186172F17E35F4512BB1A3E93B614ADED"
        )); // 40 hex
        assert!(is_valid_info_hash("abcdefghijklmnopqrstuvwxyz234567")); // 32 base32
    }

    // Regression test for a real gap found in review: a malformed
    // `info_hash` from apibay used to flow straight into `build_magnet`
    // with no validation, silently producing a magnet `qbit::extract_btih`
    // can't parse back out: downgrading that grab to the slow polling path
    // with no error surfaced anywhere.
    #[test]
    fn is_valid_info_hash_rejects_malformed_values() {
        assert!(!is_valid_info_hash(""));
        assert!(!is_valid_info_hash("too-short"));
        assert!(!is_valid_info_hash(
            "not-a-hex-string-at-all-nope!!!!!!!!!!!!"
        )); // 40 chars, non-hex
        assert!(!is_valid_info_hash(
            "8F87C7C186172F17E35F4512BB1A3E93B614ADE"
        )); // 39 hex chars
    }
}
