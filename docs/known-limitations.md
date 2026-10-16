# Known limitations

What SwarmOps does not do yet, or does less well than it should, as of M1, and the milestone
meant to change it. The items come from M1's reviews: the carry-overs its tasks handed to later
milestones, the deferred minors that are still open, and the rulings of the final review, which
left 14 questions to the maintainer (the table at the end lists all 14). Each entry says where
it was found; those pointers (an M1 task, a review, the 2026-10-16 comparison) refer to the
maintainer's local records, which are not in the repository. Code-level minors that change
nothing a user or an agent can see (naming, comments, micro-optimizations, test labels) are not
listed.

Targets are milestones of the umbrella spec (§13): M2 diff, history and derived data · M3 git
import · M4 merge and landing · M8 git export · M5 workspaces · M6 verify, crash safety and
fuzzing · M7 blame and review · M9 simulation and benchmarks · M10 RPC server. M1.5, the
hardening milestone after M1 ([its plan](superpowers/plans/2026-10-16-m1-5-hardening.md)), closes
a few. "Unscheduled" means no milestone has taken the item yet; "documented" means it is the
intended behaviour.

When a change closes an item, it removes the entry here in the same commit (or says what is
left).

## Durability

- **The crash model is a process crash.** The sweeps abort the process at a chosen filesystem
  call, which leaves the kernel's page cache intact; a power loss, which can lose, tear or
  reorder unsynced writes, is not modelled yet. *Target: M1.5, Task 3 (power-loss images).*
  `abort()` also stands in for `SIGKILL`: a test with a literal `SIGKILL` is *M6*. *Found: M1
  Task 6; final review, question 11.*
- **A double failure leaves an index risk open.** When an index sync fails, the writer cuts the
  unsynced records back so the journal can supply them again; if the cut fails too, the error
  names both failures, and the risk stays open until the next successful sync, which could carry
  the checkpoint past records the failed writeback lost. *Target: M6 (fsck). Found: M1 Task 6.*
- **A reader can get `E_IO` after such a cut.** A snapshot that counted the records a writer
  then cut fails with an I/O error until the reader takes a new one. It never returns wrong data.
  *Documented (storage spec §3). Found: M1 Task 6.*
- **Full fsync is slow on this host.** An uncontended durable commit takes about 21 ms through
  the binary, at about 4 ms per fsync; the umbrella spec's commit target (5 ms plus 0.3 ms per
  file) leaves syncs out. Batching object and journal syncs, and whether any
  `--durability fast` flag is justified, are re-evaluated with new measurements. *Target: M2.
  Found: M1 Task 7b; final review, question 4.*

## Concurrency and the lock

- **The lock is bounded, not first-in, first-out.** Waiters queue in the kernel, but a woken
  waiter races the others and the writer that just let go. With 32 writers committing in a loop,
  a commit can wait a few hundred transactions; where a transaction takes more than about 28 ms
  (a slow disk), that can exceed the 10 s `lock_timeout` and end in `E_BUSY` (retryable). The
  quiet-window measurement saw no `E_BUSY` and a slowest commit of 918 ms (1.2 s at store
  level). A ticket or FIFO hand-off on top of `flock` is the next step. *Target: M6. Found: M1
  Task 7c; final review, question 3.*
- **Recovery after a crash walks all of `journal.off` under the lock.** Before it cuts or
  lists anything, the first writer after a crash checks every entry of `journal.off`. On a long
  journal that walk could approach `lock_timeout` for the writers queued behind it. *Target: M6
  (fsck, and the lock timeout against journal size). Found: M1 fuzzing review; final review,
  question 5.*
- **An abandoned lock wait parks a thread.** A wait that times out leaves its helper thread in
  the kernel's queue until the holder lets go, and a hung holder leaves one parked thread per
  abandoned wait. That costs nothing in a short-lived CLI process; a long-lived one (the stdio
  MCP server is one, a daemon would be another) needs a way out. *Target: M10. Found: M1
  Task 7c; final review, question 10.*
- **Concurrent `init` on one directory is unsupported.** The loser may overwrite the winner's
  `REPO` file or fail; two concurrent creations of a metadata store on one root are unsupported
  too. A no-clobber `REPO` link is the cheap hardening. *Target: M5 (with the workspace
  registry). Found: M1 Task 2c; final review, question 1.*
- **A reader's snapshot loop has no cap.** A reader re-reads the refs and frames until the
  frame count holds still, without a limit or back-off; under sustained commits with many frames
  past the checkpoint it could keep retrying. *Unscheduled. Found: M1 Task 2c.*
- **One transient error after a crash.** A reader that races the first repair after a crash can
  see `E_STORE_CORRUPT` once; reading again succeeds. *Unscheduled (document or retry once).
  Found: M1 final review.*
- **Readers may return state that is not durable yet.** A reader sees a frame before its
  `fdatasync` completes, so until M10 a power loss, or a failed sync of that frame (which the
  writer then cuts back), can take back state a reader returned (operations spec §4 says what
  agents may rely on). Strict read-committed reads, limited to
  frames already listed in `journal.off`, come if remote readers need them. *Target: M10.
  Found: M1 Task 2c; final review, question 2.*

## Reads and response sizes

- **`cat` reads the whole blob.** `read_file` loads all of it even when `max_bytes` asks for
  less; ranged reads (`range`, `--lines`) come later. *Target: M2. Found: M1 Task 4b; final
  review, question 6.*
- **`ls` of a wide directory decodes its whole tree.** Paging limits what is returned, not what
  is read; size and depth (`ReadTreeReq.depth`, each entry's `size`) come later, and the default
  page sizes are measured again then, because `size` makes every entry longer. *Target: M2.
  Found: M1 Task 5; final review, question 9.*
- **Every read takes a full metadata snapshot**, and a snapshot reads every ref file: cheap with
  a few refs, linear in their number. A commit takes about four snapshots; passing one through
  is the fix once measurements ask for it. *Target: M2 (with the other commit-path costs below).
  Found: M1 Tasks 2c, 3 and 4b.*
- **A large frame past the checkpoint is parsed by every read.** After a bulk write, such as a
  git import, every `read()` parses the big frame again until the next checkpoint; the import
  should flush, or the store checkpoint once the frames past it grow large. *Target: M3. Found:
  M1 Task 2c.*

## Commits, refs and history

- **No merge.** An agent that loses a ref race and commits its change again on top of the
  winner's can overwrite the winner's edits to the same files: M1 has no three-way merge.
  *Target: M4 (merge and landing). Found: M1 Task 4b.*
- **A retried commit can record a second changeset.** A commit signs at its own time, so when
  a commit's outcome is unknown (its process died before it answered) and it is run again, a
  first attempt that did commit stays as an unreferenced changeset beside the second. An
  idempotency key (`commit --key`) closes this. *Target: M4. Found: M1 final fix pass.*
- **No second root commit.** Once `main` exists, an omitted parent means `main`'s head, so the
  API cannot make a root commit. *Target: M3 (`commit_raw`, which git import needs). Found: M1
  Task 4a; final review, question 12.*
- **`--expect` with a ref name cannot detect a move.** A ref name given as the expected target
  resolves when the command runs, so it matches whatever the ref holds by then (it fails only
  if the ref is gone); give an id for an expectation that can fail, or be replayed.
  *Documented (`--expect` help). Found: M1 Task 5; final review, question 13.*
- **No blob-id puts.** A change cannot name a blob already in the store by its id; its content
  is always sent again. *Target: M2 (`ContentInput::Blob`). Found: M1 Task 3.*

## Verification and repair

- **No repair tool.** `verify` reports damage (a corrupt listed frame, index records lost below
  the checkpoint, a malformed ref file, a missing index file), but M1 cannot fix any of it;
  restoring the store is the only remedy. The index can be rebuilt from the journal in
  principle (`fsck --rebuild-index`). *Target: M6 (fsck). Found: M1 Tasks 2b, 2c and 3; final
  review, question 5.*
- **Temp files of crashed writes stay.** `objects/tmp/` keeps what interrupted object writes left
  there. *Target: M6 (`gc`). Found: M1 final review, question 8.*
- **`verify` does not scale to very large stores.** It holds the journal and the index in memory,
  and checks each sorted-view entry with its own reads (about 4 million system calls at a
  million changesets). *Target: M6 (streaming verify). Found: M1 Task 3; final review.*
- **A missing `journal.off` opens as `E_IO`**, not `E_STORE_CORRUPT`; `verify` names it.
  *Unscheduled. Found: M1 final review.*
- **`verify --deep` skips object files that are symlinks**, because the object listing ignores
  them; SwarmOps never writes one. *Unscheduled. Found: M1 final review.*

## Hostile input

[SECURITY.md](../SECURITY.md) says which inputs are in scope.

- **An oversized zstd frame aborts a read.** An object file whose zstd frame records a huge
  decompressed size, equal to the header's length, makes `get` allocate that size up front and
  panic or abort, instead of failing with `BadHeader`. SwarmOps never writes such a file; a
  crafted or damaged store can hold one. The fix is a streaming decoder that stops one byte past
  the expected length. *Unscheduled in the ledger; proposed for M6, with a fuzz target for
  object files. Found: M1 Task 1.*
- **Crafted `journal.off` entries can cut a frame.** Entries crafted to agree with each other,
  around an embedded valid frame, can make recovery cut a whole committed frame; that is outside
  the crash model, and `verify`, which walks every listed frame, reports it. A checksum on each
  entry is a format change: the next `FORMAT` (planned as 2, M6), together with a frame header
  that carries the frame's sequence number and the previous frame's CRC. *Target: M6. Found: M1
  fuzzing; final review, question 7.*
- **A zeroed `journal.off` entry deep in the chain** (three or more links before the end) is
  outside the crash model: recovery checks the last two links, and `verify` finds the rest.
  *Target: M6 (the next `FORMAT`'s checksums). Found: M1 fuzzing review.*
- **Metadata keys written below the API.** `show` can lose an entry for a changeset that holds
  both a key `x` and `x:b64`; the API refuses the `:b64` suffix on commit, so only a writer below
  it can do this, which the git import will be. It must run the same check. *Target: M3. Found:
  M1 Task 4b.*

## Serving requests

- **`meta.resolved` is shared by a handle's requests.** `LocalOps` records the revisions it
  resolves on the handle, so concurrent requests on one handle can mix their entries. The CLI
  makes one request per process; the MCP server needs per-request resolution. *Target: M1.5,
  Task 5. Found: M1 Task 4b.*
- **`{"file": "<path>"}` content is read by whoever serves the request.** That is the caller
  for the CLI and the stdio MCP server; a network server must refuse such content, and a client
  must turn it into bytes before the request leaves. *Target: M10. Found: M1 Task 4b.*

## Git import and export

- **`created_at` cannot show a time before 1970.** A changeset keeps its signatures' times as
  signed seconds, as git does (storage spec §1, §7), but the API's `created_at` in `show` and
  `log`, unsigned milliseconds derived from the committer's time, reports 0 for any time before
  1970. Whether the git import keeps such commits as they are or refuses them unless asked to be
  lossy is M3's choice. *Target: M3. Found: M1 Task 4a.*
- **No batch insertion yet.** One transaction can hold up to 65,535 records, but the index has no
  batch append and `insert_changesets_batch` does not exist yet (storage spec §3.1). *Target: M3.
  Found: M1 Task 2b.*
- **No test reads a real repository yet.** M1 had none, so the final review had nothing to judge
  (question 14). The first come with the git import, behind `SWARMOPS_HOST_REPOS=1`. *Target:
  M3.*

## Platforms

- **The filesystem check is by path name only.** A store under `/mnt/` is refused, but NFS, SMB
  or FUSE mounts are not detected (and not supported). *Target: M6 (detection by `statfs`).
  Found: M1 Task 3; umbrella spec §4.*
- **btrfs and a resumed `init`.** Resuming an interrupted `init` relies on ext4's and XFS's
  ordered journal to make the crashed attempt's directories durable; on btrfs they might not be
  (recoverable by creating the store again). *Unscheduled. Found: M1 Task 3.*
- **How a store path resolves.** A `..` after an existing file is taken lexically, and a
  dangling symlink is followed (both documented on the resolver). *Unscheduled. Found: M1
  Task 3.*
- **No migration between pre-release formats.** A store made before a format change may be
  refused afterwards and need creating again; it happened once during M1. *Documented (the
  versioning policy, before 1.0).*

## Performance

- **Part of each lock pass is unexplained.** Under 32 writers a pass takes about 14 ms, about
  four raw `fdatasync`s for a transaction that makes two; about 6.5 ms of it is not accounted
  for. *Target: M2 (profiling, in a quiet window). Found: M1 Task 7b.*
- **Commit-path costs.** Each blob is hashed twice (for its id, then when stored); reading one
  field of a changeset's index record reads all five index files; every commit's
  materialization scans the index tail; and the reflog walks frames past the snapshot only to
  drop them. *Target: M2. Found: M1 Tasks 2c and 3; deferred again by Task 8.*
- **Small object-store costs.** A raw object read copies its payload instead of stripping the
  header in place, and a put that creates a fan-out directory checks for it under the store's
  mutex. *Unscheduled. Found: M1 Task 1.*
- **No benchmark gate.** The benches run by hand (`just bench`, `just perf`); failing on a 20 %
  regression (`bench/thresholds.toml`) comes later. *Target: M9. Found: docs/perf.md.*

## Tests and tooling

These do not change what SwarmOps does, only how well the tests can see it.

- **Shared test support and the fault-injection doubles.** Three crates share test support
  through `#[path]` includes, and `CrashAtFs` compiles into the store crate even without the
  `test-hooks` feature (its environment hooks are gated; a release build ignores them).
  *Target: M1.5, Task 6 (a `swarmops-testkit` crate). Found: M1 Task 8; the 2026-10-16
  comparison.*
- **The gate lints only the all-features build.** Clippy runs with `--all-features`; the build
  without features is compiled and tested only by the release `test_hooks` run. *Unscheduled.
  Found: M1 final fix pass.*
- **Coverage is not measured by the gate.** It was measured once, part-way through M1, for core
  and store only: 94.61 % of lines. *Unscheduled.*
- **Gaps the reviews found and left.** A `CrashAtFs` off-by-one would go unnoticed (the test
  crashes only at call 2); the object store's link-error cleanup and failed-fsync paths, the
  CLI's panic path and the journal's adoption path in the crash sweeps are untested; there are
  no tests yet for a relative symlink target or a `..` after a symlink in the store path, and
  `verify`'s "read only up to its damage" message is not pinned; the CLI tests assume a temporary
  directory without spaces; and one end-to-end test skips when run as root. *Unscheduled.
  Found: M1 Tasks 1, 2a, 3, 5 and 8, and the final fix pass.*
- **The specs are in Korean.** They are the authority; [architecture.md](architecture.md) is an
  English overview, not a translation. *Unscheduled.*

## The final review's 14 questions

The final review of M1 left 14 questions for the maintainer to rule on. All 14 were ruled, and
each ruling is tracked above:

| # | Question | Ruling | Entry |
| --- | --- | --- | --- |
| 1 | Concurrent `init` on one directory | unsupported, documented; revisit with workspaces | M5, "Concurrent `init`" |
| 2 | Readers return state from frames not yet synced | allowed by the spec until remote readers | M10, "Readers may return state" |
| 3 | The lock's wait tail | `lock_timeout` stays 10 s; a FIFO hand-off later | M6, "bounded, not first-in, first-out" |
| 4 | Commit latency against the spec's target | target restated without syncs; batching re-evaluated | M2, "Full fsync is slow" |
| 5 | The recovery walk under the lock | with fsck, and the lock timeout against journal size | M6, "Recovery after a crash walks all of `journal.off`", "No repair tool" |
| 6 | `read_file` reads the whole blob | ranged reads | M2, "`cat` reads the whole blob" |
| 7 | Crafted `journal.off` entries | a per-entry checksum in the next `FORMAT` | M6, "Crafted `journal.off` entries" |
| 8 | `objects/tmp` leftovers | `gc` | M6, "Temp files of crashed writes" |
| 9 | Wide directories in `ls` | size and depth | M2, "`ls` of a wide directory" |
| 10 | Parked helper threads | the daemon | M10, "An abandoned lock wait" |
| 11 | `abort()` standing in for `SIGKILL` | a literal `SIGKILL` test | M6, "The crash model" |
| 12 | Second root commits | `commit_raw` | M3, "No second root commit" |
| 13 | `--expect` with a ref name | documented | "`--expect` with a ref name" |
| 14 | Tests that read host repositories | none in M1: nothing to judge | M3, "No test reads a real repository" |
