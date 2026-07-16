use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use breadarr_shared::dto::SearchResult;
use serde::Deserialize;

use crate::api::AppState;

fn default_kind() -> String {
    "series".to_string()
}

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    #[serde(default = "default_kind")]
    kind: String,
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<SearchResult>>, (StatusCode, String)> {
    if params.kind == "movie" {
        let Some(tmdb) = &state.tmdb else {
            return Err((
                StatusCode::PRECONDITION_FAILED,
                "tmdb.bearer_token is not configured".into(),
            ));
        };
        let results = tmdb
            .search_movie(&params.q)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?
            .into_iter()
            .map(|r| SearchResult {
                external_id: r.external_id,
                title: r.title,
                year: r.year.map(|y| y as i64),
            })
            .collect();
        return Ok(Json(results));
    }

    let Some(tvdb) = &state.tvdb else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "tvdb.api_key is not configured".into(),
        ));
    };
    let results = tvdb
        .search_series(&params.q)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?
        .into_iter()
        .map(|r| SearchResult {
            external_id: r.external_id,
            title: r.name,
            year: r.year.map(|y| y as i64),
        })
        .collect();
    Ok(Json(results))
}
