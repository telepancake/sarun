# Latvian render comparison corpus

These are fixed-revision HTML pairs captured on 2026-07-30 from the local
`lvwiki-20260729-refprefix16m-128k` mirror and from `lv.wikipedia.org`.  The
official files use the same revision IDs as the local archive, so differences
are attributable to rendering or to data deliberately unavailable offline,
not to unrelated edits made later.

Each `.html.gz` file is a complete response.  The pair names and source
revision IDs are:

| page | revision |
| --- | ---: |
| `sakumlapa` | 3673304 |
| `latvija` | 4479260 |
| `matematika` | 4255661 |
| `riga` | 4484408 |
| `category-politika` | 1943948 |

The `.local.html.gz` files were captured after the site-direction, magic-word,
page-count, and Wikibase-property fixes in the same change set.  Decompress
the pair locally for a DOM/text comparison; no network request is needed.
