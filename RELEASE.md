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

On a pushed `v*` tag, with no secret but each job's own token, on the maintainers' own hosts:
every job runs on the self-hosted runners labelled `swarmops-local` that run CI, but the macOS
build, which runs on the maintainers' Mac, on a self-hosted runner labelled `macos-local`; none
runs on GitHub's hosted runners. That Mac's runner must be registered and online: until it is,
the macOS build waits, and nothing is published without it.

1. **Checks the tag** (job `tag, version and notes`): the tag is `v` and the workspace version of
   `Cargo.toml` (`v0.1.0` for `version = "0.1.0"`); `CHANGELOG.md` has a non-empty
   `## [X.Y.Z] - YYYY-MM-DD` section, which becomes the release notes (its relative links made
   absolute, to the tag's files, then a line on checking a download against `SHA256SUMS`; at
   most the 125,000 characters a release takes); `stable` holds the tagged commit; and every
   golden file of the previous release tag is unchanged (`just golden-frozen`). No other job
   starts unless these pass.
2. **Runs the release gate** (job `just check`): `just check` on the tagged commit, with the tools
   and versions of CI's `check` job but without its cache or the runner's target directory, so
   that it builds from clean.
3. **Builds the binaries** (job `build <target>`, one per Linux target): `swarmops` and `so` with
   `cargo build --release --locked` on the pinned toolchain, without the `test-hooks` feature and
   with `SWARMOPS_BUILD_CHANNEL=release`, which marks a binary as a release's and which no other
   build sets, for `x86_64-unknown-linux-gnu`, natively, and `aarch64-unknown-linux-gnu`,
   cross-compiled with Ubuntu's aarch64 GCC, each inside an `ubuntu:24.04` container pinned by
   digest (glibc 2.39), which installs the compilers, QEMU and the pinned toolchain and builds
   into its own filesystem. Each binary is smoke-run, the aarch64 ones under QEMU's user-mode
   emulator: it must print the tag's version, need no glibc newer than 2.34, and `init`,
   `commit`, `show`, `cat` and `verify --deep` a store with every `SWARMOPS_TEST_*` hook set,
   which a release build ignores. Each target is packed into `swarmops-X.Y.Z-<target>.tar.gz`
   (both binaries, `LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md`; entries
   sorted, owned by root and dated at the tagged commit), with its SHA-256, and the packing fails
   unless the archive holds exactly these files. `THIRD-PARTY-LICENSES` gives each third-party
   crate compiled into the binaries with its license and the license texts; it is committed, and
   `just check`, which the release gate runs, fails when it no longer matches `Cargo.lock`.

   The macOS build (job `build aarch64-apple-darwin`) is the same build for
   `aarch64-apple-darwin`, Apple silicon, natively on the maintainers' Mac and in no container:
   the pinned toolchain with that Mac's Xcode command line tools as linker and C compiler, into a
   target directory of its own, emptied first. Its cargo home is a directory of the run too,
   emptied first, so the crates are downloaded afresh and nothing under `~/.cargo` is read; the
   job fails when a `.cargo/config.toml` lies above the workspace or the runner's environment
   sets `RUSTFLAGS`, `RUSTC_WRAPPER`, `RUSTUP_TOOLCHAIN` or a `CARGO_*` variable. The toolchain
   itself is that Mac's rustup's, the pinned version. `MACOSX_DEPLOYMENT_TARGET=11.0` pins the oldest
   macOS the binaries start on. Each binary must be for arm64 alone, link against nothing outside
   `/usr/lib` and `/System/Library` (`otool -L`), name macOS 11.0 as its minimum
   (`otool -l`, `LC_BUILD_VERSION`), carry the linker's ad-hoc signature (`codesign --verify`),
   print the tag's version, and pass the Linux builds' store test. It is packed into
   `swarmops-X.Y.Z-aarch64-apple-darwin.tar.gz` with the Linux archives' layout by Python's
   `tarfile`, macOS's `tar` having no option to sort or to set owners and dates: entries in byte
   order of their names, owned by 0:0, dated at the tagged commit, with fixed modes, and a gzip
   header without name or time. No Intel (`x86_64-apple-darwin`) build is made, and no package:
   the `.deb` and `.rpm` are Linux's.
4. **Makes and installs the packages** (job `packages <target>`, one per Linux target):
   [`scripts/packages.py`](scripts/packages.py) makes `swarmops_X.Y.Z_<arch>.deb` and
   `swarmops-X.Y.Z-1.<arch>.rpm` (`amd64` and `x86_64`, `arm64` and `aarch64`) from the target's
   archive alone, checked against its SHA-256 first: the archive's binaries, byte for byte, in
   `/usr/bin`, and its `LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md` in
   `/usr/share/doc/swarmops`, where the `.deb` adds `copyright`, both license files in Debian's
   machine-readable copyright format, which images without `/usr/share/doc` keep; entries sorted,
   owned by root, dated at the tagged commit, with fixed modes, so that the same archive gives the
   same packages, byte for byte. Their dependencies are the binaries' own, read from their ELF
   dynamic sections: glibc at the newest version they need that is not weak (`libc6 (>= 2.34)`),
   and libgcc. Then each package is installed in a container
   pinned by digest, the `.deb` with `dpkg` in Debian 12 and the `.rpm` with `rpm` in AlmaLinux 9,
   whose glibc 2.34 is the oldest the binaries run on: `rpm` checks every digest of the `.rpm`,
   both installed binaries must print the tag's version, `dpkg --verify` and `rpm --verify` must
   find every file as packaged, `rpm --verify` must then see a byte appended to an installed
   binary, and removing the package must remove its files. The aarch64 packages are installed for
   aarch64, in Debian with that architecture's C library (multiarch) and into an AlmaLinux root of
   that architecture, against whose glibc `rpm` checks the dependencies, and run under QEMU's
   user-mode emulator; these two containers download from Debian's and AlmaLinux's mirrors. The
   job runs on the runner, which starts the containers; they mount the packages read-only.
5. **Publishes** (job `publish the release`, the only one with a write permission,
   `contents: write`, and only for a pushed tag): checks each archive and package against the
   SHA-256 its job wrote, writes `SHA256SUMS`, and creates the release `vX.Y.Z`, titled
   `SwarmOps X.Y.Z`, with the three archives (two for Linux, one for macOS), the packages,
   `SHA256SUMS` and the notes. A version with a
   pre-release part, such as `0.2.0-rc.1`, is published as a pre-release; its packages carry the
   version as `0.2.0~rc.1`, which `dpkg` and `rpm` order before `0.2.0`, and their file names keep
   `0.2.0-rc.1`.

**A rehearsal** runs the workflow by hand on a branch, `gh workflow run release.yml --ref <branch>
--repo KeiaiLab/SwarmOps`: it builds, smoke-runs, packs, makes the packages of and installs them for
the workspace version of the branch's commit, and publishes nothing: `publish` runs for a pushed
`v*` tag only, and the tag's checks and `just check` (which CI runs on every push) are skipped. The
run's artifacts hold its archives and packages for 14 days. The macOS build is part of a rehearsal,
so one shows whether the Mac's runner is there before a tag is pushed.

Like CI, the workflow pins every action by commit, starts from `permissions: {}`, keeps no
credentials in its checkouts, and CI's zizmor job audits it. The container images it builds and
installs in are pinned by digest, each digest named once in `packaging/entrance/Dockerfile`,
whose `FROM` lines Dependabot moves; `just check` fails while a workflow or a script names
another digest, so the build image's digest in `build.yml` and `scheduled.yml` is moved in the
same pull request. The rustup-init release the build installs is moved by hand, in both files
at once (`just check` fails otherwise). No job restores a cache: what a
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
- **Linux (x86_64 and aarch64, glibc 2.34 or newer) and, from 0.12.0 on, macOS on Apple
  silicon; Windows is the rest of milestone M12.** The aarch64 Linux binaries are
  cross-compiled and only smoke-tested, under QEMU; the test suite runs on x86_64 Linux in CI
  ([docs/policies/msrv-and-platforms.md](docs/policies/msrv-and-platforms.md)). The macOS
  binaries are built and smoke-tested on the maintainers' Mac, where no workflow runs
  `just check` yet, and they are signed ad hoc by the linker, with no Developer ID and no
  notarization, which Gatekeeper asks of a file that carries its quarantine mark, as a
  browser's download does; a download by `curl`, the installer's, carries none. There is no
  Intel Mac, Windows or static (musl) build. 0.12.0 is the first release whose run holds the
  macOS job: 0.11.0 and older hold Linux archives only.
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
- **`so update` itself has not met GitHub.** It is tested against stand-ins on the testing
  host. Before 0.12.0, the first release that holds the command, GitHub was asked by hand what
  the command asks, on 2026-11-18, and
  [docs/known-limitations.md](docs/known-limitations.md#updating-the-program) records the
  answers. Ask again when the command's requests change:

  ```sh
  r=https://github.com/KeiaiLab/SwarmOps/releases
  curl -sI -o /dev/null -w '%{redirect_url} %{http_code}\n' "$r/latest"
  curl -sI -o /dev/null -w '%{http_code}\n' "$r/download/v0.11.0/SHA256SUMS"
  curl -sI -o /dev/null -w '%{http_code}\n' "$r/download/v0.11.0/auto-update-from-0.11"
  ```

  The command needs `…/releases/tag/v0.11.0 302` (or `301`), then `302` for a file the release
  holds and `404` for one it does not, and GitHub answered
  `https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.11.0 302`, `302` and `404`. Not known
  yet: whether a release takes a file of one line as a mark. That shows at the first
  `just release-marks X.Y.Z --upload`, which fails when the release does not list the file
  afterwards.

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
   - the golden vectors in `crates/swarmops-core/tests/golden/` and FORMAT 3's confirmed ones in
     `crates/swarmops-store/tests/golden3/` (not `provisional/`) did not change: `just
     golden-frozen` checks it, and the release workflow runs it on a pushed tag;
   - `FORMAT` was bumped if the store layout changed;
   - every change to `crates/swarmops-cli/tests/snapshots/` (the schema, the MCP tools, the CLI
     output) is intended and in the changelog;
   - the envelope's `api` number went up if a response broke;
   - the crates and licenses that `THIRD-PARTY-LICENSES` adds are ones the archives may carry
     (`scripts/third-party-licenses.py` writes it from `Cargo.lock`).
   - the repository page still matches `.github/repo-settings.json`:
     `python3 -I scripts/repo-settings.py check` reads the host, so it runs here and not in
     `just check`;
   - the units of `docs/roadmap.md`'s tables whose state is `on stable` name the release instead
     (`in X.Y.Z`), each one the release holds;
   - when a page was added since the last release, `web/addresses.txt` lists it: run
     `node scripts/check-site.mjs --write-addresses` in `web/` and commit the result;
   - the macOS sentences name the right releases: 0.11.0 is the last release without a macOS
     archive and 0.12.0 the first with one. The installer's `MACOS_AFTER`
     (`scripts/install.sh` and its copy `web/public/install.sh`), the message
     `scripts/test-install.py` expects, and the sentences beside "0.11.0" and "0.12.0" in
     `docs/guides/install.md`, `docs/guides/parallel-agents.md`,
     `docs/operate/install-and-upgrade.md`, `docs/policies/msrv-and-platforms.md`,
     `docs/troubleshooting.md`, `docs/known-limitations.md`, CHANGELOG.md's macOS entries and
     this file's "Not there yet" hold that. A release whose workflow has no
     `build aarch64-apple-darwin` job holds no macOS archive: say so in them before it is
     tagged.
   - the installer's `UPDATE_AFTER` (`scripts/install.sh` and its copy) names the last release
     without `so update`, 0.11.0, and `scripts/test-install.py`'s `NO_UPDATE` and `FIRST_UPDATE`
     agree: 0.12.0 is the first release that holds the command.
   - what an installed build expects of a release did not change, or the changelog says so
     under a group that withholds the release ([step 4](#checklist)): `so update` in every
     release after 0.11.0 runs the installer that was built into it, not this release's. That
     installer asks for `swarmops-X.Y.Z-<target>.tar.gz` with `swarmops` and `so` in a
     directory of the archive's name, for the archive's line in `SHA256SUMS`, and for its own
     options `--version` and `--dir`; and `so update` asks `releases/latest` for a redirect to
     `releases/tag/vX.Y.Z` and each binary's `--version` for `swarmops X.Y.Z`. A release that
     moves any of these is one those builds fail to install, each once
     ([known limitations](docs/known-limitations.md#updating-the-program)).

     The pages on updating say "a release after 0.11.0" and "from the first release after
     0.11.0 on" for the same reason, or name 0.12.0, the first such release: README.md's
     install section ("From 0.12.0 on"), `docs/guides/install.md`
     ("How do I update?"), `docs/operate/install-and-upgrade.md` (the ways to install, and
     "Automatic update"), `docs/guides/parallel-agents.md`, `docs/guides/claude-code.md`,
     `docs/policies/versioning-and-stability.md` ("What automatic update crosses"),
     `docs/troubleshooting.md`, SECURITY.md and `docs/known-limitations.md` ("Releases up to
     0.11.0 hold no `update`").

     Other sentences say which systems a release is built for, and which it is not, and
     change with the systems the release workflow builds:
     - README.md: the platform line under the status line ("macOS on Apple silicon from
       0.12.0 on", "no Windows or Intel Mac build"), "When not to use it" and the roadmap
       paragraph ("the macOS build ships from 0.12.0 on");
     - `docs/guides/install.md`: "On a Mac" ("0.12.0 is the first release that holds that
       build"), with the sample run of an older release that follows it;
     - `docs/roadmap.md`: the M12 rows ("from v0.12.0 on") and its paragraph;
     - `docs/known-limitations.md`: the Platforms entry on the macOS build;
     - `web/content/product/index.md` ("Not on Windows or an Intel Mac"),
       `web/components/home/hero.tsx` (`MACOS_SINCE`, the install facts' "no Windows build",
       and `PLATFORMS` in `web/lib/site.ts`), `web/components/home/fit.tsx` ("Windows or an
       Intel Mac") and `web/components/markdown-page.tsx` ("Runs on: Linux, …");
     - `web/scripts/check-site.mjs`, rule S8: `TRIPLE` reads the Linux and macOS targets of
       `install.sh`, Home must name each system and architecture they are for, and
       `UNSUPPORTED` requires Home to name Windows and the Intel Mac as without a build.
4. **Write the changelog.** Move the entries under **Unreleased** in
   [CHANGELOG.md](CHANGELOG.md) to a new `## [X.Y.Z] - YYYY-MM-DD` section, mark breaking
   changes, and leave an empty **Unreleased** section. The section's date is the UTC date of the
   release commit, the last commit before the tag. At the bottom, add the new section's link,
   `[X.Y.Z]: https://github.com/KeiaiLab/SwarmOps/compare/vA.B.C...vX.Y.Z` from the previous
   release `vA.B.C` (`[0.1.0]: https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.1.0` for the
   first), and point `[Unreleased]` at `compare/vX.Y.Z...stable`. The new section is the
   release's notes: the workflow refuses a tag without one. It opens with a summary of at most
   60 words, in sentences of at most 25, that names no milestone or slice id. Incompatible
   changes and security fixes each get a group of their own; a release with neither says so in
   one line. A release that an installed build must not take unasked says so in a group's
   heading, with the words `no automatic update` in its parentheses, such as
   `### Changed (breaking, no automatic update)`: `just release-marks` reads the headings
   ([step 9](#checklist)). It takes the words wherever a `###` heading has them, however they
   are spaced, cased and punctuated, and stops at a heading of another depth that says them,
   so a heading written to withhold a release never gives it a mark. Two things call for it,
   and nothing else does: the release changes what stores older builds can open (`init`, or a
   writer by default, starts to make a store that an older release refuses), or it needs a step
   by hand before or after the binaries are replaced. A group that says `(breaking)` alone does
   not withhold a release: its changes are in the notes that the update's line links. The first
   release also brings up to date what says nothing is released yet: README's status badge,
   status line, maturity row and install section, the README's documentation table,
   `docs/operations.md`'s install section, SECURITY.md's "Which versions get fixes?" section,
   CHANGELOG.md's introduction, the versioning policy's first paragraph, this file's first line,
   the Maturity row of `web/content/product/vs-git.md` and the issue template's contact text
   (`.github/ISSUE_TEMPLATE/config.yml`).
5. **Bump the version** in `[workspace.package]` of `Cargo.toml` and in the
   `version = "X.Y.Z"` of every internal dependency in `crates/*/Cargo.toml`,
   `fuzz/Cargo.toml` and `bench/compare/Cargo.toml`; let `cargo check` update `Cargo.lock`.
   In the same change, update every version in README.md's code blocks to `X.Y.Z`
   (`scripts/check-docs.py`, rule `readme-version`, compares them with the workspace version).
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
   wait for its build and packages jobs to pass, the macOS build among them: a packaging failure
   or a missing runner found there costs no tag. The Site workflow's run for this commit fails
   its `readme-home-drift` rule and deploys nothing: the README names `X.Y.Z` and Home the
   newest tag, which is the last release's until step 7. The tag's own Site run passes.
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
   remove it (`sudo apt remove swarmops`, `sudo dnf remove swarmops`). On an Apple silicon Mac,
   run the installer for the version into a scratch directory
   (`curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --version X.Y.Z --dir /tmp/so-check`)
   and both binaries' `--version`.

   Then say which installed builds may take the release unasked. Until this is done, a build
   that updates itself only remembers that it saw the release:

   ```sh
   just release-marks X.Y.Z            # prints each minor line, marked or withheld, and why
   just release-marks X.Y.Z --upload   # uploads the marks with gh, then checks the release
   ```

   A mark is a file of the release named `auto-update-from-<major>.<minor>`: a build of that
   minor line installs the release when the file is there, and announces it otherwise
   ([`update`, In the background](docs/reference/commands/update.md#in-the-background)). The
   first command runs no `gh` and writes nothing. It gives a mark to each earlier minor line
   of the same major version that holds `so update`, and withholds it from a line whose way
   to this release crosses a release with a `no automatic update` group ([step 4](#checklist)).
   Read what it prints before the second command: a line withheld stays where it is until
   someone runs `so update` there, and no later release reaches it unasked either. A release
   whose check above found a defect gets no mark
   ([When a published release is bad](#when-a-published-release-is-bad)).
10. **Afterwards**, check that `stable` carries the release commit and the tag, and that the next
    change starts a new **Unreleased** entry.

## When a published release is bad

Installs that update themselves take a marked release within a day of use, so a bad one is
stopped first and fixed second:

1. **Withdraw the marks**: `just release-marks X.Y.Z --withdraw` removes every
   `auto-update-from-*` file from the release and checks that none is left. From then on no
   build installs the release unasked. A build that already has it keeps it: nothing installs
   an older version, so there is no way back but the installer with `--version`, on each host.
2. **Fix forward.** Land the fix on `stable` and release `X.Y.Z+1` by the checklist above. Its
   marks, uploaded at its step 9, are what moves the installs on, those that took the bad
   release and those that did not.
3. **Never reuse a tag or replace a release's files.** A build that has `X.Y.Z` sees nothing
   newer in a second `X.Y.Z`, and one that is downloading meets a checksum that does not match.

Deleting the release is not needed. Where it is deleted all the same, `releases/latest` names
the release before it, and no build goes back to that one.
