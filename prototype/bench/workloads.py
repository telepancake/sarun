#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pyfuse3>=3.2",
#   "trio>=0.22",
#   "python-magic>=0.4",
#   "wcmatch>=8.4",
# ]
# ///
"""workloads — the metadata/churn microbenchmarks behind FINDINGS.md's numbers,
committed so any two revisions can be compared on the same box.

Workloads (each run native-under-bwrap AND through the sarun overlay, same
bwrap flags, so the only difference is the FUSE layer — same method as
overlay_bench.py, whose machinery this reuses):

  git-status   one `git status --porcelain` walk over a synthetic 5 000-file
               repo: a one-op-per-distinct-path metadata storm. Reports the
               COLD first run (every op crosses FUSE) and the WARM best
               (kernel dentry/attr caches + the dir-listing cache).
  exec-storm   300 execs of /bin/true via bash: many ops on FEW paths —
               cache-friendly, nearly free through the overlay.
  file-churn   200 × (create + delete) of small files: the capture write
               path (provenance + sqlite row per file), configure-style spam.
  rpc          remote-UI verb round-trip against a live headless engine
               (connect + dispatch + reply per call) — the attach-mode tax.

Comparing revisions (the point of this file):

    bench/workloads.py                          # current working tree
    git show <rev>:sarun > /tmp/sarun_old
    SARUN_PATH=/tmp/sarun_old bench/workloads.py    # any past revision

Numbers are machine-relative: compare two runs from the SAME box only. The
transferable statistic is the per-op overhead (printed for git-status):
(overlay − native) / ops — additive CPU per FUSE crossing, ~independent of
storage speed.

Reference results (this repo's dev container, 2026-06; post dir-listing
cache, post engine/UI split — see bench/FINDINGS.md):
    git-status   native 0.010s · overlay cold 0.44s (~26×, ~85µs/op) · warm 0.058s
    exec-storm   native 0.31s  · overlay 0.52s  (1.7×)
    file-churn   native 0.24s  · overlay 0.81s  (3.3×)
    rpc          0.37 ms / call
"""
import os, shutil, subprocess, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import overlay_bench as ob

FILES_PER_DIR, DIRS = 50, 100          # 5 000 tracked files
GIT_CMD = ["git", "status", "--porcelain"]
EXEC_CMD = ["bash", "-c", "for i in $(seq 300); do /bin/true; done"]
CHURN_CMD = ["bash", "-c",
             "for i in $(seq 200); do echo 'int main;' > t$i.c; rm t$i.c; done"]


def make_repo(root: Path) -> Path:
    repo = root / "repo"; repo.mkdir(parents=True)
    env = dict(os.environ, GIT_AUTHOR_NAME="b", GIT_AUTHOR_EMAIL="b@b",
               GIT_COMMITTER_NAME="b", GIT_COMMITTER_EMAIL="b@b")
    subprocess.run(["git", "init", "-q", "."], cwd=repo, check=True, env=env)
    for d in range(DIRS):
        dd = repo / f"dir{d}"; dd.mkdir()
        for f in range(FILES_PER_DIR):
            (dd / f"f{f}.txt").write_text(f"content {d} {f}\n")
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True, env=env)
    subprocess.run(["git", "-c", "commit.gpgsign=false", "commit", "-qm", "seed"],
                   cwd=repo, check=True, env=env)
    return repo


def series(args, runs):
    out = []
    for _ in range(runs):
        t0 = time.monotonic()
        p = subprocess.run(args, stdout=subprocess.DEVNULL,
                           stderr=subprocess.PIPE)
        if p.returncode != 0:
            sys.stderr.write(p.stderr.decode(errors="replace")[-1500:])
            sys.exit(1)
        out.append(time.monotonic() - t0)
    return out


def main() -> int:
    # The workdir must NOT live under bwrap's --tmpfs paths (/tmp, /run).
    work = Path(tempfile.mkdtemp(prefix="wlbench-", dir="/root"))
    sarun = ob.load_sarun()
    tmproot, mount, box_root, sid = ob.setup_overlay(sarun)
    try:
        repo = make_repo(work)
        print(f"{'workload':<12}{'native':>9}{'ovl cold':>10}{'ovl warm':>10}"
              f"{'cold×':>7}{'warm×':>7}")
        for name, cmd, cwd, runs in (("git-status", GIT_CMD, repo, 5),
                                     ("exec-storm", EXEC_CMD, repo, 4),
                                     ("file-churn", CHURN_CMD, repo, 4)):
            nat = series(ob.make_bwrap(None, str(cwd), cmd, overlay=False), runs)
            ovl = series(ob.make_bwrap(box_root, str(cwd), cmd, overlay=True), runs)
            n, c, w = min(nat), ovl[0], min(ovl[1:])
            print(f"{name:<12}{n:>8.3f}s{c:>9.3f}s{w:>9.3f}s"
                  f"{c/n:>6.1f}x{w/n:>6.1f}x")
            if name == "git-status":
                ops = DIRS * (FILES_PER_DIR + 2)   # ~1 lstat/file + readdirs
                print(f"{'':<12}per-op overhead, cold: "
                      f"{(c - min(nat)) / ops * 1e6:.0f} µs/op "
                      f"(machine-portable-ish; ratios are not)")
    finally:
        try: mount.stop()
        except Exception: pass
        shutil.rmtree(tmproot, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)

    # rpc: remote-UI verb round-trip against a real headless engine (fresh
    # XDG temp tree — the overlay phase's tree was deleted above).
    eng_tmp = tempfile.mkdtemp(prefix="wlbench-rpc-")
    os.environ["XDG_STATE_HOME"] = os.path.join(eng_tmp, "state")
    os.environ["XDG_RUNTIME_DIR"] = os.path.join(eng_tmp, "run")
    sarun_path = str(os.environ.get("SARUN_PATH") or
                     Path(__file__).resolve().parent.parent / "sarun")
    eng = subprocess.Popen([sys.executable, sarun_path, "engine"],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        r = sarun.RemoteSupervisor(sarun.sock_path())
        deadline = time.time() + 60
        while time.time() < deadline:
            try: r.session_dicts(); break
            except Exception: time.sleep(0.2)
        t0 = time.monotonic(); N = 200
        for _ in range(N): r.session_dicts()
        print(f"{'rpc':<12}{(time.monotonic()-t0)/N*1e3:.2f} ms / verb call")
    finally:
        eng.terminate()
        try: eng.wait(timeout=15)
        except Exception: eng.kill()
        shutil.rmtree(eng_tmp, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
