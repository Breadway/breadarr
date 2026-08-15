mod api;
mod db;
mod importer;
mod jellyfin;
mod library_scan;
mod matcher;
mod metadata;
mod notify;
mod parser;
mod qbit;
mod scheduler;
mod scoring;
mod sources;
mod transcode;

use std::env;

use anyhow::{bail, Result};
use breadarr_shared::Config;
use jellyfin::JellyfinClient;
use metadata::tvdb::TvdbClient;
use qbit::QbitClient;
use rusqlite::Connection;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    // ort logs its own session/hardware setup at INFO, which drowns out
    // everything else at the default level — keep it to warnings and up.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(format!(
            "{},ort::logging=warn",
            config.daemon.log_level
        )))
        .init();

    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("debug-qbit-add") => {
            let Some(magnet) = args.get(2) else {
                bail!("usage: breadarrd debug-qbit-add <magnet-uri>");
            };
            return debug_qbit_add(&config, magnet).await;
        }
        Some("debug-jellyfin-refresh") => {
            return debug_jellyfin_refresh(&config).await;
        }
        Some("debug-tvdb-add") => {
            let Some(query) = args.get(2) else {
                bail!("usage: breadarrd debug-tvdb-add <series-name>");
            };
            return debug_tvdb_add(&config, query).await;
        }
        Some("debug-anime-map-refresh") => {
            return debug_anime_map_refresh(&config).await;
        }
        Some("debug-match-title") => {
            let Some(query) = args.get(2) else {
                bail!("usage: breadarrd debug-match-title <raw-release-title>");
            };
            return debug_match_title(&config, query).await;
        }
        Some("debug-grab-cycle") => {
            let feed_url = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("https://nyaa.si/?page=rss");
            return debug_grab_cycle(&config, feed_url).await;
        }
        Some("debug-import-cycle") => {
            return debug_import_cycle(&config).await;
        }
        Some("debug-1337x-search") => {
            let Some(query) = args.get(2) else {
                bail!("usage: breadarrd debug-1337x-search <query>");
            };
            return debug_1337x_search(&config, query).await;
        }
        Some("debug-tvdb-search") => {
            let Some(query) = args.get(2) else {
                bail!("usage: breadarrd debug-tvdb-search <query>");
            };
            return debug_tvdb_search(&config, query).await;
        }
        Some("debug-scan-tv") => {
            let Some(path) = args.get(2) else {
                bail!("usage: breadarrd debug-scan-tv <root-dir>");
            };
            return debug_scan_tv(&config, path).await;
        }
        Some("debug-scan-movies") => {
            let Some(path) = args.get(2) else {
                bail!("usage: breadarrd debug-scan-movies <root-dir>");
            };
            return debug_scan_movies(&config, path).await;
        }
        Some("debug-search-show") => {
            let Some(title) = args.get(2) else {
                bail!("usage: breadarrd debug-search-show <exact-title>");
            };
            return debug_search_show(&config, title).await;
        }
        Some("debug-reconcile-report") => {
            return debug_reconcile_report(&config).await;
        }
        Some("debug-qbit-list") => {
            let category = args.get(2).map(String::as_str);
            return debug_qbit_list(&config, category).await;
        }
        Some("remux-backlog") => {
            return remux_backlog_cmd(&config).await;
        }
        Some("probe-library") => {
            return probe_library_cmd(&config).await;
        }
        Some("transcode-library") => {
            return transcode_library_cmd(&config).await;
        }
        Some("retranscode-oversized") => {
            return retranscode_oversized_cmd(&config).await;
        }
        Some("verify-library") => {
            return verify_library_cmd(&config).await;
        }
        Some("relink-orphaned-files") => {
            return relink_orphaned_files_cmd(&config).await;
        }
        _ => {}
    }

    run_daemon(config).await
}

async fn run_daemon(config: Config) -> Result<()> {
    info!("starting breadarrd");

    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(e) = db::backup_before_open(&config.db_path()) {
        // Never block startup on a backup failure (disk full, permissions)
        // — losing the safety net for this one run is far better than the
        // daemon refusing to start at all.
        tracing::warn!(error = %e, "database backup failed, continuing without one");
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;
    info!(path = %config.db_path().display(), "database ready");
    match transcode::reset_orphaned_running_jobs(&conn) {
        Ok(0) => {}
        Ok(n) => info!(
            n,
            "reset orphaned 'running' transcode jobs left over from a previous crash"
        ),
        Err(e) => tracing::warn!(error = %e, "failed to reset orphaned transcode jobs"),
    }

    // A second, independent connection for the HTTP API rather than sharing
    // `background_loop`'s. Both point at the same on-disk (WAL-mode)
    // database, so this doesn't weaken consistency — it just means an API
    // request (a TUI poll, a review-queue approval) is no longer serialized
    // behind whatever `background_loop` happens to be doing, which used to
    // include holding its connection's lock across an entire grab/import/
    // search cycle's network I/O and jitter sleeps. `busy_timeout` (set in
    // `db::init`) covers the rare case where both connections genuinely
    // want to write at the same instant.
    let api_conn = Connection::open(config.db_path())?;
    db::init(&api_conn)?;

    let listener = tokio::net::TcpListener::bind(&config.daemon.listen_addr).await?;
    info!(addr = %config.daemon.listen_addr, "listening");
    if config.daemon.api_token.is_empty() {
        tracing::warn!(
            "daemon.api_token is empty; any local process can add/delete/grab via {}",
            config.daemon.listen_addr
        );
    } else if !config.listen_is_loopback() {
        info!("API token auth is required (listen_addr is non-loopback)");
    }

    let tvdb = if config.tvdb.api_key.is_empty() {
        None
    } else {
        Some(std::sync::Arc::new(TvdbClient::new(
            config.tvdb.api_key.clone(),
        )))
    };
    let qbit = if config.qbit.base_url.is_empty() {
        None
    } else {
        let client = QbitClient::new(config.qbit.base_url.clone())?;
        if !config.qbit.username.is_empty() {
            client
                .login(&config.qbit.username, &config.qbit.password)
                .await?;
        }
        Some(std::sync::Arc::new(client))
    };
    let tmdb = if config.tmdb.bearer_token.is_empty() {
        None
    } else {
        Some(std::sync::Arc::new(metadata::tmdb::TmdbClient::new(
            config.tmdb.bearer_token.clone(),
        )))
    };
    let (background_tx, background_rx) = tokio::sync::mpsc::channel(8);
    // Lifted out of `background_loop` so shutdown can wait for an in-flight
    // ffmpeg encode instead of dropping the process the instant SIGTERM
    // arrives (systemd's TimeoutStopSec is longer than this wait).
    let transcode_busy = std::sync::Arc::new(tokio::sync::Mutex::new(()));

    let background_conn = std::sync::Arc::new(tokio::sync::Mutex::new(conn));
    let state = api::AppState {
        conn: std::sync::Arc::new(tokio::sync::Mutex::new(api_conn)),
        tvdb,
        tmdb,
        qbit,
        qbit_category: config.qbit.category.clone(),
        cycle_status: std::sync::Arc::new(std::sync::Mutex::new(api::CycleStatus::default())),
        config: config.clone(),
        background_tx: None,
    };

    let background = state.qbit.clone().map(|qbit| {
        let jellyfin = if config.jellyfin.base_url.is_empty() {
            None
        } else {
            Some(JellyfinClient::new(
                config.jellyfin.base_url.clone(),
                config.jellyfin.api_key.clone(),
            ))
        };
        background_loop(
            background_conn,
            qbit,
            jellyfin,
            config.clone(),
            state.cycle_status.clone(),
            background_rx,
            transcode_busy.clone(),
        )
    });
    let mut state = state;
    if background.is_some() {
        // Only set once the background loop is actually going to run and
        // poll the receiver end of this channel — otherwise `background_rx`
        // (moved into the discarded `.map()` closure above, since `.map()`
        // never calls its closure on `None`) is already dropped, and any
        // send on `background_tx` would hang forever with no receiver.
        state.background_tx = Some(background_tx);
    } else {
        tracing::warn!("qbit.base_url is not set; automatic grab/import loop is disabled");
    }

    let app = api::router(state);
    // `background_loop` holds a `rusqlite::Connection` across `.await`
    // points, which isn't `Send` — it has to stay a branch of this same
    // root future (already not `Send`-constrained, since it's only ever
    // `.await`ed directly, never `tokio::spawn`ed) rather than its own task.
    tokio::select! {
        result = async { axum::serve(listener, app).await } => {
            if let Err(err) = result {
                error!(error = %err, "http server failed");
            }
        }
        _ = async {
            match background {
                Some(fut) => fut.await,
                None => std::future::pending().await,
            }
        } => {
            // `background_loop` retries its own init and never returns in
            // normal operation, so reaching here means something truly
            // unexpected happened — exit non-zero rather than falling
            // through to `Ok(())`, so systemd's `Restart=` actually engages
            // instead of leaving a "successfully exited" daemon dead until
            // someone notices by hand.
            anyhow::bail!("background grab/import loop exited unexpectedly");
        }
        _ = wait_for_shutdown() => {
            info!("shutdown signal received");
        }
    }

    // Wait for an in-flight transcode (it holds this mutex for the whole
    // cycle) so ffmpeg can finish, or at least so systemd's longer
    // TimeoutStopSec applies instead of an instant drop.
    info!("waiting up to 120s for in-flight transcode to finish");
    match tokio::time::timeout(std::time::Duration::from_secs(120), transcode_busy.lock()).await {
        Ok(_) => info!("no in-flight transcode (or it finished)"),
        Err(_) => tracing::warn!("timed out waiting 120s for in-flight transcode"),
    }

    Ok(())
}

async fn load_title_matcher(config: &Config) -> Result<matcher::TitleMatcher> {
    let (model_path, tokenizer_path) = matcher::ensure_model(&config.model_dir()).await?;
    matcher::TitleMatcher::load(&model_path, &tokenizer_path)
}

/// Runs the daemon's own auto-grab/import/search loop for as long as the
/// process lives. Owns its own `TitleMatcher` (the HTTP API never needs one
/// — matching only happens when a fresh release comes in here or in a
/// review-queue approval, which just replays an already-decided grab).
/// nyaa RSS (anime TV) is a feed watch; movies and non-anime TV go through
/// the search-driven loop instead (1337x, or nyaa's search mode for anime
/// movies) — see `scheduler::run_search_cycle`.
async fn background_loop(
    conn: std::sync::Arc<tokio::sync::Mutex<Connection>>,
    qbit: std::sync::Arc<QbitClient>,
    jellyfin: Option<JellyfinClient>,
    config: Config,
    cycle_status: std::sync::Arc<std::sync::Mutex<api::CycleStatus>>,
    mut background_rx: tokio::sync::mpsc::Receiver<api::BackgroundRequest>,
    transcode_busy: std::sync::Arc<tokio::sync::Mutex<()>>,
) {
    let notifier = notify::Notifier::new(&config.notifications.webhook_url);
    {
        let conn = conn.lock().await;
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
             VALUES (1, 'nyaa', 'rss', ?1, ?2, 1)",
            rusqlite::params![
                config.sources.nyaa_rss_url,
                config.sources.grab_poll_interval_secs
            ],
        ) {
            error!(error = %e, "failed to register nyaa source row");
        }
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
             VALUES (2, '1337x', 'scrape', ?1, ?2, 1)",
            rusqlite::params![
                config
                    .sources
                    .torrent_1337x_mirrors
                    .first()
                    .cloned()
                    .unwrap_or_default(),
                config.sources.search_poll_interval_secs
            ],
        ) {
            error!(error = %e, "failed to register 1337x source row");
        }
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
             VALUES (3, 'tpb', 'scrape', ?1, ?2, 1)",
            rusqlite::params![
                config.sources.tpb_api_url,
                config.sources.search_poll_interval_secs
            ],
        ) {
            error!(error = %e, "failed to register tpb source row");
        }
    }

    // A transient failure here (network blip during the one-time model
    // download, momentary disk issue) must not permanently disable the
    // loop — `run_daemon`'s `select!` treats a *return* from this function
    // as fatal and exits the whole process, but only so systemd's restart
    // policy can engage; retrying in-place first means a blip doesn't need
    // a process restart at all.
    let mut title_matcher = loop {
        match load_title_matcher(&config).await {
            Ok(m) => break m,
            Err(e) => {
                error!(error = %e, "failed to init title matcher; retrying in 30s");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
    };

    let nyaa_source = sources::rss::RssSource::new(config.sources.nyaa_rss_url.clone());
    let scrape_source =
        sources::scrape::ScrapeSource::new(config.sources.torrent_1337x_mirrors.clone());
    let tpb_source = sources::tpb::TpbSource::new(config.sources.tpb_api_url.clone());

    let mut grab_ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.sources.grab_poll_interval_secs,
    ));
    let mut import_ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.sources.import_poll_interval_secs,
    ));
    let mut search_ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.sources.search_poll_interval_secs,
    ));
    let mut upgrade_ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.sources.upgrade_poll_interval_secs,
    ));
    // Guards against the transcode ticker itself blocking every other cycle
    // (import, search, upgrade, reconcile) for the full multi-minute
    // duration of an encode — the ticker below spawns each cycle detached
    // rather than awaiting it inline, and this is what stops two spawned
    // cycles from running at once if one is still going when the next tick
    // fires (a real possibility: files easily take longer to encode than
    // `poll_interval_secs`). `claim_pending_jobs`'s own concurrency cap
    // already makes overlap *safe*; this just keeps it from happening
    // pointlessly. Created in `run_daemon` so shutdown can wait on it.
    let mut transcode_ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.transcode.poll_interval_secs,
    ));
    // Disk state doesn't change on its own — hourly is plenty to catch a
    // file deleted/moved by hand without adding meaningful load (one query
    // per tracked episode file, all local). Deliberately does *not* fire at
    // t=0 like the other tickers (`tokio::time::interval`'s default first
    // tick is immediate) — startup is exactly when a network/removable mount
    // is most likely to still be coming up, and reconcile misreading an
    // absent mount as a mass file deletion is the one failure mode worth
    // paying a full interval's delay to avoid.
    let mut reconcile_ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(3600),
        std::time::Duration::from_secs(3600),
    );
    grab_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    import_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    search_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    upgrade_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    reconcile_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    transcode_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Cycle-level backoff on top of the search loop's own per-mirror
    // cooldowns: a whole cycle failing (source exhausted, or an outright
    // error) means something's more broadly wrong than one bad mirror, so
    // back off the *cycle interval* itself — 1h, 2h, 4h, capped — rather
    // than keep firing every `search_poll_interval_secs` regardless.
    let mut search_skip_ticks: u32 = 0;
    let mut search_fail_streak: u32 = 0;

    loop {
        tokio::select! {
            _ = grab_ticker.tick() => {
                if !config.sources.grab_enabled {
                    continue;
                }
                let result = {
                    let conn = conn.lock().await;
                    scheduler::run_grab_cycle(&conn, &nyaa_source, 1, &mut title_matcher, &qbit, &config.qbit.category).await
                };
                if let (Ok(stats), Some(n)) = (&result, &notifier) {
                    if stats.queued_for_review > 0 {
                        n.send("breadarr: review queue", &format!(
                            "{} release(s) need manual confirmation this cycle", stats.queued_for_review
                        )).await;
                    }
                }
                let record = match &result {
                    Ok(stats) => {
                        info!(?stats, "grab cycle complete");
                        api::CycleRecord { at: chrono::Utc::now(), ok: true, detail: format!("{stats:?}") }
                    }
                    Err(e) => {
                        error!(error = %e, "grab cycle failed");
                        api::CycleRecord { at: chrono::Utc::now(), ok: false, detail: e.to_string() }
                    }
                };
                cycle_status.lock().expect("cycle_status poisoned").last_grab = Some(record);
            }
            _ = import_ticker.tick() => {
                let result = {
                    let conn = conn.lock().await;
                    importer::run_import_cycle(&conn, &qbit, jellyfin.as_ref(), &config.qbit.category, &config.qbit.container_downloads_path, &config.qbit.host_downloads_path, config.transcode.enabled.then_some(&config.transcode)).await
                };
                if let (Ok(stats), Some(n)) = (&result, &notifier) {
                    if stats.failed > 0 {
                        n.send("breadarr: import failures", &format!(
                            "{} import(s) gave up this cycle after repeated failures", stats.failed
                        )).await;
                    }
                    if stats.quality_flagged > 0 {
                        n.send("breadarr: quality concern", &format!(
                            "{} freshly-imported file(s) failed a post-import ground-truth check \
                             (under 1080p or unreadable, despite what the release title claimed) \
                             — see the library-health report", stats.quality_flagged
                        )).await;
                    }
                }
                let record = match &result {
                    Ok(stats) => {
                        info!(?stats, "import cycle complete");
                        api::CycleRecord { at: chrono::Utc::now(), ok: true, detail: format!("{stats:?}") }
                    }
                    Err(e) => {
                        error!(error = %e, "import cycle failed");
                        api::CycleRecord { at: chrono::Utc::now(), ok: false, detail: e.to_string() }
                    }
                };
                cycle_status.lock().expect("cycle_status poisoned").last_import = Some(record);
            }
            _ = search_ticker.tick() => {
                if !config.sources.search_enabled {
                    continue;
                }
                if search_skip_ticks > 0 {
                    search_skip_ticks -= 1;
                    continue;
                }
                let result = {
                    let conn = conn.lock().await;
                    scheduler::run_search_cycle(
                        &conn,
                        &tpb_source, 3,
                        &scrape_source, 2,
                        &nyaa_source, 1,
                        &mut title_matcher,
                        &qbit, &config.qbit.category,
                        config.sources.search_budget_per_cycle,
                    ).await
                };
                let cycle_failed = matches!(&result, Ok(stats) if stats.source_exhausted) || result.is_err();
                let record = match &result {
                    Ok(stats) => {
                        info!(?stats, "search cycle complete");
                        api::CycleRecord { at: chrono::Utc::now(), ok: !stats.source_exhausted, detail: format!("{stats:?}") }
                    }
                    Err(e) => {
                        error!(error = %e, "search cycle failed");
                        api::CycleRecord { at: chrono::Utc::now(), ok: false, detail: e.to_string() }
                    }
                };
                if cycle_failed {
                    search_fail_streak = (search_fail_streak + 1).min(10);
                    search_skip_ticks = (1u32 << search_fail_streak.min(3)).saturating_sub(1).min(7);
                } else {
                    search_fail_streak = 0;
                    search_skip_ticks = 0;
                }
                let just_halted = {
                    let mut status = cycle_status.lock().expect("cycle_status poisoned");
                    status.last_search = Some(record);
                    let just_halted = search_fail_streak >= 10 && !status.search_halted;
                    if search_fail_streak >= 10 {
                        if just_halted {
                            error!("search cycle has failed 10 cycles in a row; still retrying at max backoff, but this needs attention");
                        }
                        status.search_halted = true;
                    } else if !cycle_failed {
                        status.search_halted = false;
                    }
                    just_halted
                };
                if just_halted {
                    if let Some(n) = &notifier {
                        n.send("breadarr: search halted", "The search-driven loop has failed 10 cycles in a row and is now at max backoff — still retrying automatically, but this needs attention.").await;
                    }
                }
                if let (Ok(stats), Some(n)) = (&result, &notifier) {
                    if stats.queued_for_review > 0 {
                        n.send("breadarr: review queue", &format!(
                            "{} release(s) need manual confirmation this cycle", stats.queued_for_review
                        )).await;
                    }
                }
            }
            _ = upgrade_ticker.tick() => {
                if !config.sources.upgrade_enabled {
                    continue;
                }
                let result = {
                    let conn = conn.lock().await;
                    scheduler::run_upgrade_cycle(
                        &conn,
                        &tpb_source, 3,
                        &scrape_source, 2,
                        &nyaa_source, 1,
                        &mut title_matcher,
                        &qbit, &config.qbit.category,
                        config.sources.upgrade_budget_per_cycle,
                        config.sources.upgrade_min_score_gain,
                    ).await
                };
                let record = match &result {
                    Ok(stats) => {
                        info!(?stats, "upgrade cycle complete");
                        api::CycleRecord { at: chrono::Utc::now(), ok: !stats.source_exhausted, detail: format!("{stats:?}") }
                    }
                    Err(e) => {
                        error!(error = %e, "upgrade cycle failed");
                        api::CycleRecord { at: chrono::Utc::now(), ok: false, detail: e.to_string() }
                    }
                };
                cycle_status.lock().expect("cycle_status poisoned").last_upgrade = Some(record);
                if let (Ok(stats), Some(n)) = (&result, &notifier) {
                    if stats.grabbed > 0 {
                        n.send("breadarr: quality upgrade", &format!(
                            "{} file(s) replaced this cycle with a better-scoring release", stats.grabbed
                        )).await;
                    }
                }
            }
            _ = transcode_ticker.tick() => {
                if !config.transcode.enabled {
                    continue;
                }
                let Ok(busy_permit) = transcode_busy.clone().try_lock_owned() else {
                    // A previous cycle is still running (this file's
                    // encode took longer than one poll interval) — skip
                    // this tick rather than spawning a second overlapping
                    // one; the still-running cycle will pick up any newly
                    // pending jobs on its own next iteration anyway.
                    continue;
                };
                let conn = conn.clone();
                let cfg = config.transcode.clone();
                let jellyfin = jellyfin.clone();
                let cycle_status = cycle_status.clone();
                tokio::spawn(async move {
                    let _permit = busy_permit;
                    let result = transcode::run_cycle(conn, cfg, jellyfin.as_ref()).await;
                    let record = match &result {
                        Ok(stats) => {
                            info!(?stats, "transcode cycle complete");
                            api::CycleRecord { at: chrono::Utc::now(), ok: true, detail: format!("{stats:?}") }
                        }
                        Err(e) => {
                            error!(error = %e, "transcode cycle failed");
                            api::CycleRecord { at: chrono::Utc::now(), ok: false, detail: e.to_string() }
                        }
                    };
                    cycle_status.lock().expect("cycle_status poisoned").last_transcode = Some(record);
                });
            }
            _ = reconcile_ticker.tick() => {
                let result = {
                    let conn = conn.lock().await;
                    importer::reconcile_missing_files(&conn, false)
                };
                match result {
                    Ok(outcome) if outcome.aborted => {
                        error!("missing-file reconciliation aborted: an anomalous fraction of the library looked gone at once");
                        if let Some(n) = &notifier {
                            n.send("breadarr: reconcile aborted", &format!(
                                "reconcile found far more missing files than expected in one pass and refused to \
                                 touch anything — check whether a library mount is offline. Nothing was changed. \
                                 (details: {outcome:?})"
                            )).await;
                        }
                    }
                    Ok(outcome) if outcome.repaired > 0 || outcome.cleared > 0 => {
                        info!(?outcome, "reconciled library state against disk");
                        if outcome.cleared > 0 {
                            if let Some(n) = &notifier {
                                n.send("breadarr: files went missing", &format!(
                                    "{} file(s) tracked in the library are no longer on disk \
                                     (deleted or moved outside breadarr) — cleared from tracking, \
                                     will be re-searched if still monitored. {} other stale path(s) \
                                     were auto-repaired.", outcome.cleared, outcome.repaired
                                )).await;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => error!(error = %e, "missing-file reconciliation failed"),
                }
                // Right after reconcile, in the same tick, so a path it just
                // repaired (a rename or an in-place transcode) gets re-probed
                // immediately rather than waiting for its own turn a full
                // interval later.
                let probe_result = {
                    let conn = conn.lock().await;
                    importer::probe_library(&conn)
                };
                match probe_result {
                    Ok(report) if report.probed > 0 || report.failed > 0 => {
                        info!(?report, "media probe sweep complete");
                    }
                    Ok(_) => {}
                    Err(e) => error!(error = %e, "media probe sweep failed"),
                }
            }
            Some(req) = background_rx.recv() => {
                match req {
                    api::BackgroundRequest::SearchNow { media_item_id, reply } => {
                        let result = {
                            let conn = conn.lock().await;
                            match scheduler::enumerate_search_targets_for_media_item(&conn, media_item_id) {
                                Ok(targets) => scheduler::execute_search_targets(
                                    &conn,
                                    &targets,
                                    &tpb_source, 3,
                                    &scrape_source, 2,
                                    &nyaa_source, 1,
                                    &mut title_matcher,
                                    &qbit, &config.qbit.category,
                                ).await,
                                Err(e) => Err(e),
                            }
                        };
                        if let Ok(stats) = &result {
                            info!(?stats, media_item_id, "manual search-now complete");
                        }
                        // Ignore a send failure — the HTTP request that asked
                        // for this may have already timed out/disconnected,
                        // which doesn't invalidate the search itself (it
                        // still ran and any grabs it made are already
                        // recorded).
                        let _ = reply.send(result);
                    }
                    api::BackgroundRequest::FetchCandidates { media_item_id, episode_id, reply } => {
                        let result = {
                            let conn = conn.lock().await;
                            scheduler::fetch_candidates(
                                &conn,
                                media_item_id,
                                episode_id,
                                &tpb_source, 3,
                                &scrape_source, 2,
                                &nyaa_source, 1,
                            ).await
                        };
                        let _ = reply.send(result);
                    }
                    api::BackgroundRequest::GrabCandidate { media_item_id, episode_id, source_id, raw_title, link, guid, reply } => {
                        let result = {
                            let conn = conn.lock().await;
                            scheduler::grab_candidate(
                                &conn,
                                &qbit,
                                &config.qbit.category,
                                media_item_id,
                                episode_id,
                                source_id,
                                &raw_title,
                                &link,
                                &guid,
                            ).await
                        };
                        if let Ok(()) = &result {
                            info!(media_item_id, episode_id, raw_title, "manually picked candidate grabbed");
                        }
                        let _ = reply.send(result);
                    }
                }
            }
        }
    }
}

async fn debug_qbit_add(config: &Config, magnet: &str) -> Result<()> {
    if config.qbit.base_url.is_empty() {
        bail!("qbit.base_url is not set in config");
    }
    let client = QbitClient::new(config.qbit.base_url.clone())?;
    if !config.qbit.username.is_empty() {
        client
            .login(&config.qbit.username, &config.qbit.password)
            .await?;
    }
    client.add_magnet(magnet, &config.qbit.category).await?;
    println!(
        "added magnet to qbittorrent (category={})",
        config.qbit.category
    );
    Ok(())
}

async fn debug_jellyfin_refresh(config: &Config) -> Result<()> {
    if config.jellyfin.base_url.is_empty() {
        bail!("jellyfin.base_url is not set in config");
    }
    let client = JellyfinClient::new(
        config.jellyfin.base_url.clone(),
        config.jellyfin.api_key.clone(),
    );
    client.refresh_library().await?;
    println!("jellyfin library refresh triggered");
    Ok(())
}

async fn debug_tvdb_add(config: &Config, query: &str) -> Result<()> {
    if config.tvdb.api_key.is_empty() {
        bail!("tvdb.api_key is not set in config");
    }
    let tvdb = TvdbClient::new(config.tvdb.api_key.clone());
    let results = tvdb.search_series(query).await?;
    let Some(top) = results.into_iter().next() else {
        bail!("no TVDB results for {query:?}");
    };
    println!(
        "top match: {} ({:?}) tvdb_id={} aliases={}",
        top.name,
        top.year,
        top.external_id,
        top.aliases.len()
    );

    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let media_item_id = metadata::add_series(
        &conn,
        &tvdb,
        &top.external_id,
        &top.name,
        top.year,
        &top.aliases,
        "/tmp/breadarr-debug-library",
        1,
    )
    .await?;

    let season_count: i64 = conn.query_row(
        "SELECT count(*) FROM season WHERE media_item_id = ?1",
        [media_item_id],
        |row| row.get(0),
    )?;
    let episode_count: i64 = conn.query_row(
        "SELECT count(*) FROM episode WHERE media_item_id = ?1",
        [media_item_id],
        |row| row.get(0),
    )?;
    println!(
        "added media_item id={media_item_id}: {season_count} seasons, {episode_count} episodes"
    );
    Ok(())
}

async fn debug_anime_map_refresh(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let client = reqwest::Client::new();
    let count = metadata::anime_map::refresh(&mut conn, &client).await?;
    println!("anime_mapping refreshed: {count} entries");
    Ok(())
}

async fn debug_match_title(config: &Config, query: &str) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let (model_path, tokenizer_path) = matcher::ensure_model(&config.model_dir()).await?;
    let mut title_matcher = matcher::TitleMatcher::load(&model_path, &tokenizer_path)?;

    match title_matcher.match_title(&conn, query)? {
        matcher::MatchOutcome::Auto(c) => {
            println!(
                "AUTO-MATCH media_item_id={} matched_text={:?} confidence={:.3}",
                c.media_item_id, c.matched_text, c.confidence
            );
        }
        matcher::MatchOutcome::NeedsReview(c) => {
            let review_id = matcher::queue_for_review(&conn, query, &c, None, None)?;
            println!(
                "NEEDS REVIEW (queued id={review_id}) media_item_id={} matched_text={:?} confidence={:.3}",
                c.media_item_id, c.matched_text, c.confidence
            );
        }
        matcher::MatchOutcome::NoMatch => {
            println!("NO MATCH for {query:?}");
        }
    }
    Ok(())
}

async fn debug_grab_cycle(config: &Config, feed_url: &str) -> Result<()> {
    if config.qbit.base_url.is_empty() {
        bail!("qbit.base_url is not set in config");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    conn.execute(
        "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
         VALUES (1, 'nyaa', 'rss', ?1, 300, 1)",
        [feed_url],
    )?;

    let qbit = QbitClient::new(config.qbit.base_url.clone())?;
    if !config.qbit.username.is_empty() {
        qbit.login(&config.qbit.username, &config.qbit.password)
            .await?;
    }

    let (model_path, tokenizer_path) = matcher::ensure_model(&config.model_dir()).await?;
    let mut title_matcher = matcher::TitleMatcher::load(&model_path, &tokenizer_path)?;

    let source = sources::rss::RssSource::new(feed_url);
    let stats = scheduler::run_grab_cycle(
        &conn,
        &source,
        1,
        &mut title_matcher,
        &qbit,
        &config.qbit.category,
    )
    .await?;

    println!("{stats:?}");
    Ok(())
}

async fn debug_import_cycle(config: &Config) -> Result<()> {
    if config.qbit.base_url.is_empty() {
        bail!("qbit.base_url is not set in config");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let qbit = QbitClient::new(config.qbit.base_url.clone())?;
    if !config.qbit.username.is_empty() {
        qbit.login(&config.qbit.username, &config.qbit.password)
            .await?;
    }

    let jellyfin = if config.jellyfin.base_url.is_empty() {
        None
    } else {
        Some(JellyfinClient::new(
            config.jellyfin.base_url.clone(),
            config.jellyfin.api_key.clone(),
        ))
    };

    let stats = importer::run_import_cycle(
        &conn,
        &qbit,
        jellyfin.as_ref(),
        &config.qbit.category,
        &config.qbit.container_downloads_path,
        &config.qbit.host_downloads_path,
        config.transcode.enabled.then_some(&config.transcode),
    )
    .await?;
    println!("{stats:?}");
    Ok(())
}

async fn debug_1337x_search(config: &Config, query: &str) -> Result<()> {
    let source = sources::scrape::ScrapeSource::new(config.sources.torrent_1337x_mirrors.clone());
    let items = sources::ReleaseSource::fetch(&source, Some(query)).await?;
    println!("{} results", items.len());
    let mut sorted = items;
    sorted.sort_by_key(|i| std::cmp::Reverse(i.seeders.unwrap_or(0)));
    for item in sorted.iter().take(10) {
        println!(
            "  seeders={:<6} leechers={:<6} size={:>10} {}",
            item.seeders.unwrap_or(0),
            item.leechers.unwrap_or(0),
            item.size_bytes
                .map(|b| format!("{:.1}MB", b as f64 / 1_048_576.0))
                .unwrap_or_default(),
            item.title
        );
    }

    if let Some(top) = sorted.first() {
        println!("\nresolving magnet for top result: {}", top.title);
        let client = reqwest::Client::new();
        let magnet = sources::scrape::resolve_magnet(&client, &top.link).await?;
        println!("magnet: {}", &magnet[..magnet.len().min(120)]);
    }
    Ok(())
}

async fn debug_tvdb_search(config: &Config, query: &str) -> Result<()> {
    if config.tvdb.api_key.is_empty() {
        bail!("tvdb.api_key is not set in config");
    }
    let tvdb = TvdbClient::new(config.tvdb.api_key.clone());
    let results = tvdb.search_series(query).await?;
    println!("{} results for {query:?}", results.len());
    for r in &results {
        println!(
            "  id={} name={:?} year={:?} aliases={:?}",
            r.external_id, r.name, r.year, r.aliases
        );
    }
    Ok(())
}

async fn debug_scan_tv(config: &Config, path: &str) -> Result<()> {
    if config.tvdb.api_key.is_empty() {
        bail!("tvdb.api_key is not set in config");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let tvdb = TvdbClient::new(config.tvdb.api_key.clone());
    let (model_path, tokenizer_path) = matcher::ensure_model(&config.model_dir()).await?;
    let mut title_matcher = matcher::TitleMatcher::load(&model_path, &tokenizer_path)?;
    let jellyfin = if config.jellyfin.base_url.is_empty() {
        None
    } else {
        Some(JellyfinClient::new(
            config.jellyfin.base_url.clone(),
            config.jellyfin.api_key.clone(),
        ))
    };
    let report = library_scan::scan_tv_root(
        &conn,
        &tvdb,
        &mut title_matcher,
        std::path::Path::new(path),
        1,
        jellyfin.as_ref(),
    )
    .await?;

    println!(
        "matched={} unmatched={} files_linked={} files_renamed={} files_reorganized={}",
        report.matched.len(),
        report.unmatched.len(),
        report.files_linked,
        report.files_renamed,
        report.files_reorganized
    );
    if !report.unmatched.is_empty() {
        println!("unmatched:");
        for name in &report.unmatched {
            println!("  {name}");
        }
    }
    Ok(())
}

async fn debug_scan_movies(config: &Config, path: &str) -> Result<()> {
    if config.tmdb.bearer_token.is_empty() {
        bail!("tmdb.bearer_token is not set in config");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let tmdb = metadata::tmdb::TmdbClient::new(config.tmdb.bearer_token.clone());
    let (model_path, tokenizer_path) = matcher::ensure_model(&config.model_dir()).await?;
    let mut title_matcher = matcher::TitleMatcher::load(&model_path, &tokenizer_path)?;
    let jellyfin = if config.jellyfin.base_url.is_empty() {
        None
    } else {
        Some(JellyfinClient::new(
            config.jellyfin.base_url.clone(),
            config.jellyfin.api_key.clone(),
        ))
    };
    let report = library_scan::scan_movie_root(
        &conn,
        &tmdb,
        &mut title_matcher,
        std::path::Path::new(path),
        2,
        jellyfin.as_ref(),
    )
    .await?;

    println!(
        "matched={} unmatched={} files_linked={} files_renamed={} files_reorganized={}",
        report.matched.len(),
        report.unmatched.len(),
        report.files_linked,
        report.files_renamed,
        report.files_reorganized
    );
    if !report.unmatched.is_empty() {
        println!("unmatched:");
        for name in &report.unmatched {
            println!("  {name}");
        }
    }
    Ok(())
}

/// Manually scoped acquisition pass over a single already-tracked show or
/// movie: every currently-missing episode, in one bounded run — not subject
/// to the background loop's per-cycle budget or due-ness cadence (those
/// exist to pace *unattended, indefinite* operation; a one-off, explicitly
/// requested pass over one title's own backlog doesn't need throttling
/// against itself). Still uses the same mirror rotation/cooldown and
/// per-item jitter as the background loop, so it stays no more aggressive
/// per request than normal operation — just not spread across hours.
async fn debug_search_show(config: &Config, title: &str) -> Result<()> {
    if config.qbit.base_url.is_empty() {
        bail!("qbit.base_url is not set in config");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let Some(media_item_id) = scheduler::find_media_item_id_by_title(&conn, title)? else {
        bail!("no tracked media_item with title {title:?} (must match exactly)");
    };

    conn.execute(
        "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
         VALUES (1, 'nyaa', 'rss', ?1, ?2, 1)",
        rusqlite::params![
            config.sources.nyaa_rss_url,
            config.sources.grab_poll_interval_secs
        ],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
         VALUES (2, '1337x', 'scrape', ?1, ?2, 1)",
        rusqlite::params![
            config
                .sources
                .torrent_1337x_mirrors
                .first()
                .cloned()
                .unwrap_or_default(),
            config.sources.search_poll_interval_secs
        ],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO source (id, name, kind, base_url, poll_interval_secs, enabled)
         VALUES (3, 'tpb', 'scrape', ?1, ?2, 1)",
        rusqlite::params![
            config.sources.tpb_api_url,
            config.sources.search_poll_interval_secs
        ],
    )?;

    let qbit = QbitClient::new(config.qbit.base_url.clone())?;
    if !config.qbit.username.is_empty() {
        qbit.login(&config.qbit.username, &config.qbit.password)
            .await?;
    }
    let mut title_matcher = load_title_matcher(config).await?;
    let tpb_source = sources::tpb::TpbSource::new(config.sources.tpb_api_url.clone());
    let scrape_source =
        sources::scrape::ScrapeSource::new(config.sources.torrent_1337x_mirrors.clone());
    let nyaa_source = sources::rss::RssSource::new(config.sources.nyaa_rss_url.clone());

    let targets = scheduler::enumerate_search_targets_for_media_item(&conn, media_item_id)?;
    println!("{} missing episode(s)/movie for {title:?}", targets.len());

    let stats = scheduler::execute_search_targets(
        &conn,
        &targets,
        &tpb_source,
        3,
        &scrape_source,
        2,
        &nyaa_source,
        1,
        &mut title_matcher,
        &qbit,
        &config.qbit.category,
    )
    .await?;
    println!("{stats:?}");
    Ok(())
}

/// Dry-run report of what `reconcile_missing_files` would do against the
/// configured database, without writing anything — meant to be run by hand
/// against a freshly-restored or otherwise suspect database before trusting
/// the daemon's own hourly reconcile ticker to run unattended against it.
async fn debug_reconcile_report(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let outcome = importer::reconcile_missing_files(&conn, true)?;
    println!("{outcome:?}");
    if outcome.aborted {
        println!(
            "ABORTED: too many files looked missing at once (see logs) — do not run live \
             reconcile against this database until you've investigated why."
        );
    } else if outcome.cleared > 20 {
        println!(
            "NOTE: {} file(s) would be cleared as genuinely missing. Double-check this is \
             expected before starting the daemon normally.",
            outcome.cleared
        );
    }
    Ok(())
}

/// Lists qBittorrent torrents (optionally filtered by category) as
/// tab-separated hash/progress/state/name — read-only, and deliberately
/// prints nothing about how the client authenticated (credentials never
/// leave `QbitClient`). Meant for ad-hoc reconciliation between a `release`
/// row's `raw_title` and qBittorrent's own view when a hash needs to be
/// looked up or double-checked by hand.
async fn debug_qbit_list(config: &Config, category: Option<&str>) -> Result<()> {
    if config.qbit.base_url.is_empty() {
        bail!("qbit.base_url is not set in config");
    }
    let qbit = QbitClient::new(config.qbit.base_url.clone())?;
    if !config.qbit.username.is_empty() {
        qbit.login(&config.qbit.username, &config.qbit.password)
            .await?;
    }
    let torrents = qbit.list_torrents(category).await?;
    for t in &torrents {
        println!("{}\t{:.4}\t{}\t{}", t.hash, t.progress, t.state, t.name);
    }
    eprintln!("{} torrent(s)", torrents.len());
    Ok(())
}

/// Sweeps the whole library for files flagged `flag_non_english_default_audio`
/// (a non-English track set as default) and applies the same track-promotion
/// fix already used automatically right after a fresh download — but against
/// files already sitting in the library. Deliberately a manual command, not
/// wired to any ticker: unlike the read-mostly `probe_library` sweep, this
/// rewrites real files.
async fn remux_backlog_cmd(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let report = importer::remux_backlog(&conn)?;
    println!("{report:?}");
    Ok(())
}

/// Runs `probe_library` repeatedly until a full pass finds nothing left to
/// probe — the running daemon does this incrementally (bounded per hourly
/// tick, see `PROBE_SWEEP_BATCH_LIMIT`) so a large existing library backlog
/// doesn't stall the grab/import/search cycles; this command is for
/// immediately backfilling that same backlog by hand instead of waiting for
/// it to trickle in over several hours.
/// One-time reconciliation for episodes whose `episode_file` association
/// went missing (almost certainly the earlier DB-recovery incident) despite
/// their real file still sitting exactly where breadarr's own importer
/// would have put it — see `importer::find_relinkable_episode_files`'s doc
/// comment for the full story. Purely additive: reports what it found
/// before touching anything, never moves/deletes/overwrites a single file,
/// and flags ambiguous matches for a human to look at rather than guessing.
async fn relink_orphaned_files_cmd(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let (candidates, ambiguous) = importer::find_relinkable_episode_files(&conn)?;
    println!(
        "found {} orphaned episode file(s) to relink, {} ambiguous case(s) left for manual review",
        candidates.len(),
        ambiguous.len()
    );
    for c in &candidates {
        println!(
            "  relink: {} S{:02}E{:02} -> {}",
            c.series_title,
            c.season_number,
            c.episode_number,
            c.path.display()
        );
    }
    for a in &ambiguous {
        println!("  ambiguous, skipped: {a}");
    }

    let linked = importer::relink_episode_files(&conn, &candidates)?;
    println!("done: {linked} episode(s) relinked");
    Ok(())
}

async fn probe_library_cmd(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let mut total_probed = 0usize;
    let mut total_failed = 0usize;
    loop {
        let report = importer::probe_library(&conn)?;
        total_probed += report.probed;
        total_failed += report.failed;
        println!("batch: {report:?}");
        if report.probed == 0 {
            break;
        }
    }
    println!("done: {total_probed} probed total, {total_failed} failed total");
    Ok(())
}

/// One-time backfill: enqueues every existing-library file eligible for
/// AV1 transcoding (see `transcode::find_backlog_candidates` for the exact
/// eligibility rules — not already AV1, not anime, not HDR/2160p+ per the
/// first pass's scope) as a `transcode_job` row, then drives the same
/// worker loop the daemon's steady-state ticker uses
/// (`transcode::run_cycle`) until nothing is left pending. Shares that one
/// code path deliberately — there is exactly one place that actually runs
/// an encode, whether triggered by a backlog sweep or a fresh grab.
// Deliberately does NOT call `transcode::reset_orphaned_running_jobs` on
// startup the way `run_daemon` does — from this CLI's vantage point a
// `running` row could belong to the actual daemon's own ticker legitimately
// working on it right now (see the safety writeup on `claim_pending_jobs`:
// running this backfill alongside a live daemon with transcode enabled is
// intentionally supported, the two now share one atomic, count-aware
// concurrency cap), and there's no reliable way to tell that apart from a
// genuinely orphaned row from a ago-crashed run of this same command.
// If *this* command itself is killed mid-run, its claimed jobs stay
// `running` until the daemon is next restarted (which does its own reset).
async fn transcode_library_cmd(config: &Config) -> Result<()> {
    if !config.transcode.enabled {
        bail!("transcode.enabled is false in config — enable it before running a backfill");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let candidates = transcode::find_backlog_candidates(&conn, &config.transcode)?;
    println!(
        "found {} backlog candidate(s), highest-bitrate first",
        candidates.len()
    );
    for candidate in &candidates {
        transcode::enqueue(
            &conn,
            candidate.episode_file_id,
            candidate.video_codec.as_deref(),
            candidate.size_bytes,
            candidate.is_anime,
            false,
        )?;
    }

    drive_transcode_queue_to_completion(conn, config).await
}

/// Re-transcodes files a real rate-control bug left larger than their
/// original (already AV1, so `transcode-library`'s own backfill query
/// excludes them) — see `transcode::find_oversized_av1_candidates`'s doc
/// comment for the full story. Enqueues with `force_reencode: true`, the
/// only thing that lets `encode_and_verify` attempt a real re-encode of a
/// file that's already AV1 rather than treating it as nothing-to-do.
async fn retranscode_oversized_cmd(config: &Config) -> Result<()> {
    if !config.transcode.enabled {
        bail!("transcode.enabled is false in config — enable it before running this");
    }
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let candidates = transcode::find_oversized_av1_candidates(&conn, &config.transcode)?;
    println!(
        "found {} oversized AV1 file(s) to re-transcode, worst offenders first",
        candidates.len()
    );
    for candidate in &candidates {
        transcode::enqueue(
            &conn,
            candidate.episode_file_id,
            candidate.video_codec.as_deref(),
            candidate.size_bytes,
            candidate.is_anime,
            true,
        )?;
    }

    drive_transcode_queue_to_completion(conn, config).await
}

/// Shared by `transcode_library_cmd` and `retranscode_oversized_cmd`: drains
/// whatever's now `pending` in `transcode_job` via repeated `run_cycle`
/// calls until nothing is left, printing a running total. The only
/// difference between the two commands is which candidates got enqueued
/// (and with what `force_reencode` value) before this runs — there's
/// exactly one place that actually drives the worker loop to completion.
async fn drive_transcode_queue_to_completion(conn: Connection, config: &Config) -> Result<()> {
    let jellyfin = if config.jellyfin.base_url.is_empty() {
        None
    } else {
        Some(JellyfinClient::new(
            config.jellyfin.base_url.clone(),
            config.jellyfin.api_key.clone(),
        ))
    };

    let conn = std::sync::Arc::new(tokio::sync::Mutex::new(conn));
    let mut total_succeeded = 0usize;
    let mut total_skipped = 0usize;
    let mut total_failed = 0usize;
    let mut total_bytes_saved: i64 = 0;
    loop {
        let stats =
            transcode::run_cycle(conn.clone(), config.transcode.clone(), jellyfin.as_ref()).await?;
        if stats.attempted == 0 {
            break;
        }
        total_succeeded += stats.succeeded;
        total_skipped += stats.skipped;
        total_failed += stats.failed;
        total_bytes_saved += stats.bytes_saved;
        println!("batch: {stats:?}");
    }
    println!(
        "done: {total_succeeded} succeeded, {total_skipped} skipped (not beneficial), {total_failed} failed, {:.1} GB saved total",
        total_bytes_saved as f64 / 1_073_741_824.0
    );
    Ok(())
}

/// Runs `importer::verify_library` — the expensive full-decode corruption
/// check (`ffmpeg -xerror`, actually decoding every frame) against every
/// header-probed-ok file that hasn't been decode-verified yet. Unlike
/// `probe_library_cmd`, this deliberately doesn't loop to a fixed point:
/// each file can take minutes, so one full pass over whatever's currently
/// unverified is the whole point of a single invocation — run it again
/// later (or on a cron) to pick up files added since.
async fn verify_library_cmd(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(config.db_path())?;
    db::init(&conn)?;

    let report = importer::verify_library(&conn)?;
    println!("{report:?}");
    Ok(())
}

async fn wait_for_shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
