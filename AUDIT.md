# sarun codebase audit — serious issues and the comments that excuse them

Adversarial audit, four parallel deep passes (overlay/apply data-loss; engine
concurrency/control plane; repo-wide guard-comment hunt; oaita secrets + test
integrity), reconciled by hand. Findings verified against code, not comments.

## Meta (own it first)

- The most dangerous findings sit behind confident `SAFETY` / "refusing to" /
  "best-effort" comments that assert the exact guarantee the code does not
  deliver. That is the anti-pattern flagged: documentation that excuses a bug
  instead of the bug being fixed.
- Two of these are **mine, this session**: the `unshare(CLONE_FS)` "fall back to
  process cwd" comments (H2), and I re-committed `close_stray_fds` (H1) in the
  vendoring reconstruction *with an approving commit message* instead of
  flagging its false safety claim.
- My own keyword-based "guard hunt" sub-pass concluded the anti-pattern was
  "essentially absent / culture is fail-loud." The passes that traced the data
  path found the opposite. The charitable keyword read is itself the failure
  mode — its conclusion is discarded here.

---

## CRITICAL — data loss / durability

| ID | Location | Issue |
|----|----------|-------|
| C1 | `capture.rs:365` | Capture DB opens `journal_mode=DELETE; synchronous=OFF`. The *only* store holding a box's captured work is **not crash-durable** — an OS crash/power-loss after a write is "captured" can leave the sqlar **corrupt**, not just missing its last txn. Blobs are never `fsync`'d. No comment justifies the `OFF`. |
| C2 | `review.rs:485–533` | Apply/materialize **writes through symlinked parent directories**. The guard only checks the *final* component. Apply order is `ORDER BY name`, so a box capturing `etc -> /` then `etc/evil` materializes the symlink first, then writes `etc/evil` **onto `/evil` on the real host**. Comment: *"refusing to write through a symlink"* — false for ancestors. Also `std::fs::write` truncate-in-place (no temp+rename, no fsync): an error mid-write destroys prior content. |
| C3 | `review.rs:550–588` | Multi-path apply has **no atomicity, no rollback**. Materialize-then-consume per path; if path 3/10 errors, 1–2 are on the real FS and consumed, 3–10 pending. A coherent change set can end half-applied with no "nothing happened" outcome. |

## HIGH — correctness

| ID | Location | Issue |
|----|----------|-------|
| H1 | `brush-core/src/commands.rs` `sarun_close_stray_fds` | SAFETY comment claims *"allocate nothing … async-signal-safe."* The closure runs `read_dir` + `Vec::with_capacity` + `into_string` + `read_link(format!())` **between `fork()` and `execve()`** in a multithreaded process → child can **deadlock on the malloc lock before exec**. Fires for every external command a box forks. *(Re-blessed by me in the reconstruction.)* |
| H2 | `find_builtin.rs:131`, `xargs_builtin.rs:132` | `unshare(CLONE_FS)` failure → `find .` walks the **engine daemon's `$HOME`**, `xargs` children run there, **exit 0, no error**. Trips for every find/xargs under any seccomp profile filtering `unshare`. Comment *"fall back to the process cwd"* hides a silent wrong-directory bug. *(Mine, this session.)* |
| H3 | `review.rs` / `control.rs:375,1163` | `apply`/`discard` have **no running-box guard** (unlike `dissolve`). Apply reads `blob_path(id,rowid)` — the same file the live FUSE `write` is mid-`write_at` on — and can stamp a **torn blob** onto the real host. Comment *"apply() operates on a stopped box's archive"* is unenforced. |
| H4 | `review.rs:453–483` | Metadata restore is fire-and-forget: every `utimensat`/`lchown`/`lsetxattr`/`set_permissions` result dropped. "best-effort" is fairly scoped to *owner* but silently blankets **mode** — a `0600` secret can land world-readable, apply reports success. |
| H5 | `oci.rs:1074` `read_blob_by_digest` | **Path traversal**: a manifest-supplied digest string is `join`ed onto a host path **without** the `..`/absolute guard the layer-ingest path (`oci.rs:1226`) correctly applies. A crafted/pulled image can read a host file outside the blob store. |
| H6 | `prototype/test_symlink_escape.py`, `test_write_path_contract.py`, `test_chmod_readonly.py`, `test_changes_view_incremental.py`, `test_table_reconcile.py`, `test_rpane_scroll.py`, `test_ui_smoke.py`, `test_pty_ui_rs.py` | **8 assertion-bearing test files are silently NOT RUN by `make test`** — verified: `pytest --collect-only` yields 0 items from each (all logic under `__main__`, no `def test_*`), and no Makefile target invokes them as scripts. Most damning: `test_symlink_escape.py` is an *adversarial test that exists to prove the daemon never follows a sandbox-planted symlink onto a host target* — the **C2 bug class** — and it runs **never**. (These are prototype tests, so they wouldn't have caught the Rust-engine C2 either; the Rust port has no symlink-escape test at all. Double gap: guard not run, production path never covered.) `conftest.py`'s false-green protection can't help — no collected item ⇒ the hook never fires. |

## MEDIUM

| ID | Location | Issue |
|----|----------|-------|
| M1 | `review.rs` | No clobber/staleness check on apply (the `stale` flag is UI-advisory only): a real file changed between capture and apply is silently overwritten. A directory tombstone → `remove_dir_all` on the live host path with no scope check. |
| M2 | `main.rs:86` + `control.rs` | Single-instance guard is TOCTOU: `connect().is_ok()` probe, then much later `remove_file`+`bind`. Two engines on the same `XDG_RUNTIME_DIR` can steal the socket and corrupt each other's mount/overlay. |
| M3 | `brush.rs:167` | CoreutilWrapper's process-global `dup2(fd 1/2)` is narrowed by `spawned_pipeline_stage` but still races *non-pipeline* sibling threads (FUSE serve, ECHO reader, find/xargs workers) in the same address space. |
| M4 | `control.rs` (pervasive) | `state.lock().unwrap()` everywhere, no `catch_unwind` around connection handlers. One panic under the lock **poisons it → permanent control-plane outage** while the process keeps running. |
| M5 | `overlay.rs:1981` | `let _ = f.sync_all()` — an in-box program's successful `fsync()` doesn't guarantee the captured copy hit disk (compounds C1). |
| M6 | `overlay.rs:1329` | Passthrough/`-d direct` swallows `create_dir_all` errors then opens for write; a partially-created real-host dir tree can be left behind on a write the box thinks failed. |

## LOW

- **L1** `review.rs:516,454`, overlay `statfs` — `CString::new(path).unwrap()` panics the handler thread on an interior-NUL path.
- **L2** `review.rs:535` — apply path-shape safety rests on the *incidental* property that FUSE rows can't contain `..`; a future capture source (OCI/tar import) admitting `..` reopens host traversal.
- **L3** `frames.rs:114` — frame length field trusted (u32); a box dribbling a `0xFFFFFFFF` header grows the per-channel buffer toward 4 GiB. No max-frame cap.
- **L4** `runner.rs:621,968,1186,1196`, `brush.rs:731,783` — `libc::write/read` return values ignored on echo/PTY/frame paths; a short write silently truncates captured output or desyncs the length-prefixed frame stream.
- **L5** `control.rs:434` — `shutdown` SIGTERMs self and returns `ok`; in-flight apply/register on other threads race the teardown (acknowledged in-comment, not mitigated).

## TECH-DEBT / STALE DOC

- **T1** `find_builtin.rs:157`, `xargs_builtin.rs:157`, `brush.rs:297/336`, `runner.rs` — `(code & 0xff)` + `.code().unwrap_or(1/127)` collapse signal deaths (SIGSEGV/SIGKILL) into bogus exit codes; a capture/provenance tool records a lie about *why* a command died.
- **T2** `overlay.rs:11` — module header still says *"rename is ENOSYS for now"*; rename is fully implemented at `overlay.rs:2024`.
- **T3** `CLAUDE.md` — claims `make test` excludes only `test_e2e.py` + `test_pjdfstest.py`; the Makefile also ignores **`test_oci.py`**. `test_pjdfstest.py` docstring still carries a stale `/home/user/venv` example.
- **T4** ~23 `prototype/test_*_rs.py` — self-report `PASS (skipped)` and `return 0` when the Rust binary can't be built; green is **vacuous in a toolchain-less env** (real here because the binary is built).
- **T5** `engine/vendor/findutils/Cargo.lock` — gitignored but regenerated; recurring "accidental artifact" that just blocked the rebase. Never fixed at the root.

## The guard-comments themselves (the load-bearing lies)

1. `commands.rs` close_stray_fds — *"allocate nothing … async-signal-safe"* → allocates + `opendir`/`readlink` (H1).
2. `review.rs` materialize — *"refusing to write through a symlink"* → not for parent dirs (C2).
3. `find_builtin.rs`/`xargs_builtin.rs` — *"fall back to the process cwd"* → silent wrong-directory (H2). **Mine.**
4. `review.rs` metadata — *"best-effort"* (owner) silently covering mode (H4).
5. `overlay.rs:11` — *"rename is ENOSYS for now"* → implemented (T2).

---

## Verified CLEAN (reported honestly, not everything is broken)

- **Secrets — oaita proxy is clean.** The upstream `api_key` lives only in host
  memory (`oaita/config.rs`, `proxy.rs`); the box-visible `oaita.toml` is a
  FUSE-substituted safe file with **no key** (`main.rs:161`, `overlay.rs:423`);
  the `Bearer` header is attached **only host-side** after the UDS crossing
  (`client.rs:202`); box env carries only `OPENAI_BASE_URL` + `SARUN_BROKER`, no
  key; the `api_log` sqlar lives in an engine dir hidden from boxes
  (`is_engine_path`), and the key is never a logged column. TLS is properly
  verified (rustls `RootCertStore`, **no** `danger_accept_invalid_certs`).
- **Discard never touches the real FS** (`review.rs:595`) — verified.
- **Copy-up is genuinely lazy CoW** — the lower/host file is only ever read,
  never written, except the explicitly opt-in `-d`/passthrough rule.
- **Test suite is *mostly* honest, with one material hole (H6).** `conftest.py`
  genuinely converts the non-raising `check()`/`_fails` idiom into real pytest
  failures; broad `except` blocks route into `_fails` or are teardown-only; the
  explicit `make test` exclusions (e2e/pjdfstest/oci) are heavy-but-real suites
  with their own targets, not hidden failures; "141 passed" is a plausible,
  honestly-directioned count (below the 158 collected, the right direction).
  BUT see H6: 8 real test files contribute **zero** collected items and are
  invisible to `make test` — including the adversarial symlink-escape guard.
  So the green number is trustworthy *for what it runs*, and silently blind to
  those 8 files.

---

## Suggested fix order (highest stakes first)

1. **C1** — `synchronous=NORMAL` + WAL; fsync blobs before reporting "captured".
2. **C2** — verify every ancestor (or `O_NOFOLLOW` per component) + write-temp-then-rename in materialize.
3. **H1** — pre-fork fd snapshot or `close_range(2)` over a precomputed keep-set; delete the false SAFETY comment.
4. **H2 / H3** — make `unshare` failure and live-box apply **fail loudly**, not silently.
5. **H5** — apply the ingest-path `..`/absolute guard to `read_blob_by_digest`.
6. **H6** — cheap, high-value: add the missing `def test_*` pytest entry to the 8
   inert files (the other `*_rs.py` already do this), so `make test` actually
   runs them; and write a Rust-engine symlink-escape test that exercises C2.
7. Then H4, the MEDIUMs, and the stale-doc cleanups (T2/T3/T5).
