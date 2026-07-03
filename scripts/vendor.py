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
    h = hashlib.sha256(repr(sorted(entry.items())).encode())
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
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(src, dst, follow_symlinks=False)
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


def main():
    force = "--force" in sys.argv
    with open(os.path.join(ENGINE, "vendor.toml"), "rb") as f:
        manifest = tomllib.load(f)["crates"]
    os.makedirs(CACHE, exist_ok=True)

    for name, entry in manifest.items():
        want = stamp_of(name, entry)
        stamp = os.path.join(VENDOR, name, ".stamp")
        if not force and os.path.exists(stamp) and open(stamp).read() == want:
            continue
        if entry["source"] == "crates.io":
            root = fetch_crate(name, entry["version"], entry["sha256"])
        else:
            root = fetch_git(entry["url"], entry["commit"])
            if "subdir" in entry:
                root = os.path.join(root, entry["subdir"])
        assemble(name, entry, root)
        with open(stamp, "w") as f:
            f.write(want)
        print(f"vendor: {name}")


if __name__ == "__main__":
    main()
