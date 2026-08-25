# Reference-attach implementation plan (chips 3–4 of ATTACH-CONVERGENCE.md)

Working plan, 2026-07-05. Anchors verified against the tree at time of
writing; re-grep before editing.

## Bookkeeping (Q1)
ro_attachments meta JSON becomes a heterogeneous array: ints = box ids
(unchanged), objects = external refs
`{"kind","store","ref","rev","prefix","name"}`.
- rev pins: git = full commit sha (content-addressed — survives
  re-import or surfaces as a named error; NEVER a frame index, update()
  prepends frames); wiki = head.rev_id; ietf = head draft rev.
- name = display string, format preserved verbatim (git:label/ref@sha8).
- capture.rs:255 field → Mutex<Vec<RoAttachment>>;
  enum RoAttachment { Box(i64), Ext(ExtRef) } with untagged serde
  (number→Box, object→Ext): old metas parse unchanged, int-only lists
  serialize byte-identically.
- Accessors split: ro_attachment_box_ids() (hydrate + ro_attach verb)
  vs ro_attachment_list().
- Rev resolution in the attach verbs; hydrate only parses JSON — store
  re-opened lazily at first use.

## Overlay integration (Q2)
enum ChainLink { Box(Arc<BoxState>), Ext(Arc<ExtAttachment>) } — scoped
to the overlay (chain_of is the single funnel, overlay.rs:981). Do NOT
trait-object BoxState (~40 box_of consumers).

ExtAttachment (new engine/src/attach.rs): ext ref +
OnceLock<Result<Arc<dyn Readout>,String>> (opened on first use) +
RwLock<HashMap<String,Option<ExtEntry>>> memo (sound: attachment
immutable at pinned rev) + prefix fast-path (rel not under prefix →
None, no readout touch). Inner.ext: RwLock<HashMap<(owner,slot),
Arc<ExtAttachment>>>, populated by hydrate_chain (no I/O).

Touch points (grep-verified): chain_of :981 → Vec<ChainLink>; resolve
:1034-1093 Ext arm → existing Layer variants + NEW Layer::ExtFile{att,
rel,size,mode}; ro_denied :1001 Ext arm = att.entry(rel).is_some();
chain_dir_has_children :1124; scan_dir :1320-1399 (merge children as
present names — attachments have no whiteouts/holes/opaque);
hydrate_chain :729-758 + add_box :719 registration; Layer::UpperFile
consumers get ExtFile arms: box_read_file :783, box_file_mode :804,
box_path_kind :865, attr_of :1256 (uses carried size/mode — getattr
NEVER materializes), copy_up :1286 (defensive EROFS arm — unreachable),
open :1594. readlink :1494 untouched. The ~12 ro_denied mutation call
sites keep their signature.

## Blob serving / depot-cache (Q3)
Only open() needs a real fd (Fh stores File, read_at; mmap/exec go
through it; kernel passthrough fds only for passthrough-ruled paths
:1636, never attachments). open() ExtFile arm: Blob::File(p) → open
store loose file directly; Blob::Bytes(b) → depot_cache file_for(&b)
→ open pool path RO. Hashing stays internal to the cache (§7 sanctions
it; overlay never sees a hash). Cache root state_home()/cache (already
hidden by is_engine_path :550); one Cache per Inner (OnceLock).
Eviction: evict_unreferenced() at startup + after detach/box-delete;
never on the FUSE path. box_read_file: Bytes direct, File → fs::read.

## Attach verbs (Q5)
git_attach: read_meta only (ref→sha resolve + membership check) — no
read_store, no layer build. wiki_attach: page_head for rev_id only.
ietf_attach: head rev via head_layer (NOT m.history()). Each: push
Ext row, register live, return {"ok","name","rev"} — no "box" id.
attach_ro_layer deleted (three verbs are its only callers);
import_layer STAYS (rotation/dissolve control.rs:2042/2050 + test).
UI: discover.rs:141-145 stops assuming ints — Box rows extend parents,
Ext rows → new session-dict "attachments":[{name,kind,rev,error?}]
rendered inline on the owning session row.

## Failure modes (Q6)
Store gone at first use: entry/children → empty (resolve falls
through; §8 independence keeps captured layer valid), blob on resolved
entry → EIO, error surfaced in "attachments" + log. Never brick box
open. Concurrent updates: wiki/ietf append-only; gitdepot update()
prepends (old frames verbatim); mirror re-import → sha survives
byte-identical or lookup fails with the sha named.

## Ordered commits
1. capture.rs bookkeeping enum + accessors split; unit test old-format
   round-trip; suites unchanged.
2. engine/src/attach.rs (ExtAttachment, per-kind lazy open vs chip-1/2
   Readout, memo, prefix guard) + hydrate/add_box registration; unit
   tests vs tiny fixture stores. Nothing consumes it yet.
3. overlay.rs: ChainLink + chain_of + resolve Ext arm + Layer::ExtFile
   + chain_dir_has_children + non-fd consumers (attr_of/box_read_file/
   box_file_mode/box_path_kind/copy_up-defensive).
4. overlay.rs: scan_dir + ro_denied Ext arms; EROFS + merged-listing
   tests; test_ro_attach_rs.py rerun.
5. depot-cache wiring: open() ExtFile arm + Cache in Inner + eviction
   hooks + Cargo dep. cat/mmap/exec through mount; repeat-read hits
   same pool path.
6. control.rs verb rewrite + attach_ro_layer deletion + discover.rs
   attachments field + UI row rendering; update test_git_attach_rs.py
   / test_mirror_attach_rs.py (box-count/name assertions move to
   "attachments").
7. prototype/test_attach_convergence_rs.py: §8 byte-identical invariant
   (same workload with/without attachment → identical captured sqlar
   modulo rowids) + laziness (attach big store: no new *.sqlar, no
   cache blob until first read); wire into Makefile.
