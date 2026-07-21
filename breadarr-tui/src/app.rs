use anyhow::Result;
use breadarr_shared::dto::{
    CalendarEntry, HealthDetail, LibraryHealthReport, MediaItemDetail, MediaItemSummary,
    QualityProfileSummary, ReleaseCandidate, ReleaseSummary, ReviewQueueEntry, SearchResult,
    StuckReport, WeightsDto,
};
use breadarr_shared::DaemonClient;
use ratatui::widgets::ListState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Library,
    History,
    Review,
    Add,
    Stuck,
    Calendar,
    LibraryHealth,
    Profiles,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::Library,
        Tab::History,
        Tab::Review,
        Tab::Add,
        Tab::Stuck,
        Tab::Calendar,
        Tab::LibraryHealth,
        Tab::Profiles,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Tab::Library => "Library",
            Tab::History => "History",
            Tab::Review => "Review Queue",
            Tab::Add => "Add Show",
            Tab::Stuck => "Stuck",
            Tab::Calendar => "Calendar",
            Tab::LibraryHealth => "Health",
            Tab::Profiles => "Profiles",
        }
    }
}

pub enum Focus {
    List,
    AddSearchInput,
    AddResults,
    /// The manual release picker overlay — `App::candidates` holds the
    /// list, `App::candidates_episode_id` remembers which episode (or, if
    /// `None`, the open movie) it was fetched for so a grab can be
    /// submitted against the right target.
    Candidates,
    /// Text-entry mode for one weight axis on the Profiles tab's detail
    /// view — `App::weight_input_buffer` holds the in-progress digits.
    WeightInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddKind {
    Series,
    Movie,
}

impl AddKind {
    pub fn label(&self) -> &'static str {
        match self {
            AddKind::Series => "TV",
            AddKind::Movie => "Movie",
        }
    }
}

/// Client-side filter over the already-fetched `media_items` — the server
/// has no filter/sort query params, and the library is small enough that
/// refiltering the in-memory `Vec` on every keypress is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryFilter {
    All,
    Series,
    Movies,
    Missing,
    Unmonitored,
}

impl LibraryFilter {
    pub const ALL: [LibraryFilter; 5] = [
        LibraryFilter::All,
        LibraryFilter::Series,
        LibraryFilter::Movies,
        LibraryFilter::Missing,
        LibraryFilter::Unmonitored,
    ];

    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn label(&self) -> &'static str {
        match self {
            LibraryFilter::All => "all",
            LibraryFilter::Series => "series",
            LibraryFilter::Movies => "movies",
            LibraryFilter::Missing => "missing",
            LibraryFilter::Unmonitored => "unmonitored",
        }
    }

    fn matches(&self, m: &MediaItemSummary) -> bool {
        match self {
            LibraryFilter::All => true,
            LibraryFilter::Series => m.kind == "series",
            LibraryFilter::Movies => m.kind == "movie",
            LibraryFilter::Missing => m.missing_count > 0,
            LibraryFilter::Unmonitored => !m.monitored,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySort {
    TitleAsc,
    MissingDesc,
    Kind,
}

impl LibrarySort {
    pub const ALL: [LibrarySort; 3] = [
        LibrarySort::TitleAsc,
        LibrarySort::MissingDesc,
        LibrarySort::Kind,
    ];

    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn label(&self) -> &'static str {
        match self {
            LibrarySort::TitleAsc => "title",
            LibrarySort::MissingDesc => "missing",
            LibrarySort::Kind => "kind",
        }
    }
}

/// Which half of the Stuck tab has selection/arrow-key focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckSection {
    Stalled,
    Maxed,
}

/// A weight axis's display name plus a getter/setter pair, so
/// `WEIGHT_FIELDS` can enumerate `WeightsDto`'s fields by position instead
/// of every caller matching on an index.
type WeightField = (
    &'static str,
    fn(&WeightsDto) -> f32,
    fn(&mut WeightsDto, f32),
);

/// One row per `WeightsDto` axis, in the fixed order shown/edited on the
/// Profiles tab.
pub const WEIGHT_FIELDS: [WeightField; 9] = [
    ("seeder", |w| w.seeder, |w, v| w.seeder = v),
    (
        "resolution_tier",
        |w| w.resolution_tier,
        |w, v| w.resolution_tier = v,
    ),
    ("source_tier", |w| w.source_tier, |w, v| w.source_tier = v),
    ("codec_tier", |w| w.codec_tier, |w, v| w.codec_tier = v),
    ("bit_depth", |w| w.bit_depth, |w, v| w.bit_depth = v),
    ("container", |w| w.container, |w, v| w.container = v),
    (
        "group_allowlist",
        |w| w.group_allowlist,
        |w, v| w.group_allowlist = v,
    ),
    ("repack", |w| w.repack, |w, v| w.repack = v),
    ("hdr", |w| w.hdr, |w, v| w.hdr = v),
];

/// A search hit tagged with which endpoint it came from — `run_add_search`
/// queries series and movies together (see its doc comment for why), so
/// each result needs to remember its own kind rather than the app tracking
/// one global mode.
#[derive(Debug, Clone)]
pub struct AddResult {
    pub kind: AddKind,
    pub result: SearchResult,
}

pub struct App {
    pub client: DaemonClient,
    pub daemon_up: bool,
    pub health: Option<HealthDetail>,
    pub tab: Tab,
    pub focus: Focus,
    pub status: String,
    pub should_quit: bool,
    /// Toggled by `?`; intercepted at the top of `main.rs`'s key handler
    /// like `Focus::AddSearchInput`/`Focus::WeightInput`, but kept as its
    /// own bool (not folded into `Focus`) since it needs to open from any
    /// tab rather than being scoped to one.
    pub help_visible: bool,
    /// Set by the `R` key; consumed by `main.rs`'s refresh loop to bypass
    /// the normal 3s throttle for one immediate refresh.
    pub force_refresh: bool,

    pub media_items: Vec<MediaItemSummary>,
    pub media_state: ListState,
    /// Filtered+sorted indices into `media_items`, recomputed by
    /// `recompute_library_view` — the top-level Library list and
    /// `media_state` always operate over this, never over `media_items`
    /// directly, so filter/sort never has to touch the raw fetched data.
    pub library_view: Vec<usize>,
    pub library_filter: LibraryFilter,
    pub library_sort: LibrarySort,
    pub detail: Option<MediaItemDetail>,
    /// Selection within `detail.episodes` — separate from `media_state`
    /// since they're two different lists sharing the same tab.
    pub episode_state: ListState,

    pub releases: Vec<ReleaseSummary>,
    pub releases_state: ListState,

    pub review_items: Vec<ReviewQueueEntry>,
    pub review_state: ListState,

    pub add_query: String,
    pub add_results: Vec<AddResult>,
    pub add_results_state: ListState,

    pub stuck: Option<StuckReport>,
    pub stuck_focus: StuckSection,
    pub stalled_state: ListState,
    pub maxed_state: ListState,
    pub calendar: Vec<CalendarEntry>,
    pub library_health: Option<LibraryHealthReport>,

    pub candidates: Vec<ReleaseCandidate>,
    pub candidates_state: ListState,
    /// Which episode the current `candidates` list was fetched for —
    /// `None` means it was fetched for the open movie itself.
    pub candidates_episode_id: Option<i64>,

    /// Set by a first `x` press in the Library detail view; a second press
    /// while this is `true` actually deletes. Any other key clears it. A
    /// lightweight guard against an accidental single keystroke deleting a
    /// tracked show/movie.
    pub confirm_delete: bool,

    /// Same two-press guard as `confirm_delete`, but for deleting an
    /// imported *file* (not the whole tracked show/movie) — a separate
    /// flag since the two actions have different confirmation text and
    /// shouldn't arm each other.
    pub confirm_delete_file: bool,

    pub quality_profiles: Vec<QualityProfileSummary>,
    pub profiles_state: ListState,
    /// The profile currently open for editing — separate from
    /// `quality_profiles` (like `detail` vs. `media_items`) so the two
    /// list levels (profiles, then that profile's weight axes) don't
    /// share a selection.
    pub profile_detail: Option<QualityProfileSummary>,
    pub profile_weight_state: ListState,
    /// In-progress digits while `Focus::WeightInput` is active.
    pub weight_input_buffer: String,
}

impl App {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client,
            daemon_up: false,
            health: None,
            tab: Tab::Library,
            focus: Focus::List,
            status: String::new(),
            should_quit: false,
            help_visible: false,
            force_refresh: false,
            media_items: Vec::new(),
            media_state: ListState::default(),
            library_view: Vec::new(),
            library_filter: LibraryFilter::All,
            library_sort: LibrarySort::TitleAsc,
            detail: None,
            episode_state: ListState::default(),
            releases: Vec::new(),
            releases_state: ListState::default(),
            review_items: Vec::new(),
            review_state: ListState::default(),
            add_query: String::new(),
            add_results: Vec::new(),
            add_results_state: ListState::default(),
            stuck: None,
            stuck_focus: StuckSection::Stalled,
            stalled_state: ListState::default(),
            maxed_state: ListState::default(),
            calendar: Vec::new(),
            library_health: None,
            candidates: Vec::new(),
            candidates_state: ListState::default(),
            candidates_episode_id: None,
            confirm_delete: false,
            confirm_delete_file: false,
            quality_profiles: Vec::new(),
            profiles_state: ListState::default(),
            profile_detail: None,
            profile_weight_state: ListState::default(),
            weight_input_buffer: String::new(),
        }
    }

    pub async fn refresh_active_tab(&mut self) {
        match self.client.health_detail().await {
            Ok(detail) => {
                self.daemon_up = true;
                self.health = Some(detail);
            }
            Err(_) => {
                self.daemon_up = false;
                self.health = None;
            }
        }
        if !self.daemon_up {
            self.status = "daemon unreachable".to_string();
            return;
        }
        let result: Result<()> = async {
            match self.tab {
                Tab::Library => {
                    if self.detail.is_some() {
                        if let Some(id) = self.selected_media_id() {
                            self.detail = Some(self.client.media_detail(id).await?);
                        }
                    } else {
                        self.media_items = self.client.list_media().await?;
                        self.recompute_library_view();
                    }
                }
                Tab::History => {
                    self.releases = self.client.releases().await?;
                    if self.releases_state.selected().is_none() && !self.releases.is_empty() {
                        self.releases_state.select(Some(0));
                    }
                }
                Tab::Review => {
                    self.review_items = self.client.review_queue().await?;
                    if self.review_state.selected().is_none() && !self.review_items.is_empty() {
                        self.review_state.select(Some(0));
                    }
                }
                Tab::Add => {}
                Tab::Stuck => {
                    let report = self.client.stuck().await?;
                    if self.stalled_state.selected().is_none() && !report.stalled_grabs.is_empty()
                    {
                        self.stalled_state.select(Some(0));
                    }
                    if self.maxed_state.selected().is_none()
                        && !report.maxed_out_search_targets.is_empty()
                    {
                        self.maxed_state.select(Some(0));
                    }
                    self.stuck = Some(report);
                }
                Tab::Calendar => {
                    self.calendar = self.client.calendar().await?;
                }
                Tab::LibraryHealth => {
                    self.library_health = Some(self.client.library_health().await?);
                }
                Tab::Profiles => {
                    if self.profile_detail.is_none() {
                        self.quality_profiles = self.client.quality_profiles().await?;
                        if self.profiles_state.selected().is_none()
                            && !self.quality_profiles.is_empty()
                        {
                            self.profiles_state.select(Some(0));
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            self.status = format!("error: {e}");
        }
    }

    fn selected_media_id(&self) -> Option<i64> {
        self.detail.as_ref().map(|d| d.id)
    }

    /// Rebuilds `library_view` from `media_items` under the current
    /// filter/sort, then re-selects whichever item was selected before (by
    /// id, not raw index) if it's still in view — falls back to the first
    /// item, or no selection if the view is now empty. Called after
    /// `media_items` changes, and after `library_filter`/`library_sort`
    /// change.
    pub fn recompute_library_view(&mut self) {
        let previously_selected_id = self.selected_media_item().map(|m| m.id);

        let mut indices: Vec<usize> = self
            .media_items
            .iter()
            .enumerate()
            .filter(|(_, m)| self.library_filter.matches(m))
            .map(|(i, _)| i)
            .collect();

        match self.library_sort {
            LibrarySort::TitleAsc => indices.sort_by(|&a, &b| {
                self.media_items[a]
                    .title
                    .to_lowercase()
                    .cmp(&self.media_items[b].title.to_lowercase())
            }),
            LibrarySort::MissingDesc => indices.sort_by(|&a, &b| {
                self.media_items[b]
                    .missing_count
                    .cmp(&self.media_items[a].missing_count)
            }),
            LibrarySort::Kind => indices.sort_by(|&a, &b| {
                self.media_items[a].kind.cmp(&self.media_items[b].kind).then_with(|| {
                    self.media_items[a]
                        .title
                        .to_lowercase()
                        .cmp(&self.media_items[b].title.to_lowercase())
                })
            }),
        }
        self.library_view = indices;

        match previously_selected_id
            .and_then(|id| self.library_view.iter().position(|&i| self.media_items[i].id == id))
        {
            Some(pos) => self.media_state.select(Some(pos)),
            None if !self.library_view.is_empty() => self.media_state.select(Some(0)),
            None => self.media_state.select(None),
        }
    }

    pub fn selected_media_item(&self) -> Option<&MediaItemSummary> {
        let idx = self.media_state.selected()?;
        let real_idx = *self.library_view.get(idx)?;
        self.media_items.get(real_idx)
    }

    pub fn move_selection(&mut self, delta: i32) {
        let (state, len) = match self.tab {
            Tab::Library if matches!(self.focus, Focus::Candidates) => {
                (&mut self.candidates_state, self.candidates.len())
            }
            Tab::Library if self.detail.is_none() => {
                (&mut self.media_state, self.library_view.len())
            }
            Tab::Library => (
                &mut self.episode_state,
                self.detail.as_ref().map_or(0, |d| d.episodes.len()),
            ),
            Tab::History => (&mut self.releases_state, self.releases.len()),
            Tab::Review => (&mut self.review_state, self.review_items.len()),
            Tab::Add => (&mut self.add_results_state, self.add_results.len()),
            Tab::Stuck => {
                let report_len = self.stuck.as_ref().map_or((0, 0), |r| {
                    (r.stalled_grabs.len(), r.maxed_out_search_targets.len())
                });
                match self.stuck_focus {
                    StuckSection::Stalled => (&mut self.stalled_state, report_len.0),
                    StuckSection::Maxed => (&mut self.maxed_state, report_len.1),
                }
            }
            Tab::Profiles if self.profile_detail.is_some() => {
                (&mut self.profile_weight_state, WEIGHT_FIELDS.len())
            }
            Tab::Profiles => (&mut self.profiles_state, self.quality_profiles.len()),
            _ => return,
        };
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, len as i32 - 1);
        state.select(Some(next as usize));
    }

    pub async fn open_detail(&mut self) {
        if !matches!(self.tab, Tab::Library) || self.detail.is_some() {
            return;
        }
        let Some(item) = self.selected_media_item() else {
            return;
        };
        match self.client.media_detail(item.id).await {
            Ok(detail) => {
                self.episode_state.select(if detail.episodes.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.detail = Some(detail);
            }
            Err(e) => self.status = format!("error loading detail: {e}"),
        }
    }

    pub fn close_detail(&mut self) {
        self.detail = None;
        self.episode_state.select(None);
    }

    /// Toggles monitored on whichever episode is currently selected in the
    /// open detail view.
    pub async fn toggle_monitor_selected_episode(&mut self) {
        let Some(detail) = &self.detail else {
            return;
        };
        let Some(idx) = self.episode_state.selected() else {
            return;
        };
        let Some(episode) = detail.episodes.get(idx) else {
            return;
        };
        let episode_id = episode.id;
        let result = if episode.monitored {
            self.client.unmonitor_episode(episode_id).await
        } else {
            self.client.monitor_episode(episode_id).await
        };
        match result {
            Ok(()) => {
                self.status = "episode monitor state updated".to_string();
                if let Some(id) = self.selected_media_id() {
                    if let Ok(fresh) = self.client.media_detail(id).await {
                        self.detail = Some(fresh);
                    }
                }
            }
            Err(e) => self.status = format!("episode monitor toggle failed: {e}"),
        }
    }

    /// Toggles monitored on every episode in the currently-selected
    /// episode's season at once.
    pub async fn toggle_monitor_selected_season(&mut self) {
        let Some(detail) = &self.detail else {
            return;
        };
        let Some(idx) = self.episode_state.selected() else {
            return;
        };
        let Some(episode) = detail.episodes.get(idx) else {
            return;
        };
        let (media_item_id, season_number, monitored) =
            (detail.id, episode.season_number as i64, episode.monitored);
        let result = if monitored {
            self.client
                .unmonitor_season(media_item_id, season_number)
                .await
        } else {
            self.client
                .monitor_season(media_item_id, season_number)
                .await
        };
        match result {
            Ok(()) => {
                self.status = format!("season {season_number} monitor state updated");
                if let Ok(fresh) = self.client.media_detail(media_item_id).await {
                    self.detail = Some(fresh);
                }
            }
            Err(e) => self.status = format!("season monitor toggle failed: {e}"),
        }
    }

    pub async fn approve_selected_review(&mut self) {
        let Some(idx) = self.review_state.selected() else {
            return;
        };
        let Some(item) = self.review_items.get(idx).cloned() else {
            return;
        };
        match self.client.approve_review(item.id).await {
            Ok(()) => self.status = format!("approved: {}", item.raw_release_title),
            Err(e) => self.status = format!("approve failed: {e}"),
        }
        self.review_items = self.client.review_queue().await.unwrap_or_default();
    }

    pub async fn reject_selected_review(&mut self) {
        let Some(idx) = self.review_state.selected() else {
            return;
        };
        let Some(item) = self.review_items.get(idx).cloned() else {
            return;
        };
        match self.client.reject_review(item.id).await {
            Ok(()) => self.status = format!("rejected: {}", item.raw_release_title),
            Err(e) => self.status = format!("reject failed: {e}"),
        }
        self.review_items = self.client.review_queue().await.unwrap_or_default();
    }

    /// Searches series and movies together instead of requiring the user to
    /// pick a mode first — a prior single-mode-plus-toggle design left the
    /// toggle bound to F2, which some terminals/window managers swallow
    /// before it ever reaches the TUI, silently stranding the search in the
    /// wrong mode with no visible error (a real, confusing dead end hit live:
    /// a movie search returned nothing because the mode had never actually
    /// switched). Querying both up front removes the failure mode entirely.
    pub async fn run_add_search(&mut self) {
        if self.add_query.trim().is_empty() {
            return;
        }
        let (series_result, movie_result) = tokio::join!(
            self.client.search_series(&self.add_query),
            self.client.search_movies(&self.add_query)
        );

        let mut results = Vec::new();
        let mut errors = Vec::new();
        match series_result {
            Ok(hits) => results.extend(hits.into_iter().map(|result| AddResult {
                kind: AddKind::Series,
                result,
            })),
            Err(e) => errors.push(format!("series search failed: {e}")),
        }
        match movie_result {
            Ok(hits) => results.extend(hits.into_iter().map(|result| AddResult {
                kind: AddKind::Movie,
                result,
            })),
            Err(e) => errors.push(format!("movie search failed: {e}")),
        }
        results.sort_by_key(|a| a.result.title.to_lowercase());

        self.add_results_state
            .select(if results.is_empty() { None } else { Some(0) });
        self.add_results = results;
        self.status = if errors.is_empty() {
            String::new()
        } else {
            errors.join("; ")
        };
        self.focus = Focus::AddResults;
    }

    pub async fn add_selected_search_result(&mut self, root_folder: &str) {
        let Some(idx) = self.add_results_state.selected() else {
            return;
        };
        let Some(AddResult { kind, result }) = self.add_results.get(idx).cloned() else {
            return;
        };
        let outcome = match kind {
            AddKind::Series => {
                let req = breadarr_shared::dto::AddSeriesRequest {
                    tvdb_id: result.external_id.clone(),
                    title: result.title.clone(),
                    year: result.year,
                    aliases: Vec::new(),
                    root_folder: root_folder.to_string(),
                };
                self.client.add_series(&req).await.map(|r| r.media_item_id)
            }
            AddKind::Movie => {
                let req = breadarr_shared::dto::AddMovieRequest {
                    tmdb_id: result.external_id.clone(),
                    title: result.title.clone(),
                    year: result.year,
                    root_folder: root_folder.to_string(),
                };
                self.client.add_movie(&req).await.map(|r| r.media_item_id)
            }
        };
        match outcome {
            Ok(media_item_id) => {
                self.status = format!("added {:?} (media_item_id={media_item_id})", result.title);
                self.add_results.clear();
                self.add_query.clear();
                self.focus = Focus::AddSearchInput;
            }
            Err(e) => self.status = format!("add failed: {e}"),
        }
    }

    /// Manual "search now" for the show/movie currently open in the Library
    /// detail view — can take a while (jittered, one request per missing
    /// item), so the status line makes that explicit rather than looking
    /// like the UI hung.
    pub async fn search_now_selected(&mut self) {
        let Some(id) = self.detail.as_ref().map(|d| d.id) else {
            return;
        };
        self.status = "searching now (this can take a while)...".to_string();
        match self.client.search_now(id).await {
            Ok(stats) => {
                self.status = format!(
                    "search complete: {} target(s), {} grabbed, {} error(s)",
                    stats.targets, stats.grabbed, stats.errors
                );
            }
            Err(e) => self.status = format!("search failed: {e}"),
        }
    }

    /// Fetches candidates for whatever's selected in the open detail view —
    /// the currently-highlighted episode, or the movie itself if there's no
    /// episode list. Can take a while, same as `search_now_selected`, since
    /// it runs a real search under the hood.
    pub async fn fetch_candidates_for_selected(&mut self) {
        let Some(detail) = &self.detail else {
            return;
        };
        let episode_id = if detail.kind == "movie" {
            None
        } else {
            let Some(idx) = self.episode_state.selected() else {
                return;
            };
            let Some(episode) = detail.episodes.get(idx) else {
                return;
            };
            Some(episode.id)
        };
        let media_item_id = detail.id;

        self.status = "fetching candidates (this can take a while)...".to_string();
        let result = match episode_id {
            Some(id) => self.client.episode_candidates(id).await,
            None => self.client.movie_candidates(media_item_id).await,
        };
        match result {
            Ok(candidates) => {
                self.status = format!("{} candidate(s) found", candidates.len());
                self.candidates_state
                    .select(if candidates.is_empty() { None } else { Some(0) });
                self.candidates = candidates;
                self.candidates_episode_id = episode_id;
                self.focus = Focus::Candidates;
            }
            Err(e) => self.status = format!("candidate fetch failed: {e}"),
        }
    }

    /// Grabs whichever candidate is currently selected in the picker
    /// overlay, then returns to the normal detail view and refreshes it.
    pub async fn grab_selected_candidate(&mut self) {
        let Some(idx) = self.candidates_state.selected() else {
            return;
        };
        let Some(candidate) = self.candidates.get(idx).cloned() else {
            return;
        };
        let Some(media_item_id) = self.detail.as_ref().map(|d| d.id) else {
            return;
        };
        let result = match self.candidates_episode_id {
            Some(episode_id) => {
                self.client
                    .grab_episode_candidate(episode_id, &candidate)
                    .await
            }
            None => {
                self.client
                    .grab_movie_candidate(media_item_id, &candidate)
                    .await
            }
        };
        match result {
            Ok(()) => {
                self.status = format!("grabbed: {}", candidate.raw_title);
                self.close_candidates();
                if let Ok(fresh) = self.client.media_detail(media_item_id).await {
                    self.detail = Some(fresh);
                }
            }
            Err(e) => self.status = format!("grab failed: {e}"),
        }
    }

    pub fn close_candidates(&mut self) {
        self.candidates.clear();
        self.candidates_state.select(None);
        self.candidates_episode_id = None;
        self.focus = Focus::List;
    }

    pub fn open_profile_detail(&mut self) {
        if !matches!(self.tab, Tab::Profiles) || self.profile_detail.is_some() {
            return;
        }
        let Some(idx) = self.profiles_state.selected() else {
            return;
        };
        let Some(profile) = self.quality_profiles.get(idx).cloned() else {
            return;
        };
        self.profile_detail = Some(profile);
        self.profile_weight_state.select(Some(0));
    }

    pub fn close_profile_detail(&mut self) {
        self.profile_detail = None;
        self.profile_weight_state.select(None);
        self.weight_input_buffer.clear();
        self.focus = Focus::List;
    }

    /// Seeds the edit buffer from the currently-selected weight axis's
    /// value and switches focus into text-entry mode — mirrors the Add
    /// tab's search-input pattern (`Focus::AddSearchInput`), just for a
    /// numeric field instead of a search query.
    pub fn start_editing_selected_weight(&mut self) {
        let Some(profile) = &self.profile_detail else {
            return;
        };
        let Some(idx) = self.profile_weight_state.selected() else {
            return;
        };
        let Some((_, get, _)) = WEIGHT_FIELDS.get(idx) else {
            return;
        };
        self.weight_input_buffer = format!("{}", get(&profile.weights));
        self.focus = Focus::WeightInput;
    }

    pub fn cancel_weight_edit(&mut self) {
        self.weight_input_buffer.clear();
        self.focus = Focus::List;
    }

    /// Parses the edit buffer, applies it to the in-memory profile, and
    /// submits the *complete* resolved weights set to the daemon — the API
    /// always overwrites the full stored JSON (see `update_weights`'s doc
    /// comment server-side), so every other axis has to be sent along
    /// unchanged, not just the one being edited.
    pub async fn commit_weight_edit(&mut self) {
        let Some(idx) = self.profile_weight_state.selected() else {
            return;
        };
        let Some((name, _, set)) = WEIGHT_FIELDS.get(idx) else {
            return;
        };
        let value: f32 = match self.weight_input_buffer.trim().parse() {
            Ok(v) => v,
            Err(_) => {
                self.status = format!("'{}' is not a valid number", self.weight_input_buffer);
                return;
            }
        };
        let Some(profile) = &mut self.profile_detail else {
            return;
        };
        set(&mut profile.weights, value);
        let profile_id = profile.id;
        let weights = profile.weights;

        match self
            .client
            .update_quality_profile_weights(profile_id, weights)
            .await
        {
            Ok(()) => {
                self.status = format!("{name} updated to {value}");
                self.weight_input_buffer.clear();
                self.focus = Focus::List;
            }
            Err(e) => self.status = format!("weight update failed: {e}"),
        }
    }

    /// Toggles monitored for whatever's selected in the top-level Library
    /// list, without needing to open its detail view first.
    pub async fn toggle_monitor_list_selected(&mut self) {
        let Some(item) = self.selected_media_item() else {
            return;
        };
        let (id, monitored) = (item.id, item.monitored);
        let result = if monitored {
            self.client.unmonitor(id).await
        } else {
            self.client.monitor(id).await
        };
        match result {
            Ok(()) => {
                self.status = "monitor state updated".to_string();
                self.media_items = self.client.list_media().await.unwrap_or_default();
                self.recompute_library_view();
            }
            Err(e) => self.status = format!("monitor toggle failed: {e}"),
        }
    }

    pub async fn toggle_monitor_selected(&mut self) {
        let Some(detail) = &self.detail else {
            return;
        };
        let id = detail.id;
        let result = if detail.monitored {
            self.client.unmonitor(id).await
        } else {
            self.client.monitor(id).await
        };
        match result {
            Ok(()) => {
                self.status = "monitor state updated".to_string();
                if let Ok(fresh) = self.client.media_detail(id).await {
                    self.detail = Some(fresh);
                }
            }
            Err(e) => self.status = format!("monitor toggle failed: {e}"),
        }
    }

    /// First call arms the confirmation; a second call while armed actually
    /// deletes. Any other keypress (see `main.rs`) clears the pending state.
    pub async fn delete_selected(&mut self) {
        let Some(id) = self.detail.as_ref().map(|d| d.id) else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            self.status = "press x again to confirm delete".to_string();
            return;
        }
        self.confirm_delete = false;
        match self.client.delete_media(id).await {
            Ok(()) => {
                self.status = "deleted".to_string();
                self.detail = None;
                self.media_items = self.client.list_media().await.unwrap_or_default();
                self.recompute_library_view();
            }
            Err(e) => self.status = format!("delete failed: {e}"),
        }
    }

    /// Deletes the imported file for whatever's selected — the movie
    /// itself if the open detail is a movie, or the currently-selected
    /// episode otherwise — freeing it to be grabbed again. Same
    /// arm-then-confirm pattern as `delete_selected`, via
    /// `confirm_delete_file` instead so the two don't cross-arm.
    pub async fn delete_selected_file(&mut self) {
        let Some(detail) = &self.detail else {
            return;
        };
        if !self.confirm_delete_file {
            self.confirm_delete_file = true;
            self.status = "press d again to confirm deleting this file".to_string();
            return;
        }
        self.confirm_delete_file = false;

        let result = if detail.kind == "movie" {
            self.client.delete_movie_file(detail.id).await
        } else {
            let Some(idx) = self.episode_state.selected() else {
                return;
            };
            let Some(episode) = detail.episodes.get(idx) else {
                return;
            };
            self.client.delete_episode_file(episode.id).await
        };
        match result {
            Ok(()) => {
                self.status = "file deleted, will be re-searched".to_string();
                if let Some(id) = self.selected_media_id() {
                    if let Ok(fresh) = self.client.media_detail(id).await {
                        self.detail = Some(fresh);
                    }
                }
            }
            Err(e) => self.status = format!("file delete failed: {e}"),
        }
    }

    /// Jumps from whichever Stuck-tab row is selected straight to that
    /// show's Library detail view.
    pub async fn jump_to_stuck_target(&mut self) {
        let Some(report) = &self.stuck else {
            return;
        };
        let media_item_id = match self.stuck_focus {
            StuckSection::Maxed => self
                .maxed_state
                .selected()
                .and_then(|i| report.maxed_out_search_targets.get(i))
                .map(|t| t.media_item_id),
            StuckSection::Stalled => self
                .stalled_state
                .selected()
                .and_then(|i| report.stalled_grabs.get(i))
                .map(|g| g.media_item_id),
        };
        let Some(id) = media_item_id else {
            return;
        };
        match self.client.media_detail(id).await {
            Ok(detail) => {
                self.tab = Tab::Library;
                self.episode_state.select(if detail.episodes.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.detail = Some(detail);
                self.focus = Focus::List;
            }
            Err(e) => self.status = format!("error loading detail: {e}"),
        }
    }
}
