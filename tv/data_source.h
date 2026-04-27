/* data_source.h - SQL-backed row iterators for tv's panels.
 *
 * Each panel mode runs SQL against the .tvdb. The DataSource exposed
 * to the engine is just an iterator wrapper. Heavy aggregations are
 * cached in tv_idx_* tables that are built on first use.
 *
 * Modes (visible numbering matches keys 1..7):
 *   1  process tree   (cursor: tgid)
 *   2  file tree      (cursor: path)
 *   3  event log      (cursor: "tgid:ts_ns" or just ts_ns)
 *   4  deps           (anchor: a path; rows: paths read to produce it)
 *   5  reverse deps   (anchor: a path; rows: paths derived from it)
 *   6  dep cmds       (anchor: a path; rows: processes in dep closure)
 *   7  rdep cmds      (anchor: a path; rows: processes in rdep closure)
 */
#pragma once

#include "engine.h"

#include <string>
#include <unordered_map>
#include <vector>

class TvDb;

struct AppState {
    int          mode = 1;
    std::string  cursor_id;        /* current cursor row id (mode-dependent) */
    std::string  search;           /* /search query - substring/glob filter */
    /* Flag filter - string of one-letter category codes that must ALL
     * be satisfied by a row. Vocabulary is mode-dependent:
     *   mode 2 (files): R W E w f s k   (see lpane_files for meaning)
     *   mode 1 (procs): K F D            (see lpane_processes for meaning)
     * The string syntax keeps composing simple: "WE" = writes AND errors.
     */
    std::string  flag_filter;
    std::string  subject_file;     /* anchor for modes 4..7 */
    bool         grouped = true;   /* tree (true) vs flat (false) view */
    bool         subtree_only = false;  /* mode 1: restrict to subtree of cursor */
    std::string  subtree_root;     /* mode 1: tgid of subtree root */
    bool         show_pids = false;     /* mode 1: prefix labels with [tgid] */
    /* Right-pane section collapsing: heading id (e.g. "__ho") -> collapsed. */
    std::vector<std::string> collapsed_sections;
    /* Time-range cutoffs (0 = no cutoff). Applied to ts_ns columns in
     * modes 0/1/2/3. Bound to keys '<' (before) and '>' (after) in the
     * UI: first press seeds the cutoff from the cursor's timestamp,
     * second press clears it. Lets you slice a long trace down to a
     * window without losing the rest of the filter state. */
    int64_t      ts_after_ns  = 0;
    int64_t      ts_before_ns = 0;
    /* Optional third column - htop-style snapshot of the process tree
     * at a chosen instant.  Toggled with `T` (capital).  When on, the
     * snapshot anchor (htop_anchor_ns) is auto-derived from the
     * currently-selected event each time the cursor commits.
     *
     * Coloring matches htop's "new/dying process" hint:
     *   - green: process started < 1 s before the anchor
     *   - red  : process exited  < 1 s before the anchor
     *   - dim  : process is alive at the anchor but otherwise unremarkable
     *   - cursor-row colour for the producing tgid of the selected event
     * The pane is purely informational — it doesn't accept focus.
     */
    bool         show_htop_col   = false;
    int64_t      htop_anchor_ns  = 0;
    /* tgid of the selected event's producer, highlighted in the htop
     * column so the eye can find it inside the snapshot tree. */
    std::string  htop_focus_tgid;
};

class TvDataSource {
public:
    explicit TvDataSource(TvDb &db, AppState &state);

    DataSource make_data_source();
    void invalidate();         /* both panes - for live data updates */
    void invalidate_rpane();   /* cursor change - keep lpane cache */

    /* Per-mode column layout for the left pane. Returned ColDef
     * pointer is owned by the data source (stable across calls).
     * The engine receives this via Tui::set_panel_columns() on every
     * mode change - see apply_layout(). */
    struct PanelLayout {
        const ColDef *cols;
        int           ncols;
        const char   *title;   /* used as a column-header row in the title bar */
    };
    PanelLayout lpane_layout() const;
    PanelLayout rpane_layout() const;
    PanelLayout hat_layout() const;   /* shared by both hat panels */
    /* Convenience: push lpane/rpane layouts to the engine. Call after
     * every mode change so cols/title match the new mode's row shape.
     * Hat panels have a single full-width column in every mode. */
    void apply_layout(class Tui &tui, int lpane, int rpane) const;
    void apply_hat_layout(class Tui &tui, int hat_top, int hat_bot) const;

    /* Number of rows the hat panes will produce for the current state.
     * Used by the app to size the hat boxes (weight=0, min_size=N) so
     * that an empty hat takes zero screen rows. Both helpers build
     * lazily on first call after invalidate(). */
    int hat_top_row_count();
    int hat_bot_row_count();

    /* Recompute the hat panes from a *window* of lpane rows (cached
     * since the last rebuild). Used by the main loop to make the hats
     * behave like sticky breadcrumbs: as you scroll the lpane, the
     * proc-tree hat (mode 1) shows the common-ancestor chain of the
     * rows currently in view, and the file-tree hat (mode 2) shows the
     * common path prefix of the visible rows. Cheap - no SQL.
     *
     *   first_row  - index of the topmost visible lpane row (>= 0)
     *   n_rows     - number of visible rows (must be > 0; clamped)
     *
     * No-op outside modes 1/2 (other modes don't have hats), and a
     * no-op while the lpane cache itself is dirty (the next ensure
     * pass will recompute hats globally as before). Returns true if
     * hat_top_/hat_bot_ contents changed (so the caller can mark only
     * the hat panels dirty). */
    bool recompute_hats_for_window(int first_row, int n_rows);

    /* Per-mode column layout for the htop snapshot column (panel 4
     * when show_htop_col is true).  Single flex column.  The data
     * source builds the rows lazily on first row_begin(panel=4). */
    PanelLayout htop_layout() const;
    void apply_htop_layout(class Tui &tui, int htop_pane) const;
    /* Force the next htop_pane build to use this anchor (in ns).
     * Comparing against the cached anchor lets us avoid rebuilding
     * when the cursor commits to a row with the same timestamp. */
    void set_htop_anchor_ns(int64_t ts_ns);

private:
    void row_begin(int panel);
    bool row_has_more(int panel);
    RowData row_next(int panel);

    void rebuild_lpane();
    void rebuild_rpane();
    void ensure_hats_built(); /* rebuild stale hat caches */

    /* Mode-specific lpane builders. */
    void lpane_outputs();             /* mode 0: stdout/stderr stream */
    void lpane_processes();
    void lpane_files();
    void lpane_events();
    void lpane_deps(int reverse);     /* 0=deps, 1=rdeps */
    void lpane_dep_cmds(int reverse); /* 0=dcmds, 1=rcmds */
    /* Common-prefix detection helper for mode 2 (file lists).
     * Mutates lpane_ to strip the common path prefix and feeds the
     * prefix line into hat_bot_. The mode-1 (proc tree) variant is
     * inlined in lpane_processes() because it needs the procs map to
     * walk parent_id chains. */
    void emit_path_hat();

    /* Mode-specific rpane builders. */
    void rpane_process_detail(const std::string &tgid);
    void rpane_file_detail(const std::string &path);
    void rpane_event_detail(const std::string &id);
    void rpane_output_detail(const std::string &id); /* mode 0 cursor */

    /* Build/serve the htop-style snapshot column. */
    void rebuild_htop();

    TvDb     &db_;
    AppState &state_;

    std::vector<RowData> lpane_;
    std::vector<RowData> rpane_;
    std::vector<RowData> hat_top_;
    std::vector<RowData> hat_bot_;
    std::vector<RowData> htop_;
    /* Cached per-rebuild-of-lpane: tgid -> in-trace ancestor chain
     * (child→parent ordered, init filtered out). Lets the proc-tree hat
     * be recomputed for any lpane row window without re-running SQL.
     * Populated by lpane_processes(); empty in other modes. */
    std::unordered_map<int, std::vector<int>> proc_chain_;
    /* Same per-rebuild snapshot, for proc display labels in the hat
     * (basename of exe, plus optional [tgid]). Mirrors the procs map
     * built inside lpane_processes() but kept tiny - just what the hat
     * label needs. */
    std::unordered_map<int, std::string> proc_label_;
    size_t lpane_idx_ = 0;
    size_t rpane_idx_ = 0;
    size_t hat_top_idx_ = 0;
    size_t hat_bot_idx_ = 0;
    size_t htop_idx_ = 0;
    bool   built_lpane_ = false;
    bool   built_rpane_ = false;
    bool   built_hat_top_ = false;
    bool   built_hat_bot_ = false;
    bool   built_htop_ = false;
    int64_t built_htop_for_ns_  = -1;
    std::string built_htop_focus_tgid_;
    std::string built_for_cursor_;
    std::string built_for_subject_;
    int    built_for_mode_ = -1;
    std::string built_for_search_;
    std::string built_for_flag_filter_;
    bool   built_for_grouped_ = true;
    bool   built_for_subtree_only_ = false;
    std::string built_for_subtree_root_;
    bool   built_for_show_pids_ = false;
    std::vector<std::string> built_for_collapsed_;
    int64_t built_for_ts_after_ns_  = 0;
    int64_t built_for_ts_before_ns_ = 0;
};
