/* tv_db.h - tv's storage and query layer, on DuckDB.
 *
 * A trace lives on disk as a single DuckDB native database file
 * (`foo.tvdb`). One table per trace event class:
 *
 *   exec, argv, env, auxv, exit, open, cwd, stdout, stderr
 *
 * Schema mirrors trace/trace.h:
 *   - all tables share six common header columns
 *     (ts_ns UBIGINT, pid INT, tgid INT, ppid INT, nspid INT, nstgid INT)
 *   - argv/env/auxv are split into one row per element (idx column)
 *   - exit/open carry their type-specific fixed fields as extra columns
 *   - the rest store the variable blob in a single BLOB column
 *
 * DuckDB's native file format is columnar, zstd-compressed, and
 * mmaped on open; "loading" a 20 GB trace is opening one fd.
 */
#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <vector>

struct TraceEvent;

class TvDb {
public:
    /* Open a `.tvdb` file (creates it and the schema if missing) for
     * read+write. `*err` carries a message on nullptr return. */
    static std::unique_ptr<TvDb> open_file(const std::string &path,
                                           std::string *err);

    /* Create an in-memory `.tvdb` (used for tests / live mode buffer). */
    static std::unique_ptr<TvDb> open_memory(std::string *err);

    ~TvDb();
    TvDb(const TvDb &) = delete;
    TvDb &operator=(const TvDb &) = delete;

    /* Append one decoded trace event into the matching table. Internally
     * uses cached duckdb_appender objects per table; call flush() to
     * commit pending appends to the on-disk file. */
    bool append(const TraceEvent &ev, std::string *err);

    /* Flush all open appenders and CHECKPOINT the database. Required
     * before queries that should see appended data, and at shutdown. */
    bool flush(std::string *err);

    /* Run a query and read column 0 of every row as int64. Returns
     * empty on error and sets *err. */
    std::vector<int64_t> query_int64(const std::string &sql,
                                     std::string *err);

    /* Run a query, return matrix of strings (rows x cols).
     * BLOB columns are converted to lossy UTF-8. */
    std::vector<std::vector<std::string>>
    query_strings(const std::string &sql, std::string *err);

    /* Total number of rows across all event tables (for sanity / tests). */
    int64_t total_event_count();

    /* Lazy indices. Each ensure_*() materialises a small derived table
     * the first time it is called for a given .tvdb, then records the
     * fact in tv_meta so subsequent opens skip the work. They are no-ops
     * after the first call (per-process) or first build (per-file). */
    bool ensure_proc_index(std::string *err);   /* tv_idx_proc */
    bool ensure_path_index(std::string *err);   /* tv_idx_path */
    bool ensure_edge_index(std::string *err);   /* tv_idx_edge (proc<->path)*/
    /* Kept for source compat; tv_idx_open_canon is now built as a
     * real on-disk table by ensure_path_index() (the v3 view variant
     * pushed the ASOF JOIN into every per-row query and made mode 2
     * lag). This is now equivalent to ensure_path_index(). */
    bool ensure_canon_table(std::string *err);

    /* Path of the backing file (".tvdb" or ":memory:" for in-mem). */
    const std::string &path() const;

    /* Opaque handle for callers that need the duckdb_connection
     * directly (e.g. lazy row iterators). Returns void*; caller must
     * cast to duckdb_connection. */
    void *raw_conn();

private:
    TvDb();
    static std::unique_ptr<TvDb> open_with_path(const char *path,
                                                const std::string &display,
                                                std::string *err);
    struct Impl;
    std::unique_ptr<Impl> impl_;
};
