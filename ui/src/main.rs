// sarun-ui — Rust UI client SPIKE (the m5 architecture de-risk, parallel to
// ptyspike). Proves a Rust client can: speak the engine's wire protocol over
// the control socket, fetch real box state, render it with ratatui, and be
// HEADLESSLY tested against a live engine. Scope: read-only sessions view via
// `--once` (render one frame to a TestBackend, print it, exit). The
// interactive crossterm loop and the review panes are deferred; this validates
// the client+render+test architecture, not a full Textual replacement yet.
//
//   sarun-ui --once --sock /path/to/ui.sock

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use serde_json::Value;
use serde_json::json;

fn rpc(sock: &str, verb: &str, args: Value) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    let msg = json!({"type": "ui", "verb": verb, "args": args});
    s.write_all(format!("{msg}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let rep: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep.get("error").and_then(Value::as_str)
                   .unwrap_or("rpc failed").to_string());
    }
    Ok(rep.get("r").cloned().unwrap_or(Value::Null))
}

fn sessions_lines(sessions: &Value) -> Vec<Line<'static>> {
    let mut out = vec![];
    if let Some(arr) = sessions.as_array() {
        for s in arr {
            let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("");
            let path = g("path");
            let id = g("session_id");
            let status = g("status");
            let cmd = s.get("cmd").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str)
                     .collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            out.push(Line::from(format!(
                "{:<10} {:<8} {:<10} {}",
                if path.is_empty() { id } else { path }, id, status, cmd)));
        }
    }
    if out.is_empty() {
        out.push(Line::from("(no boxes)"));
    }
    out
}

fn render_once(sock: &str) -> Result<String, String> {
    let sessions = rpc(sock, "session_dicts", json!([]))?;
    let mut lines = vec![Line::from(format!(
        "{:<10} {:<8} {:<10} {}", "PATH", "ID", "STATUS", "CMD"))];
    lines.extend(sessions_lines(&sessions));
    let backend = TestBackend::new(100, 30);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;
    term.draw(|f| {
        let p = Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title(" sarun · boxes "));
        f.render_widget(p, f.area());
    }).map_err(|e| e.to_string())?;
    Ok(format!("{}", term.backend()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut once = false;
    let mut sock = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => once = true,
            "--sock" => sock = it.next().cloned().unwrap_or_default(),
            _ => {}
        }
    }
    if sock.is_empty() {
        sock = std::env::var("SARUN_SOCK").unwrap_or_default();
    }
    if !once {
        eprintln!("sarun-ui spike: only --once --sock PATH is implemented");
        std::process::exit(2);
    }
    match render_once(&sock) {
        Ok(buf) => { print!("{buf}"); std::process::exit(0); }
        Err(e) => { eprintln!("sarun-ui: {e}"); std::process::exit(1); }
    }
}
