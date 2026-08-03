use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct AudioStream {
    pub codec: Option<String>,
    pub language: Option<String>,
    pub is_default: bool,
    pub channels: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleStream {
    pub language: Option<String>,
    /// mov_text (mp4's timed-text subtitle codec) isn't valid inside a
    /// Matroska container — a transcode pipeline that always outputs `.mkv`
    /// needs to know this per-stream to convert rather than blindly stream
    /// copy. See `transcode::subtitle_codec_args`.
    pub codec: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaProbe {
    pub duration_secs: Option<f64>,
    pub container_bitrate: Option<i64>,
    /// ffprobe's own long-form container name (e.g. "Matroska / WebM") —
    /// distinct from the file extension, which can lie or just be absent.
    pub container_format: Option<String>,
    pub video_codec: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub video_bitrate: Option<i64>,
    pub frame_rate: Option<f64>,
    /// The primary video stream's `color_transfer` tag as ffprobe reports
    /// it (e.g. "smpte2084", "arib-std-b67", "bt709") — kept as the raw
    /// string rather than pre-reduced to a bool so a future caller isn't
    /// stuck with just today's idea of what counts as "HDR" (Dolby Vision
    /// profiles, for instance, don't all surface the same way here).
    pub color_transfer: Option<String>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    /// The complete ffprobe JSON response, kept verbatim. Every field
    /// above is a deliberately-narrow, purpose-built read of this same
    /// payload; this is the escape hatch — future functionality that needs
    /// something not already surfaced as its own column (chapters, extra
    /// stream tags, encoder info, etc.) can mine it here without requiring
    /// a re-probe of the whole library.
    pub raw_json: String,
}

impl MediaProbe {
    /// Best-effort HDR classification from `color_transfer` — PQ
    /// (smpte2084, the common HDR10/HDR10+/Dolby-Vision-base-layer
    /// transfer function) or HLG (arib-std-b67). Not exhaustive by design;
    /// see `color_transfer`'s own doc comment for why the raw value is kept
    /// around too.
    pub fn is_hdr(&self) -> bool {
        matches!(
            self.color_transfer.as_deref(),
            Some("smpte2084") | Some("arib-std-b67")
        )
    }
}

fn is_english(lang: Option<&str>) -> bool {
    matches!(lang, Some("eng") | Some("en"))
}

impl MediaProbe {
    pub fn has_english_audio(&self) -> bool {
        self.audio.iter().any(|a| is_english(a.language.as_deref()))
    }

    /// True when the default-disposition audio track (if any) is not
    /// English — mirrors `mkv::default_track_is_english_or_unset`'s "unset
    /// default isn't a problem" behavior, since the codebase's post-download
    /// remux fix already treats those two cases identically.
    pub fn default_audio_is_non_english(&self) -> bool {
        match self.audio.iter().find(|a| a.is_default) {
            Some(a) => !is_english(a.language.as_deref()),
            None => false,
        }
    }
}

/// One `ffprobe` call gets container + every stream's codec/resolution/
/// bitrate/language/default-disposition in a single JSON payload — no need
/// for separate audio/video/subtitle passes.
pub fn probe(path: &Path) -> Result<MediaProbe> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .context("failed to run ffprobe")?;

    if !output.status.success() {
        bail!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let raw_json = String::from_utf8_lossy(&output.stdout).into_owned();
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("ffprobe output was not valid JSON")?;

    Ok(build_media_probe(&json, raw_json))
}

/// The actual JSON-to-`MediaProbe` mapping, split out from `probe` so it can
/// be unit-tested directly against hand-built ffprobe-shaped JSON — real
/// muxers are inconsistent enough about stream ordering/disposition (see
/// `probe_finds_the_real_video_stream_even_when_attached_pic_comes_first`'s
/// doc comment) that constructing every case as an actual file via `ffmpeg`
/// isn't always practical.
fn build_media_probe(json: &serde_json::Value, raw_json: String) -> MediaProbe {
    let format = &json["format"];
    let duration_secs = format["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok());
    let container_bitrate = format["bit_rate"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok());
    let container_format = format["format_long_name"].as_str().map(str::to_string);

    let streams = json["streams"].as_array().cloned().unwrap_or_default();

    let mut probe = MediaProbe {
        duration_secs,
        container_bitrate,
        container_format,
        raw_json,
        ..Default::default()
    };

    for stream in &streams {
        let codec_type = stream["codec_type"].as_str().unwrap_or("");
        let language = stream["tags"]["language"]
            .as_str()
            .or_else(|| stream["tags"]["LANGUAGE"].as_str())
            .map(str::to_string);
        let is_attached_pic = stream["disposition"]["attached_pic"].as_i64() == Some(1);
        match codec_type {
            // First non-attached-pic video stream — a second "video" stream
            // in a real-world file is almost always an embedded cover-art
            // thumbnail, not a second picture track, and ffmpeg does not
            // guarantee it comes *after* the real content stream (some mp4
            // remuxes and mkvmerge outputs put it first). Explicitly
            // checking `disposition.attached_pic` rather than relying on
            // stream order means a cover-art-first file no longer has its
            // actual video codec/height silently replaced by the
            // thumbnail's.
            "video" if probe.video_codec.is_none() && !is_attached_pic => {
                probe.video_codec = stream["codec_name"].as_str().map(str::to_string);
                probe.width = stream["width"].as_i64();
                probe.height = stream["height"].as_i64();
                probe.video_bitrate = stream["bit_rate"]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok());
                probe.frame_rate = stream["avg_frame_rate"]
                    .as_str()
                    .and_then(parse_frame_rate_fraction)
                    .or_else(|| {
                        stream["r_frame_rate"]
                            .as_str()
                            .and_then(parse_frame_rate_fraction)
                    });
                probe.color_transfer = stream["color_transfer"].as_str().map(str::to_string);
            }
            "audio" => {
                probe.audio.push(AudioStream {
                    codec: stream["codec_name"].as_str().map(str::to_string),
                    language,
                    is_default: stream["disposition"]["default"].as_i64() == Some(1),
                    channels: stream["channels"].as_i64(),
                });
            }
            "subtitle" => {
                let codec = stream["codec_name"].as_str().map(str::to_string);
                probe.subtitles.push(SubtitleStream { language, codec });
            }
            _ => {}
        }
    }

    probe
}

/// ffprobe reports frame rate as a "num/den" fraction string (e.g.
/// "24000/1001" for 23.976fps) rather than a plain number. `0/0` means
/// "unknown" (common for e.g. still-image or data streams that don't
/// really have one), not a real rate.
fn parse_frame_rate_fraction(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

/// A cheap header-level probe succeeding only proves the container is
/// parseable — it doesn't catch a truncated file or mid-stream bit-rot,
/// which requires actually decoding every frame. `ffmpeg -xerror` does
/// that: any decode error either exits non-zero or writes to stderr. This
/// is CPU-bound and can take minutes per file (it reads and decodes the
/// whole thing), so it's deliberately a separate, opt-in pass rather than
/// part of every routine scan — see `verify-library` in `main.rs`.
pub enum DecodeCheck {
    Ok,
    Corrupt(String),
}

pub fn verify_decodable(path: &Path) -> Result<DecodeCheck> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .context("failed to run ffmpeg for decode verification")?;

    if output.status.success() && output.stderr.is_empty() {
        Ok(DecodeCheck::Ok)
    } else {
        Ok(DecodeCheck::Corrupt(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// Bounded, sampled variant of `verify_decodable` for callers where a full
/// decode's O(duration) cost is the actual bottleneck — the transcode
/// pipeline measured this in practice: two concurrent full-file decode
/// verifications pinned two CPU cores at 400%+ each for the whole
/// verification pass, dwarfing the GPU encode time itself for long files.
///
/// Decodes only fixed-size windows (`sample_secs` each) near the start,
/// middle, and end of the file, rather than every frame — a deliberate
/// trade of "catches most real corruption cheaply" for "bounded cost
/// regardless of file length", not equivalent thoroughness to a full
/// decode. Truncation specifically doesn't need this: `encode_and_verify`'s
/// separate duration-match check against the original already catches that
/// regardless of what this function samples, since a truncated output's
/// container-reported duration comes up short either way.
///
/// Falls back to a full `verify_decodable` when `duration_secs` is small
/// enough that sampling wouldn't save meaningful time anyway.
pub fn verify_decodable_sampled(
    path: &Path,
    duration_secs: f64,
    sample_secs: f64,
) -> Result<DecodeCheck> {
    if duration_secs <= sample_secs * 3.0 {
        return verify_decodable(path);
    }

    let windows = [
        0.0,
        (duration_secs / 2.0 - sample_secs / 2.0).max(0.0),
        (duration_secs - sample_secs).max(0.0),
    ];

    for start in windows {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-xerror"]);
        if start > 0.0 {
            cmd.args(["-ss", &format!("{start:.2}")]);
        }
        cmd.arg("-i").arg(path);
        cmd.args(["-t", &format!("{sample_secs:.2}"), "-f", "null", "-"]);
        let output = cmd
            .output()
            .context("failed to run ffmpeg for sampled decode verification")?;

        if !(output.status.success() && output.stderr.is_empty()) {
            return Ok(DecodeCheck::Corrupt(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }
    Ok(DecodeCheck::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_english_audio_true_when_any_track_is_english() {
        let probe = MediaProbe {
            audio: vec![
                AudioStream {
                    codec: None,
                    language: Some("jpn".to_string()),
                    is_default: true,
                    channels: None,
                },
                AudioStream {
                    codec: None,
                    language: Some("eng".to_string()),
                    is_default: false,
                    channels: None,
                },
            ],
            ..Default::default()
        };
        assert!(probe.has_english_audio());
    }

    #[test]
    fn has_english_audio_false_with_no_tracks() {
        assert!(!MediaProbe::default().has_english_audio());
    }

    #[test]
    fn default_audio_is_non_english_true_when_default_track_is_not_english() {
        let probe = MediaProbe {
            audio: vec![AudioStream {
                codec: None,
                language: Some("ita".to_string()),
                is_default: true,
                channels: None,
            }],
            ..Default::default()
        };
        assert!(probe.default_audio_is_non_english());
    }

    #[test]
    fn default_audio_is_non_english_false_when_no_default_is_set() {
        // Mirrors mkv::default_track_is_english_or_unset: an unset default
        // isn't treated as a problem.
        let probe = MediaProbe {
            audio: vec![AudioStream {
                codec: None,
                language: Some("ita".to_string()),
                is_default: false,
                channels: None,
            }],
            ..Default::default()
        };
        assert!(!probe.default_audio_is_non_english());
    }

    #[test]
    fn default_audio_is_non_english_false_when_default_is_english() {
        let probe = MediaProbe {
            audio: vec![AudioStream {
                codec: None,
                language: Some("eng".to_string()),
                is_default: true,
                channels: None,
            }],
            ..Default::default()
        };
        assert!(!probe.default_audio_is_non_english());
    }

    #[test]
    fn probe_fails_gracefully_for_a_nonexistent_file() {
        let result = probe(Path::new("/nonexistent/path/to/nothing.mkv"));
        assert!(result.is_err());
    }

    fn generate_clip(dir: &Path, name: &str, duration_secs: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:duration={duration_secs}:rate=5"),
            ])
            .args(["-c:v", "libx264", "-preset", "ultrafast"])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a test clip");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate a test clip: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    #[test]
    fn verify_decodable_sampled_passes_a_genuinely_intact_short_file() {
        // Short enough to hit the "falls back to a full check" path.
        let dir = std::env::temp_dir().join(format!(
            "breadarr-ffprobe-sampled-short-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_clip(&dir, "short.mkv", 2);

        let result = verify_decodable_sampled(&clip, 2.0, 20.0).unwrap();
        assert!(matches!(result, DecodeCheck::Ok));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_decodable_sampled_passes_a_genuinely_intact_long_file() {
        // Long enough (duration > sample_secs * 3) to actually exercise the
        // windowed start/middle/end sampling path, not the short-file
        // fallback.
        let dir = std::env::temp_dir().join(format!(
            "breadarr-ffprobe-sampled-long-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_clip(&dir, "long.mkv", 10);

        let result = verify_decodable_sampled(&clip, 10.0, 2.0).unwrap();
        assert!(matches!(result, DecodeCheck::Ok));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_decodable_sampled_flags_a_file_that_does_not_decode_at_all() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-ffprobe-sampled-corrupt-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.mkv");
        std::fs::write(&path, b"this is not a real video file").unwrap();

        let result = verify_decodable_sampled(&path, 10.0, 2.0).unwrap();
        assert!(matches!(result, DecodeCheck::Corrupt(_)));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Generates a tiny real video via ffmpeg's `lavfi` synthetic source —
    /// validates the actual JSON field extraction (width/height/codec/
    /// duration/audio language+default) against genuine ffprobe output,
    /// not just hand-built `MediaProbe` fixtures.
    fn generate_test_clip(dir: &Path, width: u32, height: u32) -> std::path::PathBuf {
        let path = dir.join("clip.mkv");
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={width}x{height}:duration=1:rate=1"))
            .args(["-f", "lavfi", "-i", "sine=frequency=1000:duration=1"])
            .args(["-metadata:s:a:0", "language=eng"])
            .args(["-c:v", "libx264", "-c:a", "aac"])
            .arg(&path)
            .output()
            .expect("failed to run ffmpeg to generate a test clip");
        assert!(
            status.status.success(),
            "ffmpeg failed to generate test clip: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        path
    }

    #[test]
    fn probe_extracts_real_resolution_and_audio_language_from_a_generated_clip() {
        let dir =
            std::env::temp_dir().join(format!("breadarr-ffprobe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = generate_test_clip(&dir, 640, 360);

        let probe = probe(&clip).unwrap();
        assert_eq!(probe.width, Some(640));
        assert_eq!(probe.height, Some(360));
        assert!(probe.video_codec.is_some());
        assert!(probe.duration_secs.unwrap_or(0.0) > 0.0);
        assert!(probe.has_english_audio());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_extracts_extra_metadata_from_a_generated_clip() {
        let dir = std::env::temp_dir().join(format!(
            "breadarr-ffprobe-extra-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Generated at rate=1 (see generate_test_clip), so frame_rate
        // should come back at (or very near) 1.0.
        let clip = generate_test_clip(&dir, 640, 360);

        let probe = probe(&clip).unwrap();
        assert!(
            probe.frame_rate.is_some_and(|r| (r - 1.0).abs() < 0.1),
            "frame_rate was {:?}",
            probe.frame_rate
        );
        assert!(
            probe
                .container_format
                .as_deref()
                .is_some_and(|f| !f.is_empty()),
            "container_format should be populated"
        );
        assert!(
            probe.audio.first().is_some_and(|a| a.channels.is_some()),
            "audio channel count should be populated"
        );
        // A real generated clip's default color_transfer is unlikely to be
        // an HDR transfer function — this is really asserting `is_hdr()`
        // doesn't spuriously fire on ordinary SDR content.
        assert!(!probe.is_hdr());
        // The raw payload is kept verbatim and should parse as JSON on its
        // own — a caller mining it later needs that to actually be true.
        assert!(!probe.raw_json.is_empty());
        assert!(serde_json::from_str::<serde_json::Value>(&probe.raw_json).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Regression test for a real gap found in review: `probe`'s "first video
    // stream wins" selection used to have no idea about
    // `disposition.attached_pic` and just trusted stream order — a
    // convention real muxers don't reliably follow (ffmpeg's own mp4 muxer
    // was observed reordering an attached-pic stream to the *end*
    // regardless of requested `-map` order, which makes constructing a
    // genuine cover-art-*first* file via `ffmpeg` impractical — so this
    // exercises `build_media_probe` directly against hand-built,
    // real-shaped ffprobe JSON instead of a generated file, deterministically
    // covering the ordering `ffmpeg`'s own tooling won't produce). Unlike
    // `generate_clip_with_attached_pic` in `transcode::mod::tests` (cover
    // art second, the already-handled case), this puts it *first* to prove
    // the fix is order-independent, not just "skip the second video
    // stream".
    #[test]
    fn probe_finds_the_real_video_stream_even_when_attached_pic_comes_first() {
        let json = serde_json::json!({
            "format": { "duration": "10.0", "bit_rate": "5000000", "format_long_name": "Matroska / WebM" },
            "streams": [
                {
                    "index": 0,
                    "codec_type": "video",
                    "codec_name": "png",
                    "width": 64,
                    "height": 64,
                    "disposition": { "attached_pic": 1 }
                },
                {
                    "index": 1,
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 640,
                    "height": 360,
                    "disposition": { "attached_pic": 0 }
                }
            ]
        });

        let probe = build_media_probe(&json, "{}".to_string());
        assert_eq!(probe.width, Some(640), "must pick the real content stream's width, not the 64x64 cover art's");
        assert_eq!(probe.height, Some(360), "must pick the real content stream's height, not the 64x64 cover art's");
        assert_eq!(probe.video_codec.as_deref(), Some("h264"), "must pick the real content stream's codec, not the cover art's png");
    }

    // Companion case: attached-pic *second* (the ordering the code
    // previously assumed was the only one) must still work exactly as
    // before — this fix is additive, not a behavior change for the
    // already-handled ordering.
    #[test]
    fn probe_finds_the_real_video_stream_when_attached_pic_comes_second() {
        let json = serde_json::json!({
            "format": { "duration": "10.0", "bit_rate": "5000000", "format_long_name": "Matroska / WebM" },
            "streams": [
                {
                    "index": 0,
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 640,
                    "height": 360,
                    "disposition": { "attached_pic": 0 }
                },
                {
                    "index": 1,
                    "codec_type": "video",
                    "codec_name": "png",
                    "width": 64,
                    "height": 64,
                    "disposition": { "attached_pic": 1 }
                }
            ]
        });

        let probe = build_media_probe(&json, "{}".to_string());
        assert_eq!(probe.width, Some(640));
        assert_eq!(probe.height, Some(360));
        assert_eq!(probe.video_codec.as_deref(), Some("h264"));
    }
}
