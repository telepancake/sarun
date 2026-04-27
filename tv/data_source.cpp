/* data_source.cpp - implements data_source.h.
 *
 * Each mode rebuilds its panel from a SQL query. Heavy aggregations
 * are kept in tv_idx_* tables (built lazily via TvDb::ensure_*()).
 *
 * Tree views (process hierarchy, file path tree) build a small
 * adjacency map from the result set and DFS in C++ - no recursive CTE
 * needed at the row-emit layer (recursive CTEs are reserved for
 * dep/rdep closure traversal in modes 4..7).
 */

#include "data_source.h"
#include "tv_db.h"

#include <algorithm>
#include <cctype>
#include <cstdarg>
#include <cstdio>
#include <cstring>
#include <functional>
#include <map>
#include <memory>
#include <set>
#include <sstream>
#include <unordered_map>
#include <unordered_set>

namespace {

/* -- tiny formatting helpers ---------------------------------------- */

std::string sfmt(const char *fmt, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    std::vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    return buf;
}

std::string format_duration_ns(int64_t start_ns, int64_t end_ns) {
    if (end_ns < start_ns) return "";
    double s = (end_ns - start_ns) / 1e9;
    char buf[32];
    if (s < 0.001)      std::snprintf(buf, sizeof buf, "%.0f us", s * 1e6);
    else if (s < 1.0)   std::snprintf(buf, sizeof buf, "%.0f ms", s * 1000);
    else if (s < 60.0)  std::snprintf(buf, sizeof buf, "%.2f s", s);
    else if (s < 3600)  std::snprintf(buf, sizeof buf, "%.1f m", s / 60);
    else                std::snprintf(buf, sizeof buf, "%.1f h", s / 3600);
    return buf;
}

std::string format_exit(const std::string &kind, const std::string &code,
                        const std::string &core, bool ended) {
    if (!ended) return "running";
    if (kind.empty()) return "-";
    int k = std::atoi(kind.c_str());
    int c = std::atoi(code.c_str());
    if (k == 1) {
        std::string s = sfmt("sig %d", c);
        if (core == "true" || core == "1") s += " (core)";
        return s;
    }
    return c == 0 ? "ok" : sfmt("exit %d", c);
}

std::string sql_escape(const std::string &s) {
    std::string out = "'";
    for (char c : s) {
        if (c == '\'') out += "''";
        else out += c;
    }
    out += "'";
    return out;
}

std::string basename_of(const std::string &p) {
    auto pos = p.find_last_of('/');
    return pos == std::string::npos ? p : p.substr(pos + 1);
}

/* Compact number formatting: 0, 374, 26.8k, 1.56M, 374G, ...
 * Used in column displays where horizontal space is tight - the user
 * called out that "5 opens, 3 procs" wastes a fixed-width column with
 * unit text repeated on every row. */
std::string compact_count(uint64_t n) {
    if (n < 1000) return std::to_string(n);
    static const char *suf[] = {"k", "M", "G", "T", "P"};
    double d = (double)n / 1000.0;
    int idx = 0;
    while (d >= 1000.0 && idx + 1 < (int)(sizeof suf / sizeof suf[0])) {
        d /= 1000.0; idx++;
    }
    char buf[32];
    if      (d >= 100) std::snprintf(buf, sizeof buf, "%.0f%s", d, suf[idx]);
    else if (d >= 10)  std::snprintf(buf, sizeof buf, "%.1f%s", d, suf[idx]);
    else               std::snprintf(buf, sizeof buf, "%.2f%s", d, suf[idx]);
    return buf;
}

/* -- flag filter grammar -------------------------------------------- *
 *
 * The user asked for require / forbid / all-of / any-of semantics on
 * one input line. The grammar is small:
 *
 *   bare letter   require flag    "W"      = must have W
 *   '+' letter    require flag    "+W"     = must have W
 *   '-' letter    forbid  flag    "-E"     = must NOT have E
 *   juxtaposition AND group       "+W-E"   = must have W AND not E
 *   ','           group separator "+W,+R"  = (must have W) OR (must have R)
 *
 * Whitespace is ignored. The signed prefix ('+' or '-') applies only
 * to the letter that follows, then resets. Bare letters keep working
 * as legacy "+letter" so old habits don't break.
 *
 * A FlagSpec is a disjunction of conjunctions. Each builder maps the
 * letters to mode-specific SQL via a callback, and build_flag_sql()
 * assembles "(a AND NOT b) OR (c)" expressions. */

struct FlagPred { char letter; bool require; };
struct FlagSpec { std::vector<std::vector<FlagPred>> groups; };

FlagSpec parse_flag_spec(const std::string &s) {
    FlagSpec fs;
    std::vector<FlagPred> cur;
    bool sign = true;            /* default: require */
    for (char c : s) {
        if (c == ' ' || c == '\t') continue;
        if (c == ',') {
            if (!cur.empty()) { fs.groups.push_back(std::move(cur)); cur.clear(); }
            sign = true;
            continue;
        }
        if (c == '+') { sign = true;  continue; }
        if (c == '-') { sign = false; continue; }
        if (std::isalpha((unsigned char)c)) {
            cur.push_back({c, sign});
            sign = true;          /* sign applies to one letter only */
        }
    }
    if (!cur.empty()) fs.groups.push_back(std::move(cur));
    return fs;
}

bool flag_spec_uses(const FlagSpec &fs, char l) {
    for (auto &g : fs.groups)
        for (auto &p : g)
            if (p.letter == l) return true;
    return false;
}

template <typename Fn>
std::string build_flag_sql(const FlagSpec &fs, Fn letter_to_sql) {
    if (fs.groups.empty()) return "";
    std::string out;
    for (auto &g : fs.groups) {
        std::string conj;
        for (auto &p : g) {
            std::string e = letter_to_sql(p.letter);
            if (e.empty()) continue;
            std::string clause = p.require ? ("(" + e + ")")
                                           : ("NOT (" + e + ")");
            if (!conj.empty()) conj += " AND ";
            conj += clause;
        }
        if (conj.empty()) continue;
        if (!out.empty()) out += " OR ";
        out += "(" + conj + ")";
    }
    return out;
}

const char *style_for_exit(const std::string &kind, const std::string &code,
                           bool ended) {
    if (!ended) return "green";
    if (kind.empty()) return "dim";
    if (kind == "1") return "err";
    return code == "0" ? "ok" : "err";
}

RowStyle to_style(const char *s) {
    if (!s) return RowStyle::Normal;
    if (!std::strcmp(s, "err"))     return RowStyle::Error;
    if (!std::strcmp(s, "ok"))      return RowStyle::Normal;
    if (!std::strcmp(s, "green"))   return RowStyle::Green;
    if (!std::strcmp(s, "dim"))     return RowStyle::Dim;
    if (!std::strcmp(s, "head"))    return RowStyle::Heading;
    if (!std::strcmp(s, "yellow"))  return RowStyle::Yellow;
    return RowStyle::Normal;
}

RowData mk_row(const std::string &id, const std::string &text,
               RowStyle style = RowStyle::Normal) {
    RowData r;
    r.id = id;
    r.style = style;
    r.cols.push_back(text);
    return r;
}

RowData mk_kv(const std::string &id, const std::string &key,
              const std::string &val, RowStyle style = RowStyle::Normal) {
    /* Two-column row: the engine renders col[0] in a fixed-width key
     * column and col[1] in the flex value column.  link_id carries the
     * raw value for app-level navigation. */
    RowData r;
    r.id = id;
    r.style = style;
    r.cols.push_back(key);
    r.cols.push_back(val);
    r.link_id = val;
    return r;
}

/* OPEN flags -> human flags string (R/W/RW + O_CREAT/O_TRUNC hints). */
std::string flags_text(int flags) {
    std::string s;
    int acc = flags & 3;
    if      (acc == 0) s = "R";
    else if (acc == 1) s = "W";
    else if (acc == 2) s = "RW";
    else               s = "?";
    if (flags & 0x40)    s += "+create";
    if (flags & 0x200)   s += "+trunc";
    if (flags & 0x400)   s += "+append";
    if (flags & 0x80000) s += "+cloexec";
    return s;
}

/* Sanitize a captured stdout/stderr blob for inline display in a single
 * row.
 *
 * Three things matter, all reported as bugs:
 *   1. A single trailing '\n' (or "\r\n") is not interesting and showing
 *      it as " " stutters at the right edge of every line.  Strip one.
 *   2. CSI / SGR escape sequences (\x1b[...m, \x1b[K, etc.) shouldn't
 *      show as literal "^[[..." text - the engine's sput_field already
 *      ignores ANSI when measuring visible length, so we just have to
 *      let them through unmodified instead of mangling the leading ESC.
 *   3. Other control characters (LF/CR/TAB inside the line, plus C0
 *      controls) still wreck single-row layout, so those get folded to
 *      a single space.
 *
 * Visible truncation honours `max_visible` glyphs (UTF-8 leading bytes,
 * SGR escapes don't count).  When we cut, we emit a SGR reset before the
 * ellipsis so a colour from a half-included escape can't bleed into the
 * next column. */
std::string sanitize_output_line(const std::string &in,
                                 size_t max_visible) {
    std::string s = in;
    /* Strip exactly one trailing newline (and a CR before it if any). */
    if (!s.empty() && s.back() == '\n') s.pop_back();
    if (!s.empty() && s.back() == '\r') s.pop_back();

    std::string out;
    out.reserve(s.size());
    size_t visible = 0;
    bool truncated = false;
    bool saw_sgr = false;
    for (size_t i = 0; i < s.size(); ) {
        unsigned char c = static_cast<unsigned char>(s[i]);
        /* CSI: ESC [ ... <final 0x40-0x7e>.  Pass through verbatim;
         * sput_field already skips it for length accounting.  We also
         * accept lone ESC + final byte (Fp/Fe) as a courtesy. */
        if (c == 0x1b && i + 1 < s.size()) {
            size_t j = i + 1;
            if (s[j] == '[') {
                j++;
                while (j < s.size()) {
                    unsigned char fb = static_cast<unsigned char>(s[j]);
                    if (fb >= 0x40 && fb <= 0x7e) { j++; break; }
                    j++;
                }
            } else {
                j++; /* skip the second byte of the 2-char escape */
            }
            out.append(s, i, j - i);
            saw_sgr = true;
            i = j;
            continue;
        }
        if (c == '\n' || c == '\r' || c == '\t') {
            out += ' ';
            visible++;
            i++;
        } else if (c < 0x20 || c == 0x7f) {
            /* drop other control bytes */
            i++;
        } else if ((c & 0xc0) == 0x80) {
            /* utf-8 continuation: append without bumping visible count */
            out += s[i++];
        } else {
            out += s[i++];
            visible++;
        }
        if (visible >= max_visible && i < s.size()) {
            truncated = true;
            break;
        }
    }
    /* Only emit a SGR reset on the way out when we actually let one
     * through (so a half-included colour from the truncated tail can't
     * bleed into the next column).  When the cell contained no SGR at
     * all, skip the reset - it would be visible noise in some weak
     * terminals. */
    if (saw_sgr) out += "\x1b[0m";
    if (truncated) out += "…";
    return out;
}

} // namespace

/* -- ctor / DataSource wiring --------------------------------------- */

TvDataSource::TvDataSource(TvDb &db, AppState &state)
    : db_(db), state_(state) {}

DataSource TvDataSource::make_data_source() {
    DataSource s;
    s.row_begin    = [this](int p) { row_begin(p); };
    s.row_has_more = [this](int p) { return row_has_more(p); };
    s.row_next     = [this](int p) { return row_next(p); };
    return s;
}

void TvDataSource::invalidate() {
    built_lpane_ = false;
    built_rpane_ = false;
    built_hat_top_ = false;
    built_hat_bot_ = false;
    built_htop_ = false;
}

void TvDataSource::invalidate_rpane() {
    built_rpane_ = false;
}

void TvDataSource::set_htop_anchor_ns(int64_t ts_ns) {
    if (state_.htop_anchor_ns != ts_ns) {
        state_.htop_anchor_ns = ts_ns;
        built_htop_ = false;
    }
}

/* -- per-mode column layouts ---------------------------------------- */

namespace {
/* Column arrays kept as file-static so their addresses are stable
 * across calls (the engine stores raw pointers via add_panel /
 * set_panel_columns). Each layout drives both the panel chrome (the
 * title doubles as a column-header row) and the per-row column
 * emission in the matching lpane_*() builder. */

/* Mode 0 - output stream:
 *   col 0 time(9r) | col 1 k(3) | col 2 tgid(9r) | col 3 data(flex)
 * Right-aligned cells include a trailing space in their content so the
 * eye gets a visible gap before the next column (otherwise a 7-char
 * timestamp would butt up against the kind letter). */
const ColDef k_lpane_cols_outputs[] = {
    { 9, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    { 3, TUI_ALIGN_LEFT,  TUI_OVERFLOW_TRUNCATE},
    { 9, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
};

/* Mode 1 - process tree:
 *   col 0 name(flex) | col 1 pid(8r) | col 2 exit(8r) | col 3 dur(10r) */
const ColDef k_lpane_cols_procs[] = {
    {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
    { 8, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    { 8, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    {10, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
};

/* Mode 2 - file tree (flag column on LEFT, compact-number stat cols):
 *   col 0 flag(4) | col 1 name(flex) | col 2 opens(7r) | col 3 procs(7r)
 *   col 4 reads(7r) | col 5 writes(7r) */
const ColDef k_lpane_cols_files[] = {
    { 4, TUI_ALIGN_LEFT,  TUI_OVERFLOW_TRUNCATE},
    {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
    { 7, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    { 7, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    { 7, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    { 7, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
};

/* Mode 3 - event log:
 *   col 0 time(9r) | col 1 kind(8) | col 2 tgid(9r) | col 3 info(flex)
 * Same trailing-space convention as mode 0 (see above) so the columns
 * read as four distinct strips, not one run-on chunk. */
const ColDef k_lpane_cols_events[] = {
    { 9, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    { 8, TUI_ALIGN_LEFT,  TUI_OVERFLOW_TRUNCATE},
    { 9, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
};

/* Modes 4/5 - dep / rdep file closure (just name + a stat column). */
const ColDef k_lpane_cols_deps[] = {
    {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
    { 7, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
};

/* Modes 6/7 - dep cmds (process-shaped, just name + pid). */
const ColDef k_lpane_cols_dep_cmds[] = {
    {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
    { 8, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
};

/* rpane is a key/value detail panel and stays the same shape across
 * modes: a fixed key column and a flex value column. */
const ColDef k_rpane_cols[] = {
    {18, TUI_ALIGN_LEFT, TUI_OVERFLOW_TRUNCATE},
    {-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS},
};

} // namespace

TvDataSource::PanelLayout TvDataSource::lpane_layout() const {
    switch (state_.mode) {
        case 0: return {k_lpane_cols_outputs, 4,
            "  time      k    tgid   output"};
        case 2: return {k_lpane_cols_files, 6,
            "  flag  path                                 opens  procs  reads writes"};
        case 3: return {k_lpane_cols_events, 4,
            "  time      kind      tgid   event"};
        case 4: case 5: return {k_lpane_cols_deps, 2,
            "  path                                        opens"};
        case 6: case 7: return {k_lpane_cols_dep_cmds, 2,
            "  process                                       pid"};
        case 1:
        default: return {k_lpane_cols_procs, 4,
            "  process                                       pid    exit   duration"};
    }
}

TvDataSource::PanelLayout TvDataSource::rpane_layout() const {
    return {k_rpane_cols, 2, "detail"};
}

void TvDataSource::apply_layout(Tui &tui, int lpane, int rpane) const {
    auto l = lpane_layout();
    auto r = rpane_layout();
    tui.set_panel_columns(lpane, l.cols, l.ncols, l.title);
    tui.set_panel_columns(rpane, r.cols, r.ncols, r.title);
}

namespace {
/* Hat panels use a single full-width column; their content is one
 * line of summary text (a path prefix or an ancestor chain) per row.
 * No title bar — the hat is its own banner. */
const ColDef k_hat_cols[] = {
    {-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS},
};
} // namespace

TvDataSource::PanelLayout TvDataSource::hat_layout() const {
    return {k_hat_cols, 1, nullptr};
}

void TvDataSource::apply_hat_layout(Tui &tui, int hat_top, int hat_bot) const {
    auto h = hat_layout();
    tui.set_panel_columns(hat_top, h.cols, h.ncols, h.title);
    tui.set_panel_columns(hat_bot, h.cols, h.ncols, h.title);
}

namespace {
/* htop column: a single flex column.  Rows are rendered with their own
 * style (Green = just-spawned, Error = just-died, Search = the focus
 * tgid, Dim = passive ancestor) so the colouring tells the lifecycle
 * story.  The column gets a one-line title that is updated on each
 * rebuild to show the snapshot anchor's relative time. */
const ColDef k_htop_cols[] = {
    {-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS},
};
} // namespace

TvDataSource::PanelLayout TvDataSource::htop_layout() const {
    return {k_htop_cols, 1, "  procs at cursor (T to toggle)"};
}

void TvDataSource::apply_htop_layout(Tui &tui, int htop_pane) const {
    auto h = htop_layout();
    tui.set_panel_columns(htop_pane, h.cols, h.ncols, h.title);
}

void TvDataSource::ensure_hats_built() {
    /* Forward decl: dirty_for_lpane is defined just below. */
    extern bool dirty_for_lpane(const AppState &s,
                     int prev_mode, const std::string &prev_search,
                     const std::string &prev_flag_filter,
                     bool prev_grouped, bool prev_subtree_only,
                     const std::string &prev_subtree_root,
                     const std::string &prev_subject,
                     bool prev_show_pids,
                     int64_t prev_ts_after, int64_t prev_ts_before);
    /* Hats are produced as a side-effect of rebuild_lpane() (the
     * builders need the same per-mode data anyway). Force lpane to
     * rebuild if stale, which refreshes hat_top_ and hat_bot_ too. */
    bool stale = !built_lpane_ ||
        dirty_for_lpane(state_, built_for_mode_, built_for_search_,
                        built_for_flag_filter_,
                        built_for_grouped_, built_for_subtree_only_,
                        built_for_subtree_root_, built_for_subject_,
                        built_for_show_pids_,
                        built_for_ts_after_ns_, built_for_ts_before_ns_);
    if (stale) {
        rebuild_lpane();
        built_for_mode_ = state_.mode;
        built_for_search_ = state_.search;
        built_for_flag_filter_ = state_.flag_filter;
        built_for_grouped_ = state_.grouped;
        built_for_subtree_only_ = state_.subtree_only;
        built_for_subtree_root_ = state_.subtree_root;
        built_for_subject_ = state_.subject_file;
        built_for_show_pids_ = state_.show_pids;
        built_for_ts_after_ns_ = state_.ts_after_ns;
        built_for_ts_before_ns_ = state_.ts_before_ns;
        built_lpane_ = true;
        built_hat_top_ = true;
        built_hat_bot_ = true;
    }
}

int TvDataSource::hat_top_row_count() {
    ensure_hats_built();
    return static_cast<int>(hat_top_.size());
}

int TvDataSource::hat_bot_row_count() {
    ensure_hats_built();
    return static_cast<int>(hat_bot_.size());
}

bool dirty_for_lpane(const AppState &s,
                     int prev_mode, const std::string &prev_search,
                     const std::string &prev_flag_filter,
                     bool prev_grouped, bool prev_subtree_only,
                     const std::string &prev_subtree_root,
                     const std::string &prev_subject,
                     bool prev_show_pids,
                     int64_t prev_ts_after, int64_t prev_ts_before) {
    return s.mode != prev_mode || s.search != prev_search ||
           s.flag_filter != prev_flag_filter ||
           s.grouped != prev_grouped || s.subtree_only != prev_subtree_only ||
           s.subtree_root != prev_subtree_root ||
           s.subject_file != prev_subject ||
           s.show_pids != prev_show_pids ||
           s.ts_after_ns  != prev_ts_after ||
           s.ts_before_ns != prev_ts_before;
}

void TvDataSource::row_begin(int panel) {
    if (panel == 0) {
        ensure_hats_built();
        lpane_idx_ = 0;
    } else if (panel == 2) {
        ensure_hats_built();
        hat_top_idx_ = 0;
    } else if (panel == 3) {
        ensure_hats_built();
        hat_bot_idx_ = 0;
    } else if (panel == 4) {
        /* htop snapshot column.  Cache key is (anchor_ns, focus_tgid).
         * Rebuild lazily; the main loop calls set_htop_anchor_ns()
         * whenever the cursor commits to a new event. */
        if (!built_htop_ ||
            built_htop_for_ns_     != state_.htop_anchor_ns ||
            built_htop_focus_tgid_ != state_.htop_focus_tgid) {
            rebuild_htop();
            built_htop_for_ns_     = state_.htop_anchor_ns;
            built_htop_focus_tgid_ = state_.htop_focus_tgid;
            built_htop_ = true;
        }
        htop_idx_ = 0;
    } else {
        if (!built_rpane_ || built_for_cursor_ != state_.cursor_id ||
            built_for_mode_ != state_.mode ||
            built_for_collapsed_ != state_.collapsed_sections) {
            rebuild_rpane();
            built_for_cursor_ = state_.cursor_id;
            built_for_collapsed_ = state_.collapsed_sections;
            built_rpane_ = true;
        }
        rpane_idx_ = 0;
    }
}

bool TvDataSource::row_has_more(int panel) {
    if (panel == 0) return lpane_idx_ < lpane_.size();
    if (panel == 2) return hat_top_idx_ < hat_top_.size();
    if (panel == 3) return hat_bot_idx_ < hat_bot_.size();
    if (panel == 4) return htop_idx_ < htop_.size();
    return rpane_idx_ < rpane_.size();
}

RowData TvDataSource::row_next(int panel) {
    if (panel == 0) return lpane_.at(lpane_idx_++);
    if (panel == 2) return hat_top_.at(hat_top_idx_++);
    if (panel == 3) return hat_bot_.at(hat_bot_idx_++);
    if (panel == 4) return htop_.at(htop_idx_++);
    return rpane_.at(rpane_idx_++);
}

/* -- lpane dispatch ------------------------------------------------- */

void TvDataSource::rebuild_lpane() {
    lpane_.clear();
    /* Hat caches are produced as a side-effect of building lpane —
     * the proc-tree and file-tree builders fill hat_top_ / hat_bot_
     * directly when a common ancestor / path prefix is detected. */
    hat_top_.clear();
    hat_bot_.clear();
    proc_chain_.clear();
    proc_label_.clear();
    switch (state_.mode) {
        case 0: lpane_outputs(); break;
        case 1: lpane_processes(); break;
        case 2: lpane_files(); break;
        case 3: lpane_events(); break;
        case 4: lpane_deps(0); break;
        case 5: lpane_deps(1); break;
        case 6: lpane_dep_cmds(0); break;
        case 7: lpane_dep_cmds(1); break;
        default:
            lpane_.push_back(mk_row("__bad",
                "(unknown mode " + std::to_string(state_.mode) + ")",
                RowStyle::Dim));
    }
    if (lpane_.empty()) {
        lpane_.push_back(mk_row("__empty", "(no rows)", RowStyle::Dim));
    }
}

/* Recompute hat_top_ / hat_bot_ from a window of cached lpane rows.
 * Called both during the global rebuild (with first=0, n=lpane.size())
 * and from the main loop after the user scrolls (with the topmost
 * visible row + viewport height).
 *
 * Modes 1 and 2 are the only ones with hats, and they have very
 * different shapes:
 *   - Mode 1: walk each visible non-synthetic row's cached ancestor
 *     chain (proc_chain_), then compute the longest common suffix
 *     across non-empty chains.  Drop init (PID ≤ 1) so we don't
 *     stutter "init › …".  Render as basename[›basename]…
 *   - Mode 2: collect leaf-file row ids (which are full paths), find
 *     the longest common path-segment prefix across them, back off to
 *     the last '/' so the hat names a directory.  When there is just
 *     one visible leaf, back off one segment (so its parent dir shows).
 * Returns true if hat_top_ or hat_bot_ contents actually changed. */
bool TvDataSource::recompute_hats_for_window(int first_row, int n_rows) {
    if (state_.mode != 1 && state_.mode != 2) return false;
    if (!built_lpane_ || lpane_.empty()) return false;
    if (state_.subtree_only) return false;
    if (first_row < 0) first_row = 0;
    if (n_rows <= 0)   n_rows = static_cast<int>(lpane_.size());
    int last = first_row + n_rows;
    if (last > static_cast<int>(lpane_.size()))
        last = static_cast<int>(lpane_.size());
    if (first_row >= last) return false;

    /* -- Mode 1: process tree -- */
    if (state_.mode == 1) {
        std::vector<std::vector<int>> chains;
        chains.reserve(static_cast<size_t>(last - first_row));
        for (int i = first_row; i < last; i++) {
            const auto &row = lpane_[i];
            if (row.id.empty() || row.id[0] == '_') continue;
            int t = std::atoi(row.id.c_str());
            auto it = proc_chain_.find(t);
            if (it == proc_chain_.end()) continue;
            chains.push_back(it->second);
        }
        chains.erase(std::remove_if(chains.begin(), chains.end(),
                         [](const std::vector<int> &c){ return c.empty(); }),
                     chains.end());
        std::vector<RowData> new_top;
        if (!chains.empty()) {
            std::vector<int> common;
            for (size_t k = 0;; k++) {
                int candidate = -1;
                bool ok = true;
                for (auto &c : chains) {
                    if (k >= c.size()) { ok = false; break; }
                    int v = c[c.size() - 1 - k];
                    if (candidate < 0) candidate = v;
                    else if (v != candidate) { ok = false; break; }
                }
                if (!ok) break;
                common.push_back(candidate);
            }
            while (!common.empty() && common.front() <= 1)
                common.erase(common.begin());
            if (!common.empty()) {
                std::string label;
                for (size_t i = 0; i < common.size(); i++) {
                    auto lit = proc_label_.find(common[i]);
                    std::string nm = (lit != proc_label_.end()) ? lit->second
                                                                : std::string();
                    if (nm.empty()) continue;
                    if (!label.empty()) label += " › ";
                    label += nm;
                    if (state_.show_pids) {
                        label += "[";
                        label += std::to_string(common[i]);
                        label += "]";
                    }
                }
                if (!label.empty()) {
                    RowData hat;
                    hat.id    = "hat_proc";
                    hat.style = RowStyle::Heading;
                    hat.cols  = {label};
                    new_top.push_back(std::move(hat));
                }
            }
        }
        bool changed = (new_top.size() != hat_top_.size()) ||
                       (!new_top.empty() && !hat_top_.empty() &&
                        new_top[0].cols[0] != hat_top_[0].cols[0]);
        /* Don't drop the hat just because the visible window happened
         * to contain no real proc rows (e.g. only the synthetic "(no
         * processes)" placeholder, or the user scrolled past the end).
         * Keep whatever was last shown until a window with proc rows
         * comes back into view. */
        if (new_top.empty() && !hat_top_.empty()) return false;
        hat_top_ = std::move(new_top);
        return changed;
    }

    /* -- Mode 2: file tree -- */
    auto is_path_id = [](const std::string &s) {
        return !s.empty() && s[0] == '/' &&
               !(s.size() >= 2 && s[0] == '_' && s[1] == '_');
    };
    /* Leaf ids = paths not ending in '/' (directory aggregate ids end
     * in '/'). The original emit_path_hat() also keys off this. */
    std::vector<const std::string *> leaves;
    for (int i = first_row; i < last; i++) {
        const auto &id = lpane_[i].id;
        if (!is_path_id(id)) continue;
        if (id.back() == '/') continue;
        leaves.push_back(&id);
    }
    std::vector<RowData> new_bot;
    if (!leaves.empty()) {
        std::string prefix = *leaves.front();
        if (leaves.size() >= 2) {
            for (auto *p : leaves) {
                size_t i = 0;
                while (i < prefix.size() && i < p->size() && prefix[i] == (*p)[i])
                    i++;
                prefix.resize(i);
                if (prefix.empty()) break;
            }
        }
        size_t slash = prefix.rfind('/');
        if (slash != std::string::npos && slash > 0) {
            prefix.resize(slash + 1);
            if (prefix.size() > 1) {
                RowData hat;
                hat.id    = "hat_prefix";
                hat.style = RowStyle::Heading;
                hat.cols  = {prefix};
                new_bot.push_back(std::move(hat));
            }
        }
    }
    bool changed = (new_bot.size() != hat_bot_.size()) ||
                   (!new_bot.empty() && !hat_bot_.empty() &&
                    new_bot[0].cols[0] != hat_bot_[0].cols[0]);
    /* Same conservatism as the proc-tree branch: a window with no leaf
     * paths leaves the previous hat in place rather than blanking it. */
    if (new_bot.empty() && !hat_bot_.empty()) return false;
    hat_bot_ = std::move(new_bot);
    return changed;
}

/* -- Mode 1: process tree ------------------------------------------- */

namespace {
struct ProcRow {
    int         tgid = 0;
    int         ppid = 0;
    std::string exe;
    std::string name;
    int64_t     start_ns = 0;
    int64_t     end_ns   = 0;
    std::string exit_kind, exit_code, core_dumped;
    bool        ended = false;
    /* for tree */
    std::vector<int> children;
    bool             matches_search = true;
};

std::string proc_name_label(const ProcRow &p, bool show_basename_only,
                            bool show_pid) {
    std::string label;
    if (show_pid) label = sfmt("[%d] ", p.tgid);
    label += show_basename_only ? p.name : p.exe;
    if (label.size() > 0 && p.exe.empty() && p.name.empty())
        label += "(no exec)";
    return label;
}

std::string proc_stats_label(const ProcRow &p) {
    std::string s = format_duration_ns(p.start_ns, p.end_ns);
    std::string ex = format_exit(p.exit_kind, p.exit_code, p.core_dumped, p.ended);
    if (!ex.empty()) {
        if (!s.empty()) s += "  ";
        s += ex;
    }
    return s;
}

/* Back-compat: combined label (used by mode 7 dep cmds, which only has
 * one column). */
std::string proc_label(const ProcRow &p, bool show_basename_only,
                       bool show_pid) {
    std::string label = proc_name_label(p, show_basename_only, show_pid);
    std::string st = proc_stats_label(p);
    if (!st.empty()) label += "  " + st;
    return label;
}
} // namespace

void TvDataSource::lpane_processes() {
    std::string err;
    if (!db_.ensure_proc_index(&err)) {
        lpane_.push_back(mk_row("__err", "(index: " + err + ")", RowStyle::Error));
        return;
    }

    std::string sql =
        "SELECT tgid, ppid, "
        "       CAST(exe AS VARCHAR), "
        "       start_ns, end_ns, "
        "       exit_kind, exit_code, core_dumped "
        "FROM tv_idx_proc";
    /* Computed-flag filter for processes. Letters are mode-specific:
     *   K  killed by signal OR still running when trace ended
     *   F  exited != 0 with at least one file open for writing
     *      (filters out grep/test/etc. with normal non-zero rc)
     *   D  process exec'd a file that was written by another tgid
     *      in the trace (i.e. ran a derived script/elf)
     * The grammar (`+W`, `-E`, comma alternation) is parsed once into a
     * FlagSpec; build_flag_sql() composes per-letter SQL fragments
     * with AND/OR/NOT. */
    std::string where;
    FlagSpec flagspec = parse_flag_spec(state_.flag_filter);
    bool need_canon_for_proc =
        flag_spec_uses(flagspec, 'F') || flag_spec_uses(flagspec, 'D');
    auto proc_letter_sql = [](char l) -> std::string {
        switch (l) {
            case 'K': return "exit_kind = 1 OR exit_kind IS NULL";
            case 'F': return
                "((exit_kind = 0 AND exit_code <> 0) OR exit_kind = 1 "
                " OR exit_kind IS NULL) "
                "AND tgid IN (SELECT DISTINCT tgid FROM tv_idx_open_canon "
                "             WHERE (flags & 3) <> 0 AND err = 0)";
            case 'D': return
                "tgid IN ("
                "  SELECT e.tgid FROM exec e "
                "  WHERE EXISTS ("
                "    SELECT 1 FROM tv_idx_open_canon w "
                "    WHERE w.tgid <> e.tgid AND (w.flags & 3) <> 0 "
                "      AND w.err = 0 AND w.path = CAST(e.exe AS VARCHAR) "
                "      AND w.ts_ns < e.ts_ns))";
        }
        return "";        /* unknown letters are silently ignored */
    };
    std::string fexpr = build_flag_sql(flagspec, proc_letter_sql);
    if (!fexpr.empty()) where = fexpr;
    /* Time cutoffs (modes 0/1/3 honour these): start_ns is the proc's
     * entry timestamp - "after T" keeps procs that started at or after T. */
    if (state_.ts_after_ns > 0)
        where = (where.empty() ? "" : "(" + where + ") AND ") +
                sfmt("start_ns >= %lld", (long long)state_.ts_after_ns);
    if (state_.ts_before_ns > 0)
        where = (where.empty() ? "" : "(" + where + ") AND ") +
                sfmt("start_ns <= %lld", (long long)state_.ts_before_ns);
    if (need_canon_for_proc) {
        if (!db_.ensure_path_index(&err)) {
            lpane_.push_back(mk_row("__err", err, RowStyle::Error));
            return;
        }
    }
    if (!where.empty()) sql += " WHERE " + where;
    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    if (rows.empty()) {
        lpane_.push_back(mk_row("__empty", "(no processes)", RowStyle::Dim));
        return;
    }

    /* Search-match set: a tgid matches the search if its exe, name,
     * any argv element, or any env key/val contains the query string.
     * The exe/name match is checked inline below; the argv/env match is
     * resolved via two extra SQL lookups so a search like `/PATH=` or
     * `/--verbose` actually surfaces the right processes (the user
     * reported that hits in env/argv were silently missing). */
    std::unordered_set<int> argv_env_match_tgids;
    if (!state_.search.empty()) {
        std::string esc = sql_escape(state_.search);
        std::string mq =
            "SELECT DISTINCT tgid FROM argv "
            "  WHERE CAST(arg AS VARCHAR) LIKE '%' || " + esc + " || '%' "
            "UNION "
            "SELECT DISTINCT tgid FROM env "
            "  WHERE CAST(key AS VARCHAR) LIKE '%' || " + esc + " || '%' "
            "     OR CAST(val AS VARCHAR) LIKE '%' || " + esc + " || '%'";
        std::string e2;
        auto mr = db_.query_strings(mq, &e2);
        if (e2.empty()) {
            for (auto &row : mr)
                if (!row.empty() && !row[0].empty())
                    argv_env_match_tgids.insert(std::atoi(row[0].c_str()));
        }
    }

    std::unordered_map<int, ProcRow> procs;
    procs.reserve(rows.size());
    for (auto &r : rows) {
        ProcRow p;
        p.tgid     = std::atoi(r[0].c_str());
        p.ppid     = std::atoi(r[1].c_str());
        p.exe      = r[2];
        p.name     = basename_of(p.exe);
        p.start_ns = r[3].empty() ? 0 : std::stoll(r[3]);
        p.end_ns   = r[4].empty() ? p.start_ns : std::stoll(r[4]);
        p.exit_kind = r[5];
        p.exit_code = r[6];
        p.core_dumped = r[7];
        p.ended = !r[5].empty();
        if (!state_.search.empty()) {
            p.matches_search = (p.exe.find(state_.search) != std::string::npos) ||
                               (p.name.find(state_.search) != std::string::npos) ||
                               argv_env_match_tgids.count(p.tgid) > 0;
        }
        procs.emplace(p.tgid, std::move(p));
    }

    /* Build adjacency: child list for each ppid. */
    std::vector<int> roots;
    if (state_.grouped) {
        for (auto &kv : procs) {
            ProcRow &p = kv.second;
            auto pit = procs.find(p.ppid);
            if (pit == procs.end()) {
                roots.push_back(p.tgid);
            } else {
                pit->second.children.push_back(p.tgid);
            }
        }
        /* If subtree_only and a root tgid is set, restrict to that subtree. */
        if (state_.subtree_only && !state_.subtree_root.empty()) {
            int rt = std::atoi(state_.subtree_root.c_str());
            if (procs.count(rt)) roots = { rt };
        }
        std::sort(roots.begin(), roots.end(),
                  [&](int a, int b) { return procs[a].start_ns < procs[b].start_ns; });
        for (auto &kv : procs) {
            auto &c = kv.second.children;
            std::sort(c.begin(), c.end(),
                [&](int a, int b) { return procs[a].start_ns < procs[b].start_ns; });
        }
    } else {
        for (auto &kv : procs) roots.push_back(kv.first);
        std::sort(roots.begin(), roots.end(),
                  [&](int a, int b) { return procs[a].start_ns < procs[b].start_ns; });
    }

    /* Search-aware pruning: in tree mode, drop subtrees whose own row
     * and all descendants don't match. */
    std::function<bool(int)> any_match = [&](int t) -> bool {
        ProcRow &p = procs[t];
        if (state_.search.empty() || p.matches_search) return true;
        for (int c : p.children) if (any_match(c)) return true;
        return false;
    };

    /* DFS emit. */
    std::function<void(int, const std::string &, bool)> emit =
        [&](int t, const std::string &prefix, bool is_last) {
            ProcRow &p = procs[t];
            if (state_.grouped && !state_.search.empty() && !any_match(t)) return;
            std::string indent;
            if (state_.grouped) {
                indent = prefix;
                if (!prefix.empty() || prefix == "")
                    indent += is_last ? "└─ " : "├─ ";
                /* Note: roots are emitted with empty prefix and no glyph. */
            }
            std::string text = state_.grouped ? indent : "";
            text += proc_name_label(p, /*basename_only=*/true,
                                    state_.show_pids);
            std::string dur = format_duration_ns(p.start_ns, p.end_ns);
            std::string ex  = format_exit(p.exit_kind, p.exit_code,
                                          p.core_dumped, p.ended);
            RowStyle style = RowStyle::Normal;
            if (!p.ended)             style = RowStyle::Green;
            else if (p.exit_kind == "1") style = RowStyle::Error;
            else if (p.exit_code != "0" && !p.exit_code.empty())
                style = RowStyle::Error;
            if (!state_.search.empty() && p.matches_search)
                style = RowStyle::Search;
            RowData row;
            row.id = std::to_string(p.tgid);
            row.parent_id = std::to_string(p.ppid);
            row.has_children = !p.children.empty();
            /* 4-column row matching k_lpane_cols_procs:
             *   name | pid | exit | duration */
            row.cols.push_back(std::move(text));
            row.cols.push_back(std::to_string(p.tgid));
            row.cols.push_back(std::move(ex));
            row.cols.push_back(std::move(dur));
            row.style = style;
            lpane_.push_back(std::move(row));
            if (!state_.grouped) return;
            std::string child_prefix = prefix + (is_last ? "   " : "│  ");
            for (size_t i = 0; i < p.children.size(); i++)
                emit(p.children[i], child_prefix, i + 1 == p.children.size());
        };

    if (state_.grouped) {
        for (size_t i = 0; i < roots.size(); i++)
            emit(roots[i], "", i + 1 == roots.size());
    } else {
        for (int t : roots) emit(t, "", true);
    }

    /* Proc-tree hat. Surfaces the common ancestor chain as a one-row
     * Heading in the dedicated hat panel (above the list). When there
     * is no common ancestor, hat_top_ stays empty and the hat panel
     * shrinks to zero rows.
     *
     * Algorithm:
     *  - For each visible (non-synthetic) row, walk parent_id back
     *    through the procs map to build a chain of in-trace ancestors,
     *    ordered child→parent (chain[0] is the row's immediate parent).
     *  - Common ancestor sequence = longest suffix shared across every
     *    non-empty chain. Rows whose chain is empty (their parent
     *    isn't in the trace) don't constrain the common chain — they
     *    just don't contribute. So when the topmost row is itself
     *    nested, its ancestors still show up as a hat even if some
     *    sibling roots have no in-trace parent.
     *  - Drop a leading PID ≤ 1 (init) so we don't stutter "init › …"
     *    on every trace. */
    if (!state_.subtree_only && !lpane_.empty()) {
        /* Cache per-tgid ancestor chain and display label so the hat
         * can be recomputed cheaply for any lpane window when the user
         * scrolls (sticky-breadcrumb behaviour). */
        for (auto &kv : procs) {
            const ProcRow &pp = kv.second;
            std::vector<int> chain;
            int cur = pp.ppid;
            for (int depth = 0; depth < 256; depth++) {
                if (cur <= 0) break;
                auto it = procs.find(cur);
                if (it == procs.end()) break;
                chain.push_back(cur);
                if (it->second.ppid == cur) break;   /* self-loop guard */
                cur = it->second.ppid;
            }
            proc_chain_[pp.tgid] = std::move(chain);
            proc_label_[pp.tgid] = basename_of(pp.exe);
        }
        recompute_hats_for_window(0, static_cast<int>(lpane_.size()));
    }
}

/* -- Mode 2: file tree ---------------------------------------------- */

namespace {
struct FileRow {
    std::string path;
    int opens=0, errors=0, procs=0, reads=0, writes=0;
};

RowData mk_file_row(const FileRow &r, const std::string &display_name,
                    bool search_hit) {
    /* 6-column row matching k_lpane_cols_files:
     *   flag | name | opens | procs | reads | writes
     * Flag column on the LEFT so the eye can scan a vertical strip
     * for "W..." or "RW." rows. Numbers are compact (26.8k, 1.56M)
     * and right-aligned to keep the column width fixed at 7. */
    std::string flags;
    flags += r.reads  ? 'R' : '-';
    flags += r.writes ? 'W' : '-';
    flags += r.errors ? 'E' : '-';
    RowStyle st = r.errors ? RowStyle::Error :
                  r.writes ? RowStyle::Yellow : RowStyle::Normal;
    if (search_hit) st = RowStyle::Search;
    RowData row;
    row.id = r.path;
    row.cols = {
        flags,
        display_name,
        compact_count((uint64_t)r.opens),
        compact_count((uint64_t)r.procs),
        compact_count((uint64_t)r.reads),
        compact_count((uint64_t)r.writes),
    };
    row.style = st;
    return row;
}
} // namespace

void TvDataSource::lpane_files() {
    std::string err;
    if (!db_.ensure_path_index(&err)) {
        lpane_.push_back(mk_row("__err", "(index: " + err + ")", RowStyle::Error));
        return;
    }
    std::string sql =
        "SELECT path, opens, errors, procs, reads, writes "
        "FROM tv_idx_path";
    std::string where;
    if (!state_.search.empty())
        where += "path LIKE '%' || " + sql_escape(state_.search) + " || '%'";
    /* File flag vocabulary (parsed by parse_flag_spec):
     *   R/W/E: raw read/write/error (matches the visible badge)
     *   w    : "written and not unlinked afterwards" (currently
     *          approximated as writes>0; we don't track unlink yet)
     *   f    : path was written by a process that exited != 0 / killed /
     *          still running when trace ended ("written by a failure")
     *   s    : path is outside common system/toolkit roots
     *          (/usr, /bin, /sbin, /lib*, /opt, /srv)
     *   k    : path is NOT a kernel/IPC pseudo-path (pipe:, socket:,
     *          anon_inode:, /dev/, /sys/, /proc/) */
    FlagSpec flagspec = parse_flag_spec(state_.flag_filter);
    bool need_failed_writers = flag_spec_uses(flagspec, 'f');
    auto file_letter_sql = [](char l) -> std::string {
        switch (l) {
            case 'R': return "reads  > 0";
            case 'W': return "writes > 0";
            case 'E': return "errors > 0";
            case 'w': return "writes > 0";
            case 'f': return "path IN (SELECT path FROM tv_idx_path_failed_writers)";
            case 's': return
                "NOT (path LIKE '/usr/%' OR path LIKE '/bin/%' OR "
                     "path LIKE '/sbin/%' OR path LIKE '/lib%' OR "
                     "path LIKE '/opt/%' OR path LIKE '/srv/%')";
            case 'k': return
                "NOT (path LIKE 'pipe:%' OR path LIKE 'socket:%' OR "
                     "path LIKE 'anon_inode:%' OR path LIKE '/dev/%' OR "
                     "path LIKE '/sys/%' OR path LIKE '/proc/%')";
        }
        return "";
    };
    std::string fexpr = build_flag_sql(flagspec, file_letter_sql);
    if (!fexpr.empty()) {
        if (where.empty()) where = fexpr;
        else where = "(" + where + ") AND (" + fexpr + ")";
    }
    /* Time cutoffs: a file's first_ns is when it first appeared. */
    if (state_.ts_after_ns > 0)
        where = (where.empty() ? "" : "(" + where + ") AND ") +
                sfmt("first_ns >= %lld", (long long)state_.ts_after_ns);
    if (state_.ts_before_ns > 0)
        where = (where.empty() ? "" : "(" + where + ") AND ") +
                sfmt("first_ns <= %lld", (long long)state_.ts_before_ns);
    if (need_failed_writers) {
        if (!db_.ensure_proc_index(&err)) {
            lpane_.push_back(mk_row("__err", err, RowStyle::Error));
            return;
        }
        /* Built once per connection lifetime; cheap. */
        (void)db_.query_strings(
            "CREATE TEMP TABLE IF NOT EXISTS tv_idx_path_failed_writers AS "
            "SELECT DISTINCT o.path "
            "FROM tv_idx_open_canon o JOIN tv_idx_proc p USING (tgid) "
            "WHERE (o.flags & 3) <> 0 "
            "  AND (p.exit_kind = 1 OR p.exit_kind IS NULL OR "
            "       (p.exit_kind = 0 AND p.exit_code <> 0))",
            &err);
    }
    if (!where.empty()) sql += " WHERE " + where;
    sql += " ORDER BY path LIMIT 5000";
    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }

    std::vector<FileRow> frows;
    frows.reserve(rows.size());
    for (auto &r : rows) {
        FileRow f;
        f.path   = r[0];
        f.opens  = std::atoi(r[1].c_str());
        f.errors = std::atoi(r[2].c_str());
        f.procs  = std::atoi(r[3].c_str());
        f.reads  = std::atoi(r[4].c_str());
        f.writes = std::atoi(r[5].c_str());
        frows.push_back(std::move(f));
    }

    if (!state_.grouped) {
        for (auto &f : frows) {
            bool hit = !state_.search.empty() &&
                       f.path.find(state_.search) != std::string::npos;
            lpane_.push_back(mk_file_row(f, f.path, hit));
        }
        /* Hat treatment for the flat list. The reviewer flagged that
         * all-items-share-a-parent flat lists weren't getting a hat at
         * all (the call below used to live only inside the tree
         * branch), and that a single deeply-nested top row also got
         * nothing. emit_path_hat() now handles both cases - we just
         * need at least one leaf row. */
        if (!state_.subtree_only && !lpane_.empty()) {
            emit_path_hat();
        }
        return;
    }

    /* Tree mode: build a directory tree from the path list and
     * aggregate stats up the tree so directory rows still show
     * meaningful badges/counts. */
    struct Node {
        std::string component;
        std::string full_path;
        bool        is_file = false;
        int         file_idx = -1;
        FileRow     agg;                /* aggregated over subtree */
        int         file_count = 0;
        std::map<std::string, std::unique_ptr<Node>> children;
    };
    Node root;
    for (size_t i = 0; i < frows.size(); i++) {
        const std::string &p = frows[i].path;
        if (p.empty()) continue;
        Node *cur = &root;
        size_t pos = 0;
        std::string acc = (p[0] == '/') ? "/" : "";
        if (p[0] == '/') pos = 1;
        while (pos <= p.size()) {
            size_t nx = p.find('/', pos);
            std::string comp = p.substr(pos, nx == std::string::npos
                                            ? p.size() - pos : nx - pos);
            if (comp.empty() && nx == std::string::npos) break;
            acc += (acc.empty() || acc.back() == '/') ? comp : "/" + comp;
            auto it = cur->children.find(comp);
            if (it == cur->children.end()) {
                auto n = std::make_unique<Node>();
                n->component = comp;
                n->full_path = acc;
                Node *raw = n.get();
                cur->children.emplace(comp, std::move(n));
                cur = raw;
            } else {
                cur = it->second.get();
            }
            if (nx == std::string::npos) break;
            pos = nx + 1;
        }
        cur->is_file = true;
        cur->file_idx = (int)i;
    }

    /* Recursive aggregation. */
    std::function<void(Node *)> aggregate = [&](Node *n) {
        if (n->is_file) {
            n->agg = frows[n->file_idx];
            n->file_count = 1;
            return;
        }
        for (auto &kv : n->children) {
            Node *c = kv.second.get();
            aggregate(c);
            n->agg.opens  += c->agg.opens;
            n->agg.errors += c->agg.errors;
            n->agg.procs  += c->agg.procs;   /* over-counts; ok for hint */
            n->agg.reads  += c->agg.reads;
            n->agg.writes += c->agg.writes;
            n->file_count += c->file_count;
        }
    };
    aggregate(&root);

    std::function<void(Node *, int)> emit = [&](Node *n, int depth) {
        for (auto &kv : n->children) {
            Node *c = kv.second.get();
            std::string indent(depth * 2, ' ');
            if (c->is_file) {
                bool hit = !state_.search.empty() &&
                           frows[c->file_idx].path.find(state_.search)
                                != std::string::npos;
                RowData rw = mk_file_row(frows[c->file_idx],
                                         indent + c->component, hit);
                lpane_.push_back(std::move(rw));
            } else {
                /* Directory row: same 6-column layout as leaf rows so
                 * dirs and files line up. The badge comes from the
                 * aggregate flags; the four stat columns get the
                 * subtree totals (file_count fills the "procs" slot
                 * for directories - it's the most useful number to
                 * see while scanning the tree). */
                FileRow agg = c->agg;
                agg.path = c->full_path + "/";
                std::string flags;
                flags += agg.reads  ? 'R' : '-';
                flags += agg.writes ? 'W' : '-';
                flags += agg.errors ? 'E' : '-';
                RowData rw;
                rw.id = c->full_path + "/";
                rw.cols = {
                    flags,
                    indent + c->component + "/",
                    compact_count((uint64_t)agg.opens),
                    compact_count((uint64_t)c->file_count),
                    compact_count((uint64_t)agg.reads),
                    compact_count((uint64_t)agg.writes),
                };
                /* Directories get cyan; failed-error directories take
                 * priority colouring to draw the eye. */
                rw.style = agg.errors ? RowStyle::Error : RowStyle::CyanBold;
                rw.has_children = true;
                lpane_.push_back(std::move(rw));
            }
            emit(c, depth + 1);
        }
    };
    emit(&root, 0);

    /* "Hat" row: longest common path-segment prefix shared by all
     * emitted rows. The user complaint was that a deeply-nested
     * subtree shows giant left-margin indent ("│  │  │  ..."). When
     * everything visible lives under e.g. /usr/include/dir_0/sub_0/,
     * we surface that prefix once at the top, flush-left and styled
     * as a heading, and strip it from the body so the leftmost column
     * carries actual differentiating data.
     *
     * Skipped when:
     *   - fewer than 2 rows (nothing to factor)
     *   - the prefix is just "/" or "" (already flush-left visually)
     *   - the user pinned a subtree (state_.subtree_only) - then the
     *     existing subtree banner is the hat, no duplicate. */
    if (!state_.subtree_only && !lpane_.empty()) {
        emit_path_hat();
    }
}

/* Find the longest common path-segment prefix across lpane_ rows whose
 * id is a path. Mutates the rows to strip the prefix and inserts a hat
 * row at the front. Path-segment-aware: "/usr/include/dir_0/" and
 * "/usr/include/dir_0/sub_0/file.h" share "/usr/include/dir_0/", but
 * "/usr/inc/" and "/usr/include/" only share "/usr/".
 *
 * Strategy: look only at *leaf file* rows (id not ending in '/') for
 * the prefix calculation. Directory-aggregate rows that are strict
 * prefixes of the hat get folded into the hat (hidden). This is what
 * factors out the giant left-margin indent the user complained about
 * in deep trees - the dir rows merge into the header.
 *
 * Special case: if there is only ONE visible leaf, the "common prefix"
 * is just that leaf, which we then back off by one segment to show its
 * parent directory. This is the "topmost item is nested" case the
 * reviewer asked for - a single deeply-nested row still gets a hat
 * naming the directory it lives in. */
void TvDataSource::emit_path_hat() {
    auto is_path_row = [](const RowData &r) {
        return !r.id.empty() && r.id[0] == '/' &&
               !(r.id.size() >= 2 && r.id[0] == '_' && r.id[1] == '_');
    };
    auto is_leaf = [&](const RowData &r) {
        return is_path_row(r) && r.id.back() != '/';
    };
    int n_leaves = 0;
    const std::string *first_leaf = nullptr;
    for (const auto &r : lpane_) {
        if (!is_leaf(r)) continue;
        if (!first_leaf) first_leaf = &r.id;
        n_leaves++;
    }
    if (n_leaves < 1 || !first_leaf) return;
    std::string prefix = *first_leaf;
    if (n_leaves >= 2) {
        for (const auto &r : lpane_) {
            if (!is_leaf(r)) continue;
            size_t i = 0;
            while (i < prefix.size() && i < r.id.size() && prefix[i] == r.id[i])
                i++;
            prefix.resize(i);
            if (prefix.empty()) return;
        }
    }
    /* Single-leaf or multi-leaf: cut back to the last '/' so the hat is
     * always a directory. With one leaf this strips the filename; with
     * many leaves it strips back to the first divergence. */
    size_t slash = prefix.rfind('/');
    if (slash == std::string::npos || slash == 0) return;
    prefix.resize(slash + 1);
    if (prefix.size() <= 1) return;

    /* Drop dir-aggregate rows whose id is a prefix-of-or-equal-to the
     * hat. They're now represented by the hat itself - keeping them
     * would just be a stutter ("/usr/" then "/usr/include/" then hat
     * "/usr/include/dir_0/"). */
    auto is_subsumed_dir = [&](const RowData &r) {
        if (!is_path_row(r)) return false;
        if (r.id.empty() || r.id.back() != '/') return false;
        return prefix.size() >= r.id.size() &&
               prefix.starts_with(r.id);
    };
    lpane_.erase(std::remove_if(lpane_.begin(), lpane_.end(),
                                is_subsumed_dir),
                 lpane_.end());

    /* Strip the prefix from each surviving row's display column.
     * Re-derive the indent from the path tail's '/' count so cols[1]
     * stays consistent with the visible hierarchy. */
    for (auto &r : lpane_) {
        if (!is_path_row(r)) continue;
        if (r.cols.size() < 2) continue;
        if (r.id.size() <= prefix.size()) continue;
        std::string tail = r.id.substr(prefix.size());
        bool is_dir = !tail.empty() && tail.back() == '/';
        std::string body = is_dir ? tail.substr(0, tail.size() - 1) : tail;
        int depth = 0;
        for (size_t i = 0; i + 1 < body.size(); i++)
            if (body[i] == '/') depth++;
        size_t last_slash = body.rfind('/');
        std::string leaf = (last_slash == std::string::npos)
            ? body
            : body.substr(last_slash + 1);
        std::string indent(depth * 2, ' ');
        r.cols[1] = indent + leaf + (is_dir ? "/" : "");
    }

    /* Hat row, fed to the dedicated hat panel above the list (not
     * prepended to lpane_). Empty hat_bot_ → the panel collapses to
     * zero rows. */
    RowData hat;
    hat.id = "hat_prefix";
    hat.style = RowStyle::Heading;
    hat.cols = {prefix};
    hat_bot_.push_back(std::move(hat));
}

/* -- Mode 0: output (stdout/stderr) view ---------------------------- */

void TvDataSource::lpane_outputs() {
    /* Merged stdout+stderr stream.  Two presentations are supported,
     * toggled with `t` (state_.grouped):
     *   - flat:  chronological interleave (default; the original mode 0).
     *   - tree:  events bucketed by tgid, with a process header row in
     *            front of each group.  Within a group rows stay in
     *            chronological order; groups are sorted by their first
     *            event's timestamp.
     * Subtree filter, search, flag filter and time cutoffs apply the
     * same way in both modes. */
    std::string err;
    /* CAST ts_ns and tgid to VARCHAR explicitly: duckdb_value_varchar
     * on a UBIGINT/INTEGER column projected through UNION ALL can
     * return empty in this DuckDB build. */
    std::string sql =
        "SELECT 'O' AS k, CAST(ts_ns AS VARCHAR) AS ts, "
        "       CAST(tgid AS VARCHAR) AS tg, CAST(data AS VARCHAR) AS d "
        "FROM stdout_ UNION ALL "
        "SELECT 'E', CAST(ts_ns AS VARCHAR), CAST(tgid AS VARCHAR), "
        "       CAST(data AS VARCHAR) FROM stderr_";
    std::string where;
    if (!state_.search.empty())
        where = "d LIKE '%' || " + sql_escape(state_.search) + " || '%'";
    /* Output flag vocabulary: O = stdout, E = stderr.
     * Grammar lets you say "+O,-E" (only stdout, no stderr) etc. */
    FlagSpec flagspec = parse_flag_spec(state_.flag_filter);
    auto out_letter_sql = [](char l) -> std::string {
        switch (l) {
            case 'O': return "k = 'O'";
            case 'E': return "k = 'E'";
        }
        return "";
    };
    std::string fexpr = build_flag_sql(flagspec, out_letter_sql);
    if (!fexpr.empty()) {
        if (where.empty()) where = fexpr;
        else where = "(" + where + ") AND (" + fexpr + ")";
    }
    if (state_.ts_after_ns > 0)
        where = (where.empty() ? "" : "(" + where + ") AND ") +
                sfmt("CAST(ts AS UBIGINT) >= %llu",
                     (unsigned long long)state_.ts_after_ns);
    if (state_.ts_before_ns > 0)
        where = (where.empty() ? "" : "(" + where + ") AND ") +
                sfmt("CAST(ts AS UBIGINT) <= %llu",
                     (unsigned long long)state_.ts_before_ns);
    sql = "SELECT * FROM (" + sql + ")";
    if (!where.empty()) sql += " WHERE " + where;
    sql += " ORDER BY CAST(ts AS UBIGINT) LIMIT 5000";
    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    int64_t base_ns = 0;
    if (!rows.empty() && !rows[0][1].empty()) base_ns = std::stoll(rows[0][1]);

    /* For grouped mode we need the exe path of each producing tgid so
     * the group header reads as "[1234] /usr/bin/cat" rather than just
     * a number.  Pull from tv_idx_proc on demand. */
    std::unordered_map<std::string, std::string> tgid_exe;
    if (state_.grouped && !rows.empty()) {
        std::set<std::string> tgids;
        for (auto &r : rows) tgids.insert(r[2]);
        if (!tgids.empty()) {
            std::string in_list;
            for (auto &t : tgids) {
                if (!in_list.empty()) in_list += ",";
                in_list += t;
            }
            std::string e2;
            (void)db_.ensure_proc_index(&e2);
            auto exe_rows = db_.query_strings(
                "SELECT CAST(tgid AS VARCHAR), CAST(exe AS VARCHAR) "
                "FROM tv_idx_proc WHERE tgid IN (" + in_list + ")", &e2);
            for (auto &er : exe_rows) tgid_exe[er[0]] = er[1];
        }
    }

    auto build_event_row = [&](const std::vector<std::string> &r) {
        const std::string &k = r[0];
        int64_t ts_ns = r[1].empty() ? 0 : std::stoll(r[1]);
        const std::string &tgid = r[2];
        std::string data = sanitize_output_line(r[3], 200);
        double rel = (ts_ns - base_ns) / 1e9;
        std::string time_s = sfmt("%6.3fs ", rel);  /* trailing space = column gap */
        RowStyle st = (k == "E") ? RowStyle::Error : RowStyle::Normal;
        if (!state_.search.empty() &&
            r[3].find(state_.search) != std::string::npos)
            st = RowStyle::Search;
        RowData row;
        /* id encodes tgid:ts_ns:k so the rpane can re-locate the event. */
        row.id = tgid + ":" + std::to_string(ts_ns) + ":" + k;
        /* 4-column row matching k_lpane_cols_outputs:
         *   time | k | tgid | data
         * The trailing " " on the right-aligned cells gives a visible
         * gap before the next column - otherwise the cells run together
         * (the engine itself emits no inter-column padding). */
        row.cols.push_back(std::move(time_s));
        row.cols.push_back(k);
        row.cols.push_back(tgid + " ");
        row.cols.push_back(std::move(data));
        row.style = st;
        /* Enter on a row -> jump to the producing process in mode 1. */
        row.link_mode = 1;
        row.link_id   = tgid;
        return row;
    };

    if (!state_.grouped) {
        for (auto &r : rows) lpane_.push_back(build_event_row(r));
        return;
    }

    /* Grouped mode: bucket rows by tgid, preserving chronological order
     * inside each group.  Group order is by first-row ts_ns so the
     * earliest-emitting process appears first - which mirrors how the
     * proc tree (mode 1) tends to be ordered. */
    struct Bucket {
        std::string tgid;
        int64_t     first_ns = 0;
        std::vector<size_t> idxs;
    };
    std::unordered_map<std::string, size_t> bucket_idx;
    std::vector<Bucket> buckets;
    for (size_t i = 0; i < rows.size(); i++) {
        const std::string &tgid = rows[i][2];
        auto it = bucket_idx.find(tgid);
        if (it == bucket_idx.end()) {
            Bucket b;
            b.tgid = tgid;
            b.first_ns = rows[i][1].empty() ? 0 : std::stoll(rows[i][1]);
            bucket_idx[tgid] = buckets.size();
            buckets.push_back(std::move(b));
            it = bucket_idx.find(tgid);
        }
        buckets[it->second].idxs.push_back(i);
    }
    std::sort(buckets.begin(), buckets.end(),
              [](const Bucket &a, const Bucket &b){ return a.first_ns < b.first_ns; });
    for (auto &b : buckets) {
        std::string exe = tgid_exe.count(b.tgid) ? tgid_exe[b.tgid] : std::string("?");
        std::string label = "[" + b.tgid + "] " + basename_of(exe) +
                            "  (" + std::to_string(b.idxs.size()) + " line" +
                            (b.idxs.size() == 1 ? "" : "s") + ")";
        RowData hdr;
        hdr.id        = "__grp_" + b.tgid;
        hdr.style     = RowStyle::Heading;
        hdr.cols      = {"", "", "", label};
        hdr.has_children = true;
        lpane_.push_back(std::move(hdr));
        for (size_t i : b.idxs) lpane_.push_back(build_event_row(rows[i]));
    }
}

/* -- Mode 3: event log ---------------------------------------------- */

void TvDataSource::lpane_events() {
    /* UNION ALL across event tables.  Argv/env/auxv used to be omitted
     * (the rationale was "see the right pane for those") but that meant
     * `/foo` couldn't find a string that only appears as a CLI flag or
     * in PATH=... - reported as a real surprise.  We now surface them
     * as their own event kinds so a search hits them naturally. */
    std::string err;
    std::string sql =
        "SELECT 'EXEC'   AS kind, ts_ns, tgid, CAST(exe AS VARCHAR) AS info "
        "FROM exec UNION ALL "
        "SELECT 'ARGV'   AS kind, ts_ns, tgid, "
        "       'argv[' || idx || ']=' || CAST(arg AS VARCHAR) "
        "FROM argv UNION ALL "
        "SELECT 'ENV'    AS kind, ts_ns, tgid, "
        "       CAST(key AS VARCHAR) || '=' || CAST(val AS VARCHAR) "
        "FROM env UNION ALL "
        "SELECT 'CWD'    AS kind, ts_ns, tgid, CAST(cwd AS VARCHAR) "
        "FROM cwd UNION ALL "
        "SELECT 'OPEN'   AS kind, ts_ns, tgid, CAST(path AS VARCHAR) "
        "FROM open_ UNION ALL "
        "SELECT 'EXIT'   AS kind, ts_ns, tgid, "
        "       CASE WHEN status_kind=1 THEN 'sig ' || code_or_sig "
        "            ELSE 'code ' || code_or_sig END "
        "FROM exit_ UNION ALL "
        "SELECT 'STDOUT' AS kind, ts_ns, tgid, CAST(data AS VARCHAR) "
        "FROM stdout_ UNION ALL "
        "SELECT 'STDERR' AS kind, ts_ns, tgid, CAST(data AS VARCHAR) "
        "FROM stderr_";
    if (!state_.search.empty())
        sql = "SELECT * FROM (" + sql + ") WHERE info LIKE '%' || " +
              sql_escape(state_.search) + " || '%'";
    /* Time cutoffs apply uniformly across all event tables. */
    if (state_.ts_after_ns > 0 || state_.ts_before_ns > 0) {
        std::string w;
        if (state_.ts_after_ns > 0)
            w = sfmt("ts_ns >= %lld", (long long)state_.ts_after_ns);
        if (state_.ts_before_ns > 0) {
            if (!w.empty()) w += " AND ";
            w += sfmt("ts_ns <= %lld", (long long)state_.ts_before_ns);
        }
        sql = "SELECT * FROM (" + sql + ") WHERE " + w;
    }
    sql = "SELECT * FROM (" + sql + ") ORDER BY ts_ns LIMIT 5000";

    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    int64_t base_ns = 0;
    if (!rows.empty() && !rows[0][1].empty()) base_ns = std::stoll(rows[0][1]);
    for (auto &r : rows) {
        const std::string &kind = r[0];
        int64_t ts_ns = r[1].empty() ? 0 : std::stoll(r[1]);
        const std::string &tgid = r[2];
        std::string info = r[3];
        std::string raw_info = info;   /* search match check uses the raw */
        if (kind == "STDOUT" || kind == "STDERR") {
            info = sanitize_output_line(info, 120);
        }
        double rel = (ts_ns - base_ns) / 1e9;
        std::string time_s = sfmt("%6.3fs ", rel);   /* trailing space = column gap */
        RowStyle st = RowStyle::Normal;
        if      (kind == "STDERR" || kind == "EXIT") st = RowStyle::Error;
        else if (kind == "EXEC")                     st = RowStyle::CyanBold;
        else if (kind == "CWD")                      st = RowStyle::Cyan;
        else if (kind == "ARGV" || kind == "ENV")    st = RowStyle::Dim;
        if (!state_.search.empty() &&
            raw_info.find(state_.search) != std::string::npos)
            st = RowStyle::Search;
        RowData row;
        row.id = tgid + ":" + std::to_string(ts_ns) + ":" + kind;
        /* 4-column row matching k_lpane_cols_events:
         *   time | kind | tgid | info
         * Trailing space on right-aligned cells gives a visible gap to
         * the next column. */
        row.cols.push_back(std::move(time_s));
        row.cols.push_back(kind);
        row.cols.push_back(tgid + " ");
        row.cols.push_back(std::move(info));
        row.style = st;
        lpane_.push_back(std::move(row));
    }
}

/* -- Modes 4 & 5: dep / rdep file closure --------------------------- */

void TvDataSource::lpane_deps(int reverse) {
    if (state_.subject_file.empty()) {
        lpane_.push_back(mk_row("__hint",
            "(no subject file: navigate to a file in mode 2 and press 4-7)",
            RowStyle::Dim));
        return;
    }
    std::string err;
    if (!db_.ensure_edge_index(&err)) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    /* Recursive closure on (path) edges:
     *  reverse=0 (deps): from start, find all paths that fed into it.
     *    edge: P writes A and reads B  =>  A depends on B.
     *  reverse=1 (rdeps): from start, find all paths derived from it.
     *    edge: P reads A and writes B  =>  B is derived from A.
     */
    const char *src_mode = reverse ? "0" : "1";   // edge mode of start side
    const char *dst_mode = reverse ? "1" : "0";   // edge mode of "next" side
    std::string sql =
        "WITH RECURSIVE closure(path, depth) AS ("
        "  SELECT " + sql_escape(state_.subject_file) + " AS path, 0"
        "  UNION "
        "  SELECT e2.path, c.depth + 1 "
        "  FROM closure c "
        "  JOIN tv_idx_edge e1 ON e1.path = c.path AND e1.mode = " + src_mode +
        "  JOIN tv_idx_edge e2 ON e2.tgid = e1.tgid AND e2.mode = " + dst_mode +
        "                    AND e2.path <> c.path "
        "  WHERE c.depth < 8 "
        ") "
        "SELECT path, MIN(depth) AS d FROM closure "
        "GROUP BY path ORDER BY d, path LIMIT 5000";
    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    for (auto &r : rows) {
        const std::string &path = r[0];
        int d = std::atoi(r[1].c_str());
        std::string text = sfmt("%*s%s", d * 2, "", path.c_str());
        RowStyle st = (d == 0) ? RowStyle::Heading : RowStyle::Normal;
        if (!state_.search.empty() &&
            path.find(state_.search) != std::string::npos)
            st = RowStyle::Search;
        RowData row;
        row.id = path;
        /* 2-column row matching k_lpane_cols_deps:
         *   name | depth (just the closure depth - cheap, useful) */
        row.cols.push_back(std::move(text));
        row.cols.push_back(std::to_string(d));
        row.style = st;
        lpane_.push_back(std::move(row));
    }
}

/* -- Modes 6 & 7: dep / rdep cmds (processes in closure) ------------ */

void TvDataSource::lpane_dep_cmds(int reverse) {
    if (state_.subject_file.empty()) {
        lpane_.push_back(mk_row("__hint",
            "(no subject file: navigate to a file in mode 2 and press 4-7)",
            RowStyle::Dim));
        return;
    }
    std::string err;
    if (!db_.ensure_edge_index(&err)) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    if (!db_.ensure_proc_index(&err)) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    const char *src_mode = reverse ? "0" : "1";
    const char *dst_mode = reverse ? "1" : "0";
    std::string sql =
        "WITH RECURSIVE closure(path) AS ("
        "  SELECT " + sql_escape(state_.subject_file) +
        "  UNION "
        "  SELECT e2.path "
        "  FROM closure c "
        "  JOIN tv_idx_edge e1 ON e1.path = c.path AND e1.mode = " + src_mode +
        "  JOIN tv_idx_edge e2 ON e2.tgid = e1.tgid AND e2.mode = " + dst_mode +
        "                    AND e2.path <> c.path "
        "), "
        "procs AS ("
        "  SELECT DISTINCT e.tgid FROM tv_idx_edge e JOIN closure c ON e.path = c.path"
        ") "
        "SELECT p.tgid, CAST(p.exe AS VARCHAR), "
        "       p.start_ns, p.end_ns, p.exit_kind, p.exit_code, p.core_dumped "
        "FROM tv_idx_proc p JOIN procs USING (tgid) "
        "ORDER BY p.start_ns LIMIT 5000";
    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        lpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    for (auto &r : rows) {
        ProcRow p;
        p.tgid = std::atoi(r[0].c_str());
        p.exe  = r[1];
        p.name = basename_of(p.exe);
        p.start_ns = r[2].empty() ? 0 : std::stoll(r[2]);
        p.end_ns   = r[3].empty() ? p.start_ns : std::stoll(r[3]);
        p.exit_kind = r[4];
        p.exit_code = r[5];
        p.core_dumped = r[6];
        p.ended = !r[4].empty();
        std::string text = proc_label(p, true, /*show_pid=*/true);
        RowStyle st = RowStyle::Normal;
        if (!p.ended) st = RowStyle::Green;
        else if (p.exit_kind == "1" || (p.exit_code != "0" && !p.exit_code.empty()))
            st = RowStyle::Error;
        if (!state_.search.empty() &&
            (p.name.find(state_.search) != std::string::npos ||
             p.exe.find(state_.search)  != std::string::npos))
            st = RowStyle::Search;
        RowData row;
        row.id = std::to_string(p.tgid);
        /* 2-column row matching k_lpane_cols_dep_cmds: name | pid */
        row.cols.push_back(std::move(text));
        row.cols.push_back(std::to_string(p.tgid));
        row.style = st;
        lpane_.push_back(std::move(row));
    }
}

/* -- rpane dispatch ------------------------------------------------- */

namespace {
bool is_heading_id(const std::string &id) {
    return id.size() >= 2 && id[0] == '_' && id[1] == '_';
}
} // namespace

void TvDataSource::rebuild_rpane() {
    rpane_.clear();
    const std::string &cid = state_.cursor_id;
    if (cid.empty()) {
        rpane_.push_back(mk_row("__empty", "(select a row)", RowStyle::Dim));
        return;
    }
    switch (state_.mode) {
        case 0:
            rpane_output_detail(cid);
            break;
        case 1:
        case 6:
        case 7:
            rpane_process_detail(cid);
            break;
        case 2:
        case 4:
        case 5:
            rpane_file_detail(cid);
            break;
        case 3:
            rpane_event_detail(cid);
            break;
        default:
            rpane_.push_back(mk_row("__stub",
                "(no detail for mode " + std::to_string(state_.mode) + ")",
                RowStyle::Dim));
    }
    if (rpane_.empty())
        rpane_.push_back(mk_row("__empty", "(no detail)", RowStyle::Dim));

    /* Apply section collapsing: remove rows that fall under a heading
     * whose id appears in collapsed_sections, until the next heading.
     * Collapsed headings get a "[+]" indicator; expanded ones get
     * "[-]" so the user can see what's collapsed. */
    if (!state_.collapsed_sections.empty()) {
        std::vector<RowData> out;
        out.reserve(rpane_.size());
        bool skip = false;
        for (auto &r : rpane_) {
            if (is_heading_id(r.id)) {
                bool col = std::find(state_.collapsed_sections.begin(),
                                     state_.collapsed_sections.end(),
                                     r.id) != state_.collapsed_sections.end();
                skip = col;
                if (col && !r.cols.empty()) {
                    /* annotate so the user sees the section is collapsed */
                    r.cols[0] = "▶ " + r.cols[0];
                } else if (!r.cols.empty() && r.style == RowStyle::Heading) {
                    r.cols[0] = "▼ " + r.cols[0];
                }
                out.push_back(std::move(r));
                continue;
            }
            if (!skip) out.push_back(std::move(r));
        }
        rpane_ = std::move(out);
    } else {
        for (auto &r : rpane_) {
            if (is_heading_id(r.id) && r.style == RowStyle::Heading &&
                !r.cols.empty())
                r.cols[0] = "▼ " + r.cols[0];
        }
    }
}

/* -- rpane: process detail ------------------------------------------ */

void TvDataSource::rpane_process_detail(const std::string &tgid_s) {
    /* Validate tgid is numeric. */
    for (char c : tgid_s) if (c < '0' || c > '9') return;
    std::string err;
    if (!db_.ensure_proc_index(&err)) {
        rpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }

    auto pr = db_.query_strings(
        "SELECT tgid, ppid, CAST(exe AS VARCHAR), "
        "       start_ns, end_ns, exit_kind, exit_code, core_dumped "
        "FROM tv_idx_proc WHERE tgid = " + tgid_s, &err);
    if (!err.empty()) { rpane_.push_back(mk_row("__err", err, RowStyle::Error)); return; }
    if (pr.empty()) {
        rpane_.push_back(mk_row("__none", "(no process " + tgid_s + ")",
                                RowStyle::Dim));
        return;
    }
    auto &p = pr.front();
    int64_t start_ns = p[3].empty() ? 0 : std::stoll(p[3]);
    int64_t end_ns   = p[4].empty() ? start_ns : std::stoll(p[4]);
    bool ended = !p[5].empty();

    rpane_.push_back(mk_row("__hp", "[Process]", RowStyle::Heading));
    rpane_.push_back(mk_kv("tgid", "tgid", p[0]));
    rpane_.push_back(mk_kv("ppid", "ppid", p[1]));
    rpane_.push_back(mk_kv("exe",  "exe",  p[2]));
    rpane_.push_back(mk_kv("name", "name", basename_of(p[2])));
    rpane_.push_back(mk_kv("dur",  "duration", format_duration_ns(start_ns, end_ns)));
    rpane_.push_back(mk_kv("status", "status",
        format_exit(p[5], p[6], p[7], ended),
        ended ? (p[5] == "1" || (p[6] != "0" && !p[6].empty())
                 ? RowStyle::Error : RowStyle::Normal)
              : RowStyle::Green));

    /* argv */
    auto argv = db_.query_strings(
        "SELECT idx, CAST(arg AS VARCHAR) FROM argv WHERE tgid = " + tgid_s +
        " ORDER BY ts_ns DESC, idx ASC LIMIT 5000", &err);
    if (!argv.empty()) {
        rpane_.push_back(mk_row("__ha", "[argv]", RowStyle::Heading));
        for (auto &a : argv) {
            rpane_.push_back(mk_kv("argv_" + a[0], "[" + a[0] + "]", a[1]));
        }
    }

    /* children */
    auto kids = db_.query_strings(
        "SELECT tgid, CAST(exe AS VARCHAR) FROM tv_idx_proc "
        "WHERE ppid = " + tgid_s + " ORDER BY start_ns LIMIT 5000", &err);
    if (!kids.empty()) {
        rpane_.push_back(mk_row("__hc",
            sfmt("[children (%zu)]", kids.size()),
            RowStyle::Heading));
        for (auto &k : kids) {
            RowData rw = mk_kv("child_" + k[0], "[" + k[0] + "]",
                               basename_of(k[1]));
            rw.link_mode = 1;       /* mode 1: process tree */
            rw.link_id   = k[0];    /* tgid */
            rpane_.push_back(std::move(rw));
        }
    }

    /* opens - first 5000 with flags + err - using canonicalised paths
     * so the user sees the same path string the file view shows. */
    auto opens = db_.query_strings(
        "SELECT ts_ns, fd, err, flags, path "
        "FROM tv_idx_open_canon WHERE tgid = " + tgid_s +
        " ORDER BY ts_ns LIMIT 5000", &err);
    if (!opens.empty()) {
        rpane_.push_back(mk_row("__ho", sfmt("[opens (%zu)]", opens.size()),
            RowStyle::Heading));
        size_t i = 0;
        for (auto &o : opens) {
            int err_ = std::atoi(o[2].c_str());
            int flags = std::atoi(o[3].c_str());
            std::string lhs = sfmt("fd %s [%s]%s", o[1].c_str(),
                                   flags_text(flags).c_str(),
                                   err_ ? sfmt(" err %d", err_).c_str() : "");
            RowData rw = mk_kv("open_" + std::to_string(i++), lhs, o[4],
                err_ ? RowStyle::Error : RowStyle::Normal);
            rw.link_mode = 2;       /* mode 2: file tree */
            rw.link_id   = o[4];    /* path */
            rpane_.push_back(std::move(rw));
        }
    }

    /* env (first 64) */
    auto env = db_.query_strings(
        "SELECT idx, CAST(key AS VARCHAR), CAST(val AS VARCHAR) "
        "FROM env WHERE tgid = " + tgid_s + " ORDER BY idx LIMIT 5000", &err);
    if (!env.empty()) {
        rpane_.push_back(mk_row("__he", sfmt("[env (%zu)]", env.size()),
            RowStyle::Heading));
        for (auto &e : env)
            rpane_.push_back(mk_kv("env_" + e[0], e[1], e[2]));
    }
}

/* -- rpane: output (mode 0) - show the producing process's events - */

void TvDataSource::rpane_output_detail(const std::string &id) {
    /* id format: "tgid:ts_ns:k" where k is O (stdout) or E (stderr).
     * Right pane lists every event from the producing tgid in
     * chronological order, with the source output event preselected
     * (cursor will land on it because we use the same "tgid:ts_ns" id
     * convention as mode 3). */
    auto p1 = id.find(':');
    if (p1 == std::string::npos) return;
    auto p2 = id.find(':', p1 + 1);
    std::string tgid_s = id.substr(0, p1);
    std::string ts_s   = (p2 == std::string::npos)
        ? id.substr(p1 + 1)
        : id.substr(p1 + 1, p2 - p1 - 1);
    for (char c : tgid_s) if (c < '0' || c > '9') return;

    std::string err;
    /* Process header. */
    auto pr = db_.query_strings(
        "SELECT CAST(exe AS VARCHAR), start_ns, end_ns, "
        "       exit_kind, exit_code "
        "FROM tv_idx_proc WHERE tgid = " + tgid_s, &err);
    if (!pr.empty()) {
        rpane_.push_back(mk_row("__hp", "[Process]", RowStyle::Heading));
        rpane_.push_back(mk_kv("tgid", "tgid", tgid_s));
        rpane_.push_back(mk_kv("name", "name", basename_of(pr.front()[0])));
        rpane_.push_back(mk_kv("exe",  "exe",  pr.front()[0]));
    }

    /* Event list for this tgid (UNION ALL across event tables).
     * We CAST ts_ns to VARCHAR explicitly because the deprecated
     * duckdb_value_varchar API on a column whose union-projected type
     * is UBIGINT can sometimes return empty. */
    rpane_.push_back(mk_row("__he", "[Events]", RowStyle::Heading));
    std::string sql =
        "SELECT * FROM ("
        "  SELECT 'EXEC'   AS kind, CAST(ts_ns AS VARCHAR) AS ts, CAST(exe AS VARCHAR) AS info "
        "  FROM exec WHERE tgid = " + tgid_s +
        "  UNION ALL "
        "  SELECT 'CWD',    CAST(ts_ns AS VARCHAR), CAST(cwd AS VARCHAR) "
        "  FROM cwd WHERE tgid = " + tgid_s +
        "  UNION ALL "
        "  SELECT 'OPEN',   CAST(ts_ns AS VARCHAR), CAST(path AS VARCHAR) "
        "  FROM open_ WHERE tgid = " + tgid_s +
        "  UNION ALL "
        "  SELECT 'EXIT',   CAST(ts_ns AS VARCHAR), "
        "    CASE WHEN status_kind=1 THEN 'sig ' || code_or_sig "
        "         ELSE 'code ' || code_or_sig END "
        "  FROM exit_ WHERE tgid = " + tgid_s +
        "  UNION ALL "
        "  SELECT 'STDOUT', CAST(ts_ns AS VARCHAR), CAST(data AS VARCHAR) "
        "  FROM stdout_ WHERE tgid = " + tgid_s +
        "  UNION ALL "
        "  SELECT 'STDERR', CAST(ts_ns AS VARCHAR), CAST(data AS VARCHAR) "
        "  FROM stderr_ WHERE tgid = " + tgid_s +
        ") ORDER BY CAST(ts AS UBIGINT) LIMIT 5000";
    auto rows = db_.query_strings(sql, &err);
    int64_t base_ns = 0;
    if (!rows.empty() && !rows[0][1].empty()) base_ns = std::stoll(rows[0][1]);
    for (auto &r : rows) {
        const std::string &kind = r[0];
        int64_t ts_ns = r[1].empty() ? 0 : std::stoll(r[1]);
        std::string info = r[2];
        if (kind == "STDOUT" || kind == "STDERR") {
            for (auto &c : info)
                if (c == '\n' || c == '\r' || c == '\t') c = ' ';
            if (info.size() > 90) info = info.substr(0, 87) + "...";
        }
        double rel = (ts_ns - base_ns) / 1e9;
        std::string lhs = sfmt("+%6.3fs %-6s", rel, kind.c_str());
        RowStyle st = (kind == "STDERR" || kind == "EXIT")
            ? RowStyle::Error
            : (kind == "EXEC" ? RowStyle::CyanBold : RowStyle::Normal);
        /* Highlight the event the user clicked on. */
        std::string row_id = "ev_" + std::to_string(ts_ns);
        if (std::to_string(ts_ns) == ts_s) st = RowStyle::Search;
        rpane_.push_back(mk_kv(row_id, lhs, info, st));
    }
}

/* -- rpane: file detail --------------------------------------------- */

void TvDataSource::rpane_file_detail(const std::string &path) {
    std::string err;
    if (!db_.ensure_path_index(&err)) {
        rpane_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    /* Synthetic directory rows from mode 2's tree view end with '/'.
     * Show an aggregate over everything under that prefix instead of
     * "(no path)". */
    bool is_dir = !path.empty() && path.back() == '/';
    std::string display_path = path;
    std::string where_clause;
    if (is_dir) {
        where_clause = "WHERE path LIKE " +
            sql_escape(path + "%");
    } else {
        where_clause = "WHERE path = " + sql_escape(path);
    }
    auto pr = db_.query_strings(
        "SELECT SUM(opens), SUM(errors), SUM(procs), "
        "       SUM(reads), SUM(writes), "
        "       MIN(first_ns), MAX(last_ns), COUNT(*) "
        "FROM tv_idx_path " + where_clause, &err);
    if (pr.empty() || pr.front()[7] == "0" || pr.front()[7].empty()) {
        rpane_.push_back(mk_row("__none", "(no path " + path + ")", RowStyle::Dim));
        return;
    }
    auto &r = pr.front();
    rpane_.push_back(mk_row("__hf",
        is_dir ? "[Directory]" : "[File]", RowStyle::Heading));
    rpane_.push_back(mk_kv("path", "path", display_path));
    if (is_dir)
        rpane_.push_back(mk_kv("count", "files", r[7]));
    rpane_.push_back(mk_kv("opens",  "opens",  r[0]));
    rpane_.push_back(mk_kv("errors", "errors", r[1],
        std::atoi(r[1].c_str()) ? RowStyle::Error : RowStyle::Normal));
    rpane_.push_back(mk_kv("procs",  "procs",  r[2]));
    rpane_.push_back(mk_kv("reads",  "reads",  r[3]));
    rpane_.push_back(mk_kv("writes", "writes", r[4],
        std::atoi(r[4].c_str()) ? RowStyle::Yellow : RowStyle::Normal));

    /* opens log: who, flags, err, using the canonicalised view. */
    std::string opens_where = is_dir
        ? "WHERE o.path LIKE " + sql_escape(path + "%")
        : "WHERE o.path = " + sql_escape(path);
    auto opens = db_.query_strings(
        "SELECT o.ts_ns, o.tgid, o.fd, o.err, o.flags, "
        "       CAST(p.exe AS VARCHAR), o.path "
        "FROM tv_idx_open_canon o "
        "LEFT JOIN tv_idx_proc p ON p.tgid = o.tgid "
        + opens_where +
        " ORDER BY o.ts_ns LIMIT 5000", &err);
    if (!opens.empty()) {
        rpane_.push_back(mk_row("__ho", sfmt("[opens (%zu)]", opens.size()),
            RowStyle::Heading));
        size_t i = 0;
        for (auto &o : opens) {
            int err_ = std::atoi(o[3].c_str());
            int flags = std::atoi(o[4].c_str());
            std::string lhs = sfmt("[%s] %s", o[1].c_str(),
                                   basename_of(o[5]).c_str());
            std::string rhs = sfmt("fd %s [%s]%s%s%s", o[2].c_str(),
                                   flags_text(flags).c_str(),
                                   err_ ? sfmt(" err %d", err_).c_str() : "",
                                   is_dir ? "  " : "",
                                   is_dir ? o[6].c_str() : "");
            RowData rw = mk_kv("open_" + std::to_string(i++), lhs, rhs,
                               err_ ? RowStyle::Error : RowStyle::Normal);
            rw.link_mode = 1;       /* mode 1: process tree */
            rw.link_id   = o[1];    /* tgid */
            rpane_.push_back(std::move(rw));
        }
    }
}

/* -- rpane: event detail -------------------------------------------- */

void TvDataSource::rpane_event_detail(const std::string &id) {
    /* id format: "tgid:ts_ns" */
    auto colon = id.find(':');
    if (colon == std::string::npos) return;
    std::string tgid_s = id.substr(0, colon);
    std::string ts_s   = id.substr(colon + 1);
    for (char c : tgid_s) if (c < '0' || c > '9') return;
    for (char c : ts_s)   if (c < '0' || c > '9') return;

    rpane_.push_back(mk_row("__he", "[Event]", RowStyle::Heading));
    rpane_.push_back(mk_kv("tgid", "tgid", tgid_s));
    rpane_.push_back(mk_kv("ts_ns", "ts_ns", ts_s));

    std::string err;
    /* Find which table it belongs to. */
    struct EvSrc { const char *kind; const char *table; const char *expr; };
    EvSrc srcs[] = {
        {"EXEC",   "exec",    "CAST(exe AS VARCHAR)"},
        {"CWD",    "cwd",     "CAST(cwd AS VARCHAR)"},
        {"OPEN",   "open_",   "CAST(path AS VARCHAR) || ' [fd ' || fd || ']'"},
        {"EXIT",   "exit_",   "CASE WHEN status_kind=1 THEN 'sig ' || code_or_sig ELSE 'code ' || code_or_sig END"},
        {"STDOUT", "stdout_", "CAST(data AS VARCHAR)"},
        {"STDERR", "stderr_", "CAST(data AS VARCHAR)"},
    };
    for (auto &s : srcs) {
        std::string sql = std::string("SELECT ") + s.expr +
            " FROM " + s.table + " WHERE tgid = " + tgid_s +
            " AND ts_ns = " + ts_s + " LIMIT 1";
        auto rs = db_.query_strings(sql, &err);
        if (!rs.empty()) {
            rpane_.push_back(mk_kv("kind", "kind", s.kind));
            rpane_.push_back(mk_kv("info", "info", rs.front()[0],
                (std::strcmp(s.kind, "STDERR") == 0 ||
                 std::strcmp(s.kind, "EXIT")   == 0)
                    ? RowStyle::Error : RowStyle::Normal));
            break;
        }
    }
    /* Always show the process this event belongs to. */
    auto pr = db_.query_strings(
        "SELECT CAST(exe AS VARCHAR) "
        "FROM tv_idx_proc WHERE tgid = " + tgid_s, &err);
    if (!pr.empty()) {
        rpane_.push_back(mk_row("__hp", "[Process]", RowStyle::Heading));
        rpane_.push_back(mk_kv("name", "name", basename_of(pr.front()[0])));
        rpane_.push_back(mk_kv("exe",  "exe",  pr.front()[0]));
    }
}


/* -- htop snapshot column ------------------------------------------- *
 *
 * Render the process tree as it stood at state_.htop_anchor_ns, the
 * timestamp of whatever event the lpane cursor sits on.  The snapshot
 * is rebuilt whenever the anchor or focus tgid changes; drawing happens
 * via the standard panel iterator (panel index 4).
 *
 * Inclusion rule:
 *   start_ns <= T  AND  (end_ns IS NULL OR end_ns >= T - 1s)
 * - "alive at T" sets the lower bound on end_ns.
 * - The 1 s grace lets the user *see* a recently-exited process for a
 *   moment after its death; that is what `htop` does too, and it is
 *   what the bug report explicitly asks for ("processes that exited
 *   less than a second before shown time are red").
 *
 * Coloring (matching the spec):
 *   - Green  : just spawned   - T - start_ns < 1 s
 *   - Error  : just died      - end_ns IS NOT NULL AND T - end_ns < 1 s
 *   - Search : focus tgid     - the producer of the selected event
 *   - Dim    : everything else
 *
 * The pane is read-only (no cursor, Tab skips it), so the order of
 * children just needs to be reproducible: sort by start_ns. */

void TvDataSource::rebuild_htop() {
    htop_.clear();
    std::string err;
    if (!db_.ensure_proc_index(&err)) {
        htop_.push_back(mk_row("__err", "(index: " + err + ")",
                               RowStyle::Error));
        return;
    }
    int64_t T = state_.htop_anchor_ns;
    if (T <= 0) {
        htop_.push_back(mk_row("__hint",
            "(no anchor — select an event)", RowStyle::Dim));
        return;
    }
    /* Recently-exited grace: 1 second.  Same threshold drives the red
     * colouring below, so it stays consistent. */
    const int64_t GRACE_NS = 1000000000LL;
    std::string sql = sfmt(
        "SELECT tgid, ppid, CAST(exe AS VARCHAR), start_ns, end_ns "
        "FROM tv_idx_proc "
        "WHERE start_ns <= %lld "
        "  AND (end_ns IS NULL OR end_ns >= %lld) "
        "ORDER BY start_ns",
        (long long)T, (long long)(T - GRACE_NS));
    auto rows = db_.query_strings(sql, &err);
    if (!err.empty()) {
        htop_.push_back(mk_row("__err", err, RowStyle::Error));
        return;
    }
    if (rows.empty()) {
        htop_.push_back(mk_row("__empty",
            "(no procs alive at this moment)", RowStyle::Dim));
        return;
    }

    struct Snap {
        int         tgid = 0;
        int         ppid = 0;
        std::string exe;
        int64_t     start_ns = 0;
        int64_t     end_ns   = 0;
        bool        ended    = false;
        std::vector<int> children;
    };
    std::unordered_map<int, Snap> snap;
    snap.reserve(rows.size());
    for (auto &r : rows) {
        Snap s;
        s.tgid     = std::atoi(r[0].c_str());
        s.ppid     = std::atoi(r[1].c_str());
        s.exe      = r[2];
        s.start_ns = r[3].empty() ? 0 : std::stoll(r[3]);
        s.ended    = !r[4].empty();
        s.end_ns   = s.ended ? std::stoll(r[4]) : 0;
        snap.emplace(s.tgid, std::move(s));
    }
    std::vector<int> roots;
    for (auto &kv : snap) {
        Snap &s = kv.second;
        auto it = snap.find(s.ppid);
        if (it == snap.end()) roots.push_back(s.tgid);
        else                  it->second.children.push_back(s.tgid);
    }
    auto by_start = [&](int a, int b){ return snap[a].start_ns < snap[b].start_ns; };
    std::sort(roots.begin(), roots.end(), by_start);
    for (auto &kv : snap) std::sort(kv.second.children.begin(),
                                    kv.second.children.end(), by_start);

    int focus_tgid = state_.htop_focus_tgid.empty() ? 0
                       : std::atoi(state_.htop_focus_tgid.c_str());

    /* Header: anchor as wall-clock-relative seconds plus a little
     * legend so the colour code is discoverable.  The anchor is shown
     * relative to the *trace's* first event rather than absolute ns —
     * matches the time column convention used elsewhere in tv. */
    int64_t base_ns = T;
    {
        auto br = db_.query_strings(
            "SELECT MIN(ts_ns) FROM ("
            "  SELECT ts_ns FROM exec UNION ALL "
            "  SELECT ts_ns FROM exit_)", &err);
        if (!br.empty() && !br[0][0].empty())
            try { base_ns = std::stoll(br[0][0]); } catch (...) {}
    }
    {
        double rel = (T - base_ns) / 1e9;
        RowData hdr;
        hdr.id    = "__htop_hdr";
        hdr.style = RowStyle::Heading;
        hdr.cols  = {sfmt("at t=%.3fs  green=spawned  red=exited  /focus", rel)};
        htop_.push_back(std::move(hdr));
    }

    std::function<void(int, const std::string &, bool)> emit =
        [&](int t, const std::string &prefix, bool is_last) {
            Snap &s = snap[t];
            std::string indent = prefix;
            if (!prefix.empty()) indent += is_last ? "└─ " : "├─ ";
            std::string name = basename_of(s.exe);
            if (name.empty()) name = "?";
            std::string label = indent + name + " [" + std::to_string(s.tgid) + "]";

            RowStyle style = RowStyle::Dim;
            if (s.tgid == focus_tgid)              style = RowStyle::Search;
            else if (T - s.start_ns < GRACE_NS)    style = RowStyle::Green;
            else if (s.ended && T > s.end_ns &&
                     T - s.end_ns  < GRACE_NS)     style = RowStyle::Error;
            else                                   style = RowStyle::Normal;

            RowData row;
            row.id        = "h:" + std::to_string(s.tgid);
            row.parent_id = "h:" + std::to_string(s.ppid);
            row.style     = style;
            row.cols      = {label};
            htop_.push_back(std::move(row));

            std::string cp = prefix + (is_last ? "   " : "│  ");
            for (size_t i = 0; i < s.children.size(); i++)
                emit(s.children[i], cp, i + 1 == s.children.size());
        };
    for (size_t i = 0; i < roots.size(); i++)
        emit(roots[i], "", i + 1 == roots.size());
}
