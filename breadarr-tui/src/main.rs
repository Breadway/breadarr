mod app;
mod ui;

use std::io;
use std::time::Duration;

use anyhow::Result;
use breadarr_shared::{Config, DaemonClient};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, Focus, LibraryRoots, StuckSection, Tab};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let base_url = format!("http://{}", config.daemon.listen_addr);
    let client = DaemonClient::new(base_url, &config.daemon.api_token)?;
    let roots = LibraryRoots {
        series: config.default_root_folder().to_string_lossy().to_string(),
        movies: config.movies_root_folder().to_string_lossy().to_string(),
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(client);
    let result = run(&mut terminal, &mut app, &roots).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    roots: &LibraryRoots,
) -> Result<()> {
    let mut last_refresh = tokio::time::Instant::now() - Duration::from_secs(10);

    loop {
        app.poll_background().await;
        app.expire_status();

        if last_refresh.elapsed() >= Duration::from_secs(3) || app.force_refresh {
            app.refresh_active_tab().await;
            app.force_refresh = false;
            last_refresh = tokio::time::Instant::now();
        }

        terminal.draw(|frame| ui::draw(frame, app))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(app, key, roots).await;
                if app.should_quit {
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_key(app: &mut App, key: KeyEvent, roots: &LibraryRoots) {
    let code = key.code;

    // Typing into the add-show search box takes priority over global keys.
    if matches!(app.tab, Tab::Add) && matches!(app.focus, Focus::AddSearchInput) {
        match code {
            KeyCode::Enter => app.run_add_search().await,
            KeyCode::Char(c) => app.add_query.push(c),
            KeyCode::Backspace => {
                app.add_query.pop();
            }
            KeyCode::Esc => {
                app.add_query.clear();
            }
            KeyCode::Tab => switch_tab(app, tab_offset(app.tab, 1)),
            KeyCode::BackTab => switch_tab(app, tab_offset(app.tab, -1)),
            _ => {}
        }
        return;
    }

    // Incremental Library title filter — same input-mode isolation as Add.
    if matches!(app.tab, Tab::Library) && matches!(app.focus, Focus::LibraryFilterInput) {
        match code {
            KeyCode::Enter => app.confirm_library_filter(),
            KeyCode::Char(c) => {
                app.library_query.push(c);
                app.recompute_library_view();
            }
            KeyCode::Backspace => {
                app.library_query.pop();
                app.recompute_library_view();
            }
            KeyCode::Esc => app.clear_library_filter(),
            KeyCode::Tab => switch_tab(app, tab_offset(app.tab, 1)),
            KeyCode::BackTab => switch_tab(app, tab_offset(app.tab, -1)),
            _ => {}
        }
        return;
    }

    // Typing a replacement value for the selected weight axis, same
    // priority-over-global-keys reasoning as the add-show search box above.
    if matches!(app.tab, Tab::Profiles) && matches!(app.focus, Focus::WeightInput) {
        match code {
            KeyCode::Enter => app.commit_weight_edit().await,
            KeyCode::Char(c) if c.is_ascii_digit() || c == '.' || c == '-' => {
                app.weight_input_buffer.push(c);
            }
            KeyCode::Backspace => {
                app.weight_input_buffer.pop();
            }
            KeyCode::Esc => app.cancel_weight_edit(),
            _ => {}
        }
        return;
    }

    // Help overlay intercepts everything while open, same
    // priority-over-global-keys idiom as the two input modes above.
    if app.help_visible {
        match code {
            KeyCode::Char('?') | KeyCode::Esc => app.help_visible = false,
            _ => {}
        }
        return;
    }

    // Any key other than a second `x`/`d` clears a pending delete
    // confirmation — the confirmation must be the very next keypress, not
    // just "any keypress before the user gets distracted."
    if app.confirm_delete && code != KeyCode::Char('x') {
        app.confirm_delete = false;
    }
    if app.confirm_delete_file && code != KeyCode::Char('d') {
        app.confirm_delete_file = false;
    }

    match code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => switch_tab(app, tab_offset(app.tab, 1)),
        KeyCode::BackTab => switch_tab(app, tab_offset(app.tab, -1)),
        KeyCode::Char(c) if c.is_ascii_digit() => {
            if let Some(n) = c.to_digit(10) {
                if (1..=Tab::ALL.len() as u32).contains(&n) {
                    switch_tab(app, Tab::ALL[(n as usize) - 1]);
                }
            }
        }
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('g') => app.select_edge(false),
        KeyCode::Char('G') => app.select_edge(true),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Esc => match app.tab {
            Tab::Library if matches!(app.focus, Focus::Candidates) => app.close_candidates(),
            Tab::Library if app.detail.is_some() => app.close_detail(),
            Tab::Library if !app.library_query.is_empty() => app.clear_library_filter(),
            Tab::Add => app.focus = Focus::AddSearchInput,
            Tab::Profiles if app.profile_detail.is_some() => app.close_profile_detail(),
            _ => {}
        },
        KeyCode::Enter => match app.tab {
            Tab::Library if matches!(app.focus, Focus::Candidates) => {
                app.grab_selected_candidate().await;
            }
            Tab::Library => app.open_detail().await,
            Tab::Add => match app.focus {
                Focus::AddResults => app.add_selected_search_result(roots).await,
                _ => app.focus = Focus::AddSearchInput,
            },
            Tab::Profiles if app.profile_detail.is_some() => {
                app.start_editing_selected_weight();
            }
            Tab::Profiles => app.open_profile_detail(),
            Tab::Stuck => app.jump_to_stuck_target().await,
            Tab::Calendar => app.jump_to_calendar_entry().await,
            _ => {}
        },
        KeyCode::Left | KeyCode::Right if matches!(app.tab, Tab::Stuck) => {
            app.stuck_focus = match app.stuck_focus {
                StuckSection::Stalled => StuckSection::Maxed,
                StuckSection::Maxed => StuckSection::Stalled,
            };
        }
        KeyCode::Char('?') => app.help_visible = true,
        KeyCode::Char('R') => app.force_refresh = true,
        KeyCode::Char('f') if matches!(app.tab, Tab::Library) && app.detail.is_none() => {
            app.library_filter = app.library_filter.next();
            app.recompute_library_view();
        }
        KeyCode::Char('o') if matches!(app.tab, Tab::Library) && app.detail.is_none() => {
            app.library_sort = app.library_sort.next();
            app.recompute_library_view();
        }
        KeyCode::Char('m') if matches!(app.tab, Tab::Library) && app.detail.is_none() => {
            app.toggle_monitor_list_selected().await;
        }
        KeyCode::Char('a') if matches!(app.tab, Tab::Review) => {
            app.approve_selected_review().await;
        }
        KeyCode::Char('r') if matches!(app.tab, Tab::Review) => {
            app.reject_selected_review().await;
        }
        KeyCode::Char('/') if matches!(app.tab, Tab::Library) && app.detail.is_none() => {
            app.start_library_filter();
        }
        KeyCode::Char('n')
            if matches!(app.tab, Tab::Library)
                && app.detail.is_some()
                && !matches!(app.focus, Focus::Candidates) =>
        {
            app.select_next_missing_episode();
        }
        KeyCode::Char('s') if matches!(app.tab, Tab::Library) && app.detail.is_some() => {
            app.search_now_selected().await;
        }
        KeyCode::Char('m') if matches!(app.tab, Tab::Library) && app.detail.is_some() => {
            app.toggle_monitor_selected().await;
        }
        KeyCode::Char('e') if matches!(app.tab, Tab::Library) && app.detail.is_some() => {
            app.toggle_monitor_selected_episode().await;
        }
        KeyCode::Char('S') if matches!(app.tab, Tab::Library) && app.detail.is_some() => {
            app.toggle_monitor_selected_season().await;
        }
        KeyCode::Char('x') if matches!(app.tab, Tab::Library) && app.detail.is_some() => {
            app.delete_selected().await;
        }
        KeyCode::Char('d') if matches!(app.tab, Tab::Library) && app.detail.is_some() => {
            app.delete_selected_file().await;
        }
        KeyCode::Char('c')
            if matches!(app.tab, Tab::Library)
                && app.detail.is_some()
                && !matches!(app.focus, Focus::Candidates) =>
        {
            app.fetch_candidates_for_selected().await;
        }
        _ => {}
    }
}

fn tab_offset(current: Tab, delta: i32) -> Tab {
    let idx = Tab::ALL.iter().position(|t| *t == current).unwrap_or(0) as i32;
    let len = Tab::ALL.len() as i32;
    Tab::ALL[(idx + delta).rem_euclid(len) as usize]
}

fn switch_tab(app: &mut App, tab: Tab) {
    if app.tab == tab {
        return;
    }
    if matches!(app.focus, Focus::Candidates) {
        app.close_candidates();
    }
    app.tab = tab;
    app.detail = None;
    app.profile_detail = None;
    app.profile_weight_state.select(None);
    app.focus = if matches!(app.tab, Tab::Add) {
        Focus::AddSearchInput
    } else {
        Focus::List
    };
}
