/* main.cpp - tv top-level: arg parsing, ingest, and TUI wiring.
 *
 * The whole pipeline:
 *
 *   wire bytes  --->  TraceDecoder  --->  TvDb (DuckDB Appender)  --->  .tvdb file
 *      ^                                                                  |
 *      |                                                                  v
 *   uproctrace child / `--trace foo.wire`                  TvDataSource (SQL queries)
 *                                                                          |
 *                                                                          v
 *                                                                    Tui (engine.h)
 *
 * No in-memory aggregation, no JSONL, one wire decoder, one storage
 * format. Trace files on disk are DuckDB native (`foo.tvdb`); they
 * are mmaped on open and queried directly - no "load" step.
 */

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cerrno>
#include <csignal>

#include <unistd.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <fcntl.h>

#include <algorithm>
#include <memory>
#include <string>
#include <vector>

#include <zstd.h>

#include "engine.h"
#include "trace/trace_stream.h"
#include "tv_db.h"
#include "data_source.h"

extern int run_tests(); /* tests.cpp */
extern int fv_main(int argc, char **argv); /* fv.cpp */
extern "C" int wiredump_main(int argc, char **argv);  /* tools/wiredump/wiredump.c */

namespace {

const char *USAGE =
    "tv - process trace viewer (DuckDB-backed)\n"
    "\n"
    "Subcommands:\n"
    "  tv --tracer EXE -- <cmd> [args...]\n"
    "                              record live (via tracer) and view\n"
    "  tv --trace <file.bin[.zst]>       ingest into <file>.tvdb and view\n"
    "  tv --open  <file.tvdb>            open existing tvdb and view\n"
    "  tv --dump                         dump mode 1 (process tree) to stdout (with --trace/--open)\n"
    "\n"
    "Tracer binaries are now separate executables — pick one with --tracer:\n"
    "  upttrace    ptrace-based, works anywhere\n"
    "  sudtrace    syscall-user-dispatch, fastest\n"
    "  modtrace    kernel module, lowest overhead\n"
    "\n"
    "Tools:\n"
    "  tv dump <trace-file>...     hexdump a trace stream\n"
    "  tv dump --selftest          atom-format roundtrip test\n"
    "  tv fv [path]                file viewer\n"
    "  tv test                     run built-in self-tests\n"
    "  tv ingest <trace> [-o OUT]  convert trace to .tvdb without UI\n";

/* -- ingest helpers ------------------------------------------------- */

bool has_suffix(const char *s, const char *suf) {
    size_t n = std::strlen(s), k = std::strlen(suf);
    return n >= k && std::memcmp(s + n - k, suf, k) == 0;
}

/* Pipe the bytes of `f` through a wire decoder feeding `db`. Returns
 * false on a hard wire/decode error or db append failure. */
bool ingest_wire_plain(FILE *f, TvDb &db, std::string *err) {
    TraceDecoder dec([&](const TraceEvent &ev) {
        std::string e;
        if (!db.append(ev, &e)) {
            std::fprintf(stderr, "tv: ingest: %s\n", e.c_str());
        }
    });
    char buf[64 * 1024];
    while (true) {
        size_t n = std::fread(buf, 1, sizeof buf, f);
        if (n == 0) break;
        if (!dec.feed(buf, n)) {
            if (err) *err = "wire decode error";
            return false;
        }
    }
    return true;
}

bool ingest_wire_zstd(FILE *f, TvDb &db, std::string *err) {
    ZSTD_DCtx *dctx = ZSTD_createDCtx();
    if (!dctx) { if (err) *err = "ZSTD_createDCtx failed"; return false; }
    size_t in_cap = ZSTD_DStreamInSize();
    size_t out_cap = ZSTD_DStreamOutSize();
    std::unique_ptr<char[]> in_buf(new char[in_cap]);
    std::unique_ptr<char[]> out_buf(new char[out_cap]);
    bool ok = true;
    TraceDecoder dec([&](const TraceEvent &ev) {
        std::string e;
        if (!db.append(ev, &e)) {
            std::fprintf(stderr, "tv: ingest: %s\n", e.c_str());
        }
    });
    while (true) {
        size_t n = std::fread(in_buf.get(), 1, in_cap, f);
        if (n == 0) break;
        ZSTD_inBuffer in{in_buf.get(), n, 0};
        while (in.pos < in.size) {
            ZSTD_outBuffer out{out_buf.get(), out_cap, 0};
            size_t r = ZSTD_decompressStream(dctx, &out, &in);
            if (ZSTD_isError(r)) {
                if (err) *err = std::string("zstd: ") + ZSTD_getErrorName(r);
                ok = false;
                goto done;
            }
            if (out.pos > 0) {
                if (!dec.feed(out_buf.get(), out.pos)) {
                    if (err) *err = "wire decode error";
                    ok = false;
                    goto done;
                }
            }
        }
    }
done:
    ZSTD_freeDCtx(dctx);
    return ok;
}

bool ingest_wire_file(const char *path, TvDb &db, std::string *err) {
    FILE *f = std::fopen(path, "rb");
    if (!f) {
        if (err) *err = std::string("open ") + path + ": " + std::strerror(errno);
        return false;
    }
    bool zst = has_suffix(path, ".zst");
    bool ok = zst ? ingest_wire_zstd(f, db, err) : ingest_wire_plain(f, db, err);
    std::fclose(f);
    return ok;
}

/* Derive a default `.tvdb` path next to the given wire file. */
std::string default_tvdb_for_wire(const char *wire_path) {
    std::string p = wire_path;
    /* strip .zst */
    if (p.size() > 4 && p.compare(p.size() - 4, 4, ".zst") == 0)
        p.resize(p.size() - 4);
    /* strip .wire */
    if (p.size() > 5 && p.compare(p.size() - 5, 5, ".wire") == 0)
        p.resize(p.size() - 5);
    return p + ".tvdb";
}

bool file_exists(const char *path) {
    struct stat st;
    return ::stat(path, &st) == 0;
}

/* -- live-trace pipe glue -------------------------------------------- */

struct LiveTrace {
    pid_t          child_pid = 0;
    int            fd = -1;
    TvDb          *db = nullptr;
    TraceDecoder   *dec = nullptr;
};

void on_trace_fd_cb(Tui &tui, int fd, LiveTrace &lt) {
    char buf[64 * 1024];
    ssize_t n = ::read(fd, buf, sizeof buf);
    if (n <= 0) {
        tui.unwatch_fd(fd);
        if (n == 0) std::fprintf(stderr, "tv: trace pipe EOF\n");
        ::close(fd); lt.fd = -1;
        return;
    }
    if (!lt.dec->feed(buf, n)) {
        std::fprintf(stderr, "tv: wire decode error\n");
        tui.unwatch_fd(fd); ::close(fd); lt.fd = -1;
    }
    /* Trigger redraw by dirtying both panels. */
    tui.dirty();
}

/* -- TUI key handling ------------------------------------------------ */

const char *HELP_LINES[] = {
    "",
    "  tv - Process Trace Viewer (DuckDB-backed)",
    "  -----------------------------------------",
    "",
    "  Up/Down jk  Navigate    PgUp/PgDn  Page    g/G  First/Last",
    "  Tab         Toggle section (info pane) / switch focus elsewhere",
    "  Enter       Follow row to its target panel",
    "",
    "  /text       Glob/text filter         Esc    Clear filters (layered)",
    "  f...        Flag filter. Grammar:",
    "                 +L  require flag L     -L  forbid flag L",
    "                 ',' separates OR-groups; juxtaposition = AND",
    "                 examples: '+W'   '+W-E'   '+W,+R'   '-k-s'",
    "              Flag letters per mode:",
    "                 mode 1 procs:  K=killed/running  F=fail-with-write",
    "                                D=derived-exec",
    "                 mode 2 files:  R/W/E  +  w=written  f=fail-writer",
    "                                s=non-system  k=non-kernel-iface",
    "                 mode 0 output: O=stdout E=stderr",
    "  <  >        Time cutoff (before/after) seeded from cursor;",
    "              press the same key again to clear.",
    "  s           Subtree-only (mode 1)     p   Show pid prefix (mode 1)",
    "  t           Tree / flat toggle (modes 1, 2; group output by tgid in mode 0)",
    "  T           Toggle htop-style snapshot column (process tree at cursor moment)",
    "",
    "  0  Output stream    1  Process tree    2  File tree    3  Event log",
    "  4  Deps             5  Reverse deps    6  Dep cmds     7  RDep cmds",
    "  q  Quit             ?  Help",
    nullptr
};

struct UiCtx {
    Tui          *tui = nullptr;
    int           lpane = -1;
    int           rpane = -1;
    int           hat_top = -1;
    int           hat_bot = -1;
    int           htop_pane = -1;
    /* Box pointers for the hat panes — their min_size is mutated each
     * render to match the data source's hat row count, so an empty
     * hat takes zero rows on screen. */
    Box          *hat_top_box = nullptr;
    Box          *hat_bot_box = nullptr;
    /* Box pointer for the htop snapshot column - toggled visible by
     * the `T` key.  When invisible we set both weight and min_size to
     * 0 so it occupies no horizontal space; visible we set weight=1
     * and min_size=24 so it claims roughly a third of the screen but
     * never collapses below something readable. */
    Box          *htop_box = nullptr;
    AppState     *state = nullptr;
    TvDataSource *src = nullptr;
    TvDb         *db = nullptr;       /* for direct lookups (e.g. ts seed) */
    /* Idle cursor commit: lpane cursor changes set pending_cursor_id;
     * a 100 ms timer commits it (and rebuilds the rpane) 300 ms after
     * the last change.  This restores the previous "autopopulate on
     * inactivity" behaviour without rebuilding the rpane on every
     * keystroke. */
    std::string   pending_cursor_id;
    int64_t       last_cursor_change_ms = 0;
    bool          have_pending = false;
};

int64_t now_ms() {
    struct timeval tv; ::gettimeofday(&tv, nullptr);
    return (int64_t)tv.tv_sec * 1000 + tv.tv_usec / 1000;
}

/* Resolve the timestamp (in ns) corresponding to the cursor's row in
 * the current mode.  Used by the time-cutoff `</>` keys *and* by the
 * htop snapshot column to anchor "the moment in time we want to look
 * at".  Returns 0 when no timestamp can be derived (no cursor, or the
 * mode has no notion of per-row time - modes 4..7).  Also fills out
 * *focus_tgid (when non-null) with the row's producing tgid as a
 * string, so the htop column can highlight that process inside the
 * snapshot tree. */
int64_t cursor_ts_ns(UiCtx &c, std::string *focus_tgid = nullptr) {
    if (focus_tgid) focus_tgid->clear();
    if (!c.state || c.state->cursor_id.empty() || !c.db) return 0;
    const std::string &cur = c.state->cursor_id;
    if (c.state->mode == 0 || c.state->mode == 3) {
        /* cursor id is "tgid:ts_ns[:k]" - both modes use the same id
         * shape so the htop anchor follows wherever the user lands. */
        auto p1 = cur.find(':');
        if (p1 == std::string::npos) return 0;
        if (focus_tgid) *focus_tgid = cur.substr(0, p1);
        auto p2 = cur.find(':', p1 + 1);
        std::string ts_s = (p2 == std::string::npos)
            ? cur.substr(p1 + 1)
            : cur.substr(p1 + 1, p2 - p1 - 1);
        try { return std::stoll(ts_s); } catch (...) { return 0; }
    }
    if (c.state->mode == 1) {
        std::string e;
        auto rows = c.db->query_strings(
            "SELECT start_ns FROM tv_idx_proc WHERE tgid = " +
            cur + " LIMIT 1", &e);
        if (focus_tgid) *focus_tgid = cur;
        if (!rows.empty() && !rows[0][0].empty())
            try { return std::stoll(rows[0][0]); } catch (...) {}
        return 0;
    }
    if (c.state->mode == 2) {
        std::string esc = "'";
        for (char ch : cur) { if (ch == '\'') esc += "''"; else esc += ch; }
        esc += "'";
        std::string e;
        auto rows = c.db->query_strings(
            "SELECT first_ns FROM tv_idx_path WHERE path = " +
            esc + " LIMIT 1", &e);
        if (!rows.empty() && !rows[0][0].empty())
            try { return std::stoll(rows[0][0]); } catch (...) {}
        return 0;
    }
    return 0;
}

/* Mark the htop snapshot column visible/invisible by toggling its
 * box's weight + min_size.  Wrapped in a helper because the same
 * mutation runs from the `T` key handler and from the layout setup. */
void apply_htop_visibility(UiCtx &c) {
    if (!c.htop_box) return;
    if (c.state && c.state->show_htop_col) {
        c.htop_box->weight   = 1;
        c.htop_box->min_size = 24;
    } else {
        c.htop_box->weight   = 0;
        c.htop_box->min_size = 0;
    }
    if (c.tui && c.htop_pane >= 0) c.tui->dirty(c.htop_pane);
}

/* Refresh the htop anchor from the current cursor and mark the htop
 * panel dirty.  No-op when the column is hidden.  Called when the
 * cursor commits (idle timer) and when `T` toggles the column on. */
void refresh_htop_anchor(UiCtx &c) {
    if (!c.state || !c.state->show_htop_col || !c.src) return;
    std::string focus;
    int64_t ts = cursor_ts_ns(c, &focus);
    /* Update focus tgid even when ts is 0 - the htop builder will
     * surface a "(no anchor)" hint and the user gets the toggle
     * feedback either way. */
    if (c.state->htop_focus_tgid != focus) c.state->htop_focus_tgid = focus;
    c.src->set_htop_anchor_ns(ts);
    if (c.tui && c.htop_pane >= 0) c.tui->dirty(c.htop_pane);
}

/* Update the hat-pane Box.min_size values to match the data source's
 * current hat row counts and mark the hat panels dirty. Called after
 * any state change that invalidates lpane (the hats are built as a
 * side-effect of building lpane).
 *
 * Why this exists: the hat panels are weight=0 fixed-size boxes; the
 * engine takes their height from min_size. When a hat has no rows we
 * want its panel to occupy zero screen rows (engine.cpp allows this
 * for weight=0,min_size=0). Per-mode the hat may be 0 or 1 rows. */
void sync_hats(UiCtx &c) {
    if (!c.src || !c.tui || !c.hat_top_box || !c.hat_bot_box) return;
    int t = c.src->hat_top_row_count();
    int b = c.src->hat_bot_row_count();
    if (c.hat_top_box->min_size != t) c.hat_top_box->min_size = t;
    if (c.hat_bot_box->min_size != b) c.hat_bot_box->min_size = b;
    c.tui->dirty(c.hat_top);
    c.tui->dirty(c.hat_bot);
}

void update_status(UiCtx &c) {
    if (!c.tui) return;
    static const char *MN[] = {
        "0:out",  "1:proc", "2:file", "3:event",
        "4:dep",  "5:rdep", "6:dcmd", "7:rcmd"
    };
    int m = c.state->mode;
    const char *mn = (m >= 0 && m <= 7) ? MN[m] : "?";
    char buf[1024];
    int n = std::snprintf(buf, sizeof buf, "tv | %s%s",
        mn, c.state->grouped ? " tree" : " flat");
    if (!c.state->cursor_id.empty())
        n += std::snprintf(buf + n, sizeof buf - n, " | cur=%s",
                           c.state->cursor_id.c_str());
    if (!c.state->subject_file.empty() && m >= 4 && m <= 7)
        n += std::snprintf(buf + n, sizeof buf - n, " | subj=%s",
                           c.state->subject_file.c_str());
    /* Composed filter list - every active filter shown.  Order:
     * subtree, glob, flags. */
    if (c.state->subtree_only && !c.state->subtree_root.empty())
        n += std::snprintf(buf + n, sizeof buf - n, " | subtree=%s",
                           c.state->subtree_root.c_str());
    if (!c.state->search.empty())
        n += std::snprintf(buf + n, sizeof buf - n, " | /%s",
                           c.state->search.c_str());
    if (!c.state->flag_filter.empty())
        n += std::snprintf(buf + n, sizeof buf - n, " | flags=%s",
                           c.state->flag_filter.c_str());
    if (c.state->ts_after_ns)
        n += std::snprintf(buf + n, sizeof buf - n, " | >=%lld",
                           (long long)c.state->ts_after_ns);
    if (c.state->ts_before_ns)
        n += std::snprintf(buf + n, sizeof buf - n, " | <=%lld",
                           (long long)c.state->ts_before_ns);
    if (c.state->show_pids)
        n += std::snprintf(buf + n, sizeof buf - n, " | pids");
    n += std::snprintf(buf + n, sizeof buf - n,
        " | 1..7 mode  t tree  T htop  s subtree  </> time  / glob  f flags  ? help");
    (void)n;
    c.tui->set_status(buf);
}

/* Set cursor_id to the first user-row in the lpane when nothing is
 * selected yet (so the right pane is populated on startup and after
 * mode switches, instead of showing "(select a row)" until you nudge
 * the cursor down).  Skips synthetic rows with ids starting with "__"
 * (e.g. "__hint", "__empty"). */
void seed_cursor_if_unset(UiCtx &c) {
    if (!c.state->cursor_id.empty()) return;
    for (int i = 0; i < 4096; i++) {
        const RowData *r = c.tui->get_cached_row(c.lpane, i);
        if (!r) break;
        if (r->id.size() >= 2 && r->id[0] == '_' && r->id[1] == '_') continue;
        c.state->cursor_id = r->id;
        c.tui->set_cursor_idx(c.lpane, i);
        c.src->invalidate();
        sync_hats(c);
        c.tui->dirty(c.rpane);
        update_status(c);
        return;
    }
}

int on_key_cb(Tui &tui, int key, int panel, int /*cursor*/, const char *row_id,
              UiCtx &c) {
    auto invalidate_and_redraw = [&]() {
        c.src->invalidate();
        sync_hats(c);
        tui.dirty();
        update_status(c);
    };
    /* Right after every key (and at idle ticks) make sure cur is set -
     * the lpane has just been (re)built and we know its first row. */
    if (key == TUI_K_NONE) {
        seed_cursor_if_unset(c);
        return TUI_DEFAULT;
    }
    if (key >= '0' && key <= '7') {
        int new_mode = key - '0';
        /* When entering a dep mode (4..7) from the file view (mode 2),
         * pin the current cursor as the subject file so the closure
         * traversal has an anchor. From mode 1, the cursor is a tgid
         * and isn't a file - the user should pick a file first. */
        if (new_mode >= 4 && new_mode <= 7) {
            if (c.state->mode == 2 && !c.state->cursor_id.empty())
                c.state->subject_file = c.state->cursor_id;
        }
        c.state->mode = new_mode;
        c.state->cursor_id.clear();
        c.have_pending = false;          /* mode switch invalidates pending */
        c.pending_cursor_id.clear();
        /* Push the new mode's column layout (and column-header title)
         * to the engine before the next render. */
        c.src->apply_layout(tui, c.lpane, c.rpane);
        invalidate_and_redraw();
        return TUI_HANDLED;
    }
    if (key == 'q') { tui.quit(); return TUI_HANDLED; }
    if (key == TUI_K_ESC) {
        /* Esc clears every active filter, layered. */
        if (!c.state->search.empty()) {
            c.state->search.clear();
            invalidate_and_redraw();
            return TUI_HANDLED;
        }
        if (!c.state->flag_filter.empty()) {
            c.state->flag_filter.clear();
            invalidate_and_redraw();
            return TUI_HANDLED;
        }
        if (c.state->ts_after_ns || c.state->ts_before_ns) {
            c.state->ts_after_ns = c.state->ts_before_ns = 0;
            invalidate_and_redraw();
            return TUI_HANDLED;
        }
        if (c.state->subtree_only) {
            c.state->subtree_only = false;
            c.state->subtree_root.clear();
            invalidate_and_redraw();
            return TUI_HANDLED;
        }
        tui.quit();
        return TUI_HANDLED;
    }
    if (key == '?') {
        tui.show_help(HELP_LINES);
        return TUI_HANDLED;
    }
    if (key == '/') {
        char buf[128] = {};
        if (tui.line_edit("glob/text: ", buf, sizeof buf) >= 0) {
            c.state->search = buf;
            invalidate_and_redraw();
        }
        return TUI_HANDLED;
    }
    if (key == 'f') {
        char buf[64] = {};
        /* Grammar: bare letters or `+L` require, `-L` forbid;
         * juxtaposition = AND inside a group, ',' = OR between groups.
         * Examples: "+W-E" rows that write but don't error;
         *           "+W,+R" rows that write OR rows that read. */
        const char *prompt =
            (c.state->mode == 1) ? "proc flags +/- (K=killed F=fail-with-write D=derived-exec; ',' = OR): " :
            (c.state->mode == 2) ? "file flags +/- (R/W/E + w/f/s/k; ',' = OR): " :
            (c.state->mode == 0) ? "output flags +/- (O=stdout E=stderr; ',' = OR): " :
                                   "flags +/- (',' = OR): ";
        if (tui.line_edit(prompt, buf, sizeof buf) >= 0) {
            c.state->flag_filter = buf;
            invalidate_and_redraw();
        }
        return TUI_HANDLED;
    }
    /* Tree/flat toggle (modes 1, 2). */
    if (key == 't') {
        c.state->grouped = !c.state->grouped;
        invalidate_and_redraw();
        return TUI_HANDLED;
    }
    /* Toggle the htop-style snapshot column (third column).  When
     * turned on, the column shows the process tree as it stood at
     * the cursor's event timestamp - green = just spawned, red = just
     * died, the producer of the selected event highlighted as the
     * focus.  Refreshes whenever the cursor commits.  No-op (but
     * still toggles state) in modes that have no per-row timestamp;
     * the column will just show "(no anchor)". */
    if (key == 'T') {
        c.state->show_htop_col = !c.state->show_htop_col;
        apply_htop_visibility(c);
        if (c.state->show_htop_col) refresh_htop_anchor(c);
        tui.dirty();
        update_status(c);
        return TUI_HANDLED;
    }
    /* Show/hide pid prefix in mode 1. */
    if (key == 'p') {
        c.state->show_pids = !c.state->show_pids;
        invalidate_and_redraw();
        return TUI_HANDLED;
    }
    /* Subtree-only toggle in mode 1: pin current cursor as root. */
    if (key == 's' && c.state->mode == 1) {
        if (c.state->subtree_only) {
            c.state->subtree_only = false;
            c.state->subtree_root.clear();
        } else if (!c.state->cursor_id.empty()) {
            c.state->subtree_only = true;
            c.state->subtree_root = c.state->cursor_id;
        }
        invalidate_and_redraw();
        return TUI_HANDLED;
    }
    /* Time cutoffs: '<' = "before", '>' = "after".
     *   first press  : seed cutoff from the cursor's timestamp
     *   second press : clear the cutoff
     * Companion to the subtree filter - lets the user slice a long
     * trace down to a window without losing search/flag state. */
    if (key == '<' || key == '>') {
        bool is_before = (key == '<');
        int64_t &slot = is_before ? c.state->ts_before_ns
                                  : c.state->ts_after_ns;
        if (slot != 0) {
            slot = 0;
            invalidate_and_redraw();
            return TUI_HANDLED;
        }
        /* Look up cursor timestamp in a mode-appropriate way. */
        int64_t ts = 0;
        const std::string &cur = c.state->cursor_id;
        if (!cur.empty()) {
            if (c.state->mode == 0 || c.state->mode == 3) {
                /* cursor id is "tgid:ts_ns[:k]". */
                auto p1 = cur.find(':');
                if (p1 != std::string::npos) {
                    auto p2 = cur.find(':', p1 + 1);
                    std::string ts_s = (p2 == std::string::npos)
                        ? cur.substr(p1 + 1)
                        : cur.substr(p1 + 1, p2 - p1 - 1);
                    try { ts = std::stoll(ts_s); } catch (...) {}
                }
            } else if (c.state->mode == 1 && c.db) {
                std::string e;
                auto rows = c.db->query_strings(
                    "SELECT start_ns FROM tv_idx_proc WHERE tgid = " +
                    cur + " LIMIT 1", &e);
                if (!rows.empty() && !rows[0][0].empty())
                    try { ts = std::stoll(rows[0][0]); } catch (...) {}
            } else if (c.state->mode == 2 && c.db) {
                /* path needs literal-quoting; do it inline. */
                std::string esc = "'";
                for (char ch : cur) {
                    if (ch == '\'') esc += "''"; else esc += ch;
                }
                esc += "'";
                std::string e;
                auto rows = c.db->query_strings(
                    "SELECT first_ns FROM tv_idx_path WHERE path = " +
                    esc + " LIMIT 1", &e);
                if (!rows.empty() && !rows[0][0].empty())
                    try { ts = std::stoll(rows[0][0]); } catch (...) {}
            }
        }
        if (ts > 0) {
            slot = ts;
            invalidate_and_redraw();
        }
        return TUI_HANDLED;
    }
    /* Tab on the rpane: collapse/expand the section the cursor is on. */
    if (key == TUI_K_TAB && panel == c.rpane && row_id) {
        std::string id = row_id;
        /* Find the heading the cursor sits on or under. */
        int cur = tui.get_cursor(c.rpane);
        std::string heading_id;
        for (int i = cur; i >= 0; i--) {
            const RowData *r = tui.get_cached_row(c.rpane, i);
            if (!r) break;
            if (r->id.size() >= 2 && r->id[0] == '_' && r->id[1] == '_'
                && r->style == RowStyle::Heading) {
                heading_id = r->id;
                break;
            }
        }
        if (!heading_id.empty()) {
            auto &v = c.state->collapsed_sections;
            auto it = std::find(v.begin(), v.end(), heading_id);
            if (it == v.end()) v.push_back(heading_id);
            else v.erase(it);
            c.src->invalidate();
            sync_hats(c);
            tui.dirty(c.rpane);
        }
        return TUI_HANDLED;
    }
    /* Enter on rpane row -> navigate using link metadata. */
    if (key == TUI_K_ENTER && panel == c.rpane && row_id) {
        int cur = tui.get_cursor(c.rpane);
        const RowData *r = tui.get_cached_row(c.rpane, cur);
        if (r && r->link_mode > 0 && !r->link_id.empty()) {
            int target_mode = r->link_mode;
            std::string target_id = r->link_id;
            c.state->mode = target_mode;
            c.state->cursor_id = target_id;
            c.src->apply_layout(tui, c.lpane, c.rpane);
            c.tui->focus(c.lpane);
            invalidate_and_redraw();
            c.tui->set_cursor(c.lpane, target_id.c_str());
        }
        return TUI_HANDLED;
    }
    /* Cursor changes: defer rpane rebuild so fast-scrolling doesn't
     * trigger a SQL query per keystroke.  We just stash the live
     * cursor id; a 100 ms timer (registered in main()) commits it
     * 300 ms after the last cursor change. */
    if (panel == c.lpane && row_id && std::string(row_id) != c.state->cursor_id) {
        c.pending_cursor_id = row_id;
        c.have_pending = true;
        c.last_cursor_change_ms = now_ms();
    }
    return TUI_DEFAULT;
}

/* -- --dump[=MODE] : non-interactive dump of an lpane (for tests) ---- */

int dump_mode(TvDb &db, int mode, const std::string &subject,
              const std::string &flag_filter,
              const std::string &search) {
    AppState st;
    st.mode = mode;
    st.subject_file = subject;
    st.flag_filter = flag_filter;
    st.search = search;
    TvDataSource ts(db, st);
    auto srcfn = ts.make_data_source();
    srcfn.row_begin(0);
    std::string first_real_id;
    while (srcfn.row_has_more(0)) {
        RowData r = srcfn.row_next(0);
        if (first_real_id.empty() && !(r.id.size() >= 2 && r.id[0] == '_' && r.id[1] == '_'))
            first_real_id = r.id;
        std::printf("%s", r.id.c_str());
        for (auto &col : r.cols) std::printf("\t%s", col.c_str());
        std::printf("\n");
    }
    /* Also dump the right pane for the first real row, so tests can
     * verify the info-pane content end-to-end. */
    if (!first_real_id.empty()) {
        st.cursor_id = first_real_id;
        ts.invalidate();
        srcfn.row_begin(1);
        std::printf("--- rpane for %s ---\n", first_real_id.c_str());
        while (srcfn.row_has_more(1)) {
            RowData r = srcfn.row_next(1);
            std::printf("%s", r.id.c_str());
            for (auto &col : r.cols) std::printf("\t%s", col.c_str());
            std::printf("\n");
        }
    }
    return 0;
}

int ingest_main(int argc, char **argv) {
    /* tv ingest <wire-file> [-o OUT.tvdb]
     * Convert a .wire (or .wire.zst) trace to a .tvdb without spawning
     * the TUI. Output defaults to next-to-input with .tvdb suffix. */
    const char *in_path = nullptr;
    const char *out_path = nullptr;
    for (int i = 1; i < argc; i++) {
        if (!std::strcmp(argv[i], "-o") && i + 1 < argc) out_path = argv[++i];
        else if (argv[i][0] == '-') {
            std::fprintf(stderr,
                "usage: tv ingest <wire> [-o OUT.tvdb]\n");
            return 2;
        }
        else if (!in_path) in_path = argv[i];
        else {
            std::fprintf(stderr,
                "tv ingest: unexpected positional arg: %s\n", argv[i]);
            return 2;
        }
    }
    if (!in_path) {
        std::fprintf(stderr, "tv ingest: missing wire file\n"
            "usage: tv ingest <wire> [-o OUT.tvdb]\n");
        return 2;
    }
    std::string out_db = out_path ? std::string(out_path)
                                  : default_tvdb_for_wire(in_path);
    /* If the output is stale (older than wire), rebuild from scratch.
     * Otherwise accept that the existing one is up to date. */
    struct stat st_wire{}, st_db{};
    bool need_build = ::stat(out_db.c_str(), &st_db) != 0;
    if (!need_build && ::stat(in_path, &st_wire) == 0 &&
        st_wire.st_mtime > st_db.st_mtime) need_build = true;
    if (!need_build) {
        std::fprintf(stderr,
            "tv ingest: %s already up to date (use rm + retry to force)\n",
            out_db.c_str());
        return 0;
    }
    ::unlink(out_db.c_str());
    std::string err;
    auto db = TvDb::open_file(out_db, &err);
    if (!db) { std::fprintf(stderr, "tv ingest: %s\n", err.c_str()); return 1; }
    if (!ingest_wire_file(in_path, *db, &err)) {
        std::fprintf(stderr, "tv ingest: %s\n", err.c_str()); return 1;
    }
    if (!db->flush(&err)) {
        std::fprintf(stderr, "tv ingest: flush: %s\n", err.c_str()); return 1;
    }
    std::fprintf(stderr, "tv ingest: %s -> %s\n", in_path, out_db.c_str());
    return 0;
}

} /* namespace */

int main(int argc, char **argv) {
    /* Subcommand dispatch (must be the first positional arg, before any
     * --flag). */
    if (argc >= 2 && argv[1][0] != '-') {
        const char *sub = argv[1];
        if (!std::strcmp(sub, "dump"))
            return wiredump_main(argc - 1, argv + 1);
        if (!std::strcmp(sub, "fv"))
            return fv_main(argc - 1, argv + 1);
        if (!std::strcmp(sub, "test"))
            return run_tests();
        if (!std::strcmp(sub, "ingest"))
            return ingest_main(argc - 1, argv + 1);
        std::fprintf(stderr, "tv: unknown subcommand: %s\n\n", sub);
        std::fputs(USAGE, stderr);
        return 1;
    }

    if (argc >= 2 && std::strcmp(argv[1], "--test") == 0)
        return run_tests();

    const char *live_tracer = nullptr;
    int no_env = 0;
    const char *trace_file = nullptr;
    const char *open_file  = nullptr;
    const char *out_db     = nullptr;
    bool dump = false;
    int  dump_mode_n = 1;
    std::string dump_subject;
    std::string dump_flags;
    std::string dump_search;
    char **cmd = nullptr;

    for (int i = 1; i < argc; i++) {
        if      (!std::strcmp(argv[i], "--trace") && i + 1 < argc) trace_file = argv[++i];
        else if (!std::strcmp(argv[i], "--open")  && i + 1 < argc) open_file  = argv[++i];
        else if (!std::strcmp(argv[i], "-o")      && i + 1 < argc) out_db     = argv[++i];
        else if (!std::strcmp(argv[i], "--dump")) dump = true;
        else if (!std::strncmp(argv[i], "--dump=", 7)) {
            dump = true;
            dump_mode_n = std::atoi(argv[i] + 7);
            if (dump_mode_n < 0 || dump_mode_n > 7) dump_mode_n = 1;
        }
        else if (!std::strcmp(argv[i], "--subject") && i + 1 < argc)
            dump_subject = argv[++i];
        else if (!std::strncmp(argv[i], "--flags=", 8))
            dump_flags = argv[i] + 8;
        else if (!std::strncmp(argv[i], "--search=", 9))
            dump_search = argv[i] + 9;
        else if (!std::strcmp(argv[i], "--no-env")) no_env = 1;
        else if (!std::strcmp(argv[i], "--tracer") && i + 1 < argc) {
            live_tracer = argv[++i];
        }
        else if (!std::strcmp(argv[i], "--") && i + 1 < argc) { cmd = argv + i + 1; break; }
        else {
            std::fprintf(stderr, "tv: unrecognised arg: %s\n", argv[i]);
            std::fputs(USAGE, stderr);
            return 1;
        }
    }

    if (!trace_file && !open_file && !cmd) {
        std::fputs(USAGE, stderr);
        return 1;
    }
    if (cmd && !live_tracer) {
        std::fprintf(stderr,
            "tv: live recording requires --tracer EXE (e.g. --tracer upttrace)\n");
        return 1;
    }

    /* Decide backing .tvdb path. */
    std::string db_path;
    if (open_file) {
        db_path = open_file;
    } else if (trace_file) {
        db_path = out_db ? out_db : default_tvdb_for_wire(trace_file);
        /* If the .tvdb is older than the wire file or absent, rebuild. */
        struct stat st_wire{}, st_db{};
        bool need_build = ::stat(db_path.c_str(), &st_db) != 0;
        if (!need_build && ::stat(trace_file, &st_wire) == 0 &&
            st_wire.st_mtime > st_db.st_mtime) need_build = true;
        if (need_build) {
            ::unlink(db_path.c_str());
            std::string err;
            auto db = TvDb::open_file(db_path, &err);
            if (!db) { std::fprintf(stderr, "tv: %s\n", err.c_str()); return 1; }
            if (!ingest_wire_file(trace_file, *db, &err)) {
                std::fprintf(stderr, "tv: %s\n", err.c_str()); return 1;
            }
            if (!db->flush(&err)) {
                std::fprintf(stderr, "tv: flush: %s\n", err.c_str()); return 1;
            }
        }
    } else {
        /* Live: write to a tempfile next to cwd. */
        if (out_db) db_path = out_db;
        else {
            char tmp[256];
            std::snprintf(tmp, sizeof tmp, "tv-live-%d.tvdb", (int)::getpid());
            db_path = tmp;
        }
        ::unlink(db_path.c_str());
    }

    std::string err;
    auto db = TvDb::open_file(db_path, &err);
    if (!db) { std::fprintf(stderr, "tv: %s\n", err.c_str()); return 1; }

    /* Live-trace child setup. */
    LiveTrace lt;
    lt.db = db.get();
    if (cmd) {
        int pipefd[2];
        if (pipe(pipefd) < 0) { std::perror("pipe"); return 1; }
        lt.child_pid = ::fork();
        if (lt.child_pid < 0) { std::perror("fork"); return 1; }
        if (lt.child_pid == 0) {
            /* Child: exec the user-chosen tracer. The tracer writes its
             * trace bytes to stdout, which we've redirected into our
             * pipe; tv reads & ingests them on the parent side. */
            ::close(pipefd[0]);
            if (::dup2(pipefd[1], STDOUT_FILENO) < 0) _exit(127);
            ::close(pipefd[1]);
            size_t cmdc = 0; while (cmd[cmdc]) cmdc++;
            /* tracer + [--no-env] + -- + cmd... + NULL */
            size_t extra = 1 + (no_env ? 1 : 0) + 1 + cmdc + 1;
            char **uargv = (char **)std::calloc(extra, sizeof(char*));
            size_t ui = 0;
            uargv[ui++] = (char*)live_tracer;
            if (no_env) uargv[ui++] = (char*)"--no-env";
            uargv[ui++] = (char*)"--";
            for (size_t j = 0; j < cmdc; j++) uargv[ui++] = cmd[j];
            uargv[ui] = nullptr;
            if (std::strchr(live_tracer, '/'))
                ::execv(live_tracer, uargv);
            else
                ::execvp(live_tracer, uargv);
            std::perror("tv: exec tracer");
            _exit(127);
        }
        ::close(pipefd[1]);
        lt.fd = pipefd[0];
    }

    /* If we just wanted a non-interactive dump, do it and exit. */
    if (dump) {
        if (cmd) { /* drain the live pipe first */
            char buf[64 * 1024];
            TraceDecoder dec([&](const TraceEvent &ev) {
                std::string e; (void)db->append(ev, &e);
            });
            while (true) {
                ssize_t n = ::read(lt.fd, buf, sizeof buf);
                if (n <= 0) break;
                if (!dec.feed(buf, n)) break;
            }
            ::close(lt.fd);
            ::waitpid(lt.child_pid, nullptr, 0);
            db->flush(&err);
        }
        return dump_mode(*db, dump_mode_n, dump_subject, dump_flags, dump_search);
    }

    /* TUI. */
    AppState state;
    state.mode = 1;
    TvDataSource src(*db, state);
    UiCtx ui;
    ui.state = &state;
    ui.src = &src;
    ui.db = db.get();

    bool headless = !::isatty(STDIN_FILENO) || !::isatty(STDOUT_FILENO);
    auto tui = headless ? Tui::open_headless(src.make_data_source(), 24, 80)
                        : Tui::open(src.make_data_source());
    if (!tui) {
        std::fprintf(stderr, "tv: cannot open terminal\n");
        return 1;
    }
    ui.tui = tui.get();

    /* lpane columns:
     *   0: tree-prefix + name + tail   (flex, ellipsised on overflow)
     *   1: flag badge (e.g. "RWE")     (fixed 4)
     *   2: stats summary               (fixed 26, right-aligned)
     * Modes that don't use cols 1/2 just emit empty strings. */
    static const ColDef lpane_cols[] = {
        {-1, TUI_ALIGN_LEFT,  TUI_OVERFLOW_ELLIPSIS},
        { 4, TUI_ALIGN_LEFT,  TUI_OVERFLOW_TRUNCATE},
        {26, TUI_ALIGN_RIGHT, TUI_OVERFLOW_TRUNCATE},
    };
    static const PanelDef lpane_def = {nullptr, lpane_cols, 3, TUI_PANEL_CURSOR};

    /* rpane columns: key (fixed width) | value (flex).  The engine
     * does the alignment, no fixed-width prefixing in app code. */
    static const ColDef rpane_cols[] = {
        {18, TUI_ALIGN_LEFT, TUI_OVERFLOW_TRUNCATE},
        {-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS},
    };
    static const PanelDef rpane_def = {nullptr, rpane_cols, 2,
                                       TUI_PANEL_CURSOR | TUI_PANEL_BORDER};
    /* Hat panels: single full-width column, no cursor (Tab skips
     * them), no title bar. The data source decides per-mode what
     * goes in each; see TvDataSource::hat_layout(). */
    static const ColDef hat_cols[] = {
        {-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS},
    };
    static const PanelDef hat_def = {nullptr, hat_cols, 1, 0};
    ui.lpane   = tui->add_panel(lpane_def);
    ui.rpane   = tui->add_panel(rpane_def);
    ui.hat_top = tui->add_panel(hat_def);
    ui.hat_bot = tui->add_panel(hat_def);
    /* Htop snapshot column - read-only, no cursor (Tab won't land on
     * it), single full-width column.  The data source supplies its
     * column array and title via apply_htop_layout(). */
    static const ColDef htop_cols[] = {
        {-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS},
    };
    static const PanelDef htop_def = {nullptr, htop_cols, 1, TUI_PANEL_BORDER};
    ui.htop_pane = tui->add_panel(htop_def);
    /* Push the per-mode column layout to the engine immediately so the
     * initial render uses the right shape (mode 1 by default). */
    src.apply_layout(*tui, ui.lpane, ui.rpane);
    src.apply_hat_layout(*tui, ui.hat_top, ui.hat_bot);
    src.apply_htop_layout(*tui, ui.htop_pane);
    /* Layout: hbox( vbox(hat_top, hat_bot, lpane), rpane, htop ).
     *   hat_top / hat_bot are weight=0 — fixed-height. min_size is
     *   updated each render to the data source's reported row count
     *   for that hat (0 → the hat collapses entirely). The list takes
     *   all remaining vertical space (weight=1).
     *   htop is weight=0,min_size=0 by default (collapses to nothing);
     *   `T` toggles it to weight=1,min_size=24. */
    static Box hat_top_box = {TUI_BOX_PANEL, 0, 0, 0, 0, {}};
    static Box hat_bot_box = {TUI_BOX_PANEL, 0, 0, 0, 0, {}};
    static Box lbox        = {TUI_BOX_PANEL, 1, 0, 0, 0, {}};
    static Box lvbox       = {TUI_BOX_VBOX,  1, 0, 0, -1, {&hat_top_box, &hat_bot_box, &lbox}};
    static Box rbox        = {TUI_BOX_PANEL, 1, 0, 0, 0, {}};
    static Box htop_box    = {TUI_BOX_PANEL, 0, 0, 0, 0, {}};
    static Box hbox        = {TUI_BOX_HBOX,  1, 0, 0, -1, {&lvbox, &rbox, &htop_box}};
    hat_top_box.panel = ui.hat_top;
    hat_bot_box.panel = ui.hat_bot;
    lbox.panel = ui.lpane;
    rbox.panel = ui.rpane;
    htop_box.panel = ui.htop_pane;
    ui.hat_top_box = &hat_top_box;
    ui.hat_bot_box = &hat_bot_box;
    ui.htop_box    = &htop_box;
    tui->set_layout(&hbox);

    tui->on_key([&ui](Tui &t, int key, int panel, int cur, const char *id) {
        return on_key_cb(t, key, panel, cur, id, ui);
    });
    update_status(ui);
    sync_hats(ui);
    tui->dirty();

    /* Force the lpane to materialise so we can pick its first row id
     * as the initial cursor. Otherwise the right pane is empty and
     * "cur=" is unset until the user presses a key. */
    {
        DataSource ds = src.make_data_source();
        ds.row_begin(0);
        while (ds.row_has_more(0)) {
            RowData r = ds.row_next(0);
            if (r.id.size() >= 2 && r.id[0] == '_' && r.id[1] == '_') continue;
            state.cursor_id = r.id;
            update_status(ui);
            break;
        }
        src.invalidate();
        sync_hats(ui);
    }

    if (lt.fd >= 0) {
        TraceDecoder dec([&](const TraceEvent &ev) {
            std::string e; (void)db->append(ev, &e);
            src.invalidate();
            sync_hats(ui);
        });
        lt.dec = &dec;
        tui->watch_fd(lt.fd, [&](Tui &t, int fd){ on_trace_fd_cb(t, fd, lt); });
    }

    /* Idle auto-select: every 100 ms check whether the lpane cursor
     * has been stable for >= 300 ms; if so, commit it as state.cursor_id
     * and rebuild the rpane.  Without this the rpane only updated on
     * Enter, which the user found jarring.
     *
     * The same tick also drives the sticky-hat behaviour: if the lpane
     * scroll position changed since last tick, ask the data source to
     * recompute hat content from the rows currently in view (so the
     * proc-tree hat shows the common ancestor of the visible window
     * rather than of the entire trace).  Cheap; pure C++ over a cached
     * map - no SQL. */
    tui->add_timer(100, [&ui](Tui &t) {
        /* Sticky hats: detect lpane scroll change and refresh hat
         * panels from the visible window.  Done first so it runs even
         * during the 300 ms grace window for cursor commits. */
        static int last_scroll = -1;
        static int last_mode   = -1;
        int scroll = t.get_scroll(ui.lpane);
        int mode   = ui.state ? ui.state->mode : -1;
        if (scroll != last_scroll || mode != last_mode) {
            last_scroll = scroll;
            last_mode   = mode;
            /* Estimate viewport height from the lpane Box; box height is
             * resolved each render so we just rely on the data source
             * clamping to the cached row count. ~64 is a safe upper
             * bound for typical terminals; smaller windows just see a
             * common ancestor over fewer rows, which is fine. */
            if (ui.src && ui.src->recompute_hats_for_window(scroll, 64)) {
                if (ui.hat_top_box && ui.hat_bot_box) {
                    int top = ui.src->hat_top_row_count();
                    int bot = ui.src->hat_bot_row_count();
                    if (ui.hat_top_box->min_size != top)
                        ui.hat_top_box->min_size = top;
                    if (ui.hat_bot_box->min_size != bot)
                        ui.hat_bot_box->min_size = bot;
                }
                t.dirty(ui.hat_top);
                t.dirty(ui.hat_bot);
            }
        }

        if (!ui.have_pending) return 1;
        if (now_ms() - ui.last_cursor_change_ms < 300) return 1;
        if (ui.pending_cursor_id == ui.state->cursor_id) {
            ui.have_pending = false;
            return 1;
        }
        ui.state->cursor_id = ui.pending_cursor_id;
        ui.have_pending = false;
        ui.src->invalidate_rpane();
        t.dirty(ui.rpane);
        refresh_htop_anchor(ui);
        update_status(ui);
        return 1;
    });

    tui->run();

    if (lt.fd >= 0) { ::close(lt.fd); lt.fd = -1; }
    if (lt.child_pid > 0) { ::kill(lt.child_pid, SIGTERM); ::waitpid(lt.child_pid, nullptr, 0); }
    return 0;
}
