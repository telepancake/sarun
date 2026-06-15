// sarun-ui — a Rust interactive ratatui client for the sarun engine. It speaks
// the engine's newline-JSON control protocol over the UI control socket (see
// engine/src/control.rs: {"type":"ui","verb":...,"args":[...]} requests, replies
// wrapped {"ok":true,"r":...}; a {"type":"subscribe"} connection becomes a
// one-way event feed yielding session_added/removed/renamed/pong lines).
//
// It mirrors the core panes of the Python Textual UI (sarun: SessionTable,
// ChangesTable, hunk view, the UI(App)):
//   - Sessions pane  : table of boxes (path/id/status/cmd) from session_dicts
//   - Changes pane   : review.session_changes for the selected box (kind/size)
//   - Hunk/detail    : review.hunks for the selected change, colored unified diff
//
// Modes:
//   sarun-ui --sock PATH          interactive crossterm loop (real terminal)
//   sarun-ui --once --sock PATH   render one frame to a TestBackend, print, exit
//                                 (headless; used by the integration tests)
//
// Keys (mirroring the Textual BINDINGS where the verb exists in the Rust engine
// today): j/k or down/up move within the focused pane; Tab cycles panes; Enter on
// a box loads its changes, Enter on a change loads its hunks; a = apply, x =
// discard (a change if one is selected, else the whole box); K = kill box; D =
// delete box; r prompts rename; R refreshes; q quits. Verbs that a parallel
// agent may still be adding (apply_hunk, decorate, change_mode) are NOT
// hard-depended on — any "unknown verb 'X'" reply is surfaced as a status
// message, never a crash.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use serde_json::Value;
use serde_json::json;

// ── wire protocol ───────────────────────────────────────────────────────────

/// One request/reply on the control socket. Returns the unwrapped `r` payload
/// on success. An engine `{"ok":false,"error":...}` reply (e.g. an unknown verb
/// for a not-yet-implemented action) becomes `Err(error_string)` — callers turn
/// that into a status line, never a panic.
fn rpc(sock: &str, verb: &str, args: Value) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    let msg = json!({"type": "ui", "verb": verb, "args": args});
    s.write_all(format!("{msg}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let rep: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("rpc failed")
            .to_string());
    }
    Ok(rep.get("r").cloned().unwrap_or(Value::Null))
}

/// The engine's `rename` is a top-level control type (not a "ui" verb): it takes
/// {"type":"rename","sid":..,"name":..} and replies {"ok":true,...}.
// Driven only by the interactive loop; the headless tests don't exercise rename.
#[cfg_attr(test, allow(dead_code))]
fn rename_rpc(sock: &str, sid: &str, name: &str) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    let msg = json!({"type": "rename", "sid": sid, "name": name});
    s.write_all(format!("{msg}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let rep: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("rename failed")
            .to_string());
    }
    Ok(rep)
}

/// Open a subscribe connection and spawn a reader thread that forwards each
/// event line (parsed JSON) to `tx`. The engine turns a {"type":"subscribe"}
/// connection into a one-way feed (session_added/removed/renamed, pong). The
/// thread ends when the socket closes; failures are silent (the UI simply stops
/// receiving live events and still works via manual refresh).
fn spawn_subscriber(sock: &str, tx: mpsc::Sender<Value>) {
    let sock = sock.to_string();
    std::thread::spawn(move || {
        let Ok(mut s) = UnixStream::connect(&sock) else { return };
        if s.write_all(b"{\"type\":\"subscribe\"}\n").is_err() {
            return;
        }
        let reader = BufReader::new(s.try_clone().expect("clone subscribe conn"));
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
        let _ = s.flush();
    });
}

// ── app state ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sessions,
    Changes,
    Hunks,
}

struct App {
    sock: String,
    sessions: Vec<Value>,
    changes: Vec<Value>,
    hunks: Value, // raw review.hunks result for the selected change
    sel_session: usize,
    sel_change: usize,
    hunk_scroll: u16,
    focus: Pane,
    status: String,
    renaming: Option<String>, // Some(buffer) while editing a new name
    #[cfg_attr(test, allow(dead_code))]
    should_quit: bool,
}

impl App {
    fn new(sock: String) -> Self {
        let mut a = App {
            sock,
            sessions: vec![],
            changes: vec![],
            hunks: Value::Null,
            sel_session: 0,
            sel_change: 0,
            hunk_scroll: 0,
            focus: Pane::Sessions,
            status: "ready · j/k move · Tab pane · Enter open · a apply · x discard · K kill · D delete · r rename · q quit".into(),
            renaming: None,
            should_quit: false,
        };
        a.refresh_sessions();
        a.load_changes();
        a
    }

    /// box_id (the engine's session_id, a stringified i64) of the selected box.
    fn cur_sid(&self) -> Option<String> {
        self.sessions
            .get(self.sel_session)
            .and_then(|s| s.get("session_id"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    fn cur_change_path(&self) -> Option<String> {
        self.changes
            .get(self.sel_change)
            .and_then(|c| c.get("path"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    fn refresh_sessions(&mut self) {
        match rpc(&self.sock, "session_dicts", json!([])) {
            Ok(Value::Array(a)) => {
                self.sessions = a;
                if self.sel_session >= self.sessions.len() {
                    self.sel_session = self.sessions.len().saturating_sub(1);
                }
            }
            Ok(_) => self.sessions.clear(),
            Err(e) => self.status = format!("session_dicts: {e}"),
        }
    }

    /// Load the changes for the selected box and reset the change cursor.
    fn load_changes(&mut self) {
        self.changes.clear();
        self.hunks = Value::Null;
        self.sel_change = 0;
        self.hunk_scroll = 0;
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "review.session_changes", json!([sid])) {
            Ok(Value::Array(a)) => self.changes = a,
            Ok(_) => {}
            Err(e) => self.status = format!("session_changes: {e}"),
        }
    }

    /// Load the hunks (unified diff) for the selected change of the selected box.
    fn load_hunks(&mut self) {
        self.hunks = Value::Null;
        self.hunk_scroll = 0;
        let (Some(sid), Some(path)) = (self.cur_sid(), self.cur_change_path()) else {
            return;
        };
        match rpc(&self.sock, "review.hunks", json!([sid, path])) {
            Ok(v) => self.hunks = v,
            Err(e) => self.status = format!("hunks: {e}"),
        }
    }

    // ── navigation ── (driven by the interactive loop; not by headless tests)

    #[cfg_attr(test, allow(dead_code))]
    fn move_down(&mut self) {
        match self.focus {
            Pane::Sessions => {
                if self.sel_session + 1 < self.sessions.len() {
                    self.sel_session += 1;
                    self.load_changes();
                }
            }
            Pane::Changes => {
                if self.sel_change + 1 < self.changes.len() {
                    self.sel_change += 1;
                    self.load_hunks();
                }
            }
            Pane::Hunks => self.hunk_scroll = self.hunk_scroll.saturating_add(1),
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn move_up(&mut self) {
        match self.focus {
            Pane::Sessions => {
                if self.sel_session > 0 {
                    self.sel_session -= 1;
                    self.load_changes();
                }
            }
            Pane::Changes => {
                if self.sel_change > 0 {
                    self.sel_change -= 1;
                    self.load_hunks();
                }
            }
            Pane::Hunks => self.hunk_scroll = self.hunk_scroll.saturating_sub(1),
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn next_pane(&mut self) {
        self.focus = match self.focus {
            Pane::Sessions => Pane::Changes,
            Pane::Changes => Pane::Hunks,
            Pane::Hunks => Pane::Sessions,
        };
    }

    /// Enter: open the selected row into the next pane.
    fn open(&mut self) {
        match self.focus {
            Pane::Sessions => {
                self.load_changes();
                self.focus = Pane::Changes;
            }
            Pane::Changes => {
                self.load_hunks();
                self.focus = Pane::Hunks;
            }
            Pane::Hunks => {}
        }
    }

    // ── verbs ──

    /// The selector for an apply/discard: a single change path when the Changes
    /// pane is focused and a change is selected; otherwise null (the engine
    /// treats null as "the whole box").
    fn review_selector(&self) -> Value {
        if self.focus == Pane::Changes {
            if let Some(p) = self.cur_change_path() {
                return json!([p]);
            }
        }
        Value::Null
    }

    fn apply(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        let sel = self.review_selector();
        match rpc(&self.sock, "review.apply", json!([sid, sel])) {
            Ok(r) => {
                let n = r.get("applied").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                self.status = format!("applied {n} change(s)");
            }
            Err(e) => self.status = format!("apply: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    fn discard(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        let sel = self.review_selector();
        match rpc(&self.sock, "review.discard", json!([sid, sel])) {
            Ok(r) => {
                let n = r.get("discarded").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                self.status = format!("discarded {n} change(s)");
            }
            Err(e) => self.status = format!("discard: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn kill(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "kill", json!([sid])) {
            Ok(_) => self.status = format!("sent SIGTERM to box {sid}"),
            Err(e) => self.status = format!("kill: {e}"),
        }
        self.refresh_sessions();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn delete(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "delete", json!([sid])) {
            Ok(_) => self.status = format!("deleted box {sid}"),
            Err(e) => self.status = format!("delete: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn commit_rename(&mut self) {
        let Some(name) = self.renaming.take() else { return };
        let Some(sid) = self.cur_sid() else { return };
        match rename_rpc(&self.sock, &sid, &name) {
            Ok(_) => self.status = format!("renamed box {sid} -> {name}"),
            Err(e) => self.status = format!("rename: {e}"),
        }
        self.refresh_sessions();
    }

    /// Apply an incoming subscribe event. Structural events (added/removed/
    /// renamed) re-read session_dicts so the list reflects the new world; pong
    /// is shown in the status line. Unknown event types are ignored.
    fn on_event(&mut self, ev: &Value) {
        match ev.get("type").and_then(Value::as_str) {
            Some("session_added") | Some("session_removed") | Some("session_renamed") => {
                self.refresh_sessions();
                self.load_changes();
                self.status = format!(
                    "event: {}",
                    ev.get("type").and_then(Value::as_str).unwrap_or("?")
                );
            }
            Some("pong") => self.status = "pong".into(),
            _ => {}
        }
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

fn title(base: &str, focused: bool) -> String {
    if focused {
        format!(" {base} «focus» ")
    } else {
        format!(" {base} ")
    }
}

fn block(t: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default().borders(Borders::ALL).border_style(style).title(t)
}

fn sessions_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<14} {:<4} {:<9} {}", "PATH", "ID", "STATUS", "CMD"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.sessions.is_empty() {
        out.push(Line::from("(no boxes)"));
        return out;
    }
    for (i, s) in app.sessions.iter().enumerate() {
        let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let path = g("path");
        let id = g("session_id");
        let status = g("status");
        let name = g("name");
        let cmd = s
            .get("cmd")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let label = if path.is_empty() { name } else { path };
        let text = format!("{label:<14} {id:<4} {status:<9} {cmd}");
        let line = if i == app.sel_session {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(text)
        };
        out.push(line);
    }
    out
}

fn changes_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<9} {:>10}  {}", "KIND", "SIZE", "PATH"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.changes.is_empty() {
        out.push(Line::from("(no changes)"));
        return out;
    }
    for (i, c) in app.changes.iter().enumerate() {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or("");
        let path = c.get("path").and_then(Value::as_str).unwrap_or("");
        let size = c.get("size").and_then(Value::as_i64).unwrap_or(0);
        let kcolor = match kind {
            "deleted" => Color::Red,
            "symlink" => Color::Magenta,
            _ => Color::Green,
        };
        let text = format!("{kind:<9} {size:>10}  {path}");
        let line = if i == app.sel_change {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(Span::styled(text, Style::default().fg(kcolor)))
        };
        out.push(line);
    }
    out
}

/// Render review.hunks into a colored unified diff. Text diffs come as
/// {"is_text":true,"hunks":[{"lines":[[tag,text],...]}]} where tag is one of
/// "hdr"/" "/"-"/"+". Non-text/binary/symlink/deleted come as
/// {"is_text":false,"diff":{kind,...}}.
fn hunk_lines(app: &App) -> Vec<Line<'static>> {
    let h = &app.hunks;
    if h.is_null() {
        return vec![Line::from("(select a change and press Enter)")];
    }
    if h.get("is_text").and_then(Value::as_bool) == Some(true) {
        let mut out = vec![];
        if let Some(hunks) = h.get("hunks").and_then(Value::as_array) {
            if hunks.is_empty() {
                out.push(Line::from("(no textual differences)"));
            }
            for hunk in hunks {
                if let Some(lines) = hunk.get("lines").and_then(Value::as_array) {
                    for l in lines {
                        let arr = l.as_array();
                        let tag = arr.and_then(|a| a.first()).and_then(Value::as_str).unwrap_or(" ");
                        let txt = arr.and_then(|a| a.get(1)).and_then(Value::as_str).unwrap_or("");
                        let (prefix, color) = match tag {
                            "hdr" => ("", Color::Cyan),
                            "+" => ("+", Color::Green),
                            "-" => ("-", Color::Red),
                            _ => (" ", Color::Gray),
                        };
                        out.push(Line::from(Span::styled(
                            format!("{prefix}{txt}"),
                            Style::default().fg(color),
                        )));
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(Line::from("(no hunks)"));
        }
        out
    } else {
        let diff = h.get("diff").cloned().unwrap_or(Value::Null);
        let kind = diff.get("kind").and_then(Value::as_str).unwrap_or("binary");
        let mut out = vec![Line::from(Span::styled(
            format!("[{kind}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))];
        if let Some(d) = diff.get("diff").and_then(Value::as_str) {
            out.push(Line::from(d.to_string()));
        } else if let Some(e) = diff.get("error").and_then(Value::as_str) {
            out.push(Line::from(format!("error: {e}")));
        } else if diff.get("content").is_some() {
            out.push(Line::from("(binary content — not rendered)"));
        }
        out
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());
    let body = root[0];
    let status_area = root[1];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(body);

    // left column: sessions on top, changes below
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(cols[0]);

    let sessions = Paragraph::new(Text::from(sessions_lines(app))).block(block(
        title("sarun · boxes", app.focus == Pane::Sessions),
        app.focus == Pane::Sessions,
    ));
    f.render_widget(sessions, left[0]);

    let changes = Paragraph::new(Text::from(changes_lines(app))).block(block(
        title("changes", app.focus == Pane::Changes),
        app.focus == Pane::Changes,
    ));
    f.render_widget(changes, left[1]);

    let hunks = Paragraph::new(Text::from(hunk_lines(app)))
        .block(block(title("diff", app.focus == Pane::Hunks), app.focus == Pane::Hunks))
        .scroll((app.hunk_scroll, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(hunks, cols[1]);

    let status_text = if let Some(buf) = &app.renaming {
        format!("rename -> {buf}_  (Enter to commit, Esc to cancel)")
    } else {
        app.status.clone()
    };
    let status = Paragraph::new(Line::from(Span::styled(
        status_text,
        Style::default().fg(Color::Black).bg(Color::Gray),
    )));
    f.render_widget(
        status,
        Rect { x: status_area.x, y: status_area.y, width: status_area.width, height: 1 },
    );
}

// ── headless one-frame render (tests / --once) ──────────────────────────────

/// Render the current app state to a TestBackend and return the buffer as text.
/// Used by `--once` and the integration tests.
fn render_to_string(app: &App, w: u16, h: u16) -> Result<String, String> {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;
    term.draw(|f| draw(f, app)).map_err(|e| e.to_string())?;
    Ok(format!("{}", term.backend()))
}

// ── interactive loop (real terminal) ────────────────────────────────────────

#[cfg(not(test))]
fn run_interactive(sock: &str) -> Result<(), String> {
    use crossterm::event;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::execute;
    use crossterm::terminal;
    use ratatui::backend::CrosstermBackend;

    let mut app = App::new(sock.to_string());
    let (tx, rx) = mpsc::channel();
    spawn_subscriber(sock, tx);

    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen).map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;

    let res = (|| -> Result<(), String> {
        loop {
            // drain live events
            while let Ok(ev) = rx.try_recv() {
                app.on_event(&ev);
            }
            term.draw(|f| draw(f, &app)).map_err(|e| e.to_string())?;
            if app.should_quit {
                break;
            }
            if !event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
                continue;
            }
            if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                // rename input mode captures keys
                if let Some(buf) = app.renaming.as_mut() {
                    match k.code {
                        KeyCode::Enter => app.commit_rename(),
                        KeyCode::Esc => {
                            app.renaming = None;
                            app.status = "rename cancelled".into();
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        KeyCode::Char(c) => buf.push(c),
                        _ => {}
                    }
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') => app.should_quit = true,
                    KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                    KeyCode::Tab => app.next_pane(),
                    KeyCode::Enter => app.open(),
                    KeyCode::Char('a') => app.apply(),
                    KeyCode::Char('x') => app.discard(),
                    KeyCode::Char('K') => app.kill(),
                    KeyCode::Char('D') => app.delete(),
                    KeyCode::Char('r') => app.renaming = Some(String::new()),
                    KeyCode::Char('R') => {
                        app.refresh_sessions();
                        app.load_changes();
                        app.status = "refreshed".into();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    terminal::disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen).map_err(|e| e.to_string())?;
    term.show_cursor().map_err(|e| e.to_string())?;
    res
}

// ── entrypoint ──────────────────────────────────────────────────────────────

#[cfg(not(test))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut once = false;
    let mut sock = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => once = true,
            "--sock" => sock = it.next().cloned().unwrap_or_default(),
            "-h" | "--help" => {
                println!(
                    "sarun-ui — Rust ratatui client for the sarun engine\n\
                     \n\
                     usage:\n  \
                     sarun-ui --sock PATH          interactive UI\n  \
                     sarun-ui --once --sock PATH   render one frame and exit (headless)\n"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    if sock.is_empty() {
        sock = std::env::var("SARUN_SOCK").unwrap_or_default();
    }
    if sock.is_empty() {
        eprintln!("sarun-ui: no socket (pass --sock PATH or set SARUN_SOCK)");
        std::process::exit(2);
    }
    if once {
        let app = App::new(sock);
        match render_to_string(&app, 100, 30) {
            Ok(buf) => {
                print!("{buf}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("sarun-ui: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = run_interactive(&sock) {
        eprintln!("sarun-ui: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
fn main() {}

// ── integration tests against a LIVE engine ─────────────────────────────────
//
// These boot the real sarun-engine `serve` in an isolated XDG/SLOPBOX_NS, create
// boxes via box_new, write real files through the FUSE mount, then drive the App
// (the same state machine the interactive loop drives) and assert the rendered
// TestBackend buffer CONTAINS the real box ids / changed-file names / diff text.
// They require a working FUSE + the engine binary; if the engine can't be found
// or fails to come up, the test self-skips with an explanatory message.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Child;
    use std::process::Command;

    struct Engine {
        child: Child,
        sock: String,
        _tmp: PathBuf,
    }

    impl Drop for Engine {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn engine_bin() -> Option<PathBuf> {
        // ui/ is a sibling of engine/; the release binary lives there.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let rel = here.join("../engine/target/release/sarun-engine");
        if rel.exists() {
            return Some(rel);
        }
        let dbg = here.join("../engine/target/debug/sarun-engine");
        dbg.exists().then_some(dbg)
    }

    /// Boot a private engine instance. Returns None (skip) if the binary is
    /// missing or the control socket never appears (no FUSE/permissions here).
    fn boot() -> Option<Engine> {
        let bin = engine_bin()?;
        // unique NS per test (pid + atomic) so parallel cargo tests don't collide.
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static N: AtomicU64 = AtomicU64::new(0);
        let ns = format!("uit{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
        let tmp = std::env::temp_dir().join(format!("sarun-ui-{ns}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).ok()?;
        let xdg = tmp.join("xdg");
        std::fs::create_dir_all(&xdg).ok()?;

        let child = Command::new(&bin)
            .arg("serve")
            .env("SLOPBOX_NS", &ns)
            .env("HOME", &tmp)
            .env("XDG_DATA_HOME", &xdg)
            .env("XDG_STATE_HOME", &xdg)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("XDG_RUNTIME_DIR", &xdg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        let sock = xdg.join(format!("slopbox.{ns}")).join("ui.sock");
        for _ in 0..60 {
            if sock.exists() && UnixStream::connect(&sock).is_ok() {
                return Some(Engine {
                    child,
                    sock: sock.to_string_lossy().into_owned(),
                    _tmp: tmp,
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut e = Engine { child, sock: String::new(), _tmp: tmp };
        let _ = e.child.kill();
        None
    }

    /// Create a box via box_new and return (sid, mount_root).
    fn make_box(sock: &str) -> (String, PathBuf) {
        let r = rpc(sock, "box_new", json!([])).expect("box_new");
        let sid = r.get("sid").and_then(Value::as_str).unwrap().to_string();
        let root = PathBuf::from(r.get("root").and_then(Value::as_str).unwrap());
        (sid, root)
    }

    #[test]
    fn sessions_pane_shows_real_box() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (sid, _root) = make_box(&eng.sock);
        let app = App::new(eng.sock.clone());
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains(&sid), "frame should contain box id {sid}; got:\n{buf}");
        assert!(buf.contains("boxes"), "sessions pane title missing");
        assert!(buf.contains("changes"), "changes pane title missing");
        assert!(buf.contains("diff"), "diff pane title missing");
    }

    #[test]
    fn changes_pane_shows_written_file() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        // write a real file through the FUSE mount → a captured change.
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir through mount");
        std::fs::write(dir.join("hello_ui_marker.txt"), b"hello from ui test\n")
            .expect("write through mount");

        // drive the App as the loop would: select box (Enter) → Changes pane.
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        app.open();
        // render wide so the full path fits (a narrow pane truncates it).
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(
            buf.contains("hello_ui_marker.txt"),
            "changes pane should list the written file; got:\n{buf}"
        );
        assert!(buf.contains("changed"), "kind 'changed' missing:\n{buf}");
        // the 19-byte write's size must show in the pane.
        assert!(buf.contains("19"), "size column missing:\n{buf}");
    }

    #[test]
    fn hunks_pane_shows_diff_content() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("diffme.txt"), b"UNIQUE_DIFF_LINE_xyzzy\n").expect("write");

        let mut app = App::new(eng.sock.clone());
        // scripted keys: Enter (box→changes), Enter (change→hunks).
        app.open();
        assert!(!app.changes.is_empty(), "expected at least one change");
        app.open();
        let buf = render_to_string(&app, 120, 40).unwrap();
        assert!(
            buf.contains("xyzzy") || buf.contains("UNIQUE_DIFF"),
            "diff pane should contain the new file's unique line; got:\n{buf}"
        );
    }

    #[test]
    fn discard_removes_the_change() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("to_discard.txt"), b"junk\n").expect("write");

        let mut app = App::new(eng.sock.clone());
        app.open(); // focus Changes, changes loaded
        let present = |a: &App| {
            a.changes.iter().any(|c| {
                c.get("path")
                    .and_then(Value::as_str)
                    .map(|p| p.contains("to_discard.txt"))
                    .unwrap_or(false)
            })
        };
        assert!(present(&app), "change should be present before discard");
        app.discard();
        assert!(!present(&app), "change should be gone after discard; status={}", app.status);
    }

    #[test]
    fn unknown_verb_is_graceful() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // a verb a parallel agent may still be adding — must NOT panic, just Err.
        let r = rpc(&eng.sock, "apply_hunk", json!(["1", "x", 0]));
        assert!(r.is_err(), "unknown verb should be an Err, not a crash");
        assert!(r.unwrap_err().contains("unknown verb"), "error should name the unknown verb");
    }

    #[test]
    fn live_event_feed_delivers_pong_and_removed() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (tx, rx) = mpsc::channel();
        spawn_subscriber(&eng.sock, tx);
        std::thread::sleep(Duration::from_millis(300));
        // ping triggers a 'pong' broadcast on the subscribe feed.
        let _ = rpc(&eng.sock, "ping", json!([]));
        let mut saw_pong = false;
        for _ in 0..5 {
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(ev) => {
                    if ev.get("type").and_then(Value::as_str) == Some("pong") {
                        saw_pong = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(saw_pong, "expected a 'pong' live event on the subscribe feed");

        // a structural event (delete) should arrive and refresh the App.
        let (sid, _root) = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        let _ = rpc(&eng.sock, "delete", json!([sid.clone()]));
        let mut saw_removed = false;
        for _ in 0..10 {
            if let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
                if ev.get("type").and_then(Value::as_str) == Some("session_removed") {
                    app.on_event(&ev);
                    saw_removed = true;
                    break;
                }
            }
        }
        assert!(saw_removed, "expected a session_removed live event");
    }

    #[test]
    fn apply_then_materializes_on_host() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // a path under /tmp the host can actually accept on apply.
        let fname = format!("applied_marker_{sid}.txt");
        std::fs::write(dir.join(&fname), b"applied content\n").expect("write");
        let host_path = PathBuf::from("/tmp").join(&fname);
        let _ = std::fs::remove_file(&host_path);

        let mut app = App::new(eng.sock.clone());
        app.open(); // changes pane
        assert!(!app.changes.is_empty(), "expected a change before apply");
        app.apply();
        assert!(
            app.status.starts_with("applied"),
            "apply status should report success; got {}",
            app.status
        );
        assert!(host_path.exists(), "applied file should materialize on host at {host_path:?}");
        let _ = std::fs::remove_file(&host_path);
    }
}
