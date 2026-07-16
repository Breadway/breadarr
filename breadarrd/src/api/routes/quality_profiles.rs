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
/// (built-in defaults plus any stored override already applied) — never
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
/// submitted values (a full replacement, not a partial patch) — the TUI
/// always submits the complete resolved set it displayed, so there's no
/// ambiguity about what "the rest stays as it was" would even mean here.
pub async fn update_weights(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateQualityProfileWeightsRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
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

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
