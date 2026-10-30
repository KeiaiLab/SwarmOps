# Changelog

Notable changes to SwarmOps. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and versions follow
[the versioning policy](docs/policies/versioning-and-stability.md) (Semantic Versioning, read the
pre-1.0 way).

SwarmOps 0.1.0 is the first release, tagged `v0.1.0`, 0.2.0 the second, with the git remotes (M13),
0.3.0 the third, with the workspaces (M5), 0.4.0 the fourth, with verify, crash safety, fuzzing, gc
and FORMAT 2 (M6), 0.5.0 the fifth, with the pilot on a real repository and the goal measurements,
0.6.0 the sixth, with the fixes those measurements asked for, and 0.7.0 the seventh, with stock git
against a store (M14); no crate is published (`publish = false`). The 0.1.0 section holds every
change up to it, grouped by milestone of the design (umbrella spec §13). From here, **Unreleased**
collects the changes on `stable` that no release holds yet, until the next release
([RELEASE.md](RELEASE.md)) moves them into a section of its own.

## [Unreleased]

### Changed (breaking)

- `op undo` and the `undo` request take the operation id `op log` prints (`<op>:<crc>`); a bare
  number is `E_INVALID_ARG`. What to do: read `id` from `op log` or from the response that
  recorded the operation.

- `ref set` and `op undo` refuse to move `main` off its first-parent history (`E_REF_PROTECTED`).
  What to do: land a change that reverts it. The error's details name the head the move would
  drop (`changeset`) and its `target`, and an undo's its `op` and `id`; nothing is written, and
  `release/<name>` refs move either way (the M15 design §4,
  [Troubleshooting](docs/troubleshooting.md#e_ref_protected)). An undo that a later move of `main`
  blocks (`E_CAS_MISMATCH`) says to land a change that reverses the operation, no longer to undo
  the later operations first; the `op undo` and `ref set` help, the MCP `undo` tool and the MCP
  server instructions state the rule.

- `ws commit` in a sparse workspace no longer widens its prefixes; an edit outside them is
  `E_WS_OUTSIDE_PREFIXES`. What to do: `ws materialize <path>` first. A path named outside the
  prefixes, and any file or directory the walk meets outside them that no `.gitignore` or
  `.swarmignore` on disk names, is refused, nothing changed; `details.paths` lists them, sorted,
  the first 20. `ws land` refuses the same before anything else; `ws rebase -m` commits the edits
  under the prefixes as before (the M15 design §5,
  [Troubleshooting](docs/troubleshooting.md#e_ws_outside_prefixes)).

### Added

- Every `op log` entry and every `undo` result that wrote an operation carry its `id`: the number
  and its journal frame's CRC-32 as 8 lowercase hex digits. An id read before a failed write took
  its operation back is `E_OP_NOT_FOUND` with `details.reason: checksum`, never the operation that
  took the number next (the M15 design §2); a number past the last is `reason: past_end`; either
  way `details.id` is the id as the request sent it. A warning after a write's commit point names
  the operation by its id ([Troubleshooting](docs/troubleshooting.md#e_op_not_found)).

- `.deb` and `.rpm` packages for x86_64 and aarch64 on every release, with their lines in
  `SHA256SUMS`: `swarmops_<version>_<arch>.deb` and `swarmops-<version>-1.<arch>.rpm`, made by
  `scripts/packages.py` from the release archive's binaries, byte for byte, into `/usr/bin`, with
  the license, the third-party notices, README and changelog in `/usr/share/doc/swarmops`, and in
  the `.deb` both licenses as a machine-readable Debian `copyright` file there, which slim images
  keep; reproducible, and depending on glibc 2.34 or newer and libgcc. The release workflow
  installs, runs, verifies and removes each one with `dpkg` in Debian 12 and `rpm` in AlmaLinux 9
  (the aarch64 ones for aarch64, under QEMU) before it publishes anything, and a run started by
  hand rehearses a branch without publishing
  ([Install from a .deb or .rpm package](docs/operate/install-and-upgrade.md#install-from-a-deb-or-rpm-package)).
- `--version` names the build's commit when `SWARMOPS_BUILD_COMMIT` is set at compile time:
  `swarmops <version>+<commit>`, the commit as semver build metadata, so a binary built from a clone
  can be compared with the commit it came from. The value must be 7 to 40 lowercase hex digits;
  any other, empty included, fails the build. Release builds leave it unset and still print
  `swarmops <version>`
  ([Install from a clone](docs/operate/install-and-upgrade.md#install-from-a-clone)).
- Two error codes in the schema, exit 1, not retryable: `E_WS_OUTSIDE_PREFIXES` (a sparse
  workspace's edits outside its prefixes, raised by `ws commit` and `ws land`) and `E_WS_STALE` (a
  workspace's base gone from its target's history, not raised yet), the workspace refusals of the
  M15 design §5 ([Troubleshooting](docs/troubleshooting.md#e_ws_outside_prefixes)).
- Golden envelopes pin 53 of the 55 error codes, seven more than before: `E_AMBIGUOUS_ID`,
  `E_IO_TRANSIENT`, `E_INTERNAL`, `E_BUSY`, `E_CAS_RETRY_EXHAUSTED`, `E_UNKNOWN_PARENT` as an
  error object, and `E_WS_OUTSIDE_PREFIXES`.

### Changed

- `just check` and `scripts/check.sh` also build the CLI with `SWARMOPS_BUILD_COMMIT` set, which
  no other step does (`scripts/check-stamped.py`): the `version` tests and the two `--version`
  pins pass with a fixed commit, each required to run, and a malformed commit must fail the build
  with the compile-time guard's message; `target/debug`'s `swarmops` and `so` are linked again
  without the fixed commit after. It adds about 5 s to a warm run.
- The [versioning policy](docs/policies/versioning-and-stability.md) counts the `--version` line
  in the command-line contract, with `SWARMOPS_BUILD_COMMIT`'s name and the values it takes, and
  names `--help` and `--version` among the runs that print other than one envelope: they print
  plain text. Changing the line's form, renaming an environment variable of the contract or
  narrowing the values `SWARMOPS_BUILD_COMMIT` takes is a breaking change.
- swarmops.cc reads like the readable-site design board: its colours in both themes, 17 px body
  text in a 60-character measure, serif titles and sections, underlined links, and code blocks
  labelled by language, each command joined with the output after it, long output folded behind
  "Show N more lines"; still no web font, no other origin and no new script.
- The weekly run's `just check-ci-perf` job also runs the other five measurement cases on 12,000
  or 13,000 commits or 12,000 changesets (`remote_perf`'s fetch, push and sync cases and the
  workspace's checked land), which took 65-76 % of `just check-ci-slow`'s 20-minute kill;
  `check-ci-slow` leaves them out, and swarmops-core's `supply_chain` tests fail a case it leaves
  out that no scheduled job runs optimized. That job and the benchmark gate wait at most 60
  minutes for the heavy lock, apart from their run time, and fail with a message when the wait
  runs out; their timeouts add the wait.
- swarmops.cc's home page follows the readable-site design board: a status line, the install
  command and an "At a glance" rail, then six numbered sections (see it, why a swarm needs it,
  what it does today, where it stands, fit, read on) behind an "On this page" list. Product pages
  carry one title, a tab strip, the same rail with their own "On this page" list, and tables that
  scroll sideways on a phone. The version and date shown are the top released section of this
  changelog, read when the site is built.

- A sparse workspace holds the `.gitignore` and `.swarmignore` files of the directories above
  its prefixes: `ws create --prefix` writes them out and `ws materialize` adds them with each
  path. `ws materialize` takes a path the tree lacks when it is on disk and its parent is the
  root, a directory of the tree or one above a prefix, writing only the ignore files above it
  that the workspace lacks, so the next commit stores it; a path absent from disk or without such
  a parent is still `E_PATH_NOT_FOUND` (the M15 design §5a).
- The `meta.resolved` of a sparse workspace's `ws` envelopes also names its materialized
  changeset, which the commands read to compare the tree with the disk (the M15 design §5a).

### Fixed

- A land key that landed a change onto one target is refused for a land onto another: `land --key
  K` onto `main` after `K` landed onto `release/1` was a new land, so a retry that named the
  wrong target landed the change twice. The land looks for the key on `main`, `release/<x>` and
  branch refs other than its target too, down to the change's base like the target's own walk, and
  refuses it with `E_INVALID_ARG`; `details.target` is new and names the ref whose history holds
  the landed changeset (the M15 design §6). Give each change and each target a key of its own. A
  key that landed below the change's base on another ref is not found, as on the target.
- `ws status`'s `behind_by` counts the changesets the target holds that the workspace's base lacks
  when the base is not on the target's history: 1 after a sync replaced the base with its
  squash-merged copy, and 0 when the target moved back onto the base's first-parent history,
  fewer than 10,000 changesets behind the base, where it counted the target's whole history up to
  10,000. To find where the two histories meet, it then reads the base's history too, so one status
  reads at most 200 `log` pages instead of 100, the value still at most 10,000.
  `WsStatus.behind_by`'s schema description now says it is 0 when the target head is on `base`'s
  first-parent history fewer than 10,000 changesets behind `base`.
- The build-and-test guide says that only the workspace operations other than `ws create` and
  `ws hook worktree-create` act as the workspace's creator without `--as` or `SWARMOPS_AGENT`,
  `ws create` always needs an identity and the hook acts as `claude-code/<name>`; it said every
  command run inside a workspace did, but `commit`, `land`, `sync` and the other commands that need
  an identity still refuse an anonymous caller there with `E_IDENTITY_REQUIRED`, while `gc`,
  `repair`, `upgrade`, `fetch` and `push` need none anywhere.
- `fetch`, and so `clone` and `sync`, import the followed branch before it moves the tracking
  refs and remote tags, and the import reads the branch's incoming ref
  (`refs/incoming/<remote>/heads/<branch>`), each object against its id. A mirror object that
  holds another object's bytes, or any refusal of the import (`E_GIT_READ`, `E_GIT_IMPORT_LOSSY`,
  `E_GIT_UNSUPPORTED`, `E_TOO_LARGE`), now leaves every tracking ref and remote tag as it was; they
  moved first before, so a later `push` leased on a value the import had refused. An import error's
  `details.git_ref` is now the incoming ref, not the tracking ref. A crash between the import and
  the move is finished by the next fetch ([architecture](docs/architecture.md), the M15 design §3).
- A pack garbled in transit, which git refuses with "pack is corrupted (SHA1 mismatch)" or "final
  sha1 did not match", is classified by rows of its own: `E_REMOTE_REJECTED` with
  `details.reason: "bad_objects"`, not retryable, no ref moved.
- A tree that holds a path with a component named `.swarmws` at any depth, in any ASCII case
  (`.SWARMWS/x`, `src/.swarmws/y`, `src/.SwarmWs/z`), is refused with `E_WS_UNPORTABLE`
  (`reason: "reserved_name"`) on every platform when a workspace is created on it, moved onto it
  (`ws rebase`) or widened onto it (`ws materialize`), and nothing is written. Only a top-level
  lower-case `.swarmws` was refused before, so `src/.swarmws/y` was written. `src/swarmws/y` and
  `src/.swarmws.bak/y` are still written. To keep such a tree out of a workspace, rename the path
  in the store with a commit, or create the workspace with `--prefix` paths that leave it out.
- A workspace made with neither `--prefix` nor `--all` is stored as the whole tree it is written
  out as (`all: true` in its state and in `ws create`, `ws info` and `ws list --check`), and one
  an older build stored with `all: false` and no prefix is read as the whole tree. In 0.7.0 such a
  workspace was otherwise handled as holding nothing: a fresh one listed every file as added and
  `ws land` refused them (`E_WS_DIRTY`), as it did the files a commit or `ws rebase -m` did not
  touch; a deleted file was left out of the commit; `ws discard <path>` was refused; and a named
  commit or `ws materialize <path>` narrowed its prefixes to that path. Each now acts as in a
  workspace of the whole tree (the M15 design §5). A workspace 0.7.0 already narrowed that way
  stays sparse: `ws materialize` the paths it needs, or create it again (`ws delete`, then
  `ws create`) once its work has landed.
- A commit the store refuses outright (such as `E_EMPTY_COMMIT` or `E_INVALID_ARG`) no longer
  leaves its pending record behind; the next `ws land` refused with "a ws commit … was cut short:
  run it again first", which could not succeed. A record an older build left that way goes with
  the next `ws commit` that finds nothing to commit.
- The scripting guide names `E_REF_NOT_FOUND` and `E_WS_STALE` as the codes not raised yet, as
  Troubleshooting does; it named `E_AUTH`, `E_POLICY_DENIED` and `E_MAINTENANCE_BUSY` too, which
  `swarmops serve` and the maintenance commands raise.
- In a sparse workspace, a `.worktreeinclude` copy or build output that only an ignore file above
  the prefixes names, at the root or in a directory above a prefix, no longer blocks `ws commit`,
  `ws land`, `ws delete --rescue` and the `worktree-remove` hook with `E_WS_OUTSIDE_PREFIXES`. One
  in another directory off the prefixes is still refused as that directory (`config` for
  `config/master.key`): `ws materialize config` clears it, and the ignore file keeps the copy out of
  the commit. A path under the prefixes that only such a file names is no longer committed; a new
  file at the root, which the refusal's hint names, now materialises instead of failing with
  `E_PATH_NOT_FOUND` ([Troubleshooting](docs/troubleshooting.md#e_ws_outside_prefixes)).
- In a sparse workspace, `ws commit`, `ws land`, `ws delete --rescue` and the `worktree-remove`
  hook refuse (`E_WS_OUTSIDE_PREFIXES`) an edit to a tree path outside the prefixes that is on
  disk, and a tree ignore file off the prefixes and missing from disk under an ignore file the
  workspace holds. An ignore file on disk hid the first, and the walk read without the second's
  rules, so a commit left such edits out without a word. A root `.swarmws` of the tree, under
  any ASCII case, is no such path: the workspace's own directory stands there (the M15 design
  §5a).

## [0.7.0] - 2026-10-29

Serve stock git. The 0.7.0 release holds M14, topology 1: `swarmops serve`, run per connection
by ssh or a web server, lets stock git clone, fetch, pull and push a store under the store's
`access.toml`, and lands a push to `refs/land/<target>`; `git import`, `git export` and `push`
take every ref name the store's format takes; and the measurements of G16 and of v0.6.0's
misses. It carries the first of v0.8.0's fixes for the open soft findings (issues #2 to #21): a
collision-flagged git object, a malformed memo and a damaged generation index are refused or
reported, `read_file` refuses a `max_bytes` of 0, `op undo`'s refusals report their repairs, and
the schema, the help and the references say what the code does.

### Added

- Two probes for v0.6.0's misses, no fix: `just perf` times a sync of one land on 1,000 and
  12,000 commits by phase (`fetch`, `sync --no-push`, `push`), and G4's landing case with
  `SWARMOPS_TEST_CLI_LANDING_LOCK_SAMPLE_US` set prints each process's wait for `meta/lock` and
  its hold, sampled from `/proc/locks`.
- `swarmops serve` (M14), the server for stock git clients, run per connection by ssh or a web
  server; it takes its caller from `--as` alone (`SWARMOPS_AGENT` does not apply). Each store it
  serves holds an `access.toml` (version 1): `[[grant]]`s naming callers (`who`) with the refs
  they may `read`, `push`, `force` (a backward move or a deletion) and `land` onto. Without the
  file every caller is refused (`E_AUTH`), and an invalid one refuses every request
  (`E_POLICY_DENIED`, with its line).
- `swarmops serve` serves reads over ssh: as an `authorized_keys` forced command it takes
  `git-upload-pack '<store>.git'`, checks the caller's `read` in the store's `access.toml`,
  exports every served ref of the committed snapshot into `<store>/git/serve.git`, and runs the
  host git's `upload-pack` there, so stock git clones, fetches, pulls and shallow-clones with
  protocol v2 or v0. Partial clone filters are not advertised (git fetches every blob), and no
  text a client gets names a host path.
- `swarmops serve` takes pushes over ssh: for `git-receive-pack '<store>.git'` it runs the host
  git's `receive-pack` on `serve.git`, which hands every command to `serve --hook proc-receive`.
  The hook imports the pushed commits from `serve.git` (a gitlink is refused,
  `E_GIT_IMPORT_LOSSY`), checks the caller's `push` grant, and moves the store's `main` or
  `release/<x>` (or, in FORMAT 2, a branch) when the client's old value is the ref's export
  (`E_CAS_MISMATCH` otherwise) and the move is forward (`main` along first parents), recorded
  for the caller `--as` names. `atomic` and push options are not advertised. The hook refuses
  (`not_from_git`) unless the parent `serve` named the store and the caller and `GIT_DIR` is that
  store's `serve.git`.
- `swarmops serve`'s pushes take the rest of the push rules: a lightweight tag is created as
  `tags/<name>` and never moved (`E_TAG_IMMUTABLE`), an annotated tag is refused
  (`annotated_tag`); a branch or tag is deleted with the `force` grant (`E_POLICY_DENIED`
  without it) and `main` never (`E_REF_PROTECTED`); a branch other than `main` moves backwards
  with the `force` grant. Each command of a push is decided on its own.
- `swarmops serve` lands pushes to `refs/land/<target>`: `git push origin HEAD:refs/land/main`
  imports the pushed commits, checks the caller's `land` grant, and lands them onto `main` as one
  squashed changeset keyed by who pushed which commit where (`gp1-` and 60 hex digits), so the
  same push again replays and writes nothing. The client reads the landed commit on a `remote:`
  line (a fetch brings it in); a conflict is refused with the report on `remote:` lines (at most
  50 paths), as are `E_NOTHING_TO_LAND` and a tag as the target. No ref is left under
  `refs/land/`.
- `swarmops serve --cgi` serves git's smart HTTP behind a web server: `PATH_INFO`
  `/<store>.git/info/refs`, `/git-upload-pack` or `/git-receive-pack` runs the host git's
  `http-backend` on the store's `serve.git`, after the same read check and refresh as over ssh,
  so clones, fetches, pulls and pushes (the push rules and `refs/land/<target>` included) work
  over HTTP with protocol v2 or v0. The caller is `--as`, or `--user-family <f>` with the web
  server's `REMOTE_USER`; a push needs no web authentication, since the access file decides.
  Refusals before git are a status and a `<CODE>: <message>` body: 403 for `E_AUTH` and
  `E_POLICY_DENIED`, 404 for a missing store or any other path (`not_smart_http`), 400 for a
  path out of the root (`bad_path`).
- `just perf` times `swarmops serve` (G16) over ssh on 12,000 commits: a clone and a no-op fetch
  against git's own `upload-pack` on the same history (medians of 5 and their ratio), and one
  push to `refs/land/main`, printed as `serve-perf:` lines.
- Operating guides [Serve over ssh](docs/operate/serve-over-ssh.md) and
  [Serve over HTTP](docs/operate/serve-over-http.md): the `authorized_keys` line,
  `AcceptEnv GIT_PROTOCOL`, the serving account's memo key directory, `access.toml` and its
  refusals, CGI entries for nginx with fcgiwrap and for Apache, what a client types and the
  refusals it sees.
- `docs/perf.md` "v0.7.0" records G16 and the v0.6.0 misses measured again: `swarmops serve` on 12,000 commits clones
  0.97× and fetches with nothing new 1.82× git's own server, a push to `refs/land/main` takes
  0.104 s, and its first clone 1,195.6 s; G17's fetch 1.34× (1.65× in v0.6.0); G4 at 50
  processes on the ext4 bench disk, p99 2.5–2.7 s for v0.6.0's code as for M14's, the lock held
  99.6 % of the run; the sync of one land 49.6 s.

### Changed

- `git import --into`, `git export --ref` and `push` take every ref name the store's format takes
  (in FORMAT 2 a branch such as `feature/x`, or a tag `tags/v1`, which an import creates and never
  moves), where they took `main` and `release/<x>` alone. A remote-tracking ref is
  `E_REF_NAME_INVALID` `reserved`, and a FORMAT 2 name in a FORMAT 1 store `E_FORMAT_REQUIRED`.
  The git side writes lightweight tags too (`refs/tags/<name>`), which a later export moves only
  when forced (`tag_exists`).
- The git remote's mirror keeps a commit-graph: every git child gets
  `fetch.writeCommitGraph=true`, so git updates it after each fetch (for G17's fetch ratio;
  v0.6.0 measured 1.04× with one, 1.65× without).
- `E_AUTH` and `E_POLICY_DENIED` are raised, by `swarmops serve`, and `schema ErrorCode` says
  so: `E_AUTH` when the serving entry names no caller or no grant of the store's `access.toml`
  names it, `E_POLICY_DENIED` when the file grants that caller no such action. Both stay exit 5,
  not retryable.
- The hints of `E_EMPTY_COMMIT` and `E_CONFLICT` name the MCP argument beside the flag
  (`allow_empty`, `resolutions`), and the MCP `commit` tool's description says when
  `allow_empty` is needed.
- The `E_REF_NAME_INVALID` hint and the help of `commit --advance`, `land --target`, `rebase
  --onto` and `ref set NAME` name the FORMAT 2 refs (a branch such as `feature/x`, a tag), where
  they said `main` or `release/<name>` alone.
- The error reference documents each `E_TOO_LARGE` details shape, `git import`'s `E_IO` after a
  commit point (`committed: true`, no `fs_op`), a source behind the target as
  `E_GIT_IMPORT_DIVERGED`, and 255 *distinct* parents for `E_GIT_UNSUPPORTED`; the concepts page
  says a `nonstandard` commit's unreadable date is kept as 0 seconds at `+0000`.
- `just check-ci-slow` leaves out the four measurement cases on 12,000 commits
  (`remote_perf::clone_cost_on_12000_commits` and `serve_perf`'s three), which ran into its
  20-minute kill (three take longer even optimized) and failed the weekly run; the new
  `just check-ci-perf` runs them optimized, one at a time, in the weekly run's last job, under
  the heavy lock and the `ci-perf` nextest profile (55-minute kill). `just check-slow` keeps them.
- `read_file` (`cat`) refuses a `max_bytes` of 0 with `E_INVALID_ARG` (`details.max_bytes`), as
  `diff` does, and its schema says the minimum is 1: it returned nothing, truncated, with a
  `next_offset` where it started, so a read on while truncated never ended (issue #4 item 2).
- The MCP tool descriptions of `read_file`, `read_tree` and `diff`, and the request types, say to
  page at the full ids the first page resolved, since a cursor or `next_offset` pins no revision,
  and that `next_offset` reads on to the file's end, past a `lines` range (issue #4 items 3 and
  13).
- The schema, the help and the land reference say what the code does: a conflict report's files
  are bounded apart from `max_conflict_bytes` (24 KiB of entries, the first file's later hunks
  spanned by one without texts, later files left out whole), `auto_merged` lists the paths merged
  into a content neither side had, every `add_add` carries hunks, `type_change` also covers a file
  the change deleted where the target made a directory, and `hunks_truncated` also marks, in
  `stat` too, a text past the line diff's bound, which gets no line counts (issue #10 item 4,
  issue #17 item 3). `git import` reuses a commit the store holds as it is, even one an earlier
  import took with a loss, `paths` also covers empty and `/`-holding names, and the import and
  export docs say how memos are checked or, under the memo key, taken (issue #8 items 3 and 9,
  issue #16).
- The versioning policy lists the external id index's entries (`SXID` version 1) as part of the
  store format, with the rule for a new entry layout, and no longer pins a crate count or 0.1.0;
  `SECURITY.md` drops the unbounded journal and index reads M6 bounded; the `op undo`, `ref list`
  and concepts pages describe `op log`'s committed numbers and FORMAT 2's refs (issue #6 item 12,
  issue #3 item 2, issue #16 item 8, issue #20 items 10 and 21, issue #21 item C1).
- The git round-trip guide and the import's documentation say which stopped imports count what
  they recorded: a store failure and a failure after a frame's commit point do
  (`details.imported`, `details.last_commit`), and a refusal (`E_GIT_IMPORT_LOSSY`,
  `E_GIT_UNSUPPORTED`, `E_GIT_READ`, `E_TOO_LARGE`, `E_INTERNAL`) keeps the frames before it
  without counting them, as an `E_CAS_MISMATCH` after the last frame records every commit without
  a count; the guide promised the count for every import cut short (issue #7 item 2, issue #8
  item 2).

### Fixed

- `op undo` refused with `E_OP_NOT_FOUND` or `E_NOTHING_TO_UNDO` reports in `meta.recovered` what
  its write repaired first, as `E_CAS_MISMATCH` already did; the next write no longer reports
  `null` for a repair the refused undo made.
- `verify` on a FORMAT 2 store names its checkpoint file `meta/journal2.ckpt`, and what it holds
  (an offset, a seq and their CRC), where it named FORMAT 1's `meta/journal.ckpt` and a u64le.
- A derived value whose put finds its slot held by a damaged file the store cannot remove is
  kept for the rest of the run, as a failed put's is, so a run derives it once (storage spec §5),
  not once for each value that needs it.
- `derive --after N` with N past the journal's last frame is `E_INVALID_ARG` (`details.after`),
  with `--follow` too and whatever `--limit`: it read as caught up before (`{derived: 0,
  cached: 0}`, exit 0), so a follower given such a frame stayed silent for good.
- History queries check each index generation they order or bound by against its parents': a
  land's merge base and a `log --path` over several paths fail with `E_STORE_CORRUPT` on a
  damaged or crafted `meta/idx/gen.col` instead of answering by it, and a revision's `~N` no
  longer names nothing by one (issue #16 item 5).
- A fetch that meets a garbled loose object in the mirror fails `E_REMOTE_REJECTED`
  `bad_objects`, no longer `unclassified` or a retried `E_REMOTE_UNREACHABLE`.
- The fetch's closure check is documented as finding missing objects and unparsable commits and
  trees only: it checks no object against its id, so a damaged loose blob of the mirror passes
  it. The fetch moves the mirror's tracking refs before it imports, so they name the damaged
  commit when the fetch's import refuses it (`E_GIT_READ`, `reason: corrupt`); a refusal in the
  first frame imports nothing, and one in a later frame keeps the frames before it (issue #21 item
  D3).
- `--repo` with a relative path works for the remote commands (`fetch`, `push`, `sync`); git
  failed with "not a git repository" (`E_REMOTE_REJECTED` `unclassified`).
- `blob put -` reads stdin itself, so a socket stdin works; it used to open `/dev/stdin`, which
  fails for a socket.
- A commit or a land's resolutions refuse a content past 512 MiB (`E_TOO_LARGE`, scope `put`) by
  the size it declares, before any file is read; the check used to wait until the contents
  before it were read.
- A store path that leads through a name that is not UTF-8 (a symlink's target, the working
  directory) is `E_IO` (`fs_op: "resolve"`) whose message names no host path, where it was
  `E_STORE_CORRUPT` with the path in its message (issue #9 item C1).
- `swarmops --help`, the CLI reference and the versioning policy name `serve` and
  `derive --follow` beside `mcp` as the runs that print other than one envelope on stdout, and
  `schema Meta` lists every operation that reports `meta.recovered`, and on which errors, where it
  named `commit`, `ref_set` and `ref_delete` alone (issue #2 item 2, issue #11 item 2).
- The order checker's W8 now ties an external id entry to its changeset's frame: it reads the
  entry's changeset id and requires a durable frame of `journal.log` (synced, or listed by any
  process) that records it. Before, it asked only that the bytes its own process wrote be
  durable, so an entry naming a changeset no durable frame held passed.
- `git export` refuses a blob, tree or commit whose bytes git's collision detection flags as a
  SHA-1 collision attack (`E_INTERNAL`, naming the object) before it memoizes an oid of it; it
  memoized the zero oid and rendered it into trees and commits. `git import` refuses a commit
  whose lossy rendering git would flag (`E_GIT_READ`, `malformed`) (issue #5 item 12).
- `verify --deep` reports as `derived_mismatch` each git memo that `git export` refuses as
  malformed: a `_mac` memo of another length, or whose `kept` or `imported` byte is neither 0
  nor 1, under any memo key, and a `git_tree_oid` or `git_blob_oid` of another length than an
  oid's. It passed over them, so a store could fail `git export` yet verify clean, and `repair`,
  which drops them, went unasked (issue #21 item D1).

## [0.6.0] - 2026-10-28

Fix what the measurements missed. The 0.6.0 release holds the landing model the v0.5.0
measurement chose (a moved `main` settled inside the lock by metalog merge), export memos a
per-store key authenticates, a fetch that checks every object it receives, a protected `main`,
the benchmark gate, and the measurements of all of it.

### Added

- `just bench`, the benchmark gate: it clears old criterion results, runs every bench and fails
  on a median over 20 % above its baseline in `bench/thresholds.toml` (113 medians from two quiet
  runs), on a listed bench without an estimate, or on an estimate neither listed nor skipped for
  a reason (`scripts/bench-gate.py`; `just check` runs its selftest). The weekly Scheduled run
  gates on it, last and alone, under the host's lock for heavy runs.
- The v0.6.0 measurements, in [docs/perf.md](docs/perf.md) ("v0.6.0"), with v0.5.0's methods:
  G1 at 50 processes and G2 met as before; G4 at 50 processes 25 lands/s, 0 % retries and nothing
  lost (met), p99 579.5–594.6 ms (missed, 7.3–7.9 s before); G17's push 1.04× (met, 3.72×
  before), its fetch 1.65× (missed; 1.04× with a commit-graph); `ws commit` 0.6 ms per changed
  file (8 ms before); a sync of one land on 12,000 commits 52.9 s (missed, not profiled);
  compaction at 1M records 173 ms and a snapshot with 256 refs 2.75 ms p99.
  [docs/goals.md](docs/goals.md) cites each.

### Changed

- `land` settles a moved target inside the lock (metalog merge, the model the M4 measurement
  chose): when `main` moved after the land read it, the writer walks first parents back to the
  head the land read (at most 256) and makes the land anew on the new head, its delta where no
  path overlaps and overlapping regular files of at most 1 MiB merged three-way, instead of
  handing it back for another attempt. Anything else goes back to the lander as before. A land
  with `expect_target` (`ws land --check`) never relocates, so it lands the tree it checked.
- `ref delete main`, and an `op undo` that would delete `main`, fail with the new
  `E_REF_PROTECTED` (exit 1, not retryable, `details.name`) in every format, with nothing written;
  `release/<x>`, tags, branches and remote-tracking refs stay deletable (the v0.6.0 design's
  decision 8).
- A fetch checks what it receives: git's fsck runs on every received object
  (`fetch.fsckObjects`, `transfer.fsckObjects`), the fetch writes the mirror's
  `refs/incoming/<remote>/` only, and the tracking refs move to them by compare-and-swap after a
  closure check finds every object they reach. A refused or missing object fails the fetch with
  `E_REMOTE_REJECTED` `details.reason: "bad_objects"`, with nothing imported and no tracking ref
  moved, and every later fetch fails the same way until the remote's history changes; there is no
  override (the v0.6.0 design's decision 9).
- `git export`, `git import`, `push`, `fetch` and `sync` trust the export oid memos that a
  per-store memo key authenticates, and stop their walk of the history there: an export of one
  commit over 2,000 imported ones reads 9 of the store's objects, not 2,015. The key is 32 random
  bytes in `$XDG_STATE_HOME/swarmops/memo-keys/<repo_id>.key` (mode 0600, made on first use;
  `SWARMOPS_MEMO_KEY_DIR` moves it), outside the store, so a memo placed in the store without it
  is refused (`E_STORE_CORRUPT`), never taken. No state directory, no key: every export oid is
  derived as before. `verify --deep` checks the new `git_oid_mac`, `git_tree_oid_mac` and
  `git_blob_oid_mac` memos too (`derived_mismatch`, `key` the key's id), and `repair` drops a
  malformed one.

## [0.5.0] - 2026-10-27

Prove and measure. The 0.5.0 release holds the pilot on a real repository, the goal
measurements, the tests of this host's repositories, the roadmap to usable and the first v0.6
fixes the measurements and the pilot asked for.

### Added

- The pilot ran on a real repository, this host's `traders` (993 commits, Python), with the 0.4.0
  binary: import, one agent session over MCP, export, the repository's own gate on the export, a
  person's commit and the second import, eight sessions landing at once with one killed, the
  second export, backup and restore, `repair` of an induced corrupt object, and the rollback to
  git. Every criterion of the runbook passed; nothing was lost or rewritten
  ([docs/pilot.md](docs/pilot.md#the-traders-pilot-a-real-repository)). A soak of eight sessions
  every 30 minutes runs on that store towards the three days that "usable" asks.
- The measurements the roadmap to usable asks for, in [docs/perf.md](docs/perf.md) ("v0.5.0"):
  G1 at 50 processes (met), G2 without syncs (met), G4 landing under load at 32 and 50 processes
  (lands/s and nothing lost met; p99 and retries missed), G9's workspace latencies (status,
  create and the check overhead met; the update after a land and `ws commit` missed) and G17's
  fetch and push ratios (missed). [docs/goals.md](docs/goals.md) cites each.
- The harness for them: `SWARMOPS_TEST_CLI_CONTENTION_CHILDREN` sets the contention test's
  process count, `landing::perf::landing_under_load` offers one land per agent every 2 s for
  120 s (`SWARMOPS_TEST_CLI_LANDING_LANDERS`) and reports its chain beside slow lands,
  `ws_processes::perf::perf_open_and_land_percentiles` gives `ws create` and `ws land`
  percentiles, and `swarmops-repo`'s `commit_latency` bench times a commit with and without its
  syncs. `just perf` runs the contention and landing cases at 32 and 50 processes
  (`SWARMOPS_PERF_PROCESSES`), then the bench.
- Tests of this host's real repositories, behind `SWARMOPS_HOST_REPOS=1`: each bare mirror in
  `SWARMOPS_HOST_REPOS_DIR` is imported, verified deep, exported, checked by `git fsck --strict`
  and imported again to the same head. The weekly run's `host repositories` job refreshes the
  mirrors with `scripts/host-repos.py` and runs them.
- The ignored perf case `remote_perf::fetch_phases_scale_with_history` splits a fetch's time, on
  a 1,000- and a 12,000-commit history, into the binary's fetch, `git fetch` alone, `git gc --auto`
  alone and the same after `git commit-graph write --reachable`, for G17's missed fetch ratio
  (`just perf`).

### Changed

- `ws commit` sends the blobs the store lacks inline in its one keyed commit request, which the
  store writes as one batch, instead of one durable `blob put` per file; a blob past what is left
  of the 64 MiB inline budget is still put alone first. G9's commit of 100 files took 805-819 ms
  before. `blobs_written` counts the puts' writes plus the commit's.
- `op log` reads the committed operations only: it waits for the store's lock (`E_BUSY` after
  the lock timeout, retryable), runs the journal tail check and lists the frames synced and
  listed in `journal.off`, so a listed number keeps its operation and `op undo` of it never undoes
  another (issue #11).
- The landing model is measured: four prototypes, five cells, three runs each, in
  [docs/perf.md](docs/perf.md) ("M4 — the landing model, measured"); metalog merge is the candidate v0.6.0 builds
  (the M4 plan's decision 10).
- Documents brought to the code: the README's topology-2 "Planned" cell no longer lists signed
  commits; `ws:<name>` is described as `ws diff`'s label, not a revision; a cache or relay tier and
  a virtual checkout are [docs/goals.md](docs/goals.md) non-goals; content-defined chunking,
  intra-line merge, a web view and agent transcripts are unscheduled known limitations.

- `docs/roadmap.md`: a "To usable" section defines when SwarmOps is usable and plans v0.5.0 to
  v0.8.0 to get there (the pilot and measurements, their fixes, M14, operations); M7, M9's
  simulator, M11 and M12 wait until then. The README's roadmap summarises it.
- `scheduled.yml`: the fuzz smoke, `bench/compare`, the git matrix and the live GitHub run wait
  for the slow suite, so that it has the host to itself, and the git 2.56.0 build no longer
  inherits the job's `CARGO_TARGET_DIR`.

### Fixed

- MCP `ws_create` without a `path` makes the workspace at `<working directory>/<name>`, as its
  schema says, instead of failing with `E_INVALID_ARG`.

## [0.4.0] - 2026-10-27

### Added

#### M6: verify, crash safety, fuzzing, gc and FORMAT 2 (2026-10-27)

- Store `FORMAT 2` (`swarmops-store 2`): ref names beyond `main` and `release/<name>`, namely
  free-form branches (`feature/login`), tags (`tags/<path>`) and remote-tracking names
  (`remote/<remote>/<path>`, kept for M11's fetch and refused to every command); each ref file is
  named by a hash of the ref's name and holds it. A tag is created and deleted, never moved
  (`E_TAG_IMMUTABLE`). `ref list` reports each ref's `kind` and takes `--prefix`.
- `FORMAT 2`'s journal (`journal2.*`): each frame header carries its seq, the previous frame's CRC
  and its own CRC, and each `journal.off` entry and the checkpoint carry checksums, so a
  misplaced, stale or lost frame, or a crafted or zeroed entry anywhere in the chain, is reported
  as damage; recovery checks the entries from the checkpoint on, not all of them.
- `swarmops upgrade` (MCP `upgrade`) moves a FORMAT 1 store to FORMAT 2 in place, under the lock,
  and finishes an interrupted upgrade on the next open; on a FORMAT 2 store it does nothing
  (`upgraded: false`). It cannot be undone: older releases open no FORMAT 2 store, and a FORMAT 1
  handle opened before the upgrade, of this release or an older one, fails its next read or write
  with `E_STORE_CORRUPT` (its hint says to verify; opening the store again reads FORMAT 2).
- `swarmops gc [--dry-run]` (MCP `gc`) removes a store's leftovers: temp files older than a day, a
  crashed `init`'s staging, derived values of older versions, and what an upgrade left. It
  deletes no object and no changeset, and counts the objects no changeset reaches
  (`orphan_objects`) without removing them.
- `swarmops repair [--dry-run]` (MCP `repair`) rebuilds `journal.off`, the changeset index and
  its sorted views, and the ref files from the journal, moves damaged objects to
  `quarantine/objects/<id>` so that `blob put` or `git import` can store them again, drops
  damaged derived values and external id entries, and verifies again (`after`). It never cuts the
  journal: damage past a torn tail is refused as `E_STORE_CORRUPT` with `details.offset`.
- `gc`, `repair` and `upgrade` take a maintenance lease: while one runs, another fails at once
  with the retryable `E_MAINTENANCE_BUSY`, raised for the first time.
- `swarmops init --format <1|2>` (MCP `repo_init`'s `format`); `info` reports the store's
  `store_version`.
- `verify --max-findings <N>` (default and maximum 1,000): the report lists at most that many
  problems and says `truncated` and `found` (every problem found).
- Error codes `E_FORMAT_REQUIRED` (a ref name a FORMAT 1 store cannot hold: `details{name,
  needs, store}`, hint `swarmops upgrade`) and `E_TAG_IMMUTABLE`, both exit 1, not retryable.
- Four libFuzzer targets: `object_file` (loose object files), `frame2_decode`,
  `journal2_recovery` and `ref_file2` (FORMAT 2's frames, journal recovery and ref files).
- Tests: a literal `SIGKILL` kills `commit --advance` at seeded random points on both formats on
  a real disk, then checks `verify --deep`, that every acknowledged commit is in `log`, and that
  the next commit succeeds (10 kills per format in `just check`, 500 in the slow suite); the
  crash, I/O-error and power-loss sweeps run once per format; golden tests pin the
  `E_FORMAT_REQUIRED`, `E_TAG_IMMUTABLE` and `E_MAINTENANCE_BUSY` envelopes and run the FORMAT 2
  acceptance (a branch and a tag in one store, the FORMAT 1 refusal and `upgrade`, a land onto a
  branch but not a tag) through the binary.

### Changed

#### M6: verify, crash safety, fuzzing, gc and FORMAT 2 (2026-10-27)

- `swarmops init` makes a FORMAT 2 store unless `--format 1` asks for one that older releases
  open. FORMAT 1 stores open, read and write as before.
- A store on a filesystem off the Linux allow-list (ext2/3/4, XFS, btrfs, tmpfs, ZFS, F2FS,
  overlayfs) is refused with `E_UNSUPPORTED_FS`, its `statfs` magic in `details.fs_type`: NFS, SMB,
  FUSE and 9p mounts among them. `init` checks the nearest existing directory, every other command
  the store's root. The `/mnt/` rule stays.
- A remote-tracking name (`remote/<remote>/<path>`) is refused to `ref set`, `ref delete`, `land`,
  `commit --advance` and `rebase --onto` (`E_REF_NAME_INVALID`, `details.reason: "reserved"`), and
  a tag as the target of `land` or `commit --advance` (`details.reason: "tag"`).
- Bounded reads: the journal is read a frame at a time, header first, and `journal.off`, the
  index columns and the sorted views in windows of 4,096 entries, so no store file is read into a
  buffer of its length; a lookup by id prefix or delta hash keeps at most 4,096 changesets.
  `cat` reads a blob in windows, and `diff` (stat and unified) and `land`'s merge leave unread a
  blob past the line diff's 8 MiB bound, with the same output as before. A NUL-holding blob past
  8 MiB opposite a text one now shows in `diff` as cut, not binary.
- `verify`'s metadata checks run in the store, reading the index and the sorted views in blocks
  of 4,096 entries.

#### Since 0.3.0

- The code follows the Google Rust style guide, applied as `docs/develop/style.md` records: no
  `unwrap()` or message-less assert in library code (`expect` with the reason), module files named
  after their item with no `mod.rs`, free functions called through their module, enums in place of
  `bool` parameters, lower-case hex literals; `just check` denies the first three. Behaviour,
  on-disk format and the public API paths are unchanged.
- Every public enum and struct is `#[non_exhaustive]` (`just check` denies exhaustive ones): other
  crates build them with `new` or `Default`, projection functions that could not fail now
  return `Result`, and `RepoError::Internal` and `StoreError::Internal` are new. The CLI and MCP
  wire contract is unchanged. An error variant this build does not map is reported as
  `E_INTERNAL` with a hint; the compile-time guarantee that every variant is mapped is gone.
  `swarmops_git::tree_bytes` returns `Option<Vec<u8>>` (`None` for a leaf kind without a git
  mode), and a `-0000` identity with a nonzero offset is refused at conversion
  (`ConvertError::Malformed`) instead of later at commit.

### Fixed

- `init` over a store whose `init` crashed in the other format no longer fails with
  `E_STORE_CORRUPT` until the directory is removed: it sets the half-made metadata aside and makes
  the store in the format asked for.
- `ws status` could miss a same-size edit made in the filesystem tick a status had confirmed the
  file in: once a later status rewrote the index for another file, the racy row was trusted. A
  row a scan does not confirm keeps no stat at or after its index's time (it is hashed again).

### Security

- gix 0.89.0 -> 0.90.0 (GHSA-q54r-jf2p-rfr2: an untrusted `.git/config` could turn off the `.git`
  path checks of checkout, index, merge and tree editing). SwarmOps runs none of those through
  gix, so it was not exposed.
- zstd 0.14.2 -> 0.14.3, zstd-safe 8.1.0 -> 8.2.0, zstd-sys 2.1.1 -> 2.1.2 (RUSTSEC-2026-0391,
  -0398, -0403, -0410). The store uses none of the affected paths (reused stream contexts, the
  seekable format, legacy frames, wasm32), so it was not exposed.

## [0.3.0] - 2026-10-22

### Added

#### M5: workspaces (2026-10-22)

- `so ws` commands (`create`, `status`, `diff`, `commit`, `rebase`, `resolve`, `land`,
  `materialize`, `discard`, `list`, `info`, `delete`, `hook`) and eleven MCP workspace tools; a
  command run inside a workspace finds its store. With `rebase`, twelve new MCP tools: 35 in all.
- Workspaces materialise a sparse tree on disk, write only what changed on update, never overwrite
  an edit or write through a symlink, and refuse trees the volume cannot hold (paths that differ
  only in case on a case-insensitive volume, names Windows refuses), naming the paths.
- `so ws status`, `ws diff` and `ws commit`: changes found by stat and an index, hashed only when
  needed; commits upload only the blobs the store lacks and replay after a crash.
- `so ws land --check <command>` rebases the workspace onto the target, runs the command there,
  and lands only if the target has not moved since, so the landed tree is the tree the command
  checked; a moved target makes it rebase and check again, up to `--max-attempts`.
- `rebase` merges a change onto a ref's head and records the result as one changeset there,
  moving no ref (`so rebase`, MCP `rebase`).
- `land --expect-target <id>` publishes only while the target names that changeset, in one
  attempt; otherwise `E_CAS_MISMATCH`, nothing written. With a rebased head, the landed tree is
  the tested tree.
- `so blob get` reads blobs by id, in batches up to a byte budget; it is no MCP tool.
- Claude Code: `so ws hook worktree-create` and `worktree-remove`, registered as the
  `WorktreeCreate` and `WorktreeRemove` hooks, give `--worktree` sessions and `isolation: worktree`
  subagents a SwarmOps workspace; uncommitted edits are kept as a rescue changeset when the
  worktree is removed. `.worktreeinclude` files are copied, and the repository's bootstrap command
  (`.swarmops/workspaces.json`) runs.
- `SWARMOPS_WS_ROOT` places the workspaces the Claude Code hooks make; `SWARMOPS_WS_BOOTSTRAP=skip`
  skips the copy of `.worktreeinclude` files and the repository's bootstrap command.
- Nine error codes: `E_WS_EXISTS`, `E_WS_NOT_FOUND`, `E_WS_LOCKED` (exit 4, retryable),
  `E_WS_DIRTY`, `E_PATH_NOT_EMPTY`, `E_UNRESOLVED_CONFLICTS`, `E_MARKERS_PRESENT`,
  `E_WS_UNPORTABLE` and `E_CHECK_FAILED`, each pinned by a golden envelope: 50 codes in all.
- Crash sweeps of a workspace's update, commit, rebase, land and checked land, and an
  eight-process checked-land test in which one lander is killed and run again: every checked land
  is on `main` once, with the tree its check recorded. The pilot's workspace step is a test of the
  binary: a hook-made workspace in a directory that is no git repository lands through MCP.
- `docs/goals.md`, G9: the workspace latency targets, none measured yet: `ws status` of 50,000
  files under 100 ms warm (umbrella spec §15) and, proposed with M5, under 150 ms after 100 edits;
  `ws create` of 10,000 files under 3 s; an update after a 10-file land under 100 ms; `ws commit`
  at G2's target plus hashing; `ws land --check` under 1 s on 12,000 changesets without the check.
- Docs: [Build and test a change](docs/operate/build-and-test.md) leads with workspaces and keeps
  the git-worktree recipe, now landing the tested tree with `rebase` and `land --expect-target`;
  the Claude Code guide's hooks section; the workspace codes in the troubleshooting page; a
  "Workspaces" section in the known limitations.

### Fixed

- Two `init`s on one directory at once: one creates the store, the other fails with `E_EXISTS`.
- A workspace update killed after it wrote an executable file and before it set the file's mode is
  finished by the next open; it was refused as an edit of that file.

## [0.2.0] - 2026-10-22

### Added

#### M13: git remotes, topology 2 (2026-10-22)

- `so clone`, `so remote add|remove|list` and `so fetch`: a store fetches a git remote over HTTPS
  or ssh into a git mirror it owns (`git/mirror.git`, a cache), and imports the followed branch's
  commits; `main` does not move. `clone` makes the store, records the remote, fetches it and
  imports the followed branch into `main`, and continues when run again after a failure.
  Remotes live in `<store>/remotes.toml`, which holds no credential.
- `so push <remote> [<ref>[:<branch>]]`: a compare-and-swap against what the last fetch saw, never
  forced; a moved remote is `E_REMOTE_DIVERGED`, a refusal `E_REMOTE_REJECTED`. Files over
  GitHub's 100 MiB limit and pushes over its 2 GB limit are refused before anything is sent, with
  a warning per file over 50 MiB. Run again with nothing new, it starts no git.
- `so sync <remote>`: fetch, land the store's unpublished changes onto the remote's head in order,
  move `main` there and push by compare-and-swap; changes the remote already holds (merged by a
  merge commit, a squash or a rebase) are recognised by their `SwarmOps-Changeset`,
  `SwarmOps-Squashed-From` and `SwarmOps-Land-Key` lines (indented or bulleted, as a squash
  description lists them) or by merging as no change, and dropped. `--no-push` for branches that
  take pull requests only. `so sync status` counts what is ahead and behind, offline.
- `remote_list`, `fetch`, `push`, `sync` and `sync_status` are MCP tools (23 in all); `clone` and
  `remote add|remove` stay in the CLI.
- The git transport runs your own git (2.39 or later, `SWARMOPS_GIT` or `PATH`): its credential
  helpers, proxies and ssh settings apply as they do for `git fetch`. `--token-env <VAR>` names a
  variable whose token reaches git as a header for that remote alone, redacted from every error;
  git never prompts.
- `<store>/authors.toml` gives an agent family the email (and name) its new commits are signed
  with, so that GitHub links them to an account; existing changesets export as before.
- Four error codes: `E_REMOTE_DIVERGED` (exit 3), `E_REMOTE_REJECTED` (exit 5),
  `E_REMOTE_UNREACHABLE` (exit 4, retryable) and `E_REMOTE_NOT_FOUND` (exit 6), pinned by golden
  envelopes over smart HTTP and ssh.
- The git import can take a ref's commits as changesets without moving any ref (`objects_only`,
  used by `fetch`), and the repository can land a change onto a head with no ref moved
  (`Repo::land_detached`, used by `sync`).
- `git import` and `git export` refuse a path inside a store's `git/` directory
  (`E_INVALID_ARG`, `details.reason` `store_mirror`): the mirror is SwarmOps' alone.
- The test kit serves git repositories without the network: git's own `http-backend` on loopback
  and a stand-in for ssh; the weekly git matrix runs the remote suites with git 2.39.5, 2.43.0
  and 2.56.0, built with curl, and a weekly job runs the live round trip with GitHub against a
  scratch repository once a repository variable names it; the live run reads its token in the
  `github-live` environment.
- `just perf` times the git remotes against the roadmap's targets (docs/perf.md, "Git remotes
  (M13)"): fetch met its target; push, sync and clone missed theirs.
- A guide, [GitHub as a remote](docs/guides/github-remote.md); the daily loop now runs on
  `so sync`, `so push` and a pull request merged by any method, and the bare clone stays for the
  round trip without a remote.
- The GitHub round trip was piloted on a private repository: two agents over MCP, pull requests
  merged by squash, rebase and merge commit, `so sync` after each, no rewritten commit
  ([docs/pilot.md](docs/pilot.md#the-m13-pilot-a-store-and-github)).

#### Since 0.1.0

- CI, the weekly run, the website build and the release now run on the maintainers' own host through
  self-hosted runners; release binaries are built inside an ubuntu:24.04 container pinned by digest
  (pull requests from forks still run on GitHub's ubuntu-24.04).
- A one-line installer, `curl -fsSL https://get.swarmops.cc/install.sh | bash` (`scripts/install.sh`,
  served as `web/public/install.sh`): it installs `swarmops` and `so` from a release, checked against
  its `SHA256SUMS`, into `~/.local/bin` (`/usr/local/bin` as root); `--version` pins a release,
  `--dir` picks the directory, a second run updates, and `--uninstall` removes the binaries and
  no store. `just check` runs its offline tests, `scripts/test-install.py`.
- `docs/goals.md`: the project's goals, each with its metric, its target and where that target
  is written, the value `docs/perf.md` has measured (or that none exists yet) and the milestone
  that delivers it, with non-goals and engineering principles.
- `docs/policies/clean-room.md`: how third-party designs reach the code, only as functional
  requirements in the project's own words, with a provenance record per feature and text, code
  and licence checks before each release.
- A Markdown check in `just check` (`scripts/check-docs.py`): relative links and anchors resolve
  as GitHub reads them, and no public page carries a host path or tool attribution.
- Usage documentation, every example run against a real store: an install guide that leads with
  the installer (pinning, fleets, updates, uninstalling, hand checks, source builds, the
  installer's errors); a quick start from install to `verify`; Concepts; guides to commit and
  land, to history and undo, to parallel agents, to Claude Code and to the git round trip; a
  troubleshooting page from symptom to command for every `E_*` code; `MAINTAINERS.md` and
  `SUPPORT.md`.
- `docs/reference/cli.md` and `docs/reference/errors.md`: every command's usage, arguments,
  options and defaults, and every error code's exit, retryable flag and description, generated
  from the code and kept current by a test (`SWARMOPS_REGEN_REFERENCE=1` rewrites them).
- The README is rewritten around the core features, each with an example run, and
  `docs/README.md` maps the documentation by task.
- Two developer guides: MCP integration (`docs/develop/mcp.md`: registration, the stdio exchange,
  protocol versions, identity, every tool with its command, size limits, failure modes) and CLI
  scripting (`docs/develop/cli-scripting.md`: the envelope, exit codes, jq recipes, retry rules,
  idempotency keys, a commit-and-land loop).
- The operations guide is split into task pages under `docs/operate/`; `docs/operations.md` is now
  their index, and its old section links still resolve.
- The new website, `web/`: a Next.js static export with shadcn/ui and Fumadocs that renders
  `docs/**/*.md` and `CHANGELOG.md` at `/docs` (sidebar groups, static search, code copy, a
  per-page Content-Security-Policy) and the product pages (home, `/product`, features, how it
  works, SwarmOps and git, the roadmap); `just web-install`, `just web-build` and `just web-check`,
  and a CI workflow.
- `docs/reference/commands/`: one page per command (25) with its syntax, arguments, a recorded
  example, its result's fields and its errors; `docs/reference/cli.md` is now the grouped index.
  Generated from the code and the golden steps, and checked by a test.
- Golden examples for blob put/has, op log/undo, derive, land, git import/export and every MCP tool,
  taken from real runs.
- Docs: the command pages of init, info, whoami, schema, derive, commit, land, blob put and blob has
  carry a Details section: behaviour, option interactions, patterns, failures and measured
  performance.
- Docs: the read commands' reference pages (resolve, cat, ls, show, log, history, diff) gain a
  hand-written Details section: option interactions, paging and cursors, errors and their causes,
  agent patterns, caveats and measured costs.
- Reference: the ref list, ref set, ref delete, op log, op undo and verify pages gain a Details
  section (behaviour, outcomes per ref state, caveats, patterns).
- The reference pages of git import, git export and mcp have hand-written Details: behaviour,
  options, patterns, failure cases.
- Docs: one reference page per MCP tool (input schema, annotations, a recorded call, its CLI
  command) and an index with protocol versions, server instructions, identity rules and size limits,
  generated from the server's tool table.
- Site: command reference pages show their metadata as a card, arguments as collapsible entries, CLI
  and MCP examples as tabs and error codes as badges linking to the error list; the command index is
  one grouped table with a filter, and command and MCP tool pages link to each other.
- Site: every docs page has a "View as Markdown" link to its source file, served beside the page at
  its URL plus `.md` (text/markdown); llms.txt now links those files.
- Docs: the README keeps its core features, install, quick start, the comparison with git and the
  roadmap; the MCP and command details now live on the MCP integration page and the command pages.
- swarmops.cc is now served from `web/`. The site workflow deploys `web/out` to Cloudflare Pages:
  production from `stable`, previews from `feat/**`. While the Cloudflare secrets are unset, it
  leaves a notice and deploys nothing. `site/` is retired: its icons, figures, notices,
  `security.txt` and the installer copy moved to `web/public/`, and `/guides/<name>` redirects (308)
  to `/docs/guides/<name>`.
- `docs/roadmap.md`: the detailed roadmap. Its goal is version control that works together with
  GitHub, in three topologies: local git with a SwarmOps remote, a store with a GitHub remote, and
  stores syncing with each other. For each it gives what works today, with the real commands, and
  the plan: commands, protocol, ref mapping, identity, offline behaviour, acceptance criteria and
  targets. Two milestones are added, M13 (git remotes from a store) and M14 (git clients against a
  store), and M11 takes `so clone` and `so sync`. `docs/goals.md` gains G15 to G17, and the site's
  roadmap page summarises the new page, topologies first.
- `docs/reference/limits.md` lists every contract limit with its constant, value, the commands that
  enforce it and its error, plus the defaults, the schema bounds and, separately, the internal
  values. `docs/reference/schema-types.md` covers the 91 API types with their fields and values.
  Both pages are generated from the code and have drift tests.

## [0.1.0] - 2026-10-20

### Added

#### M0: bootstrap (2026-10-15)

- A Cargo workspace on Rust 1.99.0 (edition 2024, pinned by `rust-toolchain.toml`), with
  `unsafe` forbidden everywhere, clippy's `all` group denied and `pedantic` warned, and
  `just check` as the definition of done (fmt, clippy `-D warnings`, nextest, rustdoc
  `-D warnings`, cargo-deny); `scripts/check.sh` runs the same steps without `just`.
- `swarmops-core`: object ids that hash a domain-separated header with BLAKE3-256
  (`swarmops:<kind>:v1`), validated UTF-8 repository paths that are never normalized, and the
  canonical byte codec for trees and changesets, which is the hash contract. Four golden hash
  vectors pin it.
- Property tests and cargo-fuzz targets for the codec (`codec_roundtrip`, `tree_decode`).
- The fsync-latency experiment ([docs/perf.md](docs/perf.md)): durability defaults to full
  fsync, and no `--durability fast` flag is introduced.

#### M1: the single-agent core (2026-10-16)

- `swarmops-store`: self-verifying loose object files, zstd-compressed when that makes them
  smaller and checked against their id on every read, written temp file → `sync_all` →
  no-clobber link → directory fsync, in parallel batches.
- The native metadata store, in pure Rust with no external database: an append-only,
  CRC-framed journal (`journal.log`, `journal.off`, `journal.ckpt`) that is the source of truth;
  a columnar changeset index with sorted views and tail compaction; one file per ref. Writers
  are serialized by an `flock` that waits in the kernel's queue and gives up after 10 s
  (`E_BUSY`); readers take no lock. After a crash, the next writer cuts a torn tail and replays
  the journal.
- `swarmops-repo`: commits from change lists, each object durable before any journal frame
  names it; reads of files and directories at any revision without a checkout; revisions
  (`main`, `release/<name>`, ids and their prefixes of 12 or more digits, `~N`); first-parent
  history; ref moves by compare-and-swap; and `verify`, whose `--deep` re-hashes every object.
- `swarmops-api`: the operation contract, as serde and JSON Schema types: the request and
  response of every operation, the JSON envelope, 24 stable `E_*` error codes with fixed exit
  codes and retry advice, `Identity` and the `StoreOps` trait.
- `swarmops-ops`: `LocalOps`, the `StoreOps` implementation over a local store, which resolves
  identities and change-list contents, and reports each failure with its code, machine-readable
  details and a hint, often the next command to run.
- `swarmops-cli`: the `swarmops` binary, with `init`, `info`, `whoami`, `schema`, `commit`,
  `cat`, `ls`, `show`, `log`, `ref list`, `ref set`, `ref delete` and `verify`. Every run prints
  exactly one JSON envelope; default pages are sized for the spec's 64 KiB response target, with
  opaque cursors for the rest.
- Crash safety tested through the real binary: a sweep aborts `init` and `commit` at every one
  of their filesystem calls, as `kill -9` would, and the store must reopen and verify clean.
  Multi-process tests (8 processes committing at once, 16 racing to move one ref) end with
  `verify --deep`. Fuzz targets cover journal frames and journal recovery.
- The `test-hooks` cargo feature: the crash tests' hooks (`SWARMOPS_TEST_CRASH_AT`,
  `SWARMOPS_TEST_FS_CALLS`, `SWARMOPS_TEST_NOW`) work only in builds that turn it on, and
  `just check` proves that a release build ignores them.
- Measurements in [docs/perf.md](docs/perf.md): 32 processes × 1,000 commits through the binary
  without one `E_BUSY` (72.7 commits/s on the reference host), and a durable, uncontended commit
  in 21 ms.
- The MIT License ([LICENSE](LICENSE)), for every crate.

#### M1.5: hardening and opening (2026-10-17)

- CI on GitHub Actions: `ci.yml` runs `just check` (the same gate as locally, `--locked`
  everywhere) and zizmor behind one `tests-pass` check; `scheduled.yml` runs the slow suite and
  a fuzz smoke weekly; Dependabot keeps the root and fuzz lockfiles valid; the nextest `ci`
  and `ci-slow` profiles kill hung tests.
- The project policies and the documentation set: [CONTRIBUTING.md](CONTRIBUTING.md),
  [SECURITY.md](SECURITY.md), [RELEASE.md](RELEASE.md), `docs/architecture.md`,
  `docs/known-limitations.md`, `docs/policies/`, the issue and pull request templates, and this
  changelog.
- `swarmops mcp`: a stdio MCP server over `StoreOps`, one tool per operation with the request
  type's JSON Schema as its `inputSchema`, and the CLI's envelope as the result
  (`structuredContent` plus a text block; `isError` on failure). MCP 2026-07-28 per request,
  and 2025-11-25 or 2025-06-18 after `initialize`. Each call resolves its revisions on its own
  (`meta.resolved` is no longer shared by a handle's requests), a mutation without an identity
  is refused, and `expected_old` of `ref_set` and `ref_delete` must be a full changeset id
  through MCP, and a `{"file": path}` content must be an absolute path that is not the server's
  own stdin or stdout.
- `server/discover` and the `-32022` error list every revision served (2026-07-28, 2025-11-25,
  2025-06-18), so a client that cannot speak 2026-07-28 can fall back to `initialize`.
- `ref set` to the target a ref already holds is a replay: `already_applied: true` and nothing
  written, in the CLI and through MCP, `--create` included; `old` and `new` are then the
  request's.
- The `--expect` help of `ref set` and `ref delete` states the README's rule: give the full
  changeset id you read, since a ref name or a relative revision such as `main~1` is read from
  the store when the command runs, and then the compare-and-swap protects nothing, and an id
  prefix stops resolving once another changeset shares it.
- `so`, the short form of `swarmops`: the same program, built and installed with it; its usage
  lines, help and clap's message for a missing subcommand (`'so ref' requires a subcommand`)
  name the binary as invoked, and everything else still says `swarmops`.
- I/O-error injection and write-ordering checks, behind `test-hooks`:
  `SWARMOPS_TEST_FAIL_AT=<k>[:once|persistent[:eio|enospc]]` fails the k-th filesystem call
  with an injected error (`FailAtFs`), and `SWARMOPS_TEST_ORDER_CHECK=1` checks every write
  against the store's write-ordering rules R1–R7 and panics at the first that breaks one
  (`OrderCheckFs`, on for every run of the binary in the CLI tests and in the store's and the
  repository's crash sweeps). A model of Linux's fsync failures (the bytes a failed sync did not
  cover stay lost, even after a later sync succeeds) tests the journal, and I/O-error sweeps fail
  every call of a checkpointing and of a compacting store transaction, of the recovery after a
  crash, and of the binary's `init` and `commit`, checking that each outcome is honest.
- The fsync-failure policy (storage spec §2 rule 3): a failed `journal.log` append or
  `fdatasync` is cut back off the journal under the lock, and nothing is committed; a failed
  `journal.off` listing after the commit point is cut back off `journal.off`, and the commit
  stands, with a warning; a tail check whose sync fails writes the unlisted frames afresh from
  the bytes it read before it lists them. Three documented double faults abort the process,
  saying why on stderr: a cut of a failed frame that fails too, a cut that fails while unlisted
  frames are written afresh, and an `init` that cannot take back its `FORMAT`. A long-lived
  process, such as `swarmops mcp`, ends with them.
- **Contract change:** `E_IO_TRANSIENT` now means that nothing was committed, with
  `details.committed: false` (it said `details.may_have_committed: true`), so running the same
  command again is safe; and a failed `journal.off` listing after the commit point is a success
  with a warning, as every later failure already was (it was `E_IO_TRANSIENT`). A journal append
  that fails before its frame is whole, which was `E_IO`, is now `E_IO_TRANSIENT` too, as is a
  write whose tail check can neither sync nor write afresh the frames `journal.off` does not
  list yet. In `swarmops-store`, `StoreError::Undecided` and `CrashAtFs::from_env` are removed.
  `StoreError::NotCommitted` (a journal append that failed and was cut back off the journal;
  retryable) is what `E_IO_TRANSIENT` now reports, and `HookedFs::from_env` replaces
  `CrashAtFs::from_env`.
- The schema describes `E_IO_TRANSIENT` by the journal append that failed (other failures before
  the commit point stay `E_IO`): the store cut what the append wrote back off the journal, so
  nothing was committed, and running the same operation again is safe.
- **Contract change (text only):** `retryable` is now defined as "can succeed when it is run
  again: as it is after a transient failure, or, after a lost compare-and-swap, with what the
  error names re-read"; it said "retried unchanged", which does not describe a lost
  compare-and-swap. No flag, exit code or schema shape changes.
- `init` takes `FORMAT` back (and syncs its removal) when the sync of the store's root after it
  fails, so that no store looks complete whose completion mark may not be durable; when it made
  the store but reading it back fails, the error says so with `details.created: true`.
- Power-loss crash images (`crates/swarmops-cli/tests/power_loss.rs`): every cut of a recorded
  run, with seeded choices of what a power loss keeps (unsynced data dropped, cut at 512 B or
  4 KiB, zero-filled, or with its first sector lost; directory entries that no fsync covered
  reverted), opened with the binary, must recover a prefix of the run's transactions that holds
  every committed one.
- Test infrastructure: the support the tests share is one crate, `swarmops-testkit` (a
  dev-dependency; no test includes another crate's file by its path any more); the store's
  `CrashAtFs` compiles only with the `test-hooks` feature, like `FailAtFs` and `OrderCheckFs`,
  so no build without it can name a fault double; and where a crate has several integration-test
  files, they are the modules of one test binary, `tests/all.rs`, their tests named
  `<file>::<test>` (the CLI's release proof, `test_hooks`, stays a binary of its own; the CLI's
  snapshots are now `all__cli__*.snap` and `all__mcp__*.snap`, their contents unchanged).
- Recovery after a power loss: an unlisted journal frame whose header's length reads 0 is a torn
  tail and is cut (it was `E_STORE_CORRUPT`); readers drop zeroed entries at the end of
  `journal.off`, as recovery did; past the checkpoint and the sorted views, the index counts only
  the records the journal vouches for, so zero-filled index records are replayed rather than
  counted (they gave wrong counts and seqs, or `E_STORE_CORRUPT`); and a snapshot that opens
  while a writer cuts the index counts every record the checkpoint passed or the sorted views
  cover.
- Where the journal cannot vouch for the index's records past the checkpoint (no journal, a
  damaged one, or a `journal.ckpt` that does not hold a u64le: damage verify reports), the
  index counts them by their lengths and now checks their `par.col` entries against the layout
  (each record's parents start where the previous record's end): a zeroed entry, which a power
  loss leaves, is `IndexCorrupt` (`E_STORE_CORRUPT`) and nothing is cut. Before, the next write
  cut `parents.log` at the zeroed entry and replayed over the parents of checkpointed records.
  `Index::open` returns any other error reading `journal.ckpt` instead of counting by length.
- `Journal::frame_count` drops the zeroed entries at the end of `journal.off` that readers and
  recovery drop; a reader's frame count after such a power loss was one too high until the next
  writer ran (the frames it read were right).
- A reader's snapshot compares the frames it reads again whole instead of counting them: a frame
  that a failed sync or rewrite cut back (storage spec §2 rule 3 (a)/(c)) and the next writer
  replaced at its seq no longer lays its records over the replacement's ref files (a ref at a
  target that never committed, at a version that never existed).
- `verify` reports a `journal.ckpt` that is not a frame boundary (every read and write fails on
  one); before, it reported nothing while the index counted by length.
- Store files that are not regular files (a FIFO, a device, a socket, or a symlink to one) are
  refused with an error instead of opened; a FIFO at `meta/journal.log` used to hang every
  command.
- A store path that leads under `/mnt/` is refused on open too (`E_UNSUPPORTED_FS`), not only
  by `init`: a store copied, moved or symlinked onto a Windows drive under WSL is not opened.
- `bench/compare/` (`just bench-compare`, `--quick` for a smoke run): the native metadata
  store, through its public API, against SQLite, redb, fjall and sled on SwarmOps' own
  workloads, in a cargo workspace of its own that `just check` never builds; SwarmOps depends on
  none of them. Its numbers, in the M1.5 section of [docs/perf.md](docs/perf.md), are the
  quiet-window run's at the plan's sizes (2026-10-17), no longer provisional.

#### M1.6: the code-review follow-ups (2026-10-20)

- The store-level 32-process contention measurement (`crates/swarmops-store/tests/contention.rs`)
  can add other writers' object syncs: with `SWARMOPS_TEST_CONTENTION_OBJECT_SYNCS=<n>` each
  writer puts and flushes n new 4 KiB blobs of its own before each transaction, outside the
  lock, as a CLI commit writes its objects, and the table gives n and each transaction's time
  in the lock once `write()` returned (p50, p99 and max). A test in the default gate checks that
  the objects are new each time and durable before the lock is taken, and that none of their
  calls is made under it. The run fails unless every writer put its n objects before each of its
  transactions (its line says so, and the store holds them all), which a second test in the
  default gate checks on a run of two writers.
- A criterion bench of a compaction on its own (`crates/swarmops-store/benches/compaction.rs`,
  which `just bench` and `just perf` run): `Index::compact()` with every sync on, on an index of
  100k and of 1M records whose unsorted tail has just passed the threshold (1,025 records), the
  sorted views restored before each iteration, untimed. It prints the bytes each compaction
  writes (both views whole, 80 per record); `SWARMOPS_BENCH_COMPACTION_RECORDS` sets other sizes
  for a quick check.
- `bench/compare` times each seeded store's first open after its load (`first_open_ms`, a W4
  section per size), once, before any workload opens it; and `fjall-unflushed`'s W4 opens are
  each of a fresh copy of its store as the load left it, taken at seeding, so that its row is
  that store's, not the one its first open settled. [docs/perf.md](docs/perf.md) keeps the
  2026-10-17 run's cells until the next quiet-window run measures the ones this changes.
- `verify` reports each problem as a finding too: `VerifyReport.findings`, at the same index as
  its line in `problems`, gives the problem's `kind` (`missing_object`, `index_mismatch`, …) and
  what it names (`object`, `changeset`, `parent`, `seq`, `offset`, `ref`, `cause`), so that a
  program acts on the damage without parsing the lines. A failed `verify`'s `E_STORE_CORRUPT`
  carries it in `details` with the rest of the report, and the hint after a deep one names
  `details.findings`. `swarmops schema` lists `VerifyFinding`.
- `swarmops mcp`: `read_file`'s entry in `tools/list` carries
  `_meta: {"anthropic/maxResultSizeChars": 200000}`, which Claude Code reads to keep a
  `read_file` result of up to 200,000 characters in the conversation instead of saving one over
  50,000 to a file. No other tool carries `_meta`.
- `swarmops_store::FsMethod` names the 14 methods of the `FsOps` seam (`FsMethod::ALL`,
  `as_str`, `is_mutating`, `flushes`): the one list the fault doubles and the test kit's
  watchers classify calls with. It compares equal to the method's name.
- Test infrastructure: where a crate's integration tests are the modules of `tests/all.rs` (core,
  api, store, repo and the CLI), `every_test_file_is_a_module_of_all_rs` fails on a file of
  `tests/` that no `mod` line names, which `autotests = false` would leave unbuilt.
- Test infrastructure: the power-loss run places a frame whose length field straddles a 512 B
  sector boundary, which a lost first sector leaves a wrong length that is not 0, and its slow
  variant ends with a transaction of more than 1,024 changesets, so that its images hold the
  compaction a writer makes past that many records (an index sync, the compaction, the
  `Compacted` frame, the checkpoint), which the run's own compaction did not.
- `meta.recovered` in the envelope of `commit`, `ref set` and `ref delete` (the CLI's and the MCP
  server's alike): what the command's write repaired before its own work, of what earlier
  writers left when they crashed or failed after their commit point (a torn journal tail cut,
  frames listed or written afresh, `journal.off` bytes dropped, index records cut, a compaction
  finished, changesets and ref files replayed), in offsets and counts, on success and on
  `E_CAS_MISMATCH`; omitted when there was nothing to repair. It is no warning: `warnings` still
  names only the steps after the command's own commit point that failed. `swarmops schema`
  gains `RecoveredOut` and `LogCutOut`. In the library, `MetaWrite::recovered()`,
  `Journal::repair_tail()` (a `TailRepair`) and `recovered` on every `CommitOutcome` and
  `CasOutcome` report the same.
- `commit --advance REF` (`CommitReq.advance` and `CommitResp.advanced`; in Rust,
  `CommitRequest::advance` and `CommitOutcome::advanced`) publishes a changeset: the same
  transaction moves REF to it, so one journal frame holds both and they are recorded together or
  not at all, and only as a one-step fast-forward from the commit's parent (for a root commit,
  only if REF does not exist yet). When REF names anything else, the commit records nothing and
  fails with `E_CAS_MISMATCH` (exit 4, `details {name, actual, committed: false}`); the same request
  made again in the same second (a commit signs in whole seconds) is the same changeset, so a replay
  (`advanced.already_applied`), but made a second or more later it is another changeset, which gets
  `E_CAS_MISMATCH` when the first attempt published, its `actual` being that attempt
  (`commit --key`, M4, makes any retry a replay). Without `--parent` it builds on REF's head. A
  publish takes one lock pass and four syncs, where a commit and then `ref set` take two and six.
  `swarmops mcp` refuses `advance` without a full 64-hex `parent` (`E_INVALID_ARG`),
  `swarmops schema` lists `Advanced`, and `swarmops_store::check_ref_name` checks a ref name as
  every ref operation does.
- The CLI-level 32-process contention measurement (`crates/swarmops-cli/tests/contention.rs`)
  can publish: with `SWARMOPS_TEST_CLI_CONTENTION_ADVANCE=1` each child publishes every commit
  on a ref of its own in one transaction (`commit --advance`), with `=0` in two (`commit`, then
  `ref set`), and the summary gives publishes/s and the journal frames, so the lock passes, each
  publish took. [docs/perf.md](docs/perf.md) has the section the quiet-window runs fill.

#### M2: diff, history and derived data (2026-10-20)

- `swarmops-store`: a store of derived values, `DerivedStore`, for the derivation engine to come:
  each value in an immutable file of its own under `meta/derived/`, named by its slot
  (`DerivedSlot`: the derivation's name and version, the object the value is about, and a key),
  with a header of its own (`SWDV`, version 1) and a BLAKE3 trailer over the slot and the value.
  The files are a cache outside the journal: written to a temp file and linked into place
  without a sync or a lock, and a file that fails its checks reads as missing and is removed, so
  that the value is derived again. `FORMAT` stays 1.
- `MetaRead::first_parent(id)` gives a changeset's first parent without the rest of its index
  record (`None` if the snapshot does not hold it, `Some(None)` for a root): the record's
  `par.col` entry and 32 bytes of `parents.log`, where `changeset` reads four columns and every
  parent. `Index::id`, `Index::generation` and `Index::first_parent` read one field of a record.
- A criterion bench of history lookups (`crates/swarmops-store/benches/history.rs`, which
  `just bench` and `just perf` run), on linear chains of 200,000 and 1,000,000 changesets:
  100-step first-parent walks by `MetaRead::changeset` and by `MetaRead::first_parent`,
  `Index::find` hits and misses beside a plain bisection, an insert with no record and with
  1,024 records in the index's unsorted tail, and `reflog("main", 100)` with 31 frames past the
  checkpoint, each with its percentiles. `SWARMOPS_BENCH_HISTORY_CHANGESETS` sets other sizes
  for a quick check.
- `swarmops-store`: `ObjectStore::stat` gives an object's kind and payload length
  (`ObjectStat`) from its file's 16-byte header alone: the payload is neither read nor verified,
  the length is the header's own, and a header the store does not write is `BadHeader`.
  `ObjectStore::retain` stats an object that a caller relies on without writing it and, when it
  is stored, has the next `flush` fsync its fan-out directory, as a put of a stored object does.
- Path history: `swarmops history <rev>:<path>` (operation and MCP tool `path_history`) lists the
  changesets that changed a file, or anything below a directory, newest first along first
  parents, each with what the path holds after it (`put` with its kind and id, or `delete`), 50
  a page with a cursor; a path nothing on the chain touched is `E_PATH_NOT_FOUND`. `log --path P`
  (`LogReq.paths`) keeps only the changesets that change those paths. Both chase `LastModified`,
  the second built-in derivation: the newest changeset on a first-parent chain whose delta has an
  entry at the path or below it, derived where the path changed and stored every 64 changesets
  of a quiet stretch, so that a later query walks at most 64. In the Rust API:
  `RepoRead::{last_modified, path_history, log_paths}`, `PathEvent` and `PathFilter::prefixes`.
- Blobs by id: `blob_put` stores content as a blob outside any commit, durable when it returns
  (`{blob, size, written}`; it needs an identity), and `blob_has` gives those of some ids the
  store holds blobs under (CLI `blob put <file|->` and `blob has <id…>`; MCP tools `blob_put` and
  `blob_has`). A change's content may name a stored blob by its id, `{"blob": "<id>"}`: the
  commit neither sends nor reads the bytes, and retains the blob, so that its flush makes the
  blob's name durable before the frame names it; an id the store holds no blob under (none, a
  tree's, a changeset's) is `E_INVALID_ARG` `{index, path, blob}`, and nothing is written. In
  the Rust API: `ContentInput::Blob`, `BlobPutReq`, `BlobPutResp`, `BlobHasReq`, `BlobHasResp`,
  `StoreOps::{blob_put, blob_has}`, `ChangeOp::PutBlob`, `Repo::put_blob`, `Repo::has_blob`,
  `BlobPut` and `RepoError::MissingBlob`.
- `swarmops-store`: `ObjectStore::read_chunks` reads an object as `get` does, with the same
  checks and errors, but hands its payload over in pieces, in order, instead of returning it: a
  raw payload from the buffer its file was read into, a zstd one 64 KiB at a time as its stream
  decodes. The pieces are hashed as they go and the id checked at the end, so `Corrupt` comes
  after the pieces before the damage went by, and the caller drops what it kept of them.
- `swarmops-repo`: `RepoRead::read_file_span` reads part of a file (`FileSpan::Bytes` from an
  offset, or `FileSpan::Lines`, a 1-based range of lines, of which those that exist) and keeps
  only that part, its first `max` bytes, while the whole blob is verified (`FileSpanRead`).
  `RepoRead::list_tree_depth` lists a directory and the directories below it to a depth, in
  pre-order and a page at a time after a path, each leaf with its size from its object's header
  (`ListedEntry`, `TreePage`); a page reads the trees it enters, and none to learn that more
  entries follow.
- `read_file` (`swarmops cat`) reads part of a file: from `offset` (`--offset N`), or the lines a
  `lines` range names (`--lines A-B`, numbered from 1, both included; those that exist). Its
  result says where the content starts (`offset`), which lines it holds (`lines`) and, when it
  is cut short, where the next page starts (`next_offset`), so that text paged by it stays
  text; an offset inside a character gives base64, and an offset past the end empty content. It
  keeps only the bytes it returns while it verifies the whole blob. An MCP agent, which the
  server asks for 32 KiB or less at a time, can now read all of a larger file.
- `read_tree` (`swarmops ls`) takes `depth` (`--depth N`, 1 to 64): the directories below the
  listed one too, in pre-order, a directory before the entries below it, each entry past depth
  1 with its `path`, and a page's cursor its last entry's path. Every file entry carries its
  `size`, from its object's header. The two tools' descriptions in `swarmops mcp` say so.
- `swarmops derive`: the eager derived values (storage spec §5; today each changeset's
  first-parent skip list) derived ahead of any reader. Once, it derives those of the changesets
  the journal records in up to 1,000 frames, from the first or after `--after N`, and reports
  how many it `derived`, how many were `cached` already, and `next`, the last frame it read, to
  go on from. With `--follow` it runs until it is killed: from the journal's end (its start with
  `--from-start`), it derives the changesets as they are committed, looking again every
  `--interval-ms` (500) once it has caught up, and prints one envelope a line for each run that
  derived something. Commits derive nothing. It writes only the cache of derived values, so it
  needs no identity, and it has no MCP tool. Its API is `StoreOps::derive` (`DeriveReq`,
  `DeriveResp`) over `Repo::derive_eager`.
- `swarmops diff FROM TO`, the operation `diff` and its MCP tool: the files that differ between
  two revisions, in path order, a page at a time (`--limit`, 500 by default, and an opaque
  `--cursor`), within `--path`s if given. `--format names` (the default) gives each file's path
  and status (`added`, `deleted`, `modified`, `type_changed`; no renames); `stat` also whether
  it is binary and the lines it inserts and deletes; `unified` also its kinds, its blob ids and
  its hunks, with `--context` lines (3 by default) around each change, the page within
  `--max-bytes` of JSON (32 KiB by default; its first file comes all the same, its hunks cut to
  those that fit, `hunks_truncated`). Both revisions resolve in one snapshot. Below it,
  `RepoRead::diff` gives the leaf-level changes between two changesets' trees.
- `swarmops-repo`: first-parent ancestry in a snapshot (`RepoRead`): `first_parents(from)` walks
  back through first parents, `ancestor(cs, n)` gives the changeset `n` first-parent steps back,
  `is_ancestor(a, of)` says whether `a` is on `of`'s first-parent chain, and `merge_base(a, b)`
  gives the newest changeset on both chains. Generations prune every walk: a gap of up to
  `SKIP_AFTER` (1,024) generations is walked by narrow index lookups, and a wider one lifts along
  the first-parent skip list, a jump per bit of the distance, deriving the lists the store lacks
  (the first derivation of a list derives the whole chain below it, once). A revision's `~N` walks
  through `ancestor`, so a long one takes a few lookups, and one that reaches past a root by more
  than its changeset's generation is `E_REV_NOT_FOUND` without a walk.
- `resolve_rev`, as `swarmops resolve <rev>` and the MCP tool `resolve_rev`: the full id of the
  changeset a revision names, resolved as every operation resolves one, and its kind, `ref` when
  the revision is exactly a ref's name and `changeset` otherwise (M5 adds `workspace`). The schema
  gains `ResolveRevReq`, `ResolvedRev` and `RevKind`, and `StoreOps` gains `resolve_rev`.

#### M3: git import (2026-10-20)

- `git import`'s contract, ahead of the command that serves it: `swarmops schema` lists
  `GitImportReq` (`git_path`, the source repository on the caller's host; `git_ref`, `HEAD` by
  default; `target_ref`, `main` by default; `lossy`, the classes of content the import may drop
  or normalize, none by default), `GitImportResp` (`imported`, `skipped`, `head_changeset`,
  `git_head_sha`, `target_ref`, `ref_moved`, `old_head`, the first 100 commits taken lossily
  and `lossy_total`, `warnings`), `LossyClass` (`gitlinks`, `paths`, `modes`, `tree_order`,
  `empty_trees`, `nonstandard`) and `LossyCommitOut`, and six error codes, none retryable:
  `E_GIT_NOT_A_REPO` and `E_GIT_REF_NOT_FOUND` (exit 6), `E_GIT_IMPORT_DIVERGED` (exit 3,
  conflict), `E_GIT_IMPORT_LOSSY`, `E_GIT_UNSUPPORTED` and `E_GIT_READ` (exit 1). Like `init`,
  `git import` runs on the client: it is no `StoreOps` operation and no MCP tool, and no server
  receives its request (operations spec §0, §1 and §7).
- The git fixture harness (`swarmops_testkit::git`, the test kit's default feature `git`): POSIX
  sh scripts in `crates/swarmops-testkit/git-fixtures/scripts/` that run real git 2.43.0 in a
  pinned environment, and the deterministic archives of what they made, committed beside them,
  each repository with a `manifest.json` from git's plumbing (its refs, its commits parents
  first with their bytes, its trees' bytes and its blobs' sizes), the oracle the git import's
  tests compare against without running git. `fixture(name)` extracts one for a test;
  `just git-fixtures` rebuilds them all with the declared git (`git-fixtures/GIT_VERSION`), and
  `check` rebuilds them with any git and names each difference from the archives' manifests,
  for the version matrix. The first fixtures: `linear`, `merges` (a `--no-ff` merge, an octopus,
  a criss-cross pair, an annotated and a lightweight tag) and `modes` (exec bits, symlinks,
  `a.b` beside a directory `a`, a path with a space and a UTF-8 name).
- `MetaWrite::insert_changesets_batch` queues many changesets in one call, in order, each parent
  in the store, queued before or earlier in the batch, and all or nothing: an error (more than 255
  parents, a parent listed twice or missing, more records than a frame's 65,535) queues none of
  them. A changeset the store holds, the transaction queued or the batch named before is
  `AlreadyPresent`. The index appends a frame's changesets with one write to each of its five
  files (`Index::append_batch`), as a commit materializes them and as replay restores them, where
  it made five writes for each changeset.
- The external id index (`swarmops_store::ExternalIds`, storage spec §7): which changeset of the
  store an id of another system names, a git commit's oid for the import, as one immutable
  44-byte file per id under `meta/external_ids/<system>/<2 hex>/<rest of the hex>` (magic
  `SXID`, version 1, a faithful flag, the changeset's id and a CRC). An entry is written as an
  object is, from a synced temp file in `objects/tmp/` linked without replacing anything, and
  `put_batch` makes its names durable before it returns; an entry already there is left as it
  is, and one with other bytes is reported as a conflict. The index is a cache outside the
  journal, so `FORMAT` stays 1. A new write-order rule, W8 (storage spec §2 rule 4), links an
  entry only once every byte its process wrote to `journal.log` is durable, so that no entry
  names a changeset a power loss can take back; the order checker enforces it.
- `verify` checks the external id index, once the changesets are: an entry whose file is not one
  (`external_id_corrupt`), one that names a changeset the store does not hold
  (`external_id_dangling`), and one of system `git` whose changeset's `git.oid` is another oid
  (`external_id_mismatch`) are findings of their own, naming the entry by `system` and `key`
  (lowercase hex), which `VerifyFinding` gains; `swarmops schema` shows them. In the library,
  `Repo::external_id` and `Repo::record_external_ids` read and write the index, and
  `VerifyProblem` has the three variants. The power-loss run records two external ids after
  their commits' frames: in every image each is whole or absent, and there once its put
  returned.
- The git fixture set: seventeen fixtures more, each script saying what it makes and a test
  holding it to that. What git writes and a changeset keeps faithfully: `roots` (three roots,
  one of them the empty tree's; an empty commit; a message without a final line feed, and an
  empty one), `signatures` (offsets from -1200 to +1400, `-0000`, a time before 1970, a Latin-1
  author name), `headers` (`encoding`, `gpgsig`, `mergetag` and an unknown header),
  `many_parents` (a merge of 255 roots, and one of 256, which the import refuses), `refs`
  (branches for incremental imports and for divergence), `replace_ref` and `loose_and_packed`.
  One for each class of content a changeset cannot hold, crafted with
  `git hash-object --literally` where git would not write it: `gitlinks`, `paths`, `modes_odd`,
  `tree_order`, `empty_trees` and `nonstandard`. And the repositories the import does not read:
  `sha256`, `shallow`, `partial_clone` and `corrupt_object`.
- `swarmops-git`, the git import's reading of git: git's commit and tree bytes parsed into their
  fields and rendered back byte for byte (`RawCommit`, `RawIdent`, `RawTree`), lenient where the
  fields can still hold a commit git would not write, which then renders to other bytes; the ids
  git gives objects (`GitOid`, `git_hash`); and `GitSource`, what the import reads from a
  repository: `HEAD`, a ref's full or short name or 40 hex digits resolved to a commit through
  annotated tags, and objects' bytes, each checked against its id. `GixSource` reads a repository
  on disk through gix 0.89.0 (pinned, its `sha1` feature alone, the workspace's one new runtime
  dependency): it refuses a SHA-256 repository, reftable refs and a shallow clone before any read,
  and never follows replacement refs.
- `Repo::commit_raw` and `Repo::commit_raw_batch`, the git import's writer: changesets whose
  fields the caller gives, with parents in their order (none makes a root changeset whatever refs
  exist, so a store can hold a second root), signatures, extra headers, message and metadata kept
  verbatim, and a root tree stored first (`Repo::put_objects`). The delta is the tree diff from
  the first parent's root to that root, so it agrees with the root by construction. A batch is
  one journal frame, one commit point: its changesets' objects are put and flushed first, with
  every object put on the handle since its last flush, then inserted in one transaction; a
  changeset the store holds is `AlreadyPresent`, and a batch of none new appends no frame. Each
  raw is checked before anything is written: at most 255 parents, none twice, metadata keys
  ascending and none ending in `:b64`, a root the store holds as a tree, parents in the store or
  earlier in the batch, and at most 65,535 changesets to a batch. `Repo::raw_batch` builds a
  batch a changeset at a time and gives each one's id before anything is written, so that a
  later one can name it as a parent. The power-loss run ends with a batch of three, a second
  root, its child and a merge: every image holds all three or none.
- Git fixtures that need a newer git: a script whose first lines say `# needs git X.Y` is skipped,
  and said to be, by an older git (`regenerate`, `check`, the fixture tests), so that it has no
  archive while the declared git is older. `swarmops_testkit::git::names()` lists only the
  fixtures at hand, `skipped(name)` says why one is not, and `check` writes the version of the
  git it ran to `<dir>/GIT_VERSION`. The first is `reftable` (`git init --ref-format=reftable`,
  git 2.45 and newer), which `GixSource::open` refuses as `Unsupported(RefStorage("reftable"))`:
  `a_reftable_repository_is_unsupported` checks that on the fixture the version matrix's newest
  git makes, and says why it checks nothing where there is none.
- The git version matrix: `scheduled.yml`'s weekly `git-matrix` job builds git 2.39.5, 2.43.0
  and 2.56.0 (the newest on 2026-10-19) from kernel.org's release tarballs, each checked against
  its pinned SHA-256 and cached by release. With each it rebuilds every git fixture and names
  each difference from the archives' manifests (`check_git_fixtures`), then runs the fixture
  tests and the git import's on the fixtures that git made (`SWARMOPS_GIT_FIXTURES_DIR`).
- The conversion of git's objects (`swarmops_git::Converter`, storage spec §7): a commit becomes
  the fields of a changeset (its parents in order, each once; its author, committer, message and
  other headers as they are, a time before 1970 included; `git.oid` metadata, and `git.lossy`
  before it when anything was lost), and its trees and blobs SwarmOps trees and blobs, each git
  tree and blob converted once. A commit is taken without loss only if it renders back to its own
  git id, trees in git's order and with git's modes (`render::tree_bytes`, `render::commit_bytes`,
  which M8's export reuses), and the conversion checks that for every commit. What a changeset
  cannot hold is refused naming the commit, the path and the class (`LossyClass`: `gitlinks`,
  `paths`, `modes`, `tree_order`, `empty_trees`, `nonstandard`), unless that class is allowed,
  when it is dropped or normalized and named in `git.lossy`; a mode of no file, directory,
  symlink or gitlink, more than 255 parents and a blob longer than an object holds are refused
  whatever is allowed, the blob before it is read.
- The git import (`swarmops_git::import`, storage spec §7): every commit a git ref reaches that the
  store does not hold becomes a changeset, parents first with all its parents in their order (a
  merge's other side included), 8,192 to a journal frame, and each frame's external ids are
  written once it is durable; then the metadata is flushed, so that no reader parses the import's
  frames again, and the target ref (`main` by default) moves to the tip's changeset by
  compare-and-swap. Run again, it takes only the commits after the one the target's head holds
  (`git.oid`), even one the tip reaches through a merge's second parent, and writes nothing when
  that is the tip; a target whose head holds no git commit, or one the tip does not reach, is
  diverged, and nothing is written. A store failure, or a frame that fails after its commit point,
  stops it after its last committed frame (a busy writer lock is retried three times), and the
  same import run again continues there; a commit the conversion refuses stops it before the
  frame that would hold it. The report counts the commits imported and the known ones the walk
  stopped at, lists the first 100 recorded with a loss, and carries what the import's writes
  repaired. An incremental run converts with the base's trees and blobs taken as made, so it
  reads only what the new commits changed.
- `swarmops git import PATH`, the git import as a command, with `--ref GIT_REF` (`HEAD` by
  default), `--into REF` (`main`), `--lossy CLASS` and `--gitlinks refuse|drop`. It reads the git
  repository at `PATH` on the caller's host, and reports `GitImportResp`, and what its writes
  repaired in `meta.recovered`. `--lossy` names a class to drop or normalize instead of refusing
  it, repeatable, or `all`; `--gitlinks drop` is `--lossy gitlinks`, and `--gitlinks`, given,
  decides that class whatever `--lossy` says. It needs an identity, checks the target's name
  before it opens the repository, and raises the six `E_GIT_*` codes: `E_GIT_NOT_A_REPO`
  (`git_path`), `E_GIT_REF_NOT_FOUND` (`git_ref`, `kind`), `E_GIT_UNSUPPORTED` (`reason`:
  `object_format`, `ref_storage`, `shallow`, `parents` or `mode`), `E_GIT_READ` (`reason`:
  `missing`, `corrupt`, `malformed` or `io`; `object`), `E_GIT_IMPORT_LOSSY` (`commit`, `class`,
  `path`, and `path_base64` for a path that is not UTF-8) and `E_GIT_IMPORT_DIVERGED` (exit 3),
  besides `E_TOO_LARGE` for a blob past the object limit, `E_CAS_MISMATCH` for a target moved
  meanwhile and a store failure's own code, each of an import that stopped after it committed
  frames saying how far it got (`imported`, `last_commit`) and hinting to run it again. Like
  `init`, it is a client-side operation (`LocalOps::git_import`): no `StoreOps` method and no MCP
  tool.
- The git import's crash and fault sweeps (`swarmops-git`'s `tests/sweeps.rs`, behind the crate's
  new `test-hooks` feature, which the gates turn on): `refs`' `newer`, two changesets to a frame,
  imported onto a fresh store and onto one that holds `older`, each run a child process under the
  order checker, is crashed (`SWARMOPS_TEST_CRASH_AT`) or failed (`SWARMOPS_TEST_FAIL_AT`: once
  with `EIO`, persistently with `ENOSPC`) at its filesystem calls. A failed import stops with a
  store failure that names no host path, its committed frames recorded and nothing after them,
  or succeeds with every commit recorded and a warning that says what failed. The store it leaves
  verifies clean, and the same import run again ends with the changesets, the external ids and
  the target an uninterrupted import leaves; after a crash it takes exactly the commits whose
  external ids the crash left out. The default gate takes every fifth call (and fails them onto
  the fresh store alone); `just check-slow` takes every call, and imports 100,000 synthetic
  commits: 13 frames, at most 14 compactions, a peak resident memory under 1 GiB.

#### M4: merge and landing (2026-10-20)

- The contract of merging and landing, in `swarmops schema`, which `land`, `op log`, `op undo`
  and `commit --key` serve: `LandReq` and `LandResp` (`status` `landed` or `replayed`);
  `ConflictReport`, with `FileConflictOut`, `ConflictKindOut`, `HunkOut` and `LinesOut`, the
  report a land that conflicts returns in `details.report` (each file with its kind of conflict,
  one of seven, its hunks, whose lines count from 1 with both ends included, their texts within
  `max_conflict_bytes`, and the target's changeset and agent that last changed it); `OpLogReq`,
  `OpLogPage`, `OperationOut`, `OperationKind`, `OpChangesetOut` and `RefMoveOut` for the
  operation log; and `UndoReq` and `UndoResp`. `CommitReq` gains `idempotency_key`, and
  `CommitResp` gains `replayed`, written in every commit response (one without it, from a store
  that predates it, reads as `false`); `show`'s `ChangesetDetail` gains `lander`, `land_key` and
  `squashed_from`, omitted for a changeset no land made. Five error codes join the table:
  `E_CONFLICT` (exit 3, which no code had), `E_CAS_RETRY_EXHAUSTED` (exit 4, retryable),
  `E_NOTHING_TO_LAND` and `E_NOTHING_TO_UNDO` (exit 1) and `E_OP_NOT_FOUND` (exit 6).
- `commit --key UUID` (`idempotency_key` in the API and over MCP) makes a retried commit replay.
  The key, a uuid v7, signs the commit at the key's time, to the second, and is recorded as
  `commit_key` metadata, so the same request on the same parent makes the same changeset, which
  the store holds already: the retry answers `replayed: true` with the first attempt's changeset,
  and writes nothing. A keyed commit whose parent (given, or the head of `main` or of the ref it
  advances) is the changeset its key made replays that one too, decided under the store's lock;
  with `--advance`, as `advanced.already_applied`. A key that is not a uuid v7 is
  `E_INVALID_ARG`. `replayed` is `true` for any commit that found its changeset in the store and
  wrote nothing.
- `swarmops op log` and `swarmops op undo`, and the MCP tools `op_log` and `undo`: the journal
  as the store's operation log, newest first (each write transaction with its number, its kind,
  its changesets and its ref moves, a page at a time below `--before`), and an undo that moves
  every ref an operation moved back to what it named before, by compare-and-swap in one
  transaction of its own, all of them or none. A ref moved since refuses the undo with
  `E_CAS_MISMATCH` (`details.op` names the operation) and nothing is written; an undo run again
  is a replay (`already_applied`); changesets are never removed. They raise `E_OP_NOT_FOUND` and
  `E_NOTHING_TO_UNDO`. The MCP server's instructions name the two tools, and leave the sizes of
  small reads to each tool's description.
- `swarmops land HEAD` and the MCP tool `land` (`StoreOps::land`): a land merges an agent's change,
  HEAD with its first-parent chain after `--base` (by default the merge base of HEAD and the
  target's head), three-way onto `--target` (`main` by default), and publishes it there as one new
  changeset whose parent is the target's head, with the target's move to it, in one transaction. A
  target that moves meanwhile makes the land merge again and retry, up to `--max-attempts` (8 by
  default, 1 to 32), then `E_CAS_RETRY_EXHAUSTED` (exit 4, retryable). A change that conflicts is
  `E_CONFLICT` (exit 3) with the conflict report in `details.report`, its hunk texts within
  `--max-conflict-bytes` (32 KiB), and nothing is recorded; a land with nothing to add is
  `E_NOTHING_TO_LAND`, saying why (`on_target`, `no_changes`). With `--key`, the same land run again
  replays (`status: replayed`), and the key given for another change is `E_INVALID_ARG`. `--human`
  prints `landed <id> on <target> (onto <head>, attempt <n>)`, `replayed <id>`, or each conflicting
  path with its kind. `show` names a land's lander, key and squashed chain (`lander`, `land_key`,
  `squashed_from`). The MCP server's instructions now say to publish a changeset by landing it,
  where they told an agent how to retry after `E_CAS_MISMATCH`.
- `swarmops land HEAD --resolve FILE` (`-` for stdin) and the `land` tool's `resolutions`
  (`LandReq.resolutions`, with `ResolutionsInput`, `ResolutionInput` and `Side` in
  `swarmops schema`) resolve a land's conflicts without a checkout: decided against the conflict
  report's `theirs`, each path takes `ours` or `theirs`, new `content` (as a commit's change gives
  it, with an optional `mode`) or `delete`, for the path and every path below it, whether it
  conflicts or not, while the target still holds that path as `against` did. A path the target
  changed since comes back in the `E_CONFLICT` report's new `stale` list, and a conflict left
  unresolved fails the land as before; `swarmops mcp` takes `against` only as a full 64-hex id.
- A land's three-way merge takes a file's kind from one side and its content from the other only
  between a file and an executable: a symlink on one side against a file on the other, both sides
  having changed the path (one its kind, the other the link's target or the file's content), is a
  `binary` conflict, as git's merge reports it ("distinct types"), never a regular file holding a
  link target or a symlink to a file's text.

#### M8: git export (2026-10-20)

- `MetaStore::read_committed`, a snapshot of the committed metadata only, for the readers whose
  results leave the host (M8's git export first, then M11's `serve` and `push`): it takes
  `meta/lock` as a writer does, waiting up to the store's `lock_timeout` (then `StoreError::Busy`,
  the retryable `E_BUSY`), runs the journal's tail check, takes the snapshot `MetaStore::read` takes
  while it still holds the lock, and returns it with what the tail check repaired (`TailRepair`). No
  writer is mid-commit meanwhile, so neither a power loss nor a failed sync's cut can take back what
  it returns; on a clean tail it writes nothing. Local readers (the CLI, the MCP server) keep the
  lock-free snapshot. In the test kit, `fs::HoldThenFail` holds one `append_durable` between its
  write and its flush until the test lets it go, then fails it.
- `git export`'s contract, ahead of the command that serves it: `swarmops schema` lists
  `GitExportReq` (`git_path`, the repository to write, on the caller's host; `source_ref`, `main`
  by default; `branch`, `main` by default; `force`, off by default) and `GitExportResp`
  (`exported`, `objects_written`, `head_changeset`, `head_sha`, `branch`, `old_sha`,
  `branch_moved`, `incremental`, `rewritten`, `warnings`), and two error codes, neither
  retryable: `E_GIT_EXPORT_DIVERGED` (exit 3, conflict; `details.reason` is `branch_ahead`,
  `both_moved`, `branch_moved`, `source_moved_back`, `lossy_history` or `moved_during_export`)
  and `E_GIT_WRITE` (exit 1; `io`, `ref_locked` or `ref_name_conflict`). Like `git import`,
  `git export` runs on the client and needs no identity: it is no `StoreOps` operation and no MCP
  tool, and no server receives its request (operations spec §0, §1 and §7).
- The git side of the coming `git export`: `GitSink`, what the export writes to a git repository,
  and `GixSink`, its gix implementation. Objects are written as loose files, as git writes them; one
  whose loose file is there already but does not read back as the object (an export cut short left
  it, and a power loss emptied it) is written again, and the empty tree counts as stored only when
  it is (gix answers for it either way; git's own writer goes by the files). `sync` makes the files
  and their fan-out directories durable, in parallel, before a branch moves; gix syncs nothing
  itself. A branch moves by git's own lock protocol, as a compare-and-swap that git and other
  exports respect: `refs/heads/<name>.lock` created exclusively and tried again for up to 100 ms
  (git's `core.filesRefLockTimeout`), the branch compared under it, the new value synced, the lock
  renamed onto the ref and its directory synced, with no reflog entry. The sink refuses what the
  import refuses (SHA-256, reftable refs, a shallow clone), a branch name git does not take, a lock
  another writer holds or a dead one left, a ref in the branch's way, and a branch to an object the
  repository lacks. `swarmops_testkit::git::empty_repository()` makes an empty bare repository by
  hand, so that no test runs git.
- Export oids: the git commit each changeset exports to, derived as `git_oid` (version 1) by
  `swarmops_git::ExportOid` and kept in the derived store. An imported changeset whose lineage
  SwarmOps did not change exports to the commit it was imported from, read without a tree; any
  other is rendered and hashed, its trees' and blobs' git oids memoized per object
  (`git_tree_oid`, `git_blob_oid`), which `Repo::derived_store` and `DeriveCtx::derived_store`
  reach. A native changeset exports to a canonical commit with its own signatures and no extra
  header, its message followed by a trailer block: `SwarmOps-Changeset: <id>`, then
  `SwarmOps-Land-Key:`, `SwarmOps-CR:` and `SwarmOps-Squashed-From:` for the metadata keys
  `land_key`, `cr` and `squashed_from`, joined to the message's last paragraph when that is a
  trailer block already (a `Co-Authored-By:`), else after a blank line. The rendering is a
  contract like the hash contract: a change to it is a new version, which every branch exported
  before it meets as divergence (`docs/policies/versioning-and-stability.md`).
- The export engine, ahead of the command that runs it: `swarmops_git::export` writes the history
  behind a store ref into a branch of a git repository through a `GitSink`, reading the ref's head
  in a committed snapshot (`Repo::snapshot_committed`, over `MetaStore::read_committed`) so that
  nothing it sends off the host can be taken back. A history imported faithfully comes back byte
  for byte, and changesets made in the store come back as canonical commits; the same store gives
  the same shas. A branch at the export of the head or of an ancestor of it is continued: only
  what it does not reach is written, parents first, an object packed in the repository trusted
  and a loose one read back and written again if damaged. The objects are made durable, then the
  native commits' oids recorded in the external id index, then the branch moved by
  compare-and-swap. Any other branch stops the export before it writes (`DivergedReason`:
  `branch_ahead`, `both_moved`, `branch_moved`, `source_moved_back`, `lossy_history`, or
  `moved_during_export` from the compare-and-swap) unless it is forced, and a branch a work tree
  has checked out is refused.
- `swarmops git export PATH [--ref REF] [--branch BRANCH] [--force]` (op `git_export`): the export
  as a command, `LocalOps::git_export` on the client. It writes the history behind `--ref`
  (`main`) into the branch `--branch` (`main`) of the git repository at `PATH`, on this host, for
  `git push` to take on, needs no identity, echoes the head it read in `meta.resolved` and what
  the tail check of its committed read repaired in `meta.recovered`, and says what it did in one
  line under `--human`. It refuses with a hint for each case: `E_GIT_EXPORT_DIVERGED` (exit 3,
  `details.reason` and, where the branch holds one, `exported_changeset`; `--force` moves the
  branch all the same, by compare-and-swap), `E_GIT_WRITE` (`ref_locked` with the lock's path,
  `ref_name_conflict`, `io`), `E_INVALID_ARG` (`reason: "checked_out"`), `E_GIT_NOT_A_REPO` (the
  hint is `git init --bare`), `E_GIT_UNSUPPORTED`, `E_GIT_READ`, `E_REF_NAME_INVALID` (a source
  ref, or a branch name git does not take) and `E_REV_NOT_FOUND`. Every `LocalOps` registers the
  derivation of export oids as it opens a store. Like `git import`, it is no MCP tool.
- The round trip's acceptance tests (`crates/swarmops-git/tests/round_trip.rs`), umbrella §13's
  completion condition for M8: every faithful git fixture, imported, with two changes landed on it
  by keyed lands and exported back into its own repository, keeps every commit byte for byte and
  gains two single-parent commits on its tip, each message ending in the `SwarmOps-Changeset`,
  `SwarmOps-Land-Key` and `SwarmOps-Squashed-From` trailers, authored by the agent that made the
  change and committed by the lander, with the files the store holds; exported into an empty
  repository, the same history comes out under the same branch oid. A land undone after its export
  stops the next export (`source_moved_back`) until it is forced, which moves the branch back. The
  single-writer layout through a git remote round-trips, one host landing and exporting and the
  other importing, each step taking only what is new; when both hosts write, the export stops on
  `both_moved` and the imports on `E_GIT_IMPORT_DIVERGED`, and nothing is merged.
- The round trip through the binary, and `git fsck` of every export in the weekly version matrix.
  `the_round_trip_through_the_binary` runs the whole of it as `swarmops` commands: `git import` of
  `linear`, two changes committed and landed with keys (`land --key`) by another agent, `git export`
  into the fixture's own repository (the two lands as linear commits on its tip, each with its
  trailers, written as 8 loose objects), the same export again (nothing to write), the same store
  exported into an empty repository (the same branch sha), a commit made in git on top of the export
  and brought in by `git import --ref main --into main` (that commit alone, onto the second land),
  and the export after it, again nothing to write. `exported_repositories_pass_git_fsck` (ignored:
  it runs git, the one `SWARMOPS_GIT` names or `git` on `PATH`, and refuses a version other than
  `git-fixtures/GIT_VERSION`'s unless `SWARMOPS_GIT_FIXTURES_ANY_GIT=1`) imports the fixtures git
  made (`linear`, `merges`, `modes`, `roots`, `refs`, `loose_and_packed`), lands two changes on each
  and exports them into an empty repository, which `git fsck --strict --no-dangling` accepts and
  whose checkout gives the store's files, executable bits and symlink targets; for the crafted
  `signatures`, `headers` and `many_parents`, git finds in the export exactly what it finds in the
  fixture. The `git-matrix` job of `scheduled.yml` runs it with each of its gits (2.39.5, 2.43.0,
  2.56.0).
- Two documents for running SwarmOps, each command in them run and its output copied:
  [docs/operations.md](docs/operations.md), the operations guide (install from a clone or a
  release archive, a store, agents through `swarmops mcp`, the daily loop of import, commit, land
  and export with one writer per trunk, `verify`, backup under the writers' lock and restore,
  upgrades and `FORMAT`, limits, and what to monitor), and [docs/pilot.md](docs/pilot.md), the
  runbook of the first real use, on this repository (its history imported, one agent session
  through MCP, the export and its round trip, the repository's CI on it, what to measure, the
  success criteria and the rollback).
- [THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES), in every release archive: each third-party crate
  that the release build of `swarmops` and `so` compiles, with the license expression it declares,
  the license it is used under (one that `deny.toml` allows) and the license texts from its
  package, zstd's C library and the libdivsufsort-lite it includes among them.
  `scripts/third-party-licenses.py` writes it from `Cargo.lock`; `just check` fails when the file
  no longer matches (so it now needs `python3`, 3.11 or newer), and the release's packing fails
  without it.

#### The v0.1.0 release batch (2026-10-20)

- Landing from many processes at once is tested through the binary, in the default gate
  (`landing::eight_processes_land_concurrently_none_lost`, `crates/swarmops-cli/tests/landing.rs`):
  eight agents, each running one `swarmops` process at a time, make four keyed lands each on one
  `main`, each land a file of its own and every other one also the agent's line of a file they all
  share, and a land that ends `E_BUSY` or `E_CAS_RETRY_EXHAUSTED` runs again with the same key.
  Afterwards `main`'s first-parent chain holds every landed changeset exactly once and nothing
  else, each move of `main` a one-step fast-forward, `main` holds every land's file and line, and
  the store verifies clean. The test kit's `landing::CliLander` is the lander it runs.

### Changed

#### M1.6: the code-review follow-ups (2026-10-20)

- `missing_docs` joins the workspace lints (M0's set was `unsafe` forbidden, clippy's `all`
  denied and `pedantic` warned): every exported item needs a `///` comment, and `just check`
  fails on one that lacks it. The lint is a warning, as `pedantic` is, so the gate's
  `-D warnings` in clippy and in rustdoc makes it an error while a local build of a draft
  still runs. A test fails if the root `Cargo.toml` drops the lint or a crate drops
  `[lints] workspace = true`.
- The write-ordering rules that `SWARMOPS_TEST_ORDER_CHECK=1` checks are W1–W7, stated in
  storage spec §2 rule 4, and a violation panics with `order check: W<n> violated by …`. They
  were R1–R7, defined only in `crates/swarmops-store/src/order_check.rs`, under the names the
  spec's review invariants (§6) also use.
- A raw object (one stored uncompressed: under 128 bytes, or content zstd cannot shrink, such
  as media or archives) is read back in the buffer its file was read into, the 16-byte header
  drained off the front, instead of being copied out of it: reading a large one peaks at about
  its size, not twice that. It is still one read of the file.
- A read gives up with the retryable `E_BUSY` when a commit lands during every one of its
  snapshot's 32 rounds (the store's `SnapshotContended`), where it used to read the refs and the
  frames again for as long as commits kept landing. `E_BUSY`'s description, which the schema
  carries, says so: another writer held the lock for the whole wait, or commits landed during
  every attempt to take a snapshot; either way nothing was committed (a commit's flushed objects
  may remain, for GC). It used to say only the first.
- The store's limit refusals (more than 255 parents, an actor over 255 bytes, more than 65,535
  records in one transaction) and malformed id prefixes are `E_INVALID_ARG`, with
  `details.store_limit`, `max` and `got`, or `details.prefix`; they were `E_IO`. A call the
  store's API does not take, which only a bug makes, is `E_INTERNAL`. No CLI, `StoreOps` or MCP
  request reaches these in M1 (storage spec §3, operations spec §0): the one request field that
  reaches a store limit is `init`'s name (its 255-byte limit is below). In the Rust API,
  `StoreError` gains `Limit` (with `LimitKind`), `BadPrefix` and `BadCall`, its `Io` keeps the
  failures the filesystem reported, and `Record::encode` returns a `Result` instead of
  panicking on a record past the layout's lengths.
- `StoreError::JournalCorrupt`'s `offset` is always a position in `journal.log`.
  `Record::decode`, which gave it the position in its input where decoding stopped, returns a
  `BadRecord { at }` with that position instead (Rust API only; the readers report a frame
  that does not decode at its own offset, as before).
- **Schema only, the wire unchanged:** `Envelope`'s schema says with a `oneOf` that exactly one
  of `result` (when `ok`) and `error` (when not) is present, and its `error` is no longer
  nullable; `RefSetReq`'s says with a `oneOf` that exactly one of `expected_old` and `create`
  is given. Through `swarmops mcp`, every tool's `inputSchema` stays a plain object, with no
  `oneOf`, `anyOf` or `allOf` at its top, which the Claude API refuses for the whole session,
  and the `expected_old` of `ref_set` and `ref_delete` there is a string of the full-id
  pattern `^[0-9a-fA-F]{64}$` that the server enforces, in place of the revision text that
  offered a ref name.
- The `expected_old` descriptions in `swarmops schema`, the `ref_set` and `ref_delete` tool
  descriptions and the MCP server's instructions say what an id prefix does, apart from a ref
  name and a relative revision: it pins the id read only until another changeset shares it,
  then it is `E_AMBIGUOUS_ID`. The instructions also say that a read may show a commit that a
  failed sync then takes back, and that a mutation naming it fails under the lock instead of
  building on it; they drop the revision grammar, which each tool's schema and a bad
  revision's hint give, and the parenthesis on Claude Code's result limits (1,990 of Claude
  Code's 2,048 characters; they were 2,043).
- Test infrastructure: one `NoSyncFs` in `swarmops-testkit`, the real filesystem without its
  flushes that fails where it fails (a sync of a missing path is `NotFound`), replaces the test
  kit's `FastFs` and the copies in the `read_snapshot` bench and the `journal_recovery` fuzz
  target, which had drifted apart (two of the three answered `Ok` to a sync of a missing path).
  `Watched(watcher, fs)` runs its watcher over any filesystem, and a watcher sees a call's
  method as an `FsMethod`. The test kit's CLI and repository helpers sit behind its default
  features `cli` and `repo`, so the fuzz crate takes it with `default-features = false`.
- The order checker's W1 also refuses a cut below what the journal vouches for: of
  `journal.log` into the frames `journal.off` lists, or of `journal.off` below an entry the
  process synced or found there (the zeroed entries past frame 0 at its end aside, which list no
  frame). Before, `truncate` was only followed, and a wrong cut showed only when a sweep's oracle
  found the frames lost.
- Test infrastructure: the crash sweeps of the journal, the index and the repository recover
  under the order checker too, as the sweeps through the binary did (they recovered on the real
  filesystem, unchecked), and so does the fsync-failure model's sweep of the journal, every run.
- `init` refuses a repository name of more than 255 bytes with `E_INVALID_ARG` (`details`
  `{"store_limit": "repo_name_bytes", "max": 255, "got": <bytes>}`) before it creates
  anything, and `open` refuses a `REPO` longer than an id and a 255-byte name
  (`E_NOT_A_REPO`): a store created with a longer name must be created again.
- `swarmops mcp`: the instructions and the `commit` tool's description say what `advance` does
  and to check `result.advanced`. To make room, the instructions' sentence on result sizes drops
  that each result holds its envelope twice, which the envelope's sentence says, and its note on
  binary files, which `read_file`'s description gives: 2,034 of Claude Code's 2,048 characters
  (they were 1,990).
- `E_IO`, `E_IO_TRANSIENT` and the `E_STORE_CORRUPT` of a metadata file that does not parse
  name the file operation that failed and the store file it was on: `details.fs_op` (one of 22
  names, such as `"open"` or `"append_durable"`) and `details.store_file` (relative to the
  store's root, such as `meta/journal.off`; absent for a path outside it). Their messages say
  the same before the operating system's text, none of them carries the store's host path but
  two `E_IO`s that operations spec §0 names (a symlink loop met while working out where that
  path leads, and tempfile's giving up after 65,536 taken temp-file names), and the schema's
  descriptions of the three codes say so. A missing index file's `E_STORE_CORRUPT`
  names it (`meta/idx/ids.col is missing`). In the Rust API, `StoreError::Io` and
  `NotCommitted` carry an `IoFailure` (`op`, `file`, `source`, `also`) in place of an
  `io::Error`, the new `StoreFs` locates the failure of every filesystem call the stores make,
  and neither `StoreError` nor `RepoError` converts from a bare `io::Error` any more.
- A write whose tail check cannot list a frame that an earlier transaction left whole and
  unlisted (that transaction's own listing in `journal.off` failed: committed, with a warning)
  is the retryable `E_IO_TRANSIENT`, `committed: false`, its `details` naming the append that
  failed, where it was the non-retryable `E_IO`: nothing of the write was committed, and running
  it again lists the frame. In the Rust API it is `StoreError::NotCommitted`, where it was
  `StoreError::Io`.
- `swarmops mcp`: refusing a `ref_set` or `ref_delete` whose `expected_old`, or a `commit` with
  `advance` whose `parent`, is no full changeset id, the server says what an id prefix does
  apart from a ref name and a relative revision (it stops resolving once another changeset
  shares it), as the tool descriptions do; and arguments that do not fit a tool's request type
  get a hint that the tool's `inputSchema` gives them, where `swarmops schema` gives the
  request type without the rules the server adds. In the schema, `VerifyFinding.object` says
  that `missing_object` also names an object the store holds as another kind.
- A journal append that the filesystem refuses for want of permission (`EACCES`, `EPERM`) or
  because it is read-only (`EROFS`), and that the store cut back, is `E_IO` (exit 1, not
  retryable) with `details.committed: false` and a hint that says what to fix first, where it
  was the retryable `E_IO_TRANSIENT`: nothing was committed, but running the same command again
  meets the same refusal until its cause is fixed. So it is for the write's own frame, for the
  tail check's listing of a frame left unlisted and for its rewrite of frames whose sync failed
  (storage spec §2 rule 3 (a)-(c)); any other failure there stays `E_IO_TRANSIENT`. The
  schema's description of `E_IO` says so.

#### M2: diff, history and derived data (2026-10-20)

- A lookup by id no longer reads the key it found a second time: the search keeps the key it
  read where it stopped, so the lookup reads one byte range fewer. Lookups by prefix and by
  delta hash (`resolve_prefix`, `by_delta_hash`) read only each result's id, and an
  insert only each parent's generation, where both read the whole index record.
- A lookup by id, prefix or delta hash searches a sorted view by interpolation on the keys'
  first 8 bytes, which BLAKE3 spreads evenly, instead of bisecting it: 5.6 key reads per
  search over 100,000 ids, where a bisection reads 16.7. It bisects where interpolation does
  not help (keys that share their first 8 bytes, or two probes in a row that each left more
  than half of the range), never reads more than 2(⌊log2 m⌋ + 1) keys, and finds the same
  bound a bisection does.
- Each request reads in one snapshot of the store's metadata (storage spec §8, operations spec
  §4): every revision and ref it names resolves in the same view, and a `commit` checks its
  parent in the view it resolved it in. `cat`, `ls`, `show`, `log`, a commit on a parent and
  `ref set` took two each (`cat` and `ls` three when the path holds nothing or the other kind),
  each snapshot reading every ref file. In the Rust API, `Repo::snapshot` returns a `RepoRead`,
  whose reads (`resolve`, `changeset`, `read_blob_at`, `list_tree`, `entry_at`, `tree`, `blob`,
  `log_first_parent`, `get_ref`, `list_refs`, and `info` on a local store) are those of one
  snapshot, and `Repo::commit_in` commits on parents resolved in one; `Repo`'s own read methods
  each take a snapshot of their own, as before.
- A commit's contents together are held to 1 GiB (`MAX_COMMIT_CONTENT_BYTES`), and a request's
  text to 64 MiB (`MAX_REQUEST_BYTES`: a `--changes` file or stdin, or one line of
  `swarmops mcp`'s input), each checked before anything is read or written: the total by the
  sizes the contents declare (a client file's length, unread; a blob named by id counts
  nothing), as the change list's shape is checked, and a file that grows while it is read is
  read no further than one byte past what the limits leave it. `E_TOO_LARGE` says which limit in
  `details.scope`: `put` (one content over 512 MiB, as before), `commit` (its `size` is the
  contents' together, and its hint stores them with `blob put` to name them by id) or
  `request` (the CLI's, before parsing); an MCP line over the limit is a JSON-RPC `-32600`
  without an `id`, its rest passed over, and the server serves on.
- `read_tree`'s default page holds 250 entries, where it held 500: entries carry a `size` now,
  and a `path` past depth 1, and 500 of them print past the 64 KiB target for a default response
  even at depth 1 (70,780 bytes, with 30-byte names), where 250 at depth 3 print in 60,971 (a
  test of `swarmops-ops` measures it). A `read_tree` cursor that is no repository path is
  `E_INVALID_ARG`, where any string was taken as a name to resume after.

#### M3: git import (2026-10-20)

- The repository library refuses a metadata key that ends in `:b64` for every writer, where only
  the operations' `commit` did: `Repo::commit` checks it before it writes anything
  (`RepoError::ReservedMetadataKey`, which the operations report as before, `E_INVALID_ARG` with
  `details.key`), and `swarmops_repo::check_metadata` is the rule a writer below the API, such as
  the git import, runs, so that `show`'s `k:b64` is always the base64 of `k`'s value. For the
  import, `Repo` also gains `put_objects`, `contains_object`, `changeset_meta` (a changeset's
  index record, its object unread) and `flush_metadata` (the index synced and the checkpoint
  moved to the end of the journal).
- An object holds at most 512 MiB (`swarmops_store::MAX_OBJECT_LEN`), the limit a put's content
  already had. The store refuses a longer object before it writes anything, and a batch
  with one such object in it writes none of its objects: `E_TOO_LARGE`, with `details`
  `{"store_limit": "object_bytes", "max": 536870912, "got": <bytes>}` (in the Rust API,
  `StoreError::Limit` with the new `LimitKind::ObjectBytes`). A stored object whose header
  records more is refused as `E_STORE_CORRUPT` before anything is allocated for it (the store's
  `BadHeader`, where a zstd frame that claimed as much was `Corrupt`), so a compression bomb in a
  crafted store costs a read at most 512 MiB of payload, plus the decoder window of up to 128 MiB,
  and about 1.1 GiB of address space (storage spec §2 rule 1, §10 item 6).

#### M8: git export (2026-10-20)

- `git import` continues a branch SwarmOps exported: a target whose head is a native changeset goes
  on from the commit that changeset exports to (`ExportOid`), so commits made in git on top of an
  export, a merge on GitHub or a Dependabot update, come in on it, linearly, where they failed with
  `E_GIT_IMPORT_DIVERGED`; a head imported with a loss, or above such a commit, goes on from its own
  commit or from its export, whichever the source's commit descends from. A native head that was
  never exported, or that the source does not build on, is still `E_GIT_IMPORT_DIVERGED`, whose
  `details.base` now names the commit the head exports to, and whose message says the head stands
  for that commit. The import derives that export oid only where the head is native or the source
  does not lead from the commit it holds (storage spec §7, operations spec §6).

#### Release preparation (2026-10-20)

- `just fuzz` writes the new inputs a run finds into `fuzz/corpus-new/<target>/`, which git
  ignores, and reads the tracked corpus (`fuzz/corpus/<target>/`) as a seed that it never writes
  to, where it wrote them into the tracked corpus and left each run's checkout with hundreds of
  untracked inputs to commit or drop. New inputs join the tracked corpus minimised by
  `cargo fuzz cmin`, as [CONTRIBUTING.md](CONTRIBUTING.md) describes.
- The example agent names in the hint of `E_IDENTITY_REQUIRED` (`--as agent/worker-1`), in the
  hint for an invalid agent name and in the description of `Identity` in `swarmops schema`
  (`agent/worker-17`) belong to a generic family, `agent`; the name grammar and the error codes
  are as before.

### Fixed

#### M1.6: the code-review follow-ups (2026-10-20)

- A reader that measured `journal.off` before a writer cut an entry off it (a listing whose sync
  failed, or the zeroed entries a crash left at its end) reads it again instead of reporting the
  journal corrupt: snapshots, paging, `reflog`, `journal_after` and `verify` no longer give a
  transient `E_STORE_CORRUPT` or report a `JournalCorrupt` problem that a second run does not.
- A snapshot or `verify` whose index counts its records by their lengths (the journal cannot
  vouch for them) no longer fails with an I/O error when a writer cuts the index as it opens:
  it counts the records the cut keeps, and fails only if the index is cut again meanwhile.
- A sorted-view entry that points at a record its view does not cover is `E_STORE_CORRUPT`
  (`IndexCorrupt`) on every lookup that reads it. Before, a lookup by id, prefix or delta hash
  answered with another changeset's record where the entry pointed into the unsorted tail, and
  failed with `E_IO` past the index's end.
- No failure of the store's filesystem calls carries the store's absolute path any more, but
  tempfile's giving up after 65,536 taken temp-file names, which is not met in practice
  ([docs/known-limitations.md](docs/known-limitations.md), "One temp-file error names a host
  path"); the errors that answer for the path a command was given still name it, most of them
  made absolute (`E_NOT_A_REPO`, `E_UNSUPPORTED_FS`, a symlink loop met while working out where
  it leads; operations spec §0). A malformed
  `journal.ckpt` or ref file, and a ref file over its size cap, are named by their place in the
  store (`meta/journal.ckpt`, `meta/refs/<name>`), and a store file that is not a regular file
  by its name. A temp file that the store cannot create or write (an object write, or the
  replacement of a ref file or of `journal.ckpt`) fails with the operating system's own error,
  errno included, where tempfile's wrapper named the temp file's absolute path and dropped the
  errno.
- `verify` reports an index record that no journal frame holds, past the journal's last
  changeset, as an `IndexMismatch` (cause `Journal`) while changeset frames follow the
  checkpoint too. The index does not count such a record there, so verify reported nothing, and
  the next write cut it without a word.
- `swarmops mcp` refuses a `commit` whose `{"file": path}` content is not a regular file (a
  FIFO, a socket, a device or a directory, or a symlink to one) with `E_INVALID_ARG` before it
  reads anything: a FIFO no longer holds up the session until something writes to it, and a
  device such as `/dev/zero` is no longer read up to the 512 MiB limit. The CLI still reads such
  files, a pipe included.
- A `ref set` whose new target is the one it expects (`--expect` and the revision name the same
  changeset) is decided under the store's metadata lock, after the tail check, as every other
  `ref set` is: it answers `already_applied` only from the journal as the next writer finds it, no
  longer from a snapshot that can show another writer's frame before that frame's sync, which a
  failed sync or a power loss could then take back. It can now wait for the lock (`E_BUSY` after
  10 s) and repair what a crashed writer left; it still writes no journal frame.
- When the tail check's rewrite of the frames `journal.off` does not list yet fails, its error
  names those frames by seq; a tail check that rewrote them no longer cuts and syncs
  `journal.log` again to the length the rewrite left; and a rewrite that does not land where the
  frames were (another writer's bytes in between) is cut back and refused as damage
  (`JournalCorrupt` at the frames' offset, `E_STORE_CORRUPT`) before anything is listed, where it
  used to list entries that pointed into other bytes.
- A `FORMAT` or `REPO` that is not a regular file, such as a directory, is refused by its read
  as not a regular file (`E_STORE_CORRUPT`, `read REPO: REPO is not a regular file`, with
  `details.fs_op` and `details.store_file`), no longer as one too long by the size a directory
  reports (`E_NOT_A_REPO`, `<store>/REPO is 4096 bytes long, …`).

#### M2: diff, history and derived data (2026-10-20)

- `blobs_written` in `commit`'s result (and `blobs_written`, `trees_written` of
  `swarmops_repo::CommitOutcome`) counts exactly what the commit's own writes placed in the
  object store: of processes that commit one new file at once, one counts its blob, where each
  that found it missing used to (eight reported 8). A commit also hashes, encodes and looks up
  each of its objects once (it hashed each object twice, encoded its changeset twice and checked
  each blob and tree for existence twice), and makes `objects/tmp` once, not once per object:
  `swarmops_core::Hashed`, which only hashing makes, carries an object with its id to the new
  `ObjectStore::put_hashed_batch` (whose `PutOutcome.written` says whether the call placed the
  file), and `Changeset::encode_hashed` gives the changeset's object and its delta hash from one
  encoding.

### Security

#### M1.6: the code-review follow-ups (2026-10-20)

- An object file whose zstd frame records a huge decompressed size, in agreement with its header
  (17 bytes of frame are enough), no longer makes a read panic (`E_INTERNAL`) or abort the process
  while it reserves that size: before the frame's output arrives, a read reserves at most 64 MiB
  for it, beside the zstd decoder's window, which the frame's header sets up to zstd's default
  limit of 128 MiB (this store's frames use at most 2 MiB): about 192 MiB at worst. Such an
  object is `E_STORE_CORRUPT`, which `verify --deep` lists. An object whose frame
  really decompresses to a huge size is still decoded whole
  ([docs/known-limitations.md](docs/known-limitations.md), "A zstd bomb object is decoded
  whole").
- `open` measures `REPO` before it reads it: a crafted `REPO` of huge length is refused by its
  length (`E_NOT_A_REPO`), where it used to be read whole, at the cost of its length in memory
  (M1.5 final review, security-2).

#### The security review's fixes (2026-10-20)

- A damaged or crafted changeset index could make reads and writes act on a changeset they were not
  asked about, all with exit 0: a sorted view's entry (`meta/sorted/*`) that named another
  changeset's record made `show` of an id, and `show`, `cat`, `ls` and `log` of a prefix, answer
  with that changeset, and `ref set` of a prefix move the ref to it; a parents entry out of place
  (`meta/idx/par.col`, `meta/parents.log`) made `log` and `~N` skip committed changesets or read
  another changeset's files, and a cycle in them made a walk go on for ever. A lookup now checks
  that the record a view names holds the key it was asked for, and that a record's parents start
  where the record before it leaves off, and is `E_STORE_CORRUPT` where either does not, a write's
  lookup included (nothing is recorded); and every first-parent walk (`log`, `~N`, `log --path`,
  `history`, a land's merge base and squash, the git export's walk) takes each parent from the
  changeset's object, which its hash binds to its id, at the cost of one object read per step.
  `verify` reports the damage, as before. Generations below the checkpoint are still taken as the
  index gives them ([docs/known-limitations.md](docs/known-limitations.md), "A crafted index's
  generations steer history queries").
- A derived value file (`meta/derived/*`) that someone placed, whole and with its checksum right but
  holding a value its derivation does not give, was read as valid, and `verify --deep` did not look
  at derived values: a skip list could make `~N` past 1,024 steps, and the ancestry checks a land's
  merge base rests on, follow a changeset off the first-parent chain (so `cat` at such a revision
  served files that `main`'s history never held), and a last touch could make `history` and
  `log --path` name a changeset that never touched the path. Now `verify --deep` checks every value
  of the built-in derivations (`first_parent_skip_list`, `last_modified`) against the objects and
  reports each one that disagrees (`derived_mismatch`, with the new `derivation` field), and the
  readers check each value they use where they can without the walk it saves (a skip list's first
  entry, its length and each jump; a last touch's changeset, its generation and its delta), refusing
  one that fails with `E_STORE_CORRUPT` (`details.derivation`, `details.changeset`; hint
  `swarmops verify --deep`). Removing `meta/derived/` has every value derived again. Values crafted
  to agree with each other still pass the readers
  ([docs/known-limitations.md](docs/known-limitations.md), "Crafted derived values that agree with
  each other are read as valid").
- A small derived value file whose zstd frame records a value of any length made a read decompress
  and allocate that much, without bound, before the file's checksum was checked. A recorded value
  longer than 512 MiB (`MAX_OBJECT_LEN`, the most an object holds) now fails the file's checks
  before anything is decoded or allocated for it: the file reads as missing and is removed, and the
  value is derived again; `DerivedStore::put` refuses such a value. Within that bound a value's
  frame is still decoded whole before its checksum is checked
  ([docs/known-limitations.md](docs/known-limitations.md), "A zstd bomb object is decoded whole").
- An object file is measured before it is read: one longer than any object's file (its 16-byte
  header and 512 MiB) is refused by its length, unread, as `E_STORE_CORRUPT` (`details.object`),
  which `verify --deep` lists as a `corrupt_object` and goes on. Such a file, sparse or crafted,
  used to be read whole before its header was checked, so `cat`, `verify --deep`, and any commit,
  walk or read that reached it took its whole length in memory, enough for the system to kill the
  process, `swarmops mcp` with it. A file within the bound is still read whole
  ([docs/known-limitations.md](docs/known-limitations.md), "`cat` reads the whole object file").
- zstd is built without its default features, so `swarmops` and `so` no longer hold its decoders for
  the legacy formats (v0.1 to v0.7, about 25,000 lines of C that no store needs), which every object
  read could reach: an object file holding a legacy frame was decoded by them, and one whose legacy
  frame held the object's own payload passed as valid, `verify --deep` included. An object in a
  legacy frame, alone or after a current one, is now `E_STORE_CORRUPT`, which `verify --deep` lists;
  what the store writes is unchanged.
- A `journal.off` that ends in a long run of zeroed entries no longer holds up every read and write:
  each scanned such a run one 8-byte read at a time, so a long enough one kept every command, and
  the MCP server, busy for hours with almost no memory. Readers and recovery drop at most 512 zeroed
  entries at its end, a 4 KiB block (a power loss leaves zeroed only the entry being written), found
  with one read, and refuse a longer run as damage: `E_STORE_CORRUPT`, the journal corrupt at offset
  0, with nothing changed on disk, which `verify` reports. The first write used to cut such a file
  back once its scan ended; now it is cut back by hand, to its last entry that is not zero
  ([docs/known-limitations.md](docs/known-limitations.md), "A `journal.off` that ends in more than
  512 zeroed entries is refused, not repaired").
- A `meta/lock` that is not a regular file is refused before it is opened (`E_STORE_CORRUPT`,
  `details.store_file` `meta/lock`; a directory there was `E_IO`), its open follows no symlink and
  waits for no reader (`O_NOFOLLOW`, `O_NONBLOCK`, then a check of what it opened), and `verify`
  reports it (`meta_corrupt`); removing it repairs the store, since the next write makes a new one.
  Before, a FIFO there made every write (a `commit`, a `ref set`, the MCP server's first write) wait
  in its open for ever, before the 10 s lock timeout starts, and a symlink there was followed, while
  `verify` saw nothing wrong. The two flags come from `libc`, which the build already held and which
  is now a direct dependency of `swarmops-store`, for its constants only.
- `swarmops mcp` no longer parses a message line into a generic JSON value before it checks
  anything, which could take about a hundred times the line's length, up to the 64 MiB a line may
  hold: one line could exhaust the memory of the server that a session and its subagents share, and
  a batch or a message before `initialize` was refused only after that parse. The server checks that
  a line is JSON without building anything, reads only the members it serves, and reads a tool's
  arguments straight into their request type, so what else a line holds costs nothing beyond its own
  bytes; a tool argument that names a field of its request type twice is now `E_INVALID_ARG`, where
  the later one was taken. A request type's own fields still take up to about 15 times their text
  ([docs/known-limitations.md](docs/known-limitations.md), "A request takes the memory its type
  holds").
- Before M2, a commit's contents together had no limit, only each put's 512 MiB, so a small commit
  request could make the process, `swarmops mcp` included, allocate gigabytes and abort. M2's limit
  on them, 1 GiB together, checked by the sizes they declare before a byte of them is read
  (`E_TOO_LARGE`, `details.scope` `commit`; **Changed**, M2), is the fix, and a test now holds that
  the MCP server refuses such a commit before it reads any of the files it names, and serves on.
- A `land` or a `diff` that reached a text file changed at many places could hold the process,
  `swarmops mcp` included, for about a minute at 200,000 lines and some four times as long at each
  doubling: the histogram line diff's time can grow with the square of a file's lines, and nothing
  bounded what it was given but the 512 MiB an object holds. The line diff now runs only on sides of
  at most 49,152 lines and 8 MiB (`MAX_DIFF_LINES`, `MAX_DIFF_BYTES`), checked before any line is
  diffed: past that, a land whose two sides changed the file differently conflicts on it as
  `binary`, which `land --resolve` takes whole, and `diff --format stat` or `unified` gives the file
  no line counts and marks its hunks cut (`hunks_truncated`). Within the bound a file diffs and
  merges as before, which can still take seconds
  ([docs/known-limitations.md](docs/known-limitations.md), "A text file past the line diff's bound
  is not diffed or merged by lines").
- A `land` that conflicted at many places answered `E_CONFLICT` with a report of one entry per
  conflict hunk of every conflicting file, which `--max-conflict-bytes` did not bound (it bounds the
  hunks' texts): one text file conflicting at 100,000 places gave an envelope of 12.7 MB, which a
  `swarmops mcp` response carries twice. A merge now holds its first 128 conflict hunks, over all
  its files, as the three-way merge makes them, and past those each file's hunks as one hunk that
  spans them, so every place where the two sides' edits meet stays within a hunk; the report lists
  the conflicting files in path order while their entries, texts aside, fit in 24 KiB, the first
  file always (its hunks while they fit, then one that spans the rest) and a later one whole or not
  at all, and says `truncated` when it leaves any out. The error's message still counts every
  conflicting path, and the texts stay within `max_conflict_bytes` as before
  ([docs/known-limitations.md](docs/known-limitations.md), "A land's conflict report leaves files
  out and joins hunks past its bound").
- `git import` and `git export` no longer open a file of the git directory without looking at what
  it is: a FIFO where they read one (`config`, `HEAD`, a ref or `objects/info/alternates`, among
  others) held the command until it was killed, and a file there that never ends, or of huge length,
  was read whole until memory ran out. Before gix opens the repository, every file of the git
  directory that the open and the command's lookups read is looked at, a symlink followed: a FIFO, a
  socket or a device is `E_GIT_UNSUPPORTED` (`details.reason` `special_file`, `details.file` its
  path in the git directory), and a ref file, `config`, `config.worktree` or
  `objects/info/alternates` longer than 16 MiB, which gix reads whole, is refused unread
  (`file_too_long`, with `size` and `max`; [docs/known-limitations.md](docs/known-limitations.md),
  "A git directory's file longer than 16 MiB is refused").
- `cargo deny` (advisories, licenses, bans, sources) checks every lockfile the project builds, where
  it builds it. It checked only the workspace's, while every `just check` also builds the fuzz
  crate's and the weekly scheduled run builds and tests `bench/compare`'s, and under the workspace's
  policy both failed. `just check` and `scripts/check.sh` check the fuzz lockfile with the
  workspace's policy and one license exception, NCSA for `libfuzzer-sys`
  (`fuzz/deny.exceptions.toml`); `just bench-compare` and the weekly run check `bench/compare`'s
  with its own policy (`bench/compare/deny.toml`: every line of the workspace's, two crate-scoped
  license exceptions, and two advisories ignored with their reasons, RUSTSEC-2025-0057 for `fxhash`
  and RUSTSEC-2024-0384 for `instant`, unmaintained crates without a fixed release that only `sled`,
  a comparison engine, brings in, and that no shipped binary holds). A test of `swarmops-core` fails
  the gate when a recipe, a script or a workflow job builds a lockfile that no `cargo deny` checks
  there.
- `git import` no longer reads a commit, a tree or an annotated tag of the source whole before it
  knows how long it is: a crafted repository's object, a small loose file whose header claims a
  huge length, made the import allocate that length, without bound, while it read it (over 500 MB
  for a file of about a hundred bytes). The import now reads each commit, tree and annotated tag
  it parses, as it already read each blob, only once the object's header says it is at most
  512 MiB long (`MAX_OBJECT_LEN`, the most an object holds), and refuses a longer one before a
  byte of it is read: `E_TOO_LARGE`, exit 1, with `details.object`, `details.kind` (the kind its
  header gives), `details.size` and `details.max`; the import stops there, as at any commit it
  refuses, before the frame that would hold the commit. Within the bound an object is still read
  whole ([docs/known-limitations.md](docs/known-limitations.md), "A git object is read whole, up
  to 512 MiB").
- An external id entry's file (`meta/external_ids/`, 44 bytes) is read no further than a byte
  past an entry, so a longer one is a damaged entry, however long: `verify` lists it as an
  `external_id_corrupt` finding (`E_STORE_CORRUPT`) and goes on, the git import and export pass
  over it with a warning, as over any damaged entry, and a put names its id as a conflict and
  leaves the file as it is. Such a file, sparse or crafted, used to be read whole, so `verify`,
  and any import, export or put that reached it, took its whole length in memory, enough for
  the system to kill the process (`swarmops mcp` with it), or failed with `E_IO` where the
  allocation was refused.
- A `git export` trusted the git oids it had memoized in the derived store
  (`meta/derived/git_blob_oid`, `git_tree_oid`, `git_oid`) as the truth about an object's content:
  a memo placed there whole and with its checksum right, naming an object the destination already
  held, could make the export write that object's bytes in place of the store's — git history a
  CI or deploy pipeline consumes, with exit 0 and a clean `verify --deep` — or move the branch to
  an arbitrary commit the destination held, neither rendered nor mapped by the export. The export
  now derives every tree's, blob's and commit's git oid from the store's own bytes and binds what
  it reuses, writes or moves the branch to to them (a native commit's oid from rendering over oids
  derived that way, an imported commit's from the external-id mapping, which stays authoritative);
  a memo that is not the oid the objects give is `E_STORE_CORRUPT` (`details.derivation`, hint
  `swarmops verify --deep`), refused before any object is reused or the branch moves, never a
  silent reuse. `verify --deep` now re-derives `git_tree_oid`, `git_blob_oid` and `git_oid` against
  the objects too and reports each mismatch (`derived_mismatch`), as it does the other derived
  values (`git_oid` names its changeset, the other two their object). The memo stays a hint that
  saves a write, not a statement of content ([docs/known-limitations.md](docs/known-limitations.md),
  "Crafted derived values that agree with each other are read as valid").
- A `git export` still took the parents of the commits it wrote, where those parents were commits
  it did not write, from the export oids memoized in the derived store (`meta/derived/git_oid`),
  and so did its search for the commit the branch's history goes on from: a memo placed there
  whole and with its checksum right, for an ancestor of those commits (a merge's side branch,
  say), could give them as their parent any commit the destination held, or make a branch that
  had diverged pass for the export of an ancestor, so the branch moved to a history the store does
  not hold, with exit 0 and the files at its tip still the store's. The export now derives the
  export oid of every changeset below the head from the store's own objects, each once a run,
  parents first and without recursion, and only checks the memos against those: one that
  disagrees is `E_STORE_CORRUPT` (`details.derivation` `git_oid`, `details.changeset`; hint
  `swarmops verify --deep`, which names it), refused before anything is written or the branch
  moves, a memo of the head that names the branch's commit included, which no longer passes for
  up to date. So every export, an incremental one included, reads the whole history below its
  head ([docs/known-limitations.md](docs/known-limitations.md), "An export reads the whole history
  below its head").
- A `git import` onto a target whose head SwarmOps made, or imported with a loss, took the commit
  that head exports to from the export oids memoized in the derived store (`meta/derived/git_oid`),
  and took the trees and blobs of that commit for the head's by their names alone: a memo placed
  there whole and with its checksum right could make the import go on from any commit of the
  source as the head's export, and record the commits made on it as faithful changesets (their
  `git.oid` set, no `git.lossy`) holding the head's files where git's differ, with exit 0. The
  import now derives that export oid from the store's own objects, with the whole history below
  the head, as the export does, and checks each memo it meets against what it derives: one that
  disagrees is `E_STORE_CORRUPT` (`details.derivation` `git_oid`, `details.changeset`; hint
  `swarmops verify --deep`, which names it), refused before anything is written. It takes a tree
  or a blob of the head for a git object only where the store's own bytes render to that object's
  oid, so no commit it records holds other bytes than git's, whatever the store says of the head;
  and in the library (`swarmops_git::import`) it no longer needs `ExportOid` registered. So an
  import onto a target reads the head's whole tree, and onto a native head the whole history below
  it ([docs/known-limitations.md](docs/known-limitations.md), "An import onto a target reads the
  head's whole tree, and onto a native head the whole history below it").
- A `git import` took an external id entry (`meta/external_ids/git/`) as the truth about which
  changeset a commit is, once the store held the changeset it named: an entry placed there whole
  and with its checksum right could make the import take a commit of the source for the target's
  head, or for any other changeset, and record the commits made on it on that changeset, a history
  git does not have, and move the target, with exit 0, no warning and nothing for `verify` to
  report. The import now takes an entry only where its changeset stands for the commit: its
  `git.oid` is that commit, or its export oid, derived from the store's own objects as the export
  derives it, is. Any other entry is passed over with a warning, as a damaged one is, and its
  commit converted again; an import whose base check then fails is `E_GIT_IMPORT_DIVERGED` and
  names the first such entry (`details.external_id`, with `commit` and `changeset`; hint
  `swarmops verify --deep`), with nothing written. `verify --deep` now also checks the entry of
  each changeset made in the store against the commit that changeset exports to, and reports one
  that names another (`external_id_mismatch`). An import whose walk meets an entry an export wrote
  derives that export oid with the history below it
  ([docs/known-limitations.md](docs/known-limitations.md), "An import onto a target reads the
  head's whole tree, and onto a native head the whole history below it").

[Unreleased]: https://github.com/KeiaiLab/SwarmOps/compare/v0.7.0...stable
[0.7.0]: https://github.com/KeiaiLab/SwarmOps/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/KeiaiLab/SwarmOps/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/KeiaiLab/SwarmOps/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/KeiaiLab/SwarmOps/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/KeiaiLab/SwarmOps/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/KeiaiLab/SwarmOps/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.1.0
