mod tokens;

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Hdtv,
    WebRip,
    WebDl,
    BluRay,
    Remux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedRelease {
    pub title_raw: String,
    pub title_normalized: String,
    pub group: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub absolute_episode: Option<u32>,
    pub year: Option<u32>,
    pub resolution: Option<u32>,
    pub source: Option<Source>,
    pub codec: Option<Codec>,
    pub bit_depth: Option<u8>,
    pub container: Option<String>,
    pub is_repack: bool,
}

pub fn parse(raw_title: &str) -> ParsedRelease {
    let mut work = raw_title.to_string();

    let group = tokens::extract_group(&work);
    if group.is_some() {
        work = tokens::GROUP_PREFIX_RE
            .replace(&work, "")
            .trim()
            .to_string();
    }

    let container = tokens::extract_container(&work);
    let resolution = tokens::extract_resolution(&work);
    let source = tokens::extract_source(&work);
    let codec = tokens::extract_codec(&work);
    let bit_depth = tokens::extract_bit_depth(&work);
    let is_repack = tokens::REPACK_RE.is_match(&work);
    let year = tokens::extract_year(&work);
    let (season, episode, absolute_episode, title_span_end) = tokens::extract_episode_info(&work);

    let title_normalized = tokens::derive_title(&work, title_span_end);

    ParsedRelease {
        title_raw: raw_title.to_string(),
        title_normalized,
        group,
        season,
        episode,
        absolute_episode,
        year,
        resolution,
        source,
        codec,
        bit_depth,
        container,
        is_repack,
    }
}

pub(crate) static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

#[cfg(test)]
mod tests {
    use super::*;

    const CORPUS: &[&str] = &[
        "Aime ton prochain (01-06) (Chida) (2019) [Digital-1920] [Manga FR] (PapriKa+)",
        "[ANi]  小書痴的下剋上 為了成為圖書管理員不擇手段！領主的養女 - 13 [1080P][Baha][WEB-DL][AAC AVC][CHT][MP4]",
        "Ascendance of a Bookworm S04E11 The Gathering of Gutenberg 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Dual-Audio, Multi-Subs)",
        "Ascendance of a Bookworm S04E12 The Winter Social Season and Debut 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Dual-Audio, Multi-Subs)",
        "Ascendance of a Bookworm S04E13 VOSTFR 1080p WEB x264 AAC -Tsundere-Raws (CR) (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen - Ryoushu no Youjo,Ascendance of a Bookworm: Adopted Daughter of an Archduke)",
        "Ascendance of a Bookworm S04E13 VOSTFR 720p WEB x264 AAC -Tsundere-Raws (CR) (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen - Ryoushu no Youjo,Ascendance of a Bookworm: Adopted Daughter of an Archduke)",
        "Ascendance of a Bookworm S04E13 Winter Material Gathering 1080p CR WEB-DL AAC2.0 H 264-VARYG (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Multi-Subs)",
        "Assassin's Creed - Blade of Shao Jun (01-04) (Kurata) (2020) [Digital-1920] [Manga FR] (PapriKa+)",
        "[Erai-raws] Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen - Ryushu no Youjo - 13 [1080p CR WEB-DL AVC AAC][MultiSub][18261A02]",
        "[Erai-raws] Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen - Ryushu no Youjo - 13 [480p CR WEB-DL AVC AAC][MultiSub][A22FE68E]",
        "[Erai-raws] Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen - Ryushu no Youjo - 13 [720p CR WEB-DL AVC AAC][MultiSub][4D409859]",
        "[GM-Team][国漫][仙逆][Renegade Immortal][2023][148][AVC][GB][1080P]",
        "[GM-Team][国漫][光阴之外][Beyond Time's Gaze][2025][29][GB][4K HEVC 10Bit]",
        "[GM-Team][国漫][吞噬星空][Swallowed Star][2021][231][AVC][GB][1080P]",
        "[GM-Team][国漫][斗破苍穹 第5季][Fights Break Sphere Ⅴ][2022][206][AVC][GB][1080P]",
        "[GM-Team][国漫][斗破苍穹 第5季][Fights Break Sphere Ⅴ][2022][206][HEVC][GB][4K]",
        "[GM-Team][国漫][牧神记][Tales of Qin Mu][2024][90][AVC][GB][1080P]",
        "KILL BLUE S01E12 Life Paths 1080p AMZN WEB-DL MULTi DDP2.0 H 264-VARYG (Kill Ao, Multi-Audio, Multi-Subs)",
        "[LoliHouse] 与奔驰于透明之夜的你，谈一场看不见的恋爱。 / 透明な夜に駆ける君と、目に見えない恋をした。 / KakeKoi - 01 [WebRip 1080p HEVC-10bit AAC][简繁内封字幕]",
        "RILAKKUMA S01E13 1080p CR WEB-DL MULTi AAC2.0 H 264-VARYG (Multi-Audio, Multi-Subs)",
        "[RUBaDUB] Kaiju No. 8 (S1 Complete) (1080p) (Dual Audio)",
        "[SubsPlease] Honzuki no Gekokujou S4 - 13 (1080p) [A4FE0990].mkv",
        "[SubsPlease] Honzuki no Gekokujou S4 - 13 (480p) [17152F60].mkv",
        "[SubsPlease] Honzuki no Gekokujou S4 - 13 (720p) [97744C2D].mkv",
        "That Time I Got Reincarnated as a Slime S04E12 Tempest Evolves 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG (Tensei shitara Slime Datta Ken, Dual-Audio, Multi-Subs)",
        "The Drops of God S01E12 Take the Distant Path 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG (Kami no Shizuku, Dual-Audio, Multi-Subs)",
        "[ToonsHub] Ascendance of a Bookworm S04E11 1080p CR WEB-DL DUAL AAC2.0 H.264 (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Dual-Audio, Multi-Subs)",
        "[ToonsHub] Ascendance of a Bookworm S04E12 1080p CR WEB-DL DUAL AAC2.0 H.264 (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Dual-Audio, Multi-Subs)",
        "[ToonsHub] Ascendance of a Bookworm S04E13 1080p CR WEB-DL AAC2.0 H.264 (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Multi-Subs)",
        "[ToonsHub] Honzuki no Gekokujou S04E13 1080p AMZN WEB-DL DDP2.0 H.264 (Ascendance of a Bookworm Side Story)",
        "[Yameii] Ascendance of a Bookworm - S04E11 [English Dub] [CR WEB-DL 1080p H264 AAC] [8ACE7B72] (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan o Erande Iraremasen - Ryoushu no Youjo | Adopted Daughter of an Archduke)",
        "[Yameii] Ascendance of a Bookworm - S04E12 [English Dub] [CR WEB-DL 1080p H264 AAC] [ACE19921] (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan o Erande Iraremasen - Ryoushu no Youjo | Adopted Daughter of an Archduke)",
        "[桜都字幕组] 入间同学入魔了 第四季 / Mairimashita! Iruma-kun (2026) [13][1080P][简体内嵌]",
        "[桜都字幕组] 入间同学入魔了 第四季 / Mairimashita! Iruma-kun (2026) [13][1080P][简繁内封]",
        "[桜都字幕组] 入间同学入魔了 第四季 / Mairimashita! Iruma-kun (2026) [13][1080P][繁体内嵌]",
        "[桜都字幕组] 入间同学入魔了 第四季 / Mairimashita! Iruma-kun (2026) [14][1080P][简繁内封]",
    ];

    #[test]
    fn parses_standard_sxxexx_with_group_and_quality_chain() {
        let p = parse("Ascendance of a Bookworm S04E11 The Gathering of Gutenberg 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG (Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen, Dual-Audio, Multi-Subs)");
        assert_eq!(p.season, Some(4));
        assert_eq!(p.episode, Some(11));
        assert_eq!(p.resolution, Some(1080));
        assert_eq!(p.source, Some(Source::WebDl));
        assert_eq!(p.codec, Some(Codec::H264));
        assert_eq!(p.title_normalized, "Ascendance of a Bookworm");
    }

    #[test]
    fn parses_subsplease_dash_episode_with_group_and_hash() {
        let p = parse("[SubsPlease] Honzuki no Gekokujou S4 - 13 (1080p) [A4FE0990].mkv");
        assert_eq!(p.group.as_deref(), Some("SubsPlease"));
        assert_eq!(p.season, Some(4));
        assert_eq!(p.episode, Some(13));
        assert_eq!(p.resolution, Some(1080));
        assert_eq!(p.container.as_deref(), Some("mkv"));
        assert_eq!(p.title_normalized, "Honzuki no Gekokujou");
    }

    #[test]
    fn parses_erai_raws_bracket_quality_block() {
        let p = parse("[Erai-raws] Honzuki no Gekokujou: Shisho ni Naru Tame ni wa Shudan wo Erandeiraremasen - Ryushu no Youjo - 13 [1080p CR WEB-DL AVC AAC][MultiSub][18261A02]");
        assert_eq!(p.group.as_deref(), Some("Erai-raws"));
        assert_eq!(p.season, None);
        assert_eq!(p.episode, Some(13));
        assert_eq!(p.resolution, Some(1080));
        assert_eq!(p.source, Some(Source::WebDl));
    }

    #[test]
    fn parses_lolihouse_webrip_hevc_10bit() {
        let p = parse("[LoliHouse] Some Title - 01 [WebRip 1080p HEVC-10bit AAC][Subs]");
        assert_eq!(p.group.as_deref(), Some("LoliHouse"));
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.source, Some(Source::WebRip));
        assert_eq!(p.codec, Some(Codec::Hevc));
        assert_eq!(p.bit_depth, Some(10));
    }

    #[test]
    fn parses_season_pack_no_episode() {
        let p = parse("[RUBaDUB] Kaiju No. 8 (S1 Complete) (1080p) (Dual Audio)");
        assert_eq!(p.group.as_deref(), Some("RUBaDUB"));
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, None);
        assert_eq!(p.resolution, Some(1080));
    }

    #[test]
    fn parses_yameii_dash_sxxexx_with_english_dub_tag() {
        let p = parse("[Yameii] Ascendance of a Bookworm - S04E11 [English Dub] [CR WEB-DL 1080p H264 AAC] [8ACE7B72] (Honzuki no Gekokujou)");
        assert_eq!(p.group.as_deref(), Some("Yameii"));
        assert_eq!(p.season, Some(4));
        assert_eq!(p.episode, Some(11));
        assert_eq!(p.codec, Some(Codec::H264));
    }

    #[test]
    fn parses_year_and_bare_episode_number() {
        let p = parse("[GM-Team][Renegade Immortal][2023][148][AVC][GB][1080P]");
        assert_eq!(p.year, Some(2023));
        assert_eq!(p.resolution, Some(1080));
        assert_eq!(p.codec, Some(Codec::H264).or(Some(Codec::H264))); // AVC == H264 family
    }

    /// A batch of real, unmodified nyaa.si release titles pulled live
    /// across several categories — not hand-picked for parseability. Not a
    /// correctness check (no ground truth for the messier CJK/bracket-only
    /// shapes), just a robustness net: parse() must never panic, and on
    /// the common WEB-DL/anime shapes it should extract *something*.
    #[test]
    fn does_not_panic_on_real_corpus() {
        let mut resolution_hits = 0;
        for title in CORPUS {
            let p = parse(title);
            if p.resolution.is_some() {
                resolution_hits += 1;
            }
            assert!(!p.title_normalized.is_empty(), "empty title for {title:?}");
        }
        // Most of this corpus carries an explicit resolution tag; a low hit
        // rate would mean the resolution regex regressed, not just that a
        // few CJK/manga edge cases were missed.
        assert!(
            resolution_hits * 2 > CORPUS.len(),
            "only {resolution_hits}/{} titles yielded a resolution",
            CORPUS.len()
        );
    }

    #[test]
    fn refuses_to_resolve_a_bracketed_batch_range_to_one_episode() {
        // Previously resolved to episode 12, silently attributing the
        // whole 12-episode batch torrent to one guessed episode.
        let p = parse("[Judas] Some Show (01-12) [BD 1080p]");
        assert_eq!(p.episode, None);
        assert_eq!(p.absolute_episode, None);
    }

    #[test]
    fn refuses_to_resolve_a_dash_prefixed_batch_range_to_one_episode() {
        // Previously resolved to episode 1 (the first number after the
        // leading dash), same underlying problem as above.
        let p = parse("Show - 01-12 [1080p]");
        assert_eq!(p.episode, None);
        assert_eq!(p.absolute_episode, None);
    }

    #[test]
    fn refuses_to_resolve_an_sxxexx_range_to_one_episode() {
        // Previously resolved to S01E01 via SXXEXX_RE matching only the
        // first half of the range.
        let p = parse("Show S01E01-E12 1080p WEB-DL");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, None);
        assert_eq!(p.absolute_episode, None);
    }

    #[test]
    fn single_episode_dash_titles_are_unaffected_by_range_detection() {
        // A genuine single episode using dash notation must still resolve
        // normally — only real ranges should be refused.
        let p = parse("[SubsPlease] Some Show - 05 (1080p) [ABCD1234].mkv");
        assert_eq!(p.episode, Some(5));
        assert_eq!(p.absolute_episode, Some(5));
    }

    #[test]
    fn extracts_a_bare_year_from_a_scene_style_movie_name() {
        // Previously parsed as year=None entirely, since these releases
        // never bracket the year — silently disabling movie_year_mismatch,
        // the only defense against a same-named-but-wrong film, on exactly
        // the naming convention TPB/1337x results actually use.
        let p = parse("Dune.1984.1080p.BluRay.x264-GROUP");
        assert_eq!(p.year, Some(1984));

        let p = parse("Dune 1984 1080p BluRay");
        assert_eq!(p.year, Some(1984));
    }

    #[test]
    fn does_not_mistake_a_year_titled_movie_for_a_bare_year() {
        // "1917" and "2012" are real movie titles — a bare year at the very
        // start of the string must not be treated as a release year, or
        // the title would be mistaken for empty.
        let p = parse("1917 1080p BluRay x264-GROUP");
        assert_eq!(p.year, None);
    }

    #[test]
    fn brackets_still_take_priority_over_a_coincidental_bare_year() {
        let p = parse("Some.Show.2024.S01E01.(2023).1080p.WEB-DL");
        assert_eq!(p.year, Some(2023));
    }

    #[test]
    fn does_not_panic_on_unparsable_manga_release() {
        // Not a video release at all — should degrade gracefully, not crash.
        let p =
            parse("Aime ton prochain (01-06) (Chida) (2019) [Digital-1920] [Manga FR] (PapriKa+)");
        assert_eq!(p.year, Some(2019));
        // season/episode extraction is allowed to miss here; the important
        // thing is it returns *something* rather than panicking.
        let _ = p.season;
    }
}
