#!/usr/bin/env python3
"""Write deterministic third-party notices beside a Sarun release artifact."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


REPO = Path(__file__).resolve().parent.parent
MANIFEST = REPO / "engine" / "Cargo.toml"
VENDOR = REPO / "engine" / "vendor"
NOTICE_PREFIXES = ("LICENSE", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE")


def notice_files(package):
    directory = Path(package["manifest_path"]).resolve().parent
    paths = []
    declared = package.get("license_file")
    if declared:
        paths.append(directory / declared)
    for path in directory.iterdir():
        if path.name.upper().startswith(NOTICE_PREFIXES):
            paths.append(path)
    unique = {}
    for path in paths:
        if path.is_file():
            unique[path.resolve()] = path
    return [unique[key] for key in sorted(unique, key=str)]


def is_third_party(package):
    if package.get("source"):
        return True
    directory = Path(package["manifest_path"]).resolve().parent
    return directory.is_relative_to(VENDOR)


def normalized_bytes(path):
    return path.read_bytes().replace(b"\r\n", b"\n")


def rust_bundle(target):
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1",
        "--filter-platform", target, "--manifest-path", str(MANIFEST),
    ]))
    packages = []
    texts = {}
    for package in metadata["packages"]:
        if not is_third_party(package):
            continue
        notices = []
        for path in notice_files(package):
            content = normalized_bytes(path)
            digest = hashlib.sha256(content).hexdigest()
            texts[digest] = content
            notices.append((path.name, digest))
        license_value = package.get("license") or ""
        if not notices and not license_value:
            raise SystemExit(
                f"{package['name']} {package['version']} has neither a notice "
                "file nor Cargo license metadata"
            )
        packages.append({
            "name": package["name"],
            "version": package["version"],
            "license": license_value or "see packaged notice",
            "source": package.get("source") or "vendored pinned source",
            "repository": package.get("repository") or "",
            "authors": package.get("authors") or [],
            "notices": notices,
        })
    packages.sort(key=lambda row: (row["name"], row["version"], row["source"]))

    lines = [
        "Sarun third-party Rust dependency notices",
        f"Cargo target: {target}",
        "Generated from Cargo.lock metadata and the exact packaged source trees.",
        "A package with no shipped notice file retains its declared Cargo license",
        "and authors here rather than silently disappearing from the inventory.",
        "",
        "PACKAGE INVENTORY",
        "=================",
    ]
    for package in packages:
        lines.extend([
            "",
            f"{package['name']} {package['version']}",
            f"  license: {package['license']}",
            f"  source: {package['source']}",
        ])
        if package["repository"]:
            lines.append(f"  repository: {package['repository']}")
        if package["authors"]:
            lines.append("  authors: " + "; ".join(package["authors"]))
        if package["notices"]:
            for name, digest in package["notices"]:
                lines.append(f"  notice: {name} sha256:{digest}")
        else:
            lines.append("  notice: no package-specific notice file was shipped")

    lines.extend(["", "", "DEDUPLICATED NOTICE TEXTS", "========================="])
    output = "\n".join(lines).encode()
    for digest in sorted(texts):
        output += (
            f"\n\n--- sha256:{digest} ---\n".encode()
            + texts[digest].rstrip(b"\n")
            + b"\n"
        )
    return output


def replace_if_changed(path, content):
    if path.exists() and path.read_bytes() == content:
        return
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_bytes(content)
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    for name in ("SWI-Prolog.txt", "zlib.txt"):
        source = REPO / "LICENSES" / name
        destination = args.output / name
        if not destination.exists() or destination.read_bytes() != source.read_bytes():
            shutil.copyfile(source, destination)
    replace_if_changed(args.output / "Rust-dependencies.txt", rust_bundle(args.target))


if __name__ == "__main__":
    main()
