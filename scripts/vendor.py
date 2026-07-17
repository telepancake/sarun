#!/usr/bin/env python3
"""Assemble engine/vendor/ from pinned pristine upstreams + patch series.

Sources are pinned in engine/vendor.toml; the sarun delta per crate lives in
engine/vendor-patches/<crate>/ as {files, series, *.patch}. This script
downloads each upstream (sha256-verified crates.io tarball, or a git commit
fetched by hash), copies the crate's file selection, and applies its series
with `git apply`. The result in engine/vendor/ is not tracked by git.

Idempotent: a crate is reassembled only when its manifest entry or patch dir
changed (content stamp in engine/vendor/<crate>/.stamp). `--force` rebuilds all.
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ENGINE = os.path.join(REPO, "engine")
VENDOR = os.path.join(ENGINE, "vendor")
PATCHES = os.path.join(ENGINE, "vendor-patches")
CACHE = os.path.join(ENGINE, ".vendor-cache")
ASSEMBLY_VERSION = "2-dereference-license-symlinks"


def copy_selected_file(src, dst, relpath):
    """Copy one selected upstream file exactly as assembly expects it."""
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    # Brush and some other workspace crates ship `LICENSE` as a link to the
    # repository-level notice.  A selected crate is assembled independently,
    # so preserving that link would leave a broken `../LICENSE`.
    if os.path.islink(src) and os.path.basename(relpath).upper().startswith(
            ("LICENSE", "COPYING", "NOTICE", "COPYRIGHT")):
        resolved = os.path.realpath(src)
        if not os.path.isfile(resolved):
            sys.exit(f"license link has no target: {src}")
        shutil.copy2(resolved, dst)
    else:
        shutil.copy2(src, dst, follow_symlinks=False)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch_crate(name, version, want_sha):
    tarball = os.path.join(CACHE, f"{name}-{version}.crate")
    if not (os.path.exists(tarball) and sha256(tarball) == want_sha):
        url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
        tmp = tarball + ".part"
        with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
            shutil.copyfileobj(r, f)
        got = sha256(tmp)
        if got != want_sha:
            sys.exit(f"{name}: sha256 mismatch: got {got}, want {want_sha}")
        os.replace(tmp, tarball)
    root = os.path.join(CACHE, f"{name}-{version}")
    if not os.path.isdir(root):
        with tarfile.open(tarball) as t:
            t.extractall(CACHE, filter="tar")
    return root


def fetch_git(url, commit):
    root = os.path.join(CACHE, "git-" + commit)
    if os.path.isdir(os.path.join(root, ".git")):
        return root
    tmp = root + ".part"
    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp)
    for cmd in (["git", "init", "-q"],
                ["git", "fetch", "-q", "--depth", "1", url, commit],
                ["git", "checkout", "-q", commit]):
        subprocess.run(cmd, cwd=tmp, check=True)
    os.replace(tmp, root)
    return root


def stamp_of(name, entry):
    h = hashlib.sha256(ASSEMBLY_VERSION.encode())
    h.update(repr(sorted(entry.items())).encode())
    pdir = os.path.join(PATCHES, name)
    for fn in sorted(os.listdir(pdir)):
        h.update(fn.encode())
        with open(os.path.join(pdir, fn), "rb") as f:
            h.update(f.read())
    return h.hexdigest()


def assemble(name, entry, root):
    pdir = os.path.join(PATCHES, name)
    with open(os.path.join(pdir, "files")) as f:
        files = f.read().splitlines()
    with open(os.path.join(pdir, "series")) as f:
        series = [l for l in f.read().splitlines() if l and not l.startswith("#")]

    # Stage OUTSIDE the repo: `git apply` run inside a work tree resolves
    # patch paths against the repo root and silently skips paths outside its
    # cwd subtree, no-op'ing the whole series with exit 0.
    stage = tempfile.mkdtemp(prefix="sarun-vendor-")
    try:
        for fl in files:
            src = os.path.join(root, fl)
            dst = os.path.join(stage, fl)
            copy_selected_file(src, dst, fl)
        for p in series:
            r = subprocess.run(
                ["git", "apply", "--whitespace=nowarn", os.path.join(pdir, p)],
                cwd=stage, capture_output=True, text=True)
            if r.returncode != 0:
                sys.stderr.write(
                    f"{r.stderr}\n"
                    f"vendor: {name}: patch failed to apply: {p}\n"
                    f"  Upstream drifted under this patch. The partially-built tree\n"
                    f"  (earlier patches applied) is kept at:\n    {stage}\n"
                    f"  To re-spin: make the patch's change there by hand, then\n"
                    f"    git diff --no-index <pristine-copy> {stage}\n"
                    f"  or edit the hunks in engine/vendor-patches/{name}/{p}\n"
                    f"  directly, and rerun `make vendor`. Do not fold patches\n"
                    f"  together. See engine/vendor-patches/README.md.\n")
                sys.exit(1)
        dest = os.path.join(VENDOR, name)
        shutil.rmtree(dest, ignore_errors=True)
        os.makedirs(VENDOR, exist_ok=True)
        shutil.move(stage, dest)
    except BaseException:
        # deliberate: leave `stage` behind — the apply-failure path above
        # points the user at it for re-spinning the failed patch
        raise


def load_manifest():
    with open(os.path.join(ENGINE, "vendor.toml"), "rb") as f:
        return tomllib.load(f)["crates"]


def upstream_root(entry):
    if entry["source"] == "crates.io":
        return fetch_crate(entry.get("name") or entry["_name"],
                           entry["version"], entry["sha256"])
    root = fetch_git(entry["url"], entry["commit"])
    if "subdir" in entry:
        root = os.path.join(root, entry["subdir"])
    return root


def cmd_assemble(force=False, only=None):
    manifest = load_manifest()
    os.makedirs(CACHE, exist_ok=True)
    for name, entry in manifest.items():
        if only and name != only:
            continue
        entry["_name"] = name
        want = stamp_of(name, entry)
        stamp = os.path.join(VENDOR, name, ".stamp")
        # A valid stamp only counts if the tree is actually there — a git
        # operation can remove the files while leaving the untracked stamp.
        populated = os.path.exists(os.path.join(VENDOR, name, "Cargo.toml"))
        if not force and populated and os.path.exists(stamp) \
                and open(stamp).read() == want:
            continue
        assemble(name, entry, upstream_root(entry))
        with open(stamp, "w") as f:
            f.write(want)
        print(f"vendor: {name}")


def assemble_baseline(name, drop_last=False):
    """Assemble `name` (pristine + series, optionally minus the last patch)
    into a temp dir and return (dir, series_list). The everyday edit loop
    diffs the WORKING tree in engine/vendor/<name> against this baseline."""
    manifest = load_manifest()
    entry = manifest[name]
    entry["_name"] = name
    root = upstream_root(entry)
    pdir = os.path.join(PATCHES, name)
    with open(os.path.join(pdir, "files")) as f:
        files = f.read().splitlines()
    with open(os.path.join(pdir, "series")) as f:
        series = [l for l in f.read().splitlines() if l and not l.startswith("#")]
    applied = series[:-1] if drop_last else series
    stage = tempfile.mkdtemp(prefix="sarun-vendor-base-")
    for fl in files:
        src = os.path.join(root, fl)
        dst = os.path.join(stage, fl)
        copy_selected_file(src, dst, fl)
    for pch in applied:
        subprocess.run(["git", "apply", "--whitespace=nowarn",
                        os.path.join(pdir, pch)], cwd=stage, check=True)
    return stage, series


def working_delta(name, baseline):
    """`git diff` of the working tree in engine/vendor/<name> against the
    assembled baseline, with a/<rel> b/<rel> paths (the series format)."""
    work = os.path.join(VENDOR, name)
    r = subprocess.run(
        ["git", "-c", "core.fileMode=false", "diff", "--no-index",
         "--src-prefix=a/", "--dst-prefix=b/", baseline, work],
        capture_output=True, text=True)
    out = r.stdout
    out = out.replace("a/" + baseline.lstrip("/") + "/", "a/")
    out = out.replace("b/" + baseline.lstrip("/") + "/", "b/")
    out = out.replace("a" + baseline + "/", "a/")
    out = out.replace("b" + baseline + "/", "b/")
    out = out.replace("a/" + work.lstrip("/") + "/", "a/")
    out = out.replace("b/" + work.lstrip("/") + "/", "b/")
    out = out.replace("a" + work + "/", "a/")
    out = out.replace("b" + work + "/", "b/")
    # drop assembly bookkeeping hunks
    kept, skip = [], False
    for line in out.split("\n"):
        if line.startswith("diff --git"):
            skip = "/.stamp" in line or "/target/" in line
        if not skip:
            kept.append(line)
    joined = "\n".join(kept)
    # a diff must end with a newline or `git apply` sees a corrupt patch
    if joined and not joined.endswith("\n"):
        joined += "\n"
    return joined


def cmd_diff(name):
    base, _ = assemble_baseline(name)
    try:
        d = working_delta(name, base)
        sys.stdout.write(d)
        return 0 if not d.strip() else 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


def next_patch_name(pdir, subject):
    nums = [int(fn[:4]) for fn in os.listdir(pdir)
            if fn[:4].isdigit() and fn.endswith(".patch")]
    n = (max(nums) + 10) if nums else 10
    slug = "".join(c if c.isalnum() else "-" for c in subject)[:60].strip("-")
    return f"{n:04d}-{slug}.patch"


def patch_header(subject, body):
    import time as _t
    date = _t.strftime("%a, %d %b %Y %H:%M:%S +0000", _t.gmtime())
    hdr = (f"From 0000000000000000000000000000000000000000 Mon Sep 17 00:00:00 2001\n"
           f"From: sarun <noreply@sarun>\nDate: {date}\n"
           f"Subject: [PATCH] {subject}\n\n")
    if body:
        hdr += body.rstrip() + "\n\n"
    return hdr + "---\n"


def restamp(name):
    manifest = load_manifest()
    entry = manifest[name]
    entry["_name"] = name
    with open(os.path.join(VENDOR, name, ".stamp"), "w") as f:
        f.write(stamp_of(name, entry))


def cmd_refresh(name, subject=None, body="", amend=False):
    pdir = os.path.join(PATCHES, name)
    base, series = assemble_baseline(name, drop_last=amend)
    try:
        delta = working_delta(name, base)
    finally:
        shutil.rmtree(base, ignore_errors=True)
    if not delta.strip():
        print(f"refresh: {name}: working tree matches the series — nothing to capture")
        return 0
    if amend:
        if not series:
            sys.exit(f"refresh --amend: {name} has no patches to amend")
        target = os.path.join(pdir, series[-1])
        head = open(target).read().split("\n---\n", 1)[0]
        with open(target, "w") as f:
            f.write(head + "\n---\n" + delta)
        print(f"refresh: folded working delta into {series[-1]}")
    else:
        if not subject:
            sys.exit("refresh: a NEW patch needs -m \"subject\" (or use --amend)")
        fn = next_patch_name(pdir, subject)
        with open(os.path.join(pdir, fn), "w") as f:
            f.write(patch_header(subject, body) + delta)
        with open(os.path.join(pdir, "series"), "a") as f:
            f.write(fn + "\n")
        print(f"refresh: captured working delta as {fn} (appended to series)")
    restamp(name)
    print(f"refresh: engine/vendor/{name} left as-is and restamped; "
          f"verify with: python3 scripts/vendor.py check {name}")
    return 0


def cmd_check(name):
    """Reassemble from pins+series into a temp dir and diff against the
    working tree — proves the series reproduces what you're running."""
    base, _ = assemble_baseline(name)
    try:
        d = working_delta(name, base)
        if d.strip():
            sys.stdout.write(d)
            print(f"\ncheck: {name}: series does NOT reproduce the working tree", file=sys.stderr)
            return 1
        print(f"check: {name}: series reproduces the working tree exactly")
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


def cmd_add(name, args):
    manifest_path = os.path.join(ENGINE, "vendor.toml")
    if name in load_manifest():
        sys.exit(f"add: {name} already in vendor.toml — edit its entry to update")
    if "--git" in args:
        url = args[args.index("--git") + 1]
        commit = args[args.index("--commit") + 1]
        subdir = args[args.index("--subdir") + 1] if "--subdir" in args else None
        entry = {"source": "git", "url": url, "commit": commit,
                 "_name": name, **({"subdir": subdir} if subdir else {})}
        lines = [f"[crates.{name}]", 'source = "git"', f'url = "{url}"',
                 f'commit = "{commit}"']
        if subdir:
            lines.append(f'subdir = "{subdir}"')
    else:
        version = args[args.index("--version") + 1]
        os.makedirs(CACHE, exist_ok=True)
        url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
        tmp = os.path.join(CACHE, f"{name}-{version}.crate")
        with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
            shutil.copyfileobj(r, f)
        digest = sha256(tmp)
        entry = {"source": "crates.io", "version": version, "sha256": digest,
                 "_name": name}
        lines = [f"[crates.{name}]", 'source = "crates.io"',
                 f'version = "{version}"', f'sha256 = "{digest}"']
    root = upstream_root(entry)
    pdir = os.path.join(PATCHES, name)
    os.makedirs(pdir, exist_ok=True)
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in
                       (".git", "target", "tests", "benches", "fuzz")]
        for fn in filenames:
            files.append(os.path.relpath(os.path.join(dirpath, fn), root))
    with open(os.path.join(pdir, "files"), "w") as f:
        f.write("\n".join(sorted(files)) + "\n")
    open(os.path.join(pdir, "series"), "a").close()
    with open(manifest_path, "a") as f:
        f.write("\n" + "\n".join(lines) + "\n")
    cmd_assemble(only=name)
    print(f"add: {name} pinned; selection in vendor-patches/{name}/files "
          f"(prune dev-only trees), empty series ready. Hack in "
          f"engine/vendor/{name}, then: scripts/vendor.py refresh {name} -m \"...\"")
    return 0


USAGE = """\
scripts/vendor.py                     assemble engine/vendor/ (default; make vendor)
scripts/vendor.py --force [CRATE]     reassemble even if stamped
scripts/vendor.py diff CRATE          show working-tree delta vs the series
scripts/vendor.py refresh CRATE -m "subject" [-b body]
                                      capture the delta as a NEW series patch
scripts/vendor.py refresh CRATE --amend
                                      fold the delta into the LAST series patch
scripts/vendor.py check CRATE         verify series reproduces the working tree
scripts/vendor.py add CRATE --version V | --git URL --commit C [--subdir D]
                                      pin a new upstream (writes vendor.toml + files)
The everyday loop: make vendor → edit engine/vendor/CRATE until green →
vendor.py refresh CRATE. Updating a pin: edit vendor.toml, make vendor,
re-spin any patch that fails to apply. Details: engine/vendor-patches/README.md
"""


def main():
    args = sys.argv[1:]
    if not args or args == ["--force"] or (args[0] == "--force"):
        only = args[1] if len(args) > 1 else None
        return cmd_assemble(force="--force" in args, only=only)
    cmd, rest = args[0], args[1:]
    if cmd in ("-h", "--help", "help"):
        print(USAGE)
        return 0
    if cmd == "diff":
        return cmd_diff(rest[0])
    if cmd == "check":
        return cmd_check(rest[0])
    if cmd == "refresh":
        name = rest[0]
        subject = rest[rest.index("-m") + 1] if "-m" in rest else None
        body = rest[rest.index("-b") + 1] if "-b" in rest else ""
        return cmd_refresh(name, subject, body, amend="--amend" in rest)
    if cmd == "add":
        return cmd_add(rest[0], rest[1:])
    print(USAGE, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main() or 0)
