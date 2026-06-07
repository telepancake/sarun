#!/usr/bin/env python3
"""New coverage tests added by acceptance review.

Tests added:
  1. Index case-sensitive subtree isolation (prune_subtree / reparent)
  2. NetworkPolicy approval_request — TIMEOUT, DENY, ALLOW-PERMANENT
  3. File-rule passthrough dispatch (engine-level via MountFixture)
  4. ChangeReview apply_hunk / discard_hunk (unit-level)
  5. e2e box rename (inline, no UI process needed)
  6. HTTPS/CONNECT through proxy — skipped (requires real TLS plumbing)

Run with:
    /home/user/venv/bin/python -m pytest test_new_coverage.py -q
"""
import asyncio
import os
import shutil
import stat as stat_mod
import subprocess
import sys
import tempfile
from pathlib import Path
from importlib.machinery import SourceFileLoader

m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()

# ── shared helpers ───────────────────────────────────────────────────────────

def _redirect_state(tmp):
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"] = str(tmp / "data")


# ════════════════════════════════════════════════════════════════════════════
#  Test 1: Index case-sensitive subtree isolation
# ════════════════════════════════════════════════════════════════════════════

def test_index_case_only_sibling_prune_subtree():
    """prune_subtree('a/b') must remove ONLY a/b/... rows and leave every a/B/... row
    intact. This guards the case-sensitive prefix-range match that has been rewritten
    three times without a guard."""
    tmp = Path(tempfile.mkdtemp(prefix="idx-case-prune-"))
    _redirect_state(tmp)
    try:
        sid = "5001"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())

        # Insert rows under a/b/... and a/B/...
        rows_ab = ["a/b/file1.txt", "a/b/file2.txt", "a/b/sub/deep.txt"]
        rows_aB = ["a/B/file1.txt", "a/B/only.txt", "a/B/sub/other.txt"]

        for rel in rows_ab + rows_aB:
            idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
            bp = m.blob_path(idx.box_id, idx.row_id(rel))
            bp.parent.mkdir(parents=True, exist_ok=True)
            bp.write_bytes(b"x")

        # Verify they all exist before the prune.
        for rel in rows_ab + rows_aB:
            assert idx.kind_of(rel) == "file", f"setup: {rel} should exist"

        # Prune a/b subtree.
        idx.prune_subtree("a/b")

        # a/b/... must be gone.
        for rel in rows_ab:
            assert idx.kind_of(rel) is None, \
                f"prune_subtree: {rel!r} should have been pruned"

        # a/B/... must be untouched.
        for rel in rows_aB:
            assert idx.kind_of(rel) == "file", \
                f"prune_subtree: {rel!r} (case-sibling) must survive"

        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_index_case_only_sibling_reparent():
    """reparent('a/b', 'a/c') must rename ONLY a/b/... rows and leave every a/B/...
    row intact, with its original name unchanged."""
    tmp = Path(tempfile.mkdtemp(prefix="idx-case-reparent-"))
    _redirect_state(tmp)
    try:
        sid = "5002"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())

        rows_ab = ["a/b/file1.txt", "a/b/file2.txt"]
        rows_aB = ["a/B/file1.txt", "a/B/other.txt"]

        for rel in rows_ab + rows_aB:
            idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
            bp = m.blob_path(idx.box_id, idx.row_id(rel))
            bp.parent.mkdir(parents=True, exist_ok=True)
            bp.write_bytes(b"x")

        idx.reparent("a/b", "a/c", wid)

        # Source a/b/... must be gone.
        for rel in rows_ab:
            assert idx.kind_of(rel) is None, \
                f"reparent: source {rel!r} should have been removed"

        # Destinations a/c/... must exist.
        expected_ac = ["a/c/file1.txt", "a/c/file2.txt"]
        for rel in expected_ac:
            assert idx.kind_of(rel) == "file", \
                f"reparent: destination {rel!r} should exist"

        # a/B/... must be completely untouched.
        for rel in rows_aB:
            assert idx.kind_of(rel) == "file", \
                f"reparent: case-sibling {rel!r} must survive unchanged"
        # Also confirm the a/B/... rows did NOT get moved to a/C/...
        for rel in rows_aB:
            upper_rel = rel.replace("a/B/", "a/C/")
            assert idx.kind_of(upper_rel) is None, \
                f"reparent: no spurious {upper_rel!r} from case-sibling rename"

        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ════════════════════════════════════════════════════════════════════════════
#  Test 2: NetworkPolicy approval_request — unit-level (asyncio, no real UI)
# ════════════════════════════════════════════════════════════════════════════

def _make_empty_rules(tmp_dir):
    """Create a fresh Rules object backed by a temp file (no pre-existing rules)."""
    rules_file = tmp_dir / "rules.txt"
    rules_file.write_text("")   # ensure file exists but is empty
    return m.Rules(rules_file)


def test_network_policy_timeout_deny_and_late_resolve_harmless():
    """TIMEOUT: approval_request with no resolver times out, returns deny, and a late
    resolve() afterward is a harmless no-op (the pending entry is gone and the late
    resolve neither raises nor wrongly takes effect).

    The 300-second timeout is monkeypatched to 0.01 s so the test is instant.
    """
    import asyncio

    tmp = Path(tempfile.mkdtemp(prefix="np-timeout-"))
    try:
        async def _run():
            rules = _make_empty_rules(tmp)
            sessions = {}
            sess = m.Session(session_id="10", box_id=10, cmd=["sh"])
            sessions["10"] = sess
            policy = m.NetworkPolicy(
                rules,
                resolve_session=sessions.get,
                record=lambda *a, **k: None,
                emit=lambda **ev: None,
            )

            # Monkeypatch the timeout so the test doesn't wait 300 s.
            import asyncio as _asyncio
            orig_wait_for = _asyncio.wait_for
            async def fast_wait_for(coro, timeout=None):
                return await orig_wait_for(coro, timeout=0.01)
            _asyncio.wait_for = fast_wait_for
            try:
                result = await policy.approval_request("10", "example.com", 80, "http")
            finally:
                _asyncio.wait_for = orig_wait_for

            # Outcome must be deny (the default on timeout).
            assert result["action"] == "deny", \
                f"timeout: expected deny, got {result['action']!r}"

            # pending must be empty now (the entry is removed after timeout).
            assert policy.pending == {}, \
                f"timeout: pending must be empty after timeout (got {policy.pending})"

            # A late resolve for the (now-gone) rid must be a no-op.
            # We construct the rid the policy would have used: "<sid[:8]>-1"
            rid = "10-1"
            try:
                policy.resolve(rid, "allow", "once")  # must not raise
            except Exception as exc:
                raise AssertionError(f"late resolve raised unexpectedly: {exc}")

            # Still no pending, still deny from the already-returned result.
            assert policy.pending == {}, \
                "late resolve: pending must still be empty"
            assert result["action"] == "deny", \
                "late resolve must not retroactively change the already-returned result"

        asyncio.run(_run())
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_network_policy_explicit_deny():
    """DENY: approval_request resolved via resolve(deny) returns action=deny."""
    tmp = Path(tempfile.mkdtemp(prefix="np-deny-"))
    try:
        async def _run():
            rules = _make_empty_rules(tmp)
            sessions = {}
            sess = m.Session(session_id="20", box_id=20, cmd=["sh"])
            sessions["20"] = sess
            policy = m.NetworkPolicy(
                rules,
                resolve_session=sessions.get,
                record=lambda *a, **k: None,
                emit=lambda **ev: None,
            )

            # Run approval_request in a background task and resolve it immediately.
            async def _resolve_after_tick():
                # One tick so the approval_request reaches the await.
                await asyncio.sleep(0)
                rid = "20-1"
                policy.resolve(rid, "deny", "once")

            task = asyncio.ensure_future(policy.approval_request(
                "20", "badhost.local", 443, "https"))
            await _resolve_after_tick()
            result = await task

            assert result["action"] == "deny", \
                f"explicit deny: expected deny, got {result['action']!r}"
            assert policy.pending == {}, "deny: pending cleared"

        asyncio.run(_run())
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_network_policy_allow_permanent_adds_rule():
    """ALLOW with PERMANENT scope: approval_request resolved with scope=permanent adds
    a permanent rule to the ruleset."""
    tmp = Path(tempfile.mkdtemp(prefix="np-perm-"))
    try:
        async def _run():
            rules = _make_empty_rules(tmp)
            sessions = {}
            sess = m.Session(session_id="30", box_id=30, cmd=["sh"])
            sessions["30"] = sess
            emitted = []
            policy = m.NetworkPolicy(
                rules,
                resolve_session=sessions.get,
                record=lambda *a, **k: None,
                emit=lambda **ev: emitted.append(ev),
            )

            async def _resolve_after_tick():
                await asyncio.sleep(0)
                rid = "30-1"
                policy.resolve(rid, "allow", "permanent", spec="host:good.example.com")

            task = asyncio.ensure_future(policy.approval_request(
                "30", "good.example.com", 443, "https"))
            await _resolve_after_tick()
            result = await task

            assert result["action"] == "allow", \
                f"permanent allow: expected allow, got {result['action']!r}"
            # A permanent rule must have been inserted into the ruleset.
            rule_lines = [r.to_line() for r in rules.rules]
            assert any("allow" in rl and "good.example.com" in rl for rl in rule_lines), \
                f"permanent allow: no rule added to ruleset (got {rule_lines!r})"
            # rules_updated event must have been emitted.
            assert any(ev.get("type") == "rules_updated" for ev in emitted), \
                f"permanent allow: no rules_updated event emitted (got {emitted!r})"

        asyncio.run(_run())
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ════════════════════════════════════════════════════════════════════════════
#  Test 3: File-rule passthrough dispatch (engine-level via live mount)
# ════════════════════════════════════════════════════════════════════════════

class _MountFixture:
    """Minimal overlay mount fixture — mirrors test_overlay_engine.MountFixture."""
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ovl-pt-"))
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        self.mnt = self.tmp / "mnt"
        self.live = self.tmp / "live"
        self.sid = "1"
        self.backing = self.live / self.sid
        self.up = self.backing / "up"
        self.up.mkdir(parents=True)
        self.mount = None
        self.index = None

    def start(self, lower=None):
        self.index = m.Index(self.backing)
        self.mount = m.OverlayMount(self.mnt, lower=lower or "/")
        ok = self.mount.start()
        if not ok:
            raise RuntimeError(f"mount failed: {self.mount._start_error}")
        self.mount.add_session(self.sid, self.up, self.index)
        self.root = self.mnt / self.sid

    def sh(self, script, timeout=15):
        return subprocess.run(
            ["timeout", str(timeout), "bash", "-c", script],
            cwd=str(self.root), capture_output=True, text=True)

    def stop(self):
        try:
            if self.mount: self.mount.stop()
        finally:
            try:
                if os.path.ismount(str(self.mnt)):
                    subprocess.run(["fusermount3", "-uz", str(self.mnt)],
                                   stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=10)
            except Exception:
                pass
            try:
                if self.index: self.index.close()
            except Exception:
                pass
            shutil.rmtree(self.tmp, ignore_errors=True)


def test_file_rule_passthrough_dispatch():
    """A passthrough FileRule on a session causes a matching path to be written to the
    host lower directly (no capture in the overlay index), while a non-matching path
    is captured normally.

    Uses the same session-level per-file frules injection that
    test_passthrough_kicks_up_to_parent (test_overlay_engine.py) already validates for
    the kick-up case; here we use a NON-nested (top-level) session with a custom
    lower so we can inspect what ended up in the lower directly.
    """
    host_lower = Path(tempfile.mkdtemp(prefix="ovl-pt-lower-"))
    fx = _MountFixture()
    try:
        fx.start(lower=str(host_lower))

        # Inject a per-file FileRules into the session with one passthrough rule.
        fr = m.FileRules.__new__(m.FileRules)
        fr.path = None
        fr.rules = [m.FileRule(action="passthrough", pattern="pt_match.txt")]
        fx.mount.ops.sessions[fx.sid]["frules"] = fr

        # Write the passthrough-matching path — should land on the real lower.
        r = fx.sh("echo passthrough-content > pt_match.txt && cat pt_match.txt")
        assert r.returncode == 0 and "passthrough-content" in r.stdout, \
            f"passthrough: create+read failed (rc={r.returncode}, stdout={r.stdout!r})"

        # The bytes must be on the real lower (host), not in the overlay index.
        host_file = host_lower / "pt_match.txt"
        assert host_file.exists(), \
            "passthrough: matching path must land on the real host lower"
        assert host_file.read_text() == "passthrough-content\n", \
            f"passthrough: host file content wrong (got {host_file.read_text()!r})"
        assert fx.index.kind_of("pt_match.txt") is None, \
            "passthrough: matching path must NOT be captured in the overlay index"

        # Write a non-matching path — should be captured in the overlay.
        r2 = fx.sh("echo captured-content > captured.txt && cat captured.txt")
        assert r2.returncode == 0 and "captured-content" in r2.stdout, \
            f"passthrough: non-matching create+read failed (rc={r2.returncode})"

        # The non-matching path must be captured in the overlay index.
        assert fx.index.kind_of("captured.txt") == "file", \
            "passthrough: non-matching path must be captured in the overlay index"
        # And NOT on the real lower (the lower starts empty).
        host_cap = host_lower / "captured.txt"
        assert not host_cap.exists(), \
            "passthrough: non-matching path must NOT land on the real lower"

    finally:
        fx.stop()
        shutil.rmtree(host_lower, ignore_errors=True)


# ════════════════════════════════════════════════════════════════════════════
#  Test 4: ChangeReview apply_hunk / discard_hunk (unit-level)
# ════════════════════════════════════════════════════════════════════════════

def test_change_review_apply_hunk_and_discard_hunk():
    """apply_hunk writes exactly that hunk to the host file; discard_hunk removes one
    hunk from the box's copy without touching the host for that hunk.

    Setup: write a host text file with a few lines, then build a finished box whose
    sqlar holds a modified version of that file (so there are real hunks). Then:
      - apply_hunk(0) must write the first hunk's change to the host and shrink the diff.
      - discard_hunk: we set up a second file with its own modified copy, call
        discard_hunk(0), and assert the box's copy loses that hunk while the host is
        untouched.
    """
    tmp = Path(tempfile.mkdtemp(prefix="cr-hunk-"))
    _redirect_state(tmp)
    # Two real host files we can freely create/destroy under /tmp.
    host_apply = Path(f"/tmp/sarun_hunk_apply_test_{os.getpid()}.txt")
    host_discard = Path(f"/tmp/sarun_hunk_discard_test_{os.getpid()}.txt")
    try:
        # ── Create host files ──────────────────────────────────────────────
        apply_orig = b"line A\nline B\nline C\nline D\n"
        discard_orig = b"alpha\nbeta\ngamma\ndelta\n"
        host_apply.write_bytes(apply_orig)
        host_discard.write_bytes(discard_orig)

        # ── Build a finished box with modified versions of both files ──────
        sid = "6001"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())

        rel_apply  = host_apply.relative_to("/").as_posix()
        rel_discard = host_discard.relative_to("/").as_posix()

        # Modified versions: one hunk each.
        apply_mod = b"line A\nline B MODIFIED\nline C\nline D\n"
        discard_mod = b"alpha\nbeta MODIFIED\ngamma\ndelta\n"

        for rel, modified in ((rel_apply, apply_mod), (rel_discard, discard_mod)):
            idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
            bp = m.blob_path(idx.box_id, idx.row_id(rel))
            bp.parent.mkdir(parents=True, exist_ok=True)
            bp.write_bytes(modified)

        m.consolidate(str(backing), sid, index=idx)
        idx.close()

        # ── Wire a minimal Supervisor so ChangeReview can read the box ─────
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=None)
        sup.sessions[sid] = m.Session(
            session_id=sid, box_id=int(sid), cmd=["sh"],
            live=False, shm_dir=str(backing))

        cr = sup.review

        # ── apply_hunk(0) for rel_apply ────────────────────────────────────
        src = cr._source(sid)
        info = cr._hunks(src, rel_apply)
        assert info is not None, "apply_hunk setup: rel_apply must be a text change"
        _ll, _ul, groups = info
        assert len(groups) >= 1, "apply_hunk setup: at least one hunk"

        result = cr.apply_hunk(sid, rel_apply, 0)
        assert result.get("ok") is True, \
            f"apply_hunk(0): expected ok=True, got {result!r}"

        # The host file must now contain the modified hunk's bytes.
        host_after_apply = host_apply.read_bytes()
        assert host_after_apply == apply_mod, \
            (f"apply_hunk: host file must have the hunk applied\n"
             f"  got:  {host_after_apply!r}\n"
             f"  want: {apply_mod!r}")

        # After applying the only hunk, the diff must shrink (no more hunks for that file).
        src2 = cr._source(sid)
        info2 = cr._hunks(src2, rel_apply)
        remaining_groups = info2[2] if info2 else []
        assert len(remaining_groups) < len(groups), \
            "apply_hunk: diff must shrink after applying the hunk"

        # ── discard_hunk(0) for rel_discard ───────────────────────────────
        src3 = cr._source(sid)
        info3 = cr._hunks(src3, rel_discard)
        assert info3 is not None, "discard_hunk setup: rel_discard must be a text change"
        _ll3, _ul3, groups3 = info3
        assert len(groups3) >= 1, "discard_hunk setup: at least one hunk"

        host_discard_before = host_discard.read_bytes()
        result2 = cr.discard_hunk(sid, rel_discard, 0)
        assert result2.get("ok") is True, \
            f"discard_hunk(0): expected ok=True, got {result2!r}"

        # Host file for rel_discard must be UNTOUCHED (discard reverts the box's copy).
        host_discard_after = host_discard.read_bytes()
        assert host_discard_after == host_discard_before, \
            (f"discard_hunk: host file must be untouched\n"
             f"  before: {host_discard_before!r}\n"
             f"  after:  {host_discard_after!r}")

        # After discard, the box's copy for rel_discard should match the host
        # (the hunk was reverted back to the original).
        src4 = cr._source(sid)
        info4 = cr._hunks(src4, rel_discard)
        remaining_groups4 = info4[2] if info4 else []
        assert len(remaining_groups4) < len(groups3), \
            "discard_hunk: diff must shrink (hunk removed from box's copy)"

    finally:
        host_apply.unlink(missing_ok=True)
        host_discard.unlink(missing_ok=True)
        shutil.rmtree(tmp, ignore_errors=True)


# ════════════════════════════════════════════════════════════════════════════
#  Test 5: e2e box rename (unit-level — no UI process needed)
# ════════════════════════════════════════════════════════════════════════════

def test_box_rename_label_only():
    """rename(box_id, NEW_NAME) is a label-only operation:
      - The NAME meta changes and the session.name is updated.
      - The dotted display path (resolve_box by new name) resolves to the same box.
      - The box_id, the backing sqlar path, and the sqlar identity are all UNCHANGED.
    """
    import re
    tmp = Path(tempfile.mkdtemp(prefix="rename-e2e-"))
    _redirect_state(tmp)

    class _FakeOps:
        def __init__(self): self.removed = []
        def add_session(self, *a, **k): pass
        def remove_session(self, sid): self.removed.append(sid)
        def add_virtual(self, *a, **k): pass

    class _FakeMount:
        def __init__(self): self.ops = _FakeOps()
        def is_healthy(self): return True
        def add_session(self, sid, *a, **k):
            try: (m.mnt_point() / str(sid)).mkdir(parents=True, exist_ok=True)
            except Exception: pass
        def remove_session(self, sid): self.ops.remove_session(sid)
        def add_ca_spoof(self, *a, **k): pass
        def set_parent(self, sid, parent): pass

    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        # Register a named box.
        ack = sup.register(dict(session_id="OLDNAME", cmd=["true"]))
        assert ack.get("ok") is True, f"rename-e2e: register ok (got {ack})"
        sid = ack["session_id"]

        sp_before = m.sqlar_path(sid)
        box_id_before = int(sid)

        # Record the backing dir and sqlar existence before rename.
        assert sp_before.exists(), "rename-e2e: sqlar must exist before rename"
        assert sup.sessions[sid].name == "OLDNAME", \
            f"rename-e2e: initial name is OLDNAME (got {sup.sessions[sid].name!r})"

        # Perform the rename.
        r = sup.rename(sid, "NEWNAME")
        assert r.get("ok") is True, f"rename-e2e: rename ok (got {r})"
        assert r.get("name") == "NEWNAME", \
            f"rename-e2e: returned name is NEWNAME (got {r.get('name')!r})"
        assert r.get("old") == "OLDNAME", \
            f"rename-e2e: returned old name is OLDNAME (got {r.get('old')!r})"

        # NAME meta changed.
        assert m.sqlar_meta_get(sp_before, "name") == "NEWNAME", \
            "rename-e2e: NAME meta must be NEWNAME after rename"
        assert sup.sessions[sid].name == "NEWNAME", \
            "rename-e2e: session.name must be NEWNAME in memory"

        # Dotted display path resolves by new name.
        resolved = sup.resolve_box("NEWNAME")
        assert resolved == sid, \
            f"rename-e2e: resolve_box('NEWNAME') must return {sid!r}, got {resolved!r}"

        # Old name no longer resolves.
        old_resolved = sup.resolve_box("OLDNAME")
        assert old_resolved is None, \
            f"rename-e2e: resolve_box('OLDNAME') must be None after rename (got {old_resolved!r})"

        # box_id is UNCHANGED.
        assert int(sid) == box_id_before, \
            "rename-e2e: box_id must be unchanged"

        # Backing sqlar path is UNCHANGED (same stem = same box_id).
        sp_after = m.sqlar_path(sid)
        assert sp_after == sp_before, \
            f"rename-e2e: sqlar path must be unchanged\n  before: {sp_before}\n  after:  {sp_after}"
        assert sp_after.exists(), \
            "rename-e2e: sqlar must still exist after rename (no file move)"

        # The sqlar stem must still be the numeric box_id, not the new name.
        assert re.fullmatch(r"\d+", sp_after.stem), \
            f"rename-e2e: sqlar stem must be numeric box_id (got {sp_after.stem!r})"

    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ════════════════════════════════════════════════════════════════════════════
#  Test 6: HTTPS/CONNECT proxy gate — SKIPPED
# ════════════════════════════════════════════════════════════════════════════

def test_https_connect_proxy_gate_skipped():
    """HTTPS/CONNECT through the proxy gate (_GatingAddon.http_connect / TLS tunnel).

    SKIPPED: exercising this path requires:
      1. A real mitmproxy ProxyServer instance with a running asyncio event loop.
      2. A live TLS certificate (mitmproxy's CA or a self-signed one) installed in a
         test trust store, so an HTTPS client can complete the handshake.
      3. A real TCP connection to a target HTTPS server (or a local TLS echo server).
    None of these are feasible without significant test infrastructure (a real
    network-available HTTPS endpoint or a local TLS server, mitmproxy's CA plumbing,
    and a correctly wired ProxyEngine).  The existing net-flow tests in test_net_flow.py
    cover the HTTP non-TLS gating path.  The TLS path would require the scaffolding
    described above.
    """
    import pytest
    pytest.skip(
        "HTTPS/CONNECT requires real TLS plumbing (mitmproxy CA, live TLS endpoint, "
        "running ProxyServer + event loop). Infrastructure not available in this env.")


# ════════════════════════════════════════════════════════════════════════════

def test_fmt_bytes_is_module_level():
    """Regression guard: fmt_bytes must be a MODULE-LEVEL function. It was once
    defined only as a nested function inside _make_ui_app while being referenced
    from ChangeReview.structural_diff_quick (module scope) — a latent NameError on
    the >4MiB binary-diff path. If it ever re-nests, m.fmt_bytes vanishes and this
    fails."""
    assert callable(getattr(m, "fmt_bytes", None)), "fmt_bytes is not module-level"
    assert m.fmt_bytes(1 << 20) == "1.0M"
    assert m.fmt_bytes(3) == "3B"
    # And the previously-crashing caller path must not raise NameError for fmt_bytes:
    # structural_diff_quick references fmt_bytes in its size-cap branch (module scope).
    import inspect
    src = inspect.getsource(m.ChangeReview.structural_diff_quick)
    assert "fmt_bytes" in src  # the call site still exists and now resolves


# ════════════════════════════════════════════════════════════════════════════
#  Test: runner-mode detection (ambient caps vs. unprivileged userns)
# ════════════════════════════════════════════════════════════════════════════

def test_capeff_has_caps_parses_known_hex():
    """_capeff_has_caps must require BOTH CAP_SYS_ADMIN (bit 21) and
    CAP_NET_ADMIN (bit 12)."""
    CAP_NET_ADMIN = 1 << 12
    CAP_SYS_ADMIN = 1 << 21
    both = CAP_NET_ADMIN | CAP_SYS_ADMIN
    assert m._capeff_has_caps(format(both, "x")) is True
    # A full set (all 64 bits) trivially includes both.
    assert m._capeff_has_caps("0000003fffffffff") is True
    # Only one of the two → False.
    assert m._capeff_has_caps(format(CAP_SYS_ADMIN, "x")) is False
    assert m._capeff_has_caps(format(CAP_NET_ADMIN, "x")) is False
    # Empty / no caps → False.
    assert m._capeff_has_caps("0000000000000000") is False
    # Malformed → False, no raise.
    assert m._capeff_has_caps("not-hex") is False
    assert m._capeff_has_caps(None) is False


def test_have_ambient_caps_reflects_status_file(tmp_path, monkeypatch):
    """_have_ambient_caps parses CapEff from /proc/self/status; True when both
    caps present, False when absent, False when the file is unreadable."""
    CAP_NET_ADMIN = 1 << 12
    CAP_SYS_ADMIN = 1 << 21
    both = format(CAP_NET_ADMIN | CAP_SYS_ADMIN, "016x")
    none = "0000000000000000"

    real_open = open
    def fake_open(path, *a, **k):
        if path == "/proc/self/status":
            return real_open(status_file, *a, **k)
        return real_open(path, *a, **k)

    status_file = str(tmp_path / "status_both")
    with real_open(status_file, "w") as f:
        f.write(f"Name:\tx\nCapEff:\t{both}\n")
    monkeypatch.setattr("builtins.open", fake_open)
    assert m._have_ambient_caps() is True

    status_file = str(tmp_path / "status_none")
    with real_open(status_file, "w") as f:
        f.write(f"Name:\tx\nCapEff:\t{none}\n")
    assert m._have_ambient_caps() is False

    # Missing file → OSError swallowed → False.
    def raising_open(path, *a, **k):
        if path == "/proc/self/status":
            raise FileNotFoundError(path)
        return real_open(path, *a, **k)
    monkeypatch.setattr("builtins.open", raising_open)
    assert m._have_ambient_caps() is False


def test_userns_runner_works_returns_bool_no_raise():
    """The end-to-end userns probe must return a bool and never raise,
    regardless of environment."""
    r = m._userns_runner_works()
    assert isinstance(r, bool)
    # Cached: second call returns the same value.
    assert m._userns_runner_works() is r
