# Contributing to SwarmOps

SwarmOps is pre-release: 0.1.0 is its first release. This page is how changes get made, for
people and agents alike. [The rules](#the-rules) are the short version; everything else here
expands on them.

## The rules

- The design documents are kept privately by the maintainers;
  [docs/architecture.md](docs/architecture.md) is the public overview.
- `unsafe` is forbidden workspace-wide.
- `just check` is the definition of done for every change.
- Tests never need the network (tests that read real repositories on this host run only when
  `SWARMOPS_HOST_REPOS=1`).
- The durability default is full fsync (the `--durability fast` flag exists only if
  `docs/perf.md` records the measurement that justifies it).
- The canonical byte layouts and the hash recipe are the hash contract and are never changed in
  place (a new layout gets a new kind/version header).
- Store-facing API types never carry filesystem paths.

## Before you change anything

- **The design documents are the authority.** The umbrella spec and its three sub-specs decide
  how SwarmOps behaves. They are in Korean, and the maintainers keep them privately;
  [docs/architecture.md](docs/architecture.md) is the public overview, in English. A change that
  changes behaviour updates the spec paragraph that owns it in the same change. If the code and
  a spec disagree, one of them is a bug: say which, and fix it.
- **The hash contract never changes in place.** The canonical byte layouts and the hash recipe
  in the storage-core spec (§1) define every object id. A new layout gets a new kind/version
  header; the golden vectors in `crates/swarmops-core/tests/golden/` are regenerated only on
  purpose (`SWARMOPS_REGEN_GOLDEN=1`), and only together with such a header. A change to the
  on-disk layout of `FORMAT 1` (journal frames, records, index and ref files) needs a `FORMAT`
  bump. [docs/policies/versioning-and-stability.md](docs/policies/versioning-and-stability.md)
  lists every public surface and how each one changes.
- **Work from a plan.** Each milestone has a plan, kept privately with the design documents,
  with its tasks and acceptance tests, and cites the spec sections it implements. Small fixes
  need no plan.
- **Check what is already known.** [docs/known-limitations.md](docs/known-limitations.md) lists
  what M1, M1.5, M1.6, M2, M3, M4 and M8 leave open and the milestone that is meant to close
  each item.

## Setup

Linux today (x86_64 and aarch64 release builds); macOS and Windows are the next milestone, M12
([docs/policies/msrv-and-platforms.md](docs/policies/msrv-and-platforms.md)).
Install Rust with rustup; `rust-toolchain.toml` selects Rust 1.99.0 with rustfmt and clippy.
Then the tools the gates use:

```sh
cargo install --locked just cargo-nextest cargo-deny
cargo install --locked cargo-insta     # to review snapshot changes
cargo install --locked cargo-fuzz      # only for fuzzing, which also needs a nightly toolchain
```

`just check` lints the fuzz crate too, and on a cold cache that compiles libFuzzer's C++
sources, so it needs a C++ compiler (`c++`). Its last step runs a Python script, with Python 3.11
or newer as `python3`.

## Branches and landing

- `stable` is the trunk and the default branch. Work happens on branches; `stable` receives
  only merges and fast-forwards.
- Work on a `feat/<topic>` branch cut from `stable`, and land it with a merge or a fast-forward
  onto `stable`, either through a pull request to `stable` or locally, then push.
- A milestone with several tasks gets an integration branch (for example
  `feat/m1-5-hardening`); each task works on its own branch (`feat/m1-5-t4`), usually in a git
  worktree under `.worktrees/` (git-ignored), and merges into the integration branch, which lands
  on `stable` after a final review.
- Merge commits say what they bring and from where: `merge: <what> (<branch>)`. Delete a branch
  once it has landed (`git branch -d`).

## The definition of done: `just check`

A change is done when `just check` passes on it. It runs, in order:

1. `cargo fmt --check`, for the workspace and for the fuzz crate;
2. `cargo clippy -D warnings`, for the workspace (every target, every feature), for the
   `test_hooks` target without the feature, with the library crates it builds (the all-features
   run compiles that test to nothing), and for the fuzz crate;
3. the test suite with `cargo nextest`, every feature on (`--all-features` turns on the test
   hooks the crash tests drive);
4. the `test_hooks` test in a release build without the feature, which proves that such a build
   ignores the test hooks (`--no-tests=fail`: it fails if the test does not run);
5. `cargo doc` with `RUSTDOCFLAGS="-D warnings"` and `--document-private-items`, which documents
   private items too, so that the CLI's internal docs (its modules are private to its library)
   stay link-checked, run twice: as it stands, and with `--all-features`, which also renders and
   link-checks the code behind `test-hooks` (the order checker and the fault doubles);
6. `cargo deny check`, advisories, licenses, bans and sources, with `deny.toml`, over both
   lockfiles the steps above build: the workspace's and the fuzz crate's, whose one license
   exception is in `fuzz/deny.exceptions.toml`;
7. `scripts/third-party-licenses.py --check`: `THIRD-PARTY-LICENSES`, the notices that the
   release archives carry for the third-party crates compiled into `swarmops` and `so`, still
   matches `Cargo.lock`, the licenses `deny.toml` allows and the crates' own license files. After
   a dependency change, run `python3 -I scripts/third-party-licenses.py` and commit the file it
   writes with `Cargo.lock`.
8. `scripts/test-install.py`: the one-line installer, `scripts/install.sh`, run under sh (and
   dash and busybox sh where installed) against a fake release on 127.0.0.1, with no network;
   and `web/public/install.sh`, the copy get.swarmops.cc serves, still the same file. After changing the
   installer, copy it to `web/public/install.sh`.
9. `scripts/check-docs.py`, its `selftest` first: the public Markdown's relative links and
   anchors resolve as GitHub reads them, and it carries no host path or tool attribution.

The third lockfile, `bench/compare/Cargo.lock`, which `just check` does not build, is checked
where it is built, by `just bench-compare` and the weekly scheduled run, with a policy of its
own: `bench/compare/deny.toml`, every line of `deny.toml` and the harness's own entries. The
`supply_chain` tests of `swarmops-core` fail a gate step, recipe, script or workflow job that
builds a workspace whose lockfile no `cargo deny` checks.

Every cargo build, test, clippy and doc step uses `--locked`, so `Cargo.lock` (committed, like
`fuzz/Cargo.lock`) is exactly what is built. `scripts/check.sh` runs the same steps on a machine
without `just`. Lints are strict: `unsafe` is forbidden, every exported item must be documented
(`missing_docs`), clippy's `all` group is denied and `pedantic` warns (so fails under
`-D warnings`), apart from five noisy lints the workspace `Cargo.toml` allows. `missing_docs`
reaches test builds too: a `pub` item under `#[cfg(test)]` that the crate root reaches needs its
`///` comment, and a new test, bench or binary crate root its `//!` one.
Code follows the Google Rust style guide as [docs/develop/style.md](docs/develop/style.md) applies
it: the second clippy step denies `unwrap()`, message-less asserts and `mod.rs` files in library
and binary code, and the page lists what review reads for and the recorded deviations.

## Tests

- **Tests never need the network.** Tests that read real repositories on this host run only when
  `SWARMOPS_HOST_REPOS=1`. Two tests check the design documents against the code (the storage
  spec's write-ordering rules, the operations spec's conflict kinds): maintainers run them with
  `SWARMOPS_PRIVATE_DOCS` set to the directory that holds the documents' `specs/` and `plans/`,
  and without it they skip and say so.
- **What git writes is a committed fixture.** The git import's tests read repositories that real
  git made, without running git. Each is a POSIX sh script in
  `crates/swarmops-testkit/git-fixtures/scripts/` (`lib.sh` holds the helpers), run in a pinned
  environment, and what it made is committed beside it as `<name>.tar`, with a `manifest.json`
  that git's plumbing wrote of the repository: the oracle the tests compare against. A test
  extracts one with `swarmops_testkit::git::fixture`. `just git-fixtures` rebuilds every archive
  on purpose, byte for byte the same when nothing changed, with the git that
  `git-fixtures/GIT_VERSION` declares (2.43.0; `SWARMOPS_GIT` names the binary, and any other
  version is refused unless `SWARMOPS_GIT_FIXTURES_ANY_GIT=1`): review the diff and commit it
  with the scripts. A new fixture is one more script and the archive that command writes. A
  script whose first lines say `# needs git X.Y` is made by that release or a later one only: an
  older git skips it and says so. While the declared git is older, such a script has no archive,
  and only the version matrix's newest git makes it (`reftable`).
  The three-way merge's corpus, `crates/swarmops-merge/tests/fixtures/merge_file/`, is committed
  the same way, from `git merge-file`: its `generate.sh` rewrites `expected.json` only by hand or
  with `SWARMOPS_REGEN_FIXTURES=1`, and runs the git 2.43 that `SWARMOPS_GIT` names (else `git`
  on `PATH`).
  The remote suites (M13) are the exception: the remote suites run git ≥ 2.39 in `just check`, as
  the product does, against the test kit's loopback `git http-backend` and its stand-in for ssh,
  never the network; git's `http-backend` must be installed with it (`git --exec-path`).
- **Test-only code stays out of release builds.** Test doubles live in test code or behind the
  `test-hooks` cargo feature (the store's `CrashAtFs`, `FailAtFs` and `OrderCheckFs`, which no
  build without the feature can name), and a release build honours no test hook. The hooks are
  six environment variables named `SWARMOPS_TEST_*` (`SWARMOPS_TEST_CRASH_AT`,
  `SWARMOPS_TEST_FS_CALLS`, `SWARMOPS_TEST_FAIL_AT`, `SWARMOPS_TEST_ORDER_CHECK`,
  `SWARMOPS_TEST_NOW`, and `SWARMOPS_TEST_GITHUB_HOSTS`, which makes a push count other hosts as
  GitHub's for its limits), read only under
  `#[cfg(feature = "test-hooks")]`. A test that needs them is marked
  `#[cfg(feature = "test-hooks")]`, or, for a whole file, its `mod` line in `tests/all.rs`
  is (the CLI's `crash` and `faults` modules), so that a plain `cargo test` skips it; the
  gates (`just check`, `scripts/check.sh`, `just check-slow`) build the tests with
  `--all-features`. Step 4 of `just check` keeps this honest. The test kit's own `test-hooks`
  feature (`swarmops-testkit`'s, which turns on the store's) is not a default, and must not
  become one: the crates take the test kit as a dev-dependency, and by feature unification a
  default would build the hooks into the `swarmops` of that release proof. Its default
  features, `cli`, `git` and `repo`, hold the helpers that need `serde_json`, `tar` and
  `swarmops-repo`; the fuzz crate takes it without them.
- **Crash and fault tests are deterministic.** A crash sweep aborts the process at its k-th
  filesystem call (`SWARMOPS_TEST_CRASH_AT=k`) for every k a dry run counted
  (`SWARMOPS_TEST_FS_CALLS`), and an I/O-error sweep fails that call instead
  (`SWARMOPS_TEST_FAIL_AT=k`). Both run the stores at parallelism 1 and pin the signing time
  (`SWARMOPS_TEST_NOW`), so that call k is the same call in every run.
  `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` takes the mode `once` (the default) or
  `persistent` (every mutating call from k on fails too, reads aside) and the errno `eio` (the
  default: a call that flushes runs, then fails, as a failed writeback does) or `enospc` (the
  call does nothing); an I/O error during recovery is two runs, a crash at k1, then a failure
  at the k2-th call of the run that recovers. `HookedFs::from_env` stacks the doubles from the
  inside out, `FailAtFs`, `CrashAtFs`, then `OrderCheckFs`, outermost so that it judges the
  injected failures as real ones; its rustdoc says which stores open on what it gives.
- **Counters that measure a spec cost model are diagnostics, not test doubles.** `SortedView`'s
  `probes` and `rec_reads` count reads in every build, one relaxed atomic add per `read_at`:
  they check storage spec §3's lookup cost model
  (`read_at_binary_search_touches_only_key_column`), since the metadata store's reads do not go
  through `FsOps`. `Index::widest_read` and `SortedView::widest_read` (M6) record the widest read,
  which the bounded-read tests hold to 4,096 entries.
- **Writes are checked against the ordering rules.** Every run of the binary in the CLI tests,
  and the store's and the repository's crash sweeps (their recovery after the crash included),
  check each filesystem call against the
  store's write-ordering rules (`SWARMOPS_TEST_ORDER_CHECK=1`; rules W1 to W8, stated in
  storage spec §2 rule 4 and checked by `crates/swarmops-store/src/order_check.rs`), and panic
  at the first call that breaks one.
- **Child processes have deadlines.** A test that runs processes uses the runner of
  `swarmops-testkit` (the dev-dependency that holds the support the tests share: filesystem
  doubles, power-loss images, and builders of stores, repositories and runs of the binary),
  which gives each child a deadline and kills and reaps it on failure, so a hang fails the test
  instead of stalling the gate. The CLI's golden tests, which run one short command at a time
  through `assert_cmd`, set its `timeout` to the runner's deadline.
- **One integration-test binary per crate.** Where a crate has several test files, they are the
  modules of its `tests/all.rs`, their tests named `<file>::<test>`; Cargo builds no other test
  target from `tests/` there (`autotests = false`), so a new test file needs its `mod` line, and
  `every_test_file_is_a_module_of_all_rs`, in each `tests/all.rs`, fails until it has one. The
  CLI's release proof, `test_hooks.rs` (step 4 of `just check`), stays a binary of its own.
- **Slow tests are ignored by default** with a reason that names their cost
  (`#[ignore = "slow, ~20 s: ~90 crashed commits"]`), and run in the slow suite below.
- **Snapshots are contracts.** The insta snapshots in `crates/swarmops-cli/tests/snapshots/`
  pin the CLI's output; `all__cli__schema_snapshot.snap` pins the JSON Schema of every request and
  response type, and the MCP server's tool list and its discovery result (which carries the
  instructions it gives the model) have snapshots of their own. A snapshot changes only with an
  intended contract change: review the diff (`cargo insta review`), and say what changed
  in the commit and in `CHANGELOG.md`. Likewise, tests that pin a sequence of filesystem calls
  change only where behaviour changes, and the change names each one. Pin an exact
  filesystem-call sequence only where the spec states a cost or an order (the per-transaction
  syncs, the rule that queueing writes nothing, the readers' call sequences in
  `crates/swarmops-store/tests/journal.rs`); elsewhere assert the call classes and orderings,
  and let `OrderCheckFs` check the write-ordering rules.

## The slow suite, measurements and fuzzing

| Command | What it does |
| --- | --- |
| `just check-slow` | `PROPTEST_CASES=2000` and every ignored test except `regenerate_golden` (which rewrites the golden vectors) and the git fixtures' two switches (`regenerate_git_fixtures`, `check_git_fixtures`): the crash and I/O-error sweeps, the power-loss images at every cut (sampled in the two long stretches of its bulk transaction that repeat what the run checks at every step), the two 32-process contention measurements, long journal and index runs. It took 723 s at the end of M1, most of it the two contention runs, which run one after the other. |
| `just check-ci-slow` | The regular suite at `PROPTEST_CASES=2000` plus every ignored test except `regenerate_golden`, the git fixtures' two switches and the two 32-process contention measurements, which are far too heavy for CI runners: what the weekly CI run does, under the `ci-slow` nextest profile. |
| `just perf` | The measurements `docs/perf.md` records, optimized: the store-level and the CLI-level 32 × 1,000 contention tests, then every store bench. It prints their summary lines; `target/perf/` keeps the full output. |
| `just bench` | The criterion benches. |
| `just fuzz [target]` | One cargo-fuzz target for 60 s on the nightly toolchain: `codec_roundtrip` (the default), `tree_decode`, `frame_decode`, `journal_recovery`, `object_file`, `frame2_decode`, `journal2_recovery` or `ref_file2`. It reads the tracked corpus as a seed and writes the new inputs it finds into `fuzz/corpus-new/<target>/`, which git ignores, so a run leaves the tree clean. |
| `just git-fixtures` | Rebuilds the git fixtures' archives and manifests (`crates/swarmops-testkit/git-fixtures/`) from their scripts with the declared git, 2.43.0: the ignored test `regenerate_git_fixtures`, with `SWARMOPS_REGEN_GIT_FIXTURES=1`. A few seconds; run it after changing a script, and commit what it rewrites. |

Run `just check-slow` before landing a change to the store's write, recovery or locking paths.
Run `just perf` only in a quiet window (nothing else heavy on the disk), and record what it
prints in `docs/perf.md`, with the host's load, when a decision rests on it: a
`--durability fast` flag, for one, may exist only if `docs/perf.md` records a measurement that
justifies it. The fuzz crate (`fuzz/`) is a cargo workspace of its own with a tracked corpus
(`fuzz/corpus/<target>/`), which `just fuzz` only reads: libFuzzer writes the new inputs it
finds into the first corpus directory it is given only, and the recipe gives it
`fuzz/corpus-new/<target>/` (git-ignored) first and the tracked corpus after it, as a seed. New
inputs join the tracked corpus minimised. Copy the tracked corpus and the new inputs into one
scratch directory and run `cargo +nightly fuzz cmin <target> <that directory>` from `fuzz/`
(it replaces the directory with the smallest set of its inputs that keeps their coverage, and
it moves the directory into a temporary one under `fuzz/`, so the two must share a filesystem);
then commit only the inputs it kept that are not tracked yet, removing no tracked input, with
each target's count and size before and after in the commit body. Crashes land in
`fuzz/artifacts/` (git-ignored), and a crash found there belongs in a regression test.

## Continuous integration

GitHub Actions runs `just check` on every push to `stable` and to `feat/**` branches and on every
pull request to `stable`. Every workflow (CI, the weekly run, the site and the release) runs on
self-hosted runners on the maintainers' host, labelled `swarmops-local`; a pull request from a
fork runs on GitHub's `ubuntu-24.04` instead, so that the host never runs a stranger's code. The one check to look at is `tests-pass`: it passes only when every job
it depends on passed. A weekly scheduled workflow runs `just check-ci-slow`, a 60-second fuzz
smoke of every target, the `cargo deny`, format, lint and test commands of `bench/compare` (a
cargo workspace of its own that `just check` does not build) and the git version matrix.
Dependabot proposes grouped, weekly updates for Cargo and GitHub Actions, after a cooldown;
review them like any other change.

- **Hung tests are killed and named.** The `ci` nextest profile ends a test after 10 minutes and
  the weekly `ci-slow` profile after 20; either way the test appears as a failure in the JUnit
  report the run uploads, and a hanging fuzz input stops after 30 seconds with its reproducer
  uploaded.
- **Tool versions are bumped by hand.** `just`, `cargo-nextest` and `cargo-deny` are pinned in
  the installer step of both workflows, `cargo-fuzz` in `scheduled.yml`'s `cargo install` step,
  each to a release at least 7 days old. Dependabot cannot bump them. The installer step
  (`fallback: none`) takes only releases in the manifest of the pinned `install-action`, so move
  its pins in a `ci:` commit after Dependabot has bumped the action; CI's tools may therefore be
  older than the local ones (nextest 0.9.146 against 0.9.148 at M1.5).
- **The git fixtures meet three gits a week.** The `git-matrix` job builds git 2.39.5, 2.43.0
  (the declared one) and 2.56.0 from kernel.org's release tarballs, each checked against the
  SHA-256 pinned beside it in `scheduled.yml` and cached by release. With each it rebuilds every
  fixture and names each difference from the archives' manifests (`check_git_fixtures`), then
  runs the fixture tests and the git import's on what that git made
  (`SWARMOPS_GIT_FIXTURES_DIR`). A script a git is too old for (`# needs git X.Y`) is skipped
  there and said to be. Last, that git checks the git export's repositories
  (`exported_repositories_pass_git_fsck`, ignored in the default gate because it runs git; run it
  with `--run-ignored ignored-only`): the fixtures git made, imported, landed on and exported into
  an empty repository, pass its `git fsck --strict`, and a checkout of the branch gives the
  store's files, modes and symlinks; for the crafted fixtures, git finds in the export what it
  finds in the fixture. It refuses a git other than the declared one unless
  `SWARMOPS_GIT_FIXTURES_ANY_GIT=1`, which the matrix sets. The newest release is moved by hand,
  in a `ci:` commit, with the SHA-256 that kernel.org's signed `sha256sums.asc` gives: Dependabot
  does not know it.
- **A new RustSec advisory turns every push red** through `cargo deny check`, on purpose. Update
  the affected crate; if no fixed release exists and the advisory cannot affect SwarmOps, add an
  `ignore` entry with the advisory id and the reason to `[advisories]` in `deny.toml`, in a
  commit of its own. An advisory for a crate that only `bench/compare` uses turns the weekly run
  and `just bench-compare` red instead, and its `ignore` entry goes to `bench/compare/deny.toml`.
- **A semver-incompatible release of a crate both lockfiles share** changes a requirement in
  `Cargo.toml`, so its Dependabot pull request fails until
  `cargo update --manifest-path fuzz/Cargo.toml -p <crate>` is pushed to it.

A ruleset on `stable` makes `tests-pass` the one required check and blocks force pushes and
deleting the branch. A push to `stable` is refused unless `tests-pass` is already green for the
commit it moves `stable` to: push a local merge to its `feat/**` branch first, and push `stable`
once CI has passed there. The rule stays: land on `stable` only what passed `just check` locally
and has `tests-pass` green for the same commit.

## Commits

- Subjects say what the change does: `<type>(<scope>): <what>`, with types such as `feat`,
  `fix`, `perf`, `test`, `refactor`, `docs`, `ci` and `chore`, and scopes such as `core`,
  `store`, `repo`, `api`, `ops`, `cli` or `spec`. For example:
  `fix(store): sync journal.log before recovery lists a frame`.
- One logical change per commit; every commit passes `just check`.
- Commit messages name what and why; no tool or generator attribution.
- A user-visible change (commands, flags, the envelope, error codes, the schema, the store
  format, durability) adds a line under **Unreleased** in [CHANGELOG.md](CHANGELOG.md).

## Reporting problems

Bugs and feature requests go to GitHub issues, through the templates. A security vulnerability
does not: [SECURITY.md](SECURITY.md) says how to report one privately.

## License

SwarmOps is released under the [MIT License](LICENSE). Contributions are accepted under the same
license.
