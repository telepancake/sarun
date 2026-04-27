/*
 * fv - filesystem viewer
 *
 * A Smalltalk-style column browser built on engine.h/engine.cpp.
 *
 * Layout:
 *   +------+------+------+
 *   | d0   | d1   | d2   |   <- dir columns (engine auto-scrolls via HSCROLL)
 *   +------+------+------+
 *   | content            |   <- file content (text or hex)
 *   +--------------------+
 *   status bar
 *
 * Keys:
 *   ^ v j k  PgUp PgDn  Home/g End   Navigate within a column
 *   <-  h                              Focus left column / go to parent dir
 *   ->  Enter                          Enter dir / focus content pane
 *   Tab                               Cycle all panels
 *   H                                 Toggle hex mode
 *   .                                 Toggle hidden files
 *   ?                                 Help
 *   q  Esc                            Quit
 */

#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <format>
#include <memory>
#include <string>
#include <vector>

#include <dirent.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <climits>

#include "engine.h"

#ifndef PATH_MAX
#define PATH_MAX 4096
#endif

static constexpr int FV_MAX_DEPTH = 16;
static constexpr int FV_READ_MAX  = 4 * 1024 * 1024;

/* -- Data types -------------------------------------------------------- */

struct FvEntry {
    char name[256];
    int  is_dir;
};

struct FvState {
    std::string             path_stack[FV_MAX_DEPTH];
    int                     depth_count{};
    std::vector<FvEntry>    entries[FV_MAX_DEPTH];
    std::vector<std::string> lines;
    int                     hex_mode{};
    int                     show_hidden{};
    std::string             shown_path;
    std::unique_ptr<Tui>    tui;

    int dir_panels[FV_MAX_DEPTH]{};   /* panel indices assigned by add_panel() */
    int content_panel{-1};
    int last_dir_panel{0};            /* dir panel to return to when Tab leaves content */
};

static FvState g;

/* -- Layout: static box tree ------------------------------------------- */

static Box   dir_boxes[FV_MAX_DEPTH];
static Box   content_box;
static Box   top_hbox, root_vbox;

/* -- Helpers ------------------------------------------------------------ */

static std::string child_path(const std::string &parent, const char *name)
{
    if (parent == "/") return std::format("/{}", name);
    return std::format("{}/{}", parent, name);
}

/* Return the depth index for a given panel index, or -1. */
static int panel_depth(int panel)
{
    for (int d = 0; d < FV_MAX_DEPTH; d++)
        if (g.dir_panels[d] == panel) return d;
    return -1;
}

/* -- Directory loading -------------------------------------------------- */

static void load_dir(int depth)
{
    if (depth < 0 || depth >= FV_MAX_DEPTH) return;
    g.entries[depth].clear();
    if (g.path_stack[depth].empty()) return;

    DIR *dir = opendir(g.path_stack[depth].c_str());
    if (!dir) return;

    struct dirent *de;
    while ((de = readdir(dir))) {
        const char *nm = de->d_name;
        if (nm[0] == '.' && (nm[1] == '\0' || (nm[1] == '.' && nm[2] == '\0'))) continue;
        if (!g.show_hidden && nm[0] == '.') continue;

        FvEntry e{};
        std::snprintf(e.name, sizeof e.name, "%s", nm);
        auto full = child_path(g.path_stack[depth], nm);
        struct stat st;
        e.is_dir = (stat(full.c_str(), &st) == 0 && S_ISDIR(st.st_mode));
        g.entries[depth].push_back(e);
    }
    closedir(dir);

    std::sort(g.entries[depth].begin(), g.entries[depth].end(),
              [](const FvEntry &a, const FvEntry &b) {
                  if (a.is_dir != b.is_dir) return a.is_dir > b.is_dir;
                  return std::strcmp(a.name, b.name) < 0;
              });
}

/* -- Content (file) loading --------------------------------------------- */

static void free_lines()
{
    g.lines.clear();
    g.shown_path.clear();
}

static bool looks_binary(const unsigned char *buf, int n)
{
    int check = n < 512 ? n : 512;
    for (int i = 0; i < check; i++) {
        unsigned char c = buf[i];
        if (c == 0 || (c < 8 && c != '\t' && c != '\n' && c != '\r')) return true;
    }
    return false;
}

static std::string mk_hex_line(const unsigned char *buf, int off, int bufsz)
{
    char tmp[80];
    int p = 0, len = bufsz - off;
    if (len > 16) len = 16;
    p += std::snprintf(tmp + p, sizeof tmp - static_cast<size_t>(p), "%08x  ", off);
    for (int i = 0; i < 16; i++) {
        if (i < len) p += std::snprintf(tmp + p, sizeof tmp - static_cast<size_t>(p), "%02x ", static_cast<unsigned>(buf[off + i]));
        else         p += std::snprintf(tmp + p, sizeof tmp - static_cast<size_t>(p), "   ");
        if (i == 7 && p < static_cast<int>(sizeof tmp) - 1) tmp[p++] = ' ';
    }
    if (p < static_cast<int>(sizeof tmp) - 1) tmp[p++] = ' ';
    if (p < static_cast<int>(sizeof tmp) - 1) tmp[p++] = '|';
    for (int i = 0; i < len && p < static_cast<int>(sizeof tmp) - 2; i++) {
        unsigned char c = buf[off + i];
        tmp[p++] = (c >= 32 && c < 127) ? static_cast<char>(c) : '.';
    }
    if (p < static_cast<int>(sizeof tmp) - 1) tmp[p++] = '|';
    tmp[p] = '\0';
    return std::string(tmp);
}

static void load_content(const std::string &path)
{
    free_lines();
    g.shown_path = path;

    int fd = open(path.c_str(), O_RDONLY);
    if (fd < 0) {
        g.lines.push_back(std::strerror(errno));
        return;
    }

    std::vector<unsigned char> buf(FV_READ_MAX + 1, 0);
    int bufsz = 0;
    while (bufsz < FV_READ_MAX) {
        auto n = read(fd, buf.data() + bufsz, static_cast<size_t>(FV_READ_MAX - bufsz));
        if (n <= 0) break;
        bufsz += static_cast<int>(n);
    }
    close(fd);

    if (g.hex_mode || looks_binary(buf.data(), bufsz)) {
        if (bufsz == 0) {
            g.lines.push_back("(empty)");
        } else {
            for (int off = 0; off < bufsz; off += 16)
                g.lines.push_back(mk_hex_line(buf.data(), off, bufsz));
        }
    } else {
        if (bufsz == 0) {
            g.lines.push_back("(empty)");
        } else {
            const char *cp = reinterpret_cast<const char *>(buf.data());
            const char *end = cp + bufsz;
            while (cp < end) {
                const char *nl = static_cast<const char *>(
                    std::memchr(cp, '\n', static_cast<size_t>(end - cp)));
                int len = nl ? static_cast<int>(nl - cp) : static_cast<int>(end - cp);
                std::string line(cp, static_cast<size_t>(len));
                if (!line.empty() && line.back() == '\r') line.pop_back();
                g.lines.push_back(std::move(line));
                cp = nl ? nl + 1 : end;
            }
        }
    }
}

/* -- Data source callbacks ---------------------------------------------- */

/* Per-panel iterator state (only one panel is iterated at a time). */
static struct {
    int panel = -1;
    int idx   = 0;
    int count = 0;
} g_fv_iter;

static void fv_row_begin(int panel)
{
    g_fv_iter.panel = panel;
    g_fv_iter.idx   = 0;
    if (panel == g.content_panel) {
        g_fv_iter.count = static_cast<int>(g.lines.size());
        return;
    }
    int d = panel_depth(panel);
    if (d >= 0)
        g_fv_iter.count = static_cast<int>(g.entries[d].size());
    else
        g_fv_iter.count = 0;
}

static bool fv_row_has_more(int /*panel*/)
{
    return g_fv_iter.idx < g_fv_iter.count;
}

static RowData fv_row_next(int panel)
{
    int rownum = g_fv_iter.idx++;

    if (panel == g.content_panel) {
        RowData rd;
        char idbuf[32];
        std::snprintf(idbuf, sizeof idbuf, "%d", rownum);
        rd.id   = idbuf;
        rd.cols = {g.lines[static_cast<size_t>(rownum)]};
        return rd;
    }

    int depth = panel_depth(panel);
    if (depth < 0 || g.entries[depth].empty()) return {};

    auto &e = g.entries[depth][static_cast<size_t>(rownum)];
    RowData rd;
    rd.id    = e.name;
    rd.style = e.is_dir ? RowStyle::Cyan : RowStyle::Normal;
    char namebuf[258];
    if (e.is_dir) std::snprintf(namebuf, sizeof namebuf, "%s/", e.name);
    else          std::snprintf(namebuf, sizeof namebuf, "%s",  e.name);
    rd.cols = {namebuf};
    return rd;
}

/* -- Sync helpers ------------------------------------------------------- */

static void sync_right_of(int d)
{
    if (d < 0 || d >= FV_MAX_DEPTH) return;
    int cursor = g.tui->get_cursor(g.dir_panels[d]);

    for (int i = d + 1; i < g.depth_count; i++) {
        g.entries[i].clear();
        g.path_stack[i].clear();
        g.tui->dirty(g.dir_panels[i]);
    }

    if (cursor < 0 || cursor >= static_cast<int>(g.entries[d].size()) || g.entries[d].empty()) {
        free_lines(); g.tui->dirty(g.content_panel); return;
    }

    auto &e = g.entries[d][static_cast<size_t>(cursor)];
    auto full = child_path(g.path_stack[d], e.name);

    if (e.is_dir && d + 1 < g.depth_count) {
        g.path_stack[d + 1] = full;
        load_dir(d + 1);
        g.tui->set_cursor_idx(g.dir_panels[d + 1], 0);
        g.tui->dirty(g.dir_panels[d + 1]);
        if (g.shown_path != full) {
            free_lines();
            g.shown_path = full;
        }
    } else if (!e.is_dir) {
        if (g.shown_path != full) {
            load_content(full);
            g.tui->set_cursor_idx(g.content_panel, 0);
        }
    }
    g.tui->dirty(g.content_panel);
}

/* Navigate d0 to the parent of its current path. */
static void go_to_parent()
{
    const std::string &cur = g.path_stack[0];
    if (cur.empty() || cur == "/") return;

    auto pos = cur.rfind('/');
    if (pos == std::string::npos) return; /* relative path without '/' - shouldn't happen */
    std::string child_name = cur.substr(pos + 1);
    std::string parent = (pos == 0) ? "/" : cur.substr(0, pos);

    /* Clear depths 1+ */
    for (int i = 1; i < g.depth_count; i++) {
        g.entries[i].clear();
        g.path_stack[i].clear();
        g.tui->dirty(g.dir_panels[i]);
    }
    free_lines();
    g.tui->dirty(g.content_panel);

    g.path_stack[0] = parent;
    load_dir(0);
    g.tui->dirty(g.dir_panels[0]);

    /* Restore cursor to the directory we came from. */
    for (int i = 0; i < static_cast<int>(g.entries[0].size()); i++) {
        if (g.entries[0][static_cast<size_t>(i)].name == child_name) {
            g.tui->set_cursor_idx(g.dir_panels[0], i);
            break;
        }
    }

    g.tui->focus(g.dir_panels[0]);
    sync_right_of(0);
}

/* -- Status bar --------------------------------------------------------- */

static void update_status()
{
    int focus_panel = g.tui->get_focus();
    std::string status;

    int d = panel_depth(focus_panel);
    if (d >= 0) {
        int cursor = g.tui->get_cursor(g.dir_panels[d]);
        if (cursor >= 0 && cursor < static_cast<int>(g.entries[d].size()) && !g.entries[d].empty()) {
            auto &e = g.entries[d][static_cast<size_t>(cursor)];
            auto full = child_path(g.path_stack[d], e.name);
            status = std::format(" {}{}", full, e.is_dir ? "/" : "");
        } else {
            status = std::format(" {}/", g.path_stack[d]);
        }
    } else if (focus_panel == g.content_panel) {
        status = std::format(" [content]  {}",
                             g.shown_path.empty() ? "(nothing)" : g.shown_path);
    }
    g.tui->set_status(status.c_str());
}

/* -- Key callback ------------------------------------------------------- */

static const char *HELP[] = {
    "",
    "  fv \xe2\x80\x94 filesystem viewer",
    "  \xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80",
    "",
    "  \xe2\x86\x91 \xe2\x86\x93  j k   Navigate within column",
    "  PgUp PgDn  Page up / down",
    "  Home g     First item     End  Last item",
    "  Tab        Cycle between columns and content pane",
    "",
    "  \xe2\x86\x90  h       Move focus to the left column / parent directory",
    "  \xe2\x86\x92  Enter   Enter directory / focus content pane",
    "",
    "  H           Toggle hex mode in the content pane",
    "  .           Toggle hidden files",
    "  ?           This help",
    "  q  Esc      Quit",
    "",
    "  Press any key to close.",
    nullptr
};

static int on_key(Tui &tui, int key, int panel,
                  int /*cursor*/, const char * /*row_id*/)
{
    int d = panel_depth(panel);
    bool in_content = (panel == g.content_panel);

    if (key == TUI_K_NONE) {
        if (d >= 0) sync_right_of(d);
        update_status();
        return TUI_HANDLED;
    }

    if (key == 'q' || key == TUI_K_ESC) { tui.quit(); return TUI_HANDLED; }
    if (key == '?') { tui.show_help(HELP); tui.dirty(); return TUI_HANDLED; }
    if (key == 'H') {
        g.hex_mode = !g.hex_mode;
        if (!g.shown_path.empty()) {
            auto saved = g.shown_path;
            load_content(saved);
            tui.set_cursor_idx(g.content_panel, 0);
        }
        tui.dirty(g.content_panel);
        return TUI_HANDLED;
    }
    if (key == '.') {
        g.show_hidden = !g.show_hidden;
        for (int i = 0; i < g.depth_count; i++) {
            if (!g.path_stack[i].empty()) load_dir(i);
            tui.dirty(g.dir_panels[i]);
        }
        tui.dirty(g.content_panel);
        return TUI_HANDLED;
    }
    if (key == TUI_K_TAB) {
        if (in_content) {
            /* Return to whichever dir panel was last active. */
            tui.focus(g.dir_panels[g.last_dir_panel]);
        } else if (d >= 0) {
            /* Switch to the content panel, remembering this dir panel. */
            g.last_dir_panel = d;
            tui.focus(g.content_panel);
        }
        update_status();
        return TUI_HANDLED;
    }
    if (key == TUI_K_LEFT || key == 'h') {
        if (in_content) {
            /* Return to last populated dir column. */
            for (int i = g.depth_count - 1; i >= 0; i--)
                if (!g.entries[i].empty() || !g.path_stack[i].empty()) {
                    tui.focus(g.dir_panels[i]); break;
                }
        } else if (d > 0) {
            tui.focus(g.dir_panels[d - 1]);
        } else {
            /* d == 0: navigate to parent directory. */
            go_to_parent();
        }
        update_status();
        return TUI_HANDLED;
    }
    if (key == TUI_K_RIGHT || (key == TUI_K_ENTER && !in_content)) {
        if (d >= 0 && d < FV_MAX_DEPTH && !g.entries[d].empty()) {
            int c = tui.get_cursor(g.dir_panels[d]);
            if (c >= 0 && c < static_cast<int>(g.entries[d].size())) {
                auto &e = g.entries[d][static_cast<size_t>(c)];
                if (e.is_dir && d + 1 < g.depth_count) {
                    /* Populate the next column immediately before focusing it. */
                    sync_right_of(d);
                    tui.focus(g.dir_panels[d + 1]);
                } else if (!e.is_dir) {
                    tui.focus(g.content_panel);
                }
            }
        }
        update_status();
        return TUI_HANDLED;
    }
    return TUI_DEFAULT;
}

/* -- Layout: fill the static box tree ----------------------------------- */

static ColDef  s_dir_col{-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_ELLIPSIS};
static ColDef  s_content_col{-1, TUI_ALIGN_LEFT, TUI_OVERFLOW_TRUNCATE};
static PanelDef s_dir_defs[FV_MAX_DEPTH]{};
static PanelDef s_content_def{};

static void build_layout()
{
    g.depth_count = FV_MAX_DEPTH;

    std::vector<Box*> hbox_ch;
    for (int i = 0; i < FV_MAX_DEPTH; i++) {
        s_dir_defs[i] = PanelDef{nullptr, &s_dir_col, 1, TUI_PANEL_CURSOR | TUI_PANEL_BORDER};
        g.dir_panels[i] = g.tui->add_panel(s_dir_defs[i]);
        dir_boxes[i] = Box{TUI_BOX_PANEL, 1, 0, 0, g.dir_panels[i], {}};
        hbox_ch.push_back(&dir_boxes[i]);
    }
    s_content_def = PanelDef{nullptr, &s_content_col, 1, TUI_PANEL_CURSOR};
    g.content_panel = g.tui->add_panel(s_content_def);
    content_box = Box{TUI_BOX_PANEL, 3, 3, 0, g.content_panel, {}};

    top_hbox = Box{TUI_BOX_HBOX, 1, 3, TUI_BOX_HSCROLL, -1, std::move(hbox_ch)};
    root_vbox = Box{TUI_BOX_VBOX, 1, 0, 0, -1, {&top_hbox, &content_box}};
}

/* -- main --------------------------------------------------------------- */

int fv_main(int argc, char **argv)
{
    const char *startpath = (argc > 1) ? argv[1] : ".";
    char abspath[PATH_MAX];
    if (!realpath(startpath, abspath)) {
        std::fprintf(stderr, "fv: %s: %s\n", startpath, std::strerror(errno));
        return 1;
    }
    struct stat st;
    if (stat(abspath, &st) != 0) {
        std::fprintf(stderr, "fv: %s: %s\n", abspath, std::strerror(errno));
        return 1;
    }
    if (S_ISREG(st.st_mode)) {
        char *slash = std::strrchr(abspath, '/');
        if (slash && slash > abspath) *slash = '\0';
        else { abspath[0] = '/'; abspath[1] = '\0'; }
    }
    g.path_stack[0] = abspath;

    DataSource src{fv_row_begin, fv_row_has_more, fv_row_next};
    g.tui = Tui::open(std::move(src));
    if (!g.tui) { std::fprintf(stderr, "fv: cannot open terminal\n"); return 1; }

    build_layout();
    g.tui->set_layout(&root_vbox);
    g.tui->on_key(on_key);

    load_dir(0);
    g.tui->focus(g.dir_panels[0]);
    sync_right_of(0);
    update_status();
    g.tui->dirty();

    g.tui->run();
    return 0;
}
