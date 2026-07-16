// Syntax-aware text editor pane (the Edit pane's engine): edtui's vim-modal
// editor widget plus one analysis provider per document grammar. Shell text is
// analyzed only by the installed `sarun_brush` relation; syntastica remains a
// provider for the other, not-yet-translated languages.
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
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use crate::prolog::{BrushDocumentRequest, Completion, Span, analyze_brush_document};
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

// ── document analysis → edtui bridge ────────────────────────────────────────

/// Per-line styled spans, precomputed for the whole buffer.
/// (start_col, end_col_inclusive, style) — cols are CHAR indices, matching
/// edtui's Index2 addressing.
pub struct FileHighlights {
    pub per_line: Vec<Vec<(usize, usize, Style)>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnalysisProvider {
    Plain,
    Brush,
    Syntastica(Lang),
}

/// One provider by file extension. Unknown extensions are plain text. Bash is
/// intentionally absent from the syntastica branch: once selected, the Brush
/// relation is the only authority for parsing and presentation evidence.
/// NOTE: no toml — syntastica-parsers' crates.io pack lacks the grammars
/// that aren't published on crates.io (toml, dockerfile, kotlin, …).
fn provider_for_path(path: &str) -> AnalysisProvider {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str());
    match ext {
        Some("sh" | "bash") => AnalysisProvider::Brush,
        Some("rs") => AnalysisProvider::Syntastica(Lang::Rust),
        Some("py") => AnalysisProvider::Syntastica(Lang::Python),
        Some("c" | "h") => AnalysisProvider::Syntastica(Lang::C),
        Some("js" | "mjs") => AnalysisProvider::Syntastica(Lang::Javascript),
        Some("md") => AnalysisProvider::Syntastica(Lang::Markdown),
        Some("json") => AnalysisProvider::Syntastica(Lang::Json),
        Some("yml" | "yaml") => AnalysisProvider::Syntastica(Lang::Yaml),
        _ => AnalysisProvider::Plain,
    }
}

/// Human tag for the pane title.
fn provider_label(provider: AnalysisProvider) -> &'static str {
    match provider {
        AnalysisProvider::Brush => "brush",
        AnalysisProvider::Syntastica(Lang::Rust) => "rust",
        AnalysisProvider::Syntastica(Lang::Python) => "python",
        AnalysisProvider::Syntastica(Lang::C) => "c",
        AnalysisProvider::Syntastica(Lang::Javascript) => "javascript",
        AnalysisProvider::Syntastica(Lang::Markdown) => "markdown",
        AnalysisProvider::Syntastica(Lang::Json) => "json",
        AnalysisProvider::Syntastica(Lang::Yaml) => "yaml",
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

fn brush_style(syntax: &str) -> Style {
    match syntax {
        "keyword" => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        "operator" => Style::default().fg(Color::Cyan),
        "variable" => Style::default().fg(Color::LightCyan),
        "string" => Style::default().fg(Color::Green),
        "delimiter" => Style::default().fg(Color::Yellow),
        "escape" | "escaped" => Style::default().fg(Color::LightYellow),
        "command" => Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
        "arithmetic" => Style::default().fg(Color::LightMagenta),
        "trivia" => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Gray),
    }
}

/// Convert relation-owned UTF-8 byte spans to edtui's per-line character
/// coordinates. Newline bytes are not paintable cells, so a multi-line span
/// is split into one segment per non-empty line intersection.
fn relation_highlights(code: &str, highlights: &[crate::prolog::Highlight]) -> FileHighlights {
    let mut line_starts = vec![0usize];
    for (offset, byte) in code.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(offset + 1);
        }
    }
    let mut per_line = vec![Vec::new(); line_starts.len()];
    for highlight in highlights {
        if highlight.span.start >= highlight.span.end
            || highlight.span.end > code.len()
            || !code.is_char_boundary(highlight.span.start)
            || !code.is_char_boundary(highlight.span.end)
        {
            continue;
        }
        let first_row = line_starts
            .partition_point(|&start| start <= highlight.span.start)
            .saturating_sub(1);
        let end_row = line_starts.partition_point(|&start| start < highlight.span.end);
        for row in first_row..end_row {
            let line_start = line_starts[row];
            let line_end = line_starts
                .get(row + 1)
                .map_or(code.len(), |next| next.saturating_sub(1));
            let start = highlight.span.start.max(line_start);
            let end = highlight.span.end.min(line_end);
            if start < end {
                let c0 = code[line_start..start].chars().count();
                let c1 = c0 + code[start..end].chars().count() - 1;
                per_line[row].push((c0, c1, brush_style(&highlight.syntax)));
            }
        }
    }
    FileHighlights { per_line }
}

fn brush_analysis(
    code: &str,
    assist: Option<Span>,
) -> Result<crate::prolog::BrushDocumentAnalysis, String> {
    analyze_brush_document(&BrushDocumentRequest {
        source: code.to_string(),
        assist,
        initial_bindings: vec![],
        observations: vec![],
    })
}

fn brush_file_highlights(code: &str) -> Result<FileHighlights, String> {
    let analysis = brush_analysis(code, None)?;
    Ok(brush_analysis_highlights(code, &analysis))
}

fn brush_analysis_highlights(
    code: &str,
    analysis: &crate::prolog::BrushDocumentAnalysis,
) -> FileHighlights {
    let mut highlights = analysis
        .candidates
        .iter()
        .flat_map(|candidate| candidate.highlights.iter().cloned())
        .collect::<Vec<_>>();
    highlights.sort_by(|left, right| {
        (
            left.span.start,
            left.span.end,
            &left.syntax,
            &left.semantic,
            &left.origin,
        )
            .cmp(&(
                right.span.start,
                right.span.end,
                &right.syntax,
                &right.semantic,
                &right.origin,
            ))
    });
    highlights.dedup();
    relation_highlights(code, &highlights)
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
    Box {
        sid: String,
        rel: String,
    },
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

#[derive(Clone, Debug)]
struct CompletionMenu {
    items: Vec<Completion>,
    selected: usize,
}

struct HighlightAnalysisResult {
    revision: u64,
    buffer_hash: u64,
    result: Result<FileHighlights, String>,
}

/// The editor pane: one open buffer with its edtui state, the precomputed
/// highlight cache, dirty tracking (buffer hash vs last-saved hash) and the
/// debounced re-highlight clock. The SAME `render` draws the right-pane and
/// fullscreen mounts — only the target Rect differs.
pub struct EditorPane {
    pub target: Target,
    pub state: EditorState,
    handler: EditorEventHandler,
    provider: AnalysisProvider,
    fh: Option<FileHighlights>,
    completions: Option<CompletionMenu>,
    analysis_tx: mpsc::Sender<HighlightAnalysisResult>,
    analysis_rx: mpsc::Receiver<HighlightAnalysisResult>,
    revision: u64,
    analysis_pending: Option<u64>,
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
        let provider = provider_for_path(&label);
        let (fh, analysis_error) = match provider {
            AnalysisProvider::Plain => (None, None),
            AnalysisProvider::Syntastica(lang) => (Some(compute(&text, lang)), None),
            AnalysisProvider::Brush => match brush_file_highlights(&text) {
                Ok(highlights) => (Some(highlights), None),
                Err(error) => (Some(FileHighlights { per_line: vec![] }), Some(error)),
            },
        };
        let state = EditorState::new(Lines::from(text.as_str()));
        let h = hash_of(&text);
        let (analysis_tx, analysis_rx) = mpsc::channel();
        let status = analysis_error.map_or_else(
            || "vim keys · Ctrl-S save · Ctrl-O open · Ctrl-E lock · z zoom · Esc back".into(),
            |error| format!("brush relation: {error}"),
        );
        Ok(EditorPane {
            target,
            state,
            handler: EditorEventHandler::default(),
            provider,
            fh,
            completions: None,
            analysis_tx,
            analysis_rx,
            revision: 0,
            analysis_pending: None,
            saved_hash: h,
            buf_hash: h,
            dirty: false,
            read_only,
            rehl_due: None,
            status,
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
        let bytes =
            std::fs::read(&path).map_err(|e| anyhow::anyhow!("editor: {}: {e}", path.display()))?;
        let ro = md.permissions().readonly();
        Self::from_bytes(Target::Host(path), bytes, ro)
    }

    /// Standalone editor open: unlike the UI's explicit existing-file prompt,
    /// a shell `edit new.sh` may create a new regular file on first save.
    pub fn open_host_or_new(path: PathBuf) -> anyhow::Result<Self> {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    anyhow::bail!("editor: {} is a symlink — open the target", path.display());
                }
                if !metadata.is_file() {
                    anyhow::bail!("editor: {} is not a regular file", path.display());
                }
                let bytes = std::fs::read(&path)
                    .map_err(|error| anyhow::anyhow!("editor: {}: {error}", path.display()))?;
                Self::from_bytes(Target::Host(path), bytes, metadata.permissions().readonly())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Self::from_bytes(Target::Host(path), vec![], false)
            }
            Err(error) => Err(anyhow::anyhow!("editor: {}: {error}", path.display())),
        }
    }

    /// Open a BOX file's current bytes (fetched by the UI over
    /// `review.file_bytes`). Saves go back over `review.write_file`.
    pub fn open_box(sid: String, rel: String, bytes: Vec<u8>) -> anyhow::Result<Self> {
        Self::from_bytes(Target::Box { sid, rel }, bytes, false)
    }

    pub fn lang_label(&self) -> &'static str {
        provider_label(self.provider)
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

    /// Apply completed analysis and request new highlights once the debounce
    /// deadline passes. Brush work runs away from the render thread and is
    /// accepted only when its revision and buffer hash still match.
    pub fn maybe_rehighlight(&mut self, now: Instant) {
        self.accept_highlight_analysis();
        if let Some(due) = self.rehl_due {
            if now >= due {
                self.rehl_due = None;
                let text = text_of(&self.state);
                match self.provider {
                    AnalysisProvider::Plain => self.fh = None,
                    AnalysisProvider::Syntastica(lang) => {
                        self.fh = Some(compute(&text, lang));
                    }
                    AnalysisProvider::Brush => self.request_brush_highlights(text),
                }
            }
        }
    }

    fn request_brush_highlights(&mut self, text: String) {
        let revision = self.revision;
        let buffer_hash = self.buf_hash;
        let tx = self.analysis_tx.clone();
        self.analysis_pending = Some(revision);
        let spawn = std::thread::Builder::new()
            .name("sarun-editor-analysis".into())
            .spawn(move || {
                let result = brush_file_highlights(&text);
                let _ = tx.send(HighlightAnalysisResult {
                    revision,
                    buffer_hash,
                    result,
                });
            });
        if let Err(error) = spawn {
            self.analysis_pending = None;
            self.status = format!("brush relation worker: {error}");
        }
    }

    fn accept_highlight_analysis(&mut self) {
        while let Ok(completed) = self.analysis_rx.try_recv() {
            if completed.revision != self.revision || completed.buffer_hash != self.buf_hash {
                continue;
            }
            self.analysis_pending = None;
            match completed.result {
                Ok(highlights) => {
                    self.fh = Some(highlights);
                    self.status = "Brush relation analysis current".into();
                }
                Err(error) => {
                    self.fh = Some(FileHighlights { per_line: vec![] });
                    self.status = format!("brush relation: {error}");
                }
            }
        }
    }

    /// True while an edit is waiting out the debounce quiet period.
    pub fn rehighlight_pending(&self) -> bool {
        self.rehl_due.is_some() || self.analysis_pending.is_some()
    }

    fn cursor_byte_offset(&self) -> usize {
        let mut offset = 0usize;
        for (row, line) in self.state.lines.iter_row().enumerate() {
            if row == self.state.cursor.row {
                offset += line
                    .iter()
                    .take(self.state.cursor.col)
                    .map(|ch| ch.len_utf8())
                    .sum::<usize>();
                return offset;
            }
            offset += line.iter().map(|ch| ch.len_utf8()).sum::<usize>() + 1;
        }
        offset
    }

    fn open_completions(&mut self) {
        let text = text_of(&self.state);
        let cursor = self.cursor_byte_offset();
        let analysis = match brush_analysis(
            &text,
            Some(Span {
                start: cursor,
                end: cursor,
            }),
        ) {
            Ok(analysis) => analysis,
            Err(error) => {
                self.completions = None;
                self.status = format!("brush relation: {error}");
                return;
            }
        };
        self.fh = Some(brush_analysis_highlights(&text, &analysis));
        let mut items = analysis
            .candidates
            .iter()
            .flat_map(|candidate| candidate.completions.iter().cloned())
            .filter(|completion| {
                completion.replace.start <= completion.replace.end
                    && completion.replace.end == cursor
                    && completion.replace.end <= text.len()
                    && text.is_char_boundary(completion.replace.start)
                    && text.is_char_boundary(completion.replace.end)
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            (left.rank, std::cmp::Reverse(left.preference), &left.insert).cmp(&(
                right.rank,
                std::cmp::Reverse(right.preference),
                &right.insert,
            ))
        });
        items.dedup_by(|left, right| left.replace == right.replace && left.insert == right.insert);
        if items.is_empty() {
            self.completions = None;
            self.status = "brush relation: no completion at cursor".into();
        } else {
            self.status = format!(
                "{} relation completion(s) · Tab/↓ next · Enter accept · Esc close",
                items.len()
            );
            self.completions = Some(CompletionMenu { items, selected: 0 });
        }
    }

    fn handle_completion_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        let Some(menu) = self.completions.as_mut() else {
            return false;
        };
        if !mods.is_empty() && code != KeyCode::BackTab {
            self.completions = None;
            return false;
        }
        match code {
            KeyCode::Tab | KeyCode::Down => {
                menu.selected = (menu.selected + 1) % menu.items.len();
                true
            }
            KeyCode::BackTab | KeyCode::Up => {
                menu.selected = menu.selected.checked_sub(1).unwrap_or(menu.items.len() - 1);
                true
            }
            KeyCode::Enter => {
                self.apply_selected_completion();
                true
            }
            KeyCode::Esc => {
                self.completions = None;
                self.status = "completion closed".into();
                true
            }
            _ => {
                self.completions = None;
                false
            }
        }
    }

    fn apply_selected_completion(&mut self) {
        if self.read_only {
            self.completions = None;
            self.status = "read-only — relation completion insertion refused".into();
            return;
        }
        let Some(completion) = self
            .completions
            .as_ref()
            .and_then(|menu| menu.items.get(menu.selected))
            .cloned()
        else {
            return;
        };
        let text = text_of(&self.state);
        let cursor = self.cursor_byte_offset();
        if completion.replace.end != cursor
            || completion.replace.start > completion.replace.end
            || completion.replace.end > text.len()
            || !text.is_char_boundary(completion.replace.start)
            || !text.is_char_boundary(completion.replace.end)
            || completion.insert.contains('\n')
        {
            self.completions = None;
            self.status = "brush relation returned an inapplicable completion span".into();
            return;
        }
        let delete_chars = text[completion.replace.start..completion.replace.end]
            .chars()
            .count();
        for _ in 0..delete_chars {
            self.handler.on_key_event(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
                &mut self.state,
            );
        }
        for ch in completion.insert.chars() {
            self.handler.on_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
                &mut self.state,
            );
        }
        self.completions = None;
        self.after_key();
        self.status = format!("inserted relation completion {}", completion.display);
    }

    #[cfg(test)]
    fn completion_items(&self) -> &[Completion] {
        self.completions
            .as_ref()
            .map_or(&[], |menu| menu.items.as_slice())
    }

    /// Handle one key. The caller (ui.rs) already consumed the F-keys; an
    /// OPEN editor consumes everything else — normal-mode letters are vim
    /// motions/operators, so pane accelerators intentionally do NOT fire
    /// from inside a buffer (leave with Esc, the chips, or F9).
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        if self.handle_completion_key(code, mods) {
            return KeyResult::Consumed;
        }
        if self.provider == AnalysisProvider::Brush
            && self.state.mode == EditorMode::Insert
            && mods.is_empty()
            && code == KeyCode::Tab
        {
            self.open_completions();
            return KeyResult::Consumed;
        }
        self.completions = None;
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
            self.revision = self.revision.wrapping_add(1);
            self.dirty = h != self.saved_hash;
            if self.provider != AnalysisProvider::Plain {
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
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
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
        self.render_completion_menu(f, area);
    }

    /// Standalone presentation deliberately has no surrounding pane frame.
    /// It leaves the final row for a compact path/status line, giving the text
    /// editor every other terminal cell and keeping the UI-pane chrome out of
    /// a user's ordinary terminal copy surface.
    fn render_standalone(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::layout::{Constraint, Layout};
        use ratatui::widgets::Paragraph;

        self.maybe_rehighlight(Instant::now());
        if let Some(fh) = &self.fh {
            inject(&mut self.state, fh, HL_WINDOW);
        }
        let [editor_area, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
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
        f.render_widget(view, editor_area);
        f.render_widget(
            Paragraph::new(format!("{} · {}", self.title(), self.status))
                .style(Style::default().fg(Color::DarkGray)),
            status_area,
        );
        self.render_completion_menu(f, editor_area);
    }

    fn render_completion_menu(&self, f: &mut ratatui::Frame, editor_area: ratatui::layout::Rect) {
        use ratatui::widgets::{Block, Borders, Clear, List, ListItem};

        let Some(menu) = &self.completions else {
            return;
        };
        let Some(cursor) = self.state.cursor_screen_position() else {
            return;
        };
        let visible = menu.items.len().min(8);
        let desired_width = menu
            .items
            .iter()
            .take(visible)
            .map(|completion| completion.display.chars().count())
            .max()
            .unwrap_or(1)
            .saturating_add(4)
            .max(24);
        let width = desired_width.min(editor_area.width.saturating_sub(2) as usize) as u16;
        let height = visible as u16 + 2;
        if width < 3 || height > editor_area.height {
            return;
        }
        let x = cursor.x.min(editor_area.right().saturating_sub(width));
        let below = cursor.y.saturating_add(1);
        let y = if below.saturating_add(height) <= editor_area.bottom() {
            below
        } else {
            cursor.y.saturating_sub(height)
        };
        let area = ratatui::layout::Rect::new(x, y, width, height);
        let start = menu.selected.saturating_sub(visible - 1);
        let items = menu.items[start..(start + visible).min(menu.items.len())]
            .iter()
            .enumerate()
            .map(|(offset, completion)| {
                let selected = start + offset == menu.selected;
                let style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(completion.display.clone()).style(style)
            })
            .collect::<Vec<_>>();
        f.render_widget(Clear, area);
        f.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" relation completions "),
            ),
            area,
        );
    }
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        use crossterm::{cursor, execute, terminal};
        if let Ok(mut tty) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        {
            let _ = execute!(tty, cursor::Show, terminal::LeaveAlternateScreen);
        }
        let _ = terminal::disable_raw_mode();
    }
}

fn save_standalone_host(pane: &mut EditorPane) -> anyhow::Result<()> {
    let Target::Host(path) = &pane.target else {
        anyhow::bail!("standalone editor received a non-host persistence target");
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("editor: {} became a symlink — save refused", path.display());
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!("editor: {} is no longer a regular file", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(anyhow::anyhow!("editor: {}: {error}", path.display())),
    }
    std::fs::write(path, pane.save_bytes())
        .map_err(|error| anyhow::anyhow!("editor: save {}: {error}", path.display()))?;
    pane.mark_saved();
    Ok(())
}

/// Run the same editor pane as a foreground terminal application. Crossterm's
/// Unix event source and raw-mode implementation use `/dev/tty`; the Ratatui
/// backend is explicitly attached there as well, so shell redirections remain
/// ordinary command I/O and cannot capture or corrupt the TUI.
pub fn run_standalone(path: PathBuf) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::{cursor, execute, terminal};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use std::io::IsTerminal;

    let mut pane = EditorPane::open_host_or_new(path)?;
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| anyhow::anyhow!("editor: controlling terminal: {error}"))?;
    if !tty.is_terminal() {
        anyhow::bail!("editor: controlling terminal is not a TTY");
    }
    terminal::enable_raw_mode()
        .map_err(|error| anyhow::anyhow!("editor: enable raw mode: {error}"))?;
    let _restore = TerminalRestore;
    execute!(tty, terminal::EnterAlternateScreen, cursor::Hide)
        .map_err(|error| anyhow::anyhow!("editor: enter terminal screen: {error}"))?;
    let backend = CrosstermBackend::new(tty);
    let mut terminal = Terminal::new(backend)
        .map_err(|error| anyhow::anyhow!("editor: initialize terminal: {error}"))?;
    let mut discard_armed = false;
    let mut redraw = true;

    loop {
        if redraw {
            terminal
                .draw(|frame| pane.render_standalone(frame, frame.area()))
                .map_err(|error| anyhow::anyhow!("editor: draw: {error}"))?;
            redraw = false;
        }
        let analysis_tick = pane.rehighlight_pending();
        if !event::poll(if analysis_tick {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(60)
        })
        .map_err(|error| anyhow::anyhow!("editor: terminal poll: {error}"))?
        {
            redraw = analysis_tick;
            continue;
        }
        let event =
            event::read().map_err(|error| anyhow::anyhow!("editor: terminal input: {error}"))?;
        let key = match event {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                terminal
                    .autoresize()
                    .map_err(|error| anyhow::anyhow!("editor: resize terminal: {error}"))?;
                redraw = true;
                continue;
            }
            _ => continue,
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        redraw = true;
        let closing_key = key.code == KeyCode::Esc;
        match pane.handle_key(key.code, key.modifiers) {
            KeyResult::Save => match save_standalone_host(&mut pane) {
                Ok(()) => discard_armed = false,
                Err(error) => pane.status = error.to_string(),
            },
            KeyResult::Close => {
                if pane.dirty && !discard_armed {
                    pane.status = "unsaved changes · Esc again to discard · Ctrl-S saves".into();
                    discard_armed = true;
                } else {
                    break;
                }
            }
            KeyResult::OpenPrompt => {
                pane.status = "standalone editor opens one path per invocation".into();
            }
            KeyResult::ToggleFull => {
                pane.status = "standalone editor is already fullscreen".into();
            }
            KeyResult::Consumed | KeyResult::NotHandled => {
                if !closing_key {
                    discard_armed = false;
                }
            }
        }
    }
    Ok(())
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
            Target::Box {
                sid: "1".into(),
                rel: name.into(),
            },
            text.as_bytes().to_vec(),
            false,
        )
        .unwrap()
    }

    #[test]
    fn relation_byte_spans_map_to_utf8_editor_cells() {
        let source = "éx\n$A\n";
        let highlights = relation_highlights(
            source,
            &[
                crate::prolog::Highlight {
                    span: Span { start: 0, end: 3 },
                    syntax: "string".into(),
                    semantic: "text".into(),
                    origin: "grammar".into(),
                },
                crate::prolog::Highlight {
                    span: Span { start: 4, end: 6 },
                    syntax: "variable".into(),
                    semantic: "parameter".into(),
                    origin: "grammar".into(),
                },
            ],
        );
        assert_eq!(
            highlights.per_line[0]
                .iter()
                .map(|(start, end, _)| (*start, *end))
                .collect::<Vec<_>>(),
            vec![(0, 1)]
        );
        assert_eq!(
            highlights.per_line[1]
                .iter()
                .map(|(start, end, _)| (*start, *end))
                .collect::<Vec<_>>(),
            vec![(0, 1)]
        );
    }

    #[test]
    fn bash_editor_uses_relation_for_backward_completion_and_insertion() {
        let mut pane = pane_of("script.sh", "A=\"\"; find . -type $A");
        assert_eq!(pane.provider, AnalysisProvider::Brush);
        assert_eq!(pane.lang_label(), "brush");
        assert!(
            pane.fh
                .as_ref()
                .is_some_and(|highlights| highlights.per_line.iter().any(|line| !line.is_empty())),
            "the Brush relation must provide the initial highlights"
        );

        pane.state.mode = EditorMode::Insert;
        pane.state.cursor = Index2::new(0, 3);
        assert_eq!(
            pane.handle_key(KeyCode::Tab, KeyModifiers::empty()),
            KeyResult::Consumed
        );
        for expected in ["D", "b", "c", "d", "f", "l", "p", "s"] {
            assert!(
                pane.completion_items()
                    .iter()
                    .any(|completion| completion.insert == expected),
                "missing relation completion {expected:?}"
            );
        }
        let backend = TestBackend::new(90, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| pane.render(frame, frame.area(), true))
            .unwrap();
        let mut rendered = String::new();
        for cell in term.backend().buffer().content() {
            rendered.push_str(cell.symbol());
        }
        assert!(
            rendered.contains("relation completions"),
            "Tab must render the completion popup, not merely cache candidates"
        );
        let selected = pane
            .completion_items()
            .iter()
            .position(|completion| completion.insert == "f")
            .unwrap();
        pane.completions.as_mut().unwrap().selected = selected;
        assert_eq!(
            pane.handle_key(KeyCode::Enter, KeyModifiers::empty()),
            KeyResult::Consumed
        );
        assert_eq!(text_of(&pane.state), "A=\"f\"; find . -type $A");
        assert_eq!(pane.state.cursor, Index2::new(0, 4));
        assert!(pane.dirty);
        assert!(pane.completions.is_none());
    }

    #[test]
    fn bash_editor_completes_visible_local_variable_after_dollar() {
        let source = "#!/bin/bash\nA=\"\"\nfind . -type $";
        let mut pane = pane_of("script.sh", source);
        pane.state.mode = EditorMode::Insert;
        pane.state.cursor = Index2::new(2, 14);
        assert_eq!(
            pane.handle_key(KeyCode::Tab, KeyModifiers::empty()),
            KeyResult::Consumed
        );
        let selected = pane
            .completion_items()
            .iter()
            .position(|completion| completion.insert == "A")
            .expect("ordinary Brush relation omitted the local A binding");
        pane.completions.as_mut().unwrap().selected = selected;
        assert_eq!(
            pane.handle_key(KeyCode::Enter, KeyModifiers::empty()),
            KeyResult::Consumed
        );
        assert_eq!(text_of(&pane.state), "#!/bin/bash\nA=\"\"\nfind . -type $A");
    }

    #[test]
    fn bash_editor_never_falls_back_when_relation_bound_is_exceeded() {
        let source = "x".repeat(crate::prolog::MAX_DOCUMENT_INPUT_BYTES + 1);
        let pane = pane_of("large.sh", &source);
        assert_eq!(pane.provider, AnalysisProvider::Brush);
        assert!(
            pane.status.contains("exceeds"),
            "status was: {}",
            pane.status
        );
        assert!(
            pane.fh
                .as_ref()
                .is_some_and(|highlights| highlights.per_line.is_empty()),
            "an explicit relation error must not invoke the Bash tree-sitter provider"
        );
    }

    #[test]
    fn read_only_editor_refuses_relation_completion_insertion() {
        let text = "A=\"\"; find . -type $A";
        let mut pane = pane_of("readonly.sh", text);
        pane.read_only = true;
        pane.state.mode = EditorMode::Insert;
        pane.state.cursor = Index2::new(0, 3);
        pane.handle_key(KeyCode::Tab, KeyModifiers::empty());
        assert!(!pane.completion_items().is_empty());
        pane.handle_key(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(text_of(&pane.state), text);
        assert!(pane.status.contains("refused"));
    }

    #[test]
    fn editor_discards_stale_background_relation_analysis() {
        let mut pane = pane_of("revision.sh", "echo old");
        pane.revision = 2;
        pane.analysis_pending = Some(2);
        pane.analysis_tx
            .send(HighlightAnalysisResult {
                revision: 1,
                buffer_hash: pane.buf_hash,
                result: Ok(FileHighlights {
                    per_line: vec![vec![]; 99],
                }),
            })
            .unwrap();
        pane.accept_highlight_analysis();
        assert_ne!(pane.fh.as_ref().unwrap().per_line.len(), 99);
        assert_eq!(pane.analysis_pending, Some(2));

        pane.analysis_tx
            .send(HighlightAnalysisResult {
                revision: 2,
                buffer_hash: pane.buf_hash,
                result: Ok(FileHighlights {
                    per_line: vec![vec![]; 7],
                }),
            })
            .unwrap();
        pane.accept_highlight_analysis();
        assert_eq!(pane.fh.as_ref().unwrap().per_line.len(), 7);
        assert_eq!(pane.analysis_pending, None);
    }

    #[test]
    fn standalone_editor_creates_and_saves_host_file() {
        let path = std::env::temp_dir().join(format!(
            "sarun_editor_builtin_{}_{}.sh",
            std::process::id(),
            hash_of(module_path!())
        ));
        let _ = std::fs::remove_file(&path);
        let mut pane = EditorPane::open_host_or_new(path.clone()).unwrap();
        pane.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        for ch in "echo ok\n".chars() {
            pane.handle_key(KeyCode::Char(ch), KeyModifiers::empty());
        }
        save_standalone_host(&mut pane).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo ok\n");
        assert!(!pane.dirty);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn standalone_editor_has_no_full_screen_pane_frame() {
        let mut pane = pane_of("script.sh", "echo ok");
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render_standalone(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            ['╔', '╗', '╚', '╝', '║', '═']
                .iter()
                .all(|border| !rendered.contains(*border))
        );
        assert!(rendered.contains("script.sh"));
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
        let max_row = pane
            .state
            .highlights
            .iter()
            .map(|h| h.start.row)
            .max()
            .unwrap();
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
        let min_row = pane
            .state
            .highlights
            .iter()
            .map(|h| h.start.row)
            .min()
            .unwrap();
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
        assert_eq!(
            pane.state.mode,
            EditorMode::Normal,
            "'i' must not enter insert"
        );
        pane.handle_key(KeyCode::Char('x'), KeyModifiers::empty());
        assert_eq!(pane.save_bytes(), before, "buffer unchanged");
        assert!(
            pane.status.contains("read-only"),
            "loud refusal: {}",
            pane.status
        );
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
        assert!(
            bin.err()
                .expect("must refuse")
                .to_string()
                .contains("binary")
        );
        let bad = EditorPane::from_bytes(
            Target::Host(PathBuf::from("/x.txt")),
            vec![0xff, 0xfe, b'a'],
            false,
        );
        assert!(
            bad.err()
                .expect("must refuse")
                .to_string()
                .contains("UTF-8")
        );
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
