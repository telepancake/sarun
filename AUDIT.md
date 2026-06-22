# sarun audit — open issues

Status of the earlier adversarial audit, reconciled against the current code.
Findings verified in the source, not from comments. Fixed and deliberate items
are recorded so they aren't re-flagged; the rest are genuinely open.

## Fixed

- **C2 (ancestor symlink escape on apply).** `review.rs` materialize now resolves
  every parent component with `O_NOFOLLOW` via `hostfs::parent_beneath`, so a
  box-planted `etc -> /` can no longer redirect a write onto the host. (The
  truncate-in-place residue is still open — see C2' below.)
- **H1 (fork/exec malloc deadlock).** `sarun_close_stray_fds` was removed: it
  only closed `FD_CLOEXEC` fds, which the kernel already closes on `execve`, so
  it was redundant; its `read_dir`/`format!`/`Vec` allocations ran between
  `fork()` and `execve()` and could deadlock on the malloc lock. The false
  "allocate nothing / async-signal-safe" comment is gone with it.
- **H2 (silent wrong-directory in find/xargs).** The `unshare(CLONE_FS)` hack is
  gone; `find`/`xargs` resolve against the shell's logical cwd
  (`Dependencies::cwd`). No fallback, no silent wrong directory.
- **H5 (OCI digest path traversal).** `read_blob_by_digest` now rejects digest
  components containing `/`, `\`, NUL, `.`, or `..` before joining them onto the
  blob path, and no longer returns unverified bytes for non-sha256 algorithms.
- **L1 (statfs NUL panic).** `statfs` returns `EINVAL` instead of `unwrap()`-ing
  a `CString::new` failure on an interior-NUL path.
- **T2 (stale "rename is ENOSYS").** The overlay header comment matched an
  unimplemented state; rename is implemented, the comment is corrected.
- **Net datapath for Tap boxes (was not in the original audit).** A regular `-n`
  box reached the network through the engine's MITM proxy + synthetic DNS, but
  the overlay shadowed `/etc/resolv.conf` and the CA bundle only for `--api`
  boxes — so every non-oaita Tap box failed DNS (`getent` exit 2) and HTTPS
  (untrusted MITM cert). Both shadows now fire for any Tap box (`is_tap`).
- **Test binary paths (part of T4).** `pjdfstest_rs` pointed `BIN` at a
  non-existent `prototype/engine/...` path; `struct_rs`/`workloads_rs`/`ui_rs`
  pointed at `release/sarun-engine` (the binary is `sarun`) and `ui_rs` drove a
  phantom `sarun-ui`. All never found a binary, so they ran `make engine` every
  time and then fake-green-skipped or timed out. Fixed to drive `sarun`; they now
  test for real.

## Deliberate (not a bug; now documented)

- **C1 (capture DB `synchronous=OFF`).** This sqlar holds a box's captured writes
  in escrow; the host is never touched until an explicit apply, so an OS crash
  can only lose or corrupt an in-progress, re-runnable box — never host data.
  `OFF` avoids an fsync per write on the high-volume capture path. WAL/NORMAL
  would add fsync latency and `-wal`/`-shm` side files for durability this store
  does not need. Justified in a comment at `capture.rs`. (If durability of a
  finished, kept-but-unapplied box ever matters, the fix is one fsync at box
  finalization, not per-txn `synchronous=NORMAL`.) M5 (`overlay.rs` dropped
  `sync_all`) is the same tradeoff.

## Open — correctness / data path

- **C2' (`review.rs` materialize).** `std::fs::write` truncates in place: an error
  mid-write destroys prior content, and there is no temp-then-rename. (Ancestor
  symlink escape is fixed; this atomicity gap is not.)
- **C3 (`review.rs` apply).** Multi-path apply has no atomicity and no rollback:
  materialize-then-consume per path, so an error at path N leaves 1..N-1 on the
  host and consumed, N.. pending. No "nothing happened" outcome.
- **H3 (`review.rs`/`control.rs`).** `apply`/`discard` have no running-box guard
  (unlike `dissolve`): they read `blob_path(id,rowid)` — the file the live FUSE
  write is mid-`write_at` on — and can stamp a torn blob onto the host.
- **H4 (`review.rs` metadata restore).** Mode is applied at create
  (`hostfs::write_file_at(..., mode)`); owner and xattr restore drop their
  results. The owner drop is legitimately best-effort (lchown EPERMs unprivileged),
  but a failed `setxattr` is silently lost.
- **M1.** Apply has no clobber/staleness check: a host file changed between
  capture and apply is silently overwritten (the `stale` flag is UI-advisory).
- **M2.** Single-instance guard is TOCTOU (`connect().is_ok()` probe, then a
  later `remove_file`+`bind`): two engines on one `XDG_RUNTIME_DIR` can race.
- **M3.** `brush.rs` CoreutilWrapper's process-global `dup2(fd 1/2)` is narrowed
  by `spawned_pipeline_stage` but still races non-pipeline sibling threads.
- **M4.** `control.rs` uses `state.lock().unwrap()` with no `catch_unwind`: one
  panic under the lock poisons it → permanent control-plane outage.
- **M6.** Passthrough/`-d` write swallows `create_dir_all` errors then opens for
  write; a partial host dir tree can be left on a write the box thinks failed.

## Open — robustness

- **L2.** Apply path-shape safety rests on FUSE rows not containing `..`; an OCI/
  tar import that admits `..` would reopen host traversal.
- **L3 (`frames.rs`).** The u32 frame length is trusted with no cap; a box can
  grow a per-channel buffer toward 4 GiB.
- **L4.** `libc::write`/`read` return values are ignored on some echo/PTY/frame
  paths; a short write silently truncates output or desyncs the frame stream.
- **L5 (`control.rs`).** `shutdown` SIGTERMs self and returns ok; in-flight
  apply/register on other threads race teardown.
- **T1.** `(code & 0xff)` + `.code().unwrap_or(...)` collapse signal deaths
  (SIGSEGV/SIGKILL) into bogus exit codes in find/xargs/brush/runner.

## Open — tests

- **T4 (fake-green skips).** Beyond the binary-path fixes above, most `*_rs.py`
  still `return 0` ("PASS (skipped)") when the binary can't be built. In this
  container the binary always builds, so the skip only ever masks breakage; it
  should fail loud instead.
- **H6 (inert test files).** `test_symlink_escape.py`, `test_write_path_contract.py`,
  `test_chmod_readonly.py`, `test_changes_view_incremental.py`,
  `test_table_reconcile.py`, `test_rpane_scroll.py`, `test_ui_smoke.py` expose no
  `def test_*`, so `make test` collects nothing from them. They test the Python
  prototype overlay/UI; their engine-relevant coverage (the symlink-escape guard
  especially, given C2) needs a Rust-engine equivalent.
- **`make test` has no `--timeout`.** A hanging test hangs the suite forever
  (pytest-timeout is installed but never passed `--timeout`).
- **Brush coreutil gate gap.** `test_make_rs`/`test_n2_rs`/`test_brush_link_rs`/
  `test_brush_nested_sh_rs` assert `cp` runs as an in-process builtin, but `cp`
  has no `brush_gates.rs` gate (gates default to forking the host binary). The
  recipes run correctly via external `cp`; the in-process assertions are ahead of
  the implementation.

## Open — tech debt

- **T5.** `engine/vendor/findutils/Cargo.lock` is gitignored but regenerated; it
  recurs as an accidental artifact and blocks rebases.
