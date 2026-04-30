/*
 * sud/runtime_config.h — Wrapper-level runtime configuration.
 *
 * The sud32/sud64 wrappers used to inherit configuration via several
 * SUD_* environment variables (SUD_INRAMFS, SUD_INRAMFS_KEY,
 * SUD_INRAMFS_CWD, SUD_OVERLAY, SUD_REMAP, SUDTRACE_OUTFILE).  This
 * coupled the launcher and the addins through a shared dumping
 * ground.  The new design replaces every such variable with a
 * positional flag in the wrapper command line:
 *
 *   sud{32,64} [--no-env]
 *              [--drop-argv N]
 *              [--cwd <abs>]
 *              [--trace-outfile <abs>]
 *              [--inramfs-key <key>] [--inramfs-meta-mb <N>]
 *              [--remap-rule <kind>:<spec>]   (repeatable)
 *              /path/to/binary [args...]
 *
 * Parsing terminates at the first non-flag argument (the target
 * binary).  All flags are positional — the wrapper accepts them in
 * any order within the leading flag block.
 *
 * Three callers manipulate this struct:
 *
 *   1. sud/wrapper.c::main parses argv into it before invoking
 *      sud_addins_wrapper_init().  Each addin's wrapper_init reads
 *      its slice from g_sud_runtime_config instead of touching the
 *      environment.
 *
 *   2. sud/sudtrace.c::build_wrapper_argv builds a config from the
 *      launcher's command-line options + auto-minted inramfs key,
 *      then emits the flag block onto the wrapper's argv.
 *
 *   3. sud/elf.c::build_exec_argv (execve interception in the SIGSYS
 *      handler) snapshots the current process's live config into a
 *      struct via sud_runtime_config_snapshot() and emits the same
 *      flag block onto the rewritten argv.  This is what makes
 *      execve(target, argv, envp) propagate the wrapper config to
 *      child processes WITHOUT writing to envp.
 *
 * The header is intentionally dependency-free (only <stddef.h>) so
 * both the freestanding wrapper and the libc-linked launcher can
 * include it.  All string handling is done with small inline helpers
 * in runtime_config.c that also avoid both <string.h> and the
 * libc-fs replacement, keeping the TU portable across both build
 * environments.
 */

#ifndef SUD_RUNTIME_CONFIG_H
#define SUD_RUNTIME_CONFIG_H

#include <stddef.h>

#define SUD_RC_MAX_REMAP_RULES 64

/* Upper bound on the number of argv slots a fully-loaded config can
 * emit (used by callers to size buffers).  Each remap rule consumes
 * 2 slots ("--remap-rule" + spec); other flags consume at most 12. */
#define SUD_RC_MAX_EMIT_ARGS  (12 + SUD_RC_MAX_REMAP_RULES * 2)

struct sud_runtime_config {
    int         no_env;             /* 1 if --no-env present          */
    int         drop_count;         /* --drop-argv N (0 = unset)      */
    const char *cwd;                /* --cwd <abs> (NULL = unset)     */
    const char *trace_outfile;      /* --trace-outfile <abs>          */
    const char *inramfs_key;        /* --inramfs-key <key>            */
    int         inramfs_meta_mb;    /* --inramfs-meta-mb N (0=default) */
    int         remap_rule_count;
    /* Each entry is a "<kind>:<spec>" string, e.g.
     *   "remap:/src=/dst"
     *   "overlay:/m=/up+/lo1+/lo2"
     *   "inramfs:/mountpoint"
     * Pointers refer to argv (parser) or to the caller's storage
     * (snapshot).  No ownership is transferred. */
    const char *remap_rules[SUD_RC_MAX_REMAP_RULES];
};

/* Reset cfg to all-zero defaults. */
void sud_runtime_config_clear(struct sud_runtime_config *cfg);

/* Parse the leading flag block from argv starting at *argi.  On
 * return *argi points at the first non-flag arg (caller should treat
 * that as the target binary path).  Strings stored into *cfg alias
 * argv entries directly — no allocation, no copying.
 *
 * Returns 0 on success, -1 on a malformed flag (e.g. --cwd missing
 * its value, or too many --remap-rule entries). */
int sud_runtime_config_parse(int argc, char **argv, int *argi,
                             struct sud_runtime_config *cfg);

/* Emit *cfg as a sequence of argv strings into out[].  Returns the
 * number of strings written, or -1 on overflow.
 *
 * Each emitted pointer is either a constant string literal
 * ("--no-env", "--cwd", etc.), a pointer into *cfg's strings, or a
 * pointer into the caller-provided int_scratch buffer (used to
 * format integer-valued flags like --drop-argv and
 * --inramfs-meta-mb).  The scratch must be at least 32 bytes; it is
 * the caller's responsibility to keep the scratch live for as long
 * as the emitted pointers are read.
 *
 * The caller must duplicate (or arena-strdup) entries before storing
 * them into a fork/exec argv that outlives *cfg or int_scratch.
 *
 * Does NOT write the target binary or its argv — the caller appends
 * those after the emitted flag block. */
int sud_runtime_config_emit(const struct sud_runtime_config *cfg,
                            const char **out, int max,
                            char *int_scratch, int int_scratch_size);

/* Replace every string field in *cfg with a heap-allocated copy
 * (via the libc strdup, which uses mmap in the freestanding wrapper
 * and malloc in the libc-linked launcher).  Required when the
 * caller-provided strings (typically argv[] entries) are about to
 * be overwritten or freed.  Strings already NULL or empty are left
 * unchanged.  No deduplication; on allocation failure the affected
 * field is set to NULL. */
void sud_runtime_config_intern(struct sud_runtime_config *cfg);

/* Test helper: copy *src into the live g_sud_runtime_config global,
 * intern its strings, and mark it present.  Replaces the env-var
 * configuration path for unit tests that previously called
 * setenv("SUD_OVERLAY", ...) / setenv("SUD_INRAMFS", ...) before
 * sud_overlay_init() / sud_inramfs_init().  Safe to call repeatedly
 * across sub-tests; old interned strings leak (process-lifetime). */
void sud_runtime_config_test_install(const struct sud_runtime_config *src);

/* Test helper: clear the live config and mark it absent.  Symmetric
 * to sud_runtime_config_test_install; lets a teardown step return
 * the global to its pre-test state. */
void sud_runtime_config_test_clear(void);

/* The live runtime config populated by sud/wrapper.c::main before
 * sud_addins_wrapper_init() runs.  Each addin's wrapper_init reads
 * its slice of the configuration from here.  When the wrapper has
 * not run (e.g. unit-test harnesses that exercise an addin
 * directly), g_sud_runtime_config_present is 0 and addins fall back
 * to a no-config behaviour (legacy env-var reads, transitional). */
extern struct sud_runtime_config g_sud_runtime_config;
extern int                       g_sud_runtime_config_present;

#endif /* SUD_RUNTIME_CONFIG_H */
