use crate::parser::ParsedRelease;

use super::profile::{ProfileKind, QualityProfile};

pub fn score(parsed: &ParsedRelease, seeders: u32, has_hdr: bool, profile: &QualityProfile) -> f32 {
    let w = &profile.weights;
    let mut total = 0.0;

    total += w.seeder * seeder_score(seeders);
    total += w.resolution_tier * resolution_tier(parsed.resolution);
    total += w.source_tier * parsed.source.map(|s| s as u8 as f32).unwrap_or(0.0);
    total += w.codec_tier * parsed.codec.map(|c| c as u8 as f32).unwrap_or(0.0);

    if parsed.bit_depth == Some(10) {
        total += w.bit_depth;
    }
    if parsed.container.as_deref() == Some("mkv") {
        total += w.container;
    }
    if let Some(group) = &parsed.group {
        if profile
            .group_allowlist
            .iter()
            .any(|g| g.eq_ignore_ascii_case(group))
        {
            total += w.group_allowlist;
        }
    }
    if parsed.is_repack {
        total += w.repack;
    }
    if has_hdr && profile.kind == ProfileKind::Movie {
        total += w.hdr;
    }

    total
}

/// Log-scaled so 5 vs 50 seeders matters far more than 400 vs 4000.
fn seeder_score(seeders: u32) -> f32 {
    ((seeders as f32) + 1.0).ln()
}

/// An unparsed resolution scores the same as the bottom tier — no signal
/// either way, not a reason to reject, but not a reason to prefer it either.
fn resolution_tier(resolution: Option<u32>) -> f32 {
    match resolution {
        Some(r) if r >= 2160 => 3.0,
        Some(r) if r >= 1080 => 2.0,
        Some(r) if r >= 720 => 1.0,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn av1_outscores_hevc_outscores_h264_all_else_equal() {
        let profile = QualityProfile::default_tv();
        let av1 = parser::parse("Show S01E01 1080p WEB-DL AV1");
        let hevc = parser::parse("Show S01E01 1080p WEB-DL HEVC");
        let h264 = parser::parse("Show S01E01 1080p WEB-DL H264");

        let s_av1 = score(&av1, 50, false, &profile);
        let s_hevc = score(&hevc, 50, false, &profile);
        let s_h264 = score(&h264, 50, false, &profile);

        assert!(s_av1 > s_hevc);
        assert!(s_hevc > s_h264);
    }

    #[test]
    fn remux_outscores_webrip_all_else_equal() {
        let profile = QualityProfile::default_movie();
        let remux = parser::parse("Movie 2024 2160p Remux H264");
        let webrip = parser::parse("Movie 2024 2160p WEBRip H264");
        assert!(score(&remux, 50, false, &profile) > score(&webrip, 50, false, &profile));
    }

    #[test]
    fn more_seeders_scores_higher_but_with_diminishing_returns() {
        let profile = QualityProfile::default_tv();
        let parsed = parser::parse("Show S01E01 1080p WEB-DL H264");

        let low = score(&parsed, 5, false, &profile);
        let mid = score(&parsed, 50, false, &profile);
        let high = score(&parsed, 4000, false, &profile);

        assert!(mid > low);
        assert!(high > mid);
        // Diminishing returns on a log scale: the *same absolute* jump of
        // +45 seeders matters far more starting from 5 than starting from
        // 4000 (equal-ratio jumps like 5->50 vs 400->4000 are a different,
        // roughly-equal-delta comparison — not what's being tested here).
        let low_plus_45 = score(&parsed, 50, false, &profile) - low;
        let high_plus_45 = score(&parsed, 4045, false, &profile) - high;
        assert!(low_plus_45 > high_plus_45);
    }

    #[test]
    fn hdr_bonus_applies_to_movies_not_tv() {
        let parsed = parser::parse("Title 2024 2160p BluRay H264");
        let movie_profile = QualityProfile::default_movie();
        let tv_profile = QualityProfile::default_tv();

        let movie_with_hdr = score(&parsed, 50, true, &movie_profile);
        let movie_without_hdr = score(&parsed, 50, false, &movie_profile);
        assert!(movie_with_hdr > movie_without_hdr);

        let tv_with_hdr = score(&parsed, 50, true, &tv_profile);
        let tv_without_hdr = score(&parsed, 50, false, &tv_profile);
        assert_eq!(
            tv_with_hdr, tv_without_hdr,
            "HDR must not affect TV/anime scoring"
        );
    }

    #[test]
    fn ten_bit_and_mkv_and_repack_each_add_a_bonus() {
        let profile = QualityProfile::default_tv();
        let base = parser::parse("Show S01E01 1080p WEB-DL H264");
        let ten_bit = parser::parse("Show S01E01 1080p WEB-DL H264 10bit");
        let mkv = parser::parse("Show S01E01 1080p WEB-DL H264.mkv");
        let repack = parser::parse("Show S01E01 1080p WEB-DL H264 REPACK");

        let base_score = score(&base, 50, false, &profile);
        assert!(score(&ten_bit, 50, false, &profile) > base_score);
        assert!(score(&mkv, 50, false, &profile) > base_score);
        assert!(score(&repack, 50, false, &profile) > base_score);
    }

    #[test]
    fn higher_resolution_outscores_lower_all_else_equal() {
        // The real scenario this guards: SubsPlease et al. publish 480p,
        // 720p, and 1080p of the same episode within the same feed
        // window — without a resolution term, these scored identically
        // apart from seeders, letting a later lower-res duplicate outscore
        // (and overwrite at import) an already-grabbed 1080p file.
        let profile = QualityProfile::default_tv();
        let p1080 = parser::parse("[SubsPlease] Show - 01 (1080p) [ABCD1234].mkv");
        let p720 = parser::parse("[SubsPlease] Show - 01 (720p) [ABCD1234].mkv");
        let p480 = parser::parse("[SubsPlease] Show - 01 (480p) [ABCD1234].mkv");

        let s1080 = score(&p1080, 50, false, &profile);
        let s720 = score(&p720, 50, false, &profile);
        let s480 = score(&p480, 50, false, &profile);

        assert!(s1080 > s720);
        assert!(s720 > s480);
    }

    #[test]
    fn allowlisted_group_scores_higher_than_unlisted() {
        let mut profile = QualityProfile::default_tv();
        profile.group_allowlist.push("SubsPlease".to_string());

        let allowlisted = parser::parse("[SubsPlease] Show - 01 [1080p]");
        let unlisted = parser::parse("[RandomGroup] Show - 01 [1080p]");

        assert!(score(&allowlisted, 50, false, &profile) > score(&unlisted, 50, false, &profile));
    }
}
