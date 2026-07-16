use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrack {
    pub id: u32,
    pub language: Option<String>,
    pub is_default: bool,
}

/// Runs `mkvmerge -J` (JSON track info) — mkvtoolnix's own structured-output
/// mode, so track inspection doesn't depend on parsing human-readable text.
pub fn inspect_audio_tracks(path: &Path) -> Result<Vec<AudioTrack>> {
    let output = Command::new("mkvmerge")
        .arg("-J")
        .arg(path)
        .output()
        .context("failed to run mkvmerge -J")?;

    if !output.status.success() {
        bail!(
            "mkvmerge -J failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("mkvmerge -J output was not valid JSON")?;

    let tracks = json["tracks"]
        .as_array()
        .context("mkvmerge -J output had no tracks array")?;

    Ok(tracks
        .iter()
        .filter(|t| t["type"] == "audio")
        .map(|t| AudioTrack {
            id: t["id"].as_u64().unwrap_or(0) as u32,
            language: t["properties"]["language"].as_str().map(str::to_string),
            is_default: t["properties"]["default_track"].as_bool().unwrap_or(false),
        })
        .collect())
}

fn is_english(lang: Option<&str>) -> bool {
    matches!(lang, Some("eng") | Some("en"))
}

pub fn has_english_track(tracks: &[AudioTrack]) -> bool {
    tracks.iter().any(|t| is_english(t.language.as_deref()))
}

/// True if there's no fix needed: either the default track is already
/// English, or there's no explicit default at all (mkvmerge/players
/// typically fall back to the first audio track, and single-track files
/// have nothing to reorder).
pub fn default_track_is_english_or_unset(tracks: &[AudioTrack]) -> bool {
    match tracks.iter().find(|t| t.is_default) {
        Some(t) => is_english(t.language.as_deref()),
        None => true,
    }
}

/// Re-flags the English track as default (and all others as non-default)
/// without re-encoding — fixes the common "Italian track 1, English track
/// 2" pattern. Returns an error if there's no English track to promote.
pub fn remux_english_default(input: &Path, output: &Path, tracks: &[AudioTrack]) -> Result<()> {
    let english_id = tracks
        .iter()
        .find(|t| is_english(t.language.as_deref()))
        .map(|t| t.id)
        .context("no English audio track to promote")?;

    let mut cmd = Command::new("mkvmerge");
    cmd.arg("-o").arg(output);
    for track in tracks {
        let flag = if track.id == english_id { "yes" } else { "no" };
        cmd.arg("--default-track-flag")
            .arg(format!("{}:{flag}", track.id));
    }
    cmd.arg(input);

    let output_result = cmd.output().context("failed to run mkvmerge remux")?;
    if !output_result.status.success() {
        bail!(
            "mkvmerge remux failed: {}",
            String::from_utf8_lossy(&output_result.stderr)
        );
    }
    Ok(())
}
