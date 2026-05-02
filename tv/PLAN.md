Goal

Reshape sud's addin stack into two clean layers and eliminate environment-variable passing:



Layer split: path_remap becomes the only layer that understands paths (resolution, dirfd, cwd, mount overlays, CoW, fakeroot, "place this path into inramfs"). inramfs becomes a pure inode/data store with no notion of pathnames at all.

Argv-only configuration: every value currently smuggled through SUD_* env vars is promoted to a sud32/sud64 command-line flag, and the same flags are re-emitted onto every child wrapper invocation by a single, shared argv-builder.



Part 1 — Re-layering inramfs and path_remap

Current overlap to dissolve

Both layers presently do path work:



sud/inramfs/addin.c contains absolutise() (dirfd + CWD + relpath join), sud_inramfs_resolve_at(), sud_inramfs_path_under_mount(), a dirfd→absolute-path table (g_fdtab[].dir_path), and a logical-CWD shadow (g_logical_cwd + SUD_INRAMFS_CWD env).

sud/inramfs/super.c parses SUD_INRAMFS=<mount>:<size_mb> and stores g_mount_path / g_mount_len, which is intrinsically a path concern.

sud/path_remap/addin.c and overlay.c independently absolutise paths, walk /proc/self/cwd, remember dirfd→merged-path mappings for synthetic dirs, and parse SUD_OVERLAY / SUD_REMAP.


Target architecture

inramfs — pure inode/data layer, addressed by inode index (uint32_t).



Header (inramfs.h) exposes only the data primitives: op_open_inode, op_read, op_write, op_pread, op_pwrite, op_lseek, op_truncate_inode, op_fstat_inode, op_chmod_inode, op_chown_inode, op_utimens_inode, op_get_kfd_inode, op_mmap, op_dup*, op_fcntl_*, op_close, plus the inode-namespace primitives needed by callers that build paths on top: inode_alloc, inode_link_into_dir, inode_unlink_from_dir, inode_dir_lookup, inode_readdir, inode_create_symlink, inode_readlink, inode_rename_in_dir. These already exist as sud_ir_dir_* / sud_ir_inode_* in internal.h — promote the relevant subset to a public API.

Move out of inramfs: absolutise, cwd_seed_from_env, cwd_publish_to_env, read_cwd_abs, g_logical_cwd, sud_inramfs_resolve_at, sud_inramfs_path_under_mount, the mount prefix (g_mount_path, g_mount_len, parse_env's mount parsing), execve_inject_cwd_env, the dir_path field of sud_ir_open_file, and the inramfs pre_syscall path-bearing hooks.

Keep in inramfs: futex superblock, inode table, small/large allocator, dirent blocks, per-inode locks, fd table keyed on memfd kfd, fd-bearing pre_syscall hooks (read/write/lseek/dup/...), sud_inramfs_owns_fd, the --inramfs-key / --inramfs-meta-mb runtime parameters (no path).

sud_inramfs_active() becomes "is the data store attached?", independent of any mount path.


path_remap — the only path-aware layer.



Becomes the universal (dirfd, relpath, cwd, intent) → action resolver. Rules form an ordered list; each rule has a match prefix, a kind, and parameters. Unified rule kinds:

passthrough — leave the syscall untouched.

remap — rewrite the path arg (today's SUD_REMAP).

overlay — the existing CoW/whiteout/merged-dir behaviour (today's SUD_OVERLAY).

inramfs — "this prefix is served from the inramfs inode store"; the rule carries an inramfs root inode (allocated lazily on first hit) and the resolver returns an (inramfs, inode_idx, residual) ticket instead of a rewritten kernel path. The path_remap dispatcher then calls inramfs's inode-level ops directly.

fakeroot (new, optional first cut) — passthrough for path resolution but tags the ticket with uid/gid override metadata that the dispatcher applies to stat/chown short-circuits. The plumbing for this layer falls naturally out of having a single resolver that returns a structured ticket rather than a string.



Owns all path machinery currently duplicated: absolutising (dirfd, path), the dirfd→logical-path table (covering both inramfs dirfds and overlay synthetic dirs), the logical CWD shadow, /proc/self/cwd reads, and the chdir/getcwd/fchdir interception that today lives in inramfs.

The dispatcher in path_remap/addin.c becomes the single place that switches on syscall number and routes the ticket to either: kernel passthrough with rewritten path, an EROFS/ENOENT short-circuit, an overlay-side helper (whiteout creation, merged-dir synthesis), or an inramfs inode-level op.


Module / file shape after the change


sud/path_remap/ grows: rename overlay.{c,h} to path.{c,h} (or resolve.{c,h}) with overlay logic as one rule kind; add cwd.c (logical CWD + /proc/self/cwd cache); add dirfd.c (process-local dirfd→logical-path map, replacing both the inramfs dir_path field and overlay's synthetic-dir tracking); add inramfs_glue.c that calls into the new inode-level inramfs API for tickets of kind inramfs.

sud/inramfs/: shrink. addin.c keeps only the fd-bearing hooks; super.c drops SUD_INRAMFS mount parsing and instead takes its key/size from cmdline-driven config (see Part 2). Drop internal.h's mount accessors. The execve_inject_cwd_env block disappears entirely (CWD inheritance is now an argv flag — see Part 2).

sud/addin.c: dispatch order becomes trace → path_remap → inramfs. inramfs now sees only fd-bearing syscalls that path_remap has already routed to it (path_remap calls sud_inramfs_op_*_inode directly inside the same pre_syscall, so the inramfs addin's pre_syscall shrinks to fd ops only).

Tests: sud/path_remap/tests/ gains coverage for the new inramfs rule kind (path-prefix → inode-store routing, plus dirfd-relative resolution into inramfs). sud/inramfs/tests/ is rewritten to drive the inode/data API directly without configuring any mount path.


Data-flow contract for a path-bearing syscall (e.g. openat)


Trace records the call (program-visible args).

Path_remap calls resolve_at(dirfd, path, intent) which produces one of: PASSTHROUGH, REWRITE(new_kernel_path), INRAMFS(inode_idx, residual, intent), WHITEOUT, READONLY, MERGED_DIR(rule).

Path_remap acts on the ticket: rewrite args[], short-circuit with -errno, materialise a synthetic merged dir, or call into sud_inramfs_op_open_inode(inode_idx, residual, flags, mode).

Inramfs (now bypassed for path lookups) only runs its fd-bearing hooks for follow-up read/write/etc on the kfd path_remap returned.


This kills every pathname concept inside sud/inramfs/ and gives path_remap a single, expressive resolver capable of expressing "remap A→B", "overlay merged from upper+lowers", "this prefix lives in inramfs", and "this prefix is fakeroot-owned" as orthogonal rule kinds.



Part 2 — Eliminate environment variables, use argv only

Inventory of env vars to retire

Variable	Currently produced by	Currently consumed by	New argv flag on sud32/sud64
SUD_INRAMFS=<path>:<mb>	user / sudtrace	sud/inramfs/super.c::parse_env	(path part) --remap-rule inramfs:<path>:<key> (consumed by path_remap), (size part) --inramfs-meta-mb <N> (consumed by inramfs)
SUD_INRAMFS_KEY	sudtrace ir_setup_owned_shm	sud/inramfs/super.c	--inramfs-key <key>
SUD_INRAMFS_CWD	inramfs cwd_publish_to_env + execve env injection	inramfs cwd_seed_from_env	--cwd <abs_path> (consumed by path_remap, the new CWD owner)
SUD_OVERLAY	user / sudtrace	sud/path_remap/overlay.c	--remap-rule overlay:<merged>=<upper>+<lower>... (repeatable)
SUD_REMAP	user / sudtrace	sud/path_remap/overlay.c	--remap-rule remap:<src>=<dst> (repeatable)
SUDTRACE_OUTFILE	sudtrace main	sud/trace/addin.c::trace_wrapper_init (fallback when SUD_OUTPUT_FD isn't valid)	--trace-outfile <abs_path>

The existing --no-env and --drop-argv N flags stay; the new flags slot into the same wrapper-flag block.


Wrapper CLI shape

Code
sud{32,64} [--no-env]
           [--drop-argv N]
           [--cwd <abs>]
           [--trace-outfile <abs>]
           [--inramfs-key <key>] [--inramfs-meta-mb <N>]
           [--remap-rule <kind>:<spec>]   (repeatable)
           /path/to/binary [args...]

All flags are positional and order-independent within the leading flag block; parsing terminates at the first non-flag argument (the target binary). No = syntax, no short forms — keeps the wrapper parser dead simple.


Single argv-rewrite pipeline

Today three places construct wrapper argv: sudtrace.c::build_wrapper_argv (parent), sud/elf.c::build_exec_argv (handler-side execve interception), and the ad-hoc execve_inject_cwd_env in inramfs (which manipulates envp). The third disappears. The first two converge on a single shared helper, sud_wrapper_argv_build(), that takes a struct sud_wrapper_config (no_env, drop_count, cwd, trace_outfile, inramfs_key, inramfs_meta_mb, vector of remap_rule strings, target_path, target_argv) and emits the canonical flag block. Both sudtrace.c and sud/elf.c call it; the handler-side caller fills the config from the current process's live state (path_remap rule list, current logical CWD, inramfs key from super.c, trace outfile from trace addin) so the child re-creates the same configuration without consulting the environment.


Where each flag is read


--cwd, --trace-outfile, --remap-rule, --inramfs-key, --inramfs-meta-mb are parsed by sud/wrapper.c::main before sud_addins_wrapper_init() is called. The parser stashes them on a small global config struct (e.g. g_sud_runtime_config) that addins read in their wrapper_init hook instead of calling getenv.

path_remap reads cwd and the remap-rule list to populate its rule table; inramfs reads inramfs-key and inramfs-meta-mb to attach the data store; trace reads trace-outfile to open its output.

The remap-rule string lets path_remap reconstruct the inramfs prefix without inramfs ever holding it (Part 1's invariant).


Re-emission to children

When path_remap's execve interception (or sud/elf.c::build_exec_argv for the wrapper-rewrite path) constructs the child's argv, it asks each addin: addin->describe_runtime_flags(config_out) and serialises the resulting struct via the shared builder. The result is that:



Every child wrapper invocation gets a complete, self-describing flag set.

envp is passed through to the kernel byte-for-byte (the child program sees exactly the env the traced program supplied to execve).

The "env as a dumping ground" pattern is gone: there is no code path in sud/ that calls setenv or unsetenv, and the only getenv calls remaining are PATH (legitimate user-program lookup in sudtrace.c::build_wrapper_argv and sud/wrapper.c::init_path_env) and any TMPDIR-style queries on behalf of the traced program itself.


Behavioural compatibility


sudtrace.c continues to accept -o FILE and --no-env from the user; it now translates these (plus auto-discovered config like the minted inramfs key, plus user-provided --remap / --overlay / --inramfs arguments — which can be exposed as new sudtrace flags) into the wrapper's argv flag block instead of setenv calls.

The SUD_OUTPUT_FD / SUD_STATE_FD fixed-fd convention is unaffected (those are fds, not env).

Tests under sud/path_remap/tests/ and sud/inramfs/tests/ switch from setenv("SUD_OVERLAY", ...) to a direct sud_path_remap_configure_for_testing(rules, cwd, ...) style API; this is a small, mechanical churn.


Migration order (single PR or staged)


Land the new sud_wrapper_config struct, the shared sud_wrapper_argv_build() helper, and wrapper.c's flag parser, with both env-var and flag inputs accepted (flag wins). All addins keep their getenv fallback for one step.

Update sudtrace.c and sud/elf.c::build_exec_argv to emit the new flags.

Migrate each addin's wrapper_init to read from the config struct; delete the corresponding getenv call and any sibling setenv/unsetenv.

Delete execve_inject_cwd_env and the inramfs CWD/path machinery as part of Part 1's relocation into path_remap.

Remove the env-var fallbacks; tests now configure via API.


After step 5, a grep -rn 'getenv\|setenv\|unsetenv' over sud/ returns only PATH-lookup sites (and TMPDIR if needed by the traced-program emulation), confirming the env-as-dumping-ground pattern has been eliminated.



Net result


sud/inramfs/ is a self-contained inode/data store, ~30% smaller, with no string path arguments in its public API and no env access.

sud/path_remap/ becomes the universal path layer; its rule model is general enough to express simple remap, overlay (CoW + whiteouts), inramfs placement, and a clean seam for fakeroot semantics.

sud32/sud64 have a single, documented CLI; configuration is fully reproducible from /proc/<pid>/cmdline of any wrapper process; no SUD_* environment variable is read or written anywhere in sud/.



Part 3 — Grow sud/fake-exec/ from MVP into the full elision layer

Status (after commit 5f9b2e5)

The skeleton has shipped:

sud/fake-exec/ exists, registers between path_remap and inramfs, and elides SYS_execve / SYS_execveat for /usr/bin/{true,false,:} (+ /bin/ aliases) by issuing a per-task SYS_exit from inside the SIGSYS handler.  Vfork-safe (raw syscalls only, no heap, no globals).  Trace fidelity preserved — trace addin runs first and records EXEC/EXIT from the program's view.  Wrapper CLI flags --fake-exec off and --fake-exec-deny <basename> propagate through sud_runtime_config_emit so every child wrapper inherits the same behaviour without touching envp.  Verified end-to-end: 200× /usr/bin/true under sudtrace runs in 120 ms vs 232 ms for untraced real true (we skip the helper's ELF load + libc init each iteration).

What is intentionally NOT in the MVP, listed in the order Part 3 should land:


Step A — Track B: full posix_spawn fork-skip with synthetic-child waitpid

Goal: unlock builtins that need richer semantics than the vfork-safe envelope (heap, libc-fs ops, multi-syscall I/O) by faking the clone(CLONE_VM|CLONE_VFORK) itself, not just the child's execve.

Inside a vfork child the only safe operations are raw syscalls and reads of immutable state.  That makes anything beyond write-then-exit impossible there.  The rest of the elision frontier therefore requires returning to the parent's normal handler context, which means synthesising the spawn's wait result.

Concrete plan:


sud/fake-exec/spawn.{c,h}, new files.  Process-wide table keyed by parent tid, with a single per-thread "in-vfork-spawn" slot:

```c
struct fake_exec_thread_state {
    int       in_vfork_spawn;      /* set in parent's clone pre_syscall */
    pid_t     pending_child_tid;   /* tid the kernel returned           */
    int       have_synth_result;   /* set by child rollback             */
    int       synth_status;        /* exit-status word for waitpid      */
    const struct sud_fake_exec_builtin *synth_builtin;
    char     *synth_argv_storage;  /* arena copy; freed on wait reap    */
};
```

Lookup is O(N) over a small fixed-size array indexed by tid; size matches the "in-flight posix_spawns per parent" upper bound (16 is plenty in practice).

Hook points in sud/fake-exec/addin.c::pre_syscall:


SYS_clone / SYS_clone3 with CLONE_VM|CLONE_VFORK in the parent.  Set in_vfork_spawn = 1 on this thread and pass through.  We do NOT skip the kernel clone in this step — see Step F.

SYS_execve in a task whose parent has in_vfork_spawn set (we test this by reading the parent's slot via shared VM, since CLONE_VM means the table is shared).  Two sub-cases:


Builtin is FAKE_EXEC_VFORK_SAFE: existing behaviour, write-then-SYS_exit inline.

Builtin is FAKE_EXEC_RICH (new flag, defined in builtins.h next to FAKE_EXEC_VFORK_SAFE): copy argv into the parent-visible synth slot via a small bump arena that lives in shared VM, mark have_synth_result = 0 and synth_builtin = b, then SYS_exit(0) the empty child immediately.  The kernel's clone return wakes the parent.

SYS_wait4 / SYS_waitid in the parent.  If the wait targets a tid that matches a synth slot whose builtin is RICH and have_synth_result is 0:


Run b->run_rich(argc, argv, envp, &fd_view) in the parent's handler context.  Full address-space isolation here: the emulator may allocate, call libc-fs primitives, do multi-syscall I/O.

Pack the returned status into a struct __wait_status word (WIFEXITED | (status<<8)) and store on the synth slot.  Set have_synth_result = 1.

Set ctx->ret = pending_child_tid, write the status word into the user buffer at args[1], and return 1 to short-circuit the kernel wait.  The kernel-side child has already exited via SYS_exit(0) from the rollback point, so its zombie is already reaped by the time we synthesise — no orphan, no SIGCHLD races.


Test scaffolding: sud/fake-exec/tests/test_fake_exec_spawn.c drives the harness with a mocked clone+vfork sequence (the kernel calls themselves are real; we just verify the state-machine transitions and that the parent's wait sees the synthesised status without the kernel entering execve).  e2e: a tiny posix_spawn driver in tests/ that spawns /usr/bin/true 10000× and asserts no zombies and a measurable speedup over --fake-exec off.

Re-emit through sud_runtime_config_emit: the per-thread state is process-local so nothing changes in the wrapper-flag block.  But add a new --fake-exec-no-rollback escape hatch (parsed and emitted alongside --fake-exec) to disable Step A's Track-B path while keeping Track-A; useful for bisecting trace-fidelity regressions.


Step B — Vfork-safe write-then-exit for echo / printf

Goal: cover the second-most-frequent pure helper in shell scripts without paying for Step A.

Both echo and printf (no %-conversion) are pure functions of argv that emit a single bounded string to stdout and exit 0.  Implementation in sud/fake-exec/builtins.c:


run_inline composes argv joined with single spaces + trailing '\n' into ctx->scratch (PATH_MAX*2 bytes, plenty for any sane invocation; classifier rejects anything larger).

After composition, addin.c emits raw_syscall6(SYS_write, fd, buf, len) once, then raw_syscall6(SYS_exit, 0).  Both calls bypass the SUD handler — and that's a problem for trace fidelity, since the real /usr/bin/echo's write would have hit the trace addin.

Trace-fidelity fix: the addin synthesises the WRITE event itself by calling a small new sud_trace_emit_synthetic_write(fd, buf, len) helper added to sud/trace/addin.h.  The helper takes the same path as a normal post_syscall observation but with caller-supplied args.  The addin emits the synthetic event before the raw write so a reader that snapshots mid-handler still sees consistent ordering.  Test: diff the trace produced by /usr/bin/echo run normally (with --fake-exec off) against the elided run; only timestamps differ.

Classifier additions: in sud/fake-exec/detect.c, accept echo and printf only when


argv has no embedded NUL or non-UTF-8 bytes (ctx->scratch holds a bounded buffer);

stdout is not O_NONBLOCK (we'd have to handle EAGAIN, which means looping in the handler — out of scope);

for printf, the format string has no %-conversion at all (the conservative subset).


Step C — /bin/sh -c <single trivial command>

Goal: catch GNU make's hot path.  make spawns each recipe line as /bin/sh -c <recipe>; a large fraction of recipes are a single command with no shell metacharacters, e.g. /bin/sh -c "true" or /bin/sh -c "/usr/bin/echo done".

Implementation in detect.c:


When path is /bin/sh or /bin/bash and argv == [..., "-c", "<cmd>", ...], run a tiny single-command shell-grammar check on <cmd>: reject anything containing |, &, ;, <, >, $, `, \, *, ?, [, ], (, ), {, }, =, ', ", or whitespace runs that aren't a single SP.  On accept, tokenise into argv and recurse the classifier on the inner command.

If the inner classifier returns INLINE_VFORK_SAFE, replace the outer execve's argv conceptually with the inner one and emit accordingly.  Emit the synthetic EXEC event for the inner binary too (otherwise the trace would show /bin/sh -c "true" and an EXIT, but no EXEC for true — readers expect both).

Test: tests/fake_exec_sh_e2e.sh drives sudtrace -- make -j8 over a fixed autotools project; assert the trace produced with --fake-exec on diffs only on timestamps from a baseline produced with --fake-exec off; assert wall-clock improvement.


Step D — Track-B builtins: test/[, cat, dirname, basename, expr, pwd

Goal: cover the rest of POSIX trivia, all of which need richer semantics than Step B's envelope (file lookups, allocation, multi-syscall I/O).  All ride Step A's Track B.

Per-builtin notes:


test / [ — full POSIX subset (-n / -z / -e / -f / -d / -r / -w / -x / -s / numeric and string comparisons / parenthesised ! / -a / -o).  File-test predicates use sud_overlay_resolve_at + a stat that goes through path_remap so inramfs files and overlay rules are honoured.

cat <single small file> — only when the file resolves into inramfs (sud_inramfs_op_open_inode / sud_inramfs_op_pread) or to a path_remap target whose size is below a threshold (1 MiB; classifier rejects larger).  Larger files go through the kernel because the in-handler memcpy then dominates and we lose the win.

dirname, basename, pwd — pure string ops; trivial.

expr — integer subset (+ - * / % comparisons), no GNU extensions.


Step E — Defaulting fake-exec into the standard SUD_ADDINS list

Once Steps A–C have soaked through tests/sudtrace_test.sh and a clean make -j8 build of an autotools project produces a byte-identical trace under --fake-exec on vs off, flip the Makefile default:

```make
SUD_ADDINS ?= sud/trace sud/path_remap sud/fake-exec sud/inramfs
```

(In the same patch: extend tests/sudtrace_test.sh's matrix to run with fake-exec compiled in by default; remove the explicit SUD_ADDINS=... wrapper from the existing inramfs e2e harness and let it fall through to the new default.)

Add sudtrace user-facing flags so users don't need to know about the wrapper-level CLI:


--no-fake-exec → translates to --fake-exec off on the wrapper argv.

--fake-exec-deny <basename> → forwards verbatim (repeatable).

Update the usage block in sud/sudtrace.c::usage().


Step F — Skip the kernel clone(CLONE_VFORK) too (stretch)

Goal: remove even the vfork itself from the syscall stream when the spawn is provably trivial.

This needs information sud doesn't have at the clone site — the child's execve hasn't happened yet, so we don't know what it's about to run.  Two avenues:


Hook glibc's posix_spawn symbol (intercept at the libc-level rather than at the syscall level).  glibc's __spawni stages the binary path / argv / file_actions on the parent's stack before clone; we read them, classify, and on hit synthesise the wait result without entering clone at all.  This is outside the SUD model — it requires loading a small interposer .so (or intercepting via the dynamic linker's PLT) and is a bigger architectural change than Steps A–E combined.

Speculative clone-elision: skip the clone, run the child code as a userspace coroutine on the parent's alt-stack until we observe the execve, then commit (synthesise wait) or roll back (actually issue the kernel clone and replay).  Rollback is hard to make watertight — most non-trivial child code mutates state — so this only ever works for an even smaller, pre-classified set of spawn helpers.

Both are deferred indefinitely.  The combined Steps A–E already capture the big-O win (we never pay for the helper's ELF load + libc init); Step F is at most a constant-factor improvement on top.


Step G — Performance harness + regression gate

Goal: make sure the gains don't silently regress.

Add tests/fake_exec_perf.sh:


Builds a small fixture (5 000 sh -c true loop, plus the autotools fixture from Step C).

Runs each fixture three times each with --fake-exec on and --fake-exec off, records median wall-clock.

Asserts the on/off ratio stays below a checked-in threshold (initially 0.6; tighten once Step A lands).

Skipped automatically when /proc/sys/kernel/sched_autogroup_enabled or other timing-fragile knobs differ from CI's reference.


Test inventory after Part 3


sud/fake-exec/tests/test_fake_exec.c — classifier + builtin registry (already shipped, 12 cases).

sud/fake-exec/tests/test_fake_exec_spawn.c — Track-B state machine + waitpid synthesis (Step A).

sud/fake-exec/tests/test_fake_exec_builtins.c — per-builtin parity vs the real binary, table-driven, one row per builtin (Steps B + D).

sud/fake-exec/tests/test_fake_exec_sh_grammar.c — single-command shell-grammar check (Step C).

tests/fake_exec_e2e.sh — sudtrace + tv dump assertions: trace fidelity (every elided invocation produces the same EXEC/EXIT events as a real run) (Steps A + B + C).

tests/fake_exec_perf.sh — perf regression gate (Step G).


Net result after Part 3


Every "trivial" exec in a typical autotools / make workload is elided in userspace.  ELF load + libc init + the helper's own runtime are gone for true / false / : / echo / printf / dirname / basename / pwd / expr / test / [ / small cat / single-token /bin/sh -c.

posix_spawn is still observed by the kernel (we issue the clone) but the child never enters the kernel's exec path — the entire helper invocation costs one clone + one exit.

Trace bytes are byte-identical to the unoptimised run (timestamps aside), so every existing tv panel, every SQL query, every downstream consumer keeps working without modification.

Wrapper CLI surface remains tiny: --fake-exec off, --fake-exec-deny <basename>, --fake-exec-no-rollback (Step A).  Configuration round-trips through sud_runtime_config_emit so children inherit it from /proc/<pid>/cmdline alone — no envp dumping ground, matching Part 2's invariant.
