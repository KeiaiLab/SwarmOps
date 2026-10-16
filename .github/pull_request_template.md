## Summary

<!-- What this changes and why. Link the plan task or the issue. -->

## Spec

<!-- The spec paragraph this follows, or the one it changes in this pull request. -->

## Tests

<!-- New tests by name; `just check`; `just check-slow` for the store's write, recovery or locking paths. -->

## Checklist

- [ ] `just check` passes, and `tests-pass` is green on this branch.
- [ ] A behaviour change updates its spec paragraph in this pull request.
- [ ] The hash contract and the golden vectors are untouched; a new layout has a new kind/version header or a `FORMAT` bump.
- [ ] Every snapshot change (the schema, the MCP tools, CLI output) and every pinned filesystem-call sequence that changed is intended and named above.
- [ ] `CHANGELOG.md` has an **Unreleased** entry for a user-visible change.
- [ ] `docs/known-limitations.md` loses the entries this closes, or gains the ones it opens.
- [ ] Every tool-assisted commit carries a `Co-Authored-By:` trailer naming its model.
