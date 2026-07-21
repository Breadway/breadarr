use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs};
use ratatui::Frame;

use crate::app::{App, Focus, StuckSection, Tab};

// Shared color palette — kept to these meanings so a color never has to be
// second-guessed at a glance:
//   Green      healthy / 0 missing / high confidence / not stuck
//   Yellow     warning / low missing / mid confidence / at backoff ceiling
//   Red        critical / high missing / low confidence / past ceiling
//   DarkGray   unmonitored / muted / not currently relevant
//   Blue       TV kind tag (categorical, not severity)
//   Magenta    Movie kind tag (categorical, not severity)
//   Cyan       focus/interactive accent (tab highlight, active input, focused section) — never severity

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

    if app.help_visible {
        draw_help_overlay(frame, frame.area(), app);
    }
}

/// Keybindings relevant to the current tab/focus/detail state, in the order
/// they should be shown — the single source of truth shared by the status
/// bar (which shows a short prefix) and the help overlay (which shows all of
/// it plus `GLOBAL_KEYS`), so the two can't drift apart.
fn context_keybindings(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.tab {
        Tab::Library if matches!(app.focus, Focus::Candidates) => {
            vec![("Enter", "grab"), ("Esc", "cancel")]
        }
        Tab::Library if app.detail.is_some() => vec![
            ("Esc", "back"),
            ("s", "search now"),
            ("m", "monitor show"),
            ("e", "monitor episode"),
            ("S", "monitor season"),
            ("x", "delete show"),
            ("d", "delete file"),
            ("c", "pick release"),
        ],
        Tab::Library => vec![
            ("Enter", "open detail"),
            ("f", "cycle filter"),
            ("o", "cycle sort"),
            ("m", "monitor toggle"),
        ],
        Tab::Review => vec![("a", "approve"), ("r", "reject")],
        Tab::Add => match app.focus {
            Focus::AddSearchInput => vec![("Enter", "search")],
            _ => vec![("Enter", "add"), ("Esc", "back to search")],
        },
        Tab::Profiles if app.profile_detail.is_some() => {
            vec![("Enter", "edit weight"), ("Esc", "back")]
        }
        Tab::Profiles => vec![("Enter", "open profile")],
        Tab::Stuck => vec![("Left/Right", "switch section"), ("Enter", "jump to show")],
        _ => vec![],
    }
}

const GLOBAL_KEYS: &[(&str, &str)] = &[
    ("Tab", "switch tab"),
    ("j/k", "move"),
    ("?", "help"),
    ("R", "refresh now"),
    ("q", "quit"),
];

/// Centers a `width` x `height` rect inside `area` — standard ratatui idiom
/// for a popup/overlay.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn draw_help_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = context_keybindings(app)
        .into_iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{key:12}"), Style::default().fg(Color::Cyan)),
                Span::raw(desc),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Global",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.extend(GLOBAL_KEYS.iter().map(|(key, desc)| {
        Line::from(vec![
            Span::styled(format!("{key:12}"), Style::default().fg(Color::Cyan)),
            Span::raw(*desc),
        ])
    }));

    let popup = centered_rect(50, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Help — ? or Esc to close"),
    );
    frame.render_widget(paragraph, popup);
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
                let (status, color) = if e.has_file {
                    ("✓", Color::Green)
                } else if e.monitored {
                    ("…", Color::Yellow)
                } else {
                    ("-", Color::DarkGray)
                };
                let title = e.title.as_deref().unwrap_or("");
                ListItem::new(Line::from(vec![
                    Span::styled(status, Style::default().fg(color)),
                    Span::raw(format!(
                        " S{:02}E{:02}  {title}",
                        e.season_number, e.episode_number
                    )),
                ]))
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
                "{} ({}) [{monitor_label}]{confirm}",
                detail.title,
                detail.year.map(|y| y.to_string()).unwrap_or_default()
            )))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = app.episode_state.clone();
        frame.render_stateful_widget(list, area, &mut state);
        return;
    }

    let items: Vec<ListItem> = app
        .library_view
        .iter()
        .map(|&i| {
            let m = &app.media_items[i];
            let kind_tag = if m.kind == "movie" { "[Movie]" } else { "[TV]" };
            let kind_color = if m.kind == "movie" {
                Color::Magenta
            } else {
                Color::Blue
            };
            let ratio = if m.episode_count > 0 {
                m.missing_count as f64 / m.episode_count as f64
            } else if m.missing_count > 0 {
                1.0
            } else {
                0.0
            };
            // Mute severity color for unmonitored items — a missing count
            // on something deliberately unmonitored isn't actionable.
            let missing_color = if !m.monitored || m.missing_count == 0 {
                Color::DarkGray
            } else if ratio <= 0.25 {
                Color::Yellow
            } else {
                Color::Red
            };
            let missing_text = if m.missing_count > 0 {
                format!(" — {} missing", m.missing_count)
            } else {
                String::new()
            };
            let title_style = if m.monitored {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{kind_tag} "), Style::default().fg(kind_color)),
                Span::styled(
                    format!(
                        "{} ({})",
                        m.title,
                        m.year.map(|y| y.to_string()).unwrap_or_default()
                    ),
                    title_style,
                ),
                Span::styled(missing_text, Style::default().fg(missing_color)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Monitored Shows ({}/{}) — filter: {}  sort: {}",
            app.library_view.len(),
            app.media_items.len(),
            app.library_filter.label(),
            app.library_sort.label()
        )))
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
            let color = if r.confidence < 0.70 {
                Color::Red
            } else if r.confidence < 0.85 {
                Color::Yellow
            } else {
                Color::Green
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("({:.0}%)", r.confidence * 100.0), Style::default().fg(color)),
                Span::raw(format!(
                    " {} -> {}",
                    r.raw_release_title,
                    r.candidate_media_title.as_deref().unwrap_or("?")
                )),
            ]))
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
// Mirrors breadarrd/src/api/routes/stuck.rs::MAXED_SEARCH_COUNT. Duplicated
// because the daemon doesn't expose it via the API; if it drifts this is
// cosmetic only (wrong shade), not a behavior bug.
const MAXED_SEARCH_COUNT: i64 = 6;

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

    let stalled_border = if matches!(app.stuck_focus, StuckSection::Stalled) {
        Color::Cyan
    } else {
        Color::Reset
    };
    let stalled_items: Vec<ListItem> = report
        .stalled_grabs
        .iter()
        .map(|g| {
            ListItem::new(Line::from(Span::styled(
                format!("{} — {} (grabbed {})", g.media_title, g.raw_title, g.grabbed_at),
                Style::default().fg(Color::Yellow),
            )))
        })
        .collect();
    let stalled_list = List::new(stalled_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(stalled_border))
                .title(format!(
                    "Stalled grabs ({}) — review queue: {} pending",
                    report.stalled_grabs.len(),
                    report.review_queue_depth
                )),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut stalled_state = app.stalled_state.clone();
    frame.render_stateful_widget(stalled_list, chunks[0], &mut stalled_state);

    let maxed_border = if matches!(app.stuck_focus, StuckSection::Maxed) {
        Color::Cyan
    } else {
        Color::Reset
    };
    let maxed_items: Vec<ListItem> = report
        .maxed_out_search_targets
        .iter()
        .map(|t| {
            let color = if t.search_count > MAXED_SEARCH_COUNT {
                Color::Red
            } else {
                Color::Yellow
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{} — {} attempts, last searched {}",
                    t.media_title,
                    t.search_count,
                    t.last_searched_at.as_deref().unwrap_or("never")
                ),
                Style::default().fg(color),
            )))
        })
        .collect();
    let maxed_list = List::new(maxed_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(maxed_border))
                .title("Search targets at max backoff — Enter: jump to show"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut maxed_state = app.maxed_state.clone();
    frame.render_stateful_widget(maxed_list, chunks[1], &mut maxed_state);
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
    let text = if !app.status.is_empty() {
        app.status.clone()
    } else {
        let hints: Vec<String> = context_keybindings(app)
            .into_iter()
            .take(4)
            .map(|(key, desc)| format!("{key}: {desc}"))
            .collect();
        format!("{}  |  ?: help", hints.join("  |  "))
    };
    let status = Paragraph::new(text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(status, area);
}
