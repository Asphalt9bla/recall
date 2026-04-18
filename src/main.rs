use chrono::prelude::*;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
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

// ── Config structs ──────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Debug)]
struct Config {
    #[serde(default)]
    capture: CaptureConfig,
    #[serde(default)]
    redaction: RedactionConfig,
    #[serde(default)]
    search: SearchConfig,
    #[serde(default)]
    display: DisplayConfig,
}

#[derive(serde::Deserialize, Debug, Default)]
struct CaptureConfig {
    #[serde(default)]
    exclude_commands: Vec<String>,
    #[serde(default)]
    exclude_dirs: Vec<String>,
}

#[derive(serde::Deserialize, Debug, Default)]
struct RedactionConfig {
    #[serde(default)]
    extra_patterns: Vec<String>,
}

#[derive(serde::Deserialize, Debug)]
struct SearchConfig {
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default)]
    semantic_search: bool,
}

#[derive(serde::Deserialize, Debug)]
struct DisplayConfig {
    #[serde(default = "default_true")]
    show_git_branch: bool,
    #[serde(default = "default_true")]
    show_duration: bool,
}

fn default_max_results() -> usize {
    50
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture: CaptureConfig::default(),
            redaction: RedactionConfig::default(),
            search: SearchConfig::default(),
            display: DisplayConfig::default(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            max_results: 50,
            semantic_search: false,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            show_git_branch: true,
            show_duration: true,
        }
    }
}

fn load_config() -> Config {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(".recall").join("config.toml");
    if let Ok(contents) = fs::read_to_string(&config_path) {
        toml::from_str(&contents).unwrap_or_default()
    } else {
        Config::default()
    }
}

// ── Database ────────────────────────────────────────────────────────────────

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

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS embeddings (
            session_id  INTEGER PRIMARY KEY,
            vector      BLOB NOT NULL
        );
    ",
    )?;

    Ok(())
}

// ── Redaction ───────────────────────────────────────────────────────────────

fn redact(text: &str, extra_patterns: &[String]) -> String {
    use regex::Regex;

    let mut builtin_patterns = vec![
        r"AKIA[0-9A-Z]{16}".to_string(),
        r"gh[pousr]_[A-Za-z0-9]{36,}".to_string(),
        r"sk-[A-Za-z0-9]{32,}".to_string(),
        r"xox[bpra]-[A-Za-z0-9\-]{10,}".to_string(),
        r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+".to_string(),
        r"(?i)(password|passwd|secret|token|api_key|apikey|auth)[\s]*[=:]+[\s]*\S+".to_string(),
        r"\b[0-9a-fA-F]{32,}\b".to_string(),
        r"(?i)bearer\s+[A-Za-z0-9\-_\.]+".to_string(),
    ];

    builtin_patterns.extend_from_slice(extra_patterns);

    let mut result = text.to_string();
    for pattern in &builtin_patterns {
        if let Ok(re) = Regex::new(pattern) {
            result = re.replace_all(&result, "[REDACTED]").to_string();
        }
    }
    result
}

// ── Capture ─────────────────────────────────────────────────────────────────

fn capture_session(conn: &Connection, args: &[String], config: &Config) -> Result<()> {
    let command = redact(
        &args.get(2).cloned().unwrap_or_default(),
        &config.redaction.extra_patterns,
    );
    let cwd = args.get(3).cloned().unwrap_or_default();
    let exit_code = args.get(4).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let git_branch = args.get(5).cloned().unwrap_or_default();
    let git_repo = args.get(6).cloned().unwrap_or_default();
    let duration = args.get(7).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let stdout = redact(
        &args.get(8).cloned().unwrap_or_default(),
        &config.redaction.extra_patterns,
    );
    let stderr = redact(
        &args.get(9).cloned().unwrap_or_default(),
        &config.redaction.extra_patterns,
    );

    if command.is_empty() {
        return Ok(());
    }

    for excluded in &config.capture.exclude_commands {
        if command == *excluded || command.starts_with(&format!("{} ", excluded)) {
            return Ok(());
        }
    }

    for excluded_dir in &config.capture.exclude_dirs {
        if cwd.starts_with(excluded_dir.as_str()) {
            return Ok(());
        }
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

// ── Session struct ───────────────────────────────────────────────────────────

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

// ── Embeddings ───────────────────────────────────────────────────────────────

fn get_embedding_model() -> Option<TextEmbedding> {
    TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(false),
    )
    .ok()
}

fn embed_and_store(conn: &Connection, session_id: i64, text: &str) {
    if let Some(model) = get_embedding_model() {
        if let Ok(embeddings) = model.embed(vec![text], None) {
            if let Some(vector) = embeddings.into_iter().next() {
                let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO embeddings (session_id, vector) VALUES (?1, ?2)",
                    rusqlite::params![session_id, bytes],
                );
            }
        }
    }
}

fn index_all_sessions(conn: &Connection) {
    let model = match get_embedding_model() {
        Some(m) => m,
        None => {
            println!("Failed to load embedding model.");
            return;
        }
    };

    let mut stmt = conn
        .prepare(
            "SELECT id, command, cwd FROM sessions
         WHERE id NOT IN (SELECT session_id FROM embeddings)",
        )
        .unwrap();

    let rows: Vec<(i64, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let total = rows.len();
    if total == 0 {
        println!("All sessions already indexed.");
        return;
    }

    println!("Indexing {} sessions...", total);

    for (i, (id, command, cwd)) in rows.iter().enumerate() {
        let text = command.clone();
        if let Ok(embeddings) = model.embed(vec![text.as_str()], None) {
            if let Some(vector) = embeddings.into_iter().next() {
                let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO embeddings (session_id, vector) VALUES (?1, ?2)",
                    rusqlite::params![id, bytes],
                );
            }
        }
        print!("\r  {}/{}", i + 1, total);
        use std::io::Write;
        io::stdout().flush().unwrap();
    }
    println!("\nDone. {} sessions indexed.", total);
}

// ── Search ───────────────────────────────────────────────────────────────────

fn search_sessions(
    conn: &Connection,
    query: &str,
    failed_only: bool,
    today_only: bool,
    max_results: usize,
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
         LIMIT {}",
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
        max_results,
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

fn semantic_search(conn: &Connection, query: &str, limit: usize) -> Vec<i64> {
    let model = match get_embedding_model() {
        Some(m) => m,
        None => return vec![],
    };

    let query_embedding = match model.embed(vec![query], None) {
        Ok(e) => e.into_iter().next().unwrap_or_default(),
        Err(_) => return vec![],
    };

    let mut stmt = match conn.prepare("SELECT session_id, vector FROM embeddings") {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    });

    let rows = match rows {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    let mut scored: Vec<(i64, f32)> = rows
        .filter_map(|r| r.ok())
        .map(|(id, bytes)| {
            let vector: Vec<f32> = bytes
                .chunks(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();

            let dot: f32 = query_embedding
                .iter()
                .zip(vector.iter())
                .map(|(a, b)| a * b)
                .sum();
            let mag_a: f32 = query_embedding.iter().map(|a| a * a).sum::<f32>().sqrt();
            let mag_b: f32 = vector.iter().map(|b| b * b).sum::<f32>().sqrt();
            let similarity = if mag_a > 0.0 && mag_b > 0.0 {
                dot / (mag_a * mag_b)
            } else {
                0.0
            };

            (id, similarity)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored
        .into_iter()
        .filter(|(_, score)| *score > 0.15) // only return meaningful matches
        .take(limit)
        .map(|(id, _)| id)
        .collect()
}

// ── TUI ──────────────────────────────────────────────────────────────────────

fn run_tui(sessions: Vec<Session>, query: &str, config: &Config) -> io::Result<()> {
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

            let header = Paragraph::new(format!(
                " recall  query: \"{}\"  ({} results)  [↑↓] navigate  [q] quit",
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

            let items: Vec<ListItem> = sessions
                .iter()
                .map(|s| {
                    let status_color = if s.exit_code == 0 {
                        Color::Green
                    } else {
                        Color::Red
                    };
                    let status_char = if s.exit_code == 0 { "✓" } else { "✗" };
                    let git_info = if config.display.show_git_branch && !s.git_branch.is_empty() {
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
                            Span::styled(s.cwd.clone(), Style::default().fg(Color::Blue)),
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

            let detail_text = if let Some(i) = list_state.selected() {
                if let Some(s) = sessions.get(i) {
                    let stdout_preview = if s.stdout.is_empty() {
                        "(no output captured)".to_string()
                    } else {
                        s.stdout.lines().take(2).collect::<Vec<_>>().join(" | ")
                    };
                    let duration_str = if config.display.show_duration {
                        if s.duration_ms > 0 {
                            format!("  |  {}s", s.duration_ms)
                        } else {
                            "  |  <1s".to_string()
                        }
                    } else {
                        String::new()
                    };
                    format!(
                        " ID: {}  |  Exit: {}  |  {}{}\n Command: {}\n Dir:     {}\n Output:  {}",
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

// ── Replay ───────────────────────────────────────────────────────────────────

fn replay_session(conn: &Connection, id: i64) -> Result<()> {
    let mut stmt = conn.prepare("SELECT command, cwd FROM sessions WHERE id = ?1")?;

    let result = stmt.query_row([id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });

    match result {
        Ok((command, cwd)) => {
            println!("┌─────────────────────────────────────────┐");
            println!("  Command:   {}", command);
            println!("  Directory: {}", cwd);
            println!("└─────────────────────────────────────────┘");
            print!("Run this command? [y/n]: ");

            use std::io::Write;
            io::stdout().flush().unwrap();

            let mut input = String::new();
            io::stdin().read_line(&mut input).unwrap();

            if input.trim().to_lowercase() == "y" {
                let status = std::process::Command::new("bash")
                    .arg("-c")
                    .arg(&command)
                    .current_dir(&cwd)
                    .status();

                match status {
                    Ok(s) => {
                        if s.success() {
                            println!("✓ Command completed successfully.");
                        } else {
                            println!("✗ Command exited with code: {}", s.code().unwrap_or(-1));
                        }
                    }
                    Err(e) => println!("Failed to run command: {}", e),
                }
            } else {
                println!("Aborted.");
            }
        }
        Err(_) => println!("No session found with ID {}", id),
    }

    Ok(())
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let config = load_config();
    let db_path = get_db_path();
    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;

    if args.get(1).map(|s| s.as_str()) == Some("capture") {
        return capture_session(&conn, &args, &config);
    }

    if args.get(1).map(|s| s.as_str()) == Some("replay") {
        let id = args.get(2).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        if id == 0 {
            println!("Usage: recall replay <id>");
        } else {
            replay_session(&conn, id)?;
        }
        return Ok(());
    }

    if args.get(1).map(|s| s.as_str()) == Some("index") {
        println!("Loading embedding model...");
        index_all_sessions(&conn);
        return Ok(());
    }

    let failed_only = args.contains(&"--failed".to_string());
    let today_only = args.contains(&"--today".to_string());
    let semantic = args.contains(&"--semantic".to_string());

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

    let sessions = if semantic {
        println!("Loading semantic search model...");
        let ids = semantic_search(&conn, &query, config.search.max_results);
        if ids.is_empty() {
            vec![]
        } else {
            let placeholders: String = ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, timestamp, command, cwd, exit_code,
                        COALESCE(git_branch,''), COALESCE(stdout,''), COALESCE(duration_ms,0)
                 FROM sessions WHERE id IN ({})
                 ORDER BY timestamp DESC",
                placeholders
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<Box<dyn rusqlite::ToSql>> = ids
                .iter()
                .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
                .collect();
            stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
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
        }
    } else {
        search_sessions(
            &conn,
            &query,
            failed_only,
            today_only,
            config.search.max_results,
        )?
    };

    if sessions.is_empty() {
        println!("No results for '{}'", query);
    } else {
        run_tui(sessions, &query, &config).unwrap();
    }

    Ok(())
}
