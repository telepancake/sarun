# The depot abstraction — design brief

The north star for this integration. Recorded from the owner's directive; the
analysis below is a starting frame, **not** a settled design. Pin the data model
before the trait (this repo's CLAUDE.md §2).

## The directive (verbatim intent)

> First we need to create an abstraction of a "depot". It should encapsulate the
> storage scheme we need for current and coming data: filesystem layers, git
> repositories, website archives, columnar data from the current sqlar.
>
> Referring to a blob by hash is something a depot *can use to implement itself*,
> not a universal interface. If a depot does implement some hashing scheme, it
> works as a filename scheme.
>
> What it DOES need is the ability to represent tree-like structures of named
> blobs, with blobs being able to attach not only to leaves, and to support the
> "whiteout/tombstone" semantics natively.
>
> This is strictly a superset of a plain git packfile plus index — so a depot
> variant implemented over git will need some coding.

## What the interface is (and is not)

- **Addressed by NAME, not by hash.** The surface is a path/name-keyed tree.
  Content-addressing is a *backend* choice; when a variant hashes, the hash is a
  filename/dedup key underneath, never part of the caller's vocabulary. This is
  the load-bearing correction: do not leak hashing into the trait.
- **A named tree where any node may carry blob content.** Not git's split of
  pure-directory trees vs leaf-only blobs — here an interior node can be *both* a
  named subtree *and* hold bytes. (Superset of git's object model.)
- **Tombstones are first-class.** Deletion is a recorded whiteout, not absence —
  so a layer can mask a name present in a lower layer. This is exactly sarun's
  overlay whiteout semantics, promoted into the store's data model.

So the minimal node shape to pin first (before any method signatures):

```
Node = name + optional blob bytes + ordered children + presence{live | tombstone}
```

and the operation set the four workloads actually need — get(path), put(path,
blob?), list(path), delete(path)=tombstone, iterate/snapshot — weighted by
frequency, before choosing any on-disk layout (§2 steps 4–6).

## The four data kinds → depot variants

The depot is a **trait**; these are **implementations**, each a layout tuned to a
shape. One interface, several backends.

| data kind | today | depot variant | why it needs coding |
|---|---|---|---|
| filesystem layers (box overlays) | sarun overlay + sqlar | **native** — an overlay upper already *is* a named tree with whiteouts | this shape is the depot's home ground; mostly a re-expression |
| git repositories (mirrors) | shared git object depot (alternates) | **git-backed** | git trees can't carry interior-node blobs or native tombstones; the mirror fakes deletion via a private ref. The "superset of packfile+index" is precisely the interior-blob + tombstone delta to add over git objects |
| website archives (captures over time) | *(unbuilt)* | **tiered-VBF** (`wikimak/depot`) — revision chains per named resource | content-addressing explodes on deep near-identical revisions (SCOPING.md); delta-chain instead |
| columnar data from sqlar | SQLite sqlar (path→blob) | **columnar / lighter-KV backend** | sqlar is already a flat named-blob store; behind the depot interface a non-SQLite backend can replace it while the overlay addresses it by name |

Blob-by-hash (a CAS) is itself one more variant — the dedup backend for fs
layers / git objects / OCI — *chosen* under the interface, not exposed by it.

## Where the existing material fits

- `wikimak/depot` — a working tiered store (revision chains), zstd-opaque,
  tested. It is **one variant**, not the abstraction. Reading its SPEC + acceptance
  suite is the fastest way to see the append/read-current/read-history/seal shape
  the website-archive variant needs.
- `strpool` — string→dense-id interning; the sidecar any variant uses to turn
  string names into the integer keys a flat index wants.
- The design docs (`docs/`, `notes/gimir-design.md`) — the owner's data-layout
  reasoning; the discipline the depot trait must not betray.

## Open questions to settle before committing the trait (do not guess)

- The exact `Node` presence/blob/children shape, and whether snapshots are
  first-class (versioned tree roots) or a variant concern.
- Whether the trait exposes *layering* (stacked depots with whiteout resolution)
  or leaves stacking to the caller — sarun's overlay wants layering.
- Which operations must be atomic at the interface (a tree commit / index flip)
  vs. backend-specific.
- How the git-backed variant encodes interior-node blobs + tombstones over git
  objects without breaking `git` compatibility for the mirror use.
- Sizing per workload (§2 step 3) before any variant's widths/thresholds.
