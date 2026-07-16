use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs};
use ratatui::Frame;

use crate::app::{App, Focus, Tab};

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(frame.area());

    draw_tabs(frame, chunks[0], app);

    match app.tab {
        Tab::Library => draw_library(frame, chunks[1], app),
        Tab::History => draw_history(frame, chunks[1], app),
        Tab::Review => draw_review(frame, chunks[1], app),
        Tab::Add => draw_add(frame, chunks[1], app),
        Tab::Stuck => draw_stuck(frame, chunks[1], app),
        Tab::Calendar => draw_calendar(frame, chunks[1], app),
        Tab::LibraryHealth => draw_library_health(frame, chunks[1], app),
        Tab::Profiles => draw_profiles(frame, chunks[1], app),
    }

    draw_status(frame, chunks[2], app);
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = Tab::ALL.iter().map(|t| Line::from(t.title())).collect();
    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);

    let (daemon_label, daemon_color) = match (&app.daemon_up, &app.health) {
        (true, Some(h)) if h.search_halted => {
            ("daemon: UP — ⚠ search halted".to_string(), Color::Yellow)
        }
        (true, Some(h)) => {
            let any_failed = [
                &h.last_grab_cycle,
                &h.last_import_cycle,
                &h.last_search_cycle,
                &h.last_upgrade_cycle,
            ]
            .into_iter()
            .flatten()
            .any(|c| !c.ok);
            if any_failed {
                (
                    "daemon: UP — last cycle had errors".to_string(),
                    Color::Yellow,
                )
            } else {
                ("daemon: UP".to_string(), Color::Green)
            }
        }
        (true, None) => ("daemon: UP".to_string(), Color::Green),
        (false, _) => ("daemon: DOWN".to_string(), Color::Red),
    };

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            daemon_label,
            Style::default().fg(daemon_color),
        )))
        .select(selected)
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
    frame.render_widget(tabs, area);
}

fn draw_library(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(detail) = &app.detail {
        if matches!(app.focus, Focus::Candidates) {
            draw_candidates(frame, area, app, &detail.title);
            return;
        }

        let items: Vec<ListItem> = detail
            .episodes
            .iter()
            .map(|e| {
                let status = if e.has_file {
                    "✓"
                } else if e.monitored {
                    "…"
                } else {
                    "-"
                };
                let title = e.title.as_deref().unwrap_or("");
                ListItem::new(format!(
                    "{status} S{:02}E{:02}  {title}",
                    e.season_number, e.episode_number
                ))
            })
            .collect();
        let monitor_label = if detail.monitored {
            "monitored"
        } else {
            "unmonitored"
        };
        let confirm = if app.confirm_delete {
            " — x AGAIN TO DELETE SHOW"
        } else if app.confirm_delete_file {
            " — d AGAIN TO DELETE FILE"
        } else {
            ""
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(format!(
                "{} ({}) [{monitor_label}] — Esc: back  s: search now  m: monitor show  \
                 e: monitor episode  S: monitor season  x: delete show  d: delete file  \
                 c: pick release{confirm}",
                detail.title,
                detail.year.map(|y| y.to_string()).unwrap_or_default()
            )))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = app.episode_state.clone();
        frame.render_stateful_widget(list, area, &mut state);
        return;
    }

    let items: Vec<ListItem> = app
        .media_items
        .iter()
        .map(|m| {
            let missing = if m.missing_count > 0 {
                format!(" — {} missing", m.missing_count)
            } else {
                String::new()
            };
            ListItem::new(format!(
                "{} ({}){}",
                m.title,
                m.year.map(|y| y.to_string()).unwrap_or_default(),
                missing
            ))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Monitored Shows — Enter for detail"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.media_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

/// Manual release picker — candidates for whatever episode/movie was
/// selected when `c` was pressed, scored (or gate-rejected with a reason)
/// exactly like the automatic search pipeline would see them.
fn draw_candidates(frame: &mut Frame, area: Rect, app: &App, media_title: &str) {
    if app.candidates.is_empty() {
        let placeholder = Paragraph::new("loading candidates...").block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("{media_title} — candidates — Esc: cancel")),
        );
        frame.render_widget(placeholder, area);
        return;
    }

    let items: Vec<ListItem> = app
        .candidates
        .iter()
        .map(|c| {
            let size = c
                .size_bytes
                .map(|b| format!("{:.2} GB", b as f64 / 1_073_741_824.0))
                .unwrap_or_else(|| "?".to_string());
            let seeders = c
                .seeders
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".to_string());
            let flags = format!(
                "{}{}",
                if c.is_season_pack { " [PACK]" } else { "" },
                if c.is_repack { " [REPACK]" } else { "" },
            );
            let verdict = match (c.score, &c.rejected_reason) {
                (Some(score), _) => format!("score {score:.1}"),
                (None, Some(reason)) => format!("REJECTED: {reason}"),
                (None, None) => "unscored".to_string(),
            };
            ListItem::new(format!(
                "[{}] {} — {seeders} seeders, {size}{flags} — {verdict}",
                c.source_name, c.raw_title
            ))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "{media_title} — candidates — Enter: grab  Esc: cancel"
        )))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.candidates_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_history(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .releases
        .iter()
        .map(|r| {
            ListItem::new(format!(
                "[{}] {} — {} (score {:.1})",
                r.status,
                r.media_title,
                r.raw_title,
                r.score.unwrap_or(0.0)
            ))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Grab History"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.releases_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_review(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .review_items
        .iter()
        .map(|r| {
            ListItem::new(format!(
                "({:.0}%) {} -> {}",
                r.confidence * 100.0,
                r.raw_release_title,
                r.candidate_media_title.as_deref().unwrap_or("?")
            ))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Review Queue — a: approve, r: reject"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.review_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

/// "Why don't I have this yet" — grabs sitting without importing longer
/// than expected, how deep the review queue has backed up, and search
/// targets that have been failing every attempt long enough for their
/// backoff to hit its ceiling. Read-only report, no selection/navigation.
fn draw_stuck(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.stuck else {
        let placeholder = Paragraph::new("loading...").block(
            Block::default()
                .borders(Borders::ALL)
                .title("Stuck — why don't I have this yet"),
        );
        frame.render_widget(placeholder, area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Min(3)])
        .split(area);

    let stalled_items: Vec<ListItem> = report
        .stalled_grabs
        .iter()
        .map(|g| {
            ListItem::new(format!(
                "{} — {} (grabbed {})",
                g.media_title, g.raw_title, g.grabbed_at
            ))
        })
        .collect();
    let stalled_list =
        List::new(stalled_items).block(Block::default().borders(Borders::ALL).title(format!(
            "Stalled grabs ({}) — review queue: {} pending",
            report.stalled_grabs.len(),
            report.review_queue_depth
        )));
    frame.render_widget(stalled_list, chunks[0]);

    let maxed_items: Vec<ListItem> = report
        .maxed_out_search_targets
        .iter()
        .map(|t| {
            ListItem::new(format!(
                "{} — {} attempts, last searched {}",
                t.media_title,
                t.search_count,
                t.last_searched_at.as_deref().unwrap_or("never")
            ))
        })
        .collect();
    let maxed_list = List::new(maxed_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Search targets at max backoff"),
    );
    frame.render_widget(maxed_list, chunks[1]);
}

fn draw_add(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let input_style = match app.focus {
        Focus::AddSearchInput => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    };
    let input = Paragraph::new(app.add_query.as_str())
        .style(input_style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Search — Enter to search movies and TV together"),
        );
    frame.render_widget(input, chunks[0]);

    let items: Vec<ListItem> = app
        .add_results
        .iter()
        .map(|r| {
            ListItem::new(format!(
                "[{}] {} ({})",
                r.kind.label(),
                r.result.title,
                r.result.year.map(|y| y.to_string()).unwrap_or_default()
            ))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Results — Enter to add"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.add_results_state.clone();
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

/// What's aired recently or airs soon (a week back, three weeks forward —
/// see `calendar::DAYS_PAST`/`DAYS_FUTURE` server-side). Read-only, no
/// selection — a lookahead view, not something acted on directly here.
fn draw_calendar(frame: &mut Frame, area: Rect, app: &App) {
    let today = chrono::Local::now().date_naive().to_string();
    let items: Vec<ListItem> = app
        .calendar
        .iter()
        .map(|e| {
            let status = if e.has_file {
                "✓"
            } else if !e.monitored {
                "-"
            } else if e.air_date.as_str() > today.as_str() {
                "…"
            } else {
                "!" // aired, monitored, still missing
            };
            let title = e.title.as_deref().unwrap_or("");
            ListItem::new(format!(
                "{status} {}  {}  S{:02}E{:02}  {title}",
                e.air_date, e.media_title, e.season_number, e.episode_number
            ))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Calendar — ✓ have it  … upcoming  ! aired but missing"),
    );
    frame.render_widget(list, area);
}

/// Read-only report on `media_file_probe` state: corruption, under-quality,
/// missing-subtitle/English-audio, non-English-default-audio, and duplicate
/// files, plus a library-wide summary — no selection/navigation, same shape
/// as `draw_stuck`.
fn draw_library_health(frame: &mut Frame, area: Rect, app: &App) {
    let Some(report) = &app.library_health else {
        let placeholder = Paragraph::new("loading...").block(
            Block::default()
                .borders(Borders::ALL)
                .title("Library Health"),
        );
        frame.render_widget(placeholder, area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(area);

    let s = &report.summary;
    let codec_summary = s
        .by_video_codec
        .iter()
        .map(|c| format!("{}={}", c.codec, c.count))
        .collect::<Vec<_>>()
        .join(", ");
    let summary_text = format!(
        "{} files, {:.1} GB total, {} probed — resolution: SD={} 720p={} 1080p={} 4K={} \
         — subtitles: {:.0}% — codecs: {codec_summary}",
        s.total_files,
        s.total_size_bytes as f64 / 1_073_741_824.0,
        s.probed_files,
        s.sd_count,
        s.hd_720p_count,
        s.full_hd_1080p_count,
        s.uhd_4k_count,
        s.pct_with_subtitles,
    );
    let summary = Paragraph::new(summary_text)
        .wrap(ratatui::widgets::Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Library summary"),
        );
    frame.render_widget(summary, chunks[0]);

    let mut items: Vec<ListItem> = Vec::new();
    let mut section = |label: &str, files: &[breadarr_shared::dto::FlaggedFile]| {
        if files.is_empty() {
            return;
        }
        items.push(ListItem::new(Span::styled(
            format!("── {label} ({}) ──", files.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for f in files {
            let label = f.episode_label.as_deref().unwrap_or("");
            items.push(ListItem::new(format!(
                "  {} {label} — {}",
                f.media_title, f.path
            )));
        }
    };
    section("Corrupt / unreadable", &report.corrupt_files);
    section("Under 1080p", &report.under_quality_files);
    section("No English audio", &report.no_english_audio_files);
    section(
        "Non-English default audio",
        &report.non_english_default_audio_files,
    );
    section("No subtitles", &report.no_subtitle_files);

    if !report.duplicate_groups.is_empty() {
        items.push(ListItem::new(Span::styled(
            format!("── Duplicate files ({}) ──", report.duplicate_groups.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for g in &report.duplicate_groups {
            let label = g.episode_label.as_deref().unwrap_or("");
            items.push(ListItem::new(format!(
                "  {} {label} — {} copies",
                g.media_title,
                g.paths.len()
            )));
        }
    }

    if items.is_empty() {
        items.push(ListItem::new("Nothing flagged — library looks clean."));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Flagged files"),
    );
    frame.render_widget(list, chunks[1]);
}

/// Quality-profile weight editing — list of profiles, then (once one is
/// opened) a flat list of its 9 scoring axes with an inline text-input
/// overlay while `Focus::WeightInput` is active.
fn draw_profiles(frame: &mut Frame, area: Rect, app: &App) {
    let Some(profile) = &app.profile_detail else {
        let items: Vec<ListItem> = app
            .quality_profiles
            .iter()
            .map(|p| ListItem::new(format!("{} ({})", p.name, p.kind)))
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Quality Profiles — Enter: edit weights"),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = app.profiles_state.clone();
        frame.render_stateful_widget(list, area, &mut state);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let editing_label = app
        .profile_weight_state
        .selected()
        .and_then(|idx| crate::app::WEIGHT_FIELDS.get(idx))
        .map(|(name, ..)| *name)
        .unwrap_or("");
    let input_style = match app.focus {
        Focus::WeightInput => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    };
    let input = Paragraph::new(app.weight_input_buffer.as_str())
        .style(input_style)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Editing {editing_label} — Enter: save  Esc: cancel"
        )));
    frame.render_widget(input, chunks[0]);

    let items: Vec<ListItem> = crate::app::WEIGHT_FIELDS
        .iter()
        .map(|(name, get, _)| ListItem::new(format!("{name}: {}", get(&profile.weights))))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "{} — Enter: edit selected  Esc: back",
            profile.name
        )))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.profile_weight_state.clone();
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let text = if app.status.is_empty() {
        "Tab: switch view  |  j/k: move  |  q: quit".to_string()
    } else {
        app.status.clone()
    };
    let status = Paragraph::new(text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(status, area);
}
