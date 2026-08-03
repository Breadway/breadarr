pub mod rss;
pub mod scrape;
pub mod tpb;

use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq)]
pub struct RawReleaseItem {
    pub title: String,
    /// Magnet URI or direct .torrent download URL — qBittorrent's add-by-URL
    /// endpoint accepts either identically.
    pub link: String,
    pub guid: String,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
}

#[async_trait]
pub trait ReleaseSource {
    /// Fetch current candidate releases. `query` is used by search-driven
    /// sources (e.g. a scraped search page); feed-based sources ignore it
    /// and return everything currently in the feed.
    async fn fetch(&self, query: Option<&str>) -> Result<Vec<RawReleaseItem>>;
}

pub(crate) fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else if c == ' ' {
                "%20".to_string()
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect()
}

pub(crate) fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    // Split on the first character that isn't part of the number, rather
    // than requiring a literal space — some sources render this without one
    // ("38.1GiB"). Requiring a space made `split_once(' ')` return `None`
    // for those, silently leaving `size_bytes` unset rather than failing
    // outright: the gate's size sanity check (`gate.rs`) treats a missing
    // size as "nothing to check" and skips it entirely instead of rejecting
    // the release, so a value this parser simply couldn't read bypassed
    // size validation altogether rather than being caught by it.
    let split_at = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num_part, unit) = s.split_at(split_at);
    let unit = unit.trim();
    let num: f64 = num_part.parse().ok()?;
    let mult = match unit {
        "B" => 1.0,
        // 1337x labels these "KB/MB/GB" (decimal-looking) but the numbers
        // are actually binary (1024-based), matching every other torrent
        // site's convention — treat them the same as the KiB/MiB/GiB nyaa
        // uses.
        "KiB" | "KB" => 1024.0,
        "MiB" | "MB" => 1024.0 * 1024.0,
        "GiB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" | "TB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((num * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gib() {
        assert_eq!(parse_human_size("38.1 GiB"), Some(40909563494));
    }

    #[test]
    fn parses_mib() {
        assert_eq!(parse_human_size("356.5 MiB"), Some(373817344));
    }

    #[test]
    fn rejects_unknown_unit() {
        assert_eq!(parse_human_size("5 XiB"), None);
    }

    // Regression test for a real gap found in review: some sources render
    // this with no space between the number and the unit — the old
    // `split_once(' ')` returned `None` for those, silently leaving
    // `size_bytes` unset (which skips the gate's size sanity check entirely)
    // rather than rejecting a value this parser genuinely couldn't read.
    #[test]
    fn parses_a_size_with_no_space_before_the_unit() {
        assert_eq!(parse_human_size("38.1GiB"), Some(40909563494));
    }
}
