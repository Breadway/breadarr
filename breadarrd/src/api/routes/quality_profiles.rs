use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::{QualityProfileSummary, UpdateQualityProfileWeightsRequest, WeightsDto};

use crate::api::AppState;
use crate::scoring::{ProfileKind, QualityProfile};

fn to_dto(weights: &crate::scoring::profile::Weights) -> WeightsDto {
    WeightsDto {
        seeder: weights.seeder,
        resolution_tier: weights.resolution_tier,
        source_tier: weights.source_tier,
        codec_tier: weights.codec_tier,
        bit_depth: weights.bit_depth,
        container: weights.container,
        group_allowlist: weights.group_allowlist,
        repack: weights.repack,
        hdr: weights.hdr,
    }
}

/// Lists every quality profile with its currently-effective weights
/// (built-in defaults plus any stored override already applied): never
/// the raw, possibly-partial stored JSON, so a client always has a
/// complete, concrete set of numbers to show and re-submit.
pub async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<QualityProfileSummary>>, (StatusCode, String)> {
    let conn = state.conn.lock().await;
    let mut stmt = conn
        .prepare("SELECT id, name, kind, weights FROM quality_profile ORDER BY id")
        .map_err(internal)?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<_>>()
        .map_err(internal)?;

    let profiles = rows
        .into_iter()
        .map(|(id, name, kind, weights_json)| {
            let profile_kind = if kind == "movie" {
                ProfileKind::Movie
            } else {
                ProfileKind::Tv
            };
            let resolved = QualityProfile::with_weights_override(profile_kind, &weights_json);
            QualityProfileSummary {
                id,
                name,
                kind,
                weights: to_dto(&resolved.weights),
            }
        })
        .collect();
    Ok(Json(profiles))
}

/// Overwrites a profile's stored `weights` column with exactly the
/// submitted values (a full replacement, not a partial patch): the TUI
/// always submits the complete resolved set it displayed, so there's no
/// ambiguity about what "the rest stays as it was" would even mean here.
pub async fn update_weights(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateQualityProfileWeightsRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !weights_are_valid(&req.weights) {
        return Err((
            StatusCode::BAD_REQUEST,
            "quality-profile weights must be finite and >= 0".into(),
        ));
    }
    let weights_json = serde_json::to_string(&req.weights).map_err(internal)?;
    let conn = state.conn.lock().await;
    let updated = conn
        .execute(
            "UPDATE quality_profile SET weights = ?1 WHERE id = ?2",
            rusqlite::params![weights_json, id],
        )
        .map_err(internal)?;
    if updated == 0 {
        return Err((StatusCode::NOT_FOUND, "quality profile not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn weights_are_valid(w: &WeightsDto) -> bool {
    [
        w.seeder,
        w.resolution_tier,
        w.source_tier,
        w.codec_tier,
        w.bit_depth,
        w.container,
        w.group_allowlist,
        w.repack,
        w.hdr,
    ]
    .into_iter()
    .all(|v| v.is_finite() && v >= 0.0)
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_weights() -> WeightsDto {
        WeightsDto {
            seeder: 1.0,
            resolution_tier: 1.0,
            source_tier: 1.0,
            codec_tier: 1.0,
            bit_depth: 1.0,
            container: 1.0,
            group_allowlist: 1.0,
            repack: 1.0,
            hdr: 1.0,
        }
    }

    #[test]
    fn weights_are_valid_accepts_finite_non_negative() {
        assert!(weights_are_valid(&valid_weights()));
    }

    #[test]
    fn weights_are_valid_rejects_nan_inf_and_negative() {
        let mut w = valid_weights();
        w.seeder = f32::NAN;
        assert!(!weights_are_valid(&w));
        w = valid_weights();
        w.hdr = f32::INFINITY;
        assert!(!weights_are_valid(&w));
        w = valid_weights();
        w.repack = -0.1;
        assert!(!weights_are_valid(&w));
    }
}
