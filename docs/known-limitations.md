# Known limitations

What SwarmOps does not do yet, or does less well than it should, as of M1.6, and the milestone
meant to change it. The items come from the M1 and M1.5 reviews and the 2026-10-16 code review:
the carry-overs their tasks handed to later milestones, the deferred minors that are still open,
and the rulings of M1's final review, which left 14 questions to the maintainer (the table at the
end lists all 14). Each entry says where it was found; those pointers (an M1 or M1.5 task, a
review, the 2026-10-16 comparison or code review) refer to the maintainer's local records, which
are not in the repository. Code-level minors that change nothing a user or an agent can see
(naming, comments, micro-optimizations, test labels) are not listed.

Targets are milestones of [the umbrella spec](superpowers/specs/2026-10-15-swarmops-design.md)
(§13): M1.6 the code-review follow-ups · M2 diff, history and derived data · M3 git import ·
M4 merge and landing · M8 git export · M5 workspaces · M6 verify, crash safety and fuzzing ·
M7 blame and review · M9 simulation and benchmarks · M11 remote sync, offline-first · M10 RPC
server. M1.5, the hardening milestone after M1
([its plan](superpowers/plans/2026-10-16-m1-5-hardening.md)), closed a few, and M1.6, the
code-review follow-ups after it
([its plan](superpowers/plans/2026-10-17-m1-6-review-followups.md)), closed more; the entry
still marked M1.6 waits for its quiet-window measurements. "Unscheduled" means no milestone has
taken the item yet; "documented" means it is the intended behaviour.

When a change closes an item, it removes the entry here in the same commit (or says what is
left).

## Durability

- **The crash model is a process crash and a modelled power loss.** The sweeps abort the process
  at a chosen filesystem call, which leaves the kernel's page cache intact. A power loss is
  modelled by the images of `crates/swarmops-cli/tests/power_loss.rs`: for one recorded run,
  what a disk may hold after the power fails at each of its steps (unsynced data dropped, cut at
  512 B or 4 KiB, zero-filled, or with its first sector lost; directory entries that no fsync
  covered reverted), each of which the binary must recover. What stays open: `abort()` stands
  in for `SIGKILL`, and the images come from a model of the disk, not a real device, and the
  recorded run is short (its journal ends at 2,016 B), so a cut at 4 KiB is in the model but
  never differs from dropping there; only the slow variant's bulk frame (1,025 changeset
  records, about 76 KB) reaches past a 4 KiB boundary. A test with a literal `SIGKILL` on a real
  device is *M6*. *Found: M1 Task 6; final review, question 11; narrowed by M1.5 Task 3 and M1.6
  Task 4.*
- **A double failure leaves an index risk open.** When an index sync fails, the writer cuts the
  unsynced records back so the journal can supply them again; if the cut fails too, the error
  names both failures, and the risk stays open until the next successful sync, which could carry
  the checkpoint past records the failed writeback lost. *Target: M6 (fsck). Found: M1 Task 6.*
- **A reader can get `E_IO` after such a cut.** A snapshot that counted the records a writer
  then cut fails with an I/O error until the reader takes a new one. A snapshot that opens while
  the cut is made counts every record the checkpoint passed or the sorted views cover and reads
  the rest from the journal, or, if the index is cut again meanwhile, fails the same way.
  Neither returns wrong data nor reports the index damaged. *Documented (storage spec §3).
  Found: M1 Task 6; M1.5 Task 3.*
- **Double and triple faults in the journal's failure policy.** A failed journal append or
  sync is cut back, or the unlisted frames written afresh (storage spec §2 rule 3 (a)–(c)), but
  a second fault can still defeat that. A process that aborts because its cut failed leaves the
  frame behind, and another process may list it after a sync that succeeds (only on a
  filesystem that recovers with its page cache intact: a cut fails in practice because the
  filesystem takes no more writes, and then that listing fails too). A `journal.off` entry whose
  cut failed stays listed though it may be lost. A crash between the cut and the rewrite of
  unlisted frames, or a rewrite that fails (its error names them by seq), loses those frames,
  and one of them may be a commit acknowledged with a warning, or one whose `journal.off` entry
  a power loss zeroed. A rewrite that lands anywhere but where the frames were (another writer's
  bytes came in between: one that writes without the lock) is cut back there and refused as
  damage, which loses the rewritten frames, one of which may be a commit acknowledged with a
  warning, and the intruding bytes, with no trace but that one error. A crash inside a frame's
  append, between the write and its `fdatasync`,
  may leave no one to receive the frame's writeback error. *Target: M6 (the next `FORMAT`'s
  frame header, with the frame's sequence number and the previous frame's CRC, and fsck).
  Found: M1.5 Task 2.*
- **A failed directory fsync in the object store is retried, not taken back.**
  `FsObjectStore::flush` keeps a fan-out directory whose fsync failed and fsyncs it again on a
  later flush; the commit whose flush failed fails. The retry is for an fsync that never ran (the
  directory could not be opened: `EMFILE`, `ENOMEM`, `EACCES`). A later success cannot make
  good a failed one, and on the filesystems the store supports it need not: there a failed
  directory fsync is fail-stop (storage spec §2 rule 5), so every later sync fails too; the
  order checker and the fsync-failure model assume the same. On a filesystem that is not
  fail-stop (ext4 without a journal, ext2, FUSE, NFS or SMB), which the store does not detect
  yet, a later fsync's success proves nothing about the earlier links, and a later commit may
  name an object whose link a power loss then takes back. *Target: M6 (detection by `statfs`).
  Found: M1.5 Task 2; the 2026-10-16 expert review.*
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
- **A read can give up under sustained commits.** A snapshot reads the refs and the frames again
  until they hold still, in at most 32 rounds; when a commit lands during every round, it fails
  with the retryable `E_BUSY` rather than wait. The bound does not make such a reader progress: each
  round reads every ref file and every frame past the checkpoint again. Reading again only the
  refs the new frames move, or starting over from a fresh checkpoint, is the next step if a
  measurement shows readers giving up; bench-compare's W6 ([perf.md](perf.md)) measured a
  snapshot and lookup beside a writer's 64 commits/s at 87.3 µs p50 and 5.0 ms at worst.
  *Unscheduled. Found: M1 Task 2c; the 2026-10-16 code review (Q9).*
- **Readers may return state that is not durable yet.** A local reader (the CLI, the MCP server)
  sees a frame before its `fdatasync` completes, so a power loss, or a failed sync of that frame
  (which the writer then cuts back), can take back state it returned (operations spec §4 says what
  agents may rely on). Readers whose results leave the host (M8's git export, M11's `serve`) take a
  committed snapshot under the lock instead, `MetaStore::read_committed`, which neither can take
  back, at the cost of a wait behind the writer that holds the lock. *Documented (storage spec §3).
  Found: M1 Task 2c; final review, question 2.*
- **A commit acknowledged with a warning can vanish for a moment.** When a writer's tail check
  writes unlisted frames afresh after a failed sync (storage spec §2 rule 3 (c)), a reader that
  reads between the cut and the rewrite does not see them: a changeset whose commit was
  acknowledged with a warning can be `E_REV_NOT_FOUND` for that moment, and a retry finds it.
  It takes a failed listing and then a failed sync. *Documented (storage spec §3). Found: M1.5
  Task 2 review.*

## Reads and response sizes

- **`cat` reads the whole object file.** `read_file` returns part of a file, from `offset` or by
  `lines`, and keeps only the bytes it returns while it verifies the whole blob (M2), but it
  still reads the blob's object file whole: a large blob stored raw costs its size in memory on
  every read, however little is asked for. Bounded reads come later. *Target: M6. Found: M1 Task
  4b; final review, question 6; the offset gap by the M1.5 Task 5 review, closed in M2.*
- **`ls` of a wide directory decodes its whole tree.** Paging limits what is returned, not what
  is read: a page decodes the whole tree object of the listed directory, and at a `depth` of
  every directory it enters, however few of their entries it lists. Bounded reads come later.
  (Sizes and depths came in M2, and the default page was measured again for them: 250 entries.)
  *Target: M6. Found: M1 Task 5; final review, question 9.*
- **A snapshot reads every ref file**: cheap with a few refs, linear in their number. Each
  request reads in one snapshot of the metadata (a commit, `cat`, `ls`, `show` and `log` take one
  each, a ref move one besides its write), so a request pays it once. Refs are `main` and
  `release/*` alone by design; should they grow past dozens, a snapshot's reading them calls for
  a validated lazy read, a per-handle map of refs or a packed refs file (a FORMAT change), and
  the `read_snapshot` bench measures 0, 16 and 256 refs to tell. *Unscheduled: when refs grow
  past dozens. Found: M1 Tasks 2c, 3 and 4b.*

## Commits, refs and history

- **No merge.** An agent that loses a ref race and commits its change again on top of the
  winner's can overwrite the winner's edits to the same files: M1 has no three-way merge. So
  can a `commit --advance` without `--parent`: it builds on the ref's head when it runs, and its
  move catches only a race from then on, not edits that landed since the agent read the files
  it puts whole. *Target: M4 (merge and landing). Found: M1 Task 4b.*
- **A retried commit can record a second changeset.** A commit signs at its own time, so when
  a commit's outcome is unknown (its process died before it answered) and it is run again, a first
  attempt that did commit stays as an unreferenced changeset beside the second. A `commit --advance`
  retried a second or more after a first attempt that published records nothing instead: the ref
  names that attempt, so the retry fails with `E_CAS_MISMATCH`, whose `actual` may be the agent's
  own change; `swarmops show <actual>` gives its `agent` and `run_id` metadata, to check before
  applying the edits again on top of it. An idempotency key (`commit --key`) closes both. *Target:
  M4. Found: M1 final fix pass; the `--advance` case, the M1.6 review.*
- **History follows first parents only.** `log`, `log --path` and `history` walk first parents,
  and a changeset's changes are those against its first parent: what a merge brought in from its
  other parents shows as the merge's own change, and the changesets on those parents are never
  listed. Native histories have no merge (`main` is linear, workspaces land squashed); git
  import's do. *Target: M3, if imported merge histories need it (`LogReq.first_parent`). Found:
  M2 Task 15 (M2 Decision 8).*
- **`--expect` with a ref name cannot detect a move.** A ref name given as the expected target
  resolves when the command runs, so it matches whatever the ref holds by then (it fails only
  if the ref is gone); give an id for an expectation that can fail, or be replayed.
  *Documented (`--expect` help). Found: M1 Task 5; final review, question 13.*
- **A commit holds its contents in memory.** All of them, up to the 1 GiB they may hold
  together, and up to about two to three times that while their objects are encoded in
  parallel, besides the request's text (at most 64 MiB). `blob put` (`blob_put`) and
  `{"blob": "<id>"}` take large contents out of the commit request. *Documented (bounded by the
  limits since M2). Found: the 2026-10-16 code review, issue 10.*

## Verification and repair

- **No repair tool.** `verify` reports damage (a corrupt listed frame, index records lost below
  the checkpoint, a malformed ref file, a missing index file), but M1 cannot fix any of it;
  restoring the store is the only remedy. The index can be rebuilt from the journal in
  principle (`fsck --rebuild-index`). *Target: M6 (fsck). Found: M1 Tasks 2b, 2c and 3; final
  review, question 5.*
- **A zero-filled index tail over a damaged journal wedges the store.** A store whose journal
  is damaged before the checkpoint, and whose index records past the checkpoint a power loss
  zeroed, opens with `IndexCorrupt` (`E_STORE_CORRUPT`) on every read and write; no writer
  repairs it. (A malformed `journal.ckpt`, or one that is not a frame boundary, fails every read
  and write with `E_STORE_CORRUPT` on its own; verify reports both.) *Target: M6 (fsck). Found:
  M1.5 minors (Task 3 review N1).*
- **A zeroed index tail over a damaged journal can be counted when no record before it has
  parents.** Where the journal cannot vouch for the index's records past the checkpoint (it is
  damaged, or the index lacks the last changeset before the checkpoint), the index counts those
  records by their lengths and checks only that each record's parents start in `parents.log`
  where the record before it leaves off. Entries a power loss zeroed read "no parents, at 0":
  after a record with parents the check refuses them (the entry above), but after records that
  have none they pass, so the zeroed records are counted as they read, and the next write keeps
  them. `verify` reports the journal's damage, not them. *Target: M6 (fsck). Found: M1.5
  code-minors review (minor 2); M1.5 final review (D11).*
- **Temp files of crashed writes stay.** `objects/tmp/` keeps what interrupted object writes, and
  interrupted writes of external id entries, left there, and `meta/derived/tmp/` what interrupted
  writes of derived values left. *Target: M6 (`gc`). Found: M1 final review, question 8; M2 Task 5.*
- **Derived values are never collected.** `meta/derived/` keeps every value it was given: those of
  a derivation's older versions, and those about objects nothing names any more. A damaged value
  file that the store fails to remove reads as missing, and is derived again, on every read.
  *Target: M6 (`gc`). Found: M2 Task 5.*
- **A damaged external id entry stays until it is removed by hand.** `verify` reports it
  (`external_id_corrupt`), but nothing removes it, and a put never replaces a file, so its id
  stays unmapped while it is there. The index is a cache: what that costs is the work its writer
  does again (the git import converts that commit again, and finds its changeset in the store).
  *Target: M6 (fsck). Found: M3 Task 6.*
- **`verify` does not scale to very large stores.** It holds the journal and the index in memory,
  and checks each sorted-view entry with its own reads (about 9 million reads at a million
  changesets, about 9 per changeset: each index record, and each entry of both sorted views, is
  read on its own). *Target: M6 (streaming verify). Found: M1 Task 3; final review.*
- **`verify`'s report has no cap.** `problems` and `findings` list every problem found, a line
  and a finding each, and a failed verify's `E_STORE_CORRUPT` carries the whole report in
  `details`, so a store damaged in many places gives a response as long as its damage, twice
  over through MCP, whose result holds its envelope twice. *Target: M6 (streaming verify, which
  pages the report and says when it cut it, `truncated`). Found: M1.5 addendum review (gap 14;
  ruling 9(c)).*
- **A missing `journal.off` opens as `E_IO`** (`details.store_file: meta/journal.off`), not
  `E_STORE_CORRUPT`. *Unscheduled. Found: M1 final review.*
- **`verify --deep` skips object files that are symlinks**, because the object listing ignores
  them; SwarmOps never writes one. *Unscheduled. Found: M1 final review.*

## Hostile input

[SECURITY.md](../SECURITY.md) says which inputs are in scope.

- **A zstd bomb object is decoded whole.** An object file whose zstd frame really produces a
  large payload, a small file that decompresses to the size it records, is decompressed in full
  before its hash is checked: up to 512 MiB, the most an object holds (`MAX_OBJECT_LEN`, storage
  spec §2 rule 1), since a header that records more is refused as corrupt before anything is
  allocated for it. A frame that only claims a size it does not produce is refused as corrupt
  too (`E_STORE_CORRUPT`, which `verify --deep` lists), before more than 64 MiB is reserved for
  its output, beside the decoder's window, which the frame's header sets up to zstd's default
  limit of 128 MiB: about 192 MiB at worst. SwarmOps never writes such a file; a crafted store
  can hold one. Streamed reads would bound `cat` below 512 MiB. *Target: M2 (streamed reads); M3
  Task 4 set the bound. Found: M1 Task 1; the 2026-10-16 code review.*
- **A huge `journal.log`, `journal.off`, index column or sorted view aborts a read.** The journal
  past the checkpoint and the `journal.off` entries from there (`read_upto`, `Journal::entries`;
  every read and write, and recovery's tail check and listed-chain walk), a column range
  (`Column::entries`: the `ids.col` or `dh.col` tail of every lookup and of a compaction,
  `by_length` over a damaged journal) and a view range (`Sorted::recs`: every match a prefix or
  delta-hash lookup returns) are each read into one buffer sized from the file's length, with an
  infallible allocation: a sparse or crafted file of gigabytes makes any command take that much
  memory and abort when the allocation fails, instead of failing with an error, and the abort is
  no panic, so `swarmops mcp` dies with it. SwarmOps never writes such a file. *Target: M6
  (bounded reads, when M6 moves the metadata read path onto `FsOps` for fault injection: the
  journal a frame at a time with a streaming CRC, `journal.off`, the index columns and the views
  in windows, and a cap on what a lookup returns). Found: M1.5 final review (security-1).*
- **Crafted `journal.off` entries can mis-number a read.** Entries crafted to agree with each
  other (one pointing into a payload whose bytes read as a frame header ending where the next
  entry starts), three or more links before the end, pass the two links recovery and readers
  check: a read that starts past them numbers its frames by them, and `frame_count` counts
  them. That is outside the crash model, and `verify`, which walks every listed frame, reports
  it. No frame is lost: before recovery cuts or lists anything it checks every listed frame
  ("Recovery after a crash walks all of `journal.off`" above), which the `journal_recovery`
  fuzz target asserts on every input. A checksum on each entry is a format change: the next
  `FORMAT` (planned as 2, M6), together with a frame header that carries the frame's sequence
  number and the previous frame's CRC. *Target: M6. Found: M1 fuzzing; final review,
  question 7.*
- **A zeroed `journal.off` entry deep in the chain** (three or more links before the end) is
  outside the crash model: recovery checks the last two links, and `verify` finds the rest.
  *Target: M6 (the next `FORMAT`'s checksums). Found: M1 fuzzing review.*
- **A changeset written outside the repository library can hold both `x` and `x:b64`.** The
  library refuses a metadata key that ends in `:b64` for every writer
  (`swarmops_repo::check_metadata`, which `Repo::commit` runs), so only a store written by other
  means, such as a crafted one, can hold such a pair; `show` then gives one of the two entries,
  and `verify` does not report it. *Unscheduled. Found: M1 Task 4b.*

## Serving requests

- **A usage error placed before `mcp` is printed on stdout.** `swarmops --frob mcp` fails
  while the command line is parsed, before any server starts, and its `E_INVALID_ARG` envelope
  goes to stdout with exit code 2; a usage error on a line that parses as `mcp` goes to stderr.
  No protocol bytes are ever interleaved, because no server starts. *Unscheduled. Found: M1.5
  Task 5 review.*
- **`{"file": "<path>"}` content is read by whoever serves the request.** That is the caller for
  the CLI and the stdio MCP server; a server that takes requests from another host (M11's
  `serve`, then M10's) must refuse such content, and a client must turn it into bytes before the
  request leaves. Through MCP a relative path, a path that is the server's own stdin or stdout,
  and anything that is not a regular file (a FIFO, a socket, a device or a directory, or a
  symlink to one) are refused before anything is read. *Target: M11 (`serve`), M10. Found: M1
  Task 4b; M1.5 Task 9 review (M11's `serve`).*
- **Four error codes are not raised yet.** `E_REF_NOT_FOUND` (no operation looks a ref up by
  name), `E_AUTH`, `E_POLICY_DENIED` and `E_MAINTENANCE_BUSY` (tokens, `policy.toml` and gc's
  maintenance lease come later) are in the schema, so that an agent can match them before any
  operation raises them. *Documented. Found: the 2026-10-16 comparison; M1.5 Task 9 review.*
- **One temp-file error names a host path.** When 65,536 temp-file names in a row are taken in
  a directory of the store, the `tempfile` crate gives up with `too many temporary files exist`
  and the directory's absolute path, which an `E_IO` then carries; the names are random, so it
  is not met in practice. The other errors that name the store's own path answer for the path a
  command was given (operations spec §0). A server that takes requests from another host must
  not pass either on as it is. *Target: M11 (`serve`), M10. Found: the M1.6 consolidated review
  (error contract).*

## Git import and export

- **`created_at` cannot show a time before 1970.** A changeset keeps its signatures' times as
  signed seconds (storage spec §1, §7), but the API's `created_at` in `show` and
  `log`, unsigned milliseconds derived from the committer's time, reports 0 for any time before
  1970. The git import keeps such commits as they are, without loss (the M3 plan's decision 4),
  and `show`'s `signatures` give their exact times. *Documented (storage spec §7). Found: M1
  Task 4a.*
- **No test reads a real repository yet.** M1 had none, so the final review had nothing to judge
  (question 14). The first come with the git import, behind `SWARMOPS_HOST_REPOS=1`. *Target:
  M3.*
- **A git import or export reports what it repaired only when it succeeds.** An import's
  `meta.recovered` adds up what its frames' and its ref move's write transactions repaired of what
  earlier writers left. An import that fails reports none of it (the import's error carries no such
  record), and the metadata flush before the ref move reports nothing either
  (`Repo::flush_metadata` returns no record), so an import that has no commit to write and finds
  the store in need of repair does the repair unreported. An export's reports what the tail check
  of its committed snapshot repaired, and an export that fails after that check (a divergence, a
  held branch lock) reports none of it: the export's error carries no such record. *Unscheduled.
  Found: M3 Task 12; M8 Task 6.*
- **An export writes loose objects.** Each object an export writes is a loose file, fsynced on its
  own, so the first export of a long history writes one file and one fsync per object until `git
  gc` packs them (M8 Decision 5; one pack per export is the alternative, which M8 Task 11's
  measurement of the service repositories weighs). *Documented (storage spec §7). Found: M8 Task
  5.*
- **A crash while an export moves its branch leaves the branch's lock.** An export that dies
  between taking `refs/heads/<branch>.lock` and renaming it onto the branch leaves the lock file,
  as git does: every later export of that branch, and git itself, refuses the branch
  (`E_GIT_WRITE`, `details.reason` `ref_locked`) until the file is removed by hand, once no git
  process runs. *Documented (storage spec §7). Found: M8 Task 5.*
- **An export leaves no reflog entry.** The branch moves without a line in the repository's
  `logs/`, so `git reflog` does not show an export's moves. *Documented (storage spec §7). Found:
  M8 Task 5.*
- **A history imported with a loss exports as new commits.** A commit the import took with a loss
  (`git.lossy`: a dropped gitlink, say) renders to other bytes than the commit it came from, and so
  does every commit above it, whose parent's oid changes with it: they export as new commits
  (`rewritten`, and a warning), which a branch built on the originals does not take
  (`lossy_history`), and the signature headers they keep verbatim (`gpgsig`) no longer verify.
  *Documented (storage spec §7; M8 Decision 13). Found: M8 Task 5.*

## Platforms

- **The filesystem check is by path name only.** A store under `/mnt/` is refused, but NFS, SMB
  or FUSE mounts are not detected (and not supported). *Target: M6 (detection by `statfs`).
  Found: M1 Task 3; umbrella spec §4.*
- **Directory fsync failures are assumed fail-stop.** On ext4 with a journal, XFS and btrfs a
  failed directory fsync makes every later sync fail until the filesystem is mounted again, so a
  later sync that succeeds covers the entries made before it (storage spec §2 rule 5). ext4
  without a journal, ext2, FUSE and network filesystems do not behave this way and are not
  supported. *Target: M6 (detection by `statfs`). Found: the 2026-10-16 code review.*
- **btrfs and a resumed `init`.** Resuming an interrupted `init` relies on ext4's and XFS's
  ordered journal to make the crashed attempt's directories durable; on btrfs they might not be
  (recoverable by creating the store again). *Unscheduled. Found: M1 Task 3.*
- **How a store path resolves.** A `..` after an existing file is taken lexically, and a
  dangling symlink is followed (both documented on the resolver). *Unscheduled. Found: M1
  Task 3.*
- **A store is found by `--repo` or `SWARMOPS_REPO`, and nothing else.** There is no default
  store location (`init` needs its directory; `~/.local/state/swarmops/repos/<name>/` is only the
  convention the README follows) and no upward search for a `.swarmws` directory. *Target: M5
  (the `.swarmws` search, with workspaces); a default location is unscheduled. Found: M1 Task 5;
  umbrella spec §4; M1.5 final review.*
- **No migration between pre-release formats.** A store made before a format change may be
  refused afterwards and need creating again; it happened once during M1. *Documented (the
  versioning policy, before 1.0).*

## Performance

- **Part of each lock pass through the binary is unexplained.** In the quiet-window run the lock
  passed every 8.27 ms at store level (about two raw `fdatasync`s, which is what a transaction
  makes) and every 13.75 ms through the binary; the difference appears only with the commits'
  own object writes on the same filesystem. *Target: M1.6 (docs/perf.md),
  then M2. Found: M1 Task 7b.*
- **Commit-path costs.** Every
  commit's materialization scans the index tail; and the reflog walks frames past the snapshot
  only to drop them. *Target: M2. Found: M1 Tasks 2c and 3; deferred again by Task 8.*
- **Small object-store costs.** A put that creates a fan-out directory checks for it under the
  store's mutex. *Unscheduled. Found: M1 Task 1.*
- **No benchmark gate.** The benches run by hand (`just bench`, `just perf`); failing on a 20 %
  regression (`bench/thresholds.toml`) comes later. *Target: M9. Found: docs/perf.md.*

## Tests and tooling

These do not change what SwarmOps does, only how well the tests can see it.

- **The gate lints the build without features only through `test_hooks`.** Clippy runs over
  every target with `--all-features`, and without features only for the `test_hooks` target and
  the library crates it builds; the other targets (unit tests, binaries, benches, the other test
  files) are linted only with the features on. *Unscheduled. Found: M1 final fix pass; narrowed by
  the M1.5 final review.*
- **Coverage is not measured by the gate.** It was measured once, part-way through M1, for core
  and store only: 94.61 % of lines. *Unscheduled.*
- **No test watches what a `RealFs` call does inside.** The order checker judges the order of
  `FsOps` calls, and the power-loss images are built from them, each call taken as `FsOps`
  documents it. That `atomic_replace` syncs its temp file before the rename and the directory
  after it, that `append_durable`, `truncate` and `write_durable_temp` sync, that
  `rename_durable` syncs the directory, and that `sync_data` and `fsync_dir` flush are read in
  `fs_ops.rs`, not tested: a `RealFs` that dropped one of those syncs would pass every gate. The
  tracer of `bench/compare` (`docs/perf.md`) counts a metadata commit's sync calls by file, but
  `just check` does not run it. *Unscheduled. Found: M1.5 final coverage review.*
- **The index reads its vouching journal around the store's filesystem seam.** To vouch for the
  records past the checkpoint, the index reads the journal through a `Journal` of its own on the
  real filesystem: in `Index::open`, which `verify` uses, and in a snapshot's or a write's index
  open that finds none of the changesets past the checkpoint among the index's last records
  (`vouched` and `vouched_with` in `meta/index.rs`). No injected failure, recorded call or hook
  reaches those reads' measurements of `journal.off` and `journal.log`, so their re-read of a
  `journal.off` a writer cut is tested only at the journal's level. *Target: M6 (the metadata
  read paths through `FsOps`, umbrella spec §14). Found: M1.5 Task 3 review (M3); M1.5 final
  review (D5).*
- **Golden envelopes cover 15 of the 24 error codes.** `error_codes_reach_the_cli` pins the
  envelope of 15; the other nine are in its `NOT_RAISED` list, each with why the test cannot raise
  it (the four codes above among them). *Unscheduled (`E_IO` joined in M1.6). Found: the
  2026-10-16 comparison.*
- **The eight-committer race can meet `E_BUSY` on a loaded host.**
  `multiprocess::eight_processes_commit_concurrently_all_persist` races eight commits, which are
  what it measures, so it retries none: a lock wait that load stretches past 10 s fails it. Its
  sixteen-racer sibling retries its setup commits (`retry_busy`). *Unscheduled. Found: M1.5
  Task 3.*
- **Gaps the reviews found and left.** The I/O-error sweeps now fail each of a commit's calls
  once, the object store's links and directory fsyncs included, and check that the commit
  reports honestly, but the cleanup after a failed link is still not asserted; the CLI's panic
  path and the journal's adoption path in the crash sweeps are untested; there are no tests yet for a
  relative symlink target or a `..` after a symlink in the store path, and `verify`'s "read only
  up to its damage" message is not pinned; the CLI tests assume a temporary directory without
  spaces; and one end-to-end test skips when run as root. *Unscheduled. Found: M1 Tasks 1, 2a,
  3, 5 and 8, and the final fix pass; narrowed by M1.5 Task 2.*
- **The specs are in Korean.** They are the authority; [architecture.md](architecture.md) is an
  English overview, not a translation. *Unscheduled.*

## The final review's 14 questions

The final review of M1 left 14 questions for the maintainer to rule on. All 14 were ruled, and
each ruling is tracked above:

| # | Question | Ruling | Entry |
| --- | --- | --- | --- |
| 1 | Concurrent `init` on one directory | unsupported, documented; revisit with workspaces | M5, "Concurrent `init`" |
| 2 | Readers return state from frames not yet synced | allowed for local readers; committed snapshots for off-host readers (M8) | "Readers may return state" |
| 3 | The lock's wait tail | `lock_timeout` stays 10 s; a FIFO hand-off later | M6, "bounded, not first-in, first-out" |
| 4 | Commit latency against the spec's target | target restated without syncs; batching re-evaluated | M2, "Full fsync is slow" |
| 5 | The recovery walk under the lock | with fsck, and the lock timeout against journal size | M6, "Recovery after a crash walks all of `journal.off`", "No repair tool" |
| 6 | `read_file` reads the whole blob | ranged reads | M6, "`cat` reads the whole object file" (ranged reads: M2) |
| 7 | Crafted `journal.off` entries | a per-entry checksum in the next `FORMAT` | M6, "Crafted `journal.off` entries" |
| 8 | `objects/tmp` leftovers | `gc` | M6, "Temp files of crashed writes" |
| 9 | Wide directories in `ls` | size and depth | M6, "`ls` of a wide directory" (size and depth: M2) |
| 10 | Parked helper threads | the daemon | M10, "An abandoned lock wait" |
| 11 | `abort()` standing in for `SIGKILL` | a literal `SIGKILL` test | M6, "The crash model" |
| 12 | Second root commits | `commit_raw` | closed in M3: `Repo::commit_raw`, the git import's writer, makes a root changeset whatever refs exist |
| 13 | `--expect` with a ref name | documented | "`--expect` with a ref name" |
| 14 | Tests that read host repositories | none in M1: nothing to judge | M3, "No test reads a real repository" |
