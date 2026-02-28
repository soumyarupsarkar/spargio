# Companion Stability

Companion crates follow semver independently from core runtime internals.

Baseline policy:

- Breaking API changes require a major bump for the affected companion crate.
- Behavior changes in timeout/cancel/error mapping are treated as semver-relevant.
- New adapters should be additive where possible.

Deprecation policy:

- Deprecate first, remove later in the next major.
- Keep compatibility shims for one minor cycle when feasible.
- Document migration replacements in this book before removal.

Operational policy:

- Companion crates are covered by `scripts/companion_ci_smoke.sh` and CI
  `companion-matrix` lane.
- Version drift with upstream protocol crates should be tracked with explicit
  compatibility updates in `IMPLEMENTATION_LOG.md`.
