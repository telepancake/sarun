// Syntax-highlighted text editor pane (the Edit pane's engine): edtui's
// vim-modal editor widget + a syntastica (tree-sitter) highlight bridge.
//
// Port of the editor-eval prototype's hl.rs. The shape that makes it fast:
// tree-sitter runs over the WHOLE buffer only on load and on a debounced
// (250ms) re-highlight after edits, bucketing resolved styles into per-line
// span lists (`FileHighlights`). Each frame then injects ONLY a
// cursor-anchored window of those spans into `EditorState::highlights` —
// edtui scans that list linearly per rendered cell, so injecting a 10k-line
// file whole costs 60-432ms/frame where the ±400-row window measured
// 1.42ms/frame in the eval.
//
// Sources are the reader's two: a host file (direct I/O) and a box file
// (bytes over the control socket — `review.file_bytes` in, the
// `review.write_file` verb out). The pane refuses loudly instead of
// guessing: >8MB, non-UTF-8, or NUL-containing (binary) buffers never
// mount, and read-only mode refuses every mutating key with a status
// message while navigation stays live.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::{
    EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Highlight, Index2,
    LineNumbers, Lines,
};
use ratatui::style::{Color, Modifier, Style};
use syntastica::Processor;
use syntastica::theme::ResolvedTheme;
use syntastica_parsers::{Lang, LanguageSetImpl};

/// Rows around the cursor whose spans are injected per frame. The viewport
/// follows the cursor, so this only needs to comfortably exceed any
/// plausible terminal height; 400 keeps the per-frame highlight list small
/// (the windowing pin) while PageDown never outruns it mid-frame.
pub const HL_WINDOW: usize = 400;

/// Edits re-highlight after this quiet period — one tree-sitter pass per
/// pause, not per keystroke.
const REHL_DEBOUNCE: Duration = Duration::from_millis(250);

/// Refuse to open buffers over this (bounded memory: the pane holds the
/// jagged char buffer + the span cache, each proportional to the file).
const MAX_EDIT_BYTES: usize = 8 << 20;

// ── the syntastica → edtui bridge (hl.rs port) ──────────────────────────────

/// Per-line styled spans, precomputed for the whole buffer.
/// (start_col, end_col_inclusive, style) — cols are CHAR indices, matching
/// edtui's Index2 addressing.
pub struct FileHighlights {
    pub per_line: Vec<Vec<(usize, usize, Style)>>,
}

/// Language by file extension. None = plain text (no highlighting, and the
/// buffer still opens — unknown extensions must never refuse or panic).
/// NOTE: no toml — syntastica-parsers' crates.io pack lacks the grammars
/// that aren't published on crates.io (toml, dockerfile, kotlin, …).
pub fn lang_for_path(path: &str) -> Option<Lang> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?;
    Some(match ext {
        "rs" => Lang::Rust,
        "py" => Lang::Python,
        "c" | "h" => Lang::C,
        "js" | "mjs" => Lang::Javascript,
        "sh" | "bash" => Lang::Bash,
        "md" => Lang::Markdown,
        "json" => Lang::Json,
        "yml" | "yaml" => Lang::Yaml,
        _ => return None,
    })
}

/// Human tag for the pane title.
fn lang_label(lang: Option<Lang>) -> &'static str {
    match lang {
        Some(Lang::Rust) => "rust",
        Some(Lang::Python) => "python",
        Some(Lang::C) => "c",
        Some(Lang::Javascript) => "javascript",
        Some(Lang::Bash) => "bash",
        Some(Lang::Markdown) => "markdown",
        Some(Lang::Json) => "json",
        Some(Lang::Yaml) => "yaml",
        _ => "plain",
    }
}

/// The ONE process-wide parser set (8 grammars) and resolved theme. The
/// grammars are immutable after load; sharing them keeps memory bounded no
/// matter how many editors a session opens.
fn language_set() -> &'static LanguageSetImpl {
    static SET: OnceLock<LanguageSetImpl> = OnceLock::new();
    SET.get_or_init(LanguageSetImpl::new)
}

fn theme() -> &'static ResolvedTheme {
    static T: OnceLock<ResolvedTheme> = OnceLock::new();
    // one::dark — RGB token colors from the eval; the pane CHROME (borders,
    // line numbers, cursor, selection) stays on the engine's ANSI
    // conventions so the editor sits inside the UI like every other pane.
    T.get_or_init(syntastica_themes::one::dark)
}

fn conv_style(s: syntastica::style::Style) -> Style {
    let c = s.color();
    let mut st = Style::default().fg(Color::Rgb(c.red, c.green, c.blue));
    if let Some(bg) = s.bg() {
        st = st.bg(Color::Rgb(bg.red, bg.green, bg.blue));
    }
    if s.bold() {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.italic() {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if s.underline() {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    st
}

/// Run tree-sitter over the full text and bucket resolved styles per line.
/// A parse error yields NO highlights (plain text), never a broken pane.
pub fn compute(code: &str, lang: Lang) -> FileHighlights {
    let mut per_line = Vec::new();
    let mut processor = Processor::new(language_set());
    if let Ok(highlights) = processor.process(code, lang) {
        for line in &highlights {
            let mut spans = Vec::new();
            let mut col = 0usize;
            for (text, key) in line {
                let n = text.chars().count();
                if n > 0 {
                    if let Some(style) = key.and_then(|k| theme().find_style(k)) {
                        spans.push((col, col + n - 1, conv_style(style)));
                    }
                    col += n;
                }
            }
            per_line.push(spans);
        }
    }
    FileHighlights { per_line }
}

/// Push the highlights for a cursor-anchored row window into the editor
/// state. edtui scans `state.highlights` linearly per rendered char, so we
/// only inject rows near the cursor (the viewport follows the cursor).
pub fn inject(state: &mut EditorState, fh: &FileHighlights, window: usize) {
    let cur = state.cursor.row;
    let lo = cur.saturating_sub(window);
    let hi = (cur + window).min(fh.per_line.len().saturating_sub(1));
    state.highlights.clear();
    for row in lo..=hi {
        if let Some(spans) = fh.per_line.get(row) {
            for &(c0, c1, style) in spans {
                state.highlights.push(Highlight::new(
                    Index2::new(row, c0),
                    Index2::new(row, c1),
                    style,
                ));
            }
        }
    }
}

/// Editor buffer -> String (for save and re-highlight). Row-joined with
/// '\n', which round-trips byte-exact with `Lines::from` (a trailing
/// newline parses as a final empty row and re-joins to the same bytes).
pub fn text_of(state: &EditorState) -> String {
    let mut out = String::new();
    for (i, line) in state.lines.iter_row().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.extend(line.iter());
    }
    out
}

fn hash_of(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

// ── the editor pane state ───────────────────────────────────────────────────

/// Where the buffer came from and where a save goes. `Box` saves cross the
/// control socket (`review.write_file`) — the pane itself never touches a
/// box store; the UI performs the RPC and calls `mark_saved`.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Host(PathBuf),
    /// `sid` as the control socket carries it (the UI's cur_sid string).
    Box { sid: String, rel: String },
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Host(p) => p.display().to_string(),
            Target::Box { sid, rel } => format!("box:{sid}:/{rel}"),
        }
    }
}

/// What a key did — the UI acts on the non-`Consumed` results (save, leave
/// the pane, toggle fullscreen, open the path prompt); everything else stays
/// inside the editor. Same contract as the reader's `KeyResult`.
#[derive(PartialEq, Debug)]
pub enum KeyResult {
    Consumed,
    NotHandled,
    Close,
    ToggleFull,
    OpenPrompt,
    /// Ctrl-S: the UI writes `save_bytes()` to `target` (direct I/O for a
    /// host file, `review.write_file` for a box file) and calls
    /// `mark_saved` on success.
    Save,
}

/// The editor pane: one open buffer with its edtui state, the precomputed
/// highlight cache, dirty tracking (buffer hash vs last-saved hash) and the
/// debounced re-highlight clock. The SAME `render` draws the right-pane and
/// fullscreen mounts — only the target Rect differs.
pub struct EditorPane {
    pub target: Target,
    pub state: EditorState,
    handler: EditorEventHandler,
    lang: Option<Lang>,
    fh: Option<FileHighlights>,
    /// Hash of the last-saved text; `dirty` == current hash differs.
    saved_hash: u64,
    /// Hash of the buffer after the last handled key — edit detection
    /// without keeping a second copy of the text (bounded memory).
    buf_hash: u64,
    pub dirty: bool,
    pub read_only: bool,
    /// When a pending re-highlight becomes due (armed by edits, disarmed by
    /// `maybe_rehighlight`). `None` = highlights are current.
    rehl_due: Option<Instant>,
    pub status: String,
}

/// Refuse anything the editor cannot faithfully round-trip: oversized,
/// NUL-containing (binary), or non-UTF-8 bytes. Loud, specific errors.
fn text_for_edit(label: &str, bytes: Vec<u8>) -> anyhow::Result<String> {
    if bytes.len() > MAX_EDIT_BYTES {
        anyhow::bail!(
            "editor: {label} is {} bytes (cap {MAX_EDIT_BYTES})",
            bytes.len()
        );
    }
    if bytes.contains(&0) {
        anyhow::bail!("editor: {label} is binary (contains NUL) — not editable");
    }
    String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("editor: {label} is not valid UTF-8 — refusing a lossy edit"))
}

impl EditorPane {
    /// Open caller-supplied bytes for `target`. `read_only` marks a source
    /// the UI knows cannot be written back.
    pub fn from_bytes(target: Target, bytes: Vec<u8>, read_only: bool) -> anyhow::Result<Self> {
        let label = target.label();
        let text = text_for_edit(&label, bytes)?;
        let lang = lang_for_path(&label);
        let fh = lang.map(|l| compute(&text, l));
        let state = EditorState::new(Lines::from(text.as_str()));
        let h = hash_of(&text);
        Ok(EditorPane {
            target,
            state,
            handler: EditorEventHandler::default(),
            lang,
            fh,
            saved_hash: h,
            buf_hash: h,
            dirty: false,
            read_only,
            rehl_due: None,
            status: "vim keys · Ctrl-S save · Ctrl-O open · Ctrl-E lock · z zoom · Esc back"
                .into(),
        })
    }

    /// Open a host file directly. An unwritable file opens read-only
    /// (navigation works; every mutation refuses with a status message).
    pub fn open_host(path: PathBuf) -> anyhow::Result<Self> {
        let md = std::fs::symlink_metadata(&path)
            .map_err(|e| anyhow::anyhow!("editor: {}: {e}", path.display()))?;
        if md.file_type().is_symlink() {
            anyhow::bail!("editor: {} is a symlink — open the target", path.display());
        }
        if !md.is_file() {
            anyhow::bail!("editor: {} is not a regular file", path.display());
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("editor: {}: {e}", path.display()))?;
        let ro = md.permissions().readonly();
        Self::from_bytes(Target::Host(path), bytes, ro)
    }

    /// Open a BOX file's current bytes (fetched by the UI over
    /// `review.file_bytes`). Saves go back over `review.write_file`.
    pub fn open_box(sid: String, rel: String, bytes: Vec<u8>) -> anyhow::Result<Self> {
        Self::from_bytes(Target::Box { sid, rel }, bytes, false)
    }

    pub fn lang_label(&self) -> &'static str {
        lang_label(self.lang)
    }

    /// The bytes a save writes — exactly the buffer, byte-for-byte.
    pub fn save_bytes(&self) -> Vec<u8> {
        text_of(&self.state).into_bytes()
    }

    /// The UI persisted `save_bytes()` successfully.
    pub fn mark_saved(&mut self) {
        self.saved_hash = self.buf_hash;
        self.dirty = false;
        self.status = format!("saved {}", self.target.label());
    }

    /// Recompute highlights if the debounce deadline has passed. Called
    /// every frame with `Instant::now()`; tests drive the clock explicitly.
    pub fn maybe_rehighlight(&mut self, now: Instant) {
        if let Some(due) = self.rehl_due {
            if now >= due {
                self.rehl_due = None;
                if let Some(l) = self.lang {
                    self.fh = Some(compute(&text_of(&self.state), l));
                }
            }
        }
    }

    /// True while an edit is waiting out the debounce quiet period.
    #[allow(dead_code)]
    pub fn rehighlight_pending(&self) -> bool {
        self.rehl_due.is_some()
    }

    /// Handle one key. The caller (ui.rs) already consumed the F-keys; an
    /// OPEN editor consumes everything else — normal-mode letters are vim
    /// motions/operators, so pane accelerators intentionally do NOT fire
    /// from inside a buffer (leave with Esc, the chips, or F9).
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        // Pane controls first, from any mode: save / open / lock-toggle.
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('s') => {
                    if self.read_only {
                        self.status = "read-only — Ctrl-E unlocks before saving".into();
                        return KeyResult::Consumed;
                    }
                    return KeyResult::Save;
                }
                KeyCode::Char('o') => return KeyResult::OpenPrompt,
                KeyCode::Char('e') => {
                    self.read_only = !self.read_only;
                    self.status = if self.read_only {
                        "read-only — navigation only (Ctrl-E unlocks)".into()
                    } else {
                        "editable".into()
                    };
                    return KeyResult::Consumed;
                }
                _ => {}
            }
        }
        if self.state.mode == EditorMode::Normal && mods.is_empty() {
            match code {
                KeyCode::Char('z') => return KeyResult::ToggleFull,
                // Normal-mode Esc has no edtui meaning — it unwinds the
                // pane (zoom first, then go_back — decided by the UI).
                KeyCode::Esc => return KeyResult::Close,
                _ => {}
            }
        }
        // Read-only: the navigation whitelist passes; everything else
        // refuses LOUDLY (status line), never silently mutates.
        if self.read_only && !is_navigation(code, mods, &self.state) {
            self.status = "read-only — mutation refused (Ctrl-E unlocks)".into();
            return KeyResult::Consumed;
        }
        self.handler
            .on_key_event(KeyEvent::new(code, mods), &mut self.state);
        self.after_key();
        KeyResult::Consumed
    }

    /// Post-key bookkeeping: dirty flag from the buffer hash, and arm the
    /// debounced re-highlight when the buffer actually changed.
    fn after_key(&mut self) {
        let h = hash_of(&text_of(&self.state));
        if h != self.buf_hash {
            self.buf_hash = h;
            self.dirty = h != self.saved_hash;
            if self.lang.is_some() {
                self.rehl_due = Some(Instant::now() + REHL_DEBOUNCE);
            }
        }
    }

    /// Pane title: target · dirty marker · language · mode.
    pub fn title(&self) -> String {
        format!(
            " {}{}{} · {} ",
            self.target.label(),
            if self.dirty { " *" } else { "" },
            if self.read_only { " [read-only]" } else { "" },
            self.lang_label(),
        )
    }

    /// Draw the editor into `area` — the ONE widget both the right-pane and
    /// fullscreen mounts use. Injects the highlight window for the current
    /// cursor and services the debounce clock.
    pub fn render(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect, focused: bool) {
        use ratatui::widgets::{Block, BorderType, Borders};
        self.maybe_rehighlight(Instant::now());
        if let Some(fh) = &self.fh {
            inject(&mut self.state, fh, HL_WINDOW);
        }
        // Chrome on the engine's pane conventions (reader.rs render):
        // double cyan border when focused, plain gray otherwise, status on
        // the bottom title.
        let (bstyle, btype) = if focused {
            (
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                BorderType::Double,
            )
        } else {
            (Style::default().fg(Color::Gray), BorderType::Plain)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(btype)
            .border_style(bstyle)
            .title(self.title())
            .title_bottom(
                ratatui::text::Line::from(self.status.clone())
                    .right_aligned()
                    .style(Style::default().fg(Color::DarkGray)),
            );
        let inner = block.inner(area);
        f.render_widget(block, area);
        let view = EditorView::new(&mut self.state)
            .theme(
                EditorTheme::default()
                    .base(Style::default().fg(Color::Gray))
                    .cursor_style(Style::default().add_modifier(Modifier::REVERSED))
                    .selection_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                    .line_numbers_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(false)
            .line_numbers(LineNumbers::Absolute);
        f.render_widget(view, inner);
    }
}

/// The read-only whitelist: cursor motion, paging, and search keys pass;
/// anything else is a (refused) mutation. Only Normal-mode keys qualify —
/// read-only can never be in Insert/Visual (mode-entering keys are refused).
fn is_navigation(code: KeyCode, mods: KeyModifiers, state: &EditorState) -> bool {
    if state.mode != EditorMode::Normal {
        return false; // unreachable in read-only; fail closed
    }
    matches!(
        code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Esc
    ) || (mods.is_empty()
        && matches!(
            code,
            KeyCode::Char('h')
                | KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Char('l')
                | KeyCode::Char('w')
                | KeyCode::Char('b')
                | KeyCode::Char('e')
                | KeyCode::Char('g')
                | KeyCode::Char('G')
                | KeyCode::Char('0')
                | KeyCode::Char('$')
                | KeyCode::Char('/')
                | KeyCode::Char('n')
                | KeyCode::Char('N')
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rust_fixture(fns: usize) -> String {
        let mut s = String::new();
        for i in 0..fns {
            s.push_str(&format!(
                "/// doc {i}\npub fn func_{i}(x: u32) -> u32 {{\n    let s = \"lit{i}\";\n    x + {i}\n}}\n"
            ));
        }
        s
    }

    fn pane_of(name: &str, text: &str) -> EditorPane {
        EditorPane::from_bytes(
            Target::Box { sid: "1".into(), rel: name.into() },
            text.as_bytes().to_vec(),
            false,
        )
        .unwrap()
    }

    /// The windowing pin: a large rust buffer computes spans for EVERY
    /// line, but `inject` pushes only the cursor-anchored window into
    /// `state.highlights` — rows far from the cursor have NO entries.
    #[test]
    fn inject_windows_highlights_around_the_cursor() {
        let src = rust_fixture(600); // 3000 lines
        let mut pane = pane_of("big.rs", &src);
        let fh = pane.fh.as_ref().expect("rust fixture must highlight");
        let lines = src.lines().count();
        assert!(fh.per_line.len() >= lines - 1, "span cache covers the file");
        assert!(
            fh.per_line[lines - 2].iter().count() > 0,
            "far rows HAVE spans in the cache"
        );
        pane.state.cursor = Index2::new(0, 0);
        inject(&mut pane.state, fh, HL_WINDOW);
        let max_row = pane.state.highlights.iter().map(|h| h.start.row).max().unwrap();
        assert!(
            max_row <= HL_WINDOW,
            "injected rows stay inside the window (max {max_row})"
        );
        let total: usize = fh.per_line.iter().map(Vec::len).sum();
        assert!(
            pane.state.highlights.len() < total,
            "windowed injection is a strict subset ({} of {total})",
            pane.state.highlights.len()
        );
        // Moving the cursor deep into the file re-centers the window.
        pane.state.cursor = Index2::new(2500, 0);
        let fh = pane.fh.take().unwrap();
        inject(&mut pane.state, &fh, HL_WINDOW);
        let min_row = pane.state.highlights.iter().map(|h| h.start.row).min().unwrap();
        assert!(min_row >= 2500 - HL_WINDOW, "window follows the cursor");
    }

    /// Open-highlight on a real terminal frame: colored (RGB) spans render
    /// near the viewport for a rust fixture.
    #[test]
    fn rust_buffer_renders_colored_spans() {
        let mut pane = pane_of("code.rs", &rust_fixture(20));
        let backend = TestBackend::new(90, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| pane.render(f, f.area(), true)).unwrap();
        let buf = term.backend().buffer().clone();
        let rgb = buf
            .content()
            .iter()
            .filter(|c| matches!(c.style().fg, Some(Color::Rgb(..))))
            .count();
        assert!(rgb > 10, "expected syntax-colored cells, got {rgb}");
    }

    /// Unknown extension = plain text: opens, renders, no highlights, no
    /// panic — the fallback the pane promises for arbitrary box files.
    #[test]
    fn unknown_extension_opens_plain() {
        let mut pane = pane_of("notes.xyz", "no such language\nsecond line\n");
        assert!(pane.fh.is_none(), "no highlighter for unknown ext");
        assert_eq!(pane.lang_label(), "plain");
        let backend = TestBackend::new(60, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| pane.render(f, f.area(), true)).unwrap();
        assert!(pane.state.highlights.is_empty());
    }

    /// Edit → dirty; save (mark_saved) → clean; the saved bytes round-trip
    /// the buffer byte-exactly, including the trailing newline.
    #[test]
    fn edit_sets_dirty_and_save_roundtrips_bytes() {
        let text = "line one\n\nline three\n"; // empty line + trailing \n
        let mut pane = pane_of("t.py", text);
        assert_eq!(pane.save_bytes(), text.as_bytes(), "load round-trips");
        assert!(!pane.dirty);
        // i<char>Esc — a real insert through the vim handler.
        pane.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        pane.handle_key(KeyCode::Char('X'), KeyModifiers::empty());
        pane.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert!(pane.dirty, "insert must set the dirty flag");
        assert_eq!(pane.save_bytes(), format!("X{text}").as_bytes());
        pane.mark_saved();
        assert!(!pane.dirty, "mark_saved clears dirty");
        // Undo back to the original: dirty again (hash differs from saved).
        pane.handle_key(KeyCode::Char('u'), KeyModifiers::empty());
        assert_eq!(pane.save_bytes(), text.as_bytes());
        assert!(pane.dirty, "undo past the save point re-dirties");
    }

    /// Read-only: navigation passes, mutations refuse loudly and change
    /// nothing, and Ctrl-S refuses instead of saving.
    #[test]
    fn read_only_whitelists_navigation_and_refuses_mutation() {
        let mut pane = pane_of("r.py", "a = 1\nb = 2\n");
        pane.read_only = true;
        let before = pane.save_bytes();
        assert_eq!(
            pane.handle_key(KeyCode::Char('j'), KeyModifiers::empty()),
            KeyResult::Consumed
        );
        assert_eq!(pane.state.cursor.row, 1, "navigation works read-only");
        pane.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert_eq!(pane.state.mode, EditorMode::Normal, "'i' must not enter insert");
        pane.handle_key(KeyCode::Char('x'), KeyModifiers::empty());
        assert_eq!(pane.save_bytes(), before, "buffer unchanged");
        assert!(pane.status.contains("read-only"), "loud refusal: {}", pane.status);
        assert!(!pane.dirty);
        assert_eq!(
            pane.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            KeyResult::Consumed,
            "Ctrl-S refuses in read-only"
        );
    }

    /// The debounce clock: an edit does NOT re-highlight synchronously (the
    /// span cache stays stale); after the deadline `maybe_rehighlight`
    /// recomputes it.
    #[test]
    fn rehighlight_debounces_after_edits() {
        let mut pane = pane_of("d.rs", "fn a() {}\n");
        let before = pane.fh.as_ref().unwrap().per_line.len();
        // o<newline text>Esc: append a new fn line.
        pane.handle_key(KeyCode::Char('o'), KeyModifiers::empty());
        for c in "fn b() {}".chars() {
            pane.handle_key(KeyCode::Char(c), KeyModifiers::empty());
        }
        pane.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert!(pane.rehighlight_pending(), "edit arms the debounce");
        assert_eq!(
            pane.fh.as_ref().unwrap().per_line.len(),
            before,
            "no synchronous re-highlight on edit"
        );
        // Before the deadline: still stale.
        pane.maybe_rehighlight(Instant::now());
        assert!(pane.rehighlight_pending(), "quiet period not over");
        // At/after the deadline: recomputed for the grown buffer.
        pane.maybe_rehighlight(Instant::now() + REHL_DEBOUNCE + Duration::from_millis(50));
        assert!(!pane.rehighlight_pending());
        assert!(
            pane.fh.as_ref().unwrap().per_line.len() > before,
            "debounced re-highlight covers the new line"
        );
    }

    /// Loud refusals: binary, non-UTF-8 and oversized buffers never mount.
    #[test]
    fn refuses_binary_nonutf8_oversized() {
        let bin = EditorPane::from_bytes(
            Target::Host(PathBuf::from("/x.bin")),
            b"a\0b".to_vec(),
            false,
        );
        assert!(bin.err().expect("must refuse").to_string().contains("binary"));
        let bad = EditorPane::from_bytes(
            Target::Host(PathBuf::from("/x.txt")),
            vec![0xff, 0xfe, b'a'],
            false,
        );
        assert!(bad.err().expect("must refuse").to_string().contains("UTF-8"));
        let big = EditorPane::from_bytes(
            Target::Host(PathBuf::from("/big.txt")),
            vec![b'a'; MAX_EDIT_BYTES + 1],
            false,
        );
        assert!(big.err().expect("must refuse").to_string().contains("cap"));
    }

    /// Key-result contract for the UI: z zooms, Esc closes (normal mode
    /// only), Ctrl-S saves, Ctrl-O prompts, insert-mode Esc stays inside.
    #[test]
    fn key_results_route_pane_controls() {
        let mut pane = pane_of("k.md", "# t\n");
        assert_eq!(
            pane.handle_key(KeyCode::Char('z'), KeyModifiers::empty()),
            KeyResult::ToggleFull
        );
        assert_eq!(
            pane.handle_key(KeyCode::Esc, KeyModifiers::empty()),
            KeyResult::Close
        );
        assert_eq!(
            pane.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            KeyResult::Save
        );
        assert_eq!(
            pane.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL),
            KeyResult::OpenPrompt
        );
        pane.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert_eq!(pane.state.mode, EditorMode::Insert);
        assert_eq!(
            pane.handle_key(KeyCode::Esc, KeyModifiers::empty()),
            KeyResult::Consumed,
            "insert-mode Esc goes to edtui, not the pane"
        );
        assert_eq!(pane.state.mode, EditorMode::Normal);
        // 'z' typed IN INSERT MODE is text, not zoom.
        pane.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert_eq!(
            pane.handle_key(KeyCode::Char('z'), KeyModifiers::empty()),
            KeyResult::Consumed
        );
        assert!(text_of(&pane.state).contains('z'));
    }
}
