use chrono::prelude::*;
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
            shell        TEXT,
            tags         TEXT
        );
    ",
    )?;

    let columns: Result<Vec<String>> = conn
        .prepare("PRAGMA table_info(sessions)")?
        .query_map([], |row| row.get(1))?
        .collect();

    let columns = columns?;
    if !columns.contains(&"tags".to_string()) {
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN tags TEXT", []);
    }

    Ok(())
}

fn capture_session(conn: &Connection, args: &[String]) -> Result<()> {
    let command = args.get(2).cloned().unwrap_or_default();
    let cwd = args.get(3).cloned().unwrap_or_default();
    let exit_code = args.get(4).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let git_branch = args.get(5).cloned().unwrap_or_default();
    let git_repo = args.get(6).cloned().unwrap_or_default();
    let duration = args.get(7).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let stdout = args.get(8).cloned().unwrap_or_default();
    let stderr = args.get(9).cloned().unwrap_or_default();

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
            (timestamp, command, cwd, git_repo, git_branch, exit_code,
             duration_ms, stdout, stderr, hostname, shell)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            timestamp, command, cwd, git_repo, git_branch, exit_code, duration, stdout, stderr,
            hostname, "bash"
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
    git_branch: String,
    stdout: String,
    duration_ms: i64,
}

fn format_time(timestamp_ms: i64) -> String {
    let dt = DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn search_sessions(
    conn: &Connection,
    query: &str,
    failed_only: bool,
    today_only: bool,
) -> Result<Vec<Session>> {
    let pattern = format!("%{}%", query);

    let today_start = if today_only {
        let now = Local::now();
        let midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        Local
            .from_local_datetime(&midnight)
            .unwrap()
            .timestamp_millis()
    } else {
        0
    };

    let sql = format!(
        "SELECT id, timestamp, command, cwd, exit_code,
                COALESCE(git_branch,''), COALESCE(stdout,''), COALESCE(duration_ms,0)
         FROM sessions
         WHERE (command LIKE ?1 OR COALESCE(stdout,'') LIKE ?1)
         {}
         {}
         ORDER BY timestamp DESC
         LIMIT 50",
        if failed_only {
            "AND exit_code != 0"
        } else {
            ""
        },
        if today_only {
            "AND timestamp >= ?2"
        } else {
            ""
        },
    );

    let mut stmt = conn.prepare(&sql)?;

    let rows = if today_only {
        stmt.query_map(rusqlite::params![pattern, today_start], |row| {
            Ok(Session {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                command: row.get(2)?,
                cwd: row.get(3)?,
                exit_code: row.get(4)?,
                git_branch: row.get(5)?,
                stdout: row.get(6)?,
                duration_ms: row.get(7)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![pattern], |row| {
            Ok(Session {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                command: row.get(2)?,
                cwd: row.get(3)?,
                exit_code: row.get(4)?,
                git_branch: row.get(5)?,
                stdout: row.get(6)?,
                duration_ms: row.get(7)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    Ok(rows)
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
                    Constraint::Length(6),
                ])
                .split(f.size());

            // ── Header ──
            let header = Paragraph::new(format!(
                " recall  query: \"{}\"  ({} results)  [↑↓] navigate  [q] quit",
                query,
                sessions.len()
            ))
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(header, chunks[0]);

            // ── List ──
            let items: Vec<ListItem> = sessions
                .iter()
                .map(|s| {
                    let status_color = if s.exit_code == 0 { Color::Green } else { Color::Red };
                    let status_char  = if s.exit_code == 0 { "✓" } else { "✗" };
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
                                s.command.clone(),
                                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(vec![
                            Span::raw("   "),
                            Span::styled(
                                format_time(s.timestamp),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::raw("  "),
                            Span::styled(s.cwd.clone(), Style::default().fg(Color::Blue)),
                            Span::styled(git_info, Style::default().fg(Color::Yellow)),
                        ]),
                    ])
                })
                .collect();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" results "))
                .highlight_style(
                    Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, chunks[1], &mut list_state);

            // ── Detail panel ──
            let detail_text = if let Some(i) = list_state.selected() {
                if let Some(s) = sessions.get(i) {
                    let stdout_preview = if s.stdout.is_empty() {
                        "(no output captured)".to_string()
                    } else {
                        s.stdout.lines().take(2).collect::<Vec<_>>().join(" | ")
                    };
                    let duration_str = if s.duration_ms > 0 {
                        format!("{}s", s.duration_ms)
                    } else {
                        "<1s".to_string()
                    };
                    format!(
                        " ID: {}  |  Exit: {}  |  {}  |  {}\n Command: {}\n Dir:     {}\n Output:  {}",
                        s.id,
                        s.exit_code,
                        format_time(s.timestamp),
                        duration_str,
                        s.command,
                        s.cwd,
                        stdout_preview,
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

    if args.get(1).map(|s| s.as_str()) == Some("capture") {
        return capture_session(&conn, &args);
    }

    let failed_only = args.contains(&"--failed".to_string());
    let today_only = args.contains(&"--today".to_string());

    let query_words: Vec<String> = args[1..]
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();

    let query = if query_words.is_empty() {
        "%".to_string()
    } else {
        query_words.join(" ")
    };

    let sessions = search_sessions(&conn, &query, failed_only, today_only)?;

    if sessions.is_empty() {
        println!("No results for '{}'", query);
    } else {
        run_tui(sessions, &query).unwrap();
    }

    Ok(())
}
