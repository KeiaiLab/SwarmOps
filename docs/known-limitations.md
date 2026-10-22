# Known limitations

What SwarmOps does not do yet, or does less well than it should, as of M13, and the milestone
meant to change it. The items come from the M1 and M1.5 reviews and the 2026-10-16 code review:
the carry-overs their tasks handed to later milestones, the deferred minors that are still open,
and the rulings of M1's final review, which left 14 questions to the maintainer (the table at the
end lists all 14). Each entry says where it was found; those pointers (an M1 or M1.5 task, a
review, the 2026-10-16 comparison or code review) refer to the maintainer's local records, which
are not in the repository. Code-level minors that change nothing a user or an agent can see
(naming, comments, micro-optimizations, test labels) are not listed.

Targets are milestones of the umbrella spec
(§13): M1.6 the code-review follow-ups · M2 diff, history and derived data · M3 git import ·
M4 merge and landing · M8 git export · M12 portability · M13 git remotes · M5 workspaces ·
M6 verify, crash safety and fuzzing · M14 git clients against a store · M7 blame and review ·
M9 simulation and a benchmark gate · M11 remote sync, offline-first.
M0, M1, M1.5, M1.6, M2, M3, M4 and M8 make the first release, 0.1.0; M13 makes 0.2.0. M1.5, the hardening
milestone after M1, closed a few, and M1.6, the code-review follow-ups after it,
closed more. "Deferred
until after the pilot" marks a delivered milestone's task that waits until SwarmOps has been
piloted on a real repository ([pilot.md](pilot.md)): the measurements that need a quiet host,
and the tests that read the maintainers' service repositories. "Unscheduled" means no milestone
has taken the item yet; "documented" means it is the intended behaviour. An RPC server is a
follow-up after v1 (umbrella spec §17), not a milestone, so no entry targets it.

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
  `--durability fast` flag is justified, are re-evaluated with new measurements. *Target: M2's
  journal-sync decision (its plan's Task 4), deferred until after the pilot. Found: M1 Task 7b;
  final review, question 4.*

## Concurrency and the lock

- **The lock is bounded, not first-in, first-out.** Waiters queue in the kernel, but a woken
  waiter races the others and the writer that just let go. With 32 writers committing in a loop,
  a commit can wait a few hundred transactions; where a transaction takes more than about 28 ms
  (a slow disk), that can exceed the 10 s `lock_timeout` and end in `E_BUSY` (retryable). The
  quiet-window measurement saw no `E_BUSY` and a slowest commit of 918 ms (1.2 s at store
  level). A ticket or FIFO hand-off on top of `flock` is the next step. *Target: M6. Found: M1
  Task 7c; final review, question 3.*
- **Landing retries under a burst.** `land` publishes by compare-and-swap: a land that loses the
  race for its target records nothing, merges again onto the new head and tries again, up to its
  `max_attempts` (8 by default, at most 32), and then fails with the retryable
  `E_CAS_RETRY_EXHAUSTED`. Nothing is lost, but a burst of lands, such as subagents that finish
  at once, costs retries: in one closed-loop run through the binary on a loaded host (load 80 to
  87, so the figures are indicative), 8, 16 and 32 processes landing 4, 4 and 3 times each on one
  `main`, each land its own file and every other one a line of one shared file, landed every
  change exactly once with `verify --deep` clean, and 22 %, 44 % and 44 % of the lands retried;
  at 32, four of the 96 took all 8 attempts. The umbrella spec's target (§15) for 50 landing
  processes is 20 lands/s or more, a p99 under 500 ms, a retry rate under 5 % and nothing lost;
  the M4 plan measures it with each process landing once every 2 s. An agent that meets
  `E_CAS_RETRY_EXHAUSTED` lands again with the same key, or asks for more attempts. *Target:
  M4's landing measurements and landing model (its plan's Tasks 11, 1 and 5), deferred until
  after the pilot. Found: the 2026-10-20 plan triage (a probe through the binary).*
- **Recovery after a crash walks all of `journal.off` under the lock.** Before it cuts or
  lists anything, the first writer after a crash checks every entry of `journal.off`. On a long
  journal that walk could approach `lock_timeout` for the writers queued behind it. *Target: M6
  (fsck, and the lock timeout against journal size). Found: M1 fuzzing review; final review,
  question 5.*
- **An abandoned lock wait parks a thread.** A wait that times out leaves its helper thread in
  the kernel's queue until the holder lets go, and a hung holder leaves one parked thread per
  abandoned wait. That costs nothing in a short-lived CLI process; a long-lived one (the stdio
  MCP server is one, a network server after v1 would be another) needs a way out. *Unscheduled
  (it was M10's; an RPC server is a follow-up after v1, umbrella spec §17). Found: M1 Task 7c;
  final review, question 10.*
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
- **A land merges a file both sides changed in memory, whole.** For a path that both the change
  and the target changed since their base, `land` reads the three versions whole and line-diffs
  them, with no cap of its own; a large file changed on both sides costs a few times its size.
  The commit limits bound what a file can hold (512 MiB a put). *Target: M6 (bounded reads).
  Found: the 2026-10-20 plan triage.*
- **`diff` in stat and unified form reads both blobs whole.** For each changed file the two
  versions are read whole and its hunks are built before the page is cut to `max_bytes`, so
  memory and time follow the files, not the page (a 33 MB file's unified diff took about
  122 MB); the default form, `names`, reads no content. *Target: M6 (bounded reads). Found: M2's
  consolidated review (issue #4 item 5).*
- **A page cursor does not pin the revisions it pages.** `diff`'s `cursor` and `read_file`'s
  `next_offset` say where the next page starts, not which revisions it pages: an agent that pages
  through a diff or a file at `main` while another agent lands gets its next page from the new
  revision, with no error. Page with the full ids the first page resolved (a diff's `from` and
  `to`). *Unscheduled (issue #4 item 3). Found: M2's consolidated review.*
- **`read_file` with `max_bytes` 0 returns nothing, as truncated.** Its result has
  `truncated: true` and a `next_offset` equal to its `offset`, so an agent that reads on while a
  result is truncated never ends; ask for at least one byte. *Unscheduled (issue #4 item 2: 0 to
  be refused with `E_INVALID_ARG`, as `diff` refuses it). Found: M2's consolidated review.*
- **A snapshot reads every ref file**: cheap with a few refs, linear in their number. Each
  request reads in one snapshot of the metadata (a commit, `cat`, `ls`, `show` and `log` take one
  each, a ref move one besides its write), so a request pays it once. Refs are `main` and
  `release/*` alone by design; should they grow past dozens, a snapshot's reading them calls for
  a validated lazy read, a per-handle map of refs or a packed refs file (a FORMAT change), and
  the `read_snapshot` bench measures 0, 16 and 256 refs to tell. *Unscheduled: when refs grow
  past dozens. Found: M1 Tasks 2c, 3 and 4b.*
- **A file of control characters reads back at many times its size.** `read_file` returns UTF-8
  content as JSON text, which spells most control characters in six bytes each (`\u0001`): a page of
  a file made of them prints at six times its length through the CLI, and at thirteen times through
  MCP, whose result holds its envelope twice, the second time as JSON text again (a 1 MiB page of
  such a file is a 13 MiB answer). `max_bytes` (1 MiB unless asked otherwise) has no upper limit but
  the file's size, and the MCP server builds its answer in memory before it sends it. *Unscheduled.
  Found: the 2026-10-20 security review.*

## Commits, refs and history

- **A retried commit that omits its parent can record a second changeset.** `commit --key` (a
  uuid v7) makes a retry of the same request replay the first attempt: the commit signs at the
  key's time and records the key as `commit_key`, so on the same parent it is the same
  changeset (`replayed`), and on the changeset the key made it replays that one. A keyed commit
  without `--parent` builds on `main`'s head (the advanced ref's, with `--advance`) as it is
  when it runs, so a retry after that head moved to another changeset commits again, on the new
  head. Given `--parent` by its full id, the retry records the changeset once; with `--advance`
  too, a ref that moved on past it makes the retry `E_CAS_MISMATCH`, and `swarmops log` shows
  whether the ref's history holds it. A commit without a key still signs at its own time, so
  its retry records a second changeset, or, as a `commit --advance` retried a second or more after a
  first attempt that published, nothing: the ref names that attempt, so the retry fails with
  `E_CAS_MISMATCH`, whose `actual` may be the agent's own change; `swarmops show <actual>` gives its
  `agent` and `run_id` metadata, to check before applying the edits again on top of it. *Target: M9
  (an index of commit keys, if measurements ask for one). Found: M1 final fix pass; the `--advance`
  case, the M1.6 review; narrowed by M4 Task 8.*
- **A keyed land searches the target's first parents.** A land with a key first looks for an
  earlier land of the same key on the target's first-parent chain, from its head down to the
  land's base, so a land made on an old base walks the changesets landed since. *Target: M9 (an
  index of commit keys, if measurements ask for one). Found: M4 Task 12's hand-off.*
- **`undo` does not take back a land that later lands built on.** It moves refs back only while
  they still name what the operation set; under later lands that is `E_CAS_MISMATCH`, and no
  `revert` lands the inverse change. *Unscheduled. Found: M4 Decision 10.*
- **No hooks or annotations after a land.** A land records no post-land verification, and
  `land`'s result has no `verification` field: annotations need a journal record the current
  `FORMAT` lacks. Agents check a landed tree with `read_file` and their own runs. *Target: M7, on
  M6's next `FORMAT` (M4 Decision 11). Found: M4 Task 12's hand-off.*
- **No rename detection.** A rename is a delete and an add, for `diff`, `log --path` and a land's
  merge alike: v1 finds no renames (storage spec §4). *Documented. Found: M4 Task 12's hand-off
  (issue #10 item 8).*
- **History follows first parents only.** `log`, `log --path` and `history` walk first parents,
  and a changeset's changes are those against its first parent: what a merge brought in from its
  other parents shows as the merge's own change, and the changesets on those parents are never
  listed. Native histories have no merge (`main` is linear, workspaces land squashed); git
  import's do. *Unscheduled: when imported merge histories need it (`LogReq.first_parent`).
  Found: M2 Task 15 (M2 Decision 8).*
- **`--expect` with a ref name cannot detect a move.** A ref name given as the expected target
  resolves when the command runs, so it matches whatever the ref holds by then (it fails only
  if the ref is gone); give an id for an expectation that can fail, or be replayed.
  *Documented (`--expect` help). Found: M1 Task 5; final review, question 13.*
- **An operation number can name another operation after a take-back.** `op log` numbers an
  operation by its journal frame's seq, and it lists a frame before the frame's sync completes,
  as every local reader does ("Readers may return state that is not durable yet", above). If the
  store then takes the frame back, the next write's frame gets the same number, and an `op undo`
  of the number listed in between moves back the refs of that next operation instead (another
  agent's, perhaps), as long as they still hold what it set, with no error: `undo` reads the frame
  under its lock, so it never undoes the frame taken back, but it cannot tell that the number
  changed hands. Only a take-back frees a number: a frame whose sync failed (its writer gets
  `E_IO_TRANSIENT` with `details.committed: false`), one that a power loss catches before its
  sync completes, and, in the triple fault storage spec §2 rule 3 leaves open, one committed with
  a warning; any other frame keeps its number. So an agent lists `op log` again before an
  `op undo` (after an error above all) and checks that the operation's refs and changesets are
  the ones it means; an undo's result lists the moves it made, and an undo is an operation of its
  own, which `op undo` can take back. *Unscheduled (issue #11 item 1): `op log` could read M8's
  committed snapshot (`read_committed`), which gives only frames that cannot be taken back, as
  the git export does; an operation identity that a take-back cannot reuse, checked under the
  lock, is the other way. Found: the M4 consolidated review (the op-log area's correctness lens
  and its judge).*
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
- **A damaged or wrong external id entry stays until it is removed by hand.** `verify` reports a
  damaged one (`external_id_corrupt`), and one whose changeset does not stand for its commit
  (`external_id_mismatch`; `verify --deep` finds it where the changeset was made in the store),
  but nothing removes either, and a put never replaces a file, so its id stays unmapped while it
  is there. The index is a cache: what that costs is the work its writer does again (the git
  import passes over the entry with a warning and converts that commit again, into the changeset
  the store holds for it if there is one). *Target: M6 (fsck). Found: M3 Task 6; the wrong entry,
  the 2026-10-20 security review's re-verification of the git import.*
- **`verify` does not scale to very large stores.** It holds the journal and the index in memory,
  and checks each sorted-view entry with its own reads (about 9 million reads at a million
  changesets, about 9 per changeset: each index record, and each entry of both sorted views, is
  read on its own). A deep check of a store with derived values also holds the first-parent
  forest (a few words per changeset) and every `last_modified` value the store holds.
  *Target: M6 (streaming verify). Found: M1 Task 3; final review.*
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
- **A damaged object file stays until it is removed by hand.** A put never replaces a file, so where
  an object's file is there already, whatever it holds (other bytes, or a symlink to nothing), a
  commit of that object's content takes the file for the object: it succeeds, names the object and
  does not count it in `blobs_written`. Reads of the object then fail with `E_STORE_CORRUPT`, and
  `verify --deep` reports it (`corrupt_object`, or `missing_object` for a symlink to nothing, which
  a shallow `verify` reports too). Once the file is removed, the same content committed again writes
  it afresh, and every changeset that names it reads again. *Target: M6 (fsck). Found: the
  2026-10-20 security review.*

## Hostile input

[SECURITY.md](../SECURITY.md) says which inputs are in scope.

- **A git object is read whole, up to 512 MiB.** The git import reads each commit, tree and
  annotated tag of the source whole to parse it, and each blob whole to store it, only once the
  object's header says it is at most 512 MiB long, the most an object holds (`MAX_OBJECT_LEN`,
  storage spec §7): a longer one is refused before a byte of it is read (`E_TOO_LARGE`, naming
  it). Within that bound a small loose file of a crafted repository can still inflate to
  512 MiB, and converting an object takes several times its length: about six to seven times
  for a tree of many short entries and up to about four times for a commit (measured at 16 and
  64 MiB), so one such tree near the bound takes some 3.5 GiB. *Unscheduled: parsing a tree in
  pieces would bound it. Found: M3's fidelity review (issue #5 item 1).*
- **A zstd bomb object is decoded whole.** An object file whose zstd frame really produces a
  large payload, a small file that decompresses to the size it records, is decompressed in full
  before its hash is checked: up to 512 MiB, the most an object holds (`MAX_OBJECT_LEN`, storage
  spec §2 rule 1), since a header that records more is refused as corrupt before anything is
  allocated for it. The bomb's real cost is 512 MiB of payload, plus the decoder window of up to
  128 MiB, and about 1.1 GiB of address space. A frame that only claims a size it does not produce
  is refused as corrupt
  too (`E_STORE_CORRUPT`, which `verify --deep` lists), before more than 64 MiB is reserved for its
  output, beside the decoder's window, which the frame's header sets up to zstd's default limit of
  128 MiB: about 192 MiB at worst. SwarmOps never writes such a file; a crafted store can hold one.
  A derived value's file (`meta/derived/`) over 4 KiB is a zstd frame too, decoded whole before its
  trailer is checked, within the same bound: a header that records a value longer than 512 MiB fails
  the file's checks before anything is decoded or allocated for it, and the file reads as missing.
  Since M2, streamed reads bound `cat` below 512 MiB (`read_chunks`); every other read of the
  payload decodes it whole. *Unscheduled; M2 streamed `cat`, and M3 Task 4 set the bound. Found: M1
  Task 1; the 2026-10-16 code review; for derived values, the 2026-10-20 security review.*
- **A huge `journal.log`, `journal.off`, index column or sorted view aborts a read.** The journal
  past the checkpoint and the `journal.off` entries from there (`read_upto`, `Journal::entries`;
  every read and write, and recovery's tail check and listed-chain walk), a column range
  (`Column::entries`: the `ids.col` or `dh.col` tail of every lookup and of a compaction,
  `by_length` over a damaged journal) and a view range (`Sorted::recs`: every match a prefix or
  delta-hash lookup returns) are each read into one buffer sized from the file's length, with an
  infallible allocation: a sparse or crafted file of gigabytes makes any command take that much
  memory and abort when the allocation fails, instead of failing with an error, and the abort is
  no panic, so `swarmops mcp` dies with it. A `journal.off` that ends in more than 512 zeroed
  entries, as a sparse one does, is refused before that read ("A `journal.off` that ends in more
  than 512 zeroed entries is refused, not repaired" below); one that does not still takes that read.
  SwarmOps never writes such a file. *Target: M6 (bounded reads, when M6 moves the metadata read
  path onto `FsOps` for fault injection: the journal a frame at a time with a streaming CRC,
  `journal.off`, the index columns and the views in windows, and a cap on what a lookup returns).
  Found: M1.5 final review (security-1); narrowed by the 2026-10-20 security review.*
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
- **A `journal.off` that ends in more than 512 zeroed entries is refused, not repaired.** A
  power loss leaves zeroed only the entry being written, so readers and recovery drop at most a
  4 KiB block of zeroed entries at the end of `journal.off`, found with one read, and refuse a
  longer run as damage (`E_STORE_CORRUPT`, at offset 0; `verify` reports it): every read and
  write fails until the file is cut back to its last entry that is not zero. *Target: M6
  (fsck). Found: 2026-10-20 review.*
- **A crafted index's generations steer history queries.** A read checks what the changeset
  index says where it can do so cheaply: a sorted view's entry must name a record that holds its
  key, and a record's parents must start where the record before it leaves off, or the read
  fails with `E_STORE_CORRUPT`; and every first-parent walk takes its parents from the changeset
  objects, which their hashes bind to their ids, so that no walk drops a committed changeset or
  loops. A record's generation below the checkpoint or under the sorted views is taken as it is,
  as is its delta hash: a damaged or crafted `meta/idx/gen.col` can make a revision's `~N` name
  nothing where a changeset is, a land take an older common ancestor than the newest for its
  merge base, and `log --path` list its changesets out of order. Every changeset such a walk
  reaches is on the objects' first-parent chain, and `verify` reports the damage. Checking a
  generation takes the journal, or every generation down to a root. *Target: M6 (fsck). Found:
  2026-10-20, when the read path came to check the index.*
- **A text file past the line diff's bound is not diffed or merged by lines.** The histogram diff's
  time can grow with the square of a file's lines, so `diff` and a land's merge run it only on sides
  of at most 49,152 lines and 8 MiB (`MAX_DIFF_LINES`, `MAX_DIFF_BYTES`): past that,
  `diff --format stat` and `unified` give a changed file no line counts and mark its hunks cut
  (`hunks_truncated`, `unified` with no hunk), an added or deleted file included, and a land whose
  two sides changed or added the file differently conflicts on it as `binary`, not `content` or
  `add_add`, which `land --resolve` takes whole, where git diffs and merges it by lines. Within the
  bound the slowest line diff measured takes about 4 s, and a merge of two such sides about 10 s on
  a loaded 32-thread host, once for each such file a request reaches, so a request over many of them
  can still take minutes. *Unscheduled: a line diff whose work, not its input, is bounded (upstream,
  in imara-diff). Found: the 2026-10-20 security review of M4.*
- **A land's conflict report leaves files out and joins hunks past its bound.** A merge holds its
  first 128 conflict hunks, over all its files, as the three-way merge makes them, and past those
  each file's hunks as one hunk that spans them, each side's lines from the first one's start to the
  last one's end, so such a hunk can span many lines that do not conflict; `read_file` reads its
  lines by their range. The report lists the conflicting files in path order while their entries,
  texts aside, fit in 24 KiB: the first always, its hunks that do not fit as one entry without texts
  that spans them, and a later one whole or not at all. The files it leaves out are counted only in
  the error's message, not in a field of the report, which says `truncated`, and `land --human`
  prints a `conflict:` line for the listed files alone. The report's `auto_merged` and `stale` lists
  have no bound but the paths of the change, and the three-way merge still makes every hunk of a
  file, within the line diff's bound, before the merge keeps 128 of them. *Unscheduled: a count of
  the files left out, a field of the report (a schema change). Found: the 2026-10-20 security review
  of M4.*
- **A changeset written outside the repository library can hold both `x` and `x:b64`.** The
  library refuses a metadata key that ends in `:b64` for every writer
  (`swarmops_repo::check_metadata`, which `Repo::commit` runs), so only a store written by other
  means, such as a crafted one, can hold such a pair; `show` then gives one of the two entries,
  and `verify` does not report it. *Unscheduled. Found: M1 Task 4b.*
- **Crafted derived values that agree with each other are read as valid.** A file under
  `meta/derived/` carries a checksum over its slot and value, not a signature, so anyone who can
  write the store's files can place a whole file with any value. The readers check what they use
  where they can without the walk the value saves: a skip list's first entry against the index,
  its length against the generation, each jump against the generations and against the list one
  level down, and where a lift or a list ends; a last touch's changeset against the snapshot, its
  generation and its delta. One that fails is `E_STORE_CORRUPT`. Lists crafted to agree with each
  other, and a last touch that names an older changeset that touched the path, pass those checks
  and are read as valid, and the engine derives new values from them; `verify --deep` checks
  every value of the built-in derivations against the objects and reports each one
  (`derived_mismatch`), and removing `meta/derived/` has every value derived again. The git
  oids the export memoizes (`git_oid`, `git_tree_oid`, `git_blob_oid`) are no longer among the
  values a crafted file can steer: the export derives every tree's, blob's and commit's git oid
  from the store's own objects, a commit's over its parents' derived the same way down to the
  roots, and binds what it reuses, writes, names as a commit's parent, compares a branch with or
  moves a branch to to them; the git import derives the commit a target's head stands for the
  same way, and takes a tree or a blob of the head for a git object only where the store's own
  bytes render to that object's oid. So no memo either reads is taken for an object's content or
  a commit's history (a memo that disagrees is `E_STORE_CORRUPT`, refused before anything is
  written or a ref or a branch moves), and a deep `verify` re-derives `git_tree_oid`,
  `git_blob_oid` and `git_oid` against the objects too (`derived_mismatch`). A
  keyed trailer whose key lives outside the store would make a placed file fail the built-in
  derivations' checks, at the cost of a new layout version. *Target: an authenticated cache is
  unscheduled. Found: the 2026-10-20 security review.*

## Serving requests

- **A usage error placed before `mcp` is printed on stdout.** `swarmops --frob mcp` fails
  while the command line is parsed, before any server starts, and its `E_INVALID_ARG` envelope
  goes to stdout with exit code 2; a usage error on a line that parses as `mcp` goes to stderr.
  No protocol bytes are ever interleaved, because no server starts. *Unscheduled. Found: M1.5
  Task 5 review.*
- **`{"file": "<path>"}` content is read by whoever serves the request.** That is the caller for
  the CLI and the stdio MCP server; a server that takes requests from another host (M11's
  `serve` first) must refuse such content, and a client must turn it into bytes before the
  request leaves. Through MCP a relative path, a path that is the server's own stdin or stdout,
  and anything that is not a regular file (a FIFO, a socket, a device or a directory, or a
  symlink to one) are refused before anything is read. *Target: M14 (`serve`, reused by M11). Found: M1
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
  not pass either on as it is. *Target: M14 (`serve`, reused by M11). Found: the M1.6 consolidated review
  (error contract).*
- **A request takes the memory its type holds.** The MCP server reads of a message line, at
  most 64 MiB, only the members it serves, and a tool's arguments only as their request type,
  so what else a line holds costs it nothing beyond the line's bytes (operations spec §0). A
  request type's own fields are built in full, though: a long list of short strings (`blob_has`
  ids) or a map of small entries (`land` resolutions) takes up to about 15 times its text, near
  1 GiB for a line at the cap, before the operation refuses what is not an id or a resolution;
  the CLI reads `--changes` and `--resolve` the same way. *Unscheduled. Found: the 2026-10-20
  security audit.*

## Git import and export

- **`created_at` cannot show a time before 1970.** A changeset keeps its signatures' times as
  signed seconds (storage spec §1, §7), but the API's `created_at` in `show` and
  `log`, unsigned milliseconds derived from the committer's time, reports 0 for any time before
  1970. The git import keeps such commits as they are, without loss (the M3 plan's decision 4),
  and `show`'s `signatures` give their exact times. *Documented (storage spec §7). Found: M1
  Task 4a.*
- **No test reads a real repository yet.** M1 had none, so the final review had nothing to judge
  (question 14). The git import's and export's tests read fixtures that real git made. *Target:
  M3's test of this host's repositories behind `SWARMOPS_HOST_REPOS=1` (its plan's Task 13) and
  M8's round trip of them (its plan's Task 11), both deferred until after the pilot.*
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
  measurement of the service repositories, deferred until after the pilot, weighs). *Documented
  (storage spec §7). Found: M8 Task 5.*
- **An export reads the whole history below its head.** The export takes no export oid the
  derived store memoizes as the truth ("Crafted derived values that agree with each other are read
  as valid", above), so every export, an incremental one and one with nothing to write included,
  derives the export oid of each changeset below the head again: it reads each changeset's object
  and its `git_oid` memo and, for a changeset other than an unchanged import, the trees and blobs
  of its root the run has not hashed yet. That is about 20 µs a changeset (a release build, on
  NVMe): an export of one new commit above 12,000 imported ones takes 0.25 s, where
  trusting the memos took 0.03 s, while a first export costs what it did or less (12.5 s for those
  12,000 commits' 36,000 objects, 13.1 s before; 1.2 s for 200 native commits over a 2,000-file
  tree, 6.6 s before). An object missing or damaged anywhere in that history, a changeset or a
  tree or blob of a native one, fails every export (`E_STORE_CORRUPT`, which `verify` lists) until
  it is restored. A keyed memo trailer whose key lives outside the store would let the export trust
  its own memos again. *Unscheduled: an authenticated cache. Found: the 2026-10-20 security
  review's re-verification of the git export.*
- **An import onto a target reads the head's whole tree, and onto a native head the whole history
  below it.** The import takes no git oid the derived store memoizes as the truth ("Crafted derived
  values that agree with each other are read as valid", above): before the head's trees and blobs
  seed its conversion it reads and hashes each of them, and where the head is native, or holds a
  git commit the source does not go on from, it derives the head's export oid with the whole
  history below it, as an export does ("An export reads the whole history below its head").
  In a release build on NVMe, at a load of about 20, an import of one commit onto an imported head
  of 2,000 small files takes 0.08 s (0.07 s before), and onto one of 2,000 files of 64 KiB 0.18 s
  (0.07 s); onto a native head above 12,000 imported commits it takes 0.31 to 0.36 s, about 25 µs
  a changeset, where trusting the memo took 0.05 s. A fresh import costs what it did. An import
  whose walk meets the external id entry of a changeset made in the store, which an export wrote,
  derives that changeset's export oid the same way, once a run, to check the entry: at a load of
  about 30, one commit made on the export of a changeset with 14,000 below it, imported into a
  target of its own, takes 0.36 to 0.46 s, where trusting the entry took 0.06 s. An object
  missing or damaged in that tree or history fails the import (`E_STORE_CORRUPT`, which `verify`
  lists) until it is restored. *Unscheduled: an authenticated cache. Found: the 2026-10-20
  security review's re-verification of the git export, and of the git import.*
- **A crash while an export moves its branch leaves the branch's lock.** A `git export` that dies
  between taking `refs/heads/<branch>.lock` and renaming it onto the branch leaves the lock file,
  as git does: every later export of that branch, and git itself, refuses the branch
  (`E_GIT_WRITE`, `details.reason` `ref_locked`) until the file is removed by hand, once no git
  process runs. The store's own git mirror (M13) does not keep them: under the mirror's lock no
  one else writes it, so the first remote command after the crash removes git lock files older
  than ten minutes, with a warning, and a newer one is the retryable `E_BUSY`
  (`details.lock` `git_mirror_file`). *Documented (storage spec §7). Found: M8 Task 5; the
  mirror's case closed by M13.*
- **An export leaves no reflog entry.** The branch moves without a line in the repository's
  `logs/`, so `git reflog` does not show an export's moves. *Documented (storage spec §7). Found:
  M8 Task 5.*
- **A history imported with a loss exports as new commits.** A commit the import took with a loss
  (`git.lossy`: a dropped gitlink, say) renders to other bytes than the commit it came from, and so
  does every commit above it, whose parent's oid changes with it: they export as new commits
  (`rewritten`, and a warning), which a branch built on the originals does not take
  (`lossy_history`), and the signature headers they keep verbatim (`gpgsig`) no longer verify.
  *Documented (storage spec §7; M8 Decision 13). Found: M8 Task 5.*
- **A git directory's file longer than 16 MiB is refused.** The git import and export let gix
  read a repository's refs (`HEAD`, every loose ref and every pseudo-ref, which a symbolic `HEAD`
  can name), `config`, `config.worktree` and `objects/info/alternates` whole, so they refuse one
  longer than 16 MiB before anything reads it (`E_GIT_UNSUPPORTED`, `details.reason`
  `file_too_long`), as they refuse a FIFO, a socket or a device where they read a file
  (`special_file`). No such file that git writes comes near the bound, but a `FETCH_HEAD` left by
  a fetch of some hundreds of thousands of refs, or a `SQUASH_MSG` of a squash of very many
  commits, can pass it: remove that file, which git keeps only for its next command, and run the
  import or export again. *Documented (storage spec §7). Found: 2026-10-20.*
- **An import holds each commit's new files in memory together.** The import stores the trees and
  blobs it converts each time 64 MiB of them have gathered (`ImportRequest::object_batch_bytes`),
  but only between commits: the new files of one commit are all held until that commit is converted,
  each up to 512 MiB, however many the commit adds, so a commit that adds large files takes their
  size together in memory, and more (about 400 MiB at its peak for one commit of 384 MiB of new
  files). *Unscheduled: storing a commit's objects as they gather, within the commit, would bound
  it. Found: the 2026-10-20 security review.*
- **A commit an earlier import took with a loss is reused without `--lossy`.** A commit the store
  already holds is reused as it is, so a later import without `--lossy` that reaches one an
  earlier import took with a loss reports `lossy_total` 0 and skips it rather than refusing.
  *Unscheduled (issue #8 item 3). Found: M3's consolidated review.*

## Git remotes

- **A fetch, a push or a sync reads the whole history below a native head.** Each imports or
  exports onto the store's history, and so pays the cost the import and the export pay there
  ("An export reads the whole history below its head", "An import onto a target reads the head's
  whole tree", above): on 12,000 commits a one-commit push costs 3.9 times what it does on 1,000,
  and a sync of one land takes 8.8 s, against roadmap targets of 1.5 times and 1 s
  ([perf.md](perf.md#git-remotes-m13-the-three-topology-2-targets), one run on a loaded host).
  *Unscheduled: an authenticated cache, as for the import and the export. Found: the M13 design's
  risks; measured by M13 Task C11.*
- **A squash that lost its trailer lines, then edited on the remote, stops sync.** Sync
  recognises a change the remote holds by a `SwarmOps-Changeset`, `SwarmOps-Squashed-From` or
  `SwarmOps-Land-Key` line in a remote commit, or by its merging as no change. A pull request
  squashed with a message that dropped the commit bodies, whose lines someone then changed again on
  the remote before the sync, matches neither: the sync stops with `E_CONFLICT` naming the change.
  When the remote holds it, move `main` to the remote head's changeset
  (`so ref set main <id> --expect <main>`) and sync again
  ([GitHub as a remote](guides/github-remote.md#when-sync-stops)). *Documented (operations spec
  §6). Found: the M13 design's risks.*
- **A trailer line is taken as proof.** A remote commit that names a change on a `SwarmOps-` line
  makes sync drop that change without checking the commit's content; anyone who can push to the
  remote can write such a line. Every drop is listed in `dropped`, and the change stays in the
  store. *Documented (the M13 design's decision 5). Found: the M13 design.*
- **A remote whose history the import took with a loss cannot be pushed onto.** Commits fetched
  with a `--lossy` class export as new commits (above, "A history imported with a loss exports as
  new commits"), so a push onto such a branch fails with `E_REMOTE_DIVERGED` (`details.reason`
  `lossy_history`) and sends nothing. Fetch and sync work; publishing onto that branch needs git.
  *Documented (operations spec §6). Found: the M13 design's risks.*
- **The mirror doubles the space a repository takes on disk.** It is a git repository of the
  remote's history beside the store's own objects. *Unscheduled: packs only, or packs served from
  the store's objects. Found: the M13 design's risks.*
- **A credential helper that opens a window can wait for a person.** SwarmOps sets
  `GIT_TERMINAL_PROMPT=0`, which git's own prompts obey; a helper with a window of its own does
  not, and under an MCP server no one answers it. Agents' hosts use ssh keys or `--token-env`.
  *Documented ([GitHub as a remote](guides/github-remote.md#credentials)). Found: the M13 design's
  risks.*
- **A long fetch inside an MCP call holds the agent's session.** The MCP server answers one request
  at a time, so a `fetch` or `sync` of a large remote blocks that session until git ends.
  *Unscheduled. Found: the M13 design's risks.*
- **`authors.toml` changed between a keyed commit and its retry makes a different changeset.** The
  file is read each time a changeset is signed, so a retry made after the file changed signs with
  the new address: the retry records a second changeset rather than replaying the first.
  *Documented (operations spec §5). Found: the M13 design's risks.*
- **A server that cuts git's first HTTP response inside its headers looks like an empty remote.**
  git then falls back to the old "dumb" HTTP protocol, finds no refs and succeeds: the fetch reports
  no branches and prunes the mirror's tracking refs, and a clone ends with no `main`. Nothing is
  forced (a later push leases on the pruned value and fails), and the next good fetch restores
  them. *Unscheduled. Found: M13 Task C10.*
- **Tested on Linux only.** The remote suites run with git 2.39.5, 2.43.0 and 2.56.0 on x86_64
  Linux; macOS and Windows, and git for Windows' `http-backend`, are M12's. *Target: M12.*

## Platforms

- **The filesystem check is by path name only.** A store under `/mnt/` is refused, but NFS, SMB and FUSE mounts, Docker Desktop's and virtiofs bind mounts among them, are neither
  detected nor supported. *Target: M6 (detection by `statfs`).
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
- **The release binaries print an `ld.so` line on glibc 2.34 to 2.38.** On glibc 2.34 to 2.38 hosts
  (Ubuntu 22.04, Debian 12, RHEL 9) every start of the release `swarmops` and `so` binaries prints
  one `ld.so` line on stderr, `weak version 'GLIBC_2.39' not found (required by …)`; exit codes,
  stdout envelopes and the MCP protocol are unaffected, and on glibc 2.39 nothing is printed.
  *Unscheduled: a build against an older glibc, or a static build, would remove it. Found: the
  2026-10-20 review of the aarch64 release build.*

## Performance

- **Part of each lock pass through the binary is unexplained.** In the quiet-window run the lock
  passed every 8.27 ms at store level (about two raw `fdatasync`s, which is what a transaction
  makes) and every 13.75 ms through the binary; the difference appears only with the commits'
  own object writes on the same filesystem. *Target: M1.6's quiet-window runs (docs/perf.md),
  then M2's journal-sync decision, both deferred until after the pilot. Found: M1 Task 7b.*
- **Commit-path costs.** Every
  commit's materialization scans the index tail; and the reflog walks frames past the snapshot
  only to drop them. *Target: M2's measured decision on them (its plan's Task 3), deferred until
  after the pilot. Found: M1 Tasks 2c and 3; deferred again by Task 8.*
- **Compaction rewrites both sorted views under the writer lock.** When the index's unsorted
  tail passes 1,024 records, the writer that holds the lock merges it into the sorted views and
  writes them anew before it lets go, so the writers queued behind it wait for a rewrite that
  grows with the store; one that waits past the 10 s `lock_timeout` fails with the retryable
  `E_BUSY`, and nothing is lost. What the rewrite costs at 100,000 and 1,000,000 records is
  M1.6's compaction bench, deferred until after the pilot. *Target: M6 (the sorted views as
  immutable runs under a manifest, in the next `FORMAT`, so that the lock holds only the
  manifest's swap), or M3's merge outside the lock (its plan's Task 15) if the bench shows a
  stall of 100 ms or more at 100,000 records first. Found: M1.6 Task 7 (the compaction design);
  the 2026-10-20 plan triage.*
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
- **Golden envelopes cover 31 of the 41 error codes.** `error_codes_reach_the_cli` pins the
  envelope of 27, and `remote_error_codes_golden` the four `E_REMOTE_*` codes over smart HTTP and
  ssh (M13); the other ten are in the first test's `NOT_RAISED` list, each with why the test cannot
  raise it (the four codes above among them). *Unscheduled (`E_IO` joined in M1.6, the six
  `E_GIT_*` codes in M3). Found: the 2026-10-16 comparison.*
- **The git remotes' reference pages have no recorded example.** The pages of `clone`,
  `remote add|remove|list`, `fetch`, `push`, `sync` and `sync status`, and of their five MCP tools,
  say "No recorded example yet": no golden step runs them, since they need a remote. *Unscheduled.
  Found: M13 Task C1.*
- **The weekly live run with GitHub needs this host's runners.** The `github-live` job pushes to a
  scratch repository whose own workflow runs on a self-hosted runner of the maintainers' host; with
  that host down, the job fails its 15-minute wait for the workflow. *Documented. Found: M13 Task
  C13.*
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
- **The `git merge-file` oracle pairs lines with Myers.** git 2.43, the declared oracle
  version, has no `--diff-algorithm` for `merge-file` (git 2.44 added it), so the corpus in
  `crates/swarmops-merge/tests/fixtures/merge_file/` records git's merges over its Myers diff,
  while the three-way merge pairs lines with the histogram diff. The cases are built from
  distinct lines, which the two diffs pair alike, and diffy, the other oracle, uses Myers too: a
  merge that only the histogram diff pairs differently is checked by no oracle. *Unscheduled:
  when the declared git moves to 2.44 or later, `generate.sh` adds
  `--diff-algorithm=histogram`. Found: M2 Task 8; M4 Task 3b.*
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
| 4 | Commit latency against the spec's target | target restated without syncs; batching re-evaluated | M2 (its Task 4, after the pilot), "Full fsync is slow" |
| 5 | The recovery walk under the lock | with fsck, and the lock timeout against journal size | M6, "Recovery after a crash walks all of `journal.off`", "No repair tool" |
| 6 | `read_file` reads the whole blob | ranged reads | M6, "`cat` reads the whole object file" (ranged reads: M2) |
| 7 | Crafted `journal.off` entries | a per-entry checksum in the next `FORMAT` | M6, "Crafted `journal.off` entries" |
| 8 | `objects/tmp` leftovers | `gc` | M6, "Temp files of crashed writes" |
| 9 | Wide directories in `ls` | size and depth | M6, "`ls` of a wide directory" (size and depth: M2) |
| 10 | Parked helper threads | the daemon | Unscheduled, "An abandoned lock wait" |
| 11 | `abort()` standing in for `SIGKILL` | a literal `SIGKILL` test | M6, "The crash model" |
| 12 | Second root commits | `commit_raw` | closed in M3: `Repo::commit_raw`, the git import's writer, makes a root changeset whatever refs exist |
| 13 | `--expect` with a ref name | documented | "`--expect` with a ref name" |
| 14 | Tests that read host repositories | none in M1: nothing to judge | M3 and M8 (their Tasks 13 and 11, after the pilot), "No test reads a real repository" |
