# Releasing SwarmOps

SwarmOps 0.1.0 is the first release. This is the checklist for it and for every one after it.
What may change between versions is in
[docs/policies/versioning-and-stability.md](docs/policies/versioning-and-stability.md).

A release is an annotated `vX.Y.Z` tag on a commit of `stable`, unsigned until a signing key is
registered (step 7). Pushing the tag runs
[`.github/workflows/release.yml`](.github/workflows/release.yml), which publishes the GitHub
release `vX.Y.Z` on the [releases page](https://github.com/KeiaiLab/SwarmOps/releases) if every
check below passes, and nothing otherwise.

## What the release workflow does

On a pushed `v*` tag, with no secret but each job's own token, on the maintainers' own host:
every job runs on the self-hosted runners labelled `swarmops-local` that run CI, none on
GitHub's hosted runners.

1. **Checks the tag** (job `tag, version and notes`): the tag is `v` and the workspace version of
   `Cargo.toml` (`v0.1.0` for `version = "0.1.0"`); `CHANGELOG.md` has a non-empty
   `## [X.Y.Z] - YYYY-MM-DD` section, which becomes the release notes (its relative links made
   absolute, to the tag's files, then a line on checking a download against `SHA256SUMS`; at
   most the 125,000 characters a release takes); and `stable` holds the tagged commit. No other
   job starts unless these pass.
2. **Runs the release gate** (job `just check`): `just check` on the tagged commit, with the tools
   and versions of CI's `check` job but without its cache or the runner's target directory, so
   that it builds from clean.
3. **Builds the binaries** (job `build <target>`, one per target): `swarmops` and `so` with
   `cargo build --release --locked` on the pinned toolchain, without the `test-hooks` feature, for
   `x86_64-unknown-linux-gnu`, natively, and `aarch64-unknown-linux-gnu`, cross-compiled with
   Ubuntu's aarch64 GCC, each inside an `ubuntu:24.04` container pinned by digest (glibc 2.39),
   which installs the compilers, QEMU and the pinned toolchain and builds into its own
   filesystem. Each binary is smoke-run, the aarch64 ones under QEMU's user-mode
   emulator: it must print the tag's version, need no glibc newer than 2.34, and `init`,
   `commit`, `show`, `cat` and `verify --deep` a store with every `SWARMOPS_TEST_*` hook set,
   which a release build ignores. Each target is packed into `swarmops-X.Y.Z-<target>.tar.gz`
   (both binaries, `LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md`; entries
   sorted, owned by root and dated at the tagged commit), with its SHA-256, and the packing fails
   unless the archive holds exactly these files. `THIRD-PARTY-LICENSES` gives each third-party
   crate compiled into the binaries with its license and the license texts; it is committed, and
   `just check`, which the release gate runs, fails when it no longer matches `Cargo.lock`.
4. **Makes and installs the packages** (job `packages <target>`, one per target):
   [`scripts/packages.py`](scripts/packages.py) makes `swarmops_X.Y.Z_<arch>.deb` and
   `swarmops-X.Y.Z-1.<arch>.rpm` (`amd64` and `x86_64`, `arm64` and `aarch64`) from the target's
   archive alone, checked against its SHA-256 first: the archive's binaries, byte for byte, in
   `/usr/bin`, and its `LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md` in
   `/usr/share/doc/swarmops`; entries sorted, owned by root, dated at the tagged commit, with fixed
   modes, so that the same archive gives the same packages, byte for byte. Their dependencies are
   the binaries' own, read from their ELF dynamic sections: glibc at the newest version they need
   that is not weak (`libc6 (>= 2.34)`), and libgcc. Then each package is installed in a container
   pinned by digest, the `.deb` with `dpkg` in Debian 12 and the `.rpm` with `rpm` in AlmaLinux 9,
   whose glibc 2.34 is the oldest the binaries run on: `rpm` checks every digest of the `.rpm`,
   both installed binaries must print the tag's version, `dpkg --verify` and `rpm --verify` must
   find every file as packaged, and removing the package must remove its files. The aarch64
   packages are installed for aarch64, in Debian with that architecture's C library (multiarch)
   and into an AlmaLinux root of that architecture, against whose glibc `rpm` checks the
   dependencies, and run under QEMU's user-mode emulator; these two containers download from
   Debian's and AlmaLinux's mirrors. The job runs on the runner, which starts the containers;
   they mount the packages read-only.
5. **Publishes** (job `publish the release`, the only one with a write permission,
   `contents: write`, and only for a pushed tag): checks each archive and package against the
   SHA-256 its job wrote, writes `SHA256SUMS`, and creates the release `vX.Y.Z`, titled
   `SwarmOps X.Y.Z`, with the archives, the packages, `SHA256SUMS` and the notes. A version with a
   pre-release part, such as `0.2.0-rc.1`, is published as a pre-release; its packages carry the
   version as `0.2.0~rc.1`, which `dpkg` and `rpm` order before `0.2.0`, and their file names keep
   `0.2.0-rc.1`.

**A rehearsal** runs the workflow by hand on a branch, `gh workflow run release.yml --ref <branch>
--repo KeiaiLab/SwarmOps`: it builds, smoke-runs, packs, makes the packages of and installs them for
the workspace version of the branch's commit, and publishes nothing: `publish` runs for a pushed
`v*` tag only, and the tag's checks and `just check` (which CI runs on every push) are skipped. The
run's artifacts hold its archives and packages for 14 days.

Like CI, the workflow pins every action by commit, starts from `permissions: {}`, keeps no
credentials in its checkouts, and CI's zizmor job audits it. The container images it builds and
installs in are pinned by digest, the build image in the workflow and the install images in
`scripts/packages.py`; Dependabot moves none of them. No job restores a cache: what a
release ships is built from the tagged commit and `Cargo.lock` alone. Its `just check` job
installs the same tool versions as `ci.yml`'s: move them together.

## Not there yet

- **No provenance attestations.** On GitHub's Free plan, artifact attestations are available
  for public repositories only
  ([GitHub docs](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)),
  and v0.1.0 was released while the repository was private, so its archives ship with SHA-256
  checksums only. The repository is public now: the publishing job can attest a release's
  archives with `actions/attest-build-provenance`, which needs `id-token: write` and
  `attestations: write` there too, and step 9 below gains
  `gh attestation verify <archive> --repo KeiaiLab/SwarmOps`.
- **Linux today (x86_64 and aarch64, glibc 2.34 or newer); macOS and Windows are milestone M12,
  developed separately.** The aarch64 binaries are
  cross-compiled and only smoke-tested, under QEMU; the test suite runs on x86_64
  ([docs/policies/msrv-and-platforms.md](docs/policies/msrv-and-platforms.md)). There is no
  macOS, Windows or static (musl) build.
- **Packages are files on the release, not a repository.** There is no apt or dnf repository to
  add, so `apt upgrade` and `dnf upgrade` do not see a new release: install the next release's
  package. The packages carry no signature of their own (`rpm --checksig` finds their digests,
  not a key's signature): check them against `SHA256SUMS`. No npm or PyPI package yet: those need
  registry accounts.
- **No crates.io publishing.** Every crate is `publish = false` until a release policy has been
  exercised on binary releases. Publishing a crate needs a decision of its own, and its own
  version and MSRV policy.
- **Before the first release that promises store compatibility**, add a test that opens a store
  written by the previous release (from 1.0 on, the versioning policy asks for one).

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

   The release workflow runs only `just check` again, on a runner that shares the host with
   CI's other runners: too contended for the 32-process measurements and `just perf`.
3. **Check the public surfaces** against the last release (`git diff <last tag>..HEAD`):
   - the golden vectors in `crates/swarmops-core/tests/golden/` did not change;
   - `FORMAT` was bumped if the store layout changed;
   - every change to `crates/swarmops-cli/tests/snapshots/` (the schema, the MCP tools, the CLI
     output) is intended and in the changelog;
   - the envelope's `api` number went up if a response broke;
   - the crates and licenses that `THIRD-PARTY-LICENSES` adds are ones the archives may carry
     (`scripts/third-party-licenses.py` writes it from `Cargo.lock`).
4. **Write the changelog.** Move the entries under **Unreleased** in
   [CHANGELOG.md](CHANGELOG.md) to a new `## [X.Y.Z] - YYYY-MM-DD` section, mark breaking
   changes, and leave an empty **Unreleased** section. The section's date is the UTC date of the
   release commit, the last commit before the tag. At the bottom, add the new section's link,
   `[X.Y.Z]: https://github.com/KeiaiLab/SwarmOps/compare/vA.B.C...vX.Y.Z` from the previous
   release `vA.B.C` (`[0.1.0]: https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.1.0` for the
   first), and point `[Unreleased]` at `compare/vX.Y.Z...stable`. The new section is the
   release's notes: the workflow refuses a tag without one. The first release also brings up to
   date what says nothing is released yet: README's status badge, status line, maturity row and
   install section, the README's documentation table, `docs/operations.md`'s install section,
   SECURITY.md's supported versions, CHANGELOG.md's introduction, the
   versioning policy's first paragraph, this file's first line, the Maturity row of `web/content/product/vs-git.md`
   and the issue template's contact text (`.github/ISSUE_TEMPLATE/config.yml`).
5. **Bump the version** in `[workspace.package]` of `Cargo.toml` and in the
   `version = "X.Y.Z"` of every internal dependency in `crates/*/Cargo.toml`,
   `fuzz/Cargo.toml` and `bench/compare/Cargo.toml`; let `cargo check` update `Cargo.lock`.
   The two other workspaces' lockfiles record the path crates' version too and fail `--locked`
   until they are updated:
   `cargo update --manifest-path fuzz/Cargo.toml -p swarmops-core -p swarmops-store -p swarmops-testkit`,
   and `cargo update --manifest-path bench/compare/Cargo.toml -p swarmops-core -p swarmops-store`
   (`-p` leaves every other pinned entry as it is; `just check` gates only the fuzz lockfile,
   `just bench-compare` the other). Run `just check` again. The first release, 0.1.0, is the
   version the workspace already has, so it skips this step. Commit the changelog and the bump
   (for 0.1.0, the changelog and the first release's updates): `chore: release X.Y.Z`.
6. **Land that commit on `stable`** and wait for `tests-pass` to be green for it. Push `stable`
   before the tag: the workflow refuses a tagged commit that `stable` does not hold. Then rehearse
   the release on it, `gh workflow run release.yml --ref stable --repo KeiaiLab/SwarmOps`, and
   wait for its build and packages jobs to pass: a packaging failure found there costs no tag.
7. **Tag** the commit with an annotated tag, unsigned until a signing key is registered (then
   `git tag -s`), and push the tag:

   ```sh
   git tag -a vX.Y.Z -m "SwarmOps X.Y.Z"
   git push origin vX.Y.Z
   ```

8. **Watch the release run** (`gh run list --repo KeiaiLab/SwarmOps --workflow release.yml`,
   then `gh run watch <run id> --repo KeiaiLab/SwarmOps`). When it fails, nothing is published:
   - a flaky job: run the failed jobs again
     (`gh run rerun <run id> --failed --repo KeiaiLab/SwarmOps`);
   - a defect: fix it on `stable`, delete the tag (`git push origin :refs/tags/vX.Y.Z`, then
     `git tag -d vX.Y.Z`), and tag the fixed commit;
   - `publish the release` itself: delete the draft release it may have left
     (`gh release delete vX.Y.Z --repo KeiaiLab/SwarmOps`), then run the job again.
9. **Check the release** as a user would: download an archive and `SHA256SUMS` from the
   releases page, run `sha256sum --check --ignore-missing SHA256SUMS`, unpack the archive and
   run `swarmops --version`. Then install a package the same way, checked against the same
   `SHA256SUMS`, on a Debian or Ubuntu host (`sudo apt install ./swarmops_X.Y.Z_amd64.deb`) or a
   RHEL-family one (`sudo dnf install ./swarmops-X.Y.Z-1.x86_64.rpm`), run `so --version`, and
   remove it (`sudo apt remove swarmops`, `sudo dnf remove swarmops`).
10. **Afterwards**, check that `stable` carries the release commit and the tag, and that the next
    change starts a new **Unreleased** entry.
