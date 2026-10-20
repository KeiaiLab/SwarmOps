# Releasing SwarmOps

Nothing has been released yet. This is the checklist for the first release and every one after
it. What may change between versions is in
[docs/policies/versioning-and-stability.md](docs/policies/versioning-and-stability.md).

## Not there yet

- **The release workflow is deferred to the first tag.** `.github/workflows/release.yml` is
  added together with the first release, not before. It will run on a pushed `v*` tag and:
  refuse to run unless the tag matches the workspace version; build the two Linux binaries,
  `swarmops` and its short form `so`, with `--locked` on the pinned toolchain; write a SHA-256
  checksum for each archive; attest the archives' build provenance with
  `actions/attest-build-provenance`; and create a draft GitHub release. Like CI, it pins every
  action by commit, starts from `permissions: {}` and grants `contents: write`,
  `id-token: write` and `attestations: write` only to the job that needs them, and zizmor must
  pass on it.
- **Provenance attestations need a public repository.** On GitHub's Free plan, artifact
  attestations are available for public repositories only
  ([GitHub docs](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)).
  Release from the repository once it is public, or ship checksums only and say so in the
  release notes.
- **No crates.io publishing.** Every crate is `publish = false` until a release policy has been
  exercised on binary releases. Publishing a crate needs a decision of its own, and its own
  version and MSRV policy.
- **Before the first release that promises store compatibility**, add a test that opens a store
  written by the previous release (from 1.0 on, the versioning policy asks for one).
- **Once the repository is public**, turn on private vulnerability reporting
  ([SECURITY.md](SECURITY.md)).

## Checklist

1. **Pick the commit.** Everything the release needs is merged on `stable`, and `tests-pass` is
   green for that commit.
2. **Run the gates on that exact commit**, in a clean checkout:
   - `just check`;
   - `just check-slow`, the full slow suite with the 32-process contention measurements;
   - `just fuzz <target>` for each of `codec_roundtrip`, `tree_decode`, `frame_decode` and
     `journal_recovery`, or a green scheduled fuzz run for the same code;
   - `just perf` in a quiet window if storage code changed since the last release, with the
     results added to `docs/perf.md`.
3. **Check the public surfaces** against the last release (`git diff <last tag>..HEAD`):
   - the golden vectors in `crates/swarmops-core/tests/golden/` did not change;
   - `FORMAT` was bumped if the store layout changed;
   - every change to `crates/swarmops-cli/tests/snapshots/` (the schema, the MCP tools, the CLI
     output) is intended and in the changelog;
   - the envelope's `api` number went up if a response broke.
4. **Write the changelog.** Move the entries under **Unreleased** in
   [CHANGELOG.md](CHANGELOG.md) to a new `## [X.Y.Z] - YYYY-MM-DD` section, mark breaking
   changes, add the compare link at the bottom, and leave an empty **Unreleased** section.
5. **Bump the version** in `[workspace.package]` of `Cargo.toml` and in the
   `version = "X.Y.Z"` of every internal dependency in `crates/*/Cargo.toml`,
   `fuzz/Cargo.toml` and `bench/compare/Cargo.toml`; let `cargo check` update `Cargo.lock`.
   The two other workspaces' lockfiles record the path crates' version too and fail `--locked`
   until they are updated:
   `cargo update --manifest-path fuzz/Cargo.toml -p swarmops-core -p swarmops-store -p swarmops-testkit`,
   and `cargo update --manifest-path bench/compare/Cargo.toml -p swarmops-core -p swarmops-store`
   (`-p` leaves every other pinned entry as it is; `just check` gates only the fuzz lockfile,
   `just bench-compare` the other). Run `just check` again. Commit: `chore: release X.Y.Z`.
6. **Tag** that commit with an annotated, signed tag, and push it only once CI for the commit is
   green:

   ```sh
   git tag -s vX.Y.Z -m "SwarmOps X.Y.Z"
   git push origin vX.Y.Z
   ```

7. **Publish.** The release workflow builds, checksums and attests the archives into a draft
   release. Check an archive's provenance
   (`gh attestation verify <archive> --repo KeiaiLab/SwarmOps`), paste the changelog section into
   the release notes, and publish the draft.
8. **Afterwards**, check that `stable` carries the release commit and the tag, and that the next
   change starts a new **Unreleased** entry.
