pub mod routes;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::metadata::tmdb::TmdbClient;
use crate::metadata::tvdb::TvdbClient;
use crate::qbit::QbitClient;
use crate::scheduler::SearchCycleStats;

/// A `tokio::sync::Mutex`, not `std::sync::Mutex` — several handlers (e.g.
/// review-queue approval) interleave synchronous DB calls with `.await`ed
/// qBittorrent/TVDB calls, and a sync `MutexGuard` can't be held across an
/// await point. This one can.
#[derive(Clone)]
pub struct AppState {
    pub conn: Arc<Mutex<Connection>>,
    pub tvdb: Option<Arc<TvdbClient>>,
    pub tmdb: Option<Arc<TmdbClient>>,
    pub qbit: Option<Arc<QbitClient>>,
    pub qbit_category: String,
    pub cycle_status: Arc<std::sync::Mutex<CycleStatus>>,
    pub config: breadarr_shared::Config,
    /// `None` when the background loop isn't running (qbit not
    /// configured) — routes that need the search pipeline (manual
    /// "search now", the candidate picker) dispatch through this rather
    /// than running it inline, since that pipeline holds a `Connection`/
    /// `&dyn ReleaseSource` across `.await` points and is therefore
    /// `!Send`, which axum's `Handler` trait doesn't allow. Routing
    /// through the loop's own already-running instance also means these
    /// reuse its already-loaded title-matcher/sources for free.
    pub background_tx: Option<tokio::sync::mpsc::Sender<BackgroundRequest>>,
}

/// Everything an API handler needs the background loop to do on its
/// behalf, because the work involves `!Send` state (see `AppState::
/// background_tx`'s doc comment) that can't be run directly inside an
/// axum handler.
pub enum BackgroundRequest {
    SearchNow {
        media_item_id: i64,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<SearchCycleStats>>,
    },
    FetchCandidates {
        media_item_id: i64,
        episode_id: Option<i64>,
        reply: tokio::sync::oneshot::Sender<
            anyhow::Result<Vec<breadarr_shared::dto::ReleaseCandidate>>,
        >,
    },
    GrabCandidate {
        media_item_id: i64,
        episode_id: Option<i64>,
        source_id: i64,
        raw_title: String,
        link: String,
        guid: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
}

/// One record per completed grab/import cycle attempt — plain `std::sync::
/// Mutex` is fine here (unlike `conn`) since updates are a single field
/// write with no `.await` in between. Exists so a silently-stalled
/// background loop (e.g. every cycle erroring for hours) is visible from a
/// single `/health` call instead of only in the journal.
#[derive(Clone, Default)]
pub struct CycleStatus {
    pub last_grab: Option<CycleRecord>,
    pub last_import: Option<CycleRecord>,
    pub last_search: Option<CycleRecord>,
    pub last_upgrade: Option<CycleRecord>,
    pub last_transcode: Option<CycleRecord>,
    /// Set once the search-driven loop's consecutive-failure backoff hits
    /// its ceiling — still ticking at max backoff underneath (self-healing
    /// if the source recovers), but worth a loud, easy-to-spot signal that
    /// something's wrong with 1337x/nyaa-search specifically.
    pub search_halted: bool,
}

#[derive(Clone)]
pub struct CycleRecord {
    pub at: chrono::DateTime<chrono::Utc>,
    pub ok: bool,
    pub detail: String,
}

/// Rejects any request lacking `Authorization: Bearer <config.daemon.api_token>`
/// once a token is actually configured — a no-op (every request passes)
/// when it's empty, so an unconfigured install behaves exactly as before.
/// `/health` is deliberately exempt even with a token configured: it's
/// commonly polled by external monitoring (e.g. an uptime dashboard) that
/// has no reason to hold the same credential as the TUI/API client, and it
/// exposes nothing more sensitive than "is the process alive."
async fn require_api_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.config.daemon.api_token.is_empty() || req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let authorized = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token, &state.config.daemon.api_token));
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "missing or invalid API token").into_response();
    }
    next.run(req).await
}

/// Byte-wise `==` short-circuits on the first mismatching byte, making
/// comparison time a (weak, but real) signal of how many leading bytes of a
/// guessed token were correct — a classic timing oracle. This always
/// touches every byte of the shorter input regardless of where they first
/// differ. Real-world exposure here is low (loopback-bound by default, a
/// personal single-user daemon), but it costs nothing to close.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(routes::health::health))
        .route("/media", get(routes::media::list).post(routes::media::add))
        .route(
            "/media/:id",
            get(routes::media::detail).delete(routes::media::delete),
        )
        .route("/media/movie", post(routes::media::add_movie))
        .route("/media/:id/search", post(routes::media::search_now))
        .route(
            "/media/:id/candidates",
            get(routes::media::list_candidates).post(routes::media::grab_candidate),
        )
        .route(
            "/episode/:id/candidates",
            get(routes::media::list_episode_candidates).post(routes::media::grab_episode_candidate),
        )
        .route("/media/:id/monitor", post(routes::media::monitor))
        .route("/media/:id/unmonitor", post(routes::media::unmonitor))
        .route(
            "/media/:id/season/:season_number/monitor",
            post(routes::media::monitor_season),
        )
        .route(
            "/media/:id/season/:season_number/unmonitor",
            post(routes::media::unmonitor_season),
        )
        .route("/episode/:id/monitor", post(routes::media::monitor_episode))
        .route(
            "/episode/:id/unmonitor",
            post(routes::media::unmonitor_episode),
        )
        .route(
            "/episode/:id/file",
            axum::routing::delete(routes::media::delete_episode_file),
        )
        .route(
            "/media/:id/file",
            axum::routing::delete(routes::media::delete_movie_file),
        )
        .route("/releases", get(routes::releases::list))
        .route("/review", get(routes::review::list))
        .route("/review/:id/approve", post(routes::review::approve))
        .route("/review/:id/reject", post(routes::review::reject))
        .route("/search", get(routes::search::search))
        .route("/stuck", get(routes::stuck::stuck))
        .route("/calendar", get(routes::calendar::calendar))
        .route(
            "/library/health",
            get(routes::library_health::library_health),
        )
        .route("/quality-profiles", get(routes::quality_profiles::list))
        .route(
            "/quality-profiles/:id/weights",
            axum::routing::put(routes::quality_profiles::update_weights),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_token,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_identical_strings() {
        assert!(constant_time_eq("secret-token", "secret-token"));
    }

    #[test]
    fn constant_time_eq_rejects_different_strings_of_the_same_length() {
        assert!(!constant_time_eq("secret-token", "secret-toke1"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq("short", "a-much-longer-token"));
    }

    #[test]
    fn constant_time_eq_treats_empty_strings_as_equal() {
        assert!(constant_time_eq("", ""));
    }
}
