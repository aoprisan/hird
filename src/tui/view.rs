//! Drawing. Reads [`App`] and writes to a frame; changes no state.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{App, Column, Mode, Screen};
use super::theme;
use crate::fmt;
use crate::identity::actor_harness;
use crate::model::{Assertion, Status, TaskSummary};

/// Draw the whole screen.
pub fn render(frame: &mut Frame, app: &App, now: DateTime<Utc>) {
    let [header, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, header, app);
    match app.screen {
        Screen::Queue => render_queue(frame, body, app, now),
        Screen::Memory => render_memory(frame, body, app, now),
    }
    render_status_bar(frame, status, app, now);

    match &app.mode {
        Mode::Help => render_help(frame, app),
        Mode::AddTask { title, body, focus } => render_add_task(frame, title, body, *focus),
        Mode::Supersede { replacement, .. } => render_supersede(frame, app, replacement),
        Mode::TaskDetail {
            task,
            events,
            learned,
        } => render_task_detail(frame, task, events, learned, now),
        Mode::AssertionDetail { assertion } => render_assertion_detail(frame, app, assertion, now),
        Mode::Normal | Mode::Filter | Mode::Search => {}
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let tab = |label: &'static str, active: bool| {
        if active {
            Span::styled(format!(" {label} "), theme::focus_style().reversed())
        } else {
            Span::styled(format!(" {label} "), theme::dim_style())
        }
    };
    let line = Line::from(vec![
        Span::styled(" hird ", theme::focus_style()),
        tab("Queue", app.screen == Screen::Queue),
        tab("Memory", app.screen == Screen::Memory),
        Span::styled("  Tab switches · ? help · q quit", theme::dim_style()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ----------------------------------------------------------------- queue board

fn render_queue(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    let area = if app.mode == Mode::Filter || !app.filter.is_empty() {
        let [input, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
        render_text_input(
            frame,
            input,
            "filter",
            &app.filter,
            app.mode == Mode::Filter,
        );
        rest
    } else {
        area
    };

    let columns = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(area);
    for (column, rect) in Column::ALL.into_iter().zip(columns.iter()) {
        render_column(frame, *rect, app, column, now);
    }
}

fn render_column(frame: &mut Frame, area: Rect, app: &App, column: Column, now: DateTime<Utc>) {
    let tasks = app.column_tasks(column);
    let focused = app.column == column && !app.mode.is_text_entry();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            theme::focus_style()
        } else {
            theme::dim_style()
        })
        .title(Line::from(vec![
            Span::styled(format!(" {} ", column.title()), theme::focus_style()),
            Span::styled(format!("{} ", tasks.len()), theme::dim_style()),
        ]));

    if tasks.is_empty() {
        let hint = if app.filter.is_empty() {
            "—"
        } else {
            "no match"
        };
        frame.render_widget(
            Paragraph::new(hint).style(theme::dim_style()).block(block),
            area,
        );
        return;
    }

    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = tasks
        .iter()
        .map(|task| ListItem::new(task_card(task, width, now)))
        .collect();

    let mut state = ListState::default();
    if focused {
        state.select(Some(app.selected_index(column).min(tasks.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(theme::selection_style()),
        area,
        &mut state,
    );
}

/// One card: `#seq title`, then a line of badges when there is anything to say.
fn task_card(task: &TaskSummary, width: usize, now: DateTime<Utc>) -> Text<'static> {
    let marker = match task.priority {
        p if p > 0 => Span::styled(format!("▲{p} "), Style::default().yellow()),
        p if p < 0 => Span::styled(format!("▼{} ", -p), theme::dim_style()),
        _ => Span::raw(""),
    };
    let title_width = width.saturating_sub(marker.content.len() + 5).max(8);

    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("#{} ", task.seq),
            Style::default().fg(theme::status_color(task.status)),
        ),
        marker,
        Span::raw(fmt::truncate(&task.title, title_width)),
    ])];

    let mut badges: Vec<Span> = Vec::new();
    if let Some(holder) = &task.claimed_by {
        badges.push(Span::styled(
            actor_harness(holder).to_string(),
            theme::badge_style(actor_harness(holder)),
        ));
        if let Some(expires) = &task.lease_expires_at {
            let text = fmt::lease_remaining(expires, now);
            let style = if text == "overdue" {
                theme::overdue_style()
            } else {
                theme::dim_style()
            };
            badges.push(Span::raw(" "));
            badges.push(Span::styled(text, style));
        }
    } else if task.status != Status::Open {
        badges.push(Span::styled(
            fmt::age_phrase(&task.updated_at, now),
            theme::dim_style(),
        ));
    }
    if !badges.is_empty() {
        let mut line = vec![Span::raw("   ")];
        line.extend(badges);
        lines.push(Line::from(line));
    }
    Text::from(lines)
}

// --------------------------------------------------------------- memory browser

fn render_memory(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    let [input, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    render_text_input(frame, input, "search", &app.query, app.mode == Mode::Search);

    let title = if app.include_superseded {
        " Assertions (including superseded) "
    } else {
        " Assertions "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if app.mode.is_text_entry() {
            theme::dim_style()
        } else {
            theme::focus_style()
        })
        .title(Span::styled(title, theme::focus_style()));

    if app.assertions.is_empty() {
        let hint = if app.query.trim().is_empty() {
            "nothing recorded yet — agents write here with mem_store"
        } else {
            "no match"
        };
        frame.render_widget(
            Paragraph::new(hint).style(theme::dim_style()).block(block),
            area,
        );
        return;
    }

    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .assertions
        .iter()
        .map(|a| ListItem::new(assertion_row(a, app, width, now)))
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.memory_selected.min(app.assertions.len() - 1)));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(theme::selection_style()),
        list,
        &mut state,
    );
}

fn assertion_row(a: &Assertion, app: &App, width: usize, now: DateTime<Utc>) -> Text<'static> {
    let mut meta = vec![Span::styled(
        actor_harness(&a.actor).to_string(),
        theme::badge_style(actor_harness(&a.actor)),
    )];
    meta.push(Span::styled(
        format!("  {}", fmt::age_phrase(&a.created_at, now)),
        theme::dim_style(),
    ));
    if let Some(seq) = a.task_id.as_ref().and_then(|id| app.task_seqs.get(id)) {
        meta.push(Span::styled(
            format!("  learned on #{seq}"),
            theme::dim_style(),
        ));
    }
    let tags = a.tag_list();
    if !tags.is_empty() {
        meta.push(Span::styled(
            format!("  #{}", tags.join(" #")),
            Style::default().cyan(),
        ));
    }
    if a.superseded_by.is_some() {
        meta.push(Span::styled("  superseded", Style::default().red()));
    }
    if app.all_projects {
        meta.push(Span::styled(format!("  {}", a.project), theme::dim_style()));
    }

    let content = if a.superseded_by.is_some() {
        Span::styled(
            fmt::truncate(&a.content, width),
            theme::dim_style().crossed_out(),
        )
    } else {
        Span::raw(fmt::truncate(&a.content, width))
    };
    Text::from(vec![Line::from(content), Line::from(meta)])
}

// -------------------------------------------------------------------- chrome

fn render_text_input(frame: &mut Frame, area: Rect, label: &str, value: &str, active: bool) {
    let cursor = if active { "▌" } else { "" };
    let line = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            if active {
                theme::focus_style().reversed()
            } else {
                theme::dim_style()
            },
        ),
        Span::raw(" "),
        Span::raw(value.to_string()),
        Span::styled(cursor, theme::focus_style()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    if let Some(toast) = &app.toast {
        let style = if toast.is_error {
            Style::default().red().bold()
        } else {
            Style::default().green()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {}", toast.text), style))),
            area,
        );
        return;
    }

    let scope = if app.all_projects {
        "all projects".to_string()
    } else {
        shorten_path(&app.project)
    };
    let counts = Status::ALL
        .into_iter()
        .filter_map(|s| app.counts.get(&s).filter(|n| **n > 0).map(|n| (s, *n)))
        .map(|(s, n)| {
            Span::styled(
                format!("{n} {s}  "),
                Style::default().fg(theme::status_color(s)),
            )
        });

    let mut spans = vec![
        Span::styled(
            format!(" {}  ", shorten_path(&app.db_path.to_string_lossy())),
            theme::dim_style(),
        ),
        Span::styled(format!("{scope}  "), Style::default().cyan()),
    ];
    spans.extend(counts);
    spans.push(Span::styled(
        format!("· {} memory  ", app.memory_total),
        theme::dim_style(),
    ));
    spans.push(Span::styled(
        format!("· polled {}", app.poll_age(now)),
        theme::dim_style(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Keep the last two path components, so the status bar stays readable.
///
/// The elision is only worth it when it actually saves room, so `/tmp/x` is
/// left alone rather than becoming the longer `…/tmp/x`.
fn shorten_path(path: &str) -> String {
    let tail: Vec<&str> = path.rsplit('/').take(2).collect();
    if tail.len() < 2 {
        return path.to_string();
    }
    let shortened = format!("…/{}/{}", tail[1], tail[0]);
    if shortened.chars().count() < path.chars().count() {
        shortened
    } else {
        path.to_string()
    }
}

// ------------------------------------------------------------------ overlays

fn render_help(frame: &mut Frame, app: &App) {
    let common: &[(&str, &str)] = &[
        ("Tab", "switch between the queue and memory"),
        ("p", "toggle project filter (current / all)"),
        ("/", "filter or search"),
        ("?", "this help"),
        ("q", "quit"),
    ];
    let queue: &[(&str, &str)] = &[
        ("j / k", "move down / up"),
        ("h / l", "previous / next column"),
        ("g / G", "first / last card"),
        ("Enter", "open the task, its history and what was learned"),
        ("a", "add a task (Tab for the body, Enter to save)"),
        ("c", "cancel the selected task"),
        ("r", "reopen the selected task"),
    ];
    let memory: &[(&str, &str)] = &[
        ("j / k", "move down / up"),
        ("Enter", "show the assertion and its provenance"),
        ("d", "supersede it with something truer"),
        ("s", "show or hide superseded assertions"),
    ];

    let mut lines = Vec::new();
    let section = |title: &str, keys: &[(&str, &str)], lines: &mut Vec<Line<'static>>| {
        lines.push(Line::from(Span::styled(
            title.to_string(),
            theme::focus_style(),
        )));
        for (key, description) in keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<8}"), Style::default().cyan()),
                Span::raw(description.to_string()),
            ]));
        }
        lines.push(Line::from(""));
    };
    section("Anywhere", common, &mut lines);
    section("Queue board", queue, &mut lines);
    section("Memory browser", memory, &mut lines);
    lines.push(Line::from(Span::styled(
        format!(
            "Leases last {} minutes; expired claims return to Open automatically.",
            app.config.lease_ttl_minutes
        ),
        theme::dim_style(),
    )));
    lines.push(Line::from(Span::styled(
        "any key closes this".to_string(),
        theme::dim_style(),
    )));

    overlay(frame, " Keys ", Text::from(lines), 62, 26);
}

fn render_add_task(frame: &mut Frame, title: &str, body: &str, focus: usize) {
    let field = |label: &str, value: &str, active: bool| {
        Line::from(vec![
            Span::styled(
                format!("{label:<7}"),
                if active {
                    theme::focus_style()
                } else {
                    theme::dim_style()
                },
            ),
            Span::raw(value.to_string()),
            Span::styled(if active { "▌" } else { "" }, theme::focus_style()),
        ])
    };
    let text = Text::from(vec![
        field("title", title, focus == 0),
        field("body", body, focus == 1),
        Line::from(""),
        Line::from(Span::styled(
            "Tab switches field · Enter saves · Esc cancels",
            theme::dim_style(),
        )),
    ]);
    overlay(frame, " New task ", text, 70, 8);
}

fn render_supersede(frame: &mut Frame, app: &App, replacement: &str) {
    let original = app
        .selected_assertion()
        .map(|a| a.content.clone())
        .unwrap_or_default();
    let text = Text::from(vec![
        Line::from(Span::styled("replacing", theme::dim_style())),
        Line::from(Span::styled(original, theme::dim_style().crossed_out())),
        Line::from(""),
        Line::from(vec![
            Span::styled("with   ", theme::focus_style()),
            Span::raw(replacement.to_string()),
            Span::styled("▌", theme::focus_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter saves · empty text records a retraction · Esc cancels",
            theme::dim_style(),
        )),
    ]);
    overlay(frame, " Supersede assertion ", text, 76, 10);
}

fn render_task_detail(
    frame: &mut Frame,
    task: &crate::model::Task,
    events: &[crate::model::TaskEvent],
    learned: &[Assertion],
    now: DateTime<Utc>,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("#{} ", task.seq),
                Style::default().fg(theme::status_color(task.status)),
            ),
            Span::styled(task.title.clone(), theme::focus_style()),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", task.status),
                Style::default().fg(theme::status_color(task.status)),
            ),
            Span::styled(
                match (&task.claimed_by, &task.lease_expires_at) {
                    (Some(holder), Some(expires)) => {
                        format!("{holder} · {}", fmt::lease_remaining(expires, now))
                    }
                    (Some(holder), None) => holder.clone(),
                    _ => format!("created {}", fmt::age_phrase(&task.created_at, now)),
                },
                theme::dim_style(),
            ),
        ]),
    ];
    if !task.body.trim().is_empty() {
        lines.push(Line::from(""));
        lines.extend(task.body.lines().map(|l| Line::from(l.to_string())));
    }
    if let Some(result) = &task.result {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("result  ", theme::focus_style()),
            Span::raw(result.clone()),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("history", theme::focus_style())));
    for event in events {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:>9}  ", fmt::age_phrase(&event.at, now)),
                theme::dim_style(),
            ),
            Span::styled(
                format!("{:<14}", event.kind),
                Style::default().fg(theme::status_color(Status::Open)),
            ),
            Span::styled(
                format!("{:<18}", actor_harness(&event.actor)),
                theme::badge_style(actor_harness(&event.actor)),
            ),
            Span::raw(fmt::truncate(&event.detail, 60)),
        ]));
    }

    if !learned.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "learned while working this",
            theme::focus_style(),
        )));
        for assertion in learned {
            lines.push(Line::from(format!("  · {}", assertion.content)));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "any key closes this",
        theme::dim_style(),
    )));
    overlay(frame, " Task ", Text::from(lines), 96, 32);
}

fn render_assertion_detail(
    frame: &mut Frame,
    app: &App,
    assertion: &Assertion,
    now: DateTime<Utc>,
) {
    let mut lines = vec![Line::from(assertion.content.clone()), Line::from("")];
    let row = |label: &str, value: String, lines: &mut Vec<Line<'static>>| {
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<10}"), theme::dim_style()),
            Span::raw(value),
        ]));
    };
    row("asserted", assertion.actor.clone(), &mut lines);
    row(
        "when",
        format!(
            "{} ({})",
            assertion.created_at,
            fmt::age_phrase(&assertion.created_at, now)
        ),
        &mut lines,
    );
    row("project", assertion.project.clone(), &mut lines);
    if !assertion.tags.is_empty() {
        row("tags", assertion.tags.replace(',', ", "), &mut lines);
    }
    if let Some(seq) = assertion
        .task_id
        .as_ref()
        .and_then(|id| app.task_seqs.get(id))
    {
        row("task", format!("#{seq}"), &mut lines);
    }
    if let Some(by) = &assertion.superseded_by {
        row("superseded", format!("by {by}"), &mut lines);
    }
    row("id", assertion.id.clone(), &mut lines);

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "any key closes this",
        theme::dim_style(),
    )));
    overlay(frame, " Assertion ", Text::from(lines), 86, 16);
}

/// Draw a centred box over whatever is beneath it.
fn overlay(frame: &mut Frame, title: &str, text: Text<'static>, width: u16, height: u16) {
    let area = centered(frame.area(), width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::focus_style())
                .title(Span::styled(title.to_string(), theme::focus_style())),
        ),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::config::Config;
    use crate::db::Db;
    use crate::repo::NewAssertion;
    use crate::tui::app::App;

    const PROJECT: &str = "/tmp/project";

    /// Render the app to an off-screen buffer and return it as plain text.
    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, app, Utc::now()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(db: &Db) -> App {
        let mut app = App::new(
            PathBuf::from("/home/user/.local/share/hird/hird.db"),
            PROJECT.to_string(),
            Config::default(),
        );
        app.refresh(db).unwrap();
        app
    }

    #[test]
    fn shorten_path_keeps_the_tail() {
        assert_eq!(shorten_path("/home/user/code/hird"), "…/code/hird");
        assert_eq!(shorten_path("hird.db"), "hird.db");
        assert_eq!(shorten_path("/tmp/x"), "/tmp/x");
    }

    #[test]
    fn centering_never_leaves_the_frame() {
        let area = Rect::new(0, 0, 40, 10);
        let small = centered(area, 100, 100);
        assert_eq!(small, Rect::new(0, 0, 40, 10));
        let fitted = centered(area, 20, 4);
        assert_eq!(fitted, Rect::new(10, 3, 20, 4));
    }

    #[test]
    fn the_board_draws_four_columns_and_the_tabs() {
        let db = Db::open_in_memory().unwrap();
        let out = screen(&app_with(&db));
        assert!(out.contains("Queue"), "{out}");
        assert!(out.contains("Memory"), "{out}");
        assert!(out.contains("Open"), "{out}");
        assert!(out.contains("Claimed"), "{out}");
        assert!(out.contains("Done"), "{out}");
        assert!(out.contains("Failed"), "{out}");
    }

    #[test]
    fn a_card_shows_its_number_title_holder_and_countdown() {
        let db = Db::open_in_memory().unwrap();
        let seq = db
            .tasks()
            .create(PROJECT, "write the parser", "", 0, "cli")
            .unwrap()
            .seq;
        db.tasks()
            .claim(seq, "codex:9f2c", Config::default().lease_ttl())
            .unwrap();

        let out = screen(&app_with(&db));
        assert!(out.contains("#1"), "{out}");
        assert!(out.contains("write the parser"), "{out}");
        assert!(out.contains("codex"), "holder badge missing:\n{out}");
        assert!(out.contains("left"), "lease countdown missing:\n{out}");
    }

    #[test]
    fn priority_markers_only_appear_when_priority_is_set() {
        let db = Db::open_in_memory().unwrap();
        db.tasks()
            .create(PROJECT, "ordinary", "", 0, "cli")
            .unwrap();
        assert!(!screen(&app_with(&db)).contains('▲'));

        db.tasks().create(PROJECT, "urgent", "", 3, "cli").unwrap();
        assert!(screen(&app_with(&db)).contains("▲3"));
    }

    #[test]
    fn the_status_bar_reports_the_database_scope_and_counts() {
        let db = Db::open_in_memory().unwrap();
        db.tasks().create(PROJECT, "a", "", 0, "cli").unwrap();
        let out = screen(&app_with(&db));
        assert!(out.contains("…/hird/hird.db"), "{out}");
        assert!(out.contains("/tmp/project"), "{out}");
        assert!(out.contains("1 open"), "{out}");
        assert!(out.contains("polled"), "{out}");
    }

    #[test]
    fn a_toast_replaces_the_status_bar() {
        let db = Db::open_in_memory().unwrap();
        let mut app = app_with(&db);
        app.toast = Some(crate::tui::app::Toast {
            text: "task 1 is now cancelled".into(),
            is_error: false,
        });
        assert!(screen(&app).contains("task 1 is now cancelled"));
    }

    #[test]
    fn empty_columns_say_so_differently_when_filtering() {
        let db = Db::open_in_memory().unwrap();
        let mut app = app_with(&db);
        assert!(screen(&app).contains('—'));

        db.tasks().create(PROJECT, "a task", "", 0, "cli").unwrap();
        app.refresh(&db).unwrap();
        app.filter = "nothing matches".into();
        assert!(screen(&app).contains("no match"));
    }

    #[test]
    fn the_memory_browser_shows_content_provenance_and_the_task_link() {
        let db = Db::open_in_memory().unwrap();
        let seq = db
            .tasks()
            .create(PROJECT, "write the parser", "", 0, "cli")
            .unwrap()
            .seq;
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "the lexer lives in src/lex.rs",
                tags: "parser",
                actor: "claude-code:af31",
                task_seq: Some(seq),
            })
            .unwrap();

        let mut app = app_with(&db);
        app.screen = Screen::Memory;
        let out = screen(&app);
        assert!(out.contains("the lexer lives in src/lex.rs"), "{out}");
        assert!(out.contains("claude-code"), "{out}");
        assert!(out.contains("learned on #1"), "{out}");
        assert!(out.contains("#parser"), "{out}");
        assert!(out.contains("search"), "search box missing:\n{out}");
    }

    #[test]
    fn an_empty_memory_browser_explains_where_assertions_come_from() {
        let db = Db::open_in_memory().unwrap();
        let mut app = app_with(&db);
        app.screen = Screen::Memory;
        assert!(screen(&app).contains("mem_store"));
    }

    #[test]
    fn the_help_overlay_lists_every_documented_key() {
        let db = Db::open_in_memory().unwrap();
        let mut app = app_with(&db);
        app.mode = Mode::Help;
        let out = screen(&app);
        for key in [
            "Tab", "j / k", "h / l", "Enter", "a", "c", "r", "d", "p", "q",
        ] {
            assert!(out.contains(key), "help omits {key:?}:\n{out}");
        }
        assert!(out.contains("15 minutes"), "{out}");
    }

    #[test]
    fn the_add_prompt_shows_both_fields() {
        let db = Db::open_in_memory().unwrap();
        let mut app = app_with(&db);
        app.mode = Mode::AddTask {
            title: "new work".into(),
            body: "the details".into(),
            focus: 1,
        };
        let out = screen(&app);
        assert!(out.contains("New task"), "{out}");
        assert!(out.contains("new work"), "{out}");
        assert!(out.contains("the details"), "{out}");
        assert!(out.contains("Esc cancels"), "{out}");
    }

    #[test]
    fn the_task_detail_overlay_shows_body_history_and_assertions() {
        let db = Db::open_in_memory().unwrap();
        let task = db
            .tasks()
            .create(
                PROJECT,
                "write the parser",
                "start with the lexer",
                0,
                "cli",
            )
            .unwrap();
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "the lexer is hand written",
                tags: "",
                actor: "codex:9f2c",
                task_seq: Some(task.seq),
            })
            .unwrap();

        let mut app = app_with(&db);
        app.mode = Mode::TaskDetail {
            task: Box::new(db.tasks().get(task.seq).unwrap()),
            events: db.tasks().events(&task.id, 40).unwrap(),
            learned: db.memory().for_task(&task.id).unwrap(),
        };
        let out = screen(&app);
        assert!(out.contains("start with the lexer"), "{out}");
        assert!(out.contains("history"), "{out}");
        assert!(out.contains("created"), "{out}");
        assert!(out.contains("the lexer is hand written"), "{out}");
    }

    #[test]
    fn the_assertion_detail_overlay_shows_full_provenance() {
        let db = Db::open_in_memory().unwrap();
        let assertion = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "the api listens on 8080",
                tags: "api,net",
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap();
        let mut app = app_with(&db);
        app.screen = Screen::Memory;
        app.mode = Mode::AssertionDetail {
            assertion: Box::new(assertion.clone()),
        };
        let out = screen(&app);
        assert!(out.contains("the api listens on 8080"), "{out}");
        assert!(out.contains("codex:9f2c"), "{out}");
        assert!(out.contains("api, net"), "{out}");
        assert!(out.contains(&assertion.id), "{out}");
    }

    #[test]
    fn rendering_survives_a_tiny_terminal() {
        let db = Db::open_in_memory().unwrap();
        db.tasks().create(PROJECT, "a task", "", 0, "cli").unwrap();
        let mut app = app_with(&db);
        app.mode = Mode::Help;

        for (w, h) in [(20u16, 5u16), (1, 1), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| render(frame, &app, Utc::now()))
                .expect("rendering must not panic at {w}x{h}");
        }
    }
}
