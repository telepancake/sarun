# sarun audit — open issues

Status of the adversarial audit, reconciled against the current code. Fixed and
deliberate items are recorded so they aren't re-flagged; the rest are open.

## Verification status (read first)

Run-verified end-to-end: `make test` is **23 passed / 14 skipped / 3 brush-gap**
(`test_make_rs`, `test_brush_link_rs`, `test_brush_nested_sh_rs` — see below),
`make test-oci` PASS, and 99 engine unit tests pass. The fixes below are live,
not just compile-checked.

Fixing M1 surfaced a regression it introduced (it gated *deletions* — see Fixed);
caught and fixed by `test_engine_rs` / `test_nested_apply_rs` once the suite was
actually run.

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
- **M1** apply refuses a host file newer than capture — but only for *content*
  rows. The first cut also gated deletions (a tombstone row carries `mtime=0`, so
  any live host file is "newer" and every unlink was refused);
  `host_changed_since_capture` now skips tombstone (`S_IFCHR`) rows. **M2**
  single-instance guard is an exclusive `flock`, not a TOCTOU connect-probe.
  **M4** a `lock()` helper recovers a poisoned mutex and connection handlers run
  under `catch_unwind`.
- **M6** passthrough/`-d` writes (`overlay.rs`) surface `create_dir_all` and
  `open` errors as the real errno instead of swallowing them and returning a
  blanket `EACCES`.
- **L2** apply path-shape safety no longer rests on FUSE rows lacking `..`:
  `hostfs::safe_components` unconditionally rejects `..`/`.`/empty/interior-NUL
  for every host mutation (unit test `rejects_dotdot_components`), so an
  OCI/tar import admitting `..` cannot reopen host traversal.
- **CLI rename** `sarun NAME rename NEW` now echoes the new name on success
  (it silently succeeded before), matching `apply`/`patch`.
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
- **L5** `shutdown` SIGTERMs self and returns ok; in-flight apply/register on
  other threads race teardown. The dangerous case (a torn host file) is already
  prevented by C2′ (apply writes a temp + atomic rename, so a mid-apply SIGTERM
  leaves a stray temp, never a corrupt target); the residual race only fails an
  in-flight `register` during an explicit shutdown. A clean drain barrier is a
  larger change, deferred.

## Brush (deferred to a focused session)

- **gate_cp** behavioral faithfulness is unverified; `test_make_rs`,
  `test_brush_link_rs`, `test_brush_nested_sh_rs` still fail (they assert
  coreutils run as in-process brush builtins).
- **M3** brush's process-global `dup2(fd 1/2)` races sibling threads — documented
  in `brush.rs`, not fixed (deep concurrency change).
- One dead-code warning remains by design: `brush.rs::box_builtins` (the
  coreutils-gating wrapper) is unused pending the gate work.

## Tech debt

- **T5** `engine/vendor/findutils/Cargo.lock` is a regenerated artifact of a
  workspace-member crate; the `.gitignore` entry is the correct handling (there's
  no lock to commit). No change needed.
