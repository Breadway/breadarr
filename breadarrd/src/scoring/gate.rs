use crate::parser::ParsedRelease;

use super::profile::QualityProfile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    MissingSeederData,
    BelowMinSeeders,
    SizeOutOfRange,
    GroupDenylisted,
    NoEnglishAudio,
    /// Below 1080p while a 1080p-or-better candidate exists for the same
    /// target in this same search batch — rejected in favor of the better
    /// one, not because low resolution is inherently disqualifying (a
    /// release with no 1080p+ option anywhere is still accepted; see
    /// `GateContext::better_resolution_available`).
    LowResolutionAlternativeExists,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    Accept,
    Reject(RejectReason),
}

pub struct GateContext {
    pub seeders: Option<u32>,
    pub size_bytes: Option<u64>,
    /// Episode/movie runtime, when known — used to sanity-check size against
    /// implied bitrate. Frequently unavailable (neither TVDB's nor TMDB's
    /// episode-level data reliably carries this), so size checks fall back
    /// to absolute per-resolution bounds when absent.
    pub runtime_minutes: Option<u32>,
    pub has_english_audio: bool,
    /// Anime gets a carve-out on the English-audio gate: falls back to
    /// Japanese-only audio when no English release exists at all.
    pub is_anime: bool,
    /// True when at least one other candidate in the same search batch
    /// parsed to 1080p or better — the signal that makes a sub-1080p
    /// candidate here a real downgrade rather than the only option. Always
    /// `false` on the RSS feed-watch path (no batch of alternatives to
    /// compare against there), so this gate never fires for that path.
    pub better_resolution_available: bool,
    /// A season/batch release covering many episodes in one torrent — its
    /// total size is naturally a multiple of a single episode's, so the
    /// per-episode size/bitrate sanity check would reject it as absurdly
    /// oversized every time. There's no reliable episode count to scale
    /// the bounds by before the torrent is actually inspected, so the size
    /// gate is skipped entirely rather than guessed at.
    pub is_season_pack: bool,
}

pub fn evaluate(parsed: &ParsedRelease, ctx: &GateContext, profile: &QualityProfile) -> GateResult {
    let Some(seeders) = ctx.seeders else {
        return GateResult::Reject(RejectReason::MissingSeederData);
    };
    if seeders < profile.min_seeders {
        return GateResult::Reject(RejectReason::BelowMinSeeders);
    }

    if !ctx.is_season_pack {
        if let (Some(size), Some(resolution)) = (ctx.size_bytes, parsed.resolution) {
            if !size_is_sane(size, resolution, ctx.runtime_minutes) {
                return GateResult::Reject(RejectReason::SizeOutOfRange);
            }
        }
    }

    if ctx.better_resolution_available && parsed.resolution.is_some_and(|r| r < 1080) {
        return GateResult::Reject(RejectReason::LowResolutionAlternativeExists);
    }

    if let Some(group) = &parsed.group {
        if profile
            .group_denylist
            .iter()
            .any(|g| g.eq_ignore_ascii_case(group))
        {
            return GateResult::Reject(RejectReason::GroupDenylisted);
        }
    }

    if !ctx.has_english_audio && !ctx.is_anime {
        return GateResult::Reject(RejectReason::NoEnglishAudio);
    }

    GateResult::Accept
}

fn size_is_sane(size_bytes: u64, resolution: u32, runtime_minutes: Option<u32>) -> bool {
    let Some(runtime_minutes) = runtime_minutes.filter(|&r| r > 0) else {
        return absolute_size_bounds(resolution).contains(&size_bytes);
    };

    let seconds = (runtime_minutes as u64) * 60;
    let bits_per_second = (size_bytes * 8) / seconds;
    let (min_bps, max_bps) = bitrate_bounds(resolution);
    (min_bps..=max_bps).contains(&bits_per_second)
}

fn bitrate_bounds(resolution: u32) -> (u64, u64) {
    match resolution {
        r if r >= 2160 => (2_000_000, 80_000_000),
        r if r >= 1080 => (800_000, 40_000_000),
        r if r >= 720 => (400_000, 20_000_000),
        _ => (150_000, 10_000_000),
    }
}

fn absolute_size_bounds(resolution: u32) -> std::ops::RangeInclusive<u64> {
    match resolution {
        r if r >= 2160 => 200_000_000..=40_000_000_000,
        r if r >= 1080 => 50_000_000..=15_000_000_000,
        r if r >= 720 => 20_000_000..=8_000_000_000,
        _ => 5_000_000..=4_000_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn ctx() -> GateContext {
        GateContext {
            seeders: Some(50),
            size_bytes: Some(400_000_000),
            runtime_minutes: Some(24),
            has_english_audio: true,
            is_anime: false,
            better_resolution_available: false,
            is_season_pack: false,
        }
    }

    #[test]
    fn accepts_a_healthy_english_release() {
        let parsed = parser::parse("Some Show S01E01 1080p WEB-DL H264");
        assert_eq!(
            evaluate(&parsed, &ctx(), &QualityProfile::default_tv()),
            GateResult::Accept
        );
    }

    #[test]
    fn rejects_missing_seeder_data() {
        let parsed = parser::parse("Some Show S01E01 1080p WEB-DL H264");
        let mut c = ctx();
        c.seeders = None;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Reject(RejectReason::MissingSeederData)
        );
    }

    #[test]
    fn rejects_below_min_seeders() {
        let parsed = parser::parse("Some Show S01E01 1080p WEB-DL H264");
        let mut c = ctx();
        c.seeders = Some(1);
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Reject(RejectReason::BelowMinSeeders)
        );
    }

    #[test]
    fn rejects_absurdly_small_file_for_claimed_resolution() {
        let parsed = parser::parse("Some Movie 2024 2160p BluRay");
        let mut c = ctx();
        c.size_bytes = Some(1_000_000); // 1MB claiming to be a 4K release
        c.runtime_minutes = None;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_movie()),
            GateResult::Reject(RejectReason::SizeOutOfRange)
        );
    }

    #[test]
    fn rejects_denylisted_group() {
        let parsed = parser::parse("[BadGroup] Some Show - 01 [1080p]");
        let mut c = ctx();
        c.is_anime = true;
        let mut profile = QualityProfile::default_tv();
        profile.group_denylist.push("BadGroup".to_string());
        assert_eq!(
            evaluate(&parsed, &c, &profile),
            GateResult::Reject(RejectReason::GroupDenylisted)
        );
    }

    #[test]
    fn rejects_non_english_audio_for_non_anime() {
        let parsed = parser::parse("Some Show S01E01 1080p WEB-DL H264");
        let mut c = ctx();
        c.has_english_audio = false;
        c.is_anime = false;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Reject(RejectReason::NoEnglishAudio)
        );
    }

    #[test]
    fn anime_without_english_audio_is_allowed_through() {
        let parsed = parser::parse("[SubsPlease] Some Anime - 01 (1080p)");
        let mut c = ctx();
        c.has_english_audio = false;
        c.is_anime = true;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Accept
        );
    }

    #[test]
    fn rejects_below_1080p_when_a_better_alternative_exists() {
        let parsed = parser::parse("Some Show S01E01 720p WEB-DL H264");
        let mut c = ctx();
        c.better_resolution_available = true;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Reject(RejectReason::LowResolutionAlternativeExists)
        );
    }

    #[test]
    fn accepts_below_1080p_when_it_is_the_only_option() {
        let parsed = parser::parse("Some Show S01E01 720p WEB-DL H264");
        let mut c = ctx();
        c.better_resolution_available = false;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Accept
        );
    }

    #[test]
    fn season_pack_bypasses_the_size_sanity_check() {
        let parsed = parser::parse("Some Show S01 1080p WEB-DL H264");
        let mut c = ctx();
        c.is_season_pack = true;
        // A whole season's worth of episodes at once — would fail
        // size_is_sane badly if evaluated as if it were one episode.
        c.size_bytes = Some(400_000_000 * 12);
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Accept
        );
    }

    #[test]
    fn accepts_1080p_regardless_of_better_resolution_available() {
        let parsed = parser::parse("Some Show S01E01 1080p WEB-DL H264");
        let mut c = ctx();
        c.better_resolution_available = true;
        assert_eq!(
            evaluate(&parsed, &c, &QualityProfile::default_tv()),
            GateResult::Accept
        );
    }
}
