# Phase 5 — Evaluate on real git repositories

Read `gitdepot/pipeline/00-context.md`, `gitdepot/DESIGN.md`, and
`gitdepot/VALIDATION.md`.

Measure the engine on real repositories (use `/home/user/sarun` itself, and any
other real repo available locally), reading results back out of the stored data:

- reconstruct trees from what is stored and check them against git's own object
  ids;
- stored size against git's packfile for the same refs;
- memory during import;
- how time and stored size grow with the number of commits.

Report the numbers to `gitdepot/EVALUATION.md`. State plainly where it wins,
where it loses, and by how much.
