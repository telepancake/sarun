#!/usr/bin/env python3
"""REVIEW-verb parity: the RUST engine (engine/) serves the per-row review
surface the Python UI calls — review.patch_text, review.change_mode,
review.decorate, review.apply_hunk, review.discard_hunk, and the vacuous
lifecycle pokes (consolidate_start, review.invalidate_consolidation,
review.invalidate_struct). Boxes are built on disk by the PYTHON storage code
(Index/consolidate/blob_path); verbs are called via m.sync_request; assertions
check REAL EFFECTS (host bytes after apply_hunk, box bytes after discard_hunk,
created-vs-modified for decorate), and cross-check hunk math against the Python
ChangeReview helpers on the SAME sqlar where feasible. Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_review_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import base64, difflib, os, socket, stat as stat_mod, subprocess, sys
import tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/prototype/sarun"
CRATE = Path(__file__).resolve().parent.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def build_box(m, sid, files, whiteouts=()):
    """Build a FINISHED on-disk box from {rel: bytes} + tombstoned rels."""
    bk = m.live_dir(sid); (bk / "up").mkdir(parents=True)
    ix = m.Index(bk); w = ix.writer_for(os.getpid())
    for rel, content in files.items():
        ix.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, w, "create")
        bp = m.blob_path(ix.box_id, ix.row_id(rel))
        bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(content)
    for rel in whiteouts:
        ix.set_entry(rel, "whiteout", 0, w, "unlink")
    m.consolidate(str(bk), sid, index=ix); ix.close()
    shutil.rmtree(bk, ignore_errors=True)


def call(m, sock, verb, *args):
    return m.sync_request(sock, type="ui", verb=verb, args=list(args))["r"]


def main():
    if not ensure_binary():
        raise SystemExit("test_review_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="revrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RV"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        # ── review.change_mode ───────────────────────────────────────────────
        cmid = "8001"
        build_box(m, cmid, {"root/cm.txt": b"hi\n"})
        rmode = call(m, sock, "review.change_mode", cmid, "root/cm.txt")
        pmode = m.sqlar_mode(m.sqlar_path(cmid), "root/cm.txt")
        check(rmode == pmode and rmode is not None,
              f"review-rs: change_mode == sqlar mode ({rmode} vs {pmode})")
        check(call(m, sock, "review.change_mode", cmid, "root/nope.txt") is None,
              "review-rs: change_mode of a missing path is null")
        call(m, sock, "delete", cmid)

        # ── review.decorate: created vs modified, is_text, stale ─────────────
        # (a) a CREATED text file (no host file): kind=created, is_text=True.
        crhost = Path("/root/rv_created.txt"); crhost.unlink(missing_ok=True)
        # (b) a MODIFIED text file (host exists): kind=modified.
        mdhost = Path("/root/rv_mod.txt"); mdhost.write_bytes(b"base\n")
        # (c) a binary change: is_text=False.
        bnhost = Path("/root/rv_bin.bin"); bnhost.unlink(missing_ok=True)
        decid = "8002"
        try:
            build_box(m, decid, {
                "root/rv_created.txt": b"new file\n",
                "root/rv_mod.txt": b"base\nadded\n",
                "root/rv_bin.bin": b"\x00\x01\x02binary\x00\n",
            })
            d_cr = call(m, sock, "review.decorate", decid, "root/rv_created.txt")
            check(d_cr["kind"] == "created" and d_cr["is_text"] is True
                  and d_cr["stale"] is False,
                  f"review-rs: decorate created text file: {d_cr}")
            d_md = call(m, sock, "review.decorate", decid, "root/rv_mod.txt")
            check(d_md["kind"] == "modified" and d_md["is_text"] is True,
                  f"review-rs: decorate modified text file: {d_md}")
            d_bn = call(m, sock, "review.decorate", decid, "root/rv_bin.bin")
            check(d_bn["is_text"] is False and d_bn["kind"] == "created",
                  f"review-rs: decorate binary file is_text=False: {d_bn}")
            # CROSS-CHECK: is_text/kind against an explicit NUL test + host stat.
            for rel in ("root/rv_created.txt", "root/rv_mod.txt", "root/rv_bin.bin"):
                cur = m.sqlar_content(m.sqlar_path(decid), rel) or b""
                low = (Path("/") / rel).read_bytes() \
                    if (Path("/") / rel).exists() else b""
                py_is_text = b"\x00" not in cur and b"\x00" not in low
                py_kind = "modified" if (Path("/") / rel).exists() else "created"
                got = call(m, sock, "review.decorate", decid, rel)
                check(got["is_text"] == py_is_text and got["kind"] == py_kind,
                      f"review-rs: decorate cross-check {rel}: "
                      f"{got} vs is_text={py_is_text} kind={py_kind}")
            # stale: bump the host mtime AFTER the stored mtime → stale True.
            future = 4_000_000_000 * 1_000_000_000  # year ~2096, in ns
            os.utime(mdhost, ns=(future, future))
            d_st = call(m, sock, "review.decorate", decid, "root/rv_mod.txt")
            check(d_st["stale"] is True,
                  f"review-rs: decorate flags a host file newer than stored: {d_st}")
        finally:
            for p in (crhost, mdhost, bnhost): p.unlink(missing_ok=True)
        call(m, sock, "delete", decid)

        # ── review.patch_text: unified diff BYTES, base64 over the wire ──────
        pthost = Path("/root/rv_patch.txt")
        pthost.write_bytes(b"\n".join(b"L%d" % i for i in range(10)) + b"\n")
        ptid = "8003"
        try:
            up = [b"L%d" % i for i in range(10)]; up[3] = b"CHANGED-3"
            build_box(m, ptid, {"root/rv_patch.txt": b"\n".join(up) + b"\n"})
            wire = m.sync_request(sock, type="ui", verb="review.patch_text",
                                  args=[ptid])["r"]
            # Wire contract: bytes are sent as {"__b": base64}; m.wire_decode
            # turns that back into real bytes (same path RemoteReview uses).
            check(set(wire) == {"__b"},
                  f"review-rs: patch_text is wire-encoded as __b ({wire!r:.40})")
            raw = m.wire_decode(wire)
            check(isinstance(raw, bytes),
                  f"review-rs: patch_text decodes to real bytes ({type(raw).__name__})")
            check(b"--- a/root/rv_patch.txt" in raw
                  and b"+++ b/root/rv_patch.txt" in raw,
                  "review-rs: patch_text carries the git-style file header")
            check(b"-L3" in raw and b"+CHANGED-3" in raw,
                  f"review-rs: patch_text shows the changed line\n{raw.decode()}")
        finally:
            pthost.unlink(missing_ok=True)
        call(m, sock, "delete", ptid)

        # ── review.apply_hunk: splice ONE hunk onto the host (byte-exact) ────
        # A 2-hunk text change: edit line 1 and line 37 of a 40-line file. Apply
        # ONLY hunk 0 → the host must gain THAT edit and NOT the other one.
        ahhost = Path("/root/rv_applyhunk.txt")
        base = [b"L%d" % i for i in range(40)]
        ahhost.write_bytes(b"\n".join(base) + b"\n")
        ahid = "8004"
        try:
            up = base[:]; up[1] = b"EDIT-A"; up[37] = b"EDIT-B"
            upbytes = b"\n".join(up) + b"\n"
            build_box(m, ahid, {"root/rv_applyhunk.txt": upbytes})
            # Oracle: how many hunks does Python see? (must be 2 separated edits)
            ll = m.ut_split(b"\n".join(base) + b"\n"); ul = m.ut_split(upbytes)
            groups = list(difflib.SequenceMatcher(None, ll, ul)
                          .get_grouped_opcodes(3))
            check(len(groups) == 2,
                  f"review-rs: apply_hunk fixture has 2 hunks (got {len(groups)})")
            # Expected host bytes after applying ONLY hunk 0 (the L1 edit):
            g = groups[0]; a1, a2 = g[0][1], g[-1][2]; b1, b2 = g[0][3], g[-1][4]
            expect = b"".join(ll[:a1] + ul[b1:b2] + ll[a2:])
            r = call(m, sock, "review.apply_hunk", ahid, "root/rv_applyhunk.txt", 0)
            check(r.get("ok") is True, f"review-rs: apply_hunk(0) ok: {r}")
            host_now = ahhost.read_bytes()
            check(host_now == expect,
                  "review-rs: apply_hunk(0) wrote EXACTLY hunk 0 to the host")
            check(b"EDIT-A" in host_now,
                  "review-rs: apply_hunk(0) — host now contains hunk 0's edit")
            check(b"EDIT-B" not in host_now,
                  "review-rs: apply_hunk(0) — host does NOT contain the OTHER hunk")
            # The box still has the remaining (second) hunk pending.
            ch = call(m, sock, "review.session_changes", ahid)
            check(any(e["path"] == "root/rv_applyhunk.txt" for e in ch),
                  "review-rs: apply_hunk leaves the still-differing change pending")
            h2 = call(m, sock, "review.hunks", ahid, "root/rv_applyhunk.txt")
            check(h2.get("is_text") is True and len(h2["hunks"]) == 1,
                  f"review-rs: one hunk remains after apply_hunk(0): "
                  f"{len(h2.get('hunks', []))}")
            # stale index now rejected.
            rbad = call(m, sock, "review.apply_hunk", ahid,
                        "root/rv_applyhunk.txt", 5)
            check(rbad.get("ok") is False and "stale" in rbad.get("error", ""),
                  "review-rs: apply_hunk rejects an out-of-range index")
        finally:
            ahhost.unlink(missing_ok=True)
        call(m, sock, "delete", ahid)

        # ── review.discard_hunk: revert ONE hunk in the BOX (back to host) ──
        dhhost = Path("/root/rv_dischunk.txt")
        dhhost.write_bytes(b"\n".join(base) + b"\n")
        dhid = "8005"
        try:
            up = base[:]; up[1] = b"EDIT-A"; up[37] = b"EDIT-B"
            upbytes = b"\n".join(up) + b"\n"
            build_box(m, dhid, {"root/rv_dischunk.txt": upbytes})
            ll = m.ut_split(b"\n".join(base) + b"\n"); ul = m.ut_split(upbytes)
            groups = list(difflib.SequenceMatcher(None, ll, ul)
                          .get_grouped_opcodes(3))
            g = groups[0]; a1, a2 = g[0][1], g[-1][2]; b1, b2 = g[0][3], g[-1][4]
            expect_box = b"".join(ul[:b1] + ll[a1:a2] + ul[b2:])
            r = call(m, sock, "review.discard_hunk", dhid,
                     "root/rv_dischunk.txt", 0)
            check(r.get("ok") is True, f"review-rs: discard_hunk(0) ok: {r}")
            box_now = m.sqlar_content(m.sqlar_path(dhid), "root/rv_dischunk.txt")
            check(box_now == expect_box,
                  "review-rs: discard_hunk(0) reverted EXACTLY hunk 0 in the box")
            check(b"EDIT-A" not in box_now,
                  "review-rs: discard_hunk(0) — box no longer has hunk 0's edit")
            check(b"EDIT-B" in box_now,
                  "review-rs: discard_hunk(0) — box KEEPS the other hunk's edit")
            # the host is untouched by discard_hunk.
            check(dhhost.read_bytes() == b"\n".join(base) + b"\n",
                  "review-rs: discard_hunk did NOT touch the host")
        finally:
            dhhost.unlink(missing_ok=True)
        call(m, sock, "delete", dhid)

        # ── apply_hunk emptying the box reaps it (single-hunk change) ────────
        sehost = Path("/root/rv_single.txt"); sehost.write_bytes(b"x\n")
        seid = "8006"
        try:
            build_box(m, seid, {"root/rv_single.txt": b"x\ny\n"})
            r = call(m, sock, "review.apply_hunk", seid, "root/rv_single.txt", 0)
            check(r.get("ok") is True, "review-rs: single-hunk apply_hunk ok")
            check(sehost.read_bytes() == b"x\ny\n",
                  "review-rs: apply_hunk wrote the full single-hunk change")
            check(not m.sqlar_path(seid).exists(),
                  "review-rs: box reaped after apply_hunk consumed its last diff")
        finally:
            sehost.unlink(missing_ok=True)

        # ── vacuous lifecycle pokes must NOT be 'unknown verb' ───────────────
        for verb in ("consolidate_start", "review.invalidate_consolidation",
                     "review.invalidate_struct"):
            rep = m.sync_request(sock, type="ui", verb=verb, args=[])
            check(rep is not None and rep.get("ok") is True,
                  f"review-rs: {verb} is a successful no-op")

        eng.terminate()
        try: eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=5)
        check(eng.returncode == 0, "review-rs: SIGTERM exits 0")
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None and eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=5)
            except Exception: pass
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("REVIEW-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_review_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
