use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use rusqlite::{Connection, Result};
use std::{env, fs, io};

fn get_db_path() -> String {
    let home = dirs::home_dir().expect("Could not find home directory");
    let recall_dir = home.join(".recall");
    fs::create_dir_all(&recall_dir).expect("Could not create ~/.recall directory");
    recall_dir.join("history.db").to_str().unwrap().to_string()
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp    INTEGER NOT NULL,
            command      TEXT NOT NULL,
            cwd          TEXT,
            git_repo     TEXT,
            git_branch   TEXT,
            exit_code    INTEGER,
            duration_ms  INTEGER,
            stdout       TEXT,
            stderr       TEXT,
            hostname     TEXT,
            shell        TEXT
        );
    ",
    )?;
    Ok(())
}

fn capture_session(conn: &Connection, args: &[String]) -> Result<()> {
    let command = args.get(2).cloned().unwrap_or_default();
    let cwd = args.get(3).cloned().unwrap_or_default();
    let exit_code = args.get(4).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let git_branch = args.get(5).cloned().unwrap_or_default();
    let git_repo = args.get(6).cloned().unwrap_or_default();

    if command.is_empty() {
        return Ok(());
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let hostname = fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();

    conn.execute(
        "INSERT INTO sessions
            (timestamp, command, cwd, git_repo, git_branch, exit_code, hostname, shell)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            timestamp, command, cwd, git_repo, git_branch, exit_code, hostname, "bash"
        ],
    )?;
    Ok(())
}

struct Session {
    id: i64,
    timestamp: i64,
    command: String,
    cwd: String,
    exit_code: i64,
    git_repo: String,
    git_branch: String,
}

fn format_time(timestamp_ms: i64) -> String {
    let secs = timestamp_ms / 1000;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    let days = secs / 86400;
    let year = 1970 + days / 365;
    let doy = days % 365;
    let month = doy / 30 + 1;
    let day = doy % 30 + 1;
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hours, minutes, seconds
    )
}

fn search_sessions(conn: &Connection, query: &str) -> Result<Vec<Session>> {
    let pattern = format!("%{}%", query);
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, command, cwd, exit_code,
                COALESCE(git_repo,''), COALESCE(git_branch,'')
         FROM sessions
         WHERE command LIKE ?1
         ORDER BY timestamp DESC
         LIMIT 50",
    )?;

    let rows = stmt.query_map([&pattern], |row| {
        Ok(Session {
            id: row.get(0)?,
            timestamp: row.get(1)?,
            command: row.get(2)?,
            cwd: row.get(3)?,
            exit_code: row.get(4)?,
            git_repo: row.get(5)?,
            git_branch: row.get(6)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

fn run_tui(sessions: Vec<Session>, query: &str) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut list_state = ListState::default();
    if !sessions.is_empty() {
        list_state.select(Some(0));
    }

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(5),
                ])
                .split(f.size());

            // ── Header ──
            let header = Paragraph::new(format!(
                " recall  searching: \"{}\"  ({} results)",
                query,
                sessions.len()
            ))
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(header, chunks[0]);

            // ── List ──
            let items: Vec<ListItem> = sessions
                .iter()
                .map(|s| {
                    let status_color = if s.exit_code == 0 {
                        Color::Green
                    } else {
                        Color::Red
                    };
                    let status_char = if s.exit_code == 0 { "✓" } else { "✗" };
                    let git_info = if !s.git_branch.is_empty() {
                        format!(" [{}]", s.git_branch)
                    } else {
                        String::new()
                    };
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(status_char, Style::default().fg(status_color)),
                            Span::raw("  "),
                            Span::styled(
                                &s.command,
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(vec![
                            Span::raw("   "),
                            Span::styled(
                                format_time(s.timestamp),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::raw("  "),
                            Span::styled(&s.cwd, Style::default().fg(Color::Blue)),
                            Span::styled(git_info, Style::default().fg(Color::Yellow)),
                        ]),
                    ])
                })
                .collect();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" results "))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, chunks[1], &mut list_state);

            // ── Detail panel ──
            let detail_text = if let Some(i) = list_state.selected() {
                if let Some(s) = sessions.get(i) {
                    format!(
                        " ID: {}  |  Exit: {}  |  Host: {}\n Command: {}\n Dir:     {}",
                        s.id, s.exit_code, "kali", s.command, s.cwd,
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let detail = Paragraph::new(detail_text)
                .style(Style::default().fg(Color::Gray))
                .block(Block::default().borders(Borders::ALL).title(" detail "));
            f.render_widget(detail, chunks[2]);
        })?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = match list_state.selected() {
                        Some(i) => (i + 1).min(sessions.len().saturating_sub(1)),
                        None => 0,
                    };
                    list_state.select(Some(i));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = match list_state.selected() {
                        Some(i) => i.saturating_sub(1),
                        None => 0,
                    };
                    list_state.select(Some(i));
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let db_path = get_db_path();
    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;

    match args.get(1).map(|s| s.as_str()) {
        Some("capture") => {
            capture_session(&conn, &args)?;
        }
        Some(query) => {
            let full_query = args[1..].join(" ");
            let sessions = search_sessions(&conn, &full_query)?;
            if sessions.is_empty() {
                println!("No results for '{}'", full_query);
            } else {
                run_tui(sessions, &full_query).unwrap();
            }
        }
        None => {
            println!("Usage:");
            println!("  recall <query>    — search your history");
        }
    }

    Ok(())
}
