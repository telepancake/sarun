/*
 * engine.cpp - TUI engine implementation.
 *
 * Implements the Tui class declared in engine.h: terminal raw mode,
 * panel-based rendering with caching, keyboard input, fd watches, timers.
 */

#include "engine.h"

#include <algorithm>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/time.h>
#include <termios.h>
#include <unistd.h>

/* -- Constants ------------------------------------------------------ */

static constexpr int PANEL_NCOLS_MAX  = 32;
static constexpr int MAX_WATCHES      = 16;
static constexpr int MAX_TIMERS       = 16;

/* -- Internal types ------------------------------------------------- */

namespace {

struct Panel {
    PanelDef def{};
    /* Owning copy of the column array - set_panel_columns() may swap in
     * a different layout per app mode, and we don't want to require the
     * caller to keep their ColDef array alive. */
    std::vector<ColDef> cols_owned;
    std::string         title_owned;
    int x = 0, y = 0, w = 0, h = 0;
    int cursor = 0, scroll = 0;
    int row_count = 0;
    bool dirty = true;
    char cursor_id[4096]{};

    /* Lazy cache - rows are read progressively from the data source. */
    std::vector<RowData> rows;
    bool complete = false; /* true when all rows have been read */
    bool iter_open = false; /* true when row_begin has been called for current refresh */
};

struct FdWatch {
    int        fd = -1;
    FdCallback cb;
    bool       active = false;
};

struct Timer {
    int           id = 0;
    int           ms = 0;
    bool          active = false;
    struct timeval fire{};
    TimerCallback cb;
};

} // anon namespace

/* -- Tui::Impl ------------------------------------------------------ */

struct Tui::Impl {
    DataSource source;
    int tty_fd = -1;
    struct termios orig_tios{};
    bool tty_raw = false;
    int term_rows = 24, term_cols = 80;

    std::vector<Panel> panels;
    int focus = -1;
    Box *layout_root = nullptr;

    std::string status;

    KeyCallback key_cb;
    FdWatch watches[MAX_WATCHES]{};
    Timer timers[MAX_TIMERS]{};
    int next_timer_id = 1;
    bool quit_flag = false;

    std::string scr;
};

/* -- Globals for signal handling + atexit ---------------------------- */

static volatile int g_resized = 0;
static Tui::Impl *g_atexit_impl = nullptr;

static void sigwinch_handler(int) { g_resized = 1; }

static void tty_restore(Tui::Impl *m) {
    if (m->tty_raw && m->tty_fd >= 0) {
        tcsetattr(m->tty_fd, TCSAFLUSH, &m->orig_tios);
        (void)write(m->tty_fd, "\x1b[?25h\x1b[?1049l", 14);
        m->tty_raw = false;
    }
    if (m->tty_fd >= 0) { close(m->tty_fd); m->tty_fd = -1; }
}

static void atexit_restore() { if (g_atexit_impl) tty_restore(g_atexit_impl); }

/* -- Screen buffer helpers ------------------------------------------ */

static void sa(Tui::Impl *m, const char *s, int n) {
    m->scr.append(s, static_cast<size_t>(n));
}
static void sp(Tui::Impl *m, const char *s) {
    m->scr.append(s);
}
static void sf(Tui::Impl *m, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));
static void sf(Tui::Impl *m, const char *fmt, ...) {
    char buf[4096];
    va_list a;
    va_start(a, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, a);
    va_end(a);
    if (n > 0) m->scr.append(buf, std::min(static_cast<size_t>(n), sizeof(buf) - 1));
}
static void sflush(Tui::Impl *m) {
    if (!m->scr.empty() && m->tty_fd >= 0)
        (void)write(m->tty_fd, m->scr.data(), m->scr.size());
    m->scr.clear();
}

/* -- Visible length (skip ANSI) ------------------------------------- */

static int visible_len(const char *s) {
    int n = 0;
    while (*s) {
        if (*s == '\x1b') { while (*s && *s != 'm') s++; if (*s) s++; continue; }
        if ((*s & 0xC0) != 0x80) n++;
        s++;
    }
    return n;
}

static void sput_field(Tui::Impl *m, const char *s, int w, int align, int ovf) {
    if (w <= 0) return;
    if (!s) s = "";
    int vl = visible_len(s);
    if (vl <= w) {
        int pad = w - vl, lp = 0, rp = 0;
        if (align == TUI_ALIGN_RIGHT) lp = pad;
        else if (align == TUI_ALIGN_CENTER) { lp = pad / 2; rp = pad - lp; }
        else rp = pad;
        for (int i = 0; i < lp; i++) sp(m, " ");
        while (*s) {
            if (*s == '\x1b') { while (*s && *s != 'm') { sa(m, s, 1); s++; } if (*s) { sa(m, s, 1); s++; } continue; }
            sa(m, s, 1); s++;
        }
        for (int i = 0; i < rp; i++) sp(m, " ");
    } else {
        int lim = (ovf == TUI_OVERFLOW_ELLIPSIS && w >= 3) ? w - 1 : w;
        int p = 0;
        while (*s && p < lim) {
            if (*s == '\x1b') { while (*s && *s != 'm') { sa(m, s, 1); s++; } if (*s) { sa(m, s, 1); s++; } continue; }
            sa(m, s, 1); if ((*s & 0xC0) != 0x80) p++; s++;
        }
        if (ovf == TUI_OVERFLOW_ELLIPSIS && w >= 3 && p < vl) { sp(m, "\xe2\x80\xa6"); p++; }
        while (p < w) { sp(m, " "); p++; }
    }
}

static const char *style_ansi(RowStyle s) {
    switch (s) {
    case RowStyle::Bold:    return "\x1b[1m";
    case RowStyle::Cyan:    return "\x1b[36m";
    case RowStyle::CyanBold:return "\x1b[36;1m";
    case RowStyle::Dim:     return "\x1b[2m";
    case RowStyle::Error:   return "\x1b[31m";
    case RowStyle::Green:   return "\x1b[32m";
    case RowStyle::Heading: return "\x1b[33;1m";
    case RowStyle::Search:  return "\x1b[1;35m";
    case RowStyle::Yellow:  return "\x1b[33m";
    default:                return "\x1b[0m";
    }
}

/* -- Panel helpers -------------------------------------------------- */

static void tty_size(Tui::Impl *m) {
    struct winsize ws;
    if (m->tty_fd >= 0 && ioctl(m->tty_fd, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 0) {
        m->term_rows = ws.ws_row;
        m->term_cols = ws.ws_col;
    }
}


static Panel *pget(Tui::Impl *m, int idx) {
    if (idx < 0 || idx >= static_cast<int>(m->panels.size())) return nullptr;
    return &m->panels[idx];
}

static int pidx(Tui::Impl *m, Panel *p) {
    if (!p) return -1;
    return static_cast<int>(p - m->panels.data());
}

/* Open the data-source iterator for this panel (if not already open). */
static void p_ensure_iter(Tui::Impl *m, Panel *p) {
    if (p->dirty) {
        p->iter_open = false;
        p->rows.clear();
        p->complete = false;
        p->row_count = 0;
        p->dirty = false;
    }
    if (p->iter_open) return;
    p->rows.clear();
    p->complete = false;
    p->row_count = 0;
    if (m->source.row_begin)
        m->source.row_begin(pidx(m, p));
    p->iter_open = true;
}

/* Read from the data source until we have at least `need` rows, or exhausted. */
static void p_ensure_rows(Tui::Impl *m, Panel *p, int need) {
    int pi = pidx(m, p);
    p_ensure_iter(m, p);
    if (p->complete) return;
    while (p->row_count < need &&
           m->source.row_has_more && m->source.row_has_more(pi)) {
        p->rows.push_back(m->source.row_next(pi));
        p->row_count = static_cast<int>(p->rows.size());
    }
    if (!m->source.row_has_more || !m->source.row_has_more(pi)) {
        p->complete = true;
        p->row_count = static_cast<int>(p->rows.size());
    }
}


/* -- Cursor position and scroll clamping -------------------------- */

static void p_clamp(Tui::Impl *m, Panel *p) {
    /* Ensure we have enough rows for the cursor position. */
    if (p->cursor > 0)
        p_ensure_rows(m, p, p->cursor + 1);
    if (p->row_count == 0 && !p->complete)
        p_ensure_rows(m, p, 1);
    if (p->row_count == 0) { p->cursor = 0; p->scroll = 0; return; }
    if (p->cursor >= p->row_count) {
        if (!p->complete) p_ensure_rows(m, p, p->cursor + 1);
        if (p->cursor >= p->row_count) p->cursor = p->row_count - 1;
    }
    if (p->cursor < 0) p->cursor = 0;
    int vh = p->h - (p->def.title ? 1 : 0);
    if (vh < 1) vh = 1;
    if (p->cursor < p->scroll) p->scroll = p->cursor;
    if (p->cursor >= p->scroll + vh) p->scroll = p->cursor - vh + 1;
    if (p->scroll < 0) p->scroll = 0;
}

static void p_sync_id(Tui::Impl *m, Panel *p) {
    p->cursor_id[0] = '\0';
    p_ensure_rows(m, p, p->cursor + 1);
    if (p->cursor >= 0 && p->cursor < p->row_count)
        std::snprintf(p->cursor_id, sizeof p->cursor_id, "%s",
                      p->rows[p->cursor].id.c_str());
}

static void p_resolve_id(Tui::Impl *m, Panel *p) {
    if (!p->cursor_id[0]) { p->cursor = 0; return; }
    int pi = pidx(m, p);
    p_ensure_iter(m, p);
    /* First check already-loaded rows. */
    for (int i = 0; i < p->row_count; i++) {
        if (p->rows[i].id == p->cursor_id) {
            p->cursor = i;
            return;
        }
    }
    /* Incrementally load more rows until found or exhausted. */
    while (!p->complete && m->source.row_has_more && m->source.row_has_more(pi)) {
        p->rows.push_back(m->source.row_next(pi));
        p->row_count = static_cast<int>(p->rows.size());
        if (p->rows.back().id == p->cursor_id) {
            p->cursor = p->row_count - 1;
            return;
        }
    }
    if (!m->source.row_has_more || !m->source.row_has_more(pi))
        p->complete = true;
    /* ID not found - keep current position (will be clamped). */
}

/* -- Box layout ----------------------------------------------------- */

static int box_contains_focus(Tui::Impl *m, Box *b) {
    if (!b) return 0;
    if (b->type == TUI_BOX_PANEL) {
        return b->panel == m->focus;
    }
    for (auto *child : b->children)
        if (box_contains_focus(m, child)) return 1;
    return 0;
}

static void resolve_box(Tui::Impl *m, Box *b, int x, int y, int w, int h) {
    if (!b) return;
    if (b->type == TUI_BOX_PANEL) {
        auto *p = pget(m, b->panel);
        if (p) {
            p->x = x; p->y = y;
            p->w = std::max(w, 1);
            /* Allow h == 0 for panels intentionally hidden by the
             * layout (weight=0, min_size=0 box). render_panel skips
             * those, so they take no screen space. */
            p->h = std::max(h, 0);
        }
        return;
    }
    bool is_hbox = (b->type == TUI_BOX_HBOX);
    int nc = static_cast<int>(b->children.size());

    /* hscroll: visible subset of children, auto-scroll to keep focus visible */
    if (is_hbox && (b->flags & TUI_BOX_HSCROLL) && nc > 0) {
        int min_col = 14;
        int nvis = std::clamp(w / min_col, 1, nc);
        int cw   = w / nvis;
        int focus_child = 0;
        for (int i = 0; i < nc; i++)
            if (box_contains_focus(m, b->children[i])) { focus_child = i; break; }
        int first = std::clamp(focus_child - nvis / 2, 0, std::max(nc - nvis, 0));
        int pos = x;
        for (int i = 0; i < nc; i++) {
            if (i >= first && i < first + nvis) {
                int tw = (i == first + nvis - 1) ? w - (pos - x) : cw;
                resolve_box(m, b->children[i], pos, y, tw, h);
                pos += tw;
            } else {
                resolve_box(m, b->children[i], -9999, y, 1, h);
            }
        }
        return;
    }

    int axis = is_hbox ? w : h;
    int total_weight = 0, fixed_sum = 0;
    for (auto *c : b->children) {
        if (c->weight == 0) fixed_sum += c->min_size;
        else { total_weight += c->weight; if (c->min_size > 0) fixed_sum += c->min_size; }
    }
    int flex = std::max(axis - fixed_sum, 0);
    int pos = is_hbox ? x : y, remaining = axis;
    for (int i = 0; i < nc; i++) {
        auto *c = b->children[i];
        int sz;
        if (c->weight == 0) {
            sz = c->min_size;
        } else {
            sz = c->min_size + (total_weight > 0 ? flex * c->weight / total_weight : 0);
            bool is_last_flex = true;
            for (int j = i + 1; j < nc; j++)
                if (b->children[j]->weight > 0) { is_last_flex = false; break; }
            if (is_last_flex) {
                int tail_fixed = 0;
                for (int j = i + 1; j < nc; j++)
                    if (b->children[j]->weight == 0) tail_fixed += b->children[j]->min_size;
                sz = remaining - tail_fixed;
            }
        }
        if (sz < 0) sz = 0;
        /* Flex children must occupy at least one cell so they remain
         * visible; fixed-size children (weight==0) honour min_size as
         * given, including 0 for layout slots that are currently empty. */
        if (c->weight > 0 && sz < 1) sz = 1;
        if (is_hbox) resolve_box(m, c, pos, y, sz, h);
        else         resolve_box(m, c, x, pos, w, sz);
        pos += sz;
        remaining -= sz;
    }
}

static void resolve_layout(Tui::Impl *m) {
    int ah = m->term_rows - 1, aw = m->term_cols;
    if (m->layout_root) resolve_box(m, m->layout_root, 0, 0, aw, ah);
}

static void resolve_col_widths(const PanelDef *d, int tw, int *out) {
    int fx = 0, fl = 0;
    for (int i = 0; i < d->ncols; i++) {
        if (d->cols[i].width > 0) fx += d->cols[i].width;
        else fl += -d->cols[i].width;
    }
    int rem = std::max(tw - fx, 0), sum = 0;
    for (int i = 0; i < d->ncols; i++) {
        out[i] = d->cols[i].width > 0 ? d->cols[i].width
                                       : (fl > 0 ? rem * (-d->cols[i].width) / fl : 0);
        sum += out[i];
    }
    if (sum < tw)
        for (int i = d->ncols - 1; i >= 0; i--)
            if (d->cols[i].width < 0) { out[i] += tw - sum; break; }
}

/* -- Keyboard input ------------------------------------------------- */

static int read_key(Tui::Impl *m) {
    if (m->tty_fd < 0) return TUI_K_NONE;
    char c;
    if (read(m->tty_fd, &c, 1) <= 0) return TUI_K_NONE;
    if (c == '\x1b') {
        char s[3];
        if (read(m->tty_fd, &s[0], 1) != 1) return TUI_K_ESC;
        if (read(m->tty_fd, &s[1], 1) != 1) return TUI_K_ESC;
        if (s[0] == '[') {
            if (s[1] >= '0' && s[1] <= '9') {
                if (read(m->tty_fd, &s[2], 1) != 1) return TUI_K_ESC;
                if (s[2] == '~') switch (s[1]) {
                    case '1': case '7': return TUI_K_HOME;
                    case '4': case '8': return TUI_K_END;
                    case '5': return TUI_K_PGUP;
                    case '6': return TUI_K_PGDN;
                }
            } else switch (s[1]) {
                case 'A': return TUI_K_UP;
                case 'B': return TUI_K_DOWN;
                case 'C': return TUI_K_RIGHT;
                case 'D': return TUI_K_LEFT;
                case 'H': return TUI_K_HOME;
                case 'F': return TUI_K_END;
            }
        } else if (s[0] == 'O') switch (s[1]) {
            case 'H': return TUI_K_HOME;
            case 'F': return TUI_K_END;
        }
        return TUI_K_ESC;
    }
    return static_cast<unsigned char>(c);
}

/* -- Rendering ------------------------------------------------------ */

static void render_panel(Tui::Impl *m, Panel *p) {
    if (p->x < -9000) return;
    if (p->h <= 0) return;          /* hidden layout slot — nothing to draw */
    const auto *d = &p->def;
    bool focused = (m->focus >= 0 && &m->panels[m->focus] == p);
    int cy = p->y, ch = p->h;
    if (d->title) {
        sf(m, "\x1b[%d;%dH", cy + 1, p->x + 1);
        /* Active-panel indicator: bright magenta on white when focused,
         * dim grey when not, plus a leading '>' marker so it's obvious
         * even on terminals where colour is washed out. */
        sp(m, focused ? "\x1b[1;45;97m" : "\x1b[2;7m");
        char hdr[512];
        std::snprintf(hdr, sizeof hdr, " %s %s ",
                      focused ? ">" : " ", d->title);
        sput_field(m, hdr, p->w, TUI_ALIGN_LEFT, TUI_OVERFLOW_TRUNCATE);
        sp(m, "\x1b[0m");
        cy++; ch--;
    }
    if (ch <= 0) return;
    int cw[32], pw = p->w;
    if (d->flags & TUI_PANEL_BORDER) pw--;
    resolve_col_widths(d, pw, cw);
    int nc = std::min(d->ncols, PANEL_NCOLS_MAX);
    int row = 0;
    int rn = p->scroll;
    for (; row < ch; rn++, row++) {
        int sr = cy + row + 1, sc = p->x + 1;
        if (d->flags & TUI_PANEL_BORDER) sc++;
        sf(m, "\x1b[%d;%dH", sr, sc);
        bool has_row = (rn >= 0 && rn < p->row_count);
        if (!has_row) {
            sp(m, "\x1b[0m");
            for (int c = 0; c < pw; c++) sp(m, " ");
            continue;
        }
        auto &r = p->rows[rn];
        bool is_cur = (d->flags & TUI_PANEL_CURSOR) && rn == p->cursor;
        if (is_cur && focused) sp(m, "\x1b[1;7m");
        else if (is_cur) sp(m, "\x1b[7m");
        else sp(m, style_ansi(r.style));
        for (int c = 0; c < nc; c++) {
            const char *cell = (c < static_cast<int>(r.cols.size())) ? r.cols[c].c_str() : "";
            sput_field(m, cell, cw[c], d->cols[c].align, d->cols[c].overflow);
        }
        sp(m, "\x1b[0m");
    }
    if (d->flags & TUI_PANEL_BORDER) {
        for (int r = 0; r < p->h; r++)
            sf(m, "\x1b[%d;%dH\x1b[0m\xe2\x94\x82", p->y + r + 1, p->x + 1);
    }
}

static void render_status(Tui::Impl *m) {
    sf(m, "\x1b[%d;1H\x1b[7;1m", m->term_rows);
    sput_field(m, m->status.c_str(), m->term_cols, TUI_ALIGN_LEFT, TUI_OVERFLOW_TRUNCATE);
    sp(m, "\x1b[0m");
}

static void render_all(Tui::Impl *m) {
    resolve_layout(m);
    m->scr.clear();
    sp(m, "\x1b[H");
    for (auto &p : m->panels) {
        if (p.dirty) {
            p.iter_open = false; /* reset iterator for fresh data */
            p.rows.clear();
            p.complete = false;
            p.row_count = 0;
            p_resolve_id(m, &p);
            p_clamp(m, &p);
            p_sync_id(m, &p);
            p.dirty = false;
        }
        /* Ensure enough rows are cached for the visible viewport. */
        int vh = p.h - (p.def.title ? 1 : 0);
        if (vh < 1) vh = 1;
        p_ensure_rows(m, &p, p.scroll + vh);
        render_panel(m, &p);
    }
    render_status(m);
    sflush(m);
}

/* -- Navigation ----------------------------------------------------- */

static bool is_engine_nav_key(int k) {
    switch (k) {
    case TUI_K_UP: case TUI_K_DOWN:
    case TUI_K_PGUP: case TUI_K_PGDN:
    case TUI_K_HOME: case TUI_K_END:
    case TUI_K_TAB:
    case 'j': case 'k': case 'g':
        return true;
    default:
        return false;
    }
}

static void default_nav(Tui::Impl *m, int k) {
    int npanels = static_cast<int>(m->panels.size());
    if (k == TUI_K_TAB) {
        if (npanels <= 0) return;
        int start = m->focus < 0 ? 0 : m->focus;
        for (int off = 1; off <= npanels; off++) {
            int idx = (start + off) % npanels;
            if (m->panels[idx].def.flags & TUI_PANEL_CURSOR) {
                m->focus = idx;
                auto &pp = m->panels[idx];
                if (pp.dirty) {
                    pp.iter_open = false;
                    pp.rows.clear();
                    pp.complete = false;
                    pp.row_count = 0;
                    p_resolve_id(m, &pp);
                    p_clamp(m, &pp);
                    p_sync_id(m, &pp);
                    pp.dirty = false;
                }
                return;
            }
        }
        return;
    }
    if (m->focus < 0 || m->focus >= npanels) return;
    auto &p = m->panels[m->focus];
    if (!(p.def.flags & TUI_PANEL_CURSOR)) return;
    int vh = p.h - (p.def.title ? 1 : 0);
    int pg = vh > 2 ? vh - 1 : 1;
    switch (k) {
    case TUI_K_UP:   case 'k': p.cursor--; break;
    case TUI_K_DOWN: case 'j': p.cursor++; break;
    case TUI_K_PGUP:           p.cursor -= pg; break;
    case TUI_K_PGDN:           p.cursor += pg; break;
    case TUI_K_HOME: case 'g': p.cursor = 0; break;
    case TUI_K_END:            /* Read all remaining rows inline to find last. */
                               { int pi = pidx(m, &p);
                                 p_ensure_iter(m, &p);
                                 while (!p.complete && m->source.row_has_more &&
                                        m->source.row_has_more(pi)) {
                                     p.rows.push_back(m->source.row_next(pi));
                                     p.row_count = static_cast<int>(p.rows.size());
                                 }
                                 p.complete = true;
                                 p.cursor = p.row_count > 0 ? p.row_count - 1 : 0;
                               } break;
    default: return;
    }
    p_clamp(m, &p);
    p_sync_id(m, &p);
}

/* fire TUI_K_NONE to notify app that navigation happened */
static void fire_key_none(Tui &tui, Tui::Impl *m) {
    if (!m->key_cb) return;
    int fp = -1;
    const char *fid = "";
    int fc = 0;
    int npanels = static_cast<int>(m->panels.size());
    if (m->focus >= 0 && m->focus < npanels) {
        auto &f = m->panels[m->focus];
        fp = m->focus; fc = f.cursor; fid = f.cursor_id;
    }
    m->key_cb(tui, TUI_K_NONE, fp, fc, fid);
}

/* ==================================================================
 * Public Tui implementation
 * ================================================================== */

Tui::Tui() : impl_(std::make_unique<Impl>()) {}
Tui::~Tui() {
    if (impl_) {
        tty_restore(impl_.get());
        if (g_atexit_impl == impl_.get()) g_atexit_impl = nullptr;
    }
}

std::unique_ptr<Tui> Tui::open(DataSource src) {
    auto t = std::unique_ptr<Tui>(new Tui());
    auto *m = t->impl_.get();
    m->source = std::move(src);
    m->tty_fd = ::open("/dev/tty", O_RDWR);
    if (m->tty_fd < 0) return nullptr;
    tcgetattr(m->tty_fd, &m->orig_tios);
    g_atexit_impl = m;
    std::atexit(atexit_restore);
    struct termios r = m->orig_tios;
    r.c_iflag &= ~static_cast<unsigned>(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
    r.c_oflag &= ~static_cast<unsigned>(OPOST);
    r.c_cflag |= CS8;
    r.c_lflag &= ~static_cast<unsigned>(ECHO | ICANON | IEXTEN | ISIG);
    r.c_cc[VMIN] = 0; r.c_cc[VTIME] = 1;
    tcsetattr(m->tty_fd, TCSAFLUSH, &r);
    m->tty_raw = true;
    (void)write(m->tty_fd, "\x1b[?1049h\x1b[?25l", 14);
    tty_size(m);
    struct sigaction sa{};
    sa.sa_handler = sigwinch_handler;
    sigaction(SIGWINCH, &sa, nullptr);
    return t;
}

std::unique_ptr<Tui> Tui::open_headless(DataSource src, int rows, int cols) {
    auto t = std::unique_ptr<Tui>(new Tui());
    auto *m = t->impl_.get();
    m->source = std::move(src);
    m->term_rows = rows > 0 ? rows : 24;
    m->term_cols = cols > 0 ? cols : 80;
    return t;
}

int Tui::add_panel(PanelDef def) {
    auto *m = impl_.get();
    int idx = static_cast<int>(m->panels.size());
    m->panels.emplace_back();
    auto &p = m->panels.back();
    p.def   = def;
    /* Copy the caller's column array so they don't need to keep it
     * alive, and so set_panel_columns() can later swap it. */
    if (def.cols && def.ncols > 0) {
        p.cols_owned.assign(def.cols, def.cols + def.ncols);
        p.def.cols = p.cols_owned.data();
    }
    if (def.title) {
        p.title_owned = def.title;
        p.def.title   = p.title_owned.c_str();
    }
    p.dirty = true;
    if (m->focus < 0 && (def.flags & TUI_PANEL_CURSOR))
        m->focus = idx;
    return idx;
}

void Tui::set_panel_columns(int panel, const ColDef *cols, int ncols,
                            const char *title) {
    auto *m = impl_.get();
    auto *p = pget(m, panel);
    if (!p) return;
    if (cols && ncols > 0) {
        p->cols_owned.assign(cols, cols + ncols);
        p->def.cols  = p->cols_owned.data();
        p->def.ncols = ncols;
    } else {
        p->cols_owned.clear();
        p->def.cols  = nullptr;
        p->def.ncols = 0;
    }
    if (title) {
        p->title_owned = title;
        p->def.title   = p->title_owned.c_str();
    }
    p->dirty = true;
}

int Tui::panel_count() const {
    return static_cast<int>(impl_->panels.size());
}

void Tui::set_layout(Box *root) {
    impl_->layout_root = root;
}

void Tui::dirty(int panel) {
    auto *m = impl_.get();
    if (panel == TUI_ALL_PANELS) { for (auto &p : m->panels) p.dirty = true; return; }
    auto *p = pget(m, panel);
    if (p) p->dirty = true;
}

void Tui::focus(int panel) {
    if (panel >= 0 && panel < static_cast<int>(impl_->panels.size()))
        impl_->focus = panel;
}

int Tui::get_focus() const {
    return impl_->focus;
}

void Tui::set_cursor(int panel, const char *id) {
    auto *m = impl_.get();
    auto *p = pget(m, panel);
    if (!p) return;
    if (!id) { p->cursor = 0; p->cursor_id[0] = '\0'; return; }
    std::snprintf(p->cursor_id, sizeof p->cursor_id, "%s", id);
    if (p->dirty) {
        p->iter_open = false;
        p->rows.clear();
        p->complete = false;
        p->row_count = 0;
        p->dirty = false;
    }
    p_resolve_id(m, p);
    p_clamp(m, p);
}

void Tui::set_cursor_idx(int panel, int idx) {
    auto *m = impl_.get();
    auto *p = pget(m, panel);
    if (!p) return;
    if (p->dirty) {
        p->iter_open = false;
        p->rows.clear();
        p->complete = false;
        p->row_count = 0;
        p->dirty = false;
    }
    p->cursor = idx;
    p_ensure_rows(m, p, idx + 1);
    p_clamp(m, p);
    p_sync_id(m, p);
}

int Tui::get_cursor(int panel) const {
    auto *p = pget(impl_.get(), panel);
    return p ? p->cursor : -1;
}

int Tui::get_scroll(int panel) const {
    auto *p = pget(impl_.get(), panel);
    return p ? p->scroll : 0;
}

const char *Tui::get_cursor_id(int panel) const {
    auto *p = pget(impl_.get(), panel);
    return (p && p->cursor_id[0]) ? p->cursor_id : "";
}

const RowData *Tui::get_cached_row(int panel, int idx) {
    auto *m = impl_.get();
    auto *p = pget(m, panel);
    if (!p || idx < 0) return nullptr;
    p_ensure_rows(m, p, idx + 1);
    if (idx >= p->row_count) return nullptr;
    return &p->rows[idx];
}

void Tui::on_key(KeyCallback cb) { impl_->key_cb = std::move(cb); }

void Tui::watch_fd(int fd, FdCallback cb) {
    for (auto &w : impl_->watches)
        if (!w.active) { w = {fd, std::move(cb), true}; return; }
}

void Tui::unwatch_fd(int fd) {
    for (auto &w : impl_->watches)
        if (w.active && w.fd == fd) { w.active = false; return; }
}

int Tui::add_timer(int ms, TimerCallback cb) {
    auto *m = impl_.get();
    for (auto &tm : m->timers) {
        if (!tm.active) {
            int id = m->next_timer_id++;
            struct timeval now;
            gettimeofday(&now, nullptr);
            tm.id = id;
            tm.ms = ms;
            tm.fire.tv_sec  = now.tv_sec  + ms / 1000;
            tm.fire.tv_usec = now.tv_usec + (ms % 1000) * 1000;
            if (tm.fire.tv_usec >= 1000000) { tm.fire.tv_sec++; tm.fire.tv_usec -= 1000000; }
            tm.cb = std::move(cb);
            tm.active = true;
            return id;
        }
    }
    return -1;
}

void Tui::remove_timer(int timer_id) {
    for (auto &tm : impl_->timers)
        if (tm.active && tm.id == timer_id) { tm.active = false; return; }
}

void Tui::quit() { impl_->quit_flag = true; }

void Tui::run() {
    auto *m = impl_.get();
    m->quit_flag = false;
    while (!m->quit_flag) {
        if (g_resized) {
            g_resized = 0;
            tty_size(m);
            for (auto &p : m->panels) p.dirty = true;
        }
        render_all(m);

        fd_set rfds;
        FD_ZERO(&rfds);
        int mfd = -1;
        if (m->tty_fd >= 0) { FD_SET(m->tty_fd, &rfds); mfd = m->tty_fd; }
        for (auto &w : m->watches)
            if (w.active) { FD_SET(w.fd, &rfds); if (w.fd > mfd) mfd = w.fd; }

        struct timeval tv{}, *tvp = nullptr;
        {
            struct timeval now;
            gettimeofday(&now, nullptr);
            long min_ms = -1;
            for (auto &tm : m->timers) {
                if (!tm.active) continue;
                long ms = (tm.fire.tv_sec - now.tv_sec) * 1000 +
                          (tm.fire.tv_usec - now.tv_usec) / 1000;
                if (ms < 0) ms = 0;
                if (min_ms < 0 || ms < min_ms) min_ms = ms;
            }
            if (min_ms >= 0) {
                tv.tv_sec  = min_ms / 1000;
                tv.tv_usec = (min_ms % 1000) * 1000;
                tvp = &tv;
            }
        }

        int sel = select(mfd + 1, &rfds, nullptr, nullptr, tvp);
        if (sel < 0 && errno == EINTR) continue;

        /* fire expired timers */
        {
            struct timeval now;
            gettimeofday(&now, nullptr);
            for (auto &tm : m->timers) {
                if (!tm.active) continue;
                if (now.tv_sec > tm.fire.tv_sec ||
                    (now.tv_sec == tm.fire.tv_sec && now.tv_usec >= tm.fire.tv_usec)) {
                    int ms2 = tm.ms;
                    int re = tm.cb(*this);
                    if (re && tm.active) {
                        tm.fire.tv_sec  = now.tv_sec  + ms2 / 1000;
                        tm.fire.tv_usec = now.tv_usec + (ms2 % 1000) * 1000;
                        if (tm.fire.tv_usec >= 1000000) { tm.fire.tv_sec++; tm.fire.tv_usec -= 1000000; }
                    } else {
                        tm.active = false;
                    }
                }
            }
        }
        if (m->quit_flag) break;

        /* fire fd watches */
        if (sel > 0)
            for (auto &w : m->watches)
                if (w.active && FD_ISSET(w.fd, &rfds))
                    w.cb(*this, w.fd);
        if (m->quit_flag) break;

        /* keyboard input */
        if (sel > 0 && m->tty_fd >= 0 && FD_ISSET(m->tty_fd, &rfds)) {
            int k = read_key(m);
            if (k != TUI_K_NONE) {
                if (is_engine_nav_key(k)) {
                    default_nav(m, k);
                    fire_key_none(*this, m);
                    continue;
                }
                int fp = -1;
                const char *fid = "";
                int fc = 0;
                int npanels = static_cast<int>(m->panels.size());
                if (m->focus >= 0 && m->focus < npanels) {
                    auto &f = m->panels[m->focus];
                    fp = m->focus; fc = f.cursor; fid = f.cursor_id;
                }
                int res = TUI_DEFAULT;
                if (m->key_cb)
                    res = m->key_cb(*this, k, fp, fc, fid);
                if (res == TUI_QUIT) break;
                if (res == TUI_DEFAULT) {
                    default_nav(m, k);
                    fire_key_none(*this, m);
                }
            }
        }
    }
}

void Tui::set_status(const char *text) {
    impl_->status = text ? text : "";
}

int Tui::rows() const { return impl_->term_rows; }
int Tui::cols() const { return impl_->term_cols; }

int Tui::line_edit(const char *prompt, char *buf, int bsz) {
    auto *m = impl_.get();
    if (m->tty_fd < 0) return 0;
    int len = static_cast<int>(std::strlen(buf)), pos = len;
    for (;;) {
        m->scr.clear();
        sf(m, "\x1b[%d;1H\x1b[7m%s%s", m->term_rows, prompt, buf);
        for (int i = static_cast<int>(std::strlen(prompt)) + len; i < m->term_cols; i++)
            sp(m, " ");
        sf(m, "\x1b[0m\x1b[%d;%dH\x1b[?25h", m->term_rows,
           static_cast<int>(std::strlen(prompt)) + pos + 1);
        sflush(m);
        int k = read_key(m);
        if (k == TUI_K_NONE) continue;
        if (k == TUI_K_ENTER || k == '\n') { sp(m, "\x1b[?25l"); sflush(m); return 1; }
        if (k == TUI_K_ESC) { sp(m, "\x1b[?25l"); sflush(m); return 0; }
        if ((k == TUI_K_BS || k == 8) && pos > 0) {
            std::memmove(buf + pos - 1, buf + pos, static_cast<size_t>(len - pos + 1));
            pos--; len--;
        } else if (k >= 32 && k < 127 && len < bsz - 1) {
            std::memmove(buf + pos + 1, buf + pos, static_cast<size_t>(len - pos + 1));
            buf[pos++] = static_cast<char>(k); len++;
        }
    }
}

void Tui::show_help(const char **lines) {
    auto *m = impl_.get();
    if (m->tty_fd < 0) return;
    m->scr.clear();
    sp(m, "\x1b[H\x1b[2J");
    for (int i = 0; lines[i]; i++)
        sf(m, "\x1b[%d;1H\x1b[36m%s\x1b[0m", i + 1, lines[i]);
    sflush(m);
    while (read_key(m) == TUI_K_NONE) ;
}

void Tui::input_key(int key) {
    auto *m = impl_.get();
    for (auto &p : m->panels) {
        if (p.dirty) {
            p.iter_open = false;
            p.rows.clear();
            p.complete = false;
            p.row_count = 0;
            p_resolve_id(m, &p);
            p_clamp(m, &p);
            p_sync_id(m, &p);
            p.dirty = false;
        }
    }
    if (is_engine_nav_key(key)) {
        default_nav(m, key);
        fire_key_none(*this, m);
        return;
    }
    int fp = -1;
    const char *fid = "";
    int fc = 0;
    int npanels = static_cast<int>(m->panels.size());
    if (m->focus >= 0 && m->focus < npanels) {
        auto &f = m->panels[m->focus];
        fp = m->focus; fc = f.cursor; fid = f.cursor_id;
    }
    int res = TUI_DEFAULT;
    if (m->key_cb) res = m->key_cb(*this, key, fp, fc, fid);
    if (res == TUI_DEFAULT) {
        default_nav(m, key);
        fire_key_none(*this, m);
    }
}

void Tui::resize(int rows, int cols) {
    auto *m = impl_.get();
    if (rows > 0) m->term_rows = rows;
    if (cols > 0) m->term_cols = cols;
    for (auto &p : m->panels) p.dirty = true;
}

void Tui::dump_panel(int panel, FILE *out,
                     std::function<void(FILE *out, int idx, const RowData &row)> fmt) {
    for (int i = 0; ; i++) {
        auto *r = get_cached_row(panel, i);
        if (!r) break;
        fmt(out, i, *r);
    }
}
