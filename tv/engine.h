#pragma once

#include <functional>
#include <memory>
#include <string>
#include <vector>

/* -- Key constants --------------------------------------------------- */

enum {
    TUI_K_NONE  = -1,
    TUI_K_UP    = 256, TUI_K_DOWN, TUI_K_LEFT, TUI_K_RIGHT,
    TUI_K_PGUP, TUI_K_PGDN, TUI_K_HOME, TUI_K_END,
    TUI_K_TAB   = 9,
    TUI_K_ENTER = 13,
    TUI_K_ESC   = 27,
    TUI_K_BS    = 127
};

inline constexpr int TUI_HANDLED =  1;
inline constexpr int TUI_DEFAULT =  0;
inline constexpr int TUI_QUIT    = -1;

enum { TUI_ALIGN_LEFT = 0, TUI_ALIGN_RIGHT, TUI_ALIGN_CENTER };
enum { TUI_OVERFLOW_TRUNCATE = 0, TUI_OVERFLOW_ELLIPSIS };

enum class RowStyle {
    Normal,
    Error,
    Search,
    Heading,
    Green,
    Dim,
    Bold,
    Cyan,
    CyanBold,
    Yellow,
};

inline constexpr int TUI_PANEL_CURSOR = 0x01;
inline constexpr int TUI_PANEL_BORDER = 0x02;

inline constexpr int TUI_BOX_HBOX  = 0;
inline constexpr int TUI_BOX_VBOX  = 1;
inline constexpr int TUI_BOX_PANEL = 2;

/* hbox flag: auto-scroll to keep focused child visible */
inline constexpr int TUI_BOX_HSCROLL = 0x01;

/* Sentinel: dirty/focus all panels */
inline constexpr int TUI_ALL_PANELS = -1;

/* -- Layout types ---------------------------------------------------- */

struct ColDef {
    int         width;
    int         align;
    int         overflow;
};

struct PanelDef {
    const char    *title;
    const ColDef  *cols;
    int            ncols;
    int            flags;
};

struct Box {
    int                    type;
    int                    weight;
    int                    min_size;
    int                    flags;
    int                    panel;     /* panel index (TUI_BOX_PANEL) or -1 */
    std::vector<Box*>      children;
};

struct RowData {
    std::string id;
    RowStyle    style = RowStyle::Normal;
    std::vector<std::string> cols;

    /* App-level metadata (engine ignores; carried through for app use) */
    std::string parent_id;
    int         link_mode = -1;
    std::string link_id;
    bool        has_children = false;
};

/* -- Tui class ------------------------------------------------------- */

class Tui;

using KeyCallback   = std::function<int(Tui &tui, int key, int panel,
                                        int cursor, const char *row_id)>;
using FdCallback    = std::function<void(Tui &tui, int fd)>;
using TimerCallback = std::function<int(Tui &tui)>;

/* Iterator-based data source.  The engine calls row_begin() once, then
   loops row_has_more() / row_next() to read every row lazily.  All rows
   are cached by the engine-the app just provides a forward iterator. */
struct DataSource {
    std::function<void(int panel)>     row_begin;
    std::function<bool(int panel)>     row_has_more;
    std::function<RowData(int panel)>  row_next;
};

class Tui {
public:
    /* Factory methods (return nullptr on failure) */
    static std::unique_ptr<Tui> open(DataSource src);
    static std::unique_ptr<Tui> open_headless(DataSource src, int rows, int cols);

    ~Tui();
    Tui(const Tui &) = delete;
    Tui &operator=(const Tui &) = delete;

    /* Panels - add before set_layout */
    int  add_panel(PanelDef def);
    int  panel_count() const;

    /* Reconfigure a panel's columns at runtime - used to give each
       app mode (process tree, file tree, event log, output stream, ...)
       its own column layout instead of forcing all modes to share one
       shape baked in at add_panel time. Pass title=nullptr to leave
       the panel title unchanged. */
    void set_panel_columns(int panel, const ColDef *cols, int ncols,
                           const char *title = nullptr);

    /* Layout */
    void set_layout(Box *root);

    /* Panel state - panel index, or TUI_ALL_PANELS for dirty */
    void        dirty(int panel = TUI_ALL_PANELS);
    void        focus(int panel);
    int         get_focus() const;

    void        set_cursor(int panel, const char *id);
    void        set_cursor_idx(int panel, int idx);
    int         get_cursor(int panel) const;
    int         get_scroll(int panel) const;
    const char *get_cursor_id(int panel) const;
    const RowData *get_cached_row(int panel, int idx);

    /* Callbacks */
    void on_key(KeyCallback cb);
    void watch_fd(int fd, FdCallback cb);
    void unwatch_fd(int fd);
    int  add_timer(int ms, TimerCallback cb);
    void remove_timer(int timer_id);

    /* Event loop */
    void run();
    void quit();

    /* Status */
    void set_status(const char *text);

    /* Headless / test helpers */
    void input_key(int key);
    void resize(int rows, int cols);
    void dump_panel(int panel, FILE *out,
                    std::function<void(FILE *out, int idx, const RowData &row)> fmt);

    /* Interactive prompts (only work with a real terminal) */
    int  line_edit(const char *prompt, char *buf, int bsz);
    void show_help(const char **lines);

    /* Terminal size */
    int rows() const;
    int cols() const;

    // Impl is exposed for internal free-function helpers in engine.cpp
    struct Impl;

private:
    Tui();
    std::unique_ptr<Impl> impl_;
};
