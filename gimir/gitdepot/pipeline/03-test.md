# Agent 3 — Test

Read `gitdepot/pipeline/00-shared-context.md` and `gitdepot/IMPL-NOTES.md`.

1. Build (exact status, report verbatim):
   `cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang cargo zigbuild --release -p gitdepot --tests --target x86_64-unknown-linux-musl`
2. Run the gitdepot lib tests AND the integration test binaries (all
   `gitdepot-*` under `target/x86_64-unknown-linux-musl/release/deps/`). Report
   pass/fail counts. Do NOT hide or paper over any failure; quote failing output.
3. Add the DECISIVE test: import a REAL multi-branch git repo's history through
   the DESIGN-faithful union + reverse-delta path (fed by the oid tree traversal
   / `cat-file --batch`, NOT `gitsrc` full-tree dumps). Then DROP and REOPEN the
   depot and reconstruct a sample of HISTORICAL commits AND branch tips SHA-exact
   from STORED bytes, comparing to `git rev-parse <c>^{tree}`. The test MUST
   exercise: multiple concurrent lanes, reverse-delta reconstruction of an OLD
   (non-tip) commit, and a cold seal. Fail loudly on any mismatch.

Do NOT change production code to make a test pass (only add/adjust tests). Do NOT
commit. Return: build status, pass/fail counts, the real-git roundtrip result,
and any failures verbatim (trimmed).
