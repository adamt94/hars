<!-- Title: Conventional Commits, e.g. `feat(met): ...`, `fix(ws): ...`. It becomes the squash commit. -->

## What and why

<!-- What changes for a user or a contributor, and why. Link the roadmap item or issue. -->

## How it was tested

<!-- Which tests cover each behaviour change. Vectors: which set, and the scripts/vectors/ script that made it. -->

## Performance evidence

<!-- What docs/PERF.md#what-a-pull-request-must-show asks for this kind of change. Write "none needed: <reason>" if nothing applies. -->

## Checklist

- [ ] `scripts/check.sh` passes (fmt, clippy `-D warnings`, tests, script tests)
- [ ] Every behaviour change has a test; values that must match HA come from vectors made by a script in `scripts/vectors/`
- [ ] Performance evidence above, and a `docs/perf/RESULTS.md` entry if the numbers moved
- [ ] Docs updated: roadmap box ticked, `docs/ARCHITECTURE.md` if structure changed, README "works today" if users see it
- [ ] No tokens, keys, personal data or real coordinates in fixtures
