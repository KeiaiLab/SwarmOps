# Known limitations

What SwarmOps does not do yet, or does less well than it should, as of M14, and the milestone
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
M0, M1, M1.5, M1.6, M2, M3, M4 and M8 make the first release, 0.1.0; M13 makes 0.2.0, M5
0.3.0 and M6 0.4.0; 0.5.0 adds no milestone: it holds the pilot, the measurements and the first v0.6 fixes; 0.6.0 holds the fixes the measurements asked for, and M14 makes 0.7.0. M1.5, the hardening milestone after M1, closed a few, and M1.6, the code-review follow-ups after it,
closed more. "Deferred
until after the pilot" marks a delivered milestone's task that waits until SwarmOps has been
piloted on a real repository ([pilot.md](pilot.md)): the measurements that need a quiet host,
and the tests that read the maintainers' service repositories. "Unscheduled" means no milestone
has taken the item yet; "documented" means it is the intended behaviour. An RPC server is a
follow-up after v1 (umbrella spec §17), not a milestone, so no entry targets it.

When a change closes an item, it removes the entry here in the same commit (or says what is
left).

## Durability

- **The power-loss model is a model.** The sweeps abort the process at a chosen filesystem call,
  which leaves the kernel's page cache intact, and since M6 a literal `SIGKILL` kills
  `commit --advance` at random points on a real disk, on both formats. A power loss is modelled by
  the images of `crates/swarmops-cli/tests/power_loss.rs`: for one recorded run, what a disk may
  hold after the power fails at each of its steps (unsynced data dropped, cut at 512 B or 4 KiB,
  zero-filled, or with its first sector lost; directory entries that no fsync covered reverted),
  each of which the binary must recover. What stays open: the images come from a model of the
  disk, not a real device, and the recorded run is short (its journal ends at 2,016 B), so a cut
  at 4 KiB is in the model but never differs from dropping there; only the slow variant's bulk
  frame (1,025 changeset records, about 76 KB) reaches past a 4 KiB boundary. *Unscheduled. Found:
  M1 Task 6; final review, question 11; narrowed by M1.5 Task 3, M1.6 Task 4 and M6 Task K1.*
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
  may leave no one to receive the frame's writeback error. A FORMAT 2 frame carries its seq and
  the previous frame's CRC, so a misplaced, stale or lost frame is reported as damage, but an
  append-only journal cannot bring a lost frame back: `repair` reports it and cuts nothing.
  Such a loss also frees an operation number: a frame committed with a warning (its listing
  failed; the warning names its id) and then lost this way gives its number to the next write.
  An operation's id carries its frame's CRC-32 beside the number, so an `op undo` of the lost
  operation's id is `E_OP_NOT_FOUND` (`reason: checksum`) and never acts on the other operation,
  unless the two frames' CRC-32s are equal: one chance in 2^32 for each number taken back (the
  M15 design §2). `repair` reports the lost frame. The tail check runs in a committed read too
  (M8's git export, M11's `serve`), so a double fault there aborts a reader process as it does a
  writer.
  *Documented (the M6 design, section 12; the v0.6.0
  design, D7). Found: M1.5 Task 2; narrowed by M6; the operation number from the M4
  consolidated review (issue #11), closed by operation ids in M15 but for the CRC-32 collision.*
- **A failed directory fsync in the object store is retried, not taken back.**
  `FsObjectStore::flush` keeps a fan-out directory whose fsync failed and fsyncs it again on a
  later flush; the commit whose flush failed fails. The retry is for an fsync that never ran (the
  directory could not be opened: `EMFILE`, `ENOMEM`, `EACCES`). A later success cannot make
  good a failed one, and on the filesystems the store supports it need not: there a failed
  directory fsync is fail-stop (storage spec §2 rule 5), so every later sync fails too; the
  order checker and the fsync-failure model assume the same. On a filesystem that is not
  fail-stop, a later fsync's success proves nothing about the earlier links, and a later commit
  may name an object whose link a power loss then takes back. Since M6 the store refuses FUSE,
  NFS and SMB by `statfs`, but ext2 and ext4 without a journal share ext4's magic and pass.
  *Documented. Found: M1.5 Task 2; the 2026-10-16 expert review; narrowed by M6.*
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
  level). A ticket or FIFO hand-off on top of `flock` is the next step. *Unscheduled (the M6
  design, decision D3, kept the bounded wait). Found: M1 Task 7c; final review, question 3.*
- **Landing under a burst queues on the lock.** Since v0.6.0 a land whose target moved after it
  read it is made anew on the new head by the writer that holds the lock (metalog merge: its delta
  where no path overlaps, overlapping regular files of at most 1 MiB merged three-way), so a burst
  no longer costs retries: at 50 processes each landing once every 2 s, every land landed, 25 a
  second, with 0 % retries and nothing lost, against 116–133 % retries in v0.5.0. What remains is
  the wait for the lock, which each publish holds for its move and its merge: the p99 is
  579.5–594.6 ms at 50 processes and 207–216 ms at 32. The umbrella spec's target (§15) for 50
  landing processes is 20 lands/s or more, a p99 under 500 ms, a retry rate under 5 % and nothing
  lost; the p99 is the one part missed ([perf.md](perf.md#v060--2026-10-28)). A land still goes
  back to its lander, and is merged again outside the lock, when a path overlaps in a way the
  lock's merge does not take (anything but a regular file of at most 1 MiB on every side, or a
  conflict), or the target moved off the first-parent line through the head the land read, or
  more than 256 changesets along it; a land with `expect_target` (`ws land --check`) never
  relocates. An agent that meets
  `E_CAS_RETRY_EXHAUSTED` lands again with the same key, or asks for more attempts. Each land
  takes the lock twice, for its `commit` and for its `land`, about 25 ms together, so at 25 lands
  a second the lock is held about 62 % of the time and the tail is the queue's wait, not the hold
  (on the bench disk in M14's window the two took about 42 ms at the median, the lock was held
  99.6 % of the run and the p99 rose to 2.5–2.7 s, for v0.6.0's code as for M14's;
  [perf.md](perf.md#v070--2026-10-29)). *Open: G4's p99 (goals.md). Found: the 2026-10-20 plan
  triage; narrowed by v0.6.0; its cause measured in M14.*
- **Recovery after a crash walks all of a FORMAT 1 `journal.off` under the lock.** Before it cuts
  or lists anything, the first writer after a crash checks every entry of a FORMAT 1 store's
  `journal.off`. On a long journal that walk could approach `lock_timeout` for the writers queued
  behind it. A FORMAT 2 entry carries its own checksum, so recovery checks only the entries from
  the checkpoint on; `swarmops upgrade` moves a store to FORMAT 2. *Documented (M6). Found: M1
  fuzzing review; final review, question 5.*
- **`repair` and `upgrade` hold the lock for the whole journal.** Rebuilding the index and
  upgrading a store read every frame under `meta/lock`, so a writer that waits for them past the
  10 s `lock_timeout` gets the retryable `E_BUSY`; an upgrade of 200,000 changesets took 2.79 s on
  a loaded host. Streaming them in steps is the next step if a store takes longer. *Unscheduled.
  Found: the M6 plan, risk R-1.*
- **An abandoned lock wait parks a thread.** A wait that times out leaves its helper thread in
  the kernel's queue until the holder lets go, and a hung holder leaves one parked thread per
  abandoned wait. That costs nothing in a short-lived CLI process; a long-lived one (the stdio
  MCP server is one, a network server after v1 would be another) needs a way out. *Unscheduled
  (it was M10's; an RPC server is a follow-up after v1, umbrella spec §17). Found: M1 Task 7c;
  final review, question 10.*
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
  back, at the cost of a wait behind the writer that holds the lock. So does `op log`, the one
  local reader that does, so that a number it lists is never given to another operation.
  *Documented (storage spec §3). Found: M1 Task 2c; final review, question 2.*
- **A committed read fails on a read-only filesystem.** `MetaStore::read_committed` (the git
  export, `push`, `serve`, `op log`) opens `meta/lock` for writing before its tail check, as a
  writer does, so on a read-only mount it fails with `E_IO` at `open meta/lock`, even where the
  tail needs no repair. The lock-free reads work there. *Unscheduled. Found: M8 area A review.*
- **A commit acknowledged with a warning can vanish for a moment.** When a tail check
  writes unlisted frames afresh after a failed sync (storage spec §2 rule 3 (c)), a reader that
  reads between the cut and the rewrite does not see them: a changeset whose commit was
  acknowledged with a warning can be `E_REV_NOT_FOUND` for that moment, and a retry finds it.
  It takes a failed listing and then a failed sync. *Documented (storage spec §3). Found: M1.5
  Task 2 review.*

## Reads and response sizes

- **`ls` of a wide directory decodes its whole tree.** Paging limits what is returned, not what
  is read: a page decodes the whole tree object of the listed directory, and at a `depth` of
  every directory it enters, however few of their entries it lists. Bounded reads come later.
  (Sizes and depths came in M2, and the default page was measured again for them: 250 entries.)
  A tree object is decoded whole within the 512 MiB object bound. *Unscheduled (a streaming tree
  decode is a codec path of its own, the M6 design, section 5). Found: M1 Task 5; final review,
  question 9.*
- **A page cursor does not pin the revisions it pages.** The `cursor` of `diff` and `read_tree`,
  and `read_file`'s `next_offset`, say where the next page starts, not which revisions it pages:
  an agent that pages through a diff, a directory or a file at `main` while another agent lands
  gets its next page from the new revision, with no error. Page with the full ids the first page
  resolved (a diff's `from` and `to`, the envelope's `meta.resolved`), as the request types and
  the MCP tool descriptions say. *Documented (issue #4 item 3; a cursor that carries the ids and
  refuses a moved page is unscheduled). Found: M2's consolidated review.*
- **A snapshot reads every ref file**: cheap with a few refs, linear in their number. Each
  request reads in one snapshot of the metadata (a commit, `cat`, `ls`, `show` and `log` take one
  each, a ref move one besides its write), so a request pays it once. A FORMAT 1 store holds
  `main` and `release/*` alone, and a FORMAT 2 store branches and tags too; should they grow past
  dozens, a snapshot's reading them calls for a validated lazy read, a per-handle map of refs or
  a packed refs file (a FORMAT change). The `read_snapshot` bench measures it: 1.43 ms p50 and
  2.75 ms p99 with 256 refs, about 5.4 µs a ref, over G3's 1 ms ([perf.md](perf.md#v060--2026-10-28)). *Unscheduled: when refs grow
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
  its retry records a second changeset, or, as a `commit --advance` given `--parent` and retried a
  second or more after a first attempt that published, nothing: the ref names that attempt, so the
  retry fails with `E_CAS_MISMATCH`, whose `actual` may be the agent's own change; `swarmops show
  <actual>` gives its `agent` and `run_id` metadata, to check before applying the edits again on
  top of it. Without `--parent` that retry builds on the ref's head, the first attempt, so its puts
  change nothing (`E_EMPTY_COMMIT`), or a delete names a path no longer there
  (`E_PATH_NOT_FOUND`). *Target: M9
  (an index of commit keys, if measurements ask for one). Found: M1 final fix pass; the `--advance`
  case, the M1.6 review; narrowed by M4 Task 8.*
- **A keyed land searches first parents, not a key index.** A land with a key first looks for an
  earlier land of the same key on the target's first-parent chain, from its head down to the
  land's base, so a land made on an old base walks the changesets landed since. A key it does not
  find there is looked for on every other `main`, `release/<x>` and branch ref (not tags or
  remote-tracking refs), down to the same base generation, and a key that landed on another ref
  is `E_INVALID_ARG` with `details.target` (the M15 design §6). Three bounds stay. The walk of
  another ref ends at the first changeset at or below the base's generation, that changeset read:
  a key whose land is further below is not found, unless that land is the ref's head, or the
  first changeset of its chain at or below that generation. Two lands with one key onto different
  targets at the same time can both land: the search is outside the store's lock. The cost grows
  with the refs a land has to walk. *Target: M9 (an index of commit keys, if measurements ask for
  one; it closes all three bounds). Found: M4 Task 12's hand-off; the other-ref search, M15
  S5a.*
- **`undo` does not take back a land on `main`, nor one that later lands built on.** `main`
  moves only forward, so the undo of a land on it is `E_REF_PROTECTED` (the M15 design §4). On a
  `release/<name>` ref an undo moves refs back only while they still name what the operation set;
  under later lands that is `E_CAS_MISMATCH`. No `revert` lands the inverse change: it is written
  and landed by hand. *Unscheduled. Found: M4 Decision 10.*
- **No hooks or annotations after a land.** A land records no post-land verification, and
  `land`'s result has no `verification` field: annotations need a journal record the current
  `FORMAT` lacks. Agents check a landed tree with `read_file` and their own runs. *Target: M7, on
  a `FORMAT 3` (M4 Decision 11; the M6 design, decision D6). Found: M4 Task 12's hand-off.*
- **A merge conflicts on a line both sides changed.** The three-way text merge (diff3) works
  on whole lines, so two edits to different parts of one line conflict. *Unscheduled. Found:
  the 2026-10-27 completeness report.*
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
- **Only `main` is protected.** `ref delete main`, an `op undo` that would delete `main`, and a
  `ref set` or `op undo` that would move `main` off its first-parent history fail with
  `E_REF_PROTECTED` in every format, with nothing written (the M15 design §4). Any other ref,
  `release/<x>`, a tag, a branch, can be deleted or moved back by whoever gives its value to
  `--expect`, and `op undo` of its move takes it back: the published procedures delete
  `release/*` refs as their normal flow, and a FORMAT 1 store takes no other name, so protecting
  them would leave it no deletable ref. No setting names other protected refs.
  *Unscheduled: a configured set of protected refs (the v0.6.0 design, decision 8). Found: the
  2026-10-27 completeness report (B23).*
- **A sync or a git import can still move `main` back.** The forward-only rule guards `ref set`
  and `op undo`; `sync`'s own move of `main` to the remote's head, and `git import --into main`,
  are not judged by it. *Unscheduled: with the configured set of protected refs. Found: the M15
  design §4.*
- **A commit holds its contents in memory.** All of them, up to the 1 GiB they may hold
  together, and up to about two to three times that while their objects are encoded in
  parallel, besides the request's text (at most 64 MiB). `blob put` (`blob_put`) and
  `{"blob": "<id>"}` take large contents out of the commit request. *Documented (bounded by the
  limits since M2). Found: the 2026-10-16 code review, issue 10.*
- **Branches and tags stay in the store.** `git export`, `git import`, `push` and `sync` take
  `main` and `release/<name>` only: mapping a FORMAT 2 branch or tag to a git ref is M14's work.
  Names that differ only in case (`Feature` and `feature`) are two refs here, and would collide in
  git's loose refs on a case-insensitive volume. *Target: M14. Found: the M6 design, section 2.5;
  the M6 plan, risk R-7.*

## Verification and repair

- **Derived values about objects nothing names stay.** `gc` removes the values of a built-in
  derivation's older versions, and `repair` the damaged ones, but values of the current version
  stay, whatever they are about, and so do the versions of a derivation a caller registered,
  whose name `gc` does not know. *Unscheduled. Found: M2 Task 5; narrowed by M6 Task G1.*
- **`gc` collects no object.** Objects no changeset reaches (those of a commit that died before
  its frame) are counted, as `orphan_objects`, and stay: collecting them needs a freshness rule on
  the put path. *Unscheduled (the M6 design, decision D5). Found: the M6 design, section 8.*
- **`verify` holds every object id in memory.** Since M6 its metadata checks read the index and
  the sorted views in blocks of 4,096 entries, but its object phase holds the set of object ids,
  and a deep check of a store with derived values also holds the first-parent forest (a few words
  per changeset) and every `last_modified` value the store holds. *Unscheduled. Found: M1 Task 3;
  final review; narrowed by M6.*
- **A missing `journal.off` opens as `E_IO`** (`details.store_file: meta/journal.off`), not
  `E_STORE_CORRUPT`; `swarmops repair` writes it again. *Unscheduled. Found: M1 final review.*
- **`verify --deep` skips object files that are symlinks**, because the object listing ignores
  them; SwarmOps never writes one. *Unscheduled. Found: M1 final review.*
- **An upgraded store fails older handles as damaged.** After `swarmops upgrade`, an older
  release, and a handle this release opened as FORMAT 1 before the upgrade, fail their next read
  or write with `E_STORE_CORRUPT` and a hint to verify: the FORMAT 1 checkpoint is poisoned so
  that no older writer appends to the old journal. Opening the store again with this release
  reads FORMAT 2. *Documented (the M6 design, section 3). Found: the M6 plan, risk R-3.*

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
- **Crafted FORMAT 1 `journal.off` entries can mis-number a read.** In a FORMAT 1 store, entries
  crafted to agree with each other (one pointing into a payload whose bytes read as a frame header
  ending where the next entry starts), three or more links before the end, pass the two links
  recovery and readers check: a read that starts past them numbers its frames by them, and
  `frame_count` counts them. That is outside the crash model, and `verify`, which walks every
  listed frame, reports it. No frame is lost: before recovery cuts or lists anything it checks
  every listed frame ("Recovery after a crash walks all of a FORMAT 1 `journal.off`" above), which
  the `journal_recovery` fuzz target asserts on every input. A FORMAT 2 entry carries the frame's
  payload CRC and a checksum of its own, which close this. *Documented (M6). Found: M1 fuzzing;
  final review, question 7.*
- **A zeroed FORMAT 1 `journal.off` entry deep in the chain** (three or more links before the
  end) is outside the crash model: recovery checks the last two links, and `verify` finds the
  rest. FORMAT 2 refuses a zeroed entry anywhere but at the end. *Documented (M6). Found: M1
  fuzzing review.*
- **A crafted index's generations steer history queries.** A read checks what the changeset
  index says where it can do so cheaply: a sorted view's entry must name a record that holds its
  key, and a record's parents must start where the record before it leaves off, or the read
  fails with `E_STORE_CORRUPT`; and every first-parent walk takes its parents from the changeset
  objects, which their hashes bind to their ids, so that no walk drops a committed changeset or
  loops. A record's generation below the checkpoint or under the sorted views is read as it is,
  but the walks that order or bound by one check it against the generations of the parents its object names (1 + the largest, 1 for a root), or fail with
  `E_STORE_CORRUPT`: each changeset a merge base's walk steps from, each touch `log --path`
  orders, and where a no of `is_ancestor` stopped; and a revision's `~N` takes no `None` from a
  generation, reaching a root or a skip list's checked end instead. Each check covers one link:
  a generation that agrees with its parents' crafted ones passes, as does a no of
  `is_ancestor` from the two generations alone, no step taken, and a keyed land's walk of its
  target, which stops at a generation. Such a `meta/idx/gen.col` can still have a land take an
  older common ancestor than the newest for its merge base, or `log --path` list its changesets
  out of order; a record's delta hash is taken as it is. Every changeset such a walk reaches is on
  the objects' first-parent chain, and `verify` reports the damage, which `swarmops repair`
  rebuilds from the journal. Checking a generation fully on every read takes the journal, or
  every generation down to a root. *Documented (M6); narrowed to one link by issue #16 item 5's fix.
  Found: 2026-10-20, when the read path came to check the index.*
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
  the CLI and the stdio MCP server. Through MCP a relative path, a path that is the server's own
  stdin or stdout, and anything that is not a regular file (a FIFO, a socket, a device or a
  directory, or a symlink to one) are refused before anything is read. `swarmops serve` (M14)
  takes no request from a git client as JSON, and the guard its operations run through refuses
  `{"file"}` content anyway (`E_INVALID_ARG` `file_content_refused`); M11's `serve --stdio`,
  which will take requests from another host, is to reuse that guard. *Target: M11. Found: M1
  Task 4b; M1.5 Task 9 review (M11's `serve`); closed for `serve`'s git paths by M14.*
- **`E_REF_NOT_FOUND` is not raised yet.** No operation looks a ref up by name; the code is in the
  schema so that an agent can match it before any operation raises it. `E_AUTH` and
  `E_POLICY_DENIED`, listed here before, are raised by `swarmops serve` since M14. *Documented.
  Found: the 2026-10-16 comparison; M1.5 Task 9 review.*
- **One temp-file error names a host path.** When 65,536 temp-file names in a row are taken in
  a directory of the store, the `tempfile` crate gives up with `too many temporary files exist`
  and the directory's absolute path, which an `E_IO` then carries; the names are random, so it
  is not met in practice. The other errors that name the store's own path answer for the path a
  command was given (operations spec §0). `swarmops serve` (M14) masks the store's path, its
  `--root` and `serve.git` as `<path>` in every text it sends a client, so neither reaches a git
  client; M11's server is to do the same. *Target: M11. Found: the M1.6 consolidated review
  (error contract); narrowed by M14.*
- **git's own lines on a push's side band are not masked.** `serve` masks what it and its hook
  write, and git's stderr; receive-pack's side band travels on git's stdout, which `serve` passes
  on as it is. One such line names a host path: when the hook's program is gone (the serving
  binary replaced or removed), `sh` names the hook file `<store>/git/serve-hooks/proc-receive`,
  and the client prints it as a `remote:` line. *Unscheduled. Found: M14 Task S3.*
- **A pushed object `serve` refused can be fetched over protocol v2.** git moves a push's objects
  out of its quarantine before the `proc-receive` hook decides, so a refused command's objects
  stay in `serve.git`, unreferenced, until `gc --auto` prunes them after git's grace period.
  `uploadpack.allowAnySHA1InWant=false` keeps them from protocol v0 clients, but protocol v2's
  `fetch` takes any object the repository holds as a want (gitprotocol-v2), so a v2 client that
  knows the id fetches it. Every caller of `upload-pack` holds the store's `read` grant, which
  covers the whole store, so no grant is crossed; but a caller with `read` alone reaches
  receive-pack, and each of its pushes may leave up to `MAX_PUSH_BYTES` (2 GiB) of objects in
  `serve.git` before the hook refuses each ref. *Open: receive-pack into an object directory of
  its own, or the decision moved into `pre-receive` (the M14 design's risk R-2). Found: M14 Task
  S3.*
- **A land through `refs/land/<target>` does not move the client's tracking branch.** The landed
  commit is a squash the server made, so the client holds none of its objects, and git would
  refuse to move `origin/<target>` onto it (`update_ref failed … nonexistent object`, git 2.43.0).
  The hook names the landed commit on a `remote:` line (`landed onto main as <40 hex>: git fetch
  brings it in`), and the next `git fetch` or `git pull` moves `origin/<target>`. *Documented (the
  M14 design §7). Found: M14 Task L1.*
- **A refused ref's `ng` line carries the code and the message, not `details.reason`.** A client
  reads `reserved`, `not_served` and `annotated_tag` as words of the message ("reserves", "is not
  served", "annotated tag"). *Unscheduled. Found: M14 Task S4.*
- **A land attempt checks access before it starts, not under the lock.** A push to
  `refs/land/<target>` reads `access.toml` again before each attempt and stops with the
  connection's refusal when the `land` grant is gone, writing nothing (the M15 design §6). A
  grant removed after an attempt's check and before its publish does not stop that attempt: it
  can still land, lose its race or conflict. *Unscheduled (the M15 design §6 leaves it out of
  scope). Found: M15 Task S5b.*
- **The first connection to a large store exports its whole history.** `serve` exports every
  served ref into `serve.git` before it advertises; a store whose history no export has reached
  yet pays that once, as loose objects: on 12,000 commits the first clone took 1,195.6 s,
  against 0.36 s from git's own server ([perf.md](perf.md#v070--2026-10-29)). The connections
  after it export only what moved. *Unscheduled (one pack per export; the M14 design's risk R-3). Found: M14 Task Q1.*
- **A request takes the memory its type holds.** The MCP server reads of a message line, at
  most 64 MiB, only the members it serves, and a tool's arguments only as their request type,
  so what else a line holds costs it nothing beyond the line's bytes (operations spec §0). A
  request type's own fields are built in full, though: a long list of short strings (`blob_has`
  ids) or a map of small entries (`land` resolutions) takes up to about 15 times its text, near
  1 GiB for a line at the cap, before the operation refuses what is not an id or a resolution;
  the CLI reads `--changes` and `--resolve` the same way. *Unscheduled. Found: the 2026-10-20
  security audit.*
- **No web view.** History, trees and diffs are read through the CLI and the MCP server; no
  page serves them to a browser. *Unscheduled. Found: the 2026-10-27 completeness report.*

## Git import and export

- **`created_at` cannot show a time before 1970.** A changeset keeps its signatures' times as
  signed seconds (storage spec §1, §7), but the API's `created_at` in `show` and
  `log`, unsigned milliseconds derived from the committer's time, reports 0 for any time before
  1970. The git import keeps such commits as they are, without loss (the M3 plan's decision 4),
  and `show`'s `signatures` give their exact times. *Documented (storage spec §7). Found: M1
  Task 4a.*
- **Real repositories are read weekly only, and nothing is landed on them.** The default gate
  reads fixtures that real git made. `host_repos::every_mirror_round_trips`, behind
  `SWARMOPS_HOST_REPOS=1` and run by the weekly `host repositories` job, round-trips bare mirrors
  of this host's repositories: import, deep verify, export, `git fsck --strict`, import of the
  export. It lands no change on them, and passes over a partial clone, whose objects are not all
  on the host. *Target: M8's round trip of them with lands (its plan's Task 11), in the pilot.
  Found: M1, final review, question 14; narrowed by the host-repository tests (v0.5.0).*
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
- **An export without its memo key reads the whole history below its head.** With the store's
  memo key (32 bytes the CLI keeps outside the store, in
  `$XDG_STATE_HOME/swarmops/memo-keys/<repo_id>.key`, made on first use), the export trusts an
  export oid memo whose keyed MAC checks (`git_oid_mac`, `git_tree_oid_mac`, `git_blob_oid_mac`)
  and reads nothing below it: an export of one commit over 2,000 imported ones reads 9 of the
  store's objects, not 2,015. Without the key (no state directory, or a library caller that
  passes none) it derives the export oid of each changeset below the head from the objects, as
  before: about 20 µs a changeset, 0.25 s for one new commit above 12,000 imported ones. The
  threat model: whoever can write the store but not the user's state directory cannot steer an
  export through memos (a memo under the key whose MAC fails is `E_STORE_CORRUPT`, never
  re-derived in silence); whoever can write both can. Below a trusted memo the export reads
  nothing, so an object missing or damaged there no longer fails it: `verify --deep` finds it.
  The key is a cache key, outside any backup ([backup and
  restore](operate/backup-and-restore.md#the-git-mirror-and-the-remotes-files)): a store restored
  without it derives its history once, then exports incrementally. *Documented (the v0.6.0
  design, decision 2). Found: the 2026-10-20 security review's re-verification of the git
  export; narrowed by v0.6.0.*
- **An import onto a target without its memo key reads the head's whole tree, and onto a native
  head the whole history below it.** With the memo key (the export's entry above) the import
  takes a subtree's, a blob's or a parent's git oid from a memo whose keyed MAC checks and reads
  nothing below it: an import of one commit onto a native head 2,000 changesets deep reads 8 of
  the store's objects, not 2,008. Without it, the import reads and hashes each of the head's
  trees and blobs before they seed its conversion, and where the head is native, or holds a git
  commit the source does not go on from, derives the head's export oid with the whole history
  below it: 0.31 to 0.36 s onto a native head above 12,000 imported commits, about 25 µs a
  changeset, at a load of about 20. An object missing or damaged in what it reads fails the
  import (`E_STORE_CORRUPT`, which `verify` lists) until it is restored. The threat model is the
  export's. *Documented (the v0.6.0 design, decision 2). Found: the 2026-10-20 security review's
  re-verification of the git export, and of the git import; narrowed by v0.6.0.*
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
  earlier import took with a loss reports `lossy_total` 0 and skips it rather than refusing, as
  `GitImportReq.lossy` and `git import`'s help say. *Documented (issue #8 item 3; refusing or
  listing such a commit is unscheduled). Found: M3's consolidated review.*
- **A refused import does not say how far it got.** An import stops before the frame that would
  hold a commit it refuses (`E_GIT_IMPORT_LOSSY`, `E_GIT_UNSUPPORTED`, `E_GIT_READ` for an
  object it cannot read, `E_TOO_LARGE` for one past the store's object size, or `E_INTERNAL` for
  a fault of the conversion), and the frames before it, 8,192 commits each, stay recorded with their
  external ids; but the error carries no `details.imported` or `details.last_commit`, which only a
  store failure and a failure after a frame's commit point report. An `E_CAS_MISMATCH` after the
  last frame, which recorded every commit, carries neither. The same import run again, once
  the refusal is dealt with, continues after those frames. *Unscheduled: the refusal would carry
  the run's progress, as a store failure's error does. Found: M3's import review (issue #7 item
  2, issue #8 item 2).*

## Git remotes

- **A sync grows with the history.** A push and a fetch no longer do: with the memo key, a
  one-commit push costs 1.02 times on 12,000 commits what it does on 1,000 (3.7 to 3.9 times
  before v0.6.0), and with the mirror's commit-graph (v0.7.0) a one-commit fetch costs 1.34 times
  (1.65 in v0.6.0), against the roadmap's 1.5. A sync of one land on 12,000 commits, the first
  after a clone, took 49.6 s, against the roadmap's 1 s (52.9 s in v0.6.0, 8.76 s in M13's loaded
  run): its land step (10.48 s) and its push (10.13 s) grow with the history the first time after
  a clone, which the first export oid derive and the first keyed export's memos would explain
  (inferred from the code, not profiled), and a second sync takes 0.213 s
  ([perf.md](perf.md#v070--2026-10-29)). *Open: G17's sync (goals.md). Found: the M13 design's
  risks; measured by M13 Task C11, v0.6.0 and v0.7.0.*
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
- **A remote whose history holds an object git's fsck refuses cannot be fetched.** git's fsck
  checks every object a fetch receives (`fetch.fsckObjects`, `transfer.fsckObjects`) and the
  fetch's tips must reach only objects the mirror holds; an error-level finding, or a missing
  object, fails the fetch with `E_REMOTE_REJECTED` `details.reason: "bad_objects"`, nothing
  imported and no tracking ref moved, and every later fetch fails the same way until the remote's
  history changes. There is no `fsck.<id>` override and no way to skip the check. *Documented
  (the v0.6.0 design, decision 9). Found: the 2026-10-27 completeness report (A3).*
- **A damaged mirror object that only a non-followed branch or tag reaches is not caught.** A
  fetch reads, and checks against its id, every object the followed branch reaches before it
  moves any tracking ref; the other branches and the tags are checked by git's fsck on receive
  and by the closure check (every object is there), not by an id check. A mirror object that
  holds other bytes and sits under such a ref stays unnoticed until the followed branch reaches
  it. One that the followed branch reaches fails every fetch with
  `E_GIT_READ` (`details.reason: "corrupt"`, `details.object`) until `git/mirror.git` is moved
  aside; the next fetch makes it again. *Documented (the M15 design §3). Found: the 2026-10-27
  completeness report (D45).*
- **A server that cuts git's first HTTP response inside its headers looks like an empty remote.**
  git then falls back to the old "dumb" HTTP protocol, finds no refs and succeeds: the fetch reports
  no branches and prunes the mirror's tracking refs, and a clone ends with no `main`. Nothing is
  forced (a later push leases on the pruned value and fails), and the next good fetch restores
  them. *Unscheduled. Found: M13 Task C10.*
- **Tested on Linux only.** The remote suites ran with git 2.43.0 on x86_64 Linux; the weekly run
  adds git 2.39.5 and 2.56.0 once its host has curl's headers. macOS and Windows, and git for
  Windows' `http-backend`, are M12's. *Target: M12.*

## Workspaces

- **A power cut can leave a workspace's files torn.** Materialised files are not synced: the store
  holds them durably, and `workspace.json`, the registry and the intent records are synced. After a
  power cut a file written just before it can be torn; `ws status` shows it as an edit, and
  `ws discard <path>` restores it from the head. *Documented (the M5 design's decision 4). Found:
  M5 design.*
- **A checked land takes two lock passes.** `ws land --check` commits the rebased changeset, runs
  the check outside the lock, then publishes only if the target has not moved: on a busy target a
  check of minutes may rebase and run again until `--max-attempts` (`E_CAS_RETRY_EXHAUSTED`,
  retryable), and each lost race leaves one unlanded changeset in the journal. `--mode merge` with
  `--check` lands onto a moved target, a weaker promise. Checked lands are not measured yet.
  *Documented (the M5 design's decision 1). Found: M5 design, risk R1.*
- **`ws:<name>` is a label, not a revision.** No read resolves it: it is only the `to` label of
  `ws diff`'s result. A read takes the workspace's head by its id, which `ws info` gives, and
  never sees edits that are not committed; `ws status` and `ws diff` show them. *Unscheduled.
  Found: M5 design, decision 5; the 2026-10-27 completeness report.*
- **No agent transcripts.** A changeset records its `agent` and `run_id`, not the session or the
  prompts that made it; a transcript stays wherever the agent's tool keeps it. *Unscheduled.
  Found: the 2026-10-27 completeness report.*
- **Names that differ only in Unicode normalisation (NFC and NFD) are two paths.** The case check
  of a case-insensitive volume folds ASCII only, and a volume that normalises names, such as
  macOS's, can make two paths of the tree one file. *Target: M12. Found: M5 design.*
- **On Windows, a timed-out check kills the check's process only.** On Unix the check runs in a
  process group of its own and a timeout kills the group; Windows has no such group kill here, so
  the check's children may outlive it. Windows is not built yet. *Target: M12. Found: M5 design,
  risk R5.*
- **Claude Code does not remove a subagent's workspace.** When a subagent or a workflow agent
  finishes, Claude Code keeps its worktree and runs no `WorktreeRemove`, so the workspaces its hook
  made stay until `so ws delete`; `so ws list --check` lists them. *Documented (Claude Code's hooks
  reference, read 2026-10-22). Found: M5 Task W9.*
- **A failed bootstrap is not run again.** When the repository's bootstrap command fails, the hook
  fails and Claude Code may retry it: the retry finds the workspace (`created: false`) and returns
  its path without running the bootstrap. *Documented. Found: M5 Task W9 review.*
- **Rough edges the reviews left.** A `ws land` cut while it publishes and run again can answer
  `E_NOTHING_TO_LAND`, or `E_CAS_RETRY_EXHAUSTED` with a smaller `--max-attempts`, though the land
  is durable and the workspace stands on it; a cut unchecked land run again with `--check` replays
  the unchecked land; a `ws land -m` cut inside its own commit asks for `ws commit` first; the
  `timeout_s` of `.swarmops/workspaces.json`'s `check` is not read (the request's, 900 s by
  default, applies), and a `--check-timeout` too large for the clock aborts the command;
  `blob get` takes `--max-bytes 0` as one blob, and caps neither the ids nor the bytes it holds.
  *Unscheduled. Found: M5 Tasks W3, W7 and W10 reviews.*
- **An older sparse workspace lacks the ignore files above its prefixes.** A sparse workspace
  holds the `.gitignore` and `.swarmignore` files of the directories above its prefixes, so its
  walk ignores what a workspace of the whole tree does. One made before that, or an ignore file
  the base gained after the workspace was made (`ws rebase` and `ws land` add no prefix), is not
  on disk: `ws commit` and `ws land` refuse build output outside the prefixes that only such a
  file names (`E_WS_OUTSIDE_PREFIXES`), and following the hint writes the ignore file with the
  path. A directory off the prefixes whose contents alone are ignored (`build/` holding only
  `*.o`) is still refused as `build`; after the hint, nothing under it is committed.
  *Documented (the M15 design §5a). Found: M15 Task S4b; narrowed by M15 Task S4b3.*
- **A new path materialises only when it is on disk with a parent.** A commit no longer widens the
  prefixes; `ws materialize` does, and takes a path the tree lacks only when it is on disk and its
  parent is the root, a directory of the tree or a directory above a prefix: only the ignore files
  above it that the workspace lacks are written, and the next commit stores it. A path absent from
  disk (a typo) or without such a parent (`nope/deeper/x` where the tree has no `nope`) is
  `E_PATH_NOT_FOUND`: materialise its topmost new directory instead, the path the refusal's hint
  names. *Documented (the M15 design §5a). Found: M15 Task S4b; narrowed by M15 Task S4b3.*
- **A `.worktreeinclude` copy off the prefixes can block a sparse workspace.** A new workspace
  gets the copies (`.env` and the like, which the tree does not hold) at their paths whatever its
  prefixes. One an ignore file of the tree names (the root `.gitignore` naming `.env`), at the
  root or in a directory above a prefix, is ignored, as in a workspace of the whole tree. Any
  other copy outside the prefixes is an edit outside them: `ws commit`, `ws land`,
  `ws delete --rescue` and the `worktree-remove` hook refuse it (`E_WS_OUTSIDE_PREFIXES`), the
  directory and its edits kept. A copy inside another directory is refused as that directory
  whatever the ignore files say (`config` for `config/master.key`, when the tree has `config/`):
  `ws materialize config` clears it, and an ignore file naming the copy keeps it out of the
  commit. For a copy no ignore file names, following the hint makes the next commit store it,
  which may be a secret, in the store for good ([`gc` collects no
  object](#verification-and-repair)): delete the copy or name it in an ignore file first. Then
  run `ws delete --rescue <name>` for a workspace the hook could not remove.
  *Documented (the M15 design §5a). Found: M15 Task S4b review; narrowed by M15 Tasks S4b2 and
  S4b3.*
- **A tree path outside the prefixes the agent wrote first loops the hints.** When the agent
  writes a file at a path the tree holds outside the prefixes, or at an ignore file above them
  the workspace does not hold yet, the refusal's hint `ws materialize <path>` is `E_WS_DIRTY`,
  whose hint (`ws commit`, `ws discard`) is refused again. Move the file aside, materialise the
  path, then put the edit back. No command drops a prefix with nothing to write (a deleted
  copy's `.env`); it stays and does no harm. *Unscheduled (the M15 design §5a leaves it out of
  scope). Found: M15 Task S4b3 design.*
- **An older binary drops the edits the tree check refuses.** A build without the tree check does
  not compare a sparse workspace's tree with the disk: in a workspace whose ignore files above the
  prefixes are on disk (every sparse workspace this release makes), its `ws commit`, `ws land` and
  `ws delete --rescue` leave out, without refusing, an edit to a tree path outside the prefixes
  that an ignore file names and an edit under an ignore file the base gained above the prefixes.
  Use this release's binary on such workspaces. *Documented (the M15 design §5a). Found: M15 Task
  S4b3 design review.*
- **A plain `ws delete` drops the edits outside a sparse workspace's prefixes.** Without
  `--rescue`, `ws delete` refuses an edit under the prefixes (`E_WS_DIRTY`), but an edit outside
  them, which `ws delete --rescue` refuses, goes with the directory. *Unscheduled (the M15 design
  §5a leaves it out of scope). Found: M15 Task S4b3 review.*
- **A command that scans a sparse workspace can fail on the tree check.** `ws status`, `ws diff`,
  `ws discard`, `ws rebase` and a plain `ws delete` scan the workspace as a commit does, the tree
  check included, though they refuse nothing it finds: an lstat error on a tree path off the
  prefixes fails them with `E_IO`, and leaves out the `status_after` page other commands attach.
  *Unscheduled. Found: M15 Task S4b3 review.*
- **`ws status` lists no change in a sparse workspace that a commit refuses.** `ws status` lists
  the changes under the prefixes only, so its `changes` list can be empty while `ws commit` and
  `ws land` refuse an entry outside them; the refusal's `details.paths` names those entries.
  *Documented (the M15 design §5 leaves the display out of scope). Found: M15 Task S4b review.*
- **`ws land` checks a stale base only against its own target, and only so far.** `E_WS_STALE`
  needs the workspace's base gone from its target's history and an operation in the log that moved
  the target to it. A land onto another `--target` is not checked, and neither are `ws commit`,
  `ws status` and `ws rebase`; a target whose first-parent history is longer than 10,000 commits
  (`BEHIND_CAP`), or a base with no such operation among the last 10,000, lands as it did before
  the check. A land cut short whose key the journal's last 10,000 operations or the target's last
  10,000 commits cannot place keeps its pending record: `ws land` and `ws rebase` both refuse,
  and `ws delete <name> --rescue` is the way out. `ws rebase` of a stale workspace takes no
  snapshot of the edits it moves onto the new head, and no test kills a land in the middle of
  the check and the update (the M15 design's third SIGKILL acceptance). *Unscheduled (the M15
  design §5 leaves them out of scope). Found: M15 Task S4c.*
- **A target that gains a reserved path after the check is not refused.** `ws land` and `ws rebase`
  check the target's paths for names the files cannot hold (`E_WS_UNPORTABLE`) before the store
  call. A target that gains a path with a component named `.swarmws` between that check and the
  store's call is landed or rebased onto, and the materialisation then refuses it with the pending
  record kept: a re-run is refused the same way while the target holds the path. The way out is
  `ws delete <name> --rescue`: the workspace's commits stay in the store. The same window lets a
  case collision through that the target creates after the check. *Unscheduled (the window is the
  time between two calls of one command). Found: M15 Task S4c review.*

## Platforms

- **Directory fsync failures are assumed fail-stop.** On ext4 with a journal, XFS and btrfs a
  failed directory fsync makes every later sync fail until the filesystem is mounted again, so a
  later sync that succeeds covers the entries made before it (storage spec §2 rule 5). ext4
  without a journal, ext2, FUSE and network filesystems do not behave this way and are not
  supported. Since M6 a store is refused unless `statfs` names ext2/3/4, XFS, btrfs, tmpfs, ZFS,
  F2FS or overlayfs (`E_UNSUPPORTED_FS`), which keeps FUSE and network mounts out, but ext2 and
  ext4 without a journal share ext4's magic and pass. *Documented. Found: the 2026-10-16 code
  review; narrowed by M6.*
- **btrfs and a resumed `init`.** Resuming an interrupted `init` relies on ext4's and XFS's
  ordered journal to make the crashed attempt's directories durable; on btrfs they might not be
  (recoverable by creating the store again). *Unscheduled. Found: M1 Task 3.*
- **How a store path resolves.** A `..` after an existing file is taken lexically, and a
  dangling symlink is followed (both documented on the resolver). *Unscheduled. Found: M1
  Task 3.*
- **No default store location.** A store is found by `--repo`, `SWARMOPS_REPO`, or, since M5, the
  nearest `.swarmws/workspace.json` above the working directory; `init` needs its directory, and
  `~/.local/state/swarmops/repos/<name>/` is only the convention the README follows.
  *Unscheduled. Found: M1 Task 5; umbrella spec §4; M1.5 final review.*
- **No migration between pre-release formats.** A store made before a format change may be
  refused afterwards and need creating again; it happened once during M1. A FORMAT 1 store moves
  to FORMAT 2 with `swarmops upgrade` (M6). *Documented (the versioning policy, before 1.0).*
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
  `E_BUSY`, and nothing is lost. The rewrite takes 50 ms at 100,000 records and 173 ms at
  1,000,000 (p50; 179 ms at most, [perf.md](perf.md#v060--2026-10-28)). *Target: the sorted views as
  immutable runs under a manifest, in a `FORMAT 3`, so that the lock holds only the manifest's
  swap (the M6 design, decision D2), or M3's merge outside the lock (its plan's Task 15) if the bench shows a
  stall of 100 ms or more at 100,000 records first. Found: M1.6 Task 7 (the compaction design);
  the 2026-10-20 plan triage.*
- **A blob is stored whole.** Each version of a file is one object (zstd-compressed past a
  small size), with no chunks or deltas, so a small edit to a large file stores it again.
  *Unscheduled (content-defined chunking). Found: the 2026-10-27 completeness report.*
- **Small object-store costs.** A put that creates a fan-out directory checks for it under the
  store's mutex. *Unscheduled. Found: M1 Task 1.*

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
  `journal.off` a writer cut is tested only at the journal's level. *Target: M9 (the metadata
  read paths through `FsOps`, umbrella spec §14; the M6 design, decision D4). Found: M1.5 Task 3 review (M3); M1.5 final
  review (D5).*
- **Golden envelopes cover 54 of the 55 error codes.** `error_codes_reach_the_cli` pins the
  envelope of 42, the nine workspace codes (M5), `E_WS_OUTSIDE_PREFIXES` and `E_WS_STALE` (M15),
  `E_FORMAT_REQUIRED` (M6), `E_AUTH` and `E_POLICY_DENIED` (M14) among them;
  `remote_error_codes_golden` the four `E_REMOTE_*` codes over smart HTTP and ssh (M13); the FORMAT 2 goldens `E_TAG_IMMUTABLE` and `E_MAINTENANCE_BUSY` (M6);
  and goldens of their own six the first test cannot raise (M15): `E_AMBIGUOUS_ID`,
  `E_IO_TRANSIENT`, `E_INTERNAL`, `E_CAS_RETRY_EXHAUSTED`, `E_BUSY` in the slow
  `op_log_behind_a_held_lock_is_e_busy`, and `E_UNKNOWN_PARENT` as an error object, since every
  operation resolves a parent first. The other is in the first test's `NOT_RAISED` list:
  `E_REF_NOT_FOUND`, which no operation raises. *Unscheduled (`E_IO` joined in M1.6, the six
  `E_GIT_*` codes in M3, `E_WS_STALE` in M15). Found: the 2026-10-16 comparison.*
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
  two fifty-racer siblings run again a command that meets `E_BUSY` (`retry_busy`): the `ref set`
  race its setup commits, and the `commit --advance` race its racers' publishes, since one that
  met `E_BUSY` recorded nothing. *Unscheduled. Found: M1.5 Task 3.*
- **A create's removal of a crashed create's staging is outside the fault doubles.** A create
  removes what a crashed create left in `meta.tmp/`, and a repository create removes an older
  `meta.tmp/` before it sets a crashed create of another format aside there, with `std::fs`:
  `FsOps` removes no directories. So the crash and I/O-error sweeps inject no fault there, and the
  power-loss images do not model it. A failed removal fails the create before it changes `meta/`
  (pinned with a directory the process may not change); a crash during it leaves part of
  `meta.tmp/`, which the next create removes. *Unscheduled: an `FsOps::remove_dir_all` is a new
  seam method that every double must forward. Found: M6 Task A4 review.*
- **Some tests run on one format.** The operations' mapping of malformed revisions, pinned to
  FORMAT 1, has no FORMAT 2 twin yet. Verify's damaged ref, lock and checkpoint cases run on both,
  and so do the git import's and export's crash and failure sweeps. *Unscheduled. Found: M6 Task
  A4; narrowed by issue #20 item 29.*
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
| 1 | Concurrent `init` on one directory | unsupported, documented; revisit with workspaces | closed in M5: `REPO` is linked no-clobber before `meta/`, under a lock held until `FORMAT`, so one `init` makes the store and the other fails with `E_EXISTS` |
| 2 | Readers return state from frames not yet synced | allowed for local readers; committed snapshots for off-host readers (M8) | "Readers may return state" |
| 3 | The lock's wait tail | `lock_timeout` stays 10 s; a FIFO hand-off later | Unscheduled (the M6 design, decision D3), "bounded, not first-in, first-out" |
| 4 | Commit latency against the spec's target | target restated without syncs; batching re-evaluated | M2 (its Task 4, after the pilot), "Full fsync is slow" |
| 5 | The recovery walk under the lock | with fsck, and the lock timeout against journal size | closed in M6 for FORMAT 2 (entry checksums) and by `swarmops repair`; FORMAT 1, "Recovery after a crash walks all of a FORMAT 1 `journal.off`" |
| 6 | `read_file` reads the whole blob | ranged reads | closed in M6: `cat` reads a blob in windows (ranged reads: M2) |
| 7 | Crafted `journal.off` entries | a per-entry checksum in the next `FORMAT` | closed in M6 for FORMAT 2; FORMAT 1, "Crafted FORMAT 1 `journal.off` entries" |
| 8 | `objects/tmp` leftovers | `gc` | closed in M6: `swarmops gc` |
| 9 | Wide directories in `ls` | size and depth | Unscheduled, "`ls` of a wide directory" (size and depth: M2) |
| 10 | Parked helper threads | the daemon | Unscheduled, "An abandoned lock wait" |
| 11 | `abort()` standing in for `SIGKILL` | a literal `SIGKILL` test | closed in M6: a literal `SIGKILL` test on a real disk; "The power-loss model is a model" |
| 12 | Second root commits | `commit_raw` | closed in M3: `Repo::commit_raw`, the git import's writer, makes a root changeset whatever refs exist |
| 13 | `--expect` with a ref name | documented | "`--expect` with a ref name" |
| 14 | Tests that read host repositories | none in M1: nothing to judge | M3's narrowed by the weekly host-repository round trip; M8 (its Task 11, the pilot), "Real repositories are read weekly only" |
