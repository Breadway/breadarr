use std::sync::LazyLock;

use regex::Regex;

use super::{Codec, Source, WS_RE};

pub(super) static GROUP_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[[^\]]+\]\s*").unwrap());

static LEADING_GROUP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[([^\]]+)\]").unwrap());

static CONTAINER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.(mkv|mp4|avi)\b").unwrap());

static RESOLUTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(2160p|1080p|720p|480p|4k)\b").unwrap());

static SOURCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(BDRemux|Remux|Blu-?Ray|BDRip|WEB-?DL|WEBRip|HDTV)\b").unwrap()
});

static CODEC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(AV1|HEVC|H\.?\s?265|x265|H\.?\s?264|x264|AVC)\b").unwrap()
});

static BIT_DEPTH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(8|10)-?bit\b").unwrap());

pub(super) static REPACK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(REPACK|PROPER)\b").unwrap());

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[(\[](19|20)\d{2}[)\]]").unwrap());
// Scene-style releases ("Dune.1984.1080p.BluRay.x264-GROUP") carry the year
// bare, with no surrounding brackets — `YEAR_RE` above never matches these
// at all, which meant `movie_year_mismatch` (the only defense against a
// same-named-but-wrong film once a title auto-matches) silently never fired
// on exactly the naming convention TPB/1337x results actually use.
static BARE_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b((?:19|20)\d{2})\b").unwrap());

// The trailing `v\d+` is a fansub revision tag ("v2" = "second release of
// this episode, fixed encode/subs") stuck directly onto the episode number
// with no separator — "S01E01v2". Without consuming it before the `\b`,
// the boundary check fails outright (digit→letter isn't a word boundary),
// so the whole pattern silently doesn't match and the file falls through
// to unparsed (verified live: every v2 release of several shows, e.g. an
// entire show that only had v2 releases, ended up with zero linked
// episode files during a library scan).
static SXXEXX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bS(\d{1,2})E(\d{1,3})(?:v\d+)?\b").unwrap());
static SXX_DASH_EP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bS(\d{1,2})\s*-\s*(\d{1,3})\b").unwrap());
// A bare season marker with no episode number attached — "S01", "Season 8",
// "Season.1", optionally with a trailing "Complete"/spelled-out season word
// (e.g. "[Season 4 Four Complete]") that's irrelevant to the number itself.
// Previously required literal parens around an explicit "S?N Complete)"
// shape, matching only one specific release-group convention; real
// releases routinely drop the parens, drop "Complete" entirely (e.g.
// "Game of Thrones - Season 8 S08 - 2019"), or spell "Season" out with a
// dot instead of a space (verified live: several real review-queue entries
// failed to resolve at all — season came back `None` — because none of
// these shapes matched the old pattern). Only reached after
// `SXXEXX_RE`/`SXX_DASH_EP_RE` have already failed to find an actual
// episode number, so treating a bare season marker as a pack signal here is
// safe.
static SEASON_PACK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bS(?:eason)?\.?\s*(\d{1,2})\b").unwrap());
static DASH_EPISODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\s*(\d{1,3})\b").unwrap());

// A batch/season-pack release covering many episodes in one torrent.
// `SXXEXX_RE`/`DASH_EPISODE_RE` above would otherwise happily resolve one
// of these to a single arbitrary episode number (verified live:
// "Show (01-12) [1080p]" resolves to episode 12, "Show - 01-12 [1080p]"
// resolves to episode 1, "Show S01E01-E12" resolves to S01E01) — the
// importer then grabs the whole multi-episode torrent, picks whichever
// file happens to be largest, and files it under that one guessed episode
// while silently discarding the rest. `looks_like_episode_range` below is
// checked first so these get refused rather than mis-resolved.
static BATCH_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(batch|complete)\b").unwrap());
// "S01E01-E12" / "S01E01-12": no word-boundary exists between the digits
// and letters in a run like "S01E01-E12" (letters and digits are both
// \w), so a boundary-anchored generic range pattern can't find this —
// needs its own literal S..E..-..E?.. shape.
static SXX_EPISODE_RANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bS(\d{1,2})E(\d{1,3})\s*-\s*E?(\d{1,3})\b").unwrap());
// Bare numeric ranges: "(01-12)", "01-12", "01~12". Requires the second
// number to be strictly larger than the first (a real episode range
// always counts up) so this doesn't fire on, say, an unrelated dash
// elsewhere in the title with a smaller trailing number.
static BARE_EPISODE_RANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{1,3})\s*[-~]\s*(\d{1,3})\b").unwrap());
// A `YYYY-MM-DD` date ("2024-01-15"): the year's 4 digits are too many for
// `BARE_EPISODE_RANGE_RE`'s/`DASH_EPISODE_RE`'s `\d{1,3}` to match as a
// whole, so those regexes' *first real match* on a date-named release ends
// up starting at the month ("01-15", or "01" alone) instead — a bare
// month/day pair, not a real episode range or episode number. Matched as a
// whole date span (not just a "year-" prefix check) so both the month *and*
// the day segment are covered — checking only the text immediately before a
// candidate match would still let "15" in "2024-01-15" slip through as a
// false "episode 15" once "01" alone was correctly rejected.
static DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:19|20)\d{2}-\d{1,2}-\d{1,2}\b").unwrap());

fn overlaps_a_date(s: &str, start: usize, end: usize) -> bool {
    DATE_RE.find_iter(s).any(|d| d.start() <= start && end <= d.end())
}

pub(super) fn looks_like_episode_range(s: &str) -> bool {
    if BATCH_WORD_RE.is_match(s) || SXX_EPISODE_RANGE_RE.is_match(s) {
        return true;
    }
    BARE_EPISODE_RANGE_RE.captures_iter(s).any(|c| {
        let first = c.get(1).unwrap();
        let second = c.get(2).unwrap();
        let a: u32 = first.as_str().parse().unwrap_or(0);
        let b: u32 = second.as_str().parse().unwrap_or(0);
        if b <= a {
            return false;
        }
        if overlaps_a_date(s, first.start(), second.end()) {
            return false;
        }
        // "Season 2 - 25": the range's first number is really a season
        // marker's own number (checked by comparing spans, not just text,
        // so this only fires when the two genuinely overlap), not a range
        // start — "2 - 25" isn't a real episode range, it's "season 2,
        // episode 25", resolved separately in `extract_episode_info`.
        let is_season_marker_number = SEASON_PACK_RE.captures(s).is_some_and(|sc| {
            sc.get(1).unwrap().range() == first.range()
        });
        !is_season_marker_number
    })
}

pub(super) fn extract_group(s: &str) -> Option<String> {
    LEADING_GROUP_RE
        .captures(s)
        .map(|c| c[1].trim().to_string())
}

pub(super) fn extract_container(s: &str) -> Option<String> {
    CONTAINER_RE.captures(s).map(|c| c[1].to_lowercase())
}

pub(super) fn extract_resolution(s: &str) -> Option<u32> {
    let m = RESOLUTION_RE.captures(s)?;
    let token = m[1].to_lowercase();
    if token == "4k" {
        return Some(2160);
    }
    token.trim_end_matches('p').parse().ok()
}

pub(super) fn extract_source(s: &str) -> Option<Source> {
    let token = SOURCE_RE.captures(s)?[1].to_lowercase();
    Some(match token.as_str() {
        "bdremux" | "remux" => Source::Remux,
        t if t.replace('-', "") == "bluray" => Source::BluRay,
        "bdrip" => Source::BluRay,
        t if t.replace('-', "") == "webdl" => Source::WebDl,
        "webrip" => Source::WebRip,
        "hdtv" => Source::Hdtv,
        _ => return None,
    })
}

pub(super) fn extract_codec(s: &str) -> Option<Codec> {
    let token = CODEC_RE.captures(s)?[1]
        .to_lowercase()
        .replace([' ', '.'], "");
    Some(match token.as_str() {
        "av1" => Codec::Av1,
        "hevc" | "h265" | "x265" => Codec::Hevc,
        "h264" | "x264" | "avc" => Codec::H264,
        _ => return None,
    })
}

pub(super) fn extract_bit_depth(s: &str) -> Option<u8> {
    BIT_DEPTH_RE.captures(s)?[1].parse().ok()
}

pub(super) fn extract_year(s: &str) -> Option<u32> {
    if let Some(m) = YEAR_RE.find(s) {
        return s[m.start() + 1..m.end() - 1].parse().ok();
    }
    // Bare-year fallback, but never at the very start of the string — a
    // movie literally titled after a year ("1917", "2012") would otherwise
    // have its own title mistaken for a year with nothing left over. Same
    // guard `library_scan.rs`'s `FOLDER_BARE_YEAR_RE` uses.
    let m = BARE_YEAR_RE.find(s)?;
    if m.start() == 0 {
        return None;
    }
    s[m.start()..m.end()].parse().ok()
}

/// `DASH_EPISODE_RE`'s first match that isn't actually the month of a
/// `YYYY-MM-DD` date. A bare `-\s*\d{1,3}\b` alone can't tell "Show - 25
/// [1080p]" (a real episode number) apart from "...2024-01-15..." (the "01"
/// is just a month, matched for the same reason `BARE_EPISODE_RANGE_RE`
/// does in `looks_like_episode_range` — the 4-digit year is too many digits
/// to match as a whole, so the regex's first real match starts one segment
/// later). Reused by every `extract_episode_info` branch that falls back to
/// `DASH_EPISODE_RE`, not just the range-detection path, since the date
/// misread happens independently of whether `looks_like_episode_range`
/// fires.
fn find_real_dash_episode(s: &str) -> Option<regex::Captures<'_>> {
    DASH_EPISODE_RE.captures_iter(s).find(|c| {
        let m = c.get(0).unwrap();
        !overlaps_a_date(s, m.start(), m.end())
    })
}

/// Returns (season, episode, absolute_episode, title_span_end) — the last
/// element is the byte offset in `s` where the episode/season token (or,
/// failing that, the first quality marker) begins, used to slice out the
/// title portion.
pub(super) fn extract_episode_info(s: &str) -> (Option<u32>, Option<u32>, Option<u32>, usize) {
    if looks_like_episode_range(s) {
        // Season is still useful to surface (e.g. for display/logging)
        // when it's unambiguous, but episode/absolute_episode are
        // deliberately left unresolved — see `looks_like_episode_range`'s
        // doc comment for why guessing one is actively harmful here.
        let season = SXXEXX_RE
            .captures(s)
            .and_then(|c| c[1].parse().ok())
            .or_else(|| SEASON_PACK_RE.captures(s).and_then(|c| c[1].parse().ok()));
        let end = first_quality_marker(s).unwrap_or(s.len());
        return (season, None, None, end);
    }
    if let Some(c) = SXXEXX_RE.captures(s) {
        let season = c[1].parse().ok();
        let episode = c[2].parse().ok();
        return (season, episode, None, c.get(0).unwrap().start());
    }
    if let Some(c) = SXX_DASH_EP_RE.captures(s) {
        let season = c[1].parse().ok();
        let episode = c[2].parse().ok();
        return (season, episode, None, c.get(0).unwrap().start());
    }
    if let Some(c) = SEASON_PACK_RE.captures(s) {
        let season = c[1].parse().ok();
        // A season marker immediately followed elsewhere by a dash-number
        // ("Season 2 - 25") names one episode within that season, not a
        // season-only pack — checked here rather than reordering the checks
        // above `SXX_DASH_EP_RE`/`SXXEXX_RE` still get first crack at more
        // specific shapes, and a genuine season-only pack (no trailing
        // dash-number anywhere) is unaffected.
        if let Some(ep) = find_real_dash_episode(s) {
            let episode: Option<u32> = ep[1].parse().ok();
            return (season, episode, None, c.get(0).unwrap().start());
        }
        return (season, None, None, c.get(0).unwrap().start());
    }
    if let Some(c) = find_real_dash_episode(s) {
        let episode: Option<u32> = c[1].parse().ok();
        return (None, episode, episode, c.get(0).unwrap().start());
    }

    let end = first_quality_marker(s).unwrap_or(s.len());
    (None, None, None, end)
}

fn first_quality_marker(s: &str) -> Option<usize> {
    [
        RESOLUTION_RE.find(s).map(|m| m.start()),
        SOURCE_RE.find(s).map(|m| m.start()),
        CODEC_RE.find(s).map(|m| m.start()),
        YEAR_RE.find(s).map(|m| m.start()),
    ]
    .into_iter()
    .flatten()
    .min()
}

pub(super) fn derive_title(s: &str, span_end: usize) -> String {
    let candidate = &s[..span_end.min(s.len())];
    let trimmed = candidate
        .trim()
        .trim_end_matches(['-', ':', '('])
        .trim_end_matches(char::is_whitespace);
    WS_RE.replace_all(trimmed, " ").trim().to_string()
}
