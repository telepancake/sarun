#!/usr/bin/env python3
"""Refresh the checked-in Wikipedia mirror catalog from Wikimedia."""

from __future__ import annotations

import datetime
import html.parser
import json
import pathlib
import urllib.request


DUMP_INDEX = "https://dumps.wikimedia.org/backup-index-bydb.html"
SITE_MATRIX = (
    "https://meta.wikimedia.org/w/api.php"
    "?action=sitematrix&format=json&formatversion=2"
)
OUTPUT = (
    pathlib.Path(__file__).resolve().parents[1]
    / "engine"
    / "data"
    / "wikipedia-sites.tsv"
)


class DumpIndexParser(html.parser.HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.databases: set[str] = set()

    def handle_starttag(
        self, tag: str, attrs: list[tuple[str, str | None]]
    ) -> None:
        if tag != "a":
            return
        href = dict(attrs).get("href", "")
        parts = href.split("/") if href else []
        if len(parts) == 2 and parts[1].isdigit():
            self.databases.add(parts[0])


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "sarun-catalog/1"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read()


def main() -> None:
    parser = DumpIndexParser()
    parser.feed(fetch(DUMP_INDEX).decode("utf-8"))
    matrix = json.loads(fetch(SITE_MATRIX))
    rows: list[tuple[str, str, str]] = []
    for key, language in matrix["sitematrix"].items():
        if not key.isdigit():
            continue
        for site in language.get("site", []):
            dbname = site.get("dbname", "")
            if site.get("code") == "wiki" and dbname in parser.databases:
                rows.append(
                    (
                        dbname,
                        language.get("localname") or language.get("name") or dbname,
                        "closed" if site.get("closed") else "open",
                    )
                )
    rows.sort()
    today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    lines = [
        f"# generated {today}",
        f"# dumps: {DUMP_INDEX}",
        f"# names: {SITE_MATRIX}",
        "# dbname\tlanguage\tstatus",
        *(f"{dbname}\t{name}\t{status}" for dbname, name, status in rows),
        "",
    ]
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text("\n".join(lines), encoding="utf-8")
    print(f"{OUTPUT}: {len(rows)} Wikipedia sites")


if __name__ == "__main__":
    main()
