use anyhow::Result;
use async_trait::async_trait;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::{parse_human_size, urlencode, RawReleaseItem, ReleaseSource};

/// RSS source for nyaa.si-style feeds, whose `nyaa:seeders`/`nyaa:leechers`/
/// `nyaa:size` custom-namespaced fields carry the seeder data this project
/// needs — generic feed libraries (e.g. feed-rs) don't surface arbitrary
/// vendor namespaces, so this parses the raw XML directly.
pub struct RssSource {
    feed_url: String,
    client: reqwest::Client,
}

impl RssSource {
    pub fn new(feed_url: impl Into<String>) -> Self {
        Self {
            feed_url: feed_url.into(),
            // The background loop holds the DB mutex across this fetch — no
            // total timeout (reqwest's default) means a stalled connection
            // hangs the whole daemon, not just this one request.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client build"),
        }
    }
}

#[async_trait]
impl ReleaseSource for RssSource {
    async fn fetch(&self, query: Option<&str>) -> Result<Vec<RawReleaseItem>> {
        let url = build_search_url(&self.feed_url, query);
        let bytes = self.client.get(&url).send().await?.bytes().await?;
        parse_nyaa_rss(&bytes)
    }
}

/// nyaa's search *is* its RSS feed with a `q` param appended — same
/// endpoint, same custom-namespace fields, so the parser needs no changes
/// at all for search-driven use.
fn build_search_url(feed_url: &str, query: Option<&str>) -> String {
    match query {
        Some(q) => {
            let sep = if feed_url.contains('?') { '&' } else { '?' };
            format!("{feed_url}{sep}q={}", urlencode(q))
        }
        None => feed_url.to_string(),
    }
}

fn parse_nyaa_rss(bytes: &[u8]) -> Result<Vec<RawReleaseItem>> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);

    let mut items = Vec::new();
    let mut buf = Vec::new();

    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut title = None;
    let mut link = None;
    let mut guid = None;
    let mut seeders = None;
    let mut leechers = None;
    let mut size_bytes = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "item" {
                    in_item = true;
                    title = None;
                    link = None;
                    guid = None;
                    seeders = None;
                    leechers = None;
                    size_bytes = None;
                }
                cur_tag = name;
            }
            Event::Text(t) if in_item => {
                let raw = t.decode()?;
                let text = unescape(&raw)?.into_owned();
                match cur_tag.as_str() {
                    "title" => title = Some(text),
                    "link" => link = Some(text),
                    "guid" => guid = Some(text),
                    "nyaa:seeders" => seeders = text.parse().ok(),
                    "nyaa:leechers" => leechers = text.parse().ok(),
                    "nyaa:size" => size_bytes = parse_human_size(&text),
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "item" {
                    if let (Some(title), Some(link), Some(guid)) =
                        (title.take(), link.take(), guid.take())
                    {
                        items.push(RawReleaseItem {
                            title,
                            link,
                            guid,
                            size_bytes,
                            seeders,
                            leechers,
                        });
                    }
                    in_item = false;
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:atom="http://www.w3.org/2005/Atom" xmlns:nyaa="https://nyaa.si/xmlns/nyaa" version="2.0">
<channel>
<title>Nyaa - Home - Torrent File RSS</title>
<item>
    <title>[Group] Some Show - 05 [1080p]</title>
    <link>https://nyaa.si/download/2130903.torrent</link>
    <guid isPermaLink="true">https://nyaa.si/view/2130903</guid>
    <pubDate>Sat, 11 Jul 2026 08:51:04 -0000</pubDate>
    <nyaa:seeders>12</nyaa:seeders>
    <nyaa:leechers>3</nyaa:leechers>
    <nyaa:size>356.5 MiB</nyaa:size>
</item>
</channel>
</rss>"#;

    #[test]
    fn parses_nyaa_item_with_custom_fields() {
        let items = parse_nyaa_rss(SAMPLE.as_bytes()).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.title, "[Group] Some Show - 05 [1080p]");
        assert_eq!(item.link, "https://nyaa.si/download/2130903.torrent");
        assert_eq!(item.guid, "https://nyaa.si/view/2130903");
        assert_eq!(item.seeders, Some(12));
        assert_eq!(item.leechers, Some(3));
        assert_eq!(item.size_bytes, Some(373817344));
    }

    #[test]
    fn build_search_url_appends_query_param() {
        assert_eq!(
            build_search_url("https://nyaa.si/?page=rss&c=1_2", Some("Some Movie 2016")),
            "https://nyaa.si/?page=rss&c=1_2&q=Some%20Movie%202016"
        );
    }

    #[test]
    fn build_search_url_handles_a_feed_url_with_no_existing_query_string() {
        assert_eq!(
            build_search_url("https://nyaa.si/rss", Some("Some Movie")),
            "https://nyaa.si/rss?q=Some%20Movie"
        );
    }

    #[test]
    fn build_search_url_is_unchanged_without_a_query() {
        assert_eq!(
            build_search_url("https://nyaa.si/?page=rss", None),
            "https://nyaa.si/?page=rss"
        );
    }

    /// Hits the real nyaa.si feed — not run by default. `cargo test -- --ignored`
    /// to sanity-check the parser against live markup if nyaa changes their format.
    #[tokio::test]
    #[ignore]
    async fn parses_live_nyaa_feed() {
        let source = RssSource::new("https://nyaa.si/?page=rss");
        let items = source.fetch(None).await.unwrap();
        assert!(
            !items.is_empty(),
            "expected at least one item from live feed"
        );
        assert!(items.iter().any(|i| i.seeders.is_some()));
    }
}
