use axum::extract::State;
use axum::Json;
use breadarr_shared::dto::{CycleInfo, HealthDetail};

use crate::api::AppState;

fn to_info(r: &crate::api::CycleRecord) -> CycleInfo {
    CycleInfo {
        at: r.at,
        ok: r.ok,
        detail: r.detail.clone(),
    }
}

pub async fn health(State(state): State<AppState>) -> Json<HealthDetail> {
    let status = state.cycle_status.lock().expect("cycle_status poisoned");
    Json(HealthDetail {
        status: "ok".to_string(),
        last_grab_cycle: status.last_grab.as_ref().map(to_info),
        last_import_cycle: status.last_import.as_ref().map(to_info),
        last_search_cycle: status.last_search.as_ref().map(to_info),
        last_upgrade_cycle: status.last_upgrade.as_ref().map(to_info),
        search_halted: status.search_halted,
    })
}
