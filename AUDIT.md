# sarun audit — open issues

Status of the adversarial audit, reconciled against the current code. Fixed and
deliberate items are recorded so they aren't re-flagged; the rest are open.

## Verification status (read first)

Most of the fixes below change box-spawning / host-mutating paths. In this
container the test harness suppresses stdout/exit for commands that spawn
namespaces + a FUSE mount, so they are **compile-verified and unit-test-verified
(102 engine unit tests pass, incl. the `hostfs` symlink-safety set), but NOT
run-verified end-to-end.** Before trusting the apply/net/brush behavior, run
`make test` (and `make test-oci`) in an environment where box output isn't
swallowed. The last *observed* full run was 154 passed / 4 brush-gap, before the
prototype removal and these fixes.

## Fixed

- **C2 + C2′ (apply host-escape & in-place truncation).** `review.rs` resolves
  every parent with per-component `O_NOFOLLOW` (`hostfs::parent_beneath`), and
  `materialize` now writes a sibling temp + `fsync` + atomic `renameat` instead
  of truncating in place — an error mid-write no longer destroys prior content.
- **H1 (fork/exec malloc deadlock).** The redundant `sarun_close_stray_fds`
  pre-exec hook (and its false "allocate nothing" comment) is gone; the kernel
  already closes `FD_CLOEXEC` fds on exec.
- **H2 (silent wrong-directory).** `find`/`xargs` resolve against the shell's
  logical cwd; the `unshare(CLONE_FS)` hack is gone.
- **H3 (apply on a running box).** `apply`/`discard` now refuse a still-running
  box (a `box_is_running` guard mirroring `dissolve`), so they can't stamp a torn
  blob the live FUSE write is mid-`write_at` on.
- **H4 (metadata restore).** xattr and mode-restore failures now propagate;
  owner stays best-effort (lchown EPERMs unprivileged) with a comment.
- **H5 (OCI digest path traversal).** `read_blob_by_digest` rejects digest
  components with `/`/`\`/NUL/`.`/`..` and no longer returns unverified bytes for
  non-sha256 algorithms.
- **H6 (adversarial coverage).** `prototype/test_symlink_escape_rs.py` is a new
  Rust-engine test that apply must not follow a box-planted ancestor symlink onto
  the host (the C2 class). The old inert Python guards were deleted with the
  prototype; this one is collected by `make test`.
- **M1** apply refuses a host file newer than capture. **M2** single-instance
  guard is an exclusive `flock`, not a TOCTOU connect-probe. **M4** a `lock()`
  helper recovers a poisoned mutex and connection handlers run under
  `catch_unwind`.
- **L1** statfs returns EINVAL instead of panicking on an interior-NUL path.
  **L3** the u32 frame length is capped. **L4** short `read`/`write` returns on
  the runner's echo/PTY/frame paths are handled.
- **T1 (signal exit codes).** find/xargs/brush report 128+signo for signal deaths
  instead of a fabricated code. **T2** the stale "rename is ENOSYS" comment is
  gone (rename is implemented). **T3** CLAUDE.md test info corrected.
- **net silent failures.** The per-box stack's error-swallowing sites
  (DHCP/DNS/TCP/route/MITM/CA-load/keylog/tap) now log or propagate; legitimate
  fire-and-forget keeps a one-line note.
- **net datapath for Tap boxes.** DNS + HTTPS-MITM worked only for `--api` boxes;
  the resolv.conf + CA-bundle shadows now fire for any Tap box (`is_tap`).
- **brush `cp` gate.** `gate_cp` runs simple GNU-faithful `cp` argvs in-process
  (the 4 brush tests); `brush.rs` no longer empties a provenance record on a JSON
  error. *(Gate faithfulness is behaviorally unverified here — see top.)*

## Deliberate (not a bug)

- **C1 (capture DB `synchronous=OFF`).** The sqlar holds a box's writes in escrow;
  the host is untouched until apply, so a crash can only lose a re-runnable box,
  never host data. `OFF` keeps the per-write fsync off the high-volume capture
  path; WAL/NORMAL would add latency + side files for durability this store
  doesn't need. Justified in `capture.rs`. (Same rationale: `overlay.rs` M5.)

## Open

- **C3** multi-path apply still has no atomicity/rollback across paths (a
  documented `TODO` in review.rs) — if path N errors, 1..N-1 are already on the
  host. A real transaction is a larger redesign.
- **M3** brush's process-global `dup2(fd 1/2)` races sibling threads — documented
  in `brush.rs`, not fixed (deep concurrency change).
- **M6** passthrough/`-d` write swallows `create_dir_all` errors then opens for
  write (`overlay.rs`) — can leave a partial host dir tree.
- **L2** apply path-shape safety rests on FUSE rows lacking `..`; an OCI/tar
  import admitting `..` would reopen host traversal.
- **L5** `shutdown` SIGTERMs self and returns ok; in-flight apply/register on
  other threads race teardown.

## Tech debt

- **T5** `engine/vendor/findutils/Cargo.lock` is a regenerated artifact of a
  workspace-member crate; the `.gitignore` entry is the correct handling (there's
  no lock to commit). No change needed.
