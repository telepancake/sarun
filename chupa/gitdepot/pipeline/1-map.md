# Phase 1 — Map the code against the design

Read `gitdepot/pipeline/00-context.md`, then `gitdepot/DESIGN.md`, then the code.

For each part of the design, find where the code implements it, and record —
with `file:symbol` and the design section — whether it CONFORMS, is PARTIAL, or
DIVERGES. Where the same design part is covered in more than one place, say which
code conforms and how the other diverges. Code that contradicts the design is a
removal candidate; cite the exact design point it violates.

Write the result to `gitdepot/WORKMAP.md`: per design section, what exists and
conforms, what is missing, what diverges (and the removal candidates). Return a
short summary and the first thing to implement.
