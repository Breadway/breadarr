use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKind {
    Movie,
    Tv,
}

#[derive(Debug, Clone)]
pub struct Weights {
    pub seeder: f32,
    /// Weighted heavily on purpose: resolution is the single most visible
    /// quality axis, and without it in the score, same-episode releases at
    /// 480p/720p/1080p (routine; SubsPlease and friends publish all three
    /// of an episode within the same feed window) scored identically apart
    /// from source/codec, letting a lower-resolution duplicate arrive later
    /// and outscore (and at import time, overwrite) an already-grabbed
    /// better one on nothing but a seeder-count edge.
    pub resolution_tier: f32,
    pub source_tier: f32,
    pub codec_tier: f32,
    pub bit_depth: f32,
    pub container: f32,
    pub group_allowlist: f32,
    pub repack: f32,
    /// HDR/Dolby Vision bonus. Always 0 on TV/anime profiles: only movie
    /// profiles score this axis at all, per the product decision that HDR
    /// only matters for movies.
    pub hdr: f32,
}

#[derive(Debug, Clone)]
pub struct QualityProfile {
    pub kind: ProfileKind,
    pub min_seeders: u32,
    pub group_denylist: Vec<String>,
    pub group_allowlist: Vec<String>,
    pub weights: Weights,
}

impl QualityProfile {
    pub fn default_tv() -> Self {
        Self {
            kind: ProfileKind::Tv,
            min_seeders: 3,
            group_denylist: Vec::new(),
            group_allowlist: Vec::new(),
            weights: Weights {
                seeder: 1.0,
                resolution_tier: 3.0,
                source_tier: 3.0,
                codec_tier: 2.0,
                bit_depth: 0.5,
                container: 0.25,
                group_allowlist: 1.0,
                repack: 1.5,
                hdr: 0.0,
            },
        }
    }

    pub fn default_movie() -> Self {
        Self {
            kind: ProfileKind::Movie,
            min_seeders: 3,
            group_denylist: Vec::new(),
            group_allowlist: Vec::new(),
            weights: Weights {
                seeder: 1.0,
                resolution_tier: 3.0,
                source_tier: 3.0,
                codec_tier: 2.0,
                bit_depth: 0.5,
                container: 0.25,
                group_allowlist: 1.0,
                repack: 1.5,
                hdr: 2.0,
            },
        }
    }

    /// Builds the default profile for `kind`, then applies a user-supplied
    /// JSON weights override on top of it: the `quality_profile.weights`
    /// column's actual content, finally read instead of ignored. Every
    /// field is individually optional, so e.g. `{"hdr": 5.0}` overrides
    /// only the HDR bonus and leaves every other axis at its built-in
    /// default, rather than requiring a full weights object to change one
    /// number. Empty (`""`/`"{}"`), missing, or malformed JSON all
    /// harmlessly fall back to exactly the hardcoded defaults: a bad
    /// config value must never break scoring or gating.
    pub fn with_weights_override(kind: ProfileKind, weights_json: &str) -> Self {
        let mut profile = match kind {
            ProfileKind::Movie => Self::default_movie(),
            ProfileKind::Tv => Self::default_tv(),
        };
        if weights_json.trim().is_empty() || weights_json.trim() == "{}" {
            return profile;
        }
        match serde_json::from_str::<WeightsOverride>(weights_json) {
            Ok(o) => profile.weights = profile.weights.with_override(&o),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    weights_json,
                    "quality_profile.weights is not valid JSON; using built-in defaults"
                );
            }
        }
        profile
    }
}

/// Every field optional so a partial JSON object only overrides the axes it
/// actually names: matches `serde_json::from_str`'s normal "missing field
/// stays at its `Default`" behavior for a struct made entirely of
/// `Option<f32>` fields.
#[derive(Debug, Clone, Default, Deserialize)]
struct WeightsOverride {
    seeder: Option<f32>,
    resolution_tier: Option<f32>,
    source_tier: Option<f32>,
    codec_tier: Option<f32>,
    bit_depth: Option<f32>,
    container: Option<f32>,
    group_allowlist: Option<f32>,
    repack: Option<f32>,
    hdr: Option<f32>,
}

impl Weights {
    fn with_override(mut self, o: &WeightsOverride) -> Self {
        if let Some(v) = o.seeder {
            self.seeder = v;
        }
        if let Some(v) = o.resolution_tier {
            self.resolution_tier = v;
        }
        if let Some(v) = o.source_tier {
            self.source_tier = v;
        }
        if let Some(v) = o.codec_tier {
            self.codec_tier = v;
        }
        if let Some(v) = o.bit_depth {
            self.bit_depth = v;
        }
        if let Some(v) = o.container {
            self.container = v;
        }
        if let Some(v) = o.group_allowlist {
            self.group_allowlist = v;
        }
        if let Some(v) = o.repack {
            self.repack = v;
        }
        if let Some(v) = o.hdr {
            self.hdr = v;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_absent_json_falls_back_to_exact_defaults() {
        for json in ["", "{}", "  ", " {} "] {
            let profile = QualityProfile::with_weights_override(ProfileKind::Tv, json);
            assert_eq!(profile.weights.resolution_tier, 3.0);
            assert_eq!(profile.weights.hdr, 0.0);
        }
    }

    #[test]
    fn partial_override_changes_only_the_named_axis() {
        let profile = QualityProfile::with_weights_override(ProfileKind::Movie, r#"{"hdr": 5.0}"#);
        assert_eq!(profile.weights.hdr, 5.0);
        // Every other axis stays at the movie default, untouched.
        assert_eq!(profile.weights.resolution_tier, 3.0);
        assert_eq!(profile.weights.codec_tier, 2.0);
        assert_eq!(profile.weights.seeder, 1.0);
    }

    #[test]
    fn full_override_replaces_every_axis() {
        let json = r#"{"seeder": 9.0, "resolution_tier": 1.0, "source_tier": 1.0,
            "codec_tier": 9.0, "bit_depth": 9.0, "container": 9.0,
            "group_allowlist": 9.0, "repack": 9.0, "hdr": 9.0}"#;
        let profile = QualityProfile::with_weights_override(ProfileKind::Tv, json);
        assert_eq!(profile.weights.seeder, 9.0);
        assert_eq!(profile.weights.resolution_tier, 1.0);
        assert_eq!(profile.weights.codec_tier, 9.0);
        assert_eq!(profile.weights.hdr, 9.0);
    }

    #[test]
    fn malformed_json_falls_back_to_defaults_instead_of_panicking() {
        let profile = QualityProfile::with_weights_override(ProfileKind::Tv, "not json at all");
        assert_eq!(profile.weights.resolution_tier, 3.0);
    }

    #[test]
    fn movie_and_tv_still_get_different_hdr_defaults_with_no_override() {
        let movie = QualityProfile::with_weights_override(ProfileKind::Movie, "{}");
        let tv = QualityProfile::with_weights_override(ProfileKind::Tv, "{}");
        assert_eq!(movie.weights.hdr, 2.0);
        assert_eq!(tv.weights.hdr, 0.0);
    }
}
