// PTY feasibility spike — proves the Rust terminal stack drives a REAL
// interactive process and renders it HEADLESSLY, end to end:
//   portable-pty (spawn a command on a PTY) -> vt100 (parse the master bytes
//   into a screen grid) -> tui-term (render the grid as a ratatui widget) ->
//   ratatui TestBackend (in-memory buffer we can assert on — no terminal).
// Also proves the INPUT direction (keystrokes written to the PTY reach the
// child) — the bidirectional path a PTY box would mux over the engine channel.
// Run: cargo run  (exits 0 on success, non-zero with a message on failure).

use std::io::Read;
use std::io::Write;

use portable_pty::CommandBuilder;
use portable_pty::PtySize;
use portable_pty::native_pty_system;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tui_term::vt100; // the version tui-term's Screen impl matches
use tui_term::widget::PseudoTerminal;

/// Spawn `argv` on a fresh PTY, optionally write `input` to it, and return all
/// bytes the child emitted (read to EOF after it exits).
fn run_on_pty(argv: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave); // so the master sees EOF once the child exits
    if let Some(data) = input {
        let mut w = pair.master.take_writer().expect("writer");
        w.write_all(data).expect("write");
        w.flush().ok();
        drop(w);
    }
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    child.wait().expect("wait");
    drop(pair.master); // force EOF for the reader thread
    h.join().expect("join")
}

/// Render PTY output bytes through vt100 + tui-term into a TestBackend and
/// return the flattened screen text.
fn render(bytes: &[u8]) -> String {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(bytes);
    let backend = TestBackend::new(80, 24);
    let mut term = Terminal::new(backend).expect("terminal");
    term.draw(|f| {
        let screen = parser.screen();
        f.render_widget(PseudoTerminal::new(screen), f.area());
    })
    .expect("draw");
    format!("{}", term.backend())
}

fn main() {
    let mut fails = 0;
    let mut check = |cond: bool, msg: &str| {
        println!("{} {msg}", if cond { "  ok " } else { " FAIL" });
        if !cond {
            fails += 1;
        }
    };

    // 1. OUTPUT: a real process's stdout rendered headlessly.
    let out = run_on_pty(&["sh", "-c", "echo HELLO-PTY-WORLD; echo second-line"], None);
    let screen = render(&out);
    check(screen.contains("HELLO-PTY-WORLD"),
          "pty: child stdout parsed by vt100 and rendered to the TestBackend");
    check(screen.contains("second-line"),
          "pty: multi-line output lands on the grid");

    // 2. CONTROL SEQUENCES: vt100 actually emulates, not just passes bytes.
    let out = run_on_pty(
        &["printf", "\\033[2J\\033[H\\033[1mBOLD-OK\\033[0m done"], None);
    let screen = render(&out);
    check(screen.contains("BOLD-OK") && !screen.contains("\u{1b}["),
          "pty: vt100 interprets escape sequences (no raw codes on screen)");

    // 3. INPUT: keystrokes written to the PTY reach the child.
    let out = run_on_pty(
        &["sh", "-c", "read line; echo GOT:$line"], Some(b"ping-from-client\n"));
    let screen = render(&out);
    check(screen.contains("GOT:ping-from-client"),
          "pty: bytes written to the master reach the child (input path)");

    println!("\n{}", if fails == 0 { "PTY SPIKE PASS" } else { "PTY SPIKE FAIL" });
    std::process::exit(if fails == 0 { 0 } else { 1 });
}
