use axum::extract::State;
use axum::Json;
use breadarr_shared::dto::{CycleInfo, HealthDetail, HealthStatus};

use crate::api::AppState;

fn to_info(r: &crate::api::CycleRecord) -> CycleInfo {
    CycleInfo {
        at: r.at,
        ok: r.ok,
        detail: r.detail.clone(),
    }
}

/// Unauthenticated liveness: cheap, no cycle detail.
pub async fn health() -> Json<HealthStatus> {
    Json(HealthStatus {
        status: "ok".to_string(),
    })
}

/// Authenticated cycle-status payload (same as the old `/health`).
pub async fn health_detail(State(state): State<AppState>) -> Json<HealthDetail> {
    let status = state.cycle_status.lock().expect("cycle_status poisoned");
    Json(HealthDetail {
        status: "ok".to_string(),
        last_grab_cycle: status.last_grab.as_ref().map(to_info),
        last_import_cycle: status.last_import.as_ref().map(to_info),
        last_search_cycle: status.last_search.as_ref().map(to_info),
        last_upgrade_cycle: status.last_upgrade.as_ref().map(to_info),
        last_transcode_cycle: status.last_transcode.as_ref().map(to_info),
        search_halted: status.search_halted,
    })
}
