use anyhow::Result;
use async_trait::async_trait;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::{parse_human_size, urlencode, RawReleaseItem, ReleaseSource};

/// RSS source for nyaa.si-style feeds, whose `nyaa:seeders`/`nyaa:leechers`/
/// `nyaa:size` custom-namespaced fields carry the seeder data this project
/// needs: generic feed libraries (e.g. feed-rs) don't surface arbitrary
/// vendor namespaces, so this parses the raw XML directly.
pub struct RssSource {
    feed_url: String,
    client: reqwest::Client,
}

impl RssSource {
    pub fn new(feed_url: impl Into<String>) -> Self {
        Self {
            feed_url: feed_url.into(),
            // The background loop holds the DB mutex across this fetch; no
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
        let bytes = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        parse_nyaa_rss(&bytes)
    }
}

/// nyaa's search *is* its RSS feed with a `q` param appended: same
/// endpoint, same custom-namespace fields, so the parser needs no changes
/// at all for search-driven use.
fn build_search_url(feed_url: &str, query: Option<&str>) -> String {
    match query {
        Some(q) => {
            let sep = if feed_url.contains('?') { '&' } else { '?' };
            format!("{feed_url}{sep}q={}&s=seeders&o=desc", urlencode(q))
        }
        None => feed_url.to_string(),
    }
}

/// Every field accumulated across one `<item>`'s `Text`/`CData` events,
/// bundled into one struct (rather than six separate `&mut Option<_>`
/// parameters) purely to keep `accumulate_field` under clippy's
/// too-many-arguments threshold.
#[derive(Default)]
struct ItemFields {
    title: Option<String>,
    link: Option<String>,
    guid: Option<String>,
    seeders: Option<u32>,
    leechers: Option<u32>,
    size_bytes: Option<u64>,
}

/// Appends rather than overwrites title/link/guid: a real feed can split a
/// single logical value across more than one `Text`/`CData` event for the
/// same tag (mixed content, or just a parser buffer boundary); a plain
/// assignment would silently keep only the *last* fragment, truncating the
/// value. `nyaa:seeders`/`nyaa:leechers`/`nyaa:size` stay parse-and-overwrite
/// since they're short numeric/size tokens, not free text expected to span
/// multiple events.
fn accumulate_field(tag: &str, text: &str, fields: &mut ItemFields) {
    match tag {
        "title" => fields.title.get_or_insert_with(String::new).push_str(text),
        "link" => fields.link.get_or_insert_with(String::new).push_str(text),
        "guid" => fields.guid.get_or_insert_with(String::new).push_str(text),
        "nyaa:seeders" => fields.seeders = text.parse().ok(),
        "nyaa:leechers" => fields.leechers = text.parse().ok(),
        "nyaa:size" => fields.size_bytes = parse_human_size(text),
        _ => {}
    }
}

fn parse_nyaa_rss(bytes: &[u8]) -> Result<Vec<RawReleaseItem>> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);

    let mut items = Vec::new();
    let mut buf = Vec::new();

    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut fields = ItemFields::default();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "item" {
                    in_item = true;
                    fields = ItemFields::default();
                }
                cur_tag = name;
            }
            Event::Text(t) if in_item => {
                let raw = t.decode()?;
                let text = unescape(&raw)?.into_owned();
                accumulate_field(&cur_tag, &text, &mut fields);
            }
            // CDATA content is raw text by definition: XML entity escaping
            // doesn't apply inside a CDATA section (running `unescape()` on
            // it would misinterpret a literal "&amp;" as an escaped
            // ampersand), so this decodes without it. Many real-world feeds
            // wrap `<title>`/`<link>` in CDATA; previously only
            // `Event::Text` was handled at all, so those items silently
            // came back with `title = None` and were dropped at the
            // `item`-close check below with zero error, pointing
            // `nyaa_rss_url` at a CDATA-heavy feed yielded zero items, not
            // a visible failure.
            Event::CData(t) if in_item => {
                let text = t.decode()?.into_owned();
                accumulate_field(&cur_tag, &text, &mut fields);
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "item" {
                    if let (Some(title), Some(link), Some(guid)) =
                        (fields.title.take(), fields.link.take(), fields.guid.take())
                    {
                        items.push(RawReleaseItem {
                            title,
                            link,
                            guid,
                            size_bytes: fields.size_bytes,
                            seeders: fields.seeders,
                            leechers: fields.leechers,
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

    // Regression test for a real gap found in review: many real-world feeds
    // wrap `<title>`/`<link>`/`<guid>` in CDATA rather than plain text
    // content (often to avoid having to XML-escape ampersands/brackets
    // common in release titles). Previously only `Event::Text` was
    // handled: `Event::CData` was silently ignored, so every field
    // wrapped this way came back `None` and the whole item was dropped at
    // the `item`-close check with no error surfaced at all.
    #[test]
    fn parses_cdata_wrapped_fields() {
        const CDATA_SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:nyaa="https://nyaa.si/xmlns/nyaa" version="2.0">
<channel>
<item>
    <title><![CDATA[[Group] Some Show & Friends - 05 [1080p]]]></title>
    <link><![CDATA[https://nyaa.si/download/2130903.torrent]]></link>
    <guid isPermaLink="true"><![CDATA[https://nyaa.si/view/2130903]]></guid>
    <nyaa:seeders>12</nyaa:seeders>
    <nyaa:leechers>3</nyaa:leechers>
    <nyaa:size>356.5 MiB</nyaa:size>
</item>
</channel>
</rss>"#;

        let items = parse_nyaa_rss(CDATA_SAMPLE.as_bytes()).unwrap();
        assert_eq!(items.len(), 1, "a CDATA-wrapped item must not be silently dropped");
        let item = &items[0];
        // A literal "&" survives verbatim: CDATA content isn't
        // XML-entity-escaped, so this must NOT come back as "&amp;".
        assert_eq!(item.title, "[Group] Some Show & Friends - 05 [1080p]");
        assert_eq!(item.link, "https://nyaa.si/download/2130903.torrent");
        assert_eq!(item.guid, "https://nyaa.si/view/2130903");
        assert_eq!(item.seeders, Some(12));
    }

    #[test]
    fn build_search_url_appends_query_param() {
        assert_eq!(
            build_search_url("https://nyaa.si/?page=rss&c=1_2", Some("Some Movie 2016")),
            "https://nyaa.si/?page=rss&c=1_2&q=Some%20Movie%202016&s=seeders&o=desc"
        );
    }

    #[test]
    fn build_search_url_handles_a_feed_url_with_no_existing_query_string() {
        assert_eq!(
            build_search_url("https://nyaa.si/rss", Some("Some Movie")),
            "https://nyaa.si/rss?q=Some%20Movie&s=seeders&o=desc"
        );
    }

    #[test]
    fn build_search_url_is_unchanged_without_a_query() {
        assert_eq!(
            build_search_url("https://nyaa.si/?page=rss", None),
            "https://nyaa.si/?page=rss"
        );
    }

    /// Hits the real nyaa.si feed: not run by default. `cargo test -- --ignored`
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
