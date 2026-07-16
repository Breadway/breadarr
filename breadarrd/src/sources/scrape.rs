use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use scraper::{Html, Selector};

use super::{parse_human_size, urlencode, RawReleaseItem, ReleaseSource};

const BASE_COOLDOWN_SECS: u64 = 5 * 60;
const MAX_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);
const RATE_LIMIT_MIN_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const RATE_LIMIT_MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// Hard cap on the tracked failure streak — well past the point where
/// `BASE_COOLDOWN_SECS * 4^(streak-1)` already exceeds `MAX_COOLDOWN`, it
/// exists purely so the exponent can never grow large enough to overflow.
const MAX_FAILURE_STREAK: u32 = 10;

/// 1337x's main domain (1337x.to) bans IPs at the Cloudflare WAF level
/// after bursts of automated traffic — a network-level block that no
/// amount of browser-fingerprint evasion gets around. Its community
/// mirrors run on separate domains/Cloudflare zones, so a ban on one
/// doesn't carry over. Requests are round-robined across mirrors (not just
/// tried in a fixed fallback order) so no single domain absorbs the bulk of
/// traffic, and a mirror that fails is demoted into a cooldown rather than
/// re-probed on the very next search.
pub struct ScrapeSource {
    mirrors: Vec<String>,
    client: reqwest::Client,
    ring: Mutex<MirrorRing>,
}

struct MirrorRing {
    next: usize,
    cooldown_until: Vec<Option<Instant>>,
    failure_streak: Vec<u32>,
}

impl MirrorRing {
    fn new(count: usize) -> Self {
        Self {
            next: 0,
            cooldown_until: vec![None; count],
            failure_streak: vec![0; count],
        }
    }
}

/// Distinguishes *why* a mirror attempt failed, since the right cooldown
/// differs: a WAF-issued rate-limit/block response means "this domain is
/// now watching you" and gets a firm minimum cooldown regardless of streak,
/// while a timeout or a challenge page gets the milder exponential ladder.
#[derive(Debug)]
enum MirrorError {
    /// No results table found at all — most likely a Cloudflare challenge
    /// page served with an HTTP 200 (so `error_for_status` wouldn't catch
    /// it), or the mirror's HTML layout has drifted.
    Challenge,
    /// 403/429/503 — an explicit throttle/block signal from the WAF, not a
    /// generic network failure.
    RateLimited {
        retry_after_secs: Option<u64>,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MirrorError::Challenge => write!(f, "no results table found (challenge page?)"),
            MirrorError::RateLimited { retry_after_secs } => {
                write!(f, "rate limited (retry_after={retry_after_secs:?})")
            }
            MirrorError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Returned by [`ScrapeSource::fetch`] when every mirror is either on
/// cooldown or failed this attempt — a distinct type (rather than a plain
/// string error) so callers can `downcast_ref` to trigger cycle-level
/// backoff without string-matching an error message.
#[derive(Debug)]
pub struct AllMirrorsFailed;

impl std::fmt::Display for AllMirrorsFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all 1337x mirrors failed or are in cooldown")
    }
}

impl std::error::Error for AllMirrorsFailed {}

impl ScrapeSource {
    pub fn new(mirrors: Vec<String>) -> Self {
        let count = mirrors.len();
        Self {
            mirrors,
            client: reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
                .timeout(Duration::from_secs(30))
                // Cloudflare's cf_clearance cookie is per-zone (per mirror
                // domain) — holding it means a mirror that challenged once
                // can pass on subsequent requests instead of re-challenging
                // every single time.
                .cookie_store(true)
                .build()
                .expect("reqwest client build"),
            ring: Mutex::new(MirrorRing::new(count)),
        }
    }

    /// Mirror indices to try, starting from the round-robin cursor and
    /// wrapping, skipping any still in cooldown. Advances the cursor
    /// unconditionally (not just on success) so consecutive searches keep
    /// moving through the ring instead of retrying the same start point.
    fn candidate_order(&self) -> Vec<usize> {
        let mut ring = self.ring.lock().expect("mirror ring poisoned");
        let start = ring.next;
        if !self.mirrors.is_empty() {
            ring.next = (ring.next + 1) % self.mirrors.len();
        }
        compute_candidate_order(
            self.mirrors.len(),
            start,
            &ring.cooldown_until,
            Instant::now(),
        )
    }

    fn record_success(&self, idx: usize) {
        let mut ring = self.ring.lock().expect("mirror ring poisoned");
        ring.failure_streak[idx] = 0;
    }

    fn demote(&self, idx: usize, err: &MirrorError) {
        let mut ring = self.ring.lock().expect("mirror ring poisoned");
        let cooldown = match err {
            MirrorError::RateLimited { retry_after_secs } => {
                // A rate-limit response isn't generic flakiness — don't let
                // it ratchet the exponential streak, just apply a firm
                // minimum (or the server's own Retry-After, capped).
                let retry_after = retry_after_secs
                    .map(|s| Duration::from_secs(s).min(RATE_LIMIT_MAX_RETRY_AFTER));
                retry_after
                    .unwrap_or(RATE_LIMIT_MIN_COOLDOWN)
                    .max(RATE_LIMIT_MIN_COOLDOWN)
            }
            MirrorError::Challenge | MirrorError::Other(_) => {
                let streak = (ring.failure_streak[idx] + 1).min(MAX_FAILURE_STREAK);
                ring.failure_streak[idx] = streak;
                let secs = BASE_COOLDOWN_SECS.saturating_mul(4u64.saturating_pow(streak - 1));
                Duration::from_secs(secs).min(MAX_COOLDOWN)
            }
        };
        ring.cooldown_until[idx] = Some(Instant::now() + cooldown);
    }

    async fn search_mirror(
        &self,
        mirror: &str,
        query: &str,
    ) -> std::result::Result<Vec<RawReleaseItem>, MirrorError> {
        let url = format!(
            "{}/search/{}/1/",
            mirror.trim_end_matches('/'),
            urlencode(query)
        );
        let resp = self.client.get(&url).send().await.map_err(|e| {
            MirrorError::Other(anyhow::Error::new(e).context(format!("request to {url} failed")))
        })?;

        let status = resp.status();
        if matches!(status.as_u16(), 403 | 429 | 503) {
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            return Err(MirrorError::RateLimited { retry_after_secs });
        }
        let resp = resp.error_for_status().map_err(|e| {
            MirrorError::Other(
                anyhow::Error::new(e).context(format!("{url} returned an error status")),
            )
        })?;
        let body = resp.text().await.map_err(|e| {
            MirrorError::Other(anyhow::Error::new(e).context("failed to read response body"))
        })?;
        parse_search_results(&body, mirror).ok_or(MirrorError::Challenge)
    }
}

/// Mirror indices to try, in round-robin order starting at `start`,
/// excluding any whose cooldown hasn't elapsed yet. Pure and separately
/// testable from the ring's locking/mutation.
fn compute_candidate_order(
    len: usize,
    start: usize,
    cooldown_until: &[Option<Instant>],
    now: Instant,
) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    (0..len)
        .map(|offset| (start + offset) % len)
        .filter(|&i| cooldown_until[i].is_none_or(|until| now >= until))
        .collect()
}

/// True if `link` is already directly usable by qBittorrent's add-by-URL
/// endpoint (a magnet URI or a direct .torrent download link) — false for
/// a detail-page URL that needs resolving first (what search results give
/// us, since the results table doesn't carry the magnet directly).
pub fn needs_resolution(link: &str) -> bool {
    !(link.starts_with("magnet:") || link.ends_with(".torrent"))
}

/// The search results table only links to a torrent's detail page, not its
/// magnet URI directly — fetches that page and extracts the magnet link.
/// A free function (not tied to a `ScrapeSource`/mirror list) because the
/// detail URL captured at search time already has the right mirror domain
/// baked in, and this needs to be callable from the grab path generically
/// (including review-queue approval, which only has a stored link, not a
/// `ScrapeSource` instance) — called only for the one candidate actually
/// being grabbed, not for every search result, to keep request volume low.
pub async fn resolve_magnet(client: &reqwest::Client, detail_url: &str) -> Result<String> {
    let resp = client
        .get(detail_url)
        .send()
        .await
        .with_context(|| format!("request to {detail_url} failed"))?
        .error_for_status()
        .with_context(|| format!("{detail_url} returned an error status"))?;
    let body = resp.text().await.context("failed to read response body")?;
    extract_magnet(&body).with_context(|| format!("no magnet link found on {detail_url}"))
}

#[async_trait]
impl ReleaseSource for ScrapeSource {
    async fn fetch(&self, query: Option<&str>) -> Result<Vec<RawReleaseItem>> {
        let Some(query) = query else {
            bail!(
                "ScrapeSource requires a search query (this is a search-driven source, not a feed)"
            );
        };
        if self.mirrors.is_empty() {
            bail!("no 1337x mirrors configured");
        }

        for idx in self.candidate_order() {
            let mirror = &self.mirrors[idx];
            match self.search_mirror(mirror, query).await {
                Ok(items) => {
                    self.record_success(idx);
                    return Ok(items);
                }
                Err(e) => {
                    tracing::warn!(mirror, error = %e, "1337x mirror failed, trying next");
                    self.demote(idx, &e);
                }
            }
        }
        Err(anyhow::Error::new(AllMirrorsFailed))
    }
}

/// Pulls the numeric torrent ID out of a 1337x listing/detail href of the
/// shape `/torrent/<id>/<slug>/` (relative) or `https://<mirror>/torrent/
/// <id>/<slug>/` (absolute) — the one part of the URL that's identical
/// across every mirror, unlike the domain. `None` if the href doesn't
/// contain `/torrent/` at all (an unexpected HTML shape), in which case
/// the caller falls back to the full URL rather than losing the item.
fn extract_torrent_id(href: &str) -> Option<&str> {
    href.split("/torrent/")
        .nth(1)?
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
}

/// `None` when the results table itself is missing from the document —
/// most likely a Cloudflare challenge page served with a 200 status (so it
/// never trips `error_for_status`), or the mirror's HTML layout drifted.
/// `Some(vec![])` is a genuine, trustworthy "no results" — those two cases
/// must not be conflated, since the former should demote the mirror and
/// the latter should not.
fn parse_search_results(html: &str, mirror: &str) -> Option<Vec<RawReleaseItem>> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table.table-list").unwrap();
    doc.select(&table_sel).next()?;

    let row_sel = Selector::parse("table.table-list tbody tr").unwrap();
    let name_link_sel = Selector::parse("td.coll-1.name a:not(.icon)").unwrap();
    let seeds_sel = Selector::parse("td.coll-2").unwrap();
    let leeches_sel = Selector::parse("td.coll-3").unwrap();
    let size_sel = Selector::parse("td.coll-4").unwrap();

    let mirror = mirror.trim_end_matches('/');
    let mut items = Vec::new();

    for row in doc.select(&row_sel) {
        let Some(link_el) = row.select(&name_link_sel).next() else {
            continue;
        };
        let Some(href) = link_el.value().attr("href") else {
            continue;
        };
        let title: String = link_el.text().collect();
        let title = title.trim().to_string();
        if title.is_empty() {
            continue;
        }
        let detail_url = if href.starts_with("http") {
            href.to_string()
        } else {
            format!("{mirror}{href}")
        };
        // The dedup key must not depend on which mirror answered: the same
        // physical torrent's `href` path (`/torrent/<id>/<slug>/`) is
        // identical across every mirror, but `detail_url` bakes in
        // whichever mirror happened to serve this particular search — with
        // ~10 mirrors round-robined for ban resilience, using the full URL
        // as `guid` meant the same release looked "new" up to 10 times
        // over, defeating `is_seen`/`mark_seen` dedup entirely and flooding
        // the review queue with duplicates of the same low-confidence
        // match (verified live: one title queued 13 times). The numeric
        // torrent ID is the stable, mirror-invariant identity instead.
        let guid = extract_torrent_id(href)
            .map(|id| format!("1337x:{id}"))
            .unwrap_or_else(|| detail_url.clone());

        let seeders = row
            .select(&seeds_sel)
            .next()
            .and_then(|e| e.text().collect::<String>().trim().parse().ok());
        let leechers = row
            .select(&leeches_sel)
            .next()
            .and_then(|e| e.text().collect::<String>().trim().parse().ok());
        let size_bytes = row
            .select(&size_sel)
            .next()
            .and_then(|e| parse_human_size(e.text().collect::<String>().trim()));

        items.push(RawReleaseItem {
            title,
            // Not directly grabbable yet — a detail-page URL, resolved to
            // a real magnet link via `resolve_magnet` right before the
            // winning candidate is actually sent to qBittorrent.
            link: detail_url,
            guid,
            size_bytes,
            seeders,
            leechers,
        });
    }

    Some(items)
}

fn extract_magnet(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"a[href^="magnet:"]"#).unwrap();
    doc.select(&sel)
        .next()
        .and_then(|e| e.value().attr("href"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ROW: &str = r#"
<table class="table-list table table-responsive table-striped">
<thead><tr><th class="coll-1 name">name</th><th class="coll-2">se</th><th class="coll-3">le</th><th class="coll-date">time</th><th class="coll-4"><span class="size">size</span></th><th class="coll-5">uploader</th></tr></thead>
<tbody>
<tr>
<td class="coll-1 name"><a href="/sub/tv/HD/1/" class="icon"><i class="flaticon-hd"></i></a><a href="/torrent/3250239/The-Big-Bang-Theory-S12E01-720p-HDTV-x264-KILLERS-eztv/">The Big Bang Theory S12E01 720p HDTV x264-KILLERS [eztv]</a></td>
<td class="coll-2 seeds">3978</td>
<td class="coll-3 leeches">1443</td>
<td class="coll-date">Sep. 25th  '18</td>
<td class="coll-4 size mob-uploader">559 MB</td>
<td class="coll-5 uploader"><a href="/user/EZTVag/">EZTVag</a></td>
</tr>
</tbody>
</table>"#;

    #[test]
    fn parses_a_real_captured_result_row() {
        let items = parse_search_results(SAMPLE_ROW, "https://13377x.info").unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item.title,
            "The Big Bang Theory S12E01 720p HDTV x264-KILLERS [eztv]"
        );
        assert_eq!(
            item.link,
            "https://13377x.info/torrent/3250239/The-Big-Bang-Theory-S12E01-720p-HDTV-x264-KILLERS-eztv/"
        );
        assert_eq!(item.guid, "1337x:3250239");
        assert_eq!(item.seeders, Some(3978));
        assert_eq!(item.leechers, Some(1443));
        assert_eq!(item.size_bytes, Some(586153984));
    }

    #[test]
    fn guid_is_identical_across_different_mirrors_for_the_same_torrent() {
        let a = parse_search_results(SAMPLE_ROW, "https://13377x.info").unwrap();
        let b = parse_search_results(SAMPLE_ROW, "https://1337x.maskbay.info").unwrap();
        // The `link` legitimately differs (it's used to actually fetch from
        // whichever mirror answered this search) but the dedup `guid` must
        // not, or mirror rotation defeats `is_seen`/`mark_seen` entirely —
        // this was a real bug that flooded the review queue with up to 13
        // duplicate entries for the same release.
        assert_ne!(a[0].link, b[0].link);
        assert_eq!(a[0].guid, b[0].guid);
    }

    #[test]
    fn extract_torrent_id_handles_relative_and_absolute_hrefs() {
        assert_eq!(
            extract_torrent_id("/torrent/3602010/Instant-Family-2018/"),
            Some("3602010")
        );
        assert_eq!(
            extract_torrent_id("https://13377x.info/torrent/3602010/Instant-Family-2018/"),
            Some("3602010")
        );
    }

    #[test]
    fn extract_torrent_id_is_none_for_an_unexpected_shape() {
        assert_eq!(extract_torrent_id("/sub/tv/HD/1/"), None);
    }

    #[test]
    fn returns_none_when_results_table_is_absent() {
        let challenge_page = "<html><body><h1>Just a moment...</h1></body></html>";
        assert_eq!(
            parse_search_results(challenge_page, "https://13377x.info"),
            None
        );
    }

    #[test]
    fn returns_empty_vec_for_a_genuine_no_results_page() {
        let empty_results = r#"<table class="table-list table table-responsive table-striped">
<thead><tr><th class="coll-1 name">name</th></tr></thead>
<tbody></tbody>
</table>"#;
        assert_eq!(
            parse_search_results(empty_results, "https://13377x.info"),
            Some(vec![])
        );
    }

    #[test]
    fn extracts_magnet_from_detail_page() {
        let html = r#"<a href="magnet:?xt=urn:btih:F30F455DC4C64A38E18C79C853B2A80B417C2343&dn=test">Magnet Download</a>"#;
        assert_eq!(
            extract_magnet(html).as_deref(),
            Some("magnet:?xt=urn:btih:F30F455DC4C64A38E18C79C853B2A80B417C2343&dn=test")
        );
    }

    #[test]
    fn urlencode_handles_spaces_and_unicode() {
        assert_eq!(urlencode("Big Buck Bunny"), "Big%20Buck%20Bunny");
    }

    #[test]
    fn candidate_order_round_robins_from_start() {
        let cooldowns = vec![None, None, None, None];
        let order = compute_candidate_order(4, 2, &cooldowns, Instant::now());
        assert_eq!(order, vec![2, 3, 0, 1]);
    }

    #[test]
    fn candidate_order_skips_mirrors_still_in_cooldown() {
        let now = Instant::now();
        let cooldowns = vec![
            None,
            Some(now + Duration::from_secs(600)), // still cooling down
            None,
            Some(now - Duration::from_secs(1)), // cooldown already elapsed
        ];
        let order = compute_candidate_order(4, 0, &cooldowns, now);
        assert_eq!(order, vec![0, 2, 3]);
    }

    #[test]
    fn candidate_order_empty_when_every_mirror_cooling_down() {
        let now = Instant::now();
        let cooldowns = vec![Some(now + Duration::from_secs(60)); 3];
        let order = compute_candidate_order(3, 0, &cooldowns, now);
        assert!(order.is_empty());
    }

    #[test]
    fn demote_applies_a_firm_minimum_cooldown_for_rate_limiting() {
        let source = ScrapeSource::new(vec!["https://a".into(), "https://b".into()]);
        source.demote(
            0,
            &MirrorError::RateLimited {
                retry_after_secs: None,
            },
        );
        let ring = source.ring.lock().unwrap();
        let until = ring.cooldown_until[0].expect("should be cooling down");
        assert!(until >= Instant::now() + Duration::from_secs(29 * 60));
    }

    #[test]
    fn demote_caps_rate_limit_retry_after_at_one_hour() {
        let source = ScrapeSource::new(vec!["https://a".into()]);
        source.demote(
            0,
            &MirrorError::RateLimited {
                retry_after_secs: Some(999_999),
            },
        );
        let ring = source.ring.lock().unwrap();
        let until = ring.cooldown_until[0].expect("should be cooling down");
        assert!(until <= Instant::now() + RATE_LIMIT_MAX_RETRY_AFTER + Duration::from_secs(5));
    }

    #[test]
    fn demote_escalates_generic_failures_exponentially() {
        let source = ScrapeSource::new(vec!["https://a".into()]);
        source.demote(0, &MirrorError::Challenge);
        let first = source.ring.lock().unwrap().cooldown_until[0].unwrap();
        source.demote(0, &MirrorError::Challenge);
        let second = source.ring.lock().unwrap().cooldown_until[0].unwrap();
        assert!(second > first);
    }

    #[test]
    fn record_success_resets_failure_streak() {
        let source = ScrapeSource::new(vec!["https://a".into()]);
        source.demote(0, &MirrorError::Challenge);
        assert_eq!(source.ring.lock().unwrap().failure_streak[0], 1);
        source.record_success(0);
        assert_eq!(source.ring.lock().unwrap().failure_streak[0], 0);
    }
}
