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
